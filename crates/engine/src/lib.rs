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
    /// `total_mass` 覆盖到的账户范围：`on_view` 时刻的已知账户数（编号上界）。
    ///
    /// [AccountIdx] 由 `intern` 单调递增分配，所以 `target >= mass_covers`
    /// 精确等价于「这个账户是在这份观点缓存之后才登记进来的」。它的读数没有进过
    /// `total_mass`，用这个偏小的分母去归一化只会把它抬到 1.0 —— [Engine::query]
    /// 因此对这类账户一律按「这份观点没表态」处理。
    ///
    /// 不记这个上界的后果：`on_view` 时对未登记账户的声明完全不进 `total_mass`
    /// （分母只剩 1e-6 的下限），账户一登记，每条声明都饱和成 1.0
    /// （实测 50 条声明 ⇒ Σv = 50，合规应 ≤ 1）。
    pub mass_covers: u32,
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
    /// 观点缓存的**条目数**上限；`0`（默认）= 无限制，与引入本字段之前逐位一致。
    ///
    /// 设计规格 §0.3 把「观点文件设条目数 / 字节数上限，超出即丢弃并视同失败」
    /// 写成**唯一的**资源保护，但旧实现里 [Engine::on_view] 无条件插入
    /// `views`，`views` 只会被应用层主动调用的 [Engine::prune_stale]
    /// 缩小（审计 A13-spec U-01 判「未实现」）。恶意应用层持续
    /// `on_view` 不同 owner 就能让内存无界增长（每份视图 40 KiB 起）。
    ///
    /// 字节数上限由形状参数固定：每份视图恒为
    /// `view_params.buckets × view_params.hashers × 4` 字节，参数不一致的视图
    /// 在入口就被丢弃，所以**条数上限即内存上限**
    /// `max_views × 视图字节数`（另加每条约几十字节的
    /// [StoredView] 元数据与 `Vec` 头）。
    ///
    /// 判定规则见 [Engine::on_view] 的「容量上限」一节，要点：
    /// **只有「新 owner 且已满」才丢；已缓存的 owner 更新既有条目永远接受**
    /// （原地覆盖不增长内存）。
    pub max_views: usize,
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
            // 0 = 无上限（旧行为）。默认值必须逐位保持：任何一个既有实验/测试
            // 都可能喂超过（将来）某个上限的 owner 数量，改默认值等于改口径。
            max_views: 0,
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

