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
//! # 「本机不认识」的账户
//!
//! **这个说法容易误解，先说准确定义。** 「本机不认识」= 「**既没被我评过、
//! 也没被我的朋友背书过**」—— 不是「我没给它打过正分」。
//!
//! 两者的区别很关键：
//!
//! * 我**没评过**但**朋友背书过**的账户 ⟹ **会**拿到声望（来自群体项）。
//!   这是背书传播的正常效果 —— 如果它拿不到，群体项机制就失效了。
//! * 我**既没评过、也没被任何人背书**的账户 ⟹ **真的拿不到任何声望**，
//!   因为没有任何路径能把它引进 `distribution()`：
//!   个人项只遍历 `self.list`（我评过的人），群体项只遍历各视图的槽位（朋友背书过的人）。
//!
//! 关于「预算」那一半：来源的视图里若声明了我完全不认识的账户，
//! 它的定点质量**照样计入分母**（`Σq`），所以花掉了来源的预算。
//! 这是保守的选择：一个来源想用「1000 个我不认识的女巫」来稀释/占用预算，
//! 代价是它对我所认识的人的话语权等比缩小。反过来（只按认识的部分归一化）
//! 会让「声明不认识的账户」变成免费操作，那就又回到预算可被绕过的老路上。
//!
//! ⚠️ 2026-11-18：本节措辞曾写成「它们……**不产生任何声望**」，与实现不符
//! （朋友背书过的账户会拿到声望）。已改成上面这样区分两种情形。
//!
//! # 全局归一化
//!
//! 最后一步一定把 `Σ` 归一到 1。两种退化情况：
//! * 正分清单为空 → 返回空分布（没有任何信息，不编造）；
//! * 群体项没有任何来源可用 → `声望 = β·个人项`，照样归一化。

use std::collections::BTreeMap;

use crate::list::{Acc, PositiveList, Time};
use crate::round::{Rng, RoundBitmap};
use crate::snapshot::NodeSnapshot;
use crate::view::SparseView;

/// 内核参数。
///
/// 「算法」旋钮 4 个（`beta` / `half_life` / `cap` / `decay_enabled`），
/// 「资源上限」旋钮 2 个（`max_views` / `max_list`）。前者改的是计算结果，
/// 后者**不改任何计算结果**，只把内存钉死在有界范围内（`0` = 无上限）。
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
    /// 观点缓存的**条目数**上限；`0` = 无上限，默认 4096。
    ///
    /// 判定规则见 [`Node::on_view`] 的「容量上限」一节，要点：
    /// **只有「新 owner 且已满」才丢；已缓存的 owner 更新既有条目永远接受**
    /// （原地覆盖不增长内存）。
    ///
    /// 默认 4096 的依据：`views` 只能装「我**主动 rate 过**的来源」的观点
    /// （`on_view` 第一条结构性拒绝条件保证），而 A12 图（`n_initial = 400`、
    /// `steps = 120`、`max_follows = 256`）上单个观察者的清单长度实测
    /// 「均值 105.1、中位 101、最大 202」，CI 小图（120 账户 / 70 步 /
    /// `max_follows = 115`）实测 54~61 —— 4096 有 16~20 倍以上余量，
    /// 正常使用**永远碰不到**这个上限。
    ///
    /// 内存上界（条目数即内存上界，因为每份视图的槽位数 `n ≤ cap`）：
    /// `max_views × (6·n_slots + 24 + 16 + BTreeMap 节点开销)`，
    /// 用序列化口径可给出最宽松的绝对上界 `max_views × (cap×6 + 2)`
    /// （`cap = 512` 时 4096 × 3074 B ≈ 12 MiB）。注意这是**上界**：
    /// 视图在内存里只分配 `n_slots × 6` 字节 + `Vec` 头（24 B），
    /// `cap×6+2` 是 `encode()` 的**序列化长度**，不是内存占用。
    pub max_views: usize,
    /// 正分清单的**条目数**上限；`0` = 无上限，默认 4096。
    ///
    /// 语义与 [`PositiveList`] 的 `max_entries` 完全一致（见 list.rs 模块文档）：
    /// 只拦**新增**，删除与改分永远接受。被拒的新增在 [`Node::rate`] 里是静默的，
    /// 用 [`Node::try_rate`] 或 [`Node::stat_rejected_list`] 观测。
    ///
    /// 默认 4096 的依据与 `max_views` 相同（清单比 `views` 还小：
    /// A12 图实测均值 105.1、最大 202）。
    pub max_list: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            beta: 0.3,
            half_life: 72,
            cap: 512,
            decay_enabled: true,
            // 资源上限默认**打开**：4096 对正常使用有 16~20 倍以上余量（见字段文档），
            // 而内存由此从「上层喂多少就涨多少」变成有界。
            // ⚠️ 与 tm_engine::EngineConfig::max_views 的默认值 0（无上限）**不同**是
            // 刻意的：那边默认 0 是为了让既有实验逐位不变，这里 mincore 是原型内核，
            // 防内存耗尽优先；需要旧行为（无上限）时显式写 0，判定分支随即不触发，
            // 逐位退化为引入本字段之前的行为。
            max_views: 4096,
            max_list: 4096,
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
///
/// # `entries` 的调用方契约
///
/// `entries` 是 `pub` 的（方便调用方直接遍历，避免不必要的拷贝），
/// 但 [`Distribution::get`] 用**二分查找**实现，它**要求 `entries` 按账户号严格升序**。
///
/// 由 `Node::distribution()` 产出的分布一定满足这个前提（它用 `BTreeMap` 构造后按序遍历）。
/// 但如果你**手工构造** `Distribution`（测试里会这么做），必须自己保证升序——
/// 否则 `get()` 会查不到明明存在的账户。
///
/// ⚠️ 未来若要收紧，可把 `entries` 改成 `pub(crate)` + 提供 `entries()` 访问器，
/// 但这会破坏 `a12_compare` 里 `.entries.into_iter()` 的用法，属于破坏性变更。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Distribution {
    /// `(账户, 声望)`，**按账户号严格升序**，只含 `> 0` 的条目。
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

    /// 归一化之后的总和（应为 1.0；空分布为 **-0.0**）。
    ///
    /// 注意空分布返回的是 `-0.0`（`std` 的 `f64::sum()` 以 `-0.0` 为初值），
    /// 它在数值上等于 `0.0`，但 `to_bits()` 不同（`0x8000000000000000` vs `0x0`）。
    /// 有测试断言数值而非位模式 —— 如果要逐位比较，请用 `v == 0.0` 而不是位模式。
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
    pub(crate) cfg: Config,
    pub(crate) list: PositiveList,
    pub(crate) sampler: RoundBitmap,
    /// 只为「我打过正分」的来源保留观点：别人硬塞过来的观点我根本不收
    /// （`on_view` 的第一条拒绝条件），所以 `|views|` 只在我**主动** rate 新账户时增长，
    /// 与攻击者的推送速率无关。
    ///
    /// ⚠️ 但它**不是**单调有界于 `|list|`：`rate(acc, 0)` 只删清单条目、不清 views，
    /// 所以反复「rate → on_view → rate(0)」会让 `|views|` 超过 `|list|`（残留记录无份额，
    /// `weights()` 靠 `list.get(owner) == None` 跳过，计算上无害，但占内存）。
    /// 需要回收的话得显式加一个清理入口；当前实现不回收。
    ///
    /// 好在这个上界**现在是被钉死的**：`|views|` 只增不减、而新增又被
    /// [`Config::max_views`] 挡住，所以它恒有 `|views| ≤ max_views`
    /// （`max_views = 0` 时才退化成「受我主动 rate 过的不同账户数约束」）。
    pub(crate) views: BTreeMap<Acc, Received>,
    pub(crate) rng: Rng,
    pub(crate) stat_views: u64,
    pub(crate) stat_rejected: u64,
    /// 因 `max_views` 上限被整条丢弃的观点数（**包含**在 `stat_rejected` 里）。
    /// 单调递增，只增不减。
    pub(crate) stat_rejected_limit: u64,
}

impl Node {
    /// 新建节点。`seed` 只影响抽样顺序，不影响任何计算结果。
    #[must_use]
    pub fn new(cfg: Config, seed: u64) -> Self {
        Self {
            cfg,
            list: PositiveList::with_max(cfg.max_list),
            sampler: RoundBitmap::new(),
            views: BTreeMap::new(),
            rng: Rng::new(seed),
            stat_views: 0,
            stat_rejected: 0,
            stat_rejected_limit: 0,
        }
    }

    // ---------------- 正分清单 ----------------

    /// 记录一次正分（`score <= 0` 表示撤销），并把 `last_check` 刷新为 `now`。
    ///
    /// 超过 [`Config::max_list`] 的**新增**被静默丢弃（撤销与改分永远接受），
    /// 丢弃只累加 [`Node::stat_rejected_list`]，不影响任何计算结果。
    /// 需要知道「这次到底写进去没有」时用 [`Node::try_rate`]。
    pub fn rate(&mut self, acc: Acc, score: f32, now: Time) {
        let _ = self.list.try_set(acc, score, now);
    }

