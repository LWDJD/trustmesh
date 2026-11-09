//! # tm_engine —— trustmesh 运行时
//!
//! 把三块拼起来：
//! - `tm_store`：账户表、正分清单、轮次抽样
//! - `tm_view`：定长压缩的观点
//! - `tm_agg`：指数衰减与聚合
//!
//! 引擎本身不做任何 I/O：
//! - 它产出「我需要谁的观点」的请求意图，由应用层去真正发起
//! - 应用层拿到观点后回调 `on_view`
//!
//! ## 两级衰减、覆盖率与不可达来源（设计规格 4.9）
//!
//! * **第一层 · 来源级衰减**（[EngineConfig::half_life]）：调的是来源**之间**的
//!   相对新鲜度，对「全体一起变旧」免疫（加权平均对均匀缩放不变）。
//! * **第二层 · 群体级衰减**（[EngineConfig::group_half_life] 与
//!   [EngineConfig::coverage_exponent]）：按覆盖率与 live 集合的加权平均年龄压
//!   整个群体项，于是 `声望 = β·个人项 + (1−β)·s·群体项`，
//!   **改变了 β:(1−β) 的实际配比**——这是第一层做不到的。
//!
//! ```text
//! coverage = |live| / |正分清单|
//! s        = coverage^α · 0.5^(avg_age_live / H_group)
//! ```
//!
//! **有替代才剔除**：只有 `coverage >= θ_remove` 时才把非 live 来源踢出群体项；
//! 否则保留它们（它们已被 `fs[v]` 衰减，而那时 `s` 本来就已经很小）。
//! 无条件剔除会把「诚实但离线」的人先踢掉，只留下 24/7 在线的攻击者，反而有害。
//! 剔除**只影响群体项**，个人评分与正分清单原封不动。
//!
//! 五个参数默认都处于**不生效**的状态（`H_group = 0` 且 `θ_remove = 0` 时 `s` 恒为 1，
//! `T_remove = 0` 时覆盖率恒为 1，α = 1.0、N_min = 0），所以默认行为与引入本机制
//! 之前逐位一致；新机制是否有效尚未验证，只在实验里显式打开。
//!
//! ## 抽样退避与兜底探测（设计规格 4.9.4）
//!
//! * [Engine::tick] 按**连续失败次数**退避（权重 1/2^n）：连续取不到观点的来源
//!   少占带宽。退避只影响抽样频率，不改评分、不改 `last_check`。
//! * [Engine::probe] 是**不走退避**的兜底探测，保证离线节点上线后还能被想起来。
//!   引擎不维护定时器，`T_probe`（建议 24h）的节奏由应用层掌握。

use std::collections::{BTreeMap, HashSet};

use tm_agg::{aggregate, combine, decay, AggMode};
use tm_store::{AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler, Score, Time};
use tm_view::{CompressedView, NoiseStats, ViewParams};

/// 一份缓存下来的观点。
#[derive(Clone)]
pub struct StoredView {
    pub view: CompressedView,
    /// 对方声明的发布时间
    pub published_at: Time,
    /// 上次成功取到的时间（衰减基准）
    pub last_check: Time,
    /// 该观点的噪声底
    pub noise: NoiseStats,
    /// 这份观点文件的总质量（接收方自己估计，用于影响力预算）
    pub total_mass: f32,
}

/// 引擎参数。
#[derive(Clone)]
pub struct EngineConfig {
    /// 个人权重 β：声望 = β × 个人项 + (1−β) × 群体项
    pub beta: Score,
    /// 半衰期（与 Time 同单位），默认 72
    pub half_life: Time,
    /// 观点压缩参数
    pub view_params: ViewParams,
    /// 群体项聚合口径
    pub agg_mode: AggMode,
    /// 低于多少的查询结果视为「没表态」
    pub noise_multiplier: Score,
    /// 是否启用时间衰减。`false` 表示不衰减（factor 恒为 1）。
    pub decay_enabled: bool,
    /// 衰减因子下限。0.0 表示无下限；0.5 表示「最多衰减一半」。
    /// 实际 factor = max(0.5^(Δt/H), decay_floor)。
    pub decay_floor: Score,
    /// **群体级**半衰期 H_group（与 [Time] 同单位），只作用于 s 的「平均年龄」项。
    ///
    /// 第二层衰减：`s = coverage^α · 0.5 ^ (avg_age_live / H_group)`，只乘在群体项上——
    /// `声望 = β·个人项 + (1−β)·s·群体项`。
    /// 它调整的是**群体项相对于个人项的绝对权重**（改变 β:(1−β) 的实际配比），
    /// 而 [EngineConfig::half_life] 调整的是来源之间的**相对**新鲜度。
    ///
    /// `0` 表示这一项**关闭**（`0.5^(...)` 恒为 1）。**默认 0**。
    ///
    /// 注意：这里的 0 是**语义上的「关闭开关」**，不是「把半衰期当 1」——
    /// 与 `tm_agg::decay(dt, 0)` 把 0 当 1 的约定是两回事（那边只是为了避免除零）。
    /// 当它与 [EngineConfig::remove_when_coverage] 同时为 0 时 `s` 恒为 1，
    /// 行为与引入 4.9 节之前逐位一致。
    pub group_half_life: Time,
    /// 覆盖率指数 α，默认 1.0。
    ///
    /// s 的覆盖率项是 `coverage^α`：α = 1 时它**就等于覆盖率**，
    /// α > 1 更严厉（覆盖率一降 s 掉得更快），α = 0 时完全不看覆盖率。
    ///
    /// 负数或 NaN 一律按 0 处理（防御非法配置，此时覆盖率项恒为 1）。
    pub coverage_exponent: Score,
    /// live 判定阈值 T_remove（与 [Time] 同单位），**默认 0（关闭）**。
    ///
    /// 判定：`now − last_check < unreachable_after` 的来源才算 **live**；
    /// `last_check` 是「上次**成功取到它观点**的时间」（[StoredView::last_check]）——
    /// 改分不算，从没取到过观点也谈不上 live。
    /// 覆盖率 [Engine::coverage] 就是「live 来源数 ÷ 正分清单条目数」。
    ///
    /// `0` 表示**不判定**（所有来源都视为 live，覆盖率恒为 1）。
    pub unreachable_after: Time,
    /// 允许剔除非 live 来源所需的最低覆盖率 θ_remove，**默认 0.0（永不剔除）**。
    ///
    /// 「**有替代才剔除**」：只有 `coverage >= θ_remove` 时才把非 live 来源踢出
    /// 群体项（分子分母都不含它）；否则**保留**它们 —— 它们已被 `fs[v]` 衰减过，
    /// 而覆盖率不足时 `s` 本来就已经很小。
    ///
    /// `0`（或负数、非有限值）表示**永不剔除**。
    pub remove_when_coverage: Score,
    /// 群体项所需的最低 live 来源数 N_min，**默认 0（不设限）**。
    ///
    /// live 来源数少于它时群体项视为空，声望退化为 `β × 个人项` ——
    /// 少量新鲜来源无法单独构成「群体意见」。
    ///
    /// `0` 表示跳过这道门槛（有几个来源就用几个）。
    pub min_group_sources: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            beta: 0.3,
            half_life: 72,
            view_params: ViewParams::default(),
            agg_mode: AggMode::Mean,
            noise_multiplier: 1.0,
            decay_enabled: true,
            decay_floor: 0.0,
            // 两级衰减默认全部关闭（H_group = 0 且 θ_remove = 0 ⇒ s 恒为 1）：
            // 默认行为必须与引入 4.9 节之前逐位一致，新机制由实验显式打开。
            group_half_life: 0,
            coverage_exponent: 1.0,
            unreachable_after: 0,
            remove_when_coverage: 0.0,
            min_group_sources: 0,
        }
    }
}

