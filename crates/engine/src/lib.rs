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

use std::collections::{HashMap, HashSet};

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
    views: HashMap<AccountIdx, StoredView>,
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
            views: HashMap::new(),
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
    pub fn tick(&mut self, now: Time, k: usize) -> Vec<FetchRequest> {
        let current: Vec<AccountIdx> = self.ratings.iter().map(|(idx, _, _)| idx).collect();
        self.sampler.sync(&current);

        let _ = now;
        let picked = self.sampler.draw(k, &mut self.rng);
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
        self.ratings.touch(owner, now);
    }

    /// 应用层取观点失败时调用：什么都不做（时间戳不更新，衰减自然发生）。
    pub fn on_view_failed(&mut self, _owner: AccountIdx, _now: Time) {}

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

    // ---------- 计算 ----------

    /// 查某个账户在自己眼中的声望。
    pub fn query(&self, target: AccountIdx, now: Time) -> Score {
        // 影响力预算：自己的评分也要归一化，总配额恒为 1
        let total_rating: f32 = self.ratings.iter().map(|(_, s, _)| s).sum();
        let raw_personal = self.ratings.get(target).unwrap_or(0.0);
        let personal = if total_rating > 0.0 { raw_personal / total_rating } else { 0.0 };

        let target_key = match self.accounts.key_of(target) {
            Some(k) => k,
            None => return combine(self.cfg.beta, personal, None),
        };

        let mut items: Vec<(f32, f32)> = Vec::with_capacity(self.views.len());
        for (owner, stored) in &self.views {
            let w_rating = match self.ratings.get(*owner) {
                Some(s) => s,
                None => continue,
            };
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

        let group = aggregate(&items, self.cfg.agg_mode);
        combine(self.cfg.beta, personal, group)
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

    fn eng() -> Engine {
        Engine::with_seed(EngineConfig { beta: 0.3, half_life: 72, ..Default::default() }, 42)
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
