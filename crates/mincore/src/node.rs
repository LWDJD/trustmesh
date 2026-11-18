//! 计算与归一化：整个内核的**全部**算法都在这个文件里。
//!
//! # 声望公式
//!
//! ```text
//! 个人项[u] = 我的正分[u] / Σ 我的正分            // 永不衰减，份额之和 ≡ 1
//! 群体项[u] = Σ_v w[v] · v观点[u] / Σ_v w[v]      // w[v] = 我的份额[v] × 0.5^(Δt/72h)
//! 声望[u]   = β · 个人项[u] + (1−β) · 群体项[u]    // β = 0.3
//! 最后整体除以 Σ_u 声望[u]（全局归一化，使 Σ ≡ 1）
//! ```
//!
//! # 归一化的**顺序**是这一节的全部重点
//!
//! 现行实现是「先除后夹紧」：`v = clamp(草图读数 / 接收端估计的总质量, 0, 1)`。
//! 接收端的总质量是把「所有已知账户的读数先夹到 1 再求和」得到的估计值，
//! 于是只要来源声明 K 个超量条目，每条都会在除法之后撞上 1.0 的上限——
//! **一个来源的预算从 1 变成 K**。
//!
//! 这里是「先夹后除」，并且夹的是**定点整数**：
//!
//! ```text
//! q ∈ [0, 65535]                 // 入口就已合法，不需要再夹
//! v = q / Σ_声明 q                // Σ 是精确整数和，不是估计
//! ```
//!
//! 于是 `Σ_声明 v ≡ 1`、单条 `v ≤ 1` 自动成立。没有饱和路径，也没有
//! 「声明了但本机不知道的账户」的特例。
//!
//! # 声明了但本机不认识的账户
//!
//! **定义**：它们的分数**计入分母**（花掉来源的预算），但**不产生任何声望**
//! （本机不知道它，也就无从谈起）。
//!
//! 这个选择的后果是保守的：一个来源想用「1000 个我不认识的女巫」来稀释/占用预算，
//! 代价是它对我所认识的人的话语权等比缩小。反过来（只按认识的部分归一化）
//! 会让「声明不认识的账户」变成免费操作，那就又回到预算可被绕过的老路上。
//!
//! # 全局归一化
//!
//! 最后一步一定把 `Σ` 归一到 1。两种退化情况：
//! * 正分清单为空 → 返回空分布（没有任何信息，不编造）；
//! * 群体项没有任何来源可用 → `声望 = β·个人项`，照样归一化。

use std::collections::BTreeMap;

use crate::list::{Acc, PositiveList, Time};
use crate::round::{Rng, RoundBitmap};
use crate::view::SparseView;

/// 内核参数。整个内核只有这 4 个旋钮。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// 个人项权重 β。
    pub beta: f32,
    /// 衰减半衰期（与 [Time] 同单位），默认 72 小时。
    pub half_life: Time,
    /// 观点容量 cap。
    pub cap: usize,
    /// 是否启用时间衰减。`false` 时因子恒为 1（消融用）。
    pub decay_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            beta: 0.3,
            half_life: 72,
            cap: 512,
            decay_enabled: true,
        }
    }
}

/// 一份收到的观点。
#[derive(Clone, Debug)]
pub struct Received {
    /// 表本身。
    pub view: SparseView,
    /// 对方**声称**的发布时间。只做展示，**绝不用于衰减**（对方能随便填）。
    pub published_at: Time,
    /// 我上次**成功**取到它的时间。这是唯一的衰减基准——我自己观察到的，对方伪造不了。
    pub last_check: Time,
}

/// 归一化后的声望分布。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Distribution {
    /// `(账户, 声望)`，按账户号升序，只含 `> 0` 的条目。
    pub entries: Vec<(Acc, f32)>,
    /// 归一化**之前**的总量（诊断用；正常情况等于 Σ 声望）。
    pub mass: f64,
}

impl Distribution {
    /// 查某个账户的声望；不在分布里返回 0。
    #[must_use]
    pub fn get(&self, acc: Acc) -> f32 {
        match self.entries.binary_search_by_key(&acc, |x| x.0) {
            Ok(i) => self.entries[i].1,
            Err(_) => 0.0,
        }
    }

