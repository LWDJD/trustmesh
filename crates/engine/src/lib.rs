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
//! ## 抽样退避与兜底探测（设计规格 4.9.4）
//!
//! * [Engine::tick] 按**连续失败次数**退避（权重 1/2^n）：连续取不到观点的来源
//!   少占带宽。退避只影响抽样频率，不改评分、不改 `last_check`。
//! * [Engine::probe] 是**不走退避**的兜底探测，保证离线节点上线后还能被想起来。
//!   引擎不维护定时器，`T_probe`（建议 24h）的节奏由应用层掌握。
//!
//! ## 受害者反馈：badlist（C2 · 一次性事件）
//!
//! 应用层判断「我正在被谁攻击」，调用 [Engine::report_bad_account] 报告那个账户。
//! 引擎看**我信任的来源**里谁对它异常热情，把它们的群体项权重乘上一个折扣
//! （[Engine::penalty_of]）。折扣不写入 [PositiveList]：原始评分永远是我
//! 自己的表态，badlist 只是「这次先少听它一点」。
//!
//! 引擎**不保存坏账户清单**：报告是事件，调用时按当前观点算一次折扣；
//! 折扣取 `min`，不复合，也**没有自动恢复**（[Engine::clear_penalties] 是
//! 应用层的手动撤销）。默认 `penalty_strength = 1.0`、折扣表为空，
//! 不调用报告接口时行为与改造前逐位一致。算法细节见 `src/badlist.rs`。

mod badlist;

use std::collections::BTreeMap;