/// [Engine::on_view] 丢弃一份观点的原因（安全加固 P2）。
///
/// 两种原因的**处理语义完全相同**（见 [Engine::on_view]）：整条丢弃、按
/// [Engine::on_view_failed] 的语义只给 owner 累计连败、不刷新 `last_check`、
/// 不进缓存、不计命中。区分它们只为可观测性：容量丢弃说明 `max_views` 已经
/// 顶满（资源保护在起作用）；非有限丢弃说明**有人在喂脏视图**（安全信号）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// 缓存已达 [EngineConfig::max_views] 且这是新 owner。
    Capacity,
    /// 这份观点的 `total_mass` 不是有限值（NaN / ±∞）。
    NonFinite,
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
    /// 被 [Engine::on_view] **整条丢弃**的观点累计数（安全加固 P2）。
    ///
    /// 与 [Engine::stats] 分开记，是为了让 `stats()` 的四元组**形状与含义
    /// 一个字节都不变**（既有实验与测试直接按位置解构它）。这里含两类丢弃：
    /// 容量（[EngineConfig::max_views]）与非有限读数；`view.params()` 不匹配
    /// 的那类仍然**只**静默丢弃、不计入——它属于协议层的「无法解释的字节」，
    /// 与「可解释但被拒」是两件事。细分计数见 [Engine::rejected_breakdown]。
    stat_rejected_view: u64,
    /// 其中因容量上限被丢的条数。
    stat_rejected_capacity: u64,
    /// 其中因读数非有限（NaN / ±∞）被丢的条数。
    stat_rejected_nonfinite: u64,
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
            stat_rejected_view: 0,
            stat_rejected_capacity: 0,
            stat_rejected_nonfinite: 0,
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
    ///
    /// # 参数校验
    ///
    /// `view.params()` 必须与 [EngineConfig::view_params] 完全一致
    /// （`buckets` / `hashers` / `seed`）。不一致的视图**整条丢弃**：
    /// 不计 [Engine::stats] 的命中、不进缓存、不清退避计数、也不更新时钟 ——
    /// 对这个引擎来说它是无法解释的字节。
    ///
    /// 这是必需的输入校验，不是优化：`hashers` 由对端声明，不校验时 16×4096
    /// 的视图能把单次 `on_view` 从 0.9 ms 拉到 572 ms（×609），单视图体积
    /// 上限 2^26 单元 = 256 MiB 也会原样进入缓存。
    ///
    /// # 容量上限（设计规格 §0.3，安全加固 P2）
    ///
    /// [EngineConfig::max_views] 给出缓存条目数上限，`0` 表示无限制。
    ///
    /// * **新 owner 且 `views.len() >= max_views`** → 整条**丢弃**，并按
    ///   [Engine::on_view_failed] 的语义处理：只给该 owner 累计连败、**不**刷新
    ///   `last_check`、不进缓存、不计 [Engine::stats] 的命中，只累加 [Engine::rejected_count] 与细分计数。
    ///   调用方本来就分不清「成功」与「失败」，所以丢弃对上层是透明的：下一次
    ///   [Engine::tick] 会按 1/2^n 退避它，链路自然收敛。
    /// * **owner 已在缓存里** → 原地覆盖既有条目，**永远接受**：内存不增长，
    ///   丢它没有任何资源保护收益，反而会阻断正常的观点刷新。
    /// * `max_views = 0` → 两条判定都不触发，行为与引入本字段之前**逐位一致**。
    ///
    /// 「字节数上限」由形状参数决定：`view.params()` 必须与
    /// [EngineConfig::view_params] 一致，于是每份缓存视图恒占
    /// `buckets × hashers × 4` 字节，**条目数上限即内存上限**。
    ///
    /// # 非有限读数的防护
    ///
    /// `total_mass` 是对已知账户的 `query` 求和。只要任一 `query` 返回 NaN 或
    /// ±∞，这份观点就**整条丢弃**（语义与容量路径完全相同），细分计数见
    /// [Engine::rejected_breakdown]。
    ///
    /// 三个数值细节决定了必须**在求和过程中**逐个检查，而不是求和之后再查：
    ///
    /// 1. `clamp(0.0, 1.0)` 把 +∞ 收成 1.0、把 -∞ 收成 0.0——`total_mass` 之外
    ///    看不出任何异常，但这份视图的读数已经被静默改写；
    /// 2. NaN 会传染给求和结果，然而 `NaN.max(1e-6)` **精确等于 `1e-6`**
    ///    （`f32::max` 返回非 NaN 的那个操作数），所以「在 `.max()` 之后查
    ///    `is_finite()`」永远查不出 NaN——它只会把总质量塌到 1e-6 这个
    ///    「放大器」档位，让每条读数都被抬到 1.0（审计 A6 D4 记的同一个洞）；
    /// 3. 求和本身也可能溢出成 +∞（已知账户很多、每个读数接近 1 也不会，
    ///    但防御性地保留 `total_mass.is_finite()` 这一条）。
    ///
    /// 注意正常路径上 `decode` 已经拒绝非有限负载，这里是**防御纵深**：
    /// 应用层可能用 [CompressedView::merge] 把 `0 × ∞` 之类的 NaN 合进视图，
    /// 或者自己构造 `f32` 数组（见 `crates/engine/tests/capacity.rs`）。
    pub fn on_view(
        &mut self,
        owner: AccountIdx,
        view: CompressedView,
        published_at: Time,
        now: Time,
    ) {
        // 参数（形状 + 种子）是全网约定；不一致的草图桶布局不同，读数没有意义。
        if view.params() != self.cfg.view_params {
            return;
        }
        self.clock = now;
        // —— 容量上限（设计规格 §0.3）——
        // 已缓存的 owner 是原地覆盖，不增长内存，永远接受；只有「新 owner 且已满」
        // 才丢。判定放在 noise_floor 与质量扫描之前：被丢的观点一分钱计算都不做。
        if self.cfg.max_views > 0
            && self.views.len() >= self.cfg.max_views
            && !self.views.contains_key(&owner)
        {
            self.reject_view(owner, Rejection::Capacity);
            return;
        }
        let noise = view.noise_floor(1000, noise_seed(owner));
        // 接收方自己估计总质量：对已知账户求和。发布方无法伪造这个估计。
        //
        // **求和过程中逐个校验有限性**，而不是求和之后再查：
        // `clamp(0.0, 1.0)` 会把 +∞ 收成 1.0、把 -∞ 收成 0.0（静默丢掉信息），
        // NaN 则会一路传染给和；更致命的是 `NaN.max(1e-6)` 精确等于 1e-6
        // （`f32::max` 返回非 NaN 的那个操作数），于是「查 .max() 之后的值」
        // 永远查不出 NaN，还会把这份视图的总质量塌到 1e-6 这个放大器档位。
        // 所以必须看 `query` 的原始返回值。
        let mut total_mass = 0.0f32;
        let mut all_finite = true;
        for i in 0..self.accounts.len() as u32 {
            if let Some(k) = self.accounts.key_of(i) {
                let q = view.query(&k);
                if !q.is_finite() {
                    all_finite = false;
                    break;
                }
                total_mass += q.clamp(0.0, 1.0);
            }
        }
        let total_mass = total_mass.max(1e-6);
        // —— 非有限读数防护 ——
        // `max(1e-6)` 对 NaN 没有定义明确的结果（这里 total_mass 本身就是 NaN），
        // +∞ 则会被原样留下、把每条读数都压成 0。与其归一化到 1e-6 让整份视图的
        // 读数被放大到 1.0，不如整条丢弃：非有限值在协议里就是「无法解释的字节」。
        if !all_finite || !total_mass.is_finite() {
            self.reject_view(owner, Rejection::NonFinite);
            return;
        }
        self.stat_hits += 1;
        self.views.insert(
            owner,
            StoredView {
                view,
                published_at,
                last_check: now,
                noise,
                total_mass,
                // 这份 total_mass 只统计了「此刻已知」的 0..len 号账户。
                mass_covers: self.accounts.len() as u32,
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

    /// 统一的「观点被拒」路径：累加计数，并按失败语义处理 owner。
    ///
    /// 规格 §0.3 的原文是「超出即丢弃并**视同失败**」。这里刻意直接复用
    /// [Engine::on_view_failed] 的两条效果（时钟前移 + 连败 +1），而不是另写一遍，
    /// 免得两条路径日后漂移。
    ///
    /// 代价说明：丢弃一条视图是 O(log n)（计数 + [tm_store::PositiveList] 里的
    /// 一次 BTreeMap 查询），而被丢弃的 `view` 参数在函数返回时立刻释放——
    /// 容量上限挡住的正是「留在缓存里」的那部分内存，不是丢弃动作本身。
    fn reject_view(&mut self, owner: AccountIdx, why: Rejection) {
        self.stat_rejected_view += 1;
        match why {
            Rejection::Capacity => self.stat_rejected_capacity += 1,
            Rejection::NonFinite => self.stat_rejected_nonfinite += 1,
        }
        // 与 on_view_failed 完全同一语义：只加连败，不刷新 last_check。
        self.ratings.record_failure(owner);
    }

    /// 完整的信任分布：所有账户分到的比值，**只要我给至少一个人打过正分，
    /// 总和就恒为 1**。
    ///
    /// 这是对外的最终结果。每一个数值就是"我把多少比例的信任交给了它"，
    /// 乘以 100 即为置信度百分比。结果按账户编号升序，只包含 query > 0 的账户。
    ///
    /// # 不变量是**有条件**的
    ///
    /// 「Σ = 1」只在正分清单非空且 β > 0 时成立——**除非我还没有给任何人打正分**。
    /// 清单为空时，我从未表达过任何偏好，没有归一化的分母，分布**未定义**：
    /// 本方法返回空 `Vec`、`Σ = 0`（空迭代器求和得到 -0.0），而不是
    /// 「每个已知账户 1/N」或全 0 的占位分布。调用方必须把空结果当作
    /// 「无分布」处理，不能拿它去做百分比换算。
    ///
    /// 这条例外是刻意保留的：不伪造均匀分布，也不改变返回类型；
    /// 两种可选修法及其代价见 `审计/P0-gap.md` ①。β = 0 且没有任何观点缓存时
    /// 同样会退化成空分布，因为此时每个 query 都返回 0。
    ///
    /// 本方法是只读的：不写折扣表、不改观点缓存、不动抽样轮次。
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
    /// **这次算出的折扣（按 f32 口径）不低于已有折扣**（min 语义下重复报告不会更狠）。
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
        //
        // **遍历的是正分清单（我评过谁），不是观点缓存（我手上有谁的观点）**，
        // 与 query 的分母完全同一个集合。没有观点的来源也在这里出现，读数是
        // None（弃权票）：它的份额照样进目标方程的分母，但不进 ā。
        // 只遍历 views 的话，求出的 f 只让「表过态的那部分」回到 ā，
        // 弃权份额会把结果再往下拖（实测 1/3 → 0.25）。
        let mut sources: Vec<SourceSnapshot> = Vec::with_capacity(self.ratings.len());
        for (owner, rating, _) in self.ratings.iter() {
            if !rating.is_finite() || rating <= 0.0 {
                continue;
            }
            let share = f64::from(rating / total);
            // 有观点：衰减基准是 last_check（我上次**成功**取到它观点的时间）；
            // 没有观点：没有衰减基准，因子按 1。这与 query 逐项一致。
            let (decay, reading) = match self.views.get(&owner) {
                Some(stored) => {
                    let r = if idx < stored.mass_covers {
                        Some(f64::from(self.reading_of(stored, &key)))
                    } else {
                        // 这份观点在 on_view 时只统计了 0..mass_covers 号账户；
                        // A 之后才登记，它没进过 total_mass，用偏小的分母归一化
                        // 会把它抬到 1.0。query 对这类账户按「没表态」处理，
                        // 闭式解必须同样处理，否则读数和实测会在同一份字节上分叉。
                        None
                    };
                    (
                        f64::from(self.decay_factor(elapsed(stored.last_check, now))),
                        r,
                    )
                }
                None => (1.0, None),
            };
            let weight = share * decay;
            if !weight.is_finite() || weight <= 0.0 {
                continue;
            }
            sources.push(SourceSnapshot {
                idx: owner,
                weight,
                reading,
            });
        }
        if sources.len() < 2 {
            // 0/1 个来源不可能有「高于平均」的人，闭式解也一定退出；
            // 提前返回省掉两次 Vec 分配。
            return 0;
        }

        let weights: Vec<f64> = sources.iter().map(|s| s.weight).collect();
        let readings: Vec<Option<f64>> = sources.iter().map(|s| s.reading).collect();
        let sol = badlist::solve_uniform(&weights, &readings);
        let f = badlist::apply_strength(sol.f, lambda);
        if f >= 1.0 {
            return 0;
        }

        // S+ 里所有来源共享同一个 f；已有更低的折扣时 min 保留旧值。
        // 不连乘、不复合：两个各罚 50% 仍然是 50%，不会变成 75%，
        // 重复报告同一个坏账户也不会越压越狠。
        let mut lowered = 0usize;
        // 比较在 Score（f32）上做，而不是在 f64 上做：折扣表里存的本来就是 f32，
        // 用 f64 比较时「闭式解算出 0.9999999999、转成 f32 恰好是 1.0」这类
        // 浮点噪声会被当成一次压低，既污染折扣表（写进一个 1.0 的空条目），
        // 又让返回值「真正被压低的来源数」多算一个。目标线已经达成时
        // （G_A = ā）必须返回 0，这是这条接口唯一的可观测承诺。
        let new_discount = f as Score;
        for (i, src) in sources.iter().enumerate() {
            if !sol.above[i] {
                continue;
            }
            let old = self.penalty_of(src.idx);
            if new_discount < old {
                self.penalties.insert(src.idx, new_discount);
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
    /// 分子先夹到 [0,1]，再除以同口径的 `total_mass`，结果再夹一次；
    /// 低于噪声底视为没表态。
    ///
    /// 顺序不能反。`total_mass` 是各已知账户的 `min(q,1)` 之和（受影响的账户数
    /// 记为 K），分子不先夹紧时，一条声明 `c >= K` 会让 `q/T = c/K >= 1`，
    /// 一个来源的预算就从 1 变成 K——实测 18 个女巫（账户的 4.5%）声明各 1000，
    /// 拿走 29.37% 的信任。分子先夹紧后，合规视图（全部读数 ≤ 1）下
    /// `q ≤ 1` 且 `T >= q`，两种写法逐位同值；只有超量声明会被压回预算内。
    ///
    /// [Engine::query] 与 badlist 必须走同一份实现，否则两条路径会对同一份字节
    /// 给出不同的读数。非有限值一律当 0：badlist 的所有中间量都必须是有限数，
    /// 而 NaN 在协议里就是「没表态」。
    fn reading_of(&self, stored: &StoredView, key: &AccountKey) -> Score {
        // 分子先夹紧，与分母（Σ min(q,1)）同一口径。
        let raw = stored.view.query(key).clamp(0.0, 1.0) / stored.total_mass;
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
    /// 群体项[u] = Σ_v 份额[v]·fs[v]·penalty[v]·观点[v][u]
    ///             ÷ Σ_v 份额[v]·fs[v]·penalty[v]             // v 遍历「我评过的全部来源」
    /// 声望[u]   = β·个人项[u] + (1−β)·群体项[u]
    /// ```
    ///
    /// * 份额 = 评分 ÷ 评分总和（影响力预算，总配额恒为 1），个人项永不衰减；
    ///
    /// `penalty[v]` 是 badlist 折扣（[Engine::penalty_of]），缺省 1.0。
    /// 它只压群体项里对该来源的**话语权**，不改个人评分，也不改 `fs`。
    ///
    /// * `fs` 是来源级衰减（[Engine::decay_factor]），只改来源**之间**的相对权重；
    /// * **分母是「我评过的全部来源」**，与有没有取到观点无关：还没有观点的来源
    ///   照样拿走自己那份份额、投一张 0 票（弃权），而不是从分母里消失。
    ///   否则覆盖率越低，先到的那一份观点越独占群体项——N=101、只取到 1 份时，
    ///   被背书账户的声望是「同样评过、但没人背书」的诚实账户的 236.67 倍；
    ///   分母补齐后正好是 1/β = 3.33 倍。
    /// * 每个读数按该视图的 `total_mass` 归一化：**分子先夹到 0~1 再除**，
    ///   与分母 `Σ min(q,1)` 同口径。反过来的话，声明值 ≥ K 时一个来源的预算
    ///   会从 1 变成 K，合规视图则逐位不变；
    /// * 观点在 `on_view` 那一刻只统计了前 [StoredView::mass_covers] 个账户；
    ///   之后才登记的账户没有进过分母，一律按「这份观点没表态」处理；
    /// * 低于噪声底的读数视为「没表态」，该来源的这一票按 0 计；
    /// * 一条观点都没有时群体项为 0，结果退化为 β·个人项。
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

        // 遍历的是正分清单（我评过谁），不是观点缓存（我手上有谁的观点）：
        // 没有观点的来源贡献 (份额, 0.0) —— 权重进分母、值按 0。
        let mut items: Vec<(f32, f32)> = Vec::with_capacity(self.ratings.len());
        // 折扣表为空（默认）时完全跳过查表与乘法：热路径零额外开销，
        // 也从代码上保证「没人调用 report_bad_account 就与改造前逐位一致」。
        let has_penalty = !self.penalties.is_empty();
        for (owner, w_rating, _) in self.ratings.iter() {
            // 有观点：衰减基准是 last_check —— 我上次**成功**取到它观点的时间，
            // 而不是 published_at（对方自己声明的发布时间，可以随意伪造）。
            // 时间回拨时 dt 按 0 算（saturating_sub），不会产生负数或回绕。
            // 没有观点：没有衰减基准，因子按 1。
            let (decay, v) = match self.views.get(&owner) {
                Some(stored) => {
                    // 这条观点在 on_view 时只统计了 0..mass_covers 号账户。
                    // 之后才 intern 的账户没有进过 total_mass，用偏小的分母归一化
                    // 会把它抬到 1.0，因此按「没表态」处理。
                    let v = if target < stored.mass_covers {
                        self.reading_of(stored, &target_key)
                    } else {
                        0.0
                    };
                    (self.decay_factor(elapsed(stored.last_check, now)), v)
                }
                None => (1.0, 0.0),
            };
            // badlist 折扣只乘在群体项的来源权重上（个人评分原封不动）。
            let w = w_rating * decay;
            let w = if has_penalty {
                w * self.penalty_of(owner)
            } else {
                w
            };
            if !w.is_finite() || w <= 0.0 {
                continue;
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

    /// 被 [Engine::on_view] **整条丢弃**的观点累计条数（安全加固 P2）。

    /// 含两类：容量上限（[EngineConfig::max_views]）与非有限读数。
    /// `view.params()` 不匹配的视图仍然只静默丢弃、不计入这里。

    /// 与 [Engine::stats] 分开暴露是刻意的：`stats()` 的四元组形状与含义
    /// 保持不变（既有实验按位置解构它），新计数走新接口。
    ///
    /// 这个数字**单调递增**、只增不减（[Engine::prune_stale] 清理旧观点不会让它
    /// 回退），它是「被拒了多少次」的累计事件计数，不是「现在有多少条被拒」。
    #[must_use]
    pub fn rejected_count(&self) -> u64 {
        self.stat_rejected_view
    }

    /// 丢弃原因的细分计数：`(因容量被丢, 因读数非有限被丢)`。

    /// 两者之和恒等于 [Engine::rejected_count]。容量那一项大于 0 说明
    /// [EngineConfig::max_views] 正在顶满（资源保护生效）；非有限那一项大于 0
    /// 说明有人在喂脏视图（安全信号，值得上层告警）。
    #[must_use]
    pub fn rejected_breakdown(&self) -> (u64, u64) {
        (self.stat_rejected_capacity, self.stat_rejected_nonfinite)
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