    /// 归一化之后的总和（应为 1.0；空分布为 0）。
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.entries.iter().map(|&(_, v)| f64::from(v)).sum()
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 指数衰减因子 `0.5^(dt / half_life)`。
///
/// 边界与 [tm_agg::decay] 逐位一致（这里重写了一份，因为生产代码只允许 std）：
/// `half_life == 0` 按 1 处理；`dt == 0` 精确返回 1.0；下溢时返回最小正 f32，
/// 永不返回 0 / NaN / 负数。
#[must_use]
pub fn decay(dt: Time, half_life: Time) -> f32 {
    let h = if half_life == 0 { 1 } else { half_life };
    if dt == 0 {
        return 1.0;
    }
    let v = 0.5f64.powf(dt as f64 / h as f64);
    if !v.is_finite() || v <= 0.0 {
        return f32::MIN_POSITIVE;
    }
    let f = v as f32;
    if f > 0.0 && f.is_finite() {
        f
    } else {
        f32::MIN_POSITIVE
    }
}

/// 一个节点：正分清单 + 抽样器 + 收到的观点。
#[derive(Clone, Debug)]
pub struct Node {
    cfg: Config,
    list: PositiveList,
    sampler: RoundBitmap,
    /// 只为「我打过正分」的来源保留观点：别人硬塞过来的观点我根本不收，
    /// 这样 `|views| ≤ |list|`，内存有界且与攻击者的推送速率无关。
    views: BTreeMap<Acc, Received>,
    rng: Rng,
    stat_views: u64,
    stat_rejected: u64,
}

impl Node {
    /// 新建节点。`seed` 只影响抽样顺序，不影响任何计算结果。
    #[must_use]
    pub fn new(cfg: Config, seed: u64) -> Self {
        Self {
            cfg,
            list: PositiveList::new(),
            sampler: RoundBitmap::new(),
            views: BTreeMap::new(),
            rng: Rng::new(seed),
            stat_views: 0,
            stat_rejected: 0,
        }
    }

    // ---------------- 正分清单 ----------------

    /// 记录一次正分（`score <= 0` 表示撤销），并把 `last_check` 刷新为 `now`。
    pub fn rate(&mut self, acc: Acc, score: f32, now: Time) {
        self.list.set(acc, score, now);
    }

    /// 我的正分清单（只读）。
    #[must_use]
    pub fn claims(&self) -> &PositiveList {
        &self.list
    }

    /// 我对某人的份额。
    #[must_use]
    pub fn share_of(&self, acc: Acc) -> Option<f64> {
        self.list.share(acc)
    }

    // ---------------- 抽样 ----------------

    /// 抽一批要请求观点的来源。
    ///
    /// 均匀、不放回、抽完自动开新一轮、**不退避**（连败计数不参与）。
    pub fn tick(&mut self, k: usize) -> Vec<Acc> {
        self.sampler.draw(&self.list, k, &mut self.rng)
    }

    /// 兜底探测 / 快速刷新：本轮剩下的一次全取，不消耗随机数。
    pub fn probe(&mut self) -> Vec<Acc> {
        self.sampler.draw_all(&self.list)
    }

    /// 本轮还剩几个没抽。
    #[must_use]
    pub fn round_remaining(&self) -> usize {
        self.sampler.remaining()
    }

    // ---------------- 观点收发 ----------------

    /// 收到一份观点。成功入库返回 `true`，被拒返回 `false`。
    ///
    /// 拒绝条件（全部是结构性的，不需要逐条判脏）：
    /// * 我对 `owner` 没有正分 —— 没关系的来源，观点对我没有权重，存了也没用；
    /// * 表是空的（`Σq == 0`）—— 没有信息。
    ///
    /// 入库时：`last_check = now`、连败清零。
    pub fn on_view(&mut self, owner: Acc, view: SparseView, published_at: Time, now: Time) -> bool {
        if !self.list.has(owner) || view.total_q() == 0 {
            self.stat_rejected += 1;
            return false;
        }
        self.stat_views += 1;
        self.views.insert(
            owner,
            Received {
                view,
                published_at,
                last_check: now,
            },
        );
        self.list.record_success(owner, now);
        true
    }

    /// 取观点失败：连败 +1，**不动** `last_check`（失败不是「刚取到」）。
    pub fn on_view_failed(&mut self, owner: Acc) {
        self.list.record_failure(owner);
    }

    /// 已缓存的观点数。
    #[must_use]
    pub fn view_count(&self) -> usize {
        self.views.len()
    }