use badlist::SourceSnapshot;
use tm_agg::{aggregate, combine, decay, AggMode};
use tm_store::{
    AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler, Score, Time,
};
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
    /// badlist 惩罚强度 λ ∈ [0, 1]，**默认 1.0（完整惩罚）**。
    ///
    /// 闭式解本身已经把折扣夹在 [0,1]；这个旋钮是「刻意留手」：
    /// `f_eff = 1 − λ·(1 − f)`。λ = 1 时结果精确落到 ā；
    /// λ = 0.5 时只削掉一半差距；λ = 0 等于关闭惩罚（报告不写入任何折扣）。
    /// 非法值（NaN / 负数 / inf）按 1.0 处理。
    ///
    /// 这是 badlist 的**唯一**配置项：没有清单、没有 EMA、没有恢复速率。
    pub penalty_strength: Score,
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
            // badlist 默认「打开但不生效」：强度 1.0、折扣表为空。
            // 没人调用 report_bad_account 时 query 完全跳过折扣分支，
            // 因此默认行为与引入本机制之前逐位一致。
            penalty_strength: 1.0,
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
    /// 来源折扣表：缺省 1.0，只存被压低的条目。
    ///
    /// `w_v = 份额_v × 衰减_v × penalty[v]`，由 [Engine::report_bad_account] 写入。
    /// 折扣是**事件结果**而非清单：表里没有「谁被报告过」，只有「谁的权重被压低」。
    /// 折扣不会自动恢复，只有 [Engine::clear_penalties] 能清空。
    /// 用 BTreeMap 而不是 HashMap 是为了遍历顺序固定（可复现）。
    penalties: BTreeMap<AccountIdx, Score>,
    /// 最近一次带时间参数的调用时刻。
    ///
    /// 公开接口 [Engine::report_bad_account] 没有 `now` 参数，用它作为时间基准；
    /// 从未调用过任何带时间的方法时为 0。
    clock: Time,
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
            penalties: BTreeMap::new(),
            clock: 0,
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
        self.clock = now;
        self.ratings.set(idx, score, now);
    }

    /// 累加式互动计数：又互动了 `times` 次。权重 = 次数 ÷ 总次数。
    pub fn record(&mut self, idx: AccountIdx, now: Time, times: u32) {
        self.clock = now;
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
        // badlist 折扣是**事件**，tick 既不重算、也不恢复：这里只更新时间基准，
        // 供之后应用层调用 report_bad_account / on_view 等接口使用。
        // 不调用报告接口时折扣表恒为空，随机数序列与引入本机制之前逐位一致。
        self.clock = now;

        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);

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
        self.clock = now;
        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);

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
    pub fn on_view(
        &mut self,
        owner: AccountIdx,
        view: CompressedView,
        published_at: Time,
        now: Time,
    ) {
        self.clock = now;
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
            StoredView {
                view,
                published_at,
                last_check: now,
                noise,
                total_mass,
            },
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
    pub fn on_view_failed(&mut self, owner: AccountIdx, now: Time) {
        self.clock = now;
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
    ///
    /// **badlist 折扣会通过 [Engine::query] 自动进入这份观点**：被罚来源对
    /// 攻击者的高读数会被压下去，于是我发布出去的观点里攻击者的份额也变低，
    /// 惩罚由此传播给下游。这是机制的核心目的——不传播的折扣等于没有罚。
    /// 折扣不会随时间自动恢复，所以一次报告的影响会一直留在后续发布的观点里，
    /// 直到应用层调用 [Engine::clear_penalties]。
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
        self.views
            .get(&owner)
            .map(|stored| elapsed(stored.last_check, now))
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
        let mut ages: Vec<Time> = self
            .views
            .values()
            .map(|stored| elapsed(stored.last_check, now))
            .collect();
        let count = ages.len();
        if count == 0 {
            return FreshnessStats::default();
        }

        // 与 decay 的口径对齐：H = 0 等价于 H = 1，避免出现「连 0 时刻的观点都不新鲜」。
        let half_life = if self.cfg.half_life == 0 {
            1
        } else {
            self.cfg.half_life
        };
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
        self.clock = now;
        let before = self.views.len();
        self.views
            .retain(|_, stored| elapsed(stored.last_check, now) <= max_age);
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

    // ---------- badlist（受害者反馈机制，一次性事件） ----------

    /// 报告一个「已确认的坏账户」：按**当前**观点，把「对这个账户比平均更亲近的
    /// 来源」打分压低。返回被**实际**压低的来源数量。
    ///
    /// 这是**一次性事件**，不是清单：
    ///
    /// * 调用时按当前观点缓存算一次统一折扣 f（算法见 `src/badlist.rs`）；
    /// * 写入规则是 `penalty[v] = min(penalty[v], f)`，**不复合**：
    ///   重复报告同一个账户不会越压越狠，报告多个账户也只是各取最严厉的一次；
    /// * 引擎不保存「谁被报告过」，只保存每个来源的折扣；
    /// * **没有自动恢复**：[Engine::tick] 不会改动折扣，误报的代价会一直保留，
    ///   直到应用层调用 [Engine::clear_penalties] 手动撤销；
    /// * 时间基准是引擎最近一次带时间参数的调用（[Engine::tick] / [Engine::rate] /
    ///   [Engine::on_view] / [Engine::probe] / [Engine::prune_stale] 的 `now`），
    ///   一次都没调用过时为 0 —— 这是 `report_bad_account` 没有 `now` 参数的配套约定。
    ///
    /// 只影响 [Engine::query] 的群体项权重，**不改** [PositiveList] 里的原始评分，
    /// 也不影响抽样与观点缓存。
    ///
    /// # 返回值
    ///
    /// 真正被压低的来源数（`[0, 来源总数]`）。以下情况都返回 0 且不写表：
    /// 坏账户没有 intern 过、没有可参与群体项的来源、S+ 为空（大家态度一致）、
    /// 闭式解没有可压缩的差距、`penalty_strength = 0`，以及
    /// **这次算出的 f 不低于已有折扣**（min 语义下重复报告不会更狠）。
    ///
    /// # 注意
    ///
    /// 应用层必须自己保证「A 确实是坏账户」：本接口没有自动纠错，
    /// 而折扣一旦写入就会一直生效（只能手动 [Engine::clear_penalties]）。
    pub fn report_bad_account(&mut self, idx: AccountIdx) -> usize {
        let key = match self.accounts.key_of(idx) {
            Some(k) => k,
            None => return 0,
        };
        let total = self.ratings.total();
        if total <= 0.0 || self.views.is_empty() {
            return 0;
        }
        let lambda = f64::from(self.cfg.penalty_strength);
        let now = self.clock;

        // 一遍扫描：把「参与群体项的来源」及其对 A 的读数固定下来。
        // 参与条件与 query 完全一致：我给它正分、有观点缓存、权重 > 0。
        let mut sources: Vec<SourceSnapshot> = Vec::with_capacity(self.views.len());
        for (owner, stored) in &self.views {
            let rating = match self.ratings.get(*owner) {
                Some(s) if s.is_finite() && s > 0.0 => s,
                _ => continue,
            };
            let share = f64::from(rating / total);
            let decay = f64::from(self.decay_factor(elapsed(stored.last_check, now)));
            let weight = share * decay;
            if !weight.is_finite() || weight <= 0.0 {
                continue;
            }
            sources.push(SourceSnapshot {
                idx: *owner,
                weight,
                reading: f64::from(self.reading_of(stored, &key)),
            });
        }
        if sources.len() < 2 {
            // 0/1 个来源不可能有「高于平均」的人，闭式解也一定退出；
            // 提前返回省掉两次 Vec 分配。
            return 0;
        }

        let weights: Vec<f64> = sources.iter().map(|s| s.weight).collect();
        let readings: Vec<f64> = sources.iter().map(|s| s.reading).collect();
        let sol = badlist::solve_uniform(&weights, &readings);
        let f = badlist::apply_strength(sol.f, lambda);
        if f >= 1.0 {
            return 0;
        }

        // S+ 里所有来源共享同一个 f；已有更低的折扣时 min 保留旧值。
        // 不连乘、不复合：两个各罚 50% 仍然是 50%，不会变成 75%，
        // 重复报告同一个坏账户也不会越压越狠。
        let mut lowered = 0usize;
        for (i, src) in sources.iter().enumerate() {
            if !sol.above[i] {
                continue;
            }
            let old = f64::from(self.penalty_of(src.idx));
            if f < old {
                // f 已由 apply_strength 夹到 [0,1]，直接转 Score。
                self.penalties.insert(src.idx, f as Score);
                lowered += 1;
            }
        }
        lowered
    }

    /// 某来源当前的折扣，范围 [0,1]，1 = 未受罚。
    ///
    /// 这是**已经乘进群体项权重**的那个值：`w_v = 份额 × 衰减 × penalty[v]`。
    /// 没有折扣记录（或记录损坏）时返回 1.0。
    #[must_use]
    pub fn penalty_of(&self, idx: AccountIdx) -> Score {
        match self.penalties.get(&idx) {
            Some(&d) if d.is_finite() => d.clamp(0.0, 1.0),
            Some(_) => 1.0,
            None => 1.0,
        }
    }

    /// 手动清空全部折扣，立刻恢复所有来源的权重。
    ///
    /// 这是**应用层显式撤销**（例如发现自己报错了），不是自动恢复：
    /// 引擎不会在任何 `tick` 里调用它，也不会让折扣随时间漂移回 1。
    /// 清空后 [Engine::query] 与引入本机制之前逐位一致；之后再次
    /// [Engine::report_bad_account] 会重新写入新的折扣。
    pub fn clear_penalties(&mut self) {
        self.penalties.clear();
    }

    /// 某个来源的观点里对 `key` 的读数，**与 [Engine::query] 的群体项同口径**：
    /// 先除以该视图的 `total_mass` 再夹到 [0,1]，低于噪声底视为没表态。
    ///
    /// 与 query 的唯一区别是非有限值：这里一律当 0（query 的历史路径没有这个兜底，
    /// 但正常压缩视图不会产生 NaN）。badlist 的所有中间量都必须是有限数。
    fn reading_of(&self, stored: &StoredView, key: &AccountKey) -> Score {
        let raw = stored.view.query(key) / stored.total_mass;
        let mut v = if raw.is_finite() {
            raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let floor = (stored.noise.median * self.cfg.noise_multiplier).max(0.0);
        if v < floor {
            v = 0.0;
        }
        v
    }

    // ---------- 计算 ----------

    /// 查某个账户在自己眼中的声望。
    ///
    /// 合成逻辑（设计规格 4.4）：
    ///
    /// ```text
    /// 个人项[u] = 份额[u]                                    // 永不衰减
    /// 群体项[u] = Σ 份额[v]·fs[v]·观点[v][u] ÷ Σ 份额[v]·fs[v]
    /// 声望[u]   = β·个人项[u] + (1−β)·群体项[u]
    /// ```
    ///
    /// * 份额 = 评分 ÷ 评分总和（影响力预算，总配额恒为 1），个人项永不衰减；
    ///
    /// `penalty[v]` 是 badlist 折扣（[Engine::penalty_of]），缺省 1.0。
    /// 它只压群体项里对该来源的**话语权**，不改个人评分，也不改 `fs`。
    ///
    /// * `fs` 是来源级衰减（[Engine::decay_factor]），只改来源**之间**的相对权重；
    /// * 低于噪声底的读数视为「没表态」，该来源的这一票按 0 计；
    /// * 没有任何观点缓存时群体项为空，结果退化为 β·个人项。
    pub fn query(&self, target: AccountIdx, now: Time) -> Score {
        // 影响力预算：自己的评分也要归一化，总配额恒为 1
        let total_rating: f32 = self.ratings.iter().map(|(_, s, _)| s).sum();
        let raw_personal = self.ratings.get(target).unwrap_or(0.0);
        let personal = if total_rating > 0.0 {
            raw_personal / total_rating
        } else {
            0.0
        };

        let target_key = match self.accounts.key_of(target) {
            Some(k) => k,
            None => return combine(self.cfg.beta, personal, None),
        };

        let mut items: Vec<(f32, f32)> = Vec::with_capacity(self.views.len());
        // 折扣表为空（默认）时完全跳过查表与乘法：热路径零额外开销，
        // 也从代码上保证「没人调用 report_bad_account 就与改造前逐位一致」。
        let has_penalty = !self.penalties.is_empty();
        for (owner, stored) in &self.views {
            let w_rating = match self.ratings.get(*owner) {
                Some(s) => s,
                None => continue,
            };
            // 衰减基准是 last_check —— 我上次**成功**取到它观点的时间，
            // 而不是 published_at（对方自己声明的发布时间，可以随意伪造）。
            // 时间回拨时 dt 按 0 算（saturating_sub），不会产生负数或回绕。
            let dt = elapsed(stored.last_check, now);
            // badlist 折扣只乘在群体项的来源权重上（个人评分原封不动）。
            let w = w_rating * self.decay_factor(dt);
            let w = if has_penalty {
                w * self.penalty_of(*owner)
            } else {
                w
            };
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

        let group = aggregate(&items, self.cfg.agg_mode);

        combine(self.cfg.beta, personal, group)
    }

    // ---------- 统计 ----------

    pub fn stats(&self) -> (u64, u64, usize, usize) {
        (
            self.stat_requests,
            self.stat_hits,
            self.views.len(),
            self.ratings.len(),
        )
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
    /// `penalty_strength` 显式写 1.0（不折扣），让这 8 条冒烟测试继续钉住
    /// 「来源级衰减 + β 混合」的旧行为；badlist 的行为见
    /// `tests/badlist.rs`。
    fn eng() -> Engine {
        Engine::with_seed(
            EngineConfig {
                beta: 0.3,
                half_life: 72,
                penalty_strength: 1.0,
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
        assert!(
            (q - 0.7).abs() < 1e-4,
            "应为 0.3×0 + 0.7×1.0 = 0.7，实际 {q}"
        );
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
        e.on_view(
            f1,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]),
            0,
            0,
        );
        e.on_view(
            f2,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]),
            0,
            0,
        );

        let at0 = e.query(target, 0);
        let at500 = e.query(target, 500);
        let at100000 = e.query(target, 100_000);
        assert!(
            (at0 - at500).abs() < 1e-6,
            "全部陈旧时结果不变: {at0} vs {at500}"
        );
        assert!(
            (at0 - at100000).abs() < 1e-6,
            "再久也不变: {at0} vs {at100000}"
        );
        assert!(
            (at0 - 0.35).abs() < 1e-4,
            "0.3×0 + 0.7×0.5 = 0.35，实际 {at0}"
        );
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
        e.on_view(
            stale,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]),
            0,
            0,
        );
        e.on_view(
            fresh,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]),
            0,
            0,
        );
        let both_fresh = e.query(target, 0);

        // 只刷新 fresh：它权重变 1.0，stale 降到 0.5（一个半衰期）
        e.on_view(
            fresh,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 0.0)]),
            72,
            72,
        );
        let fresh_wins = e.query(target, 72);

        assert!(
            fresh_wins < both_fresh,
            "新鲜的低分来源应压过陈旧的高分来源: {both_fresh} -> {fresh_wins}"
        );
        // group = (0.5×1.0 + 1.0×0.0)/(1.5) = 0.3333
        assert!(
            (fresh_wins - 0.3 + 0.7 * (1.0 / 3.0) - 0.3).abs() < 1e-4
                || (fresh_wins - (0.7 * (1.0 / 3.0))).abs() < 1e-4,
            "应为 0.7×(1/3)=0.2333，实际 {fresh_wins}"
        );
    }

    /// S5：全部离线很久，分数只降不崩，且非负
    #[test]
    fn s5_all_stale_never_negative() {
        let mut e = eng();
        let friend = e.intern(key(1));
        let target = e.intern(key(9));
        e.rate(friend, 1.0, 0);
        e.tick(0, 10);
        e.on_view(
            friend,
            CompressedView::from_pairs(ViewParams::default(), &[(key(9), 1.0)]),
            0,
            0,
        );
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
        assert!(
            (q - 0.3 * 0.1).abs() < 1e-5,
            "份额应为 0.1，声望 0.03，实际 {q}"
        );
        // 打到 100 个人，份额降到 1/100
        let mut e2 = eng();
        let mut first = 0;
        for i in 0..100u32 {
            let x = e2.intern(key((i % 250) as u8));
            if i == 0 {
                first = x;
            }
            e2.rate(x, 0.5, 0);
        }
        let q2 = e2.query(first, 0);
        assert!(q2 < q, "打 100 人后单人份额应更小: {q2} vs {q}");
    }
}