/// 「观点有多旧」的分布快照，由 [Engine::freshness_stats] 返回。
///
/// 这是一个纯粹的**观测**结果：计算它不会改变任何权重，也不影响后续的查询。
/// 全部观点都被清理掉时返回全零（而不是 NaN 或最大值）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreshnessStats {
    /// 已缓存的观点总数。
    pub count: usize,
    /// 年龄不超过一个半衰期的观点数。
    ///
    /// 判定口径与衰减严格对齐：恰好等于半衰期的那一条算「新鲜」，
    /// 因为它在当前时刻的衰减因子正好是 0.5（仍有一半话语权）。
    pub fresh_within_half_life: usize,
    /// 年龄中位数；偶数个时取中间两个的均值并向下取整。空集合为 0。
    pub median_age: Time,
    /// 最大年龄。空集合为 0。
    pub max_age: Time,
}

/// live 集合的一次扫描结果：来源数、权重和、按权重加权的年龄和。
///
/// 仅供 [Engine::live_count] / [Engine::avg_source_age] / [Engine::group_scale]
/// 三个只读接口内部使用；年龄的权重是**未衰减的份额**（不乘 fs），
/// 与群体项的 `份额 × fs` 故意不同 —— 理由见 `Engine::live_summary`。
#[derive(Clone, Copy, Debug, Default)]
struct LiveSummary {
    /// live 来源数。
    count: usize,
    /// Σ 权重（f64 累加，避免几百个来源时 f32 的可见误差）。
    weight_sum: f64,
    /// Σ 权重 × 年龄（单位：权重·时间单位）。
    age_weighted_sum: f64,
}

/// 一次性批量刷新用的请求项。
pub struct FetchRequest {
    /// 内部编号
    pub idx: AccountIdx,
    /// 对外要用的账户号（应用层据此找节点）
    pub key: AccountKey,
}