    /// 记录一次正分，返回是否被接受。唯一的拒绝情形是
    /// 「`max_list > 0`、清单已满、且这是一个**新**账户」；
    /// 删除（`score <= 0`）与更新已有条目**永远返回 `true`**。
    ///
    /// 被拒时不写任何字段、不涨清单版本号、不动连败计数。
    pub fn try_rate(&mut self, acc: Acc, score: f32, now: Time) -> bool {
        self.list.try_set(acc, score, now)
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
    /// * 表是空的（`Σq == 0`）—— 没有信息；
    /// * **容量上限**（[`Config::max_views`]，见下）。
    ///
    /// 入库时：`last_check = now`、连败清零。
    ///
    /// # 容量上限
    ///
    /// 上面两条判定之后（顺序固定，保证既有 `stats()` 期望不变）再判
    /// `max_views > 0 && views.len() >= max_views && !views.contains_key(&owner)`：
    ///
    /// * **新 owner 且已满** → 整条**丢弃**，返回 `false`，只累加
    ///   [`Node::stat_rejected_limit`] 与 [`Node::stats`] 的被拒项；
    ///   不写缓存、不刷新 `last_check`、不动连败（调用方本来就分不清成功与失败）；
    /// * **owner 已在缓存里** → 原地覆盖既有条目，**永远接受**：内存不增长，
    ///   丢它没有资源保护收益，反而会阻断正常的观点刷新；
    /// * `max_views = 0` → 这条判定不触发，行为与引入本字段之前**逐位一致**。
    ///
    /// 不变式：`stat_rejected_limit() <= stats().1`，`stats()` 的元组形状与含义不变。
    ///
    /// 为什么是「拒绝新增」而不是 LRU：LRU 的淘汰顺序依赖**历史到达顺序**，
    /// 而 `snapshot → restore` 只恢复「当前状态」、不恢复隐式的淘汰序，
    /// 于是恢复之后的 LRU 行为与关机前不同 —— 那会直接破坏快照能力与逐位确定性。
    /// 拒绝新增的接受判定只依赖 `(|views|, owner 是否已缓存)`，**无隐式状态**；
    /// `max_views = 0` 时逐位退化为旧行为。代价是「满了之后新来源进不来」，
    /// 用 [`Node::stat_rejected_limit`] 让调用方可观测。
    pub fn on_view(&mut self, owner: Acc, view: SparseView, published_at: Time, now: Time) -> bool {
        if !self.list.has(owner) || view.total_q() == 0 {
            self.stat_rejected += 1;
            return false;
        }
        if self.cfg.max_views > 0
            && self.views.len() >= self.cfg.max_views
            && !self.views.contains_key(&owner)
        {
            self.stat_rejected_limit += 1;
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

    /// 因 [`Config::max_views`] 容量上限被整条丢弃的观点累计条数。
    ///
    /// **包含**在 [`Node::stats`] 的被拒项里（不变式
    /// `stat_rejected_limit() <= stats().1`），与 [`Node::stats`] 分开暴露是刻意的：
    /// `stats()` 的二元组形状与含义保持不变（既有实验按位置解构它）。
    ///
    /// 这个数**单调递增、只增不减**（观点被覆盖或撤销正分都不会让它回退），
    /// 它是「被拒了多少次」的累计事件计数，不是「现在有多少条被拒」。
    /// 大于 0 说明缓存正在顶满、资源保护生效。
    #[must_use]
    pub fn stat_rejected_limit(&self) -> u64 {
        self.stat_rejected_limit
    }

    /// 因 [`Config::max_list`] 容量上限被拒的**新增**正分累计次数
    /// （转发 [`PositiveList::stat_rejected_limit`]）。
    ///
    /// 同样单调递增、只增不减；撤销正分与改分不计入。
    #[must_use]
    pub fn stat_rejected_list(&self) -> u64 {
        self.list.stat_rejected_limit()
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
            // 这里**不可能**出现 NaN：`PositiveList::set` 是清单的唯一写入入口，它把 NaN
            // 当成删除（见 list.rs），所以 `list.get()` 给出的 s 恒为 (0, 1] 内的有限值；
            // total 是这些有限值的 f64 和（f64::from(f32) 不会溢出，条目数 ≤ 2^32，
            // 和 ≤ 2^32），所以 share 是有限非负数。
            //
            // 但**仍然保留 NaN 分支**，并且必须用 `is_nan() || <= 0.0` 这种写法：
            // `!(x > 0.0)` 对 NaN 为真、`x <= 0.0` 对 NaN 为假，机械替换会悄悄把
            // 「跳过这一条」变成「让 NaN 流进 wsum / num」。
            // 两式逐值等价的理由：非 NaN 时由全序三分律（x > 0 / x < 0 / x == 0 三选一）
            // `!(x > 0.0)` 恰等于 `x <= 0.0`；NaN 时两侧都为真。
            // 本轮用 rustc -O 实做对拍：f32 穷举全部 2^32 个位模式，f64 覆盖全部 NaN
            // 与边界代表值 838 万个 + 2^32 个随机位模式，均 0 处不一致。
            if share.is_nan() || share <= 0.0 {
                continue;
            }
            let age = now.saturating_sub(rec.last_check);
            let f = if self.cfg.decay_enabled {
                f64::from(decay(age, self.cfg.half_life))
            } else {
                1.0
            };
            let w = share * f;
            // 同 share：share 有限非负、f 是 `decay()` 的返回值（它自己保证不返回 NaN /
            // 0 / 负数，见 decay 的文档），所以 w 也有限非负，这里不可能出现 NaN。
            // 保留 NaN 分支只是纵深防御，写法必须和原来逐值等价。
            if w.is_nan() || w <= 0.0 {
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
        // wsum 只累加 `weights()` 里已经过 NaN 检查、且满足 w > 0 的项，所以它是有限正数
        // 或 0.0，不可能为 NaN。保留 NaN 分支是纵深防御，写法与 `!(wsum > 0.0)` 逐值等价
        // （非 NaN 时由全序三分律成立，NaN 时两侧同为真；对拍方法见 `weights()` 的注释）。
        if wsum.is_nan() || wsum <= 0.0 {
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
        // 与 `group_term` 同源：wsum 只累加通过 NaN 检查的正权重，不可能为 NaN。
        // 分支仍然保留，写法与原式逐值等价。
        if wsum.is_nan() || wsum <= 0.0 {
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
        // mass 的来源：个人项 beta * (s / total) 与群体项 coef * g。s、total、beta、
        // g、coef 全部由前面已经过 NaN 检查的量构造（见 weights / group_term），
        // 所以 mass 有限、非负，不可能为 NaN。分支保留作纵深防御。
        // 与 `!(mass > 0.0)` 逐值等价（推导见 `weights()` 的注释）。若机械替换成
        // `mass <= 0.0`，NaN 时这里会**穿透**并继续做 `v / mass`，把 NaN 写进分布条目；
        // 当前写法保住了「NaN ⟹ 整份分布静默变空」的原语义。
        if mass.is_nan() || mass <= 0.0 {
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

// ---------------- 状态快照 ----------------

impl Node {
    /// 把整个状态打包成一份快照（**不含** `cfg`，但含它的摘要，见 [`NodeSnapshot`]）。
    ///
    /// 快照是纯值：可以对它 `encode()` 落盘、`decode()` 读回，再交给 [Self::restore]。
    #[must_use]
    pub fn snapshot(&self) -> NodeSnapshot {
        NodeSnapshot::capture(self)
    }

    /// 用一份快照重建节点；`cfg` 必须与产生快照时**逐位一致**，否则返回 `None`。
    ///
    /// 恢复后节点的下一次 `tick()` / `probe()` 会抽到与「关机前」完全相同的人，
    /// 因为 `rng` 状态、本轮位图（`order` / `drawn` / `remaining` / `version` / `ready`）
    /// 全部原样装回，失败计数与收发统计也一并恢复。
    #[must_use]
    pub fn restore(snap: NodeSnapshot, cfg: Config) -> Option<Node> {
        snap.into_node(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐条把分布写成 (账户, f32 位模式)，用于逐位比较。
    fn bits(d: &Distribution) -> Vec<(Acc, u32)> {
        d.entries.iter().map(|&(a, v)| (a, v.to_bits())).collect()
    }

    // ══════════════════════════ Distribution 容器 ══════════════════════════

    /// 守：Distribution::default() 是空分布——没有条目、质量 0、总和 0、查谁都查不到。
    ///
    /// 同时钉住 mass / sum 的**位模式**：空分布的 sum() 是 -0.0（Rust 的 f64 Sum
    /// 从 -0.0 起累加，只有空求和才露出这个符号位），而 mass 是 Default 给的 +0.0。
    /// 两者数值上都等于 0，但写死位模式能防住「有人把空分布改成返回 NaN 或非零」。
    #[test]
    fn t01_distribution_default为空() {
        let d = Distribution::default();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert_eq!(d.entries, Vec::<(Acc, f32)>::new());
        assert_eq!(d.mass, 0.0);
        assert_eq!(d.sum(), 0.0);
        assert_eq!(d.get(1), 0.0);
        assert_eq!(d.get(0), 0.0);
        assert_eq!(d.get(u32::MAX), 0.0);
        // 空分布的 sum() 是 -0.0：Rust 当前的 f64 Sum 实现从 -0.0 起累加，
        // 只有「空求和」才会露出这个符号位（-0.0 == 0.0，所以上面的相等断言也成立）。
        assert_eq!(
            d.sum().to_bits(),
            (-0.0f64).to_bits(),
            "空分布 sum 是 -0.0（不是 +0.0）"
        );
        assert_eq!(
            d.mass.to_bits(),
            0.0f64.to_bits(),
            "mass 是 Default 的字面量 0.0"
        );
    }

    /// 守：Distribution::get 按账户号二分查找，分布按账户号升序、不含 0 值条目。
    #[test]
    fn t02_distribution_get_升序且无零值() {
        let d = Distribution {
            entries: vec![(1, 0.5), (7, 0.25), (9, 0.25)],
            mass: 1.0,
        };
        assert_eq!(d.len(), 3);
        assert_eq!(d.get(1), 0.5);
        assert_eq!(d.get(7), 0.25);
        assert_eq!(d.get(9), 0.25);
        assert_eq!(d.get(2), 0.0, "不在分布里返回 0");
        assert_eq!(d.get(0), 0.0);
        assert_eq!(d.get(10), 0.0);
        assert_eq!(d.sum(), 1.0);
        // ⚠️ 可疑，建议作者确认：get 用的是 binary_search_by_key，**依赖条目升序**。
        // 手工塞一份乱序的 entries（[(9,·),(1,·)]）时，二分对 9 号会走到 Err 分支
        // 返回 0.0（而 1 号恰好命中）。Distribution 的 entries 由 distribution() 保证升序，
        // 所以生产路径没问题；但 struct 的字段是 pub，外部可以构造出乱序分布而不被拒绝。
        // 这里照实钉住「乱序时 get 会漏」。
        let unsorted = Distribution {
            entries: vec![(9, 0.5), (1, 0.5)],
            mass: 1.0,
        };
        assert_eq!(unsorted.get(1), 0.5, "1 号恰好被二分命中");
        assert_eq!(unsorted.get(9), 0.0, "乱序分布的 9 号会查不到（依赖升序）");
    }

    // ══════════════════════════ decay ══════════════════════════

    /// 守：decay 的边界——dt = 0 精确 1.0；整半衰期精确减半；half_life = 0 当 1 处理；
    /// 永不为 0 / NaN / 负数（下溢返回最小正 f32）；且单调不增。
    #[test]
    fn t03_decay_边界与单调性() {
        assert_eq!(decay(0, 72), 1.0, "dt = 0 精确返回 1.0");
        assert_eq!(decay(72, 72), 0.5, "一个半衰期恰好减半");
        assert_eq!(decay(144, 72), 0.25);
        assert_eq!(
            decay(36, 72),
            std::f32::consts::FRAC_1_SQRT_2,
            "半个半衰期 = 1/sqrt(2)"
        );
        // half_life = 0 按 1 处理。
        assert_eq!(decay(0, 0), 1.0);
        assert_eq!(decay(1, 0), 0.5);
        assert_eq!(decay(2, 0), 0.25);
        // 极端：下溢时返回最小正 f32，绝不为 0。
        let tiny = decay(u64::MAX, 72);
        assert_eq!(tiny, f32::MIN_POSITIVE);
        assert_eq!(decay(u64::MAX, 1), f32::MIN_POSITIVE);
        assert!(tiny > 0.0 && tiny.is_finite());
        // 单调不增，且全部是正有限值。
        let mut prev = 2.0f32;
        for dt in 0..=1000u64 {
            let d = decay(dt, 72);
            assert!(d > 0.0 && d.is_finite(), "dt = {dt} 的衰减因子非法: {d}");
            assert!(d <= prev, "dt = {dt} 处衰减不再单调");
            prev = d;
        }
    }

    // ══════════════════════════ 退化：空清单 / 无观点 ══════════════════════════

    /// 守：正分清单为空 ⟹ 空分布（不编造）、query 恒 0、publish 出空表、tick/probe 空。
    #[test]
    fn t04_空清单退化不编造() {
        let n = Node::new(Config::default(), 1);
        let d = n.distribution(0);
        assert!(d.is_empty(), "空清单不该编造分布");
        assert_eq!(d.mass, 0.0);
        assert_eq!(d.sum(), 0.0);
        for acc in [0u32, 1, 99, u32::MAX] {
            assert_eq!(n.query(acc, 0), 0.0, "空清单下 query({acc}) 必须精确为 0");
        }
        let v = n.publish(0);
        assert!(v.is_empty());
        assert_eq!(v.cap(), Config::default().cap, "空发布仍是 cap 定长表");
        assert_eq!(v.bytes(), 512 * 6 + 2);
        assert_eq!(n.view_count(), 0);
        assert_eq!(n.stats(), (0, 0));

        let mut m = Node::new(Config::default(), 1);
        assert!(m.tick(3).is_empty(), "空清单上 tick 返回空");
        assert_eq!(m.round_remaining(), 0);
        assert!(m.probe().is_empty());
    }

    /// 守：有正分、但没有任何来源观点 ⟹ 声望 = β·个人项，再全局归一化后**恰好是个人份额**。
    ///
    /// 归一化把 β 整个约掉了（β > 0 时），所以分布与 β 无关；这条同时钉住「不编造群体项」。
    #[test]
    fn t05_无观点时退化为个人项归一化() {
        let mut n = Node::new(Config::default(), 1);
        n.rate(5, 0.2, 0);
        n.rate(6, 0.8, 0);
        let d = n.distribution(0);
        assert_eq!(d.len(), 2);
        assert_eq!(bits(&d), vec![(5, 0.2f32.to_bits()), (6, 0.8f32.to_bits())]);
        assert_eq!(d.get(5), 0.2);
        assert_eq!(d.get(6), 0.8);
        assert_eq!(d.mass, f64::from(0.3f32), "mass = 归一化前总量 = β");
        assert!((d.sum() - 1.0).abs() < 1e-6);
        // query 不做全局归一化，所以留下 β。
        assert_eq!(n.query(5, 0), 0.2 * 0.3, "个人项 × β");
        assert_eq!(n.query(6, 0), 0.8 * 0.3);
        assert_eq!(n.query(7, 0), 0.0);
    }

    /// 守：只有一个来源、一条观点（来源自身也是我的清单成员）→ 声望 = β·个人项 与
    /// (1−β)·群体项 的合成；单来源时群体项退化为观点自身的归一化值。
    #[test]
    fn t06_单来源与观点合成_beta配比() {
        // 我的清单：10 号 1.0（个人项 = 1）；10 号的观点：20 号 1.0。
        let mut n = Node::new(Config::default(), 1);
        n.rate(10, 1.0, 0);
        assert!(n.on_view(10, SparseView::from_scores(64, &[(20, 1.0)]), 0, 0));
        let d = n.distribution(0);
        // 声望：10 = 0.3·1 + 0.7·0 = 0.3；20 = 0.3·0 + 0.7·1 = 0.7。归一化后不变。
        assert_eq!(
            bits(&d),
            vec![(10, 0.3f32.to_bits()), (20, 0.7f32.to_bits())]
        );
        assert!((d.get(10) - 0.3).abs() < 1e-6, "10 号 = {}", d.get(10));
        assert!((d.get(20) - 0.7).abs() < 1e-6, "20 号 = {}", d.get(20));
        assert_eq!(d.sum(), 1.0);
        assert_eq!(d.mass, 1.0);
        assert_eq!(n.query(10, 0), 0.3);
        assert_eq!(n.query(20, 0), 0.7);
    }

    /// 守：一条观点里多条等权时，群体系数按 q/Σq 分摊；β 配比逐位正确。
    #[test]
    fn t07_单来源两条等权观点() {
        let mut n = Node::new(Config::default(), 1);
        n.rate(1, 1.0, 0);
        assert!(n.on_view(1, SparseView::from_scores(64, &[(7, 0.5), (8, 0.5)]), 0, 0));
        let d = n.distribution(0);
        // 1 号个人项 1；7/8 号群体项各 0.5。
        // 声望：1 = 0.3；7 = 8 = 0.7·0.5 = 0.35。Σ = 1，无需再缩放。
        assert_eq!(
            bits(&d),
            vec![
                (1, 0.3f32.to_bits()),
                (7, 0.35f32.to_bits()),
                (8, 0.35f32.to_bits())
            ]
        );
        assert_eq!(n.query(7, 0), 0.35);
        assert_eq!(n.query(8, 0), 0.35);
        assert_eq!(n.query(1, 0), 0.3);
        assert_eq!(d.sum(), 1.0);
    }

    // ══════════════════════════ 全局归一化 ══════════════════════════

    /// 守：全局归一化——非空分布 Σ 恒为 1（容差 1e-6），条目严格升序、无 0 值。
    #[test]
    fn t08_全局归一化_总和恒为1() {
        let mut n = Node::new(Config::default(), 7);
        for i in 0..20u32 {
            n.rate(i, 0.1 + (i as f32) * 0.04, 0);
        }
        for i in 0..20u32 {
            let slots: Vec<(u32, f32)> = (0..20u32)
                .map(|j| (100 + j, ((i + j) % 7 + 1) as f32))
                .collect();
            assert!(n.on_view(i, SparseView::from_scores(64, &slots), 0, 0));
        }
        let d = n.distribution(0);
        assert_eq!(d.len(), 40, "20 个个人项 + 20 个群体项");
        assert!((d.sum() - 1.0).abs() < 1e-6, "Σ = {}", d.sum());
        assert!(
            d.entries.windows(2).all(|w| w[0].0 < w[1].0),
            "条目必须严格升序"
        );
        for &(_, v) in &d.entries {
            assert!(v > 0.0 && v.is_finite(), "分布里不许有 0 / NaN / 负数条目");
        }
        assert!((d.mass - 1.0).abs() < 1e-9, "mass = {}", d.mass);
    }

    /// 守：mass 是**归一化前**的总量；distribution 与 query 是两条路径
    /// （全量遍历 vs 单账户二分），归一化后必须给出同一个值：get(a) == (query(a)/mass) as f32。
    #[test]
    fn t09_query与distribution两条路径一致() {
        let mut n = Node::new(Config::default(), 7);
        for i in 0..20u32 {
            n.rate(i, 0.1 + (i as f32) * 0.04, 0);
        }
        for i in 0..20u32 {
            let slots: Vec<(u32, f32)> = (0..20u32)
                .map(|j| (100 + j, ((i + j) % 7 + 1) as f32))
                .collect();
            assert!(n.on_view(i, SparseView::from_scores(64, &slots), 0, 0));
        }
        let d = n.distribution(0);
        for &(a, v) in &d.entries {
            let expect = (f64::from(n.query(a, 0)) / d.mass) as f32;
            assert_eq!(v.to_bits(), expect.to_bits(), "账户 {a} 两条路径不一致");
        }
        // Σ query（不做全局归一化）应 ≈ mass。
        let sum_q: f64 = (0..20u32)
            .chain(100..120)
            .map(|a| f64::from(n.query(a, 0)))
            .sum();
        assert!(
            (sum_q - d.mass).abs() < 1e-6,
            "Σ query = {sum_q} vs mass = {}",
            d.mass
        );
        // 个人项的 query 之和恰好是 β（Σ 个人份额 = 1，且群体项不落在被评过的账户上）。
        let sum_personal: f64 = (0..20u32).map(|a| f64::from(n.query(a, 0))).sum();
        assert!(
            (sum_personal - f64::from(0.3f32)).abs() < 1e-6,
            "Σ query(个人) = {sum_personal}，应为 β"
        );
    }

    // ══════════════════════════ β 参数 ══════════════════════════

    /// 守：β 的两个极端——β = 0 只剩群体项、β = 1 只剩个人项；中间值线性配比。
    #[test]
    fn t10_beta极端值() {
        let build = |beta: f32| {
            let mut n = Node::new(
                Config {
                    beta,
                    ..Config::default()
                },
                1,
            );
            n.rate(1, 1.0, 0);
            n.rate(2, 0.5, 0);
            assert!(n.on_view(1, SparseView::from_scores(64, &[(30, 1.0)]), 0, 0));
            n
        };

        // β = 0：个人项被完全踢掉，30 号独占全部声望。
        let n0 = build(0.0);
        let d0 = n0.distribution(0);
        assert_eq!(bits(&d0), vec![(30, 1.0f32.to_bits())]);
        assert_eq!(n0.query(30, 0), 1.0);
        assert_eq!(n0.query(1, 0), 0.0);

        // β = 1：群体项被完全踢掉，1/2 号按个人份额 2/3 : 1/3。
        let n1 = build(1.0);
        let d1 = n1.distribution(0);
        assert_eq!(
            bits(&d1),
            vec![
                (1, 0.666_666_7f32.to_bits()),
                (2, 0.333_333_34f32.to_bits())
            ]
        );
        assert_eq!(d1.get(30), 0.0, "β = 1 时群体项完全退出");
        assert_eq!(n1.query(30, 0), 0.0);

        // β = 0.5：3 个条目按 (0.5·2/3, 0.5·1/3, 0.5·1) 归一化 → (1/3, 1/6, 1/2)。
        let nh = build(0.5);
        let dh = nh.distribution(0);
        assert_eq!(dh.get(1), 0.333_333_34f32);
        assert_eq!(dh.get(2), 0.166_666_67f32);
        assert_eq!(dh.get(30), 0.5f32);
        assert!((dh.sum() - 1.0).abs() < 1e-6);
    }

    /// 守：β 被 clamp01 夹到 [0,1]；NaN 收敛到 0（等价于 β = 0）。
    #[test]
    fn t11_beta越界被夹紧() {
        let build = |beta: f32| {
            let mut n = Node::new(
                Config {
                    beta,
                    ..Config::default()
                },
                1,
            );
            n.rate(1, 1.0, 0);
            n.rate(2, 0.5, 0);
            assert!(n.on_view(1, SparseView::from_scores(64, &[(30, 1.0)]), 0, 0));
            n
        };
        // β = 2 与 β = 1 等价（夹到 1）。
        let two = build(2.0).distribution(0);
        let one = build(1.0).distribution(0);
        assert_eq!(bits(&two), bits(&one), "β > 1 应等价于 β = 1");
        // β = -1 与 β = 0 等价（夹到 0）。
        let neg = build(-1.0).distribution(0);
        let zero = build(0.0).distribution(0);
        assert_eq!(bits(&neg), bits(&zero), "β < 0 应等价于 β = 0");
        // β = NaN 收敛到 0。
        let nan = build(f32::NAN).distribution(0);
        assert_eq!(bits(&nan), bits(&zero), "β = NaN 收敛到 β = 0");
        assert_eq!(build(2.0).query(30, 0), build(1.0).query(30, 0));
        assert_eq!(build(-1.0).query(30, 0), build(0.0).query(30, 0));
        assert_eq!(build(f32::NAN).query(30, 0), build(0.0).query(30, 0));
    }

    // ══════════════════════════ 衰减 ══════════════════════════

    /// 守：衰减只改**相对**权重——只有一个来源时，它变旧不改变任何条目；两个来源时，
    /// 新鲜的来源拿到更大权重（按 0.5^(dt/72) 精确配比）。
    #[test]
    fn t12_衰减只改相对权重() {
        // 只有一个来源可取时，衰减是全体同乘——分布逐位不变。
        let mut solo = Node::new(Config::default(), 1);
        solo.rate(1, 1.0, 0);
        assert!(solo.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 0));
        let early = solo.distribution(0);
        let late = solo.distribution(10_000);
        assert_eq!(bits(&early), bits(&late), "单来源时衰减是相对量，分布不变");

        // 两个来源、一个落后一个半衰期：权重比 = 1 : 0.5。
        let mut n = Node::new(Config::default(), 1);
        n.rate(1, 1.0, 0);
        n.rate(2, 1.0, 0);
        assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 0));
        assert!(n.on_view(2, SparseView::from_scores(64, &[(8, 1.0)]), 0, 0));
        // 只刷新 1 号（2 号停在 0 → 落后 72）。
        assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 72));
        let d = n.distribution(72);
        assert_eq!(d.get(1), 0.15);
        assert_eq!(d.get(2), 0.15);
        assert_eq!(n.query(8, 72), (0.7f64 / 3.0) as f32);
        assert_eq!(n.query(9, 72), (0.7f64 * 2.0 / 3.0) as f32);
        assert_eq!(d.get(8), n.query(8, 72));
        assert_eq!(d.get(9), n.query(9, 72));
        assert!(d.get(9) > d.get(8), "新鲜来源的权重应更大");
    }

    /// 守：decay_enabled = false 时因子恒为 1，两个来源无论多旧都等权。
    #[test]
    fn t13_关闭衰减时权重相等() {
        let mut n = Node::new(
            Config {
                decay_enabled: false,
                ..Config::default()
            },
            1,
        );
        n.rate(1, 1.0, 0);
        n.rate(2, 1.0, 0);
        assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 0));
        assert!(n.on_view(2, SparseView::from_scores(64, &[(8, 1.0)]), 0, 0));
        // 只有 1 号被刷新，2 号落后 72；若衰减开着，9 会 > 8。
        assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 72));
        let d = n.distribution(72);
        assert_eq!(d.get(8), d.get(9), "衰减关闭时新旧来源等权");
        assert!((d.get(8) - 0.35).abs() < 1e-6, "各 0.7/2 = 0.35");
        assert_eq!(n.query(8, 72), n.query(9, 72));
    }

    /// 守：半衰期是旋钮的直接后果——越小，陈旧来源衰减越快、份额越向新鲜来源集中。
    #[test]
    fn t14_半衰期参数生效() {
        let value_at = |hl: Time| {
            let mut n = Node::new(
                Config {
                    half_life: hl,
                    ..Config::default()
                },
                1,
            );
            n.rate(1, 1.0, 0);
            n.rate(2, 1.0, 0);
            assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 0));
            assert!(n.on_view(2, SparseView::from_scores(64, &[(8, 1.0)]), 0, 0));
            // 1 号在 72 时刻被刷新，2 号停在 0。
            assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 72));
            n.query(9, 72)
        };
        // 半衰期 72：权重比 1 : 0.5 → 9 号群体项 = 2/3。
        assert!((value_at(72) - (0.7 * 2.0 / 3.0) as f32).abs() < 1e-6);
        // 半衰期 144：权重比 1 : 0.5^(72/144) = 2^-0.5 ≈ 0.7071 → 9 号占比 ≈ 0.5858。
        let ratio = 0.5f64.powf(72.0 / 144.0);
        let expect = (0.7 * 1.0 / (1.0 + ratio)) as f32;
        assert!((value_at(144) - expect).abs() < 1e-6);
        // 半衰期 1：2 号几乎归零 → 9 号几乎独占群体项。
        assert!((value_at(1) - 0.7).abs() < 1e-5);
        // 半衰期 0 按 1 处理，与 half_life = 1 一致。
        assert_eq!(value_at(0), value_at(1));
    }

    // ══════════════════════════ on_view 的接受 / 拒绝 ══════════════════════════

    /// 守：on_view 的两条拒绝条件——owner 不在正分清单 / 表总质量为 0；
    /// 拒绝时不动统计以外的一切（不写 views、不改连败、不推进 last_check）。
    #[test]
    fn t15_on_view_拒绝条件() {
        let mut n = Node::new(Config::default(), 1);
        n.rate(5, 1.0, 0);
        // ① owner 不在清单。
        assert!(!n.on_view(6, SparseView::from_scores(8, &[(7, 1.0)]), 0, 0));
        // ② 表是空的（总质量 0）。
        assert!(!n.on_view(5, SparseView::new(8), 0, 0));
        // ③ 全是 0/NaN 的表也等价于空表。
        let zero = SparseView::from_scores(8, &[(2, 0.0), (3, f32::NAN)]);
        assert_eq!(zero.total_q(), 0);
        assert!(!n.on_view(5, zero, 0, 0));

        assert_eq!(n.view_count(), 0, "被拒的观点不该占内存");
        assert_eq!(n.stats(), (0, 3), "三次拒绝");
        assert_eq!(n.claims().fail_streak(5), 0, "拒绝不是「取失败」");
        assert_eq!(n.claims().last_check(5), Some(0));
        // 只有个人项、没有来源时仍然给出分布：5 号独占（个人份额 1 归一化后仍是 1）。
        let d = n.distribution(0);
        assert_eq!(d.len(), 1);
        assert_eq!(d.get(5), 1.0);
        assert_eq!(
            (0..10u32)
                .filter(|&a| a != 5)
                .map(|a| n.query(a, 0))
                .sum::<f32>(),
            0.0
        );
    }

    /// 守：接受观点时会写入 views、把 last_check 推进到 now、清零连败、统计 +1。
    #[test]
    fn t16_on_view_接受时刷新last_check并清零连败() {
        let mut n = Node::new(Config::default(), 1);
        n.rate(5, 1.0, 0);
        for _ in 0..3 {
            n.on_view_failed(5);
        }
        assert_eq!(n.claims().fail_streak(5), 3);
        assert!(n.on_view(5, SparseView::from_scores(8, &[(7, 1.0)]), 11, 33));
        assert_eq!(n.view_count(), 1);
        assert_eq!(n.claims().fail_streak(5), 0, "取到观点即清零连败");
        assert_eq!(n.claims().last_check(5), Some(33), "last_check = now");
        assert_eq!(n.stats(), (1, 0));
        // 累计统计：再来一次失败 + 一次成功。
        n.on_view_failed(5);
        assert!(n.on_view(5, SparseView::from_scores(8, &[(8, 1.0)]), 0, 40));
        assert_eq!(n.stats(), (2, 0), "失败不计入 stats");
        assert_eq!(n.view_count(), 1, "同一 owner 只有一份观点");
    }

    /// 守：published_at 只做展示、绝不参与衰减——把它填成任意值都不改变结果。
    #[test]
    fn t17_published_at不参与计算() {
        let build = |published_at: Time| {
            let mut n = Node::new(Config::default(), 1);
            n.rate(1, 1.0, 0);
            assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), published_at, 0));
            n
        };
        let a = build(0).distribution(1000);
        let b = build(u64::MAX).distribution(1000);
        assert_eq!(bits(&a), bits(&b), "published_at 不该影响衰减");
        // 真正的衰减基准是 last_check（= on_view 传入的 now）。
        let early = build(0).distribution(0);
        let late = build(0).distribution(10_000);
        assert_eq!(bits(&early), bits(&late), "单来源时衰减仍是相对量");
    }

    // ══════════════════════════ 未登记账户 ══════════════════════════

    /// 守：**经朋友背书**的陌生账户会拿到声望 —— 这是设计如此，不是漏洞。
    ///
    /// 场景：999 号我从没评过，但它出现在我朋友（1 号）发布的视图里。
    /// 群体项的定义就是「我朋友的看法」，所以 999 号拿到 (1−β) 群体项里 1 号分给它的那一份，
    /// 这是**背书传播**的正常效果。如果它拿不到声望，整个群体项机制就失效了。
    ///
    /// 需要精确化的地方是「不认识」的定义（见 `Node` 的模块文档）：
    /// 它不是「我没给过正分」，而是「**既没被我评过、也没被我的朋友背书过**」。
    /// 后者才真正拿不到任何声望 —— 因为没有任何路径能把它引进 `distribution()`：
    /// 个人项只遍历 `self.list`（我评过的人），群体项只遍历各视图的槽位（朋友背书过的人）。
    ///
    /// 所以本测试钉住两件事：
    /// 1. 背书机制有效（999 号拿到 0.35）；
    /// 2. 「花预算」同样成立（已知账户 100 号从 0.8 被稀释到 0.45）。
    #[test]
    fn t18_朋友背书过的陌生账户会拿到声望() {
        let build = |extra: Option<u32>| {
            let mut n = Node::new(Config::default(), 1);
            n.rate(1, 1.0, 0); // 我的来源
            n.rate(100, 0.5, 0); // 我评过的已知账户
            let slots: Vec<(u32, f32)> = match extra {
                Some(unknown) => vec![(100, 1.0), (unknown, 1.0)],
                None => vec![(100, 1.0)],
            };
            assert!(n.on_view(1, SparseView::from_scores(64, &slots), 0, 0));
            n
        };

        // 只有已知账户：100 号拿到 (1−β) 的全部群体项 → 0.7；1 号个人项 0.2。
        let known = build(None);
        let dk = known.distribution(0);
        assert_eq!(
            bits(&dk),
            vec![(1, 0.2f32.to_bits()), (100, 0.8f32.to_bits())]
        );

        // 再加一个本机「不认识」的 999：两人各分一半群体项。
        let with_unknown = build(Some(999));
        let d = with_unknown.distribution(0);
        // 手算：我的清单 total = 1.5；个人项 1 号 = 2/3、100 号 = 1/3。
        // 群体项：1 号的权重 = 2/3，其观点里 100/999 各占一半 → 各 0.5。
        // 1 号 = 0.3·(2/3) = 0.2；100 号 = 0.3·(1/3) + 0.7·0.5 = 0.45；999 号 = 0.7·0.5 = 0.35。
        // Σ = 1.0，所以这就是归一化之后的最终值（逐位断言）。
        assert_eq!(d.get(1), 0.2f32);
        assert_eq!(
            bits(&d),
            vec![
                (1, 0.2f32.to_bits()),
                (100, 0.45f32.to_bits()),
                (999, 0.35f32.to_bits()),
            ]
        );
        assert_eq!(
            d.get(999),
            0.35f32,
            "朋友背书过的陌生账户会拿到声望（背书机制正常生效）"
        );
        assert!(d.get(999) > 0.0, "朋友背书过的账户应当能拿到声望");
        assert_eq!(with_unknown.query(100, 0), 0.45f32, "被陌生条目稀释");
        assert_eq!(with_unknown.query(999, 0), 0.35f32);

        // 「花预算」这一半成立：已知账户 100 号的声望从 0.8 掉到 0.45（群体项被分走一半）。
        assert_eq!(d.get(100), 0.45f32, "陌生条目确实吃掉了预算");
        assert!(d.get(100) < dk.get(100), "加陌生条目后已知账户被稀释");

        // 视图级口径（与 tests/anti_sybil.rs 的 t02 同源）：两个槽位各拿 0.5。
        let v = SparseView::from_scores(64, &[(100, 1.0), (999, 1.0)]);
        assert_eq!(v.normalized(100), 0.5);
        assert_eq!(v.normalized(999), 0.5);
    }

    // ══════════════════════════ 视图替换 / 撤销 ══════════════════════════

    /// 守：同一 owner 再次 on_view 时**整体替换**旧观点（不是合并），旧槽位立刻消失。
    #[test]
    fn t19_on_view_替换旧观点() {
        let mut n = Node::new(Config::default(), 1);
        n.rate(1, 1.0, 0);
        assert!(n.on_view(1, SparseView::from_scores(64, &[(40, 1.0)]), 0, 0));
        assert_eq!(n.query(40, 0), 0.7);
        assert!(n.on_view(1, SparseView::from_scores(64, &[(41, 1.0)]), 0, 0));
        assert_eq!(n.view_count(), 1, "同一 owner 只留一份");
        assert_eq!(n.query(40, 0), 0.0, "旧观点被整体替换");
        assert_eq!(n.query(41, 0), 0.7);
    }

    /// 守：撤销正分后 views 里的观点**不会**被清理，但因为没有份额，它不再产生任何声望。
    ///
    /// 为什么这样是安全的：`rate(acc, 0)` 只删清单条目、不清 views，残留观点的
    /// `weights()` 会靠 `list.get(owner) == None` 跳过，所以它**不产生任何声望**。
    ///
    /// 代价是内存：反复 `rate → on_view → rate(0)` 会让 `|views|` 超过 `|list|`。
    /// 上界仍受「我主动 rate 过的不同账户数」约束（`on_view` 拒收没打过分的来源，
    /// 所以攻击者无法推高它），不是无界的，但确实不回收。
    /// 本测试钉住这个行为的**两面**：残留存在（不回收）+ 残留无声望（安全）。
    #[test]
    fn t20_撤销正分后观点残留但无声望() {
        let mut n = Node::new(Config::default(), 1);
        for i in 0..3u32 {
            n.rate(i, 1.0, 0);
            assert!(n.on_view(i, SparseView::from_scores(8, &[(50, 1.0)]), 0, 0));
        }
        assert_eq!(n.view_count(), 3);
        assert_eq!(n.claims().len(), 3);
        let before = n.distribution(0);
        assert_eq!(before.get(50), 0.7);

        // 撤销 1 号、2 号的正分（清单里只剩 0 号）。
        n.rate(1, 0.0, 0);
        n.rate(2, 0.0, 0);
        assert_eq!(n.view_count(), 3, "views 里的残留观点不会被回收");
        assert_eq!(n.claims().len(), 1);
        let after = n.distribution(0);
        // 只剩 0 号的个人项 0.3 与群体项 0.7。
        assert_eq!(
            bits(&after),
            vec![(0, 0.3f32.to_bits()), (50, 0.7f32.to_bits())]
        );
        assert_eq!(n.query(50, 0), 0.7);

        // 把最后一条正分也撤销（清单里此时只剩 0 号）：分布变空，残留观点彻底失去意义。
        n.rate(0, 0.0, 0);
        assert!(n.distribution(0).is_empty(), "清单空 ⟹ 空分布");
        assert_eq!(n.view_count(), 3, "残留的 views 仍占着内存");
        assert_eq!(n.query(50, 0), 0.0);
    }

    // ══════════════════════════ Config 的旋钮 ══════════════════════════

    /// 守：Config::default() 的六个值——4 个算法旋钮 + 2 个资源上限旋钮。
    ///
    /// 2026-11-19 加了 `max_views` / `max_list`（防内存耗尽）。补两个字段时
    /// **只给的字面量补字段、不改任何一条既有断言**：四个算法旋钮的默认值
    /// 仍然是「改任何一个都是协议变更」的那一组。
    #[test]
    fn t21_config_default六个旋钮() {
        let c = Config::default();
        assert_eq!(c.beta, 0.3);
        assert_eq!(c.half_life, 72);
        assert_eq!(c.cap, 512);
        assert!(c.decay_enabled);
        // 资源上限默认 4096（不是 0）：防内存耗尽优先，见 Config::max_views 的文档。
        assert_eq!(c.max_views, 4096);
        assert_eq!(c.max_list, 4096);
        // 结构体是可比较的，默认值必须能与之逐字段相等。
        assert_eq!(
            c,
            Config {
                beta: 0.3,
                half_life: 72,
                cap: 512,
                decay_enabled: true,
                max_views: 4096,
                max_list: 4096
            }
        );
    }

    /// 守：cap 旋钮在 publish 上生效——截断到 cap 条、按声望降序保留高分、
    /// 同分时按账户号升序（全序，因此与输入顺序无关）；发布结果 Σv ≡ 1。
    ///
    /// 注意：cap 只影响**发布**（SparseView 容量），不影响 distribution 的内容——
    /// distribution 是「我认识的全部账户」，发布才按容量截断。
    #[test]
    fn t22_cap只影响发布截断() {
        let mut n = Node::new(
            Config {
                cap: 3,
                ..Config::default()
            },
            1,
        );
        for i in [1u32, 2, 3, 4, 5] {
            n.rate(i, i as f32 / 10.0, 0);
        }
        // distribution 仍是 5 条（cap 不参与计算）。
        let d = n.distribution(0);
        assert_eq!(d.len(), 5);
        assert!(d.get(1) < d.get(5));

        // publish 按 cap = 3 截断，保留声望最高的 3/4/5 号。
        let v = n.publish(0);
        assert_eq!(v.cap(), 3);
        assert_eq!(v.len(), 3);
        assert_eq!(v.bytes(), 3 * 6 + 2, "定长表 cap×6+2");
        assert_eq!(
            v.slots(),
            &[(3, 13107), (4, 17476), (5, 21845)],
            "保留高分三条的定点分数"
        );
        assert_eq!(v.total_q(), 52428);
        let sum: f64 = v
            .normalized_slots()
            .iter()
            .map(|&(_, x)| f64::from(x))
            .sum();
        assert!((sum - 1.0).abs() < 1e-6, "发布表 Σv = {sum}");

        // 同分时按账户号升序保留靠前的（1,2,3 全等分，cap = 2 → 只留 1、2）。
        let mut e = Node::new(
            Config {
                cap: 2,
                ..Config::default()
            },
            1,
        );
        for a in [1u32, 2, 3] {
            e.rate(a, 0.5, 0);
        }
        let ve = e.publish(0);
        assert_eq!(ve.len(), 2);
        assert_eq!(ve.slots(), &[(1, 21845), (2, 21845)]);
        assert_eq!(ve.total_q(), 43690);

        // cap = 1 时只发一条。
        let mut one = Node::new(
            Config {
                cap: 1,
                ..Config::default()
            },
            1,
        );
        for a in [1u32, 2, 3] {
            one.rate(a, 0.5, 0);
        }
        let v1 = one.publish(0);
        assert_eq!(v1.len(), 1);
        assert_eq!(v1.slots(), &[(1, 21845)]);
    }

    /// 守：publish 出来的表喂给另一个节点时预算恒为 1（发布/接收同一套归一化）。
    ///
    /// 这条把「发布侧」与「接收侧」串起来：A 的发布表被 B 接收后，
    /// B 看到的那份观点 Σv ≡ 1，不存在「一条撑爆预算」的路径。
    #[test]
    fn t23_publish到接收的预算恒为1() {
        let mut a = Node::new(Config::default(), 1);
        for i in 0..10u32 {
            a.rate(i, 0.5, 0);
        }
        let v = a.publish(0);
        assert!(!v.is_empty());
        assert_eq!(v.len(), 10);
        let sum: f64 = v
            .normalized_slots()
            .iter()
            .map(|&(_, x)| f64::from(x))
            .sum();
        assert!((sum - 1.0).abs() < 1e-6, "发布表的 Σv = {sum}");
        for &(_, x) in v.normalized_slots().iter() {
            assert!(x > 0.0 && x <= 1.0, "单条 v 越界: {x}");
        }

        // B 接收这张表：它对我评过的每个来源都给同一份观点。
        let mut b = Node::new(Config::default(), 2);
        for i in 0..10u32 {
            b.rate(i, 0.5, 0);
            assert!(b.on_view(i, v.clone(), 0, 0));
        }
        // 群体项 = 这张表的归一化结果（每人 0.1）；个人项每人 0.1。
        // 声望 = 0.3·0.1 + 0.7·0.1 = 0.1，Σ = 1。
        let d = b.distribution(0);
        assert_eq!(d.len(), 10);
        assert!((d.sum() - 1.0).abs() < 1e-6);
        for i in 0..10u32 {
            assert!((d.get(i) - 0.1).abs() < 1e-6, "{i} 号 = {}", d.get(i));
        }
    }

    // ══════════════════════════ 资源上限：views / list ══════════════════════════

    /// 只改 max_views，其余默认。
    fn cfg_views(max_views: usize) -> Config {
        Config {
            max_views,
            ..Config::default()
        }
    }

    /// 只改 max_list，其余默认。
    fn cfg_list(max_list: usize) -> Config {
        Config {
            max_list,
            ..Config::default()
        }
    }

    /// 两个上限一起改，其余默认。
    fn cfg_limits(max_views: usize, max_list: usize) -> Config {
        Config {
            max_views,
            max_list,
            ..Config::default()
        }
    }

    /// 守：Config 的默认上限是 **4096 / 4096**（不是「无上限」）；
    /// 写 0 才是无上限；`Node` 的清单按 `cfg.max_list` 初始化。
    #[test]
    fn t24_config默认上限是4096() {
        let c = Config::default();
        assert_eq!(c.max_views, 4096);
        assert_eq!(c.max_list, 4096);
        assert_ne!(c.max_views, 0, "默认必须是**有限**上限（防内存耗尽优先）");
        assert_ne!(c.max_list, 0);
        // 清单上限跟着 Config 走。
        let n = Node::new(
            Config {
                max_list: 3,
                ..Config::default()
            },
            1,
        );
        assert_eq!(n.claims().max_entries(), 3, "清单上限来自 Config::max_list");
        assert_eq!(n.claims().len(), 0);
        // 默认 Node 的清单上限也是 4096。
        assert_eq!(Node::new(Config::default(), 1).claims().max_entries(), 4096);
    }

    /// 守：`max_views = 0` = 无上限——喂 2000 份不同 owner 的观点全部接收，
    /// 一次容量拒绝都没有（这是「逐位退化为旧行为」的契约）。
    #[test]
    fn t25_max_views为0表示无上限() {
        let mut n = Node::new(cfg_views(0), 1);
        let view = SparseView::from_scores(8, &[(7, 1.0)]);
        for i in 0..2000u32 {
            assert!(n.try_rate(i, 0.5, 0), "默认 max_list 够大，2000 条都能进");
            assert!(
                n.on_view(i, view.clone(), 0, 0),
                "无上限时第 {i} 份必须接收"
            );
        }
        assert_eq!(n.view_count(), 2000);
        assert_eq!(n.stat_rejected_limit(), 0, "无上限不该产生容量拒绝");
        assert_eq!(n.stats(), (2000, 0), "stats 也不该有拒绝");
    }

    /// 守：`max_views = n` 的**恰好满 / 多一条**边界：第 n 份接收，第 n+1 份新
    /// owner 整条丢弃（不占内存、不计成功、只涨容量计数与 stats 的被拒项）。
    #[test]
    fn t26_max_views恰好满与多一条的边界() {
        let mut n = Node::new(cfg_views(3), 1);
        let view = SparseView::from_scores(8, &[(7, 1.0)]);
        for i in 0..3u32 {
            assert!(n.try_rate(i, 0.5, 0));
            assert!(
                n.on_view(i, view.clone(), 0, 0),
                "第 {i} 份让缓存**恰好满**"
            );
        }
        assert_eq!(n.view_count(), 3, "恰好满 = view_count == max_views");
        assert_eq!(n.stats(), (3, 0));
        assert_eq!(n.stat_rejected_limit(), 0);
        // 第 4 份：新 owner + 已满 → 整条丢弃。
        assert!(n.try_rate(3, 0.5, 0));
        assert!(
            !n.on_view(3, view.clone(), 0, 5),
            "满了之后的新 owner 必须被拒"
        );
        assert_eq!(n.view_count(), 3, "被拒的观点不占内存");
        assert_eq!(n.stats(), (3, 1), "容量拒绝计入 stats 的被拒项，成功数不变");
        assert_eq!(n.stat_rejected_limit(), 1);
        // 被拒时**不**刷新 last_check、不记成功、不动连败。
        assert_eq!(n.claims().last_check(3), Some(0), "被拒不刷新 last_check");
        assert_eq!(n.claims().fail_streak(3), 0, "被拒不是「取失败」");
        // 再送两遍，计数继续涨，缓存仍是 3。
        // ⚠️ 必须先 rate(4)/rate(5)：否则被拒的原因是「没评过」这条结构性条件，
        // 而不是容量上限（本测试要钉的是后者）。
        assert!(n.try_rate(4, 0.5, 0));
        assert!(n.try_rate(5, 0.5, 0));
        assert!(!n.on_view(4, view.clone(), 0, 6));
        assert!(!n.on_view(5, view.clone(), 0, 7));
        assert_eq!(n.view_count(), 3);
        assert_eq!(n.stats(), (3, 3));
        assert_eq!(n.stat_rejected_limit(), 3);
    }

    /// 守：**已缓存的 owner 在满时仍然接受**（原地覆盖不增长内存）。
    /// 这是与「整条丢弃」最关键的分界：丢刷新没有任何资源收益，只会阻断正常刷新。
    #[test]
    fn t27_已缓存owner满时仍接受并原地覆盖() {
        let mut n = Node::new(cfg_views(2), 1);
        assert!(n.try_rate(1, 0.5, 0) && n.try_rate(2, 0.5, 0));
        assert!(n.on_view(1, SparseView::from_scores(8, &[(40, 1.0)]), 0, 0));
        assert!(n.on_view(2, SparseView::from_scores(8, &[(41, 1.0)]), 0, 0));
        assert_eq!(n.view_count(), 2, "已满");
        // 1 号换一份新观点：必须接受，且是真替换。
        let nv = SparseView::from_scores(8, &[(42, 1.0)]);
        assert!(n.on_view(1, nv, 0, 30), "已缓存的 owner 永远接受");
        assert_eq!(n.view_count(), 2, "原地覆盖，不增长");
        assert_eq!(n.stats(), (3, 0), "刷新计入成功，不计拒绝");
        assert_eq!(n.stat_rejected_limit(), 0, "刷新不是容量拒绝");
        assert_eq!(n.query(40, 30), 0.0, "旧观点被整体替换");
        // 2 号仍在，说明「满」不是「全部锁死」：它的背书照样产生声望。
        assert!(n.query(41, 30) > 0.0, "已缓存来源的观点仍有声望");
        // 但 3 号新 owner 依然进不来。
        assert!(n.try_rate(3, 0.5, 0));
        assert!(!n.on_view(3, SparseView::from_scores(8, &[(43, 1.0)]), 0, 0));
        assert_eq!(n.stat_rejected_limit(), 1);
    }

    /// 守：容量上限**不破坏 \(\Sigma \equiv 1\)**；被拒的新来源既不产生声望也
    /// 不稀释别人（它根本没进缓存，等价于「从来没收到过」）。
    #[test]
    fn t28_容量上限不破坏归一化() {
        let mut n = Node::new(cfg_views(2), 1);
        for i in 0..10u32 {
            n.rate(i, 0.5, 0);
        }
        // 前两份接收，其余 8 份容量拒绝（10 个 owner 都先 rate 过，所以不是结构性拒绝）。
        for i in 0..10u32 {
            let accepted = n.on_view(i, SparseView::from_scores(8, &[(100 + i, 1.0)]), 0, 0);
            assert_eq!(accepted, i < 2, "第 {i} 份的接受性");
        }
        assert_eq!(n.view_count(), 2, "只留下最先到的两份");
        assert_eq!(n.stat_rejected_limit(), 8);
        let d = n.distribution(0);
        let sum = d.sum();
        assert!((sum - 1.0).abs() < 1e-6, "Σ = {sum}");
        assert_eq!(
            d.len(),
            12,
            "10 个个人项（账户 0~9）+ 2 个群体项（100、101）"
        );
        assert!(d.get(100) > 0.0 && d.get(101) > 0.0, "前两份观点的背书有效");
        assert_eq!(d.get(102), 0.0, "被拒的来源不该出现在分布里");
    }

    /// 守：同输入跑两遍，位模式完全相同——容量上限的判定不引入任何随机性
    /// （它只看 `(|views|, owner)`，不看到达时间以外的任何东西）。
    #[test]
    fn t29_同输入两遍位模式相同() {
        let run = || {
            let mut n = Node::new(cfg_limits(3, 5), 42);
            for i in 0..10u32 {
                let _ = n.try_rate(i, 0.1 + (i as f32) * 0.05, 0);
            }
            for i in 0..10u32 {
                let slots: Vec<(u32, f32)> = (0..5u32)
                    .map(|j| (200 + j, ((i + j) % 6 + 1) as f32))
                    .collect();
                let _ = n.on_view(i, SparseView::from_scores(16, &slots), 0, i as u64);
            }
            let d = n.distribution(10);
            (
                bits(&d),
                n.stats(),
                n.stat_rejected_limit(),
                n.stat_rejected_list(),
                n.view_count(),
                n.claims().len(),
            )
        };
        let a = run();
        let b = run();
        assert_eq!(a, b, "同一输入两遍必须逐位相同（含统计与计数）");
        assert_eq!(a.4, 3, "视图数被上限钉在 3");
        assert_eq!(a.5, 5, "清单数被上限钉在 5");
        assert_eq!(
            a.2, 2,
            "容量拒绝只有 2 次（3、4 号；5~9 号是「没评过」的结构性拒绝）"
        );
        assert_eq!(a.3, 5, "清单满了之后 5 次新增被拒（账户 5~9）");
        assert_eq!(a.1, (3, 7), "stats = (3 成功, 5 结构性 + 2 容量 = 7 被拒)");
    }

    /// 守：**真喂 100 万份观点**之后 `view_count() <= max_views`、清单也有界，
    /// 且内存估算落在上界之内。这是本任务的核心验收。
    ///
    /// 怎么做到「100 万次尝试」而内存不炸：清单和视图**两道上限都开着**（默认
    /// 4096/4096）。前 4096 条把两边的空位填满；之后每轮先 `rate(老账户, 0)` 撤销
    /// 一条腾出清单名额、再 `rate(新账户)` 补进来、再 `on_view(新账户)`。
    /// 视图这边因为「只拒新增」永远进不去，而清单这边靠撤销维持 4096 条，
    /// 所以**两边的内存始终有界**——这正是本任务要证明的东西。
    /// （被撤销正分的账户在 `views` 里留了残留条目，那是既有行为，见 t20。）
    ///
    /// 字节估算按**真实内存表示**写，并把「序列化长度 ≠ 内存占用」这条写清楚。
    /// 每条缓存 ≈ `6·n_slots`（`Vec<(Acc, u16)>` 堆上元素）加 24 B（`Vec` 头）
    /// 加 16 B（`Received` 的两个 `Time`）加 `BTreeMap` 节点开销（保守取 64 B）。
    /// 序列化口径 `max_views × (cap×6 + 2)` 才是最宽松的绝对上界（cap = 512 时
    /// 4096 × 3074 B，约 12.6 MiB），真实占用远小于它。
    #[test]
    fn t30_喂百万视图后缓存有界() {
        const MAX_VIEWS: usize = 4096;
        const MAX_LIST: usize = 4096;
        const ATTEMPTS: u32 = 1_000_000;
        let mut n = Node::new(cfg_limits(MAX_VIEWS, MAX_LIST), 1);
        let view = SparseView::from_scores(512, &[(7, 1.0)]);
        let mut accepted = 0u64;
        for i in 0..ATTEMPTS {
            // 清单满了就撤销一条最老的（腾名额），保证「尝试次数」能继续涨。
            if i as usize >= MAX_LIST {
                let _ = n.try_rate(i - MAX_LIST as u32, 0.0, 0);
            }
            let _ = n.try_rate(i, 0.5, 0);
            if n.on_view(i, view.clone(), 0, 0) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_VIEWS as u64, "只有前 max_views 份能进缓存");
        assert!(
            n.view_count() <= MAX_VIEWS,
            "喂了 {ATTEMPTS} 份之后 view_count = {} 超过上限 {MAX_VIEWS}",
            n.view_count()
        );
        assert_eq!(
            n.view_count(),
            MAX_VIEWS,
            "上限会被顶满，说明保护真的在起作用"
        );
        assert_eq!(n.claims().len(), MAX_LIST, "清单同样被钉在 max_list");
        assert_eq!(n.stat_rejected_limit(), u64::from(ATTEMPTS) - 4096);
        // 内存估算（真实表示，不是序列化长度）。
        let payload = n
            .views
            .values()
            .map(|r| r.view.len() * 6 + 24 + 16)
            .sum::<usize>();
        let node_overhead = n.view_count() * 64;
        let real_upper = payload + node_overhead;
        let serialized_upper = n.view_count() * (512 * 6 + 2);
        assert!(
            real_upper < serialized_upper,
            "真实内存估算 {real_upper} B 必须小于序列化口径 {serialized_upper} B"
        );
        assert!(
            real_upper < MAX_VIEWS * (512 * 6 + 2),
            "绝对上界 4096 × (512×6+2) = {} 字节，实测估算 {real_upper}",
            MAX_VIEWS * (512 * 6 + 2)
        );
    }

    /// 守：`try_rate` 在清单满时返回 `false` 且**一个字都不写**（不涨版本、
    /// 不改总分、不写 last_check）；`rate` 同路径静默但可观测。
    #[test]
    fn t31_try_rate满时返回false且不写() {
        let mut n = Node::new(cfg_list(2), 1);
        assert!(n.try_rate(1, 0.5, 10));
        assert!(n.try_rate(2, 0.25, 20));
        assert_eq!(n.claims().len(), 2);
        let before = (
            n.claims().len(),
            n.claims().version(),
            n.claims().total(),
            n.claims().last_check(1),
            n.claims().last_check(2),
        );
        let acc = n.try_rate(3, 1.0, 30);
        assert!(!acc, "清单满了，新账户必须被拒");
        let after = (
            n.claims().len(),
            n.claims().version(),
            n.claims().total(),
            n.claims().last_check(1),
            n.claims().last_check(2),
        );
        assert_eq!(before, after, "被拒的 try_rate 不许改任何字段");
        assert_eq!(n.stat_rejected_list(), 1);
        assert!(!n.claims().has(3));
        // rate 走同一条路径：静默、但仍被计数。
        n.rate(4, 1.0, 40);
        assert_eq!(n.claims().len(), 2, "rate 也一样静默丢弃");
        assert_eq!(n.stat_rejected_list(), 2);
        // 改分 / 撤销永远接受。
        assert!(n.try_rate(1, 0.9, 50), "改分永远接受");
        assert!(n.try_rate(2, 0.0, 60), "撤销永远接受，并腾出名额");
        assert!(n.try_rate(5, 0.5, 70), "腾出的名额给新账户");
        assert_eq!(n.stat_rejected_list(), 2, "改分 / 撤销 / 腾名额都不计数");
    }

    /// 守：`stat_rejected_limit() <= stats().1` 恒成立，且两者都单调递增；
    /// `stats()` 的二元组形状与含义不变（成功数不含被拒的）。
    #[test]
    fn t32_容量拒绝计数的不变式与单调性() {
        let mut n = Node::new(cfg_views(1), 1);
        assert_eq!(n.stats(), (0, 0));
        assert_eq!(n.stat_rejected_limit(), 0);
        let view = SparseView::from_scores(8, &[(7, 1.0)]);
        n.rate(1, 0.5, 0);
        assert!(n.on_view(1, view.clone(), 0, 0));
        // 结构性拒绝（没评过 / 空表）与容量拒绝各自走各自的路径。
        assert!(!n.on_view(2, view.clone(), 0, 0), "没评过的来源");
        let mut prev_limit = n.stat_rejected_limit();
        let mut prev_rejected = n.stats().1;
        for i in 3..50u32 {
            n.rate(i, 0.5, 0);
            assert!(!n.on_view(i, view.clone(), 0, 0), "容量拒绝");
            let (succ, rej) = n.stats();
            assert!(n.stat_rejected_limit() <= rej, "不变式：容量拒绝 ⊆ 总拒绝");
            assert!(n.stat_rejected_limit() >= prev_limit, "单调不减");
            assert!(rej >= prev_rejected, "单调不减");
            assert_eq!(succ, 1, "被拒的观点不计成功");
            prev_limit = n.stat_rejected_limit();
            prev_rejected = rej;
        }
        assert_eq!(n.stat_rejected_limit(), 47, "3..50 共 47 份容量拒绝");
        assert_eq!(n.stats(), (1, 48), "总拒绝 = 1 结构性（2 号）+ 47 容量");
        assert_eq!(n.view_count(), 1);
    }

    /// 守：\(\Sigma v \equiv 1\) 不被容量上限破坏（每个 owner 只在缓存里出现一次）；
    /// 且上限 = 视图数时分布与「无上限」逐位相同（说明上限不是计算路径的一部分）。
    #[test]
    fn t33_上限不改变已有缓存上的计算结果() {
        let build = |max_views: usize| {
            let mut n = Node::new(cfg_views(max_views), 1);
            for i in 0..5u32 {
                n.rate(i, 0.5, 0);
            }
            for i in 0..5u32 {
                let slots: Vec<(u32, f32)> = (0..4u32)
                    .map(|j| (50 + j, ((i + j) % 5 + 1) as f32))
                    .collect();
                let ok = n.on_view(i, SparseView::from_scores(16, &slots), 0, 0);
                assert!(ok, "上限 {max_views} 装得下 5 份");
            }
            bits(&n.distribution(0))
        };
        let limited = build(5);
        let unlimited = build(0);
        assert_eq!(limited, unlimited, "上限够用时结果必须逐位相同");
        assert_eq!(
            limited.len(),
            9,
            "5 个个人项（账户 0~4）+ 4 个群体项（账户 50~53）"
        );
    }
}