    /// 收发统计 `(成功, 被拒)`。
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.stat_views, self.stat_rejected)
    }

    // ---------------- 计算 ----------------

    /// 权重表：¤[(来源, 权重)]¤（按来源账户号升序）+ ¤Σ 权重¤。
    ///
    /// 权重 = 我的份额 × 衰减；没有观点、份额为零、总质量为零的来源直接跳过
    /// （它们没有意见，不该稀释别人的意见）。
    fn weights(&self, now: Time) -> (Vec<(Acc, f64)>, f64) {
        let mut out: Vec<(Acc, f64)> = Vec::with_capacity(self.views.len());
        let mut wsum = 0.0f64;
        // 总分只算一次：逐个来源调 share() 会把它变成 O(来源数 × 清单长度)。
        let total = self.list.total();
        if total <= 0.0 {
            return (out, 0.0);
        }
        for (&owner, rec) in &self.views {
            if rec.view.total_q() == 0 {
                continue;
            }
            let share = match self.list.get(owner) {
                Some(s) => f64::from(s) / total,
                None => continue,
            };
            if !(share > 0.0) {
                continue;
            }
            let age = now.saturating_sub(rec.last_check);
            let f = if self.cfg.decay_enabled {
                f64::from(decay(age, self.cfg.half_life))
            } else {
                1.0
            };
            let w = share * f;
            if !(w > 0.0) {
                continue;
            }
            wsum += w;
            out.push((owner, w));
        }
        (out, wsum)
    }

    /// 群体项的**全量**分子：¤账户 -> Σ w·v¤，以及 ¤Σ w¤。
    ///
    /// 一次遍历所有来源的所有槽位，代价 ¤O(Σ cap)¤，与「网络里有多少账户」无关。
    /// 这是 ¤distribution¤ 走的路径。
    fn group_term(&self, now: Time) -> (BTreeMap<Acc, f64>, f64) {
        let (weights, wsum) = self.weights(now);
        let mut num: BTreeMap<Acc, f64> = BTreeMap::new();
        if !(wsum > 0.0) {
            return (num, 0.0);
        }
        for (owner, w) in &weights {
            let rec = &self.views[owner];
            let t = f64::from(rec.view.total_q());
            for &(acc, q) in rec.view.slots() {
                *num.entry(acc).or_insert(0.0) += w * (f64::from(q) / t);
            }
        }
        (num, wsum)
    }

    /// 单个账户的群体项分子：¤(Σ w·v, Σ w)¤。
    ///
    /// 与 [Self::group_term] 同口径、同累加顺序（按来源账户号升序），
    /// 但只对**一个**账户做二分查找，代价 ¤O(来源数 · log cap)¤。
    /// 因此 ¤query¤ 不需要为「我不关心的账户」付钱。
    fn group_of(&self, acc: Acc, now: Time) -> (f64, f64) {
        let (weights, wsum) = self.weights(now);
        if !(wsum > 0.0) {
            return (0.0, 0.0);
        }
        let mut num = 0.0f64;
        for (owner, w) in &weights {
            let rec = &self.views[owner];
            let q = rec.view.q_of(acc);
            if q == 0 {
                continue;
            }
            num += w * (f64::from(q) / f64::from(rec.view.total_q()));
        }
        (num, wsum)
    }

    /// 某人的声望（**未**做全局归一化）。
    ///
    /// `β·个人项 + (1−β)·群体项`，取值范围 `[0, 1]`。
    #[must_use]
    pub fn query(&self, acc: Acc, now: Time) -> f32 {
        let total = self.list.total();
        if total <= 0.0 {
            return 0.0;
        }
        let personal = match self.list.get(acc) {
            Some(s) => f64::from(s) / total,
            None => 0.0,
        };
        let (num, wsum) = self.group_of(acc, now);
        let group = if wsum > 0.0 { num / wsum } else { 0.0 };
        let beta = f64::from(clamp01(self.cfg.beta));
        let v = beta * personal + (1.0 - beta) * group;
        v.clamp(0.0, 1.0) as f32
    }

    /// 全局声望分布，`Σ ≡ 1`。
    ///
    /// 复杂度 `O(|views| · cap + |清单|)` —— 与「网络里有多少个账户」**无关**。
    /// 这正是它对 `trust_distribution` 的 `O(n·m)` 的结构性优势：
    /// 稀疏表不需要为「我不认识的人」付钱。
    #[must_use]
    pub fn distribution(&self, now: Time) -> Distribution {
        let total = self.list.total();
        if total <= 0.0 {
            return Distribution::default();
        }
        let beta = f64::from(clamp01(self.cfg.beta));
        let (num, wsum) = self.group_term(now);

        let mut out: BTreeMap<Acc, f64> = BTreeMap::new();
        // 个人项：遍历顺序固定为账户号升序。
        for (acc, e) in self.list.iter() {
            let p = f64::from(e.score) / total;
            if p > 0.0 {
                *out.entry(acc).or_insert(0.0) += beta * p;
            }
        }
        // 群体项。
        if wsum > 0.0 {
            let coef = (1.0 - beta) / wsum;
            for (&acc, &g) in &num {
                *out.entry(acc).or_insert(0.0) += coef * g;
            }
        }
        // 全局归一化。
        let mass: f64 = out.values().sum();
        if !(mass > 0.0) {
            return Distribution::default();
        }
        let entries: Vec<(Acc, f32)> = out
            .into_iter()
            .filter(|&(_, v)| v > 0.0)
            .map(|(a, v)| (a, (v / mass) as f32))
            .collect();
        Distribution { entries, mass }
    }

    /// 把我当前的分布发布成一份观点：取 `top-cap` 条，量化成 u16 定点。
    ///
    /// 发布侧与接收侧用同一套归一化，所以 `publish` 出来的表
    /// 被别人接收后 `Σv ≡ 1`，预算恒为 1。
    #[must_use]
    pub fn publish(&self, now: Time) -> SparseView {
        let d = self.distribution(now);
        if d.entries.is_empty() {
            return SparseView::new(self.cfg.cap);
        }
        let mut items: Vec<(Acc, f32)> = d.entries;
        if items.len() > self.cfg.cap {
            // 按分数降序保留前 cap 条，同分按账户号升序——全序，与顺序无关。
            items.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            items.truncate(self.cfg.cap);
        }
        SparseView::from_scores(self.cfg.cap, &items)
    }
}

/// 夹到 `[0, 1]`；NaN 收敛到 0。
fn clamp01(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}