pub struct Engine {
    accounts: AccountTable,
    ratings: PositiveList,
    sampler: RoundSampler,
    /// 观点缓存。用 BTreeMap 而不是 HashMap：遍历顺序必须是**编号升序**，
    /// 否则群体项的浮点累加顺序会随 HashMap 的随机种子变化，
    /// 破坏「同样输入必然得到同样输出」（设计规格 §8 不变量 1）。
    views: BTreeMap<AccountIdx, StoredView>,
    cfg: EngineConfig,
    /// 本地屏蔽清单：只用于审查来源，不参与算分
    banlist: HashSet<AccountIdx>,
    /// 抽样用的随机流（固定种子即可复现）
    rng: Rng,
    /// 统计：累计请求数 / 成功数
    stat_requests: u64,
    stat_hits: u64,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self::with_seed(cfg, 0x5EED_1234_ABCD_0001)
    }

    pub fn with_seed(cfg: EngineConfig, seed: u64) -> Self {
        let rng = Rng::new(seed);
        Self {
            accounts: AccountTable::new(),
            ratings: PositiveList::new(),
            sampler: RoundSampler::new(),
            views: BTreeMap::new(),
            cfg,
            banlist: HashSet::new(),
            rng,
            stat_requests: 0,
            stat_hits: 0,
        }
    }

    // ---------- 账户 ----------

    pub fn intern(&mut self, key: AccountKey) -> AccountIdx {
        self.accounts.intern(key)
    }
    pub fn lookup(&self, key: &AccountKey) -> Option<AccountIdx> {
        self.accounts.lookup(key)
    }
    pub fn key_of(&self, idx: AccountIdx) -> Option<AccountKey> {
        self.accounts.key_of(idx)
    }
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    // ---------- 评分 ----------

    /// 记录一次评分。score 是应用层给的 **0~n 原始数值**，库里会自动归一化。
    /// score <= 0 表示撤销。
    pub fn rate(&mut self, idx: AccountIdx, score: Score, now: Time) {
        self.ratings.set(idx, score, now);
    }

    /// 累加式互动计数：又互动了 `times` 次。权重 = 次数 ÷ 总次数。
    pub fn record(&mut self, idx: AccountIdx, now: Time, times: u32) {
        let cur = self.ratings.get(idx).unwrap_or(0.0);
        self.ratings.set(idx, cur + times as Score, now);
    }

    /// 我对某个账户的**份额**（0~1，全部份额之和恒为 1）。没表过态返回 None。
    pub fn share_of(&self, idx: AccountIdx) -> Option<Score> {
        self.ratings.share(idx)
    }

    /// 我的全部份额（总和 = 1）。
    pub fn shares(&self) -> Vec<(AccountIdx, Score)> {
        self.ratings.shares()
    }

    pub fn rating_of(&self, idx: AccountIdx) -> Option<Score> {
        self.ratings.get(idx)
    }

    pub fn positive_count(&self) -> usize {
        self.ratings.len()
    }

    // ---------- 抽样 ----------

    /// 推进一批抽样，返回需要向哪些账户请求观点。
    ///
    /// 语义：抽到的账户**立刻**记为本轮已抽过；请求失败也不退回（由上层决定是否调用
    /// `on_view`）。所以这里返回的每一项，无论成败，本轮都不会再出现。
    ///
    /// 抽样走 **退避**（设计规格 4.9.4）：连续失败 n 次的来源权重降为 1/2^n。
    /// 全部来源连败次数相同时它退化为均匀随机，与不分退避时逐个相同 ——
    /// 所以「一条都没失败过」的网络行为完全不受影响。
    /// 退避只管抽样频率：不改评分、不改 `last_check`、也不改观点缓存。
    pub fn tick(&mut self, now: Time, k: usize) -> Vec<FetchRequest> {
        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);

        let _ = now;
        // 三个字段互不重叠，借用是分离的。
        let picked = self.sampler.draw_backoff(&self.ratings, k, &mut self.rng);
        self.collect_requests(picked)
    }

    /// 兜底探测：把**本轮还没抽过**的条目全部返回（不走退避），并置为已抽过。
    ///
    /// 设计规格 4.9.4：每 `T_probe`（建议 24 小时）强制完整扫一遍正分清单，
    /// 否则退避会让长期离线的来源永远抽不到，它也就永远回不到 live 集合。
    /// 引擎自己不维护定时器 —— **节奏由应用层决定**（例如每 24 小时调一次）。
    ///
    /// * 结果按编号升序，**不消耗随机数**，完全确定、可复现；
    /// * 返回的每一项都算本轮已抽过，本轮剩余清零；本轮恰好抽空时会自动开新一轮，
    ///   所以只要正分清单非空，一次探测必然覆盖一整轮；
    /// * 探测请求不走退避，但结果照常计入成功/失败
    ///   （成功 [Engine::on_view]、失败 [Engine::on_view_failed]）。
    pub fn probe(&mut self, now: Time) -> Vec<FetchRequest> {
        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);

        let _ = now;
        let picked = self.sampler.draw_all(&self.ratings);
        self.collect_requests(picked)
    }

    /// 把抽到的编号翻成对外请求项，并累计请求数统计。
    fn collect_requests(&mut self, picked: Vec<AccountIdx>) -> Vec<FetchRequest> {
        let mut out = Vec::with_capacity(picked.len());
        for idx in picked {
            if let Some(key) = self.accounts.key_of(idx) {
                self.stat_requests += 1;
                out.push(FetchRequest { idx, key });
            }
        }
        out
    }

    /// 强制开始新一轮（用于「启动时快速同步」）。
    pub fn restart_round(&mut self) {
        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);
        self.sampler.reset(&current);
    }

    /// 某个来源当前的**连续失败次数**（[Engine::tick] 退避的输入）。
    ///
    /// * 连续失败 n 次的来源被抽中的概率降为 1/2^n；
    /// * **只有** [Engine::on_view] 成功会把它清零 —— 改分（[Engine::rate]）不算
    ///   「刚取到观点」，不清零；
    /// * 没有该评分条目时返回 0（没有关系，就无所谓连败）。
    #[must_use]
    pub fn fail_streak(&self, idx: AccountIdx) -> u32 {
        self.ratings.fail_streak(idx)
    }

    /// 本轮还剩多少个没抽。
    pub fn round_remaining(&self) -> usize {
        self.sampler.remaining()
    }

    // ---------- 收到观点 ----------

    /// 应用层成功取到对方观点后调用。
    pub fn on_view(&mut self, owner: AccountIdx, view: CompressedView, published_at: Time, now: Time) {
        let noise = view.noise_floor(1000, noise_seed(owner));
        // 接收方自己估计总质量：对已知账户求和。发布方无法伪造这个估计。
        let mut total_mass = 0.0f32;
        for i in 0..self.accounts.len() as u32 {
            if let Some(k) = self.accounts.key_of(i) {
                total_mass += view.query(&k).clamp(0.0, 1.0);
            }
        }
        let total_mass = total_mass.max(1e-6);
        self.stat_hits += 1;
        self.views.insert(
            owner,
            StoredView { view, published_at, last_check: now, noise, total_mass },
        );
        // 成功刷新：刷新 last_check（衰减基准）**并清零连败计数**。
        // 只有这里会清零退避 —— 改分（`rate`）不算「刚取到观点」。
        self.ratings.record_success(owner, now);
    }

    /// 应用层取观点失败时调用：连败计数 +1（退避的输入），其余什么都不做。
    ///
    /// * **不**更新 `last_check`：失败不是「刚取到」，观点照旧自然变旧；
    /// * **不**改评分、**不**动观点缓存、**不**阻塞也不重试（规格 0.3）；
    /// * 连续失败 n 次的来源在 [Engine::tick] 里被抽中的概率降为 1/2^n；
    ///   下一次 [Engine::on_view] 成功会把它清零。
    ///
    /// 普通抽样失败与兜底探测失败都要调用它。
    pub fn on_view_failed(&mut self, owner: AccountIdx, _now: Time) {
        self.ratings.record_failure(owner);
    }

    /// 完整的信任分布：所有账户分到的比值，**总和恒为 1**。
    ///
    /// 这是对外的最终结果。每一个数值就是"我把多少比例的信任交给了它"，
    /// 乘以 100 即为置信度百分比。
    pub fn trust_distribution(&self, now: Time) -> Vec<(AccountIdx, Score)> {
        let n = self.accounts.len() as u32;
        let mut vals: Vec<(AccountIdx, Score)> = Vec::with_capacity(n as usize);
        let mut sum = 0.0f32;
        for i in 0..n {
            let v = self.query(i, now);
            if v > 0.0 {
                sum += v;
                vals.push((i, v));
            }
        }
        if sum > 0.0 {
            for x in vals.iter_mut() {
                x.1 /= sum;
            }
        }
        vals
    }

    /// 把自己当前的声望向量压缩成一份观点文件（发布用）。
    ///
    /// 归一化到总量 1：这就是影响力预算的"发布侧"实现。
    pub fn export_view(&self, now: Time) -> CompressedView {
        let n = self.accounts.len() as u32;
        let mut vals: Vec<([u8; 32], Score)> = Vec::with_capacity(n as usize);
        let mut sum = 0.0f32;
        for i in 0..n {
            if let Some(k) = self.accounts.key_of(i) {
                let v = self.query(i, now);
                if v > 0.0 {
                    sum += v;
                    vals.push((k, v));
                }
            }
        }
        if sum > 0.0 {
            for x in vals.iter_mut() {
                x.1 /= sum;
            }
        }
        CompressedView::from_pairs(self.cfg.view_params, &vals)
    }

    /// 已知账户的编号列表（实验与调试用）。
    pub fn known_accounts(&self) -> Vec<AccountIdx> {
        (0..self.accounts.len() as u32).collect()
    }

    // ---------- 观点新鲜度 ----------

    /// 某个来源的观点已经多久没更新：`now − last_check`。
    ///
    /// * 从没成功取到过它的观点 → `None`（「没有观点」和「观点年龄为 0」是两件事）；
    /// * 刚刚取到 → `Some(0)`；
    /// * 时间回拨（`now < last_check`）按 `Some(0)` 处理，绝不产生「负年龄」。
    ///
    /// 基准是 [StoredView::last_check]（我上次**成功**取到它观点的时间），
    /// 不是 [StoredView::published_at]（对方**声称**的发布时间）。
    /// 这条区别是安全性的关键：`published_at` 由对方随便填，
    /// 把自己的旧观点说成「刚刚写的」零成本；而 `last_check` 是我自己观察到的，
    /// 对方无法伪造，所以只有它才配当衰减基准。
    #[must_use]
    pub fn view_age(&self, owner: AccountIdx, now: Time) -> Option<Time> {
        self.views.get(&owner).map(|stored| elapsed(stored.last_check, now))
    }

    /// 所有已缓存观点的年龄统计。空集合返回全零。
    ///
    /// 统计覆盖 `views` 里的全部条目，与「我现在还给不给它打分」无关：
    /// 一条观点只要还在缓存里、还在参与 [Engine::query] 的群体项，就会计入。
    /// 因此 `count` 与 `stats().2` 恒等，`prune_stale` 之后两者一起变小。
    ///
    /// 半衰期为 0 时按 1 处理（与 `tm_agg::decay` 的约定一致），
    /// 这样「fresh_within_half_life」始终精确等于「衰减因子 >= 0.5 的来源数」。
    #[must_use]
    pub fn freshness_stats(&self, now: Time) -> FreshnessStats {
        let mut ages: Vec<Time> =
            self.views.values().map(|stored| elapsed(stored.last_check, now)).collect();
        let count = ages.len();
        if count == 0 {
            return FreshnessStats::default();
        }

        // 与 decay 的口径对齐：H = 0 等价于 H = 1，避免出现「连 0 时刻的观点都不新鲜」。
        let half_life = if self.cfg.half_life == 0 { 1 } else { self.cfg.half_life };
        let fresh_within_half_life = ages.iter().filter(|&&age| age <= half_life).count();

        ages.sort_unstable();
        let median_age = if count % 2 == 1 {
            ages[count / 2]
        } else {
            // 两个 u64 直接相加可能溢出，先各自折半再补上余数。
            let (lo, hi) = (ages[count / 2 - 1], ages[count / 2]);
            lo / 2 + hi / 2 + (lo % 2 + hi % 2) / 2
        };

        FreshnessStats {
            count,
            fresh_within_half_life,
            median_age,
            max_age: ages[count - 1],
        }
    }

    /// 显式清理：丢掉年龄**超过** `max_age` 的观点，返回丢弃条数。
    ///
    /// * 边界：`age == max_age` 的观点**保留**，只有严格更旧才丢弃；
    ///   `max_age = 0` 因此只保留「恰好在这一刻取到」的观点。
    /// * 只动观点缓存，**不动正分清单**：评分是我自己的表态，
    ///   清掉它会改变 [Engine::query] 的个人项，那不是「清理陈旧观点」该做的事。
    /// * 时间回拨时年龄按 0 算，所以回拨只会让清理更保守，不会误删。
    ///
    /// 本方法**不会被自动调用**：什么时候算「太旧」是业务判断，
    /// 引擎只提供能力，节奏由上层决定（例如每 N 轮扫一次）。
    pub fn prune_stale(&mut self, now: Time, max_age: Time) -> usize {
        let before = self.views.len();
        self.views.retain(|_, stored| elapsed(stored.last_check, now) <= max_age);
        before - self.views.len()
    }

    /// 时间衰减因子的实际取值（三个模式都走这里）。
    ///
    /// - `decay_enabled = false`  → 恒为 1（不衰减）
    /// - `decay_floor > 0`        → `max(0.5^(Δt/H), floor)`（有下限，例如最多衰减一半）
    /// - 其它                      → `0.5^(Δt/H)`（原行为）
    pub fn decay_factor(&self, dt: Time) -> Score {
        if !self.cfg.decay_enabled {
            return 1.0;
        }
        let f = decay(dt, self.cfg.half_life);
        if self.cfg.decay_floor > 0.0 {
            f.max(self.cfg.decay_floor)
        } else {
            f
        }
    }

    // ---------- 两级衰减、覆盖率与不可达来源（设计规格 4.9） ----------

    /// 当前**群体级衰减系数** `s`，范围 [0, 1]：
    ///
    /// ```text
    /// s = coverage^α · 0.5 ^ (avg_age_live / H_group)
    /// ```
    ///
    /// `s` 只乘在群体项上，所以它改变的是 β:(1−β) 的**实际配比**：
    /// `声望 = β·个人项 + (1−β)·s·群体项`。这是来源级衰减做不到的事
    /// （来源级衰减对「全体一起变旧」免疫，因为加权平均对均匀缩放不变）。
    ///
    /// 覆盖率项回答「我现在能看到的世界，占我认识的世界的百分之几」：
    /// 只剩攻击者可达时 `coverage ≈ 0`，群体项权重随之趋零，攻击者的观点就压不动我。
    ///
    /// # 边界
    /// * `group_half_life = 0` **且** `remove_when_coverage = 0` → 恒为 1（完全关闭），
    ///   而且**不扫描**观点缓存；
    /// * `coverage = 0`（没有 live 来源，或正分清单为空）→ 0；
    /// * `unreachable_after = 0`（live 判定关闭）时覆盖率恒为 1，s 只剩年龄项；
    /// * 覆盖率 100% 且来源全是刚取到的 → 1；
    /// * 平均年龄恰好等于 `H_group` → 年龄项 0.5；
    /// * 平均年龄很大 → 年龄项趋近 0，但**永不精确归零**（与 `tm_agg::decay` 同口径）。
    ///
    /// 平均年龄的口径见 [Engine::avg_source_age]（四舍五入到整数时间单位），
    /// 所以年龄项与 `tm_agg::decay(avg, H_group)` 逐位一致。
    ///
    /// # 读法：`s = 0` 是「无从判断」，不是「没有问题」
    ///
    /// `s = 0` 意味着群体项被彻底关闭，此时声望只剩 `β × 个人项`。
    /// 这是一个**信息不足**的保守姿态，不代表网络健康：覆盖面极低时群体项恒为零，
    /// 任何依赖群体项的指标都会「看起来变好」，而那只是判断力没了。
    /// 观察机制是否真的在起作用，请同时看 [Engine::coverage] 与 [Engine::live_count]。
    #[must_use]
    pub fn group_scale(&self, now: Time) -> Score {
        // 两个旋钮同时关闭：s 恒为 1，等价于「没有第二层衰减」
        // （也顺手省掉一次全量扫描，默认配置下查询路径的开销与改造前完全一样）。
        if self.cfg.group_half_life == 0 && self.cfg.remove_when_coverage <= 0.0 {
            return 1.0;
        }

        let cov = self.coverage(now);
        if cov <= 0.0 {
            // 看不到任何来源 ⇒ 没有可以信任的群体意见。
            return 0.0;
        }

        // 覆盖率项：α 取 max(0, α)，NaN 由 f32::max 的「忽略 NaN」语义收敛到 0
        // （此时这一项恒为 1，等于只看年龄）。
        let alpha = self.cfg.coverage_exponent.max(0.0);
        let coverage_term = if alpha == 0.0 {
            1.0
        } else if alpha == 1.0 {
            // α = 1 时覆盖率项**精确等于覆盖率**：不做 powf，避免浮点毛刺。
            cov
        } else {
            cov.powf(alpha)
        };

        // 年龄项：H_group = 0 时恒为 1（这一项关闭）。
        let age_term = if self.cfg.group_half_life == 0 {
            1.0
        } else {
            match self.avg_source_age(now) {
                // 覆盖率 > 0 时 live 必然非空，理论上到不了这里；防御性兜底。
                None => 0.0,
                Some(avg) => decay(avg, self.cfg.group_half_life),
            }
        };

        // 两项都在 [0,1]，乘积也在 [0,1]；非有限值一律按 0（0 = 最保守的语义）。
        let s = coverage_term * age_term;
        if s.is_finite() {
            s.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// 当前**覆盖率**：`coverage = live 来源数 ÷ 正分清单条目数`，范围 [0, 1]。
    ///
    /// 它回答的是「我现在能看到的世界，占我认识的世界的百分之几」：
    ///
    /// * `1.0` —— 正分清单里每个人的观点都还新鲜；`unreachable_after = 0`（live 判定
    ///   关闭）时全部视为 live，因此也恒为 1；
    /// * `0.0` —— 一个都联系不上；正分清单为空时也返回 0（分母为 0，按 0 处理）。
    ///
    /// 分母是**正分清单条目数**，分子是其中「观点还新鲜」的条目数
    /// （[Engine::live_count]）。给我打过分、但我从没取到过观点的账户
    /// 计在分母里、不计在分子里 —— 它们正是「认识却看不到」的那部分世界。
    ///
    /// 覆盖率只取决于「有多少来源还 live」，与「剔不剔除」无关：
    /// 剔除是另一道独立的判断，见 [EngineConfig::remove_when_coverage]。
    #[must_use]
    pub fn coverage(&self, now: Time) -> Score {
        let entries = self.ratings.len();
        if entries == 0 {
            return 0.0;
        }
        if self.cfg.unreachable_after == 0 {
            // live 判定关闭 ⇒ 全部视为 live ⇒ 覆盖率恒为 1。
            // 这样「H_group > 0、T_remove = 0」可以干净地只打开年龄项。
            return 1.0;
        }
        self.live_count(now) as Score / entries as Score
    }

    /// 当前 live 集合里的来源数。
    ///
    /// 口径：只数「我打过正分」且「缓存里有观点」且年龄未到达
    /// `unreachable_after` 的来源（判定见 `Engine::is_live`）。
    /// 撤销评分会立刻把它移出 live（观点缓存本身还在，见 [Engine::prune_stale]）；
    /// 有评分但从没取到过观点的来源不算 live —— 它没有观点可参与群体项。
    ///
    /// `unreachable_after = 0`（默认，不判定）时，全部这样的来源都算 live。
    ///
    /// # 读法
    ///
    /// `live_count == 0` 表示「**现在一个来源都问不到**」，随之 `s = 0`、群体项为零。
    /// 这只是「无从判断」，**不等于「没有问题」** —— 恰恰相反，它通常意味着我
    /// 已经看不到大部分网络了（见 [Engine::coverage]）。
    #[must_use]
    pub fn live_count(&self, now: Time) -> usize {
        self.live_summary(now).count
    }

    /// live 集合上的**加权平均**观点年龄；live 为空返回 `None`。
    ///
    /// 只在**有观点缓存**的 live 来源上求平均（没有观点就没有年龄可算）；
    /// 一个 live 来源都没有、或都没有观点时返回 `None`。
    ///
    /// 权重口径：**未衰减的份额**（不带 fs）：
    ///
    /// ```text
    /// avg_age = Σ_live 份额[v]·age[v] ÷ Σ_live 份额[v]
    /// 份额[v] = 评分[v] ÷ Σ评分
    /// ```
    ///
    /// 这与群体项的 `份额 × fs` **刻意不同**：群体项要让新鲜来源在意见上更有
    /// 话语权，而平均年龄要回答「我这群来源平均有多旧」这个**事实**。
    /// 如果这里也乘 fs，老来源的年龄贡献会被它自己的衰减压低 → avg_age 偏小 →
    /// s 偏大 → 群体级衰减几乎失效（等于自己把自己的输入衰减掉）。
    ///
    /// 返回值**四舍五入到最近的时间单位**：单来源时等于它的年龄，多来源时是加权
    /// 平均的整数部分。之所以不直接截断，是因为加权平均的浮点误差有 1 ulp 量级，
    /// 截断会把「三个来源年龄都是 120」算成 119。
    /// 时间回拨时年龄按 0 算，所以永远不返回负数、不 panic。
    #[must_use]
    pub fn avg_source_age(&self, now: Time) -> Option<Time> {
        let s = self.live_summary(now);
        // 权重和必须是有限正数：live 非空时恒成立，这里只是防御性兜底。
        if s.count == 0 || !s.weight_sum.is_finite() || s.weight_sum <= 0.0 {
            return None;
        }
        let mean = s.age_weighted_sum / s.weight_sum;
        if !mean.is_finite() || mean <= 0.0 {
            // 全新鲜或时间回拨时平均年龄就是 0；NaN 也一并归零，绝不放出去。
            return Some(0);
        }
        // 四舍五入消掉加权平均的 1 ulp 误差；f64 → u64 的 `as` 转换是饱和的，
        // 极端值会截到 u64::MAX，不会 panic、不会回绕。
        Some(mean.round() as u64)
    }

    /// 来源 `stored` 在当前时刻是否仍在 live 集合里。
    ///
    /// 判定式与规格 4.9.3 一致：`now − last_check < unreachable_after`
    /// （**严格小于**，等于阈值即退出），`age[v]` 按规格取自
    /// 「上次**成功取到 v 观点**的时间」，即 [StoredView::last_check]。
    ///
    /// 用观点缓存的 `last_check` 而不是正分清单条目上的时间戳，是**有意的**：
    /// 条目时间戳会被 [Engine::rate] 顺手刷新（改分不等于「刚听到它说话」），
    /// 而「我看不看得到它的观点」只应该由「我上次成功取到观点」决定。
    /// 从没取到过观点的账户不在 `views` 里，因此谈不上 live。
    ///
    /// `unreachable_after = 0` 表示 live 判定关闭（缓存里的来源全部视为 live）。
    /// 时间回拨时年龄按 0 算，因此回拨只会让来源更「live」，不会误踢。
    fn is_live(&self, stored: &StoredView, now: Time) -> bool {
        if self.cfg.unreachable_after == 0 {
            return true;
        }
        elapsed(stored.last_check, now) < self.cfg.unreachable_after
    }

    /// 扫一遍观点缓存，统计 live 集合的规模与加权年龄。
    ///
    /// # 计数口径
    ///
    /// `count` 数的是「我打过正分」且「缓存里有观点」且年龄未到达阈值的来源
    /// （见 `Engine::is_live`）。撤销评分（打 0）会立刻把它移出 live，
    /// 尽管观点缓存还在（缓存只能由 [Engine::prune_stale] 清）。
    ///
    /// # 年龄口径
    ///
    /// 权重是**未衰减的份额** `份额[v] = 评分[v] ÷ Σ评分`（**不乘 fs**），
    /// 这一点与群体项**刻意不同**，不要合并：
    ///
    /// * 群体项用 `份额 × fs` —— 让新鲜来源在**意见**上更有话语权；
    /// * 平均年龄用**裸份额** —— 它要回答的是「我这群来源平均有多旧」这个**事实**。
    ///   如果这里也乘 fs，老来源的年龄贡献会被它自己的衰减压低 → avg_age 偏小 →
    ///   s 偏大 → 群体级衰减等于自己把输入衰减掉，几乎失效。
    ///
    /// f64 累加（几百个来源时 f32 连加会有可见误差）。
    /// 三种只读接口共用同一次扫描，保证 `live_count` / `avg_source_age` /
    /// `group_scale` 三者口径永远一致。
    fn live_summary(&self, now: Time) -> LiveSummary {
        let mut out = LiveSummary::default();
        let total = self.ratings.total();
        if total <= 0.0 {
            return out;
        }
        for (idx, stored) in &self.views {
            if !self.is_live(stored, now) {
                continue;
            }
            // 撤销评分（打 0）的来源立刻退出 live：群体项也不再认它的观点。
            let rating = match self.ratings.get(*idx) {
                Some(s) => s,
                None => continue,
            };
            // 份额，**不乘 fs**：平均年龄是「这群来源有多旧」的事实，不是加权意见。
            let share = f64::from(rating / total);
            if share <= 0.0 {
                continue;
            }
            out.count += 1;
            out.weight_sum += share;
            out.age_weighted_sum += share * (elapsed(stored.last_check, now) as f64);
        }
        out
    }

    // ---------- 计算 ----------

    /// 查某个账户在自己眼中的声望。
    ///
    /// 合成逻辑（设计规格 4.4 + 4.9）：
    ///
    /// ```text
    /// 个人项[u] = 份额[u]                                    // 永不衰减
    /// 群体项[u] = Σ_参与 份额[v]·fs[v]·观点[v][u] ÷ Σ_参与 份额[v]·fs[v]
    /// 声望[u]   = β·个人项[u] + (1−β)·s·群体项[u]
    /// ```
    ///
    /// * `fs` 是来源级衰减（[Engine::decay_factor]），只改来源**之间**的相对权重；
    /// * `s = coverage^α · 0.5^(avg_age_live/H_group)`（[Engine::group_scale]），
    ///   改 β:(1−β) 的**实际配比**；
    /// * 「参与」= live 来源；**但有替代才剔除**：只有覆盖率 ≥ θ_remove 时才把
    ///   非 live 来源踢出群体项，否则保留它们（它们已被 `fs[v]` 衰减，而未达标时
    ///   `s` 本来就已经很小）。见 [EngineConfig::remove_when_coverage]；
    /// * live 来源数 < `min_group_sources` 时群体项置空，声望退化为 β·个人项。
    ///
    /// 默认配置（`H_group = 0`、`θ_remove = 0`、`T_remove = 0`、`N_min = 0`）下：
    /// `s ≡ 1`、覆盖率恒为 1、全部来源 live、不设门槛，
    /// 结果与引入 4.9 节之前**逐位一致**。
    pub fn query(&self, target: AccountIdx, now: Time) -> Score {
        // 影响力预算：自己的评分也要归一化，总配额恒为 1
        let total_rating: f32 = self.ratings.iter().map(|(_, s, _)| s).sum();
        let raw_personal = self.ratings.get(target).unwrap_or(0.0);
        let personal = if total_rating > 0.0 { raw_personal / total_rating } else { 0.0 };

        let target_key = match self.accounts.key_of(target) {
            Some(k) => k,
            None => return combine(self.cfg.beta, personal, None),
        };

        // 「有替代才剔除」：只有覆盖率已达到 θ_remove 时，才允许把非 live 来源
        // 踢出群体项（分子分母都不含它）。覆盖率不足时保留它们 —— 它们已经被
        // fs[v] 衰减过，而那时 s 本来就已经很小，硬剔只会让攻击者占比更高。
        //
        // θ_remove = 0（默认）时 `&&` 直接短路，连覆盖率都不算。
        let remove_unreachable = self.cfg.remove_when_coverage > 0.0
            && self.coverage(now) >= self.cfg.remove_when_coverage;

        let mut items: Vec<(f32, f32)> = Vec::with_capacity(self.views.len());
        // 顺便数一遍 live 来源：N_min 门槛按 **live 数**判定（不是参与聚合的条目数），
        // 这样「剔除关着、但只剩一个能看到的来源」也会被掐掉群体项。
        let mut live = 0usize;
        for (owner, stored) in &self.views {
            let w_rating = match self.ratings.get(*owner) {
                Some(s) => s,
                None => continue,
            };
            let is_live = self.is_live(stored, now);
            if is_live {
                live += 1;
            }
            // 只有「有替代」时才剔：不可达来源退出群体项（个人评分照常保留）。
            if remove_unreachable && !is_live {
                continue;
            }
            // 衰减基准是 last_check —— 我上次**成功**取到它观点的时间，
            // 而不是 published_at（对方自己声明的发布时间，可以随意伪造）。
            // 时间回拨时 dt 按 0 算（saturating_sub），不会产生负数或回绕。
            let dt = elapsed(stored.last_check, now);
            let w = w_rating * self.decay_factor(dt);
            if w <= 0.0 {
                continue;
            }
            let mut v = stored.view.query(&target_key) / stored.total_mass;
            // Count Sketch 的估值可能为负或超过 1，先夹紧到合法区间
            v = v.clamp(0.0, 1.0);
            // 低于噪声底的读数视为「没表态」
            let floor = (stored.noise.median * self.cfg.noise_multiplier).max(0.0);
            if v < floor {
                v = 0.0;
            }
            items.push((w, v));
        }

        // 最低样本数：**live** 来源少于 N_min 时群体项置空 —— 少量新鲜来源不能
        // 单独构成「群体意见」。`min_group_sources = 0` 表示不设限（跳过门槛）。
        let group = if live < self.cfg.min_group_sources {
            None
        } else {
            aggregate(&items, self.cfg.agg_mode)
        };

        // 第二层衰减只压群体项，个人项永不衰减。
        // s = 1（默认，关闭）时这次乘法是 IEEE 恒等变换，结果逐位不变。
        let scale = self.group_scale(now);
        combine(self.cfg.beta, personal, group.map(|g| g * scale))
    }

    // ---------- banlist 审查 ----------

    pub fn ban(&mut self, idx: AccountIdx) {
        self.banlist.insert(idx);
    }
    pub fn unban(&mut self, idx: AccountIdx) {
        self.banlist.remove(&idx);
    }
    pub fn banlist_len(&self) -> usize {
        self.banlist.len()
    }

    /// 某来源的观点里「我的黑名单成员」占多大比例。
    /// 比例越高，说明它越可能在抬举攻击我的人。
    pub fn banlist_coverage(&self, owner: AccountIdx, _now: Time) -> Option<f32> {
        let stored = self.views.get(&owner)?;
        if self.banlist.is_empty() {
            return Some(0.0);
        }
        let floor = stored.noise.median * self.cfg.noise_multiplier;
        let mut hit = 0usize;
        let mut seen = 0usize;
        for &b in &self.banlist {
            if let Some(k) = self.accounts.key_of(b) {
                seen += 1;
                if stored.view.query(&k) > floor {
                    hit += 1;
                }
            }
        }
        if seen == 0 {
            None
        } else {
            Some(hit as f32 / seen as f32)
        }
    }

    // ---------- 统计 ----------

    pub fn stats(&self) -> (u64, u64, usize, usize) {
        (self.stat_requests, self.stat_hits, self.views.len(), self.ratings.len())
    }
}

/// 从 `start` 走到 `now` 经过的时间。
///
/// 时间戳由上层给出、不保证单调（见 `tm_store::Time` 的约定），
/// 所以时间回拨时必须夹到 0：`u64` 直接相减会回绕成一个巨大的正数，
/// 那会让「刚刚取到的观点」瞬间衰减成 0，后果比「当作没变旧」严重得多。
fn elapsed(start: Time, now: Time) -> Time {
    now.saturating_sub(start)
}

fn noise_seed(idx: AccountIdx) -> u64 {
    (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

#[cfg(test)]
mod smoke {
    use super::*;
    use tm_view::ViewParams;

    fn key(n: u8) -> AccountKey {
        let mut k = [0u8; 32];
        k[0] = n;
        k
    }

    /// 默认口径：β = 0.3、来源级半衰期 72 小时。
    ///
    /// 两级衰减的三个新参数显式写 0（关闭），让这 8 条冒烟测试继续钉住
    /// 「来源级衰减 + β 混合」的旧行为；新机制的行为见
    /// `tests/two_level_decay.rs`。
    fn eng() -> Engine {
        Engine::with_seed(
            EngineConfig {
                beta: 0.3,
                half_life: 72,
                group_half_life: 0,
                coverage_exponent: 1.0,
                unreachable_after: 0,
                remove_when_coverage: 0.0,
                min_group_sources: 0,
                ..Default::default()
            },
            42,
        )
    }

    /// S1：给某人打满分，没人在场时声望 = β × 个人项
    #[test]
    fn s1_personal_only() {
        let mut e = eng();
        let b = e.intern(key(2));
        let c = e.intern(key(3));
        e.rate(b, 1.0, 0);
        assert!((e.query(b, 0) - 0.3).abs() < 1e-6, "应为 β×1.0=0.3");
        assert!(e.query(c, 0).abs() < 1e-6, "没评过应为 0");
    }

    /// S2：完全不存在的账户查询为 0
    #[test]
    fn s2_unknown_is_zero() {
        let e = eng();
        assert!(e.query(12345, 0).abs() < 1e-6);
    }

    /// S3：抽样 → 回调 → 声望上升；群体项 = 朋友的判断
    #[test]
    fn s3_full_flow() {
        let mut e = eng();
        let friend = e.intern(key(1));
        let target = e.intern(key(9));
        e.rate(friend, 1.0, 0);

        let reqs = e.tick(0, 10);
        assert!(reqs.iter().any(|r| r.idx == friend), "应当抽到朋友");

        let params = ViewParams::default();
        let v = CompressedView::from_pairs(params, &[(key(9), 1.0)]);
        e.on_view(friend, v, 0, 0);

        let q = e.query(target, 0);
        assert!((q - 0.7).abs() < 1e-4, "应为 0.3×0 + 0.7×1.0 = 0.7，实际 {q}");
    }

    /// S4：衰减是"相对"的——同样陈旧的来源之间，谁也不会被削弱
    ///
    /// 这是一个必须记住的性质：群体项是加权平均（除以权重和），
    /// 所有来源一起变旧时分子分母同比例缩小，结果**完全不变**。
    #[test]
    fn s4_uniform_decay_is_invariant() {
        let mut e = eng();
        let f1 = e.intern(key(1));
        let f2 = e.intern(key(2));
        let target = e.intern(key(9));
        e.rate(f1, 1.0, 0);
        e.rate(f2, 1.0, 0);
        e.tick(0, 10);
        e.on_view(f1, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]), 0, 0);
        e.on_view(f2, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]), 0, 0);

        let at0 = e.query(target, 0);
        let at500 = e.query(target, 500);
        let at100000 = e.query(target, 100_000);
        assert!((at0 - at500).abs() < 1e-6, "全部陈旧时结果不变: {at0} vs {at500}");
        assert!((at0 - at100000).abs() < 1e-6, "再久也不变: {at0} vs {at100000}");
        assert!((at0 - 0.35).abs() < 1e-4, "0.3×0 + 0.7×0.5 = 0.35，实际 {at0}");
    }

    /// S4b：真正起作用的是"新鲜度差"——新鲜来源会压过陈旧来源
    #[test]
    fn s4b_relative_freshness_dominates() {
        let mut e = eng();
        let stale = e.intern(key(1));
        let fresh = e.intern(key(2));
        let target = e.intern(key(9));
        e.rate(stale, 1.0, 0);
        e.rate(fresh, 1.0, 0);
        e.tick(0, 10);
        // 两人都在 0 时刻给了观点，但 stale 只给 1.0，fresh 给 0.0
        e.on_view(stale, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]), 0, 0);
        e.on_view(fresh, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]), 0, 0);
        let both_fresh = e.query(target, 0);

        // 只刷新 fresh：它权重变 1.0，stale 降到 0.5（一个半衰期）
        e.on_view(fresh, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]), 72, 72);
        let fresh_wins = e.query(target, 72);

        assert!(fresh_wins < both_fresh, "新鲜的低分来源应压过陈旧的高分来源: {both_fresh} -> {fresh_wins}");
        // group = (0.5×1.0 + 1.0×0.0)/(1.5) = 0.3333
        assert!((fresh_wins - 0.3 + 0.7 * (1.0 / 3.0) - 0.3).abs() < 1e-4 || (fresh_wins - (0.7 * (1.0 / 3.0))).abs() < 1e-4,
            "应为 0.7×(1/3)=0.2333，实际 {fresh_wins}");
    }

    /// S5：全部离线很久，分数只降不崩，且非负
    #[test]
    fn s5_all_stale_never_negative() {
        let mut e = eng();
        let friend = e.intern(key(1));
        let target = e.intern(key(9));
        e.rate(friend, 1.0, 0);
        e.tick(0, 10);
        e.on_view(friend, CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]), 0, 0);
        for t in [0u64, 100, 1_000, 100_000, 10_000_000] {
            let q = e.query(target, t);
            assert!((0.0..=1.0).contains(&q), "t={t} 时越界: {q}");
        }
    }

    /// S6：抽完一轮自动重置
    #[test]
    fn s6_round_reset() {
        let mut e = eng();
        let mut idxs = vec![];
        for i in 0..5u8 {
            let i = e.intern(key(i + 1));
            e.rate(i, 0.5, 0);
            idxs.push(i);
        }
        let first = e.tick(0, 5);
        assert_eq!(first.len(), 5);
        assert_eq!(e.round_remaining(), 0);
        let second = e.tick(1, 5);
        assert_eq!(second.len(), 5, "应自动开启新一轮");
    }

    /// S7：banlist 覆盖率
    #[test]
    fn s7_banlist_coverage() {
        let mut e = eng();
        let friend = e.intern(key(1));
        let bad = e.intern(key(66));
        e.rate(friend, 1.0, 0);
        e.ban(bad);
        let v = CompressedView::from_pairs(ViewParams::default(), &[(key(66), 1.0), (key(9), 0.5)]);
        e.on_view(friend, v, 0, 0);
        let cov = e.banlist_coverage(friend, 0).unwrap_or(0.0);
        assert!((cov - 1.0).abs() < 1e-6, "覆盖率应为 1.0，实际 {cov}");
    }
    /// S8：影响力预算——打的人越多，每个人分到的份额越少
    #[test]
    fn s8_influence_budget() {
        let mut e = eng();
        // 给 10 个人各打 0.5
        let mut idxs = vec![];
        for i in 0..10u8 {
            let x = e.intern(key(i + 10));
            e.rate(x, 0.5, 0);
            idxs.push(x);
        }
        // 每个人的份额应当都是 1/10
        let q = e.query(idxs[0], 0);
        assert!((q - 0.3 * 0.1).abs() < 1e-5, "份额应为 0.1，声望 0.03，实际 {q}");
        // 打到 100 个人，份额降到 1/100
        let mut e2 = eng();
        let mut first = 0;
        for i in 0..100u32 {
            let x = e2.intern(key((i % 250) as u8));
            if i == 0 { first = x; }
            e2.rate(x, 0.5, 0);
        }
        let q2 = e2.query(first, 0);
        assert!(q2 < q, "打 100 人后单人份额应更小: {q2} vs {q}");
    }
}
