//! # tm_store —— trustmesh 存储层
//!
//! 本 crate 只做「存」，不做「算」：它保存去中心化信任网络的本地状态，
//! 并把「本轮该问谁」这件事抽象成一个不放回的均匀抽样器。
//!
//! 提供的三个基础结构：
//!
//! * [AccountTable]：账户公钥（32 字节）与紧凑整数编号之间的双向映射，
//!   编号从 0 连续递增、永不复用；压缩后的观点（Count Sketch）用编号定位槽位。
//! * [PositiveList]：本地维护的「正分清单」。只保存 score > 0 的条目，
//!   每条同时记录「上次成功取到对方观点的时间戳」，供上层计算指数衰减因子。
//! * [RoundSampler]：轮次抽样器。从正分清单中**不放回**地抽取，一轮内不重复，
//!   抽完自动开启新一轮。另提供按 1/2^连败 加权的退避抽样
//!   （[RoundSampler::draw_backoff]）与不看退避的兜底探测
//!   （[RoundSampler::draw_all]），对应设计规格 4.9.4。
//!
//! 另外提供 [Rng]：一个基于 xorshift64 的确定性伪随机数发生器，
//! 满足「不引入任何外部 crate」的工程约束。
//!
//! ## 设计约束
//!
//! 1. 评分只收正数（0 表示「没表态」；没有负分）。应用层可以给 0~n 的原始数值，
//!    存储层只按 [MAX_SCORE]（1.0e9）截断，不按 1.0 截断。
//! 2. 抽样只从「正分清单」里等概率抽取，**不按评分加权**。
//! 3. 抽样失败是正常路径，存储层不参与重试、不阻塞、也不更新时间戳。
//!    它只把「连续失败了几次」记下来（[PositiveList::record_failure]），
//!    供上层做退避；连败只在**成功**时清零并刷新时间
//!    （[PositiveList::record_success]）。
//! 4. 编号只增不减、永不回收——同一个 key 多次 intern 必然返回同一编号。
//! 5. 全部实现只依赖 std。
//! 6. 退避只改**抽样概率**（1/2^连败），不改评分、不改上次检查时间、
//!    也不改轮次语义；兜底探测（[RoundSampler::draw_all]）完全不看退避。
//!    参数默认值见设计规格 4.9.5（H_source = 72h、T_remove = 168h、T_probe = 24h）。
//!
//! ## 快速上手
//!
//! ~~~rust
//! use tm_engine::store::{AccountTable, PositiveList, RoundSampler, Rng};
//!
//! // 1. 账户表：公钥 -> 编号
//! let mut accounts = AccountTable::new();
//! let alice = accounts.intern([1u8; 32]);
//! let bob = accounts.intern([2u8; 32]);
//! assert_eq!(accounts.intern([1u8; 32]), alice); // 幂等
//! assert_eq!(accounts.len(), 2);
//!
//! // 2. 正分清单：只收 score > 0 的条目
//! let mut list = PositiveList::new();
//! list.set(alice, 0.8, 100);
//! list.set(bob, 0.3, 100);
//! assert_eq!(list.get(alice), Some(0.8));
//!
//! // 3. 轮次抽样：均匀、不放回、抽完自动重置
//! let mut sampler = RoundSampler::new();
//! sampler.reset(&list.indices());
//! let mut rng = Rng::new(42);
//! assert_eq!(sampler.draw(2, &mut rng).len(), 2);
//!
//! // 4. 抽样失败 -> 不更新时间戳；抽样成功 -> touch
//! assert_eq!(list.last_check(alice), Some(100));
//! list.touch(alice, 172);
//! assert_eq!(list.last_check(alice), Some(172));
//! assert_eq!(list.get(alice), Some(0.8)); // 分数不变
//!
//! // 5. 连败计数：失败累加、成功清零；退避只读它，不动 last_check
//! assert_eq!(list.record_failure(bob), 1);
//! assert_eq!(list.fail_streak(bob), 1);
//! list.record_success(bob, 172);
//! assert_eq!(list.fail_streak(bob), 0);
//! assert_eq!(list.last_check(bob), Some(172));
//! ~~~

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 账户公钥：定长 32 字节（例如 Ed25519 公钥）。
pub type AccountKey = [u8; 32];

/// 账户编号：由 [AccountTable] 分配的紧凑整数，从 0 开始连续递增。
pub type AccountIdx = u32;

/// 评分：正数表示信任程度，0 表示「没表态」。取值范围是 (0, MAX_SCORE]，
/// 应用层给 0~n 的原始数值，存储层按 [MAX_SCORE]（1.0e9）截断；没有负分。
pub type Score = f32;

/// 时间戳：由上层给出的相对时间（秒、毫秒均可，只要单位一致）。
///
/// 存储层只把它当作不透明的时间刻度，不解释其绝对含义，
/// 也不假设它单调递增（时间回拨时如实记录即可）。
pub type Time = u64;

/// 评分的上界。传入超过它的分数会被截断到该值。
/// 存储层的合理性上界（**不是语义上限**）。
///
/// 应用层给的是 0~n 的原始数值（互动次数、点赞数……），库只负责把它换算成占比，
/// 所以这里不能按 1.0 截断。这个值只用来挡住 +∞ / NaN / 荒谬的大数，防止
/// 后续求和溢出。
pub const MAX_SCORE: Score = 1.0e9;

/// 份额的工程下限：任何正分条目的份额都不允许是 0。
///
/// 理论份额 = s ÷ total。极端比值（MIN_POSITIVE ÷ MAX_SCORE ≈ 1.2e-47）
/// 低于 f32 最小次正规数（≈ 1.4e-45），直接转 f32 会变成 0；于是条目仍然
/// 留在正分清单里被抽样，群体项权重却恒为 0 —— 「正分」与「有影响力」脱钩。
/// 这里用最小正**正规数**兜底：它足够小（在任何实际分布里都不影响归一化），
/// 又避免次正规数在部分平台上被 flush-to-zero。只在真实份额下溢为 0 时启用。
const SHARE_FLOOR: Score = f32::MIN_POSITIVE;

// ===========================================================================
// 1. AccountTable —— 账户表
// ===========================================================================

/// 账户公钥与账户编号之间的双向映射。
///
/// * 编号从 0 开始**连续递增**：第 n 个被 intern 的 key 一定拿到编号 n-1。
/// * 编号**永不回收**：条目只增不减，刻意不提供 remove 之类的接口，
///   否则已经写进 Count Sketch 的槽位会错位。
/// * intern 幂等：同一个 key 无论 intern 多少次，都返回同一个编号。
///
/// 内部用 HashMap<AccountKey, AccountIdx> 做正向索引、
/// Vec<AccountKey> 做反向索引（下标即编号），因此
/// lookup 期望 O(1)、key_of 期望 O(1)、intern 期望 O(1)。
///
/// # 例子
///
/// ~~~
/// use tm_engine::store::AccountTable;
///
/// let mut t = AccountTable::new();
/// assert_eq!(t.intern([7u8; 32]), 0);
/// assert_eq!(t.intern([9u8; 32]), 1);
/// assert_eq!(t.intern([7u8; 32]), 0);
/// assert_eq!(t.key_of(1), Some([9u8; 32]));
/// ~~~
#[derive(Debug, Clone, Default)]
pub struct AccountTable {
    /// 公钥 -> 编号
    key_to_idx: HashMap<AccountKey, AccountIdx>,
    /// 编号 -> 公钥；下标即编号，长度即 len()
    idx_to_key: Vec<AccountKey>,
}

impl AccountTable {
    /// 创建一个空账户表。
    pub fn new() -> Self {
        Self {
            key_to_idx: HashMap::new(),
            idx_to_key: Vec::new(),
        }
    }

    /// 把公钥登记进表里，返回它的编号。
    ///
    /// key 已存在时直接返回原编号（幂等，不占用新编号）；
    /// 不存在时分配下一个连续编号。
    ///
    /// # Panics
    ///
    /// 当账户数量达到 u32::MAX（编号空间耗尽）时 panic。
    /// 这个上限由 [AccountIdx] 的类型决定，实际不可能达到。
    pub fn intern(&mut self, key: AccountKey) -> AccountIdx {
        if let Some(&idx) = self.key_to_idx.get(&key) {
            return idx;
        }
        // 编号空间检查必须在 push 之前做，保证 panic 之后表的状态不被破坏。
        assert!(
            self.idx_to_key.len() < AccountIdx::MAX as usize,
            "账户表已满：最多支持 {} 个账户",
            AccountIdx::MAX
        );
        let idx = self.idx_to_key.len() as AccountIdx;
        self.idx_to_key.push(key);
        self.key_to_idx.insert(key, idx);
        idx
    }

    /// 查询公钥对应的编号；未登记返回 None。
    pub fn lookup(&self, key: &AccountKey) -> Option<AccountIdx> {
        self.key_to_idx.get(key).copied()
    }

    /// 查询编号对应的公钥；编号越界返回 None。
    pub fn key_of(&self, idx: AccountIdx) -> Option<AccountKey> {
        self.idx_to_key.get(idx as usize).copied()
    }

    /// 已登记的账户数量（= 下一个将被分配出去的编号）。
    pub fn len(&self) -> usize {
        self.idx_to_key.len()
    }

    /// 表是否为空。
    pub fn is_empty(&self) -> bool {
        self.idx_to_key.is_empty()
    }

    // ---------------- 持久化入口（P4：状态快照） ----------------

    /// 按编号升序导出全部公钥（下标即编号）。空表返回空 `Vec`。
    ///
    /// 编号分配的唯一依据是「第 i 个 intern 拿到编号 i」，所以「编号 0..len
    /// 对应的 key 列表」就是这张表的**完整状态**——正向索引是它的函数，不必单独存。
    ///
    /// 返回的是克隆而不是借用：快照必须能脱离账户表独立存在
    /// （存盘时表还要继续接收新的 intern）。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::AccountTable;
    ///
    /// let mut t = AccountTable::new();
    /// t.intern([1u8; 32]);
    /// t.intern([2u8; 32]);
    /// assert_eq!(t.keys(), vec![[1u8; 32], [2u8; 32]]);
    /// ~~~
    pub fn keys(&self) -> Vec<AccountKey> {
        self.idx_to_key.clone()
    }

    /// 由「编号 0..n 的公钥序列」重建账户表（持久化恢复用）。
    ///
    /// 输入必须是**升序、无重复**的公钥序列：第 i 个元素就拿到编号 i。
    /// 出现重复或超过 `AccountIdx::MAX` 个条目时返回 `None`
    /// （这类输入不是一份合法快照，重建出来的编号布局必然与原件不同，
    /// 与其给出一个「看起来能用、实际已经错位」的表，不如明确拒绝）。
    ///
    /// 重建后 [AccountTable::lookup] / [AccountTable::key_of] / [AccountTable::len]
    /// 与原件**逐项相同**，且之后继续 [AccountTable::intern] 新 key 会接着
    /// 分配下一个连续编号，和一台从未重启过的引擎完全一致。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::AccountTable;
    ///
    /// let mut t = AccountTable::new();
    /// t.intern([1u8; 32]);
    /// t.intern([2u8; 32]);
    /// let rebuilt = AccountTable::from_keys(&t.keys()).expect("合法快照");
    /// assert_eq!(rebuilt.len(), 2);
    /// assert_eq!(rebuilt.key_of(1), Some([2u8; 32]));
    /// assert_eq!(rebuilt.lookup(&[1u8; 32]), Some(0));
    /// ~~~
    #[must_use]
    pub fn from_keys(keys: &[AccountKey]) -> Option<Self> {
        if keys.len() > AccountIdx::MAX as usize {
            return None;
        }
        let mut key_to_idx = HashMap::with_capacity(keys.len());
        for (i, key) in keys.iter().enumerate() {
            if key_to_idx.insert(*key, i as AccountIdx).is_some() {
                // 重复公钥：编号布局会与原件不同，拒绝而不是静默去重。
                return None;
            }
        }
        Some(Self {
            key_to_idx,
            idx_to_key: keys.to_vec(),
        })
    }
}

// ===========================================================================
// 2. PositiveList —— 正分清单
// ===========================================================================

/// 正分清单里的一个条目。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    /// 原始评分，不做衰减
    score: Score,
    /// 上次**成功**取到对方观点的时间；失败不刷新
    last_check: Time,
    /// 连续抽样失败的次数。成功清零、失败 +1、[PositiveList::set] 改分不动它。
    ///
    /// 它只被 [RoundSampler::draw_backoff] 读来做退避，不参与任何计算，
    /// 也不影响 [PositiveList::share] 之类的份额。
    fail_streak: u32,
}

/// 本地维护的「我给谁打过正分」清单。
///
/// 语义约定：
///
/// * 只保存 score > 0 的条目。set 传入 <= 0（含负数、-0.0、NaN）
///   等价于删除该条目，不会留下 score == 0 的占位项。
/// * 传入的分数若大于 [MAX_SCORE]（含 +∞）会被截断为 [MAX_SCORE]（1.0e9）。
///   这是防止后续求和溢出的**存储层上界**，不是「评分 ∈ (0, 1]」的语义上限：
///   应用层可以给 0~n 的原始数值，2.5 会原样保留，1e10 会存成 1e9。
/// * set 无论新增还是修改，都会把 last_check 刷新为传入的 now。
/// * [PositiveList::touch] 只刷新时间、不改分数，用于「成功取到观点」；
///   「抽样失败」时上层不应调用它（失败不更新时间戳）。
/// * 每个条目另记一个**连败计数**（[PositiveList::fail_streak]）：
///   失败 +1（[PositiveList::record_failure]）、成功清零
///   （[PositiveList::record_success]）。它与评分、last_check 相互独立——
///   失败不改时间，改分也不清零连败（设计规格 4.9.6 不变量 4）。
/// * 遍历顺序按编号升序，结果不受哈希随机化影响，便于复现与测试。
///
/// 用 BTreeMap 而不是 HashMap 是为了给 [PositiveList::iter] 一个
/// **确定性**的顺序；条目规模是「我打过正分的人数」，量级有限，
/// O(log n) 足够。
///
/// # 例子
///
/// ~~~
/// use tm_engine::store::PositiveList;
///
/// let mut list = PositiveList::new();
/// list.set(3, 0.5, 10);
/// assert_eq!(list.get(3), Some(0.5));
/// assert_eq!(list.last_check(3), Some(10));
///
/// list.touch(3, 20);           // 只改时间
/// assert_eq!(list.get(3), Some(0.5));
///
/// list.set(3, 0.0, 30);        // 0 表示没表态 -> 删除
/// assert!(!list.has(3));
/// ~~~
#[derive(Debug, Clone, Default)]
pub struct PositiveList {
    entries: BTreeMap<AccountIdx, Entry>,
}

impl PositiveList {
    /// 创建一个空的正分清单。
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 写入/更新一条评分，同时把 last_check 刷新为 now。
    ///
    /// score <= 0（含负数、NaN）表示「没表态」，等价于删除该条目：
    /// 若条目本来就不存在则什么也不做。
    /// score > [MAX_SCORE] 会被截断为 [MAX_SCORE]。
    ///
    /// 新增条目从连败计数 0 开始；**已存在的条目会保留原有连败计数**——
    /// 改分是「我的判断变了」，不是「我刚取到了它的观点」，
    /// 能清零连败的只有 [PositiveList::record_success]。
    pub fn set(&mut self, idx: AccountIdx, score: Score, now: Time) {
        // 注意：NaN 与任何数比较都是 false，所以 score <= 0.0 对 NaN 不成立，
        // 这里必须显式判 NaN，否则 NaN 会被当成「正分」存进去，污染后续加权。
        if score.is_nan() || score <= 0.0 {
            self.entries.remove(&idx);
            return;
        }
        // 允许任意正数（应用层给的 0~n 原始数值），只挡住无穷大/超大值。
        let score = if score.is_finite() {
            score.min(MAX_SCORE)
        } else {
            MAX_SCORE
        };
        match self.entries.get_mut(&idx) {
            // 已有条目：只更新分数与时间，连败计数原样保留。
            Some(entry) => {
                entry.score = score;
                entry.last_check = now;
            }
            // 新条目：没有历史，连败计数从 0 开始。
            None => {
                self.entries.insert(
                    idx,
                    Entry {
                        score,
                        last_check: now,
                        fail_streak: 0,
                    },
                );
            }
        }
    }

    /// 取原始评分（不做衰减）；未表态返回 None。
    pub fn get(&self, idx: AccountIdx) -> Option<Score> {
        self.entries.get(&idx).map(|e| e.score)
    }

    /// 全部评分的总和（影响力预算的分母）。
    ///
    /// 内部用 f64 累加，只在出口转回 f32：1000 条 [MAX_SCORE] 时 f32 顺序连加
    /// 会把 1e12 漂到约 1.0000095e12（相对 9.5e-6），分母偏大会让
    /// [PositiveList::shares] 的总和塌到 0.9999905。改用 f64 中途累加后，
    /// 只剩出口的一次 f32 舍入（相对约 4e-9）。
    pub fn total(&self) -> Score {
        let sum: f64 = self.entries.values().map(|e| f64::from(e.score)).sum();
        sum as Score
    }

    /// 归一化后的份额：该账户占我总配额的比值，所有份额之和恒为 1。
    ///
    /// 这是「切蛋糕」的最终结果：应用层给 0~n 的原始数值，
    /// 这里把它换算成占比。没表过态返回 None。
    ///
    /// 只要评分 > 0，返回的份额就一定 > 0：真实比值若低于 f32 能表示的
    /// 正数下限，会取 `SHARE_FLOOR`（最小正正规数），而不是悄悄下溢成 0。
    pub fn share(&self, idx: AccountIdx) -> Option<Score> {
        let s = self.entries.get(&idx)?.score;
        let total = self.total();
        if total > 0.0 {
            Some(normalized_share(s, total))
        } else {
            None
        }
    }

    /// 完整份额表，总和恒为 1（无条目时为空）。按编号升序。
    ///
    /// 与 [PositiveList::share] 同口径：每一条正分都保证拿到 > 0 的份额，
    /// 极端小的条目不会被下溢抹成 0.0。
    pub fn shares(&self) -> Vec<(AccountIdx, Score)> {
        let total = self.total();
        if total <= 0.0 {
            return Vec::new();
        }
        self.entries
            .iter()
            .map(|(k, e)| (*k, normalized_share(e.score, total)))
            .collect()
    }

    /// 该账户是否打过正分。
    pub fn has(&self, idx: AccountIdx) -> bool {
        self.entries.contains_key(&idx)
    }

    /// 刷新「上次成功取到对方观点」的时间戳。
    ///
    /// 只改时间、不改分数；条目不存在时是空操作（不会凭空创建条目）。
    pub fn touch(&mut self, idx: AccountIdx, now: Time) {
        if let Some(entry) = self.entries.get_mut(&idx) {
            entry.last_check = now;
        }
    }

    /// 取上次成功取到观点的时间戳；未表态返回 None。
    pub fn last_check(&self, idx: AccountIdx) -> Option<Time> {
        self.entries.get(&idx).map(|e| e.last_check)
    }

    // ---------------- 连败计数与可达性（设计规格 4.9.3 / 4.9.4） ----------------

    /// 记录一次抽样失败，返回新的连续失败次数。
    ///
    /// * 只把连败计数加一，**不改** [PositiveList::last_check]、**不改**评分：
    ///   失败不刷新时间，`age` 才会自然增长——这正是让不可达来源
    ///   退出 live 集合（[PositiveList::is_live]）的依据。
    /// * 计数用饱和加法：连续失败到 `u32::MAX` 后停在上限，不会回绕到 0。
    /// * 条目不存在时什么也不做，返回 0：没有这条关系，也就无所谓连败。
    ///
    /// 它是 [RoundSampler::draw_backoff] 的输入：连败 n 次的来源权重降为 1/2^n。
    ///
    /// # 例子
    ///
    /// ~~~rust
    /// use tm_engine::store::PositiveList;
    ///
    /// let mut list = PositiveList::new();
    /// list.set(7, 0.5, 1000);
    /// assert_eq!(list.fail_streak(7), 0);
    /// assert_eq!(list.record_failure(7), 1);
    /// assert_eq!(list.record_failure(7), 2);
    /// assert_eq!(list.last_check(7), Some(1000), "失败不刷新时间");
    /// assert_eq!(list.get(7), Some(0.5), "失败不改评分");
    /// ~~~
    pub fn record_failure(&mut self, idx: AccountIdx) -> u32 {
        match self.entries.get_mut(&idx) {
            Some(entry) => {
                entry.fail_streak = entry.fail_streak.saturating_add(1);
                entry.fail_streak
            }
            None => 0,
        }
    }

    /// 记录一次成功刷新：连败计数清零，并把 last_check 刷新为 now。
    ///
    /// * 这是**唯一**会清零连败计数的入口（[PositiveList::set] 改分不重置退避）。
    /// * 同时等价于一次 [PositiveList::touch]：时间戳前移，评分不变。
    /// * 条目不存在时是空操作：不会凭空创建条目，也不会留下连败记录。
    ///
    /// # 例子
    ///
    /// ~~~rust
    /// use tm_engine::store::PositiveList;
    ///
    /// let mut list = PositiveList::new();
    /// list.set(7, 0.5, 1000);
    /// list.record_failure(7);
    /// list.record_failure(7);
    /// assert_eq!(list.fail_streak(7), 2);
    ///
    /// list.record_success(7, 1072);       // 成功：清零 + 刷新时间
    /// assert_eq!(list.fail_streak(7), 0);
    /// assert_eq!(list.last_check(7), Some(1072));
    /// assert_eq!(list.get(7), Some(0.5)); // 分数不变
    /// ~~~
    pub fn record_success(&mut self, idx: AccountIdx, now: Time) {
        if let Some(entry) = self.entries.get_mut(&idx) {
            entry.fail_streak = 0;
            entry.last_check = now;
        }
    }

    /// 当前连续失败次数；无此条目返回 0。
    ///
    /// 「无此条目」与「有条目但从未失败」的返回值相同（都是 0），
    /// 因为退避只关心次数：没有关系，就没有可退避的对象。
    pub fn fail_streak(&self, idx: AccountIdx) -> u32 {
        self.entries.get(&idx).map_or(0, |e| e.fail_streak)
    }

    /// 是否仍在 live 集合：存在该条目且 `now - last_check < max_age`。
    ///
    /// 判定式与设计规格 4.9.3 完全一致（`age[v] < T_remove`，**严格小于**）：
    ///
    /// * 条目不存在返回 `false`——没有个人评分，就谈不上「还在我的群体项里」；
    /// * `max_age == 0` 恒为 `false`（`age < 0` 不可能成立），这是合法的
    ///   「全部剔除」配置，不会 panic；
    /// * 时间回拨（now < last_check）按 `age = 0` 处理（`saturating_sub`），
    ///   因此仍算 live——存储层不假设时间单调。
    ///
    /// `max_age` 由上层按规格给：默认 `T_remove = 168h`（7 天）。
    /// 判定是纯读取，不修改任何状态；退出 live 之后一次
    /// [PositiveList::record_success] 就能让它立刻回来（剔除不是黑名单）。
    ///
    /// # 例子
    ///
    /// ~~~rust
    /// use tm_engine::store::PositiveList;
    ///
    /// let mut list = PositiveList::new();
    /// list.set(7, 0.5, 1000);
    ///
    /// assert!(list.is_live(7, 1000, 168));        // 刚刷新过
    /// assert!(!list.is_live(7, 1168, 168));       // age == 168，不算 live
    /// assert!(list.is_live(7, 1167, 168));        // age == 167，仍算 live
    ///
    /// assert!(!list.is_live(8, 1000, 168));       // 没有这个条目
    /// assert!(!list.is_live(7, 1000, 0));         // max_age = 0：全部剔除
    /// ~~~
    pub fn is_live(&self, idx: AccountIdx, now: Time, max_age: Time) -> bool {
        self.entries
            .get(&idx)
            .is_some_and(|entry| now.saturating_sub(entry.last_check) < max_age)
    }

    /// 正分条目的数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 清单是否为空（为空时本轮没有可抽样的对象）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按编号升序遍历全部条目：(编号, 原始评分, 上次检查时间)。
    pub fn iter(&self) -> impl Iterator<Item = (AccountIdx, Score, Time)> + '_ {
        self.entries
            .iter()
            .map(|(&idx, e)| (idx, e.score, e.last_check))
    }

    /// 按编号升序返回全部编号。
    ///
    /// 这是给 [RoundSampler::reset] / [RoundSampler::sync] 用的便捷方法，
    /// 等价于 list.iter().map(|(idx, _, _)| idx).collect()。
    pub fn indices(&self) -> Vec<AccountIdx> {
        self.entries.keys().copied().collect()
    }

    // ---------------- 持久化入口（P4：状态快照） ----------------

    /// 一次性导出全部条目：`(编号, 原始评分, 上次检查时间, 连败计数)`，按编号升序。
    ///
    /// 与 [PositiveList::iter] 的区别是**多带了连败计数**：快照/持久化要求
    /// 四件套一次拿全。否则调用方只能先 `iter()` 拿三件套、再逐条
    /// [PositiveList::fail_streak] 补一次，既扫两遍，也容易在两次遍历之间
    /// 读到不一致的状态（快照语义上必须是同一个瞬间）。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::PositiveList;
    ///
    /// let mut list = PositiveList::new();
    /// list.set(3, 0.5, 10);
    /// list.record_failure(3);
    /// assert_eq!(list.items(), vec![(3, 0.5, 10, 1)]);
    /// ~~~
    pub fn items(&self) -> Vec<(AccountIdx, Score, Time, u32)> {
        self.entries
            .iter()
            .map(|(&idx, e)| (idx, e.score, e.last_check, e.fail_streak))
            .collect()
    }

    /// 由 [PositiveList::items] 的条目重建清单（持久化恢复用）。
    ///
    /// 这是 [PositiveList::items] 的**逆操作**。它与「逐条 [PositiveList::set]」
    /// 有两处关键不同，正是这两处让「恢复后逐位一致」成立：
    ///
    /// * `last_check` **原样保留**，不刷新成「现在」（`set` 会刷新）；
    /// * `fail_streak` **原样保留**，不归零（`set` 只对已有条目保留连败，
    ///   而重建时所有条目对新表来说都是「新条目」，会一律从 0 开始）。
    ///
    /// 输入仍按 [PositiveList::set] 的存储规则校验：`score <= 0`、NaN 的条目
    /// **丢弃**（它们本来就不该在清单里），超过 [MAX_SCORE] 的截断到上界；
    /// 同一编号出现多次时**后者覆盖前者**。因此恢复方若要求「一字不差」，
    /// 应当把 [PositiveList::len] 与输入长度比对（畸形条目会被静默丢弃）。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::PositiveList;
    ///
    /// let mut list = PositiveList::new();
    /// list.set(3, 0.5, 100);
    /// list.record_failure(3);
    /// list.record_failure(3);
    /// let restored = PositiveList::from_items(&list.items());
    /// assert_eq!(restored.items(), list.items());
    /// assert_eq!(restored.last_check(3), Some(100), "时间原样保留");
    /// assert_eq!(restored.fail_streak(3), 2, "连败计数原样保留");
    /// ~~~
    pub fn from_items(items: &[(AccountIdx, Score, Time, u32)]) -> Self {
        let mut entries = BTreeMap::new();
        for &(idx, score, last_check, fail_streak) in items {
            // 与 set 同一套校验：NaN / 非正数 = 没表态，不进清单。
            if score.is_nan() || score <= 0.0 {
                continue;
            }
            let score = if score.is_finite() {
                score.min(MAX_SCORE)
            } else {
                MAX_SCORE
            };
            entries.insert(
                idx,
                Entry {
                    score,
                    last_check,
                    fail_streak,
                },
            );
        }
        Self { entries }
    }
}

// ===========================================================================
// 3. RoundSampler —— 轮次抽样器
// ===========================================================================

/// 轮次抽样器：在一轮内**不放回**地从正分清单中均匀抽取。
///
/// # 语义
///
/// * 抽样是**均匀**的：只跟「在不在清单里」有关，跟评分大小无关。
/// * 一轮内**不重复**：抽过的元素从待抽集合里移除。
/// * **抽完自动重置**：调用 [RoundSampler::draw] 时若待抽集合已空，
///   会自动用最近一次同步到的全量清单开启新一轮，然后再抽。
/// * draw(k) 一次最多返回本轮**剩余**的元素；k 大于剩余时只返回剩余的，
///   不会在本轮内绕回去重复抽取（重复只可能出现在下一轮）。
/// * [RoundSampler::sync] 只做两件事：保留「还没抽中且仍在清单里」的元素、
///   丢弃「已从清单里删除」的元素。它**不会**把中途新增的账户塞进当前这一轮，
///   新增账户要等下一次重置（即下轮开始）才会参与。
/// * [RoundSampler::draw_backoff] 在**同样的轮次语义**上再叠一层退避：
///   本轮剩余候选里，连败 n 次的来源按 1/2^n 的权重被抽中，其余不变
///   （不重复、抽完重置、k 大于剩余只给剩余的）。连败全部相同时
///   权重全部相同，它退化为 [RoundSampler::draw]，同一 seed 结果逐个相同。
/// * [RoundSampler::draw_all] 一次取走本轮剩下的**全部**候选，不看退避，
///   供「每 T_probe 强制扫一遍」的兜底探测使用（设计规格 4.9.4）。
///
/// # 实现
///
/// 待抽集合是一个数组；每次抽一个随机下标 j，把 pending[j] 与末尾元素交换
/// 后 pop 掉（即 swap_remove），因此单次抽取是 O(1)，
/// 且不依赖任何重试/拒绝式的「抽到重复就重抽」。退避抽样把「取下标」换成
/// 按权重取（见 pick_weighted），交换弹出这一步完全复用，所以
/// 「一轮内不重复」的保证不依赖权重是否正确。
///
/// # 例子
///
/// ~~~
/// use tm_engine::store::{RoundSampler, Rng};
///
/// let mut s = RoundSampler::new();
/// s.reset(&[10, 20, 30]);
/// assert_eq!(s.remaining(), 3);
///
/// let mut rng = Rng::new(1);
/// let round = s.draw(3, &mut rng);      // 一轮内不重复
/// assert_eq!(round.len(), 3);
/// assert_eq!(s.remaining(), 0);
///
/// let next = s.draw(2, &mut rng);       // 自动开启新一轮
/// assert_eq!(next.len(), 2);
/// assert_eq!(s.remaining(), 1);
/// ~~~
#[derive(Debug, Clone, Default)]
pub struct RoundSampler {
    /// 最近一次同步到的全量清单（升序、去重）。自动重置时用它。
    members: Vec<AccountIdx>,
    /// 本轮尚未抽中的元素；是 members 的子集。
    pending: Vec<AccountIdx>,
}

impl RoundSampler {
    /// 创建一个空的抽样器。
    pub fn new() -> Self {
        Self {
            members: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// 与最新的正分清单对齐。
    ///
    /// * 保留：仍在 current 里的**未抽中**元素（本轮进度不回退）；
    /// * 丢弃：已经不在 current 里的元素（对方被取消评分/删除）；
    /// * 记录：current 成为下一次自动重置时的全量清单。
    ///
    /// 中途新增的账户不会立即进入本轮待抽集合——这保证了「一轮内不重复」
    /// 的同时，也保证了任一时刻待抽集合都是清单的子集。
    ///
    /// 幂等：用同一份清单反复调用不会改变结果。
    pub fn sync(&mut self, current: &[AccountIdx]) {
        let members = normalize(current);
        let alive: BTreeSet<AccountIdx> = members.iter().copied().collect();
        // 用 retain 而不是重建：既保留了随机化后的相对顺序，也保证不会把
        // 已经抽中的元素重新放回本轮。
        self.pending.retain(|idx| alive.contains(idx));
        self.members = members;
    }

    /// 不放回地抽取至多 k 个元素。
    ///
    /// * k == 0：直接返回空数组，不改变任何内部状态（也不会触发重置）；
    /// * 待抽集合已空且 k > 0：先用全量清单自动开启新一轮再抽；
    /// * 全量清单也为空：返回空数组。
    ///
    /// 返回顺序即抽取顺序，长度 = min(k, 本轮剩余)。
    pub fn draw(&mut self, k: usize, rng: &mut Rng) -> Vec<AccountIdx> {
        if k == 0 {
            return Vec::new();
        }
        if !self.ensure_round() {
            return Vec::new();
        }
        let take = k.min(self.pending.len());
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            // 待抽集合非空，因此 next_below 的参数一定 > 0。
            let j = rng.next_below(self.pending.len());
            // 与末尾交换后弹出：O(1) 且天然保证不放回。
            out.push(self.pending.swap_remove(j));
        }
        out
    }

    /// 带退避的抽样：按 1/2^连败 的**相对权重**不放回地抽 k 个。
    ///
    /// 连败 0 次的来源权重 1，1 次 1/2，2 次 1/4……权重小不等于抽不到，
    /// 只是更少被排在前面；单个候选被选中的概率是 `w[i] / Σw`。
    ///
    /// # 语义
    ///
    /// * 轮次位图语义与 [RoundSampler::draw] **完全一致**：k == 0 直接返回空且
    ///   不动状态；k > 0 时本轮已抽空会自动用全量清单重开一轮；
    ///   k 大于本轮剩余时只返回剩余的（不跨轮重复）。
    /// * 权重是**相对**的：同乘一个系数不改变概率，所以实现里以本轮候选的
    ///   最小连败次数为基准归一化，绝对权重 2^-800 这种在 f64 里早已下溢的值
    ///   也能正确比较。指数差另外截断在 BACKOFF_MAX_EXPONENT（64）以内，
    ///   权重下限是精确的 2^-64 > 0，因此**权重再小也抽得满 k 个**，
    ///   绝不会因为「全 0」而少抽。
    /// * 全部候选的连败次数相同时，权重全部相等 → 退化为均匀抽样，
    ///   走的正是 [RoundSampler::draw] 的那条路径（同一次 next_below），
    ///   所以**同一 seed、同一状态下两者的返回值逐个相同**。
    ///   连败次数全为 0 就是这一特例，旧行为因此完全不受影响。
    /// * 退避只读清单：不改 last_check、不改评分、不改连败计数，
    ///   也不改轮次以外的任何状态（list 是只读引用）。
    /// * 清单里查不到的编号按连败 0 次处理。调用方应当先 [RoundSampler::sync]
    ///   把已删除的账户剔出本轮，这与 [RoundSampler::draw] 对 pending 的约定一致。
    ///
    /// # 复杂度
    ///
    /// 最坏 O(k·n)，n = 本轮剩余候选数：每抽一个都要在剩余候选上求一次权重和。
    /// 批大小 k 由应用层决定，通常远小于 n。
    ///
    /// # 例子
    ///
    /// ~~~rust
    /// use tm_engine::store::{PositiveList, RoundSampler, Rng};
    ///
    /// let mut list = PositiveList::new();
    /// list.set(1, 0.5, 0);
    /// list.set(2, 0.5, 0);
    /// list.set(3, 0.5, 0);
    ///
    /// // 2 号连续失败两次 -> 权重降为 1/4，但仍在候选里，只是更少被抽中。
    /// list.record_failure(2);
    /// list.record_failure(2);
    /// assert_eq!(list.fail_streak(2), 2);
    /// assert_eq!(list.last_check(2), Some(0), "退避不动 last_check");
    ///
    /// let mut sampler = RoundSampler::new();
    /// sampler.reset(&list.indices());
    /// let mut rng = Rng::new(7);
    ///
    /// let picked = sampler.draw_backoff(&list, 2, &mut rng);
    /// assert_eq!(picked.len(), 2);
    /// assert_ne!(picked[0], picked[1], "一轮内不重复");
    /// assert_eq!(sampler.remaining(), 1);
    /// ~~~
    pub fn draw_backoff(
        &mut self,
        list: &PositiveList,
        k: usize,
        rng: &mut Rng,
    ) -> Vec<AccountIdx> {
        if k == 0 {
            return Vec::new();
        }
        if !self.ensure_round() {
            return Vec::new();
        }
        let take = k.min(self.pending.len());
        // 连败计数只读一次，之后与 pending 同步 swap_remove，两者始终一一对应。
        let mut streaks: Vec<u32> = self
            .pending
            .iter()
            .map(|&idx| list.fail_streak(idx))
            .collect();
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            let j = pick_weighted(&streaks, rng);
            // 与 draw 相同的交换弹出：不放回由结构保证，与权重无关。
            out.push(self.pending.swap_remove(j));
            streaks.swap_remove(j);
        }
        out
    }

    /// 兜底探测：返回本轮**全部**尚未抽过的条目，完全不看退避。
    ///
    /// 设计规格 4.9.4 要求每 T_probe（默认 24h）强制完整扫一遍正分清单，
    /// 否则退避会让长期离线的来源永远抽不到，它也就永远回不到 live 集合。
    /// 探测本身不看连败计数，也不消耗随机数。
    ///
    /// * 结果按编号**升序**，完全确定、可复现；
    /// * 返回后这些条目即算「本轮已抽过」，本轮剩余清零；
    /// * 本轮已抽空时与 [RoundSampler::draw] 一样**自动开启新一轮**
    ///   （用最近一次同步到的全量清单），因此只要清单非空，
    ///   一次探测必然覆盖一整轮，不会出现「这次探测什么也没问」；
    /// * 全量清单为空（从未同步过成员）时返回空数组；
    /// * 连败计数不影响结果：fail_streak 再大，只要本轮没抽过就会被返回。
    ///
    /// 探测请求**不走退避**，但它的结果照常计入成功/失败：成功了就调用
    /// [PositiveList::record_success]，连败清零、age 归零、重回 live 集合。
    ///
    /// # 例子
    ///
    /// ~~~rust
    /// use tm_engine::store::{PositiveList, RoundSampler, Rng};
    ///
    /// let mut list = PositiveList::new();
    /// for idx in 0..4u32 {
    ///     list.set(idx, 0.5, 0);
    /// }
    /// let mut sampler = RoundSampler::new();
    /// sampler.reset(&list.indices());
    ///
    /// let mut rng = Rng::new(1);
    /// assert_eq!(sampler.draw_backoff(&list, 1, &mut rng).len(), 1);
    /// assert_eq!(sampler.remaining(), 3);
    ///
    /// let probe = sampler.draw_all(&list);   // 兜底：把本轮剩下的全问一遍
    /// assert_eq!(probe.len(), 3);
    /// assert_eq!(sampler.remaining(), 0);
    /// ~~~
    pub fn draw_all(&mut self, list: &PositiveList) -> Vec<AccountIdx> {
        // 兜底探测不看清单内容（不看退避、也不看评分），参数是为与
        // draw_backoff 保持同一调用形状而保留的，应用层可以在两条路径之间直接切换。
        let _ = list;
        if !self.ensure_round() {
            return Vec::new();
        }
        let mut out = std::mem::take(&mut self.pending);
        // 固定升序：不消耗随机数，结果可复现，便于断言与日志比对。
        out.sort_unstable();
        out
    }

    /// 本轮还剩多少个没抽。
    ///
    /// 返回 0 表示本轮已抽完；下一次 draw(k > 0) 会自动开启新一轮。
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }

    /// 本轮**尚未抽中**的编号（快照与调试用），顺序即待抽集合的内部顺序。
    ///
    /// 为什么需要它：轮次进度是一份**有状态的**随机化数组（每次 swap_remove
    /// 后相对顺序都会变），只靠 `remaining()` 的计数无法把它重新构造出来。
    /// 持久化要求「关机前存盘、开机后恢复」，恢复出来的抽样器必须与原抽样器
    /// 抽到**同一个顺序的同一批人**，否则随机流一恢复就会立刻分叉。
    ///
    /// 返回的切片可以直接喂给 [RoundSampler::from_parts] 重建；它与
    /// [RoundSampler::members] 的差别是：members 是「下一次自动重置用的全量」，
    /// pending 是「本轮剩下的子集」。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::{RoundSampler, Rng};
    ///
    /// let mut s = RoundSampler::new();
    /// s.reset(&[4, 5, 6]);
    /// let mut rng = Rng::new(1);
    /// assert_eq!(s.draw(1, &mut rng).len(), 1);
    /// assert_eq!(s.pending().len(), 2, "抽走的那个不再在待抽集合里");
    /// assert_eq!(s.remaining(), s.pending().len());
    /// ~~~
    #[must_use]
    pub fn pending(&self) -> &[AccountIdx] {
        &self.pending
    }

    /// 下一次自动重置用的**全量清单**（快照与调试用），恒为升序去重。
    ///
    /// 它是 [RoundSampler::sync] 最近一次同步到的成员集合，只在
    /// [RoundSampler::start_new_round] 时被复制进待抽集合。
    /// 恢复时必须与 [RoundSampler::pending] 一起保存：只有 pending 而没有
    /// members 的话，本轮抽空后自动开的新一轮会退化（要么空，要么漏人）。
    #[must_use]
    pub fn members(&self) -> &[AccountIdx] {
        &self.members
    }

    /// 由 [RoundSampler::members] 与 [RoundSampler::pending] 原样重建抽样器。
    ///
    /// 这是快照恢复的入口。语义是「**逐字节恢复**」，不做任何清洗：
    ///
    /// * `pending` 的**顺序被原样保留**——它是随机化过的，重排会让下一次
    ///   [RoundSampler::draw] 抽出不同的人；
    /// * `pending` 里即使有不在 `members` 里的编号也照样保留，不在重建时剔除。
    ///   这是刻意的：越权数据应当由上层（`Engine::restore`）判定为畸形并拒绝，
    ///   存储层不替它做主，也就不会在「畸形输入」和「合法输入」之间给出
    ///   难以预料的部分修复结果。
    ///
    /// 合法性检查（`pending` 是 `members` 的子集、两者都不含重复编号）
    /// 留给恢复方；本函数只负责把数据装进去。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::{RoundSampler, Rng};
    ///
    /// let mut s = RoundSampler::new();
    /// s.reset(&[1, 2, 3]);
    /// let mut rng = Rng::new(7);
    /// let drawn = s.draw(1, &mut rng);
    /// let cloned = RoundSampler::from_parts(s.members(), s.pending());
    /// assert_eq!(cloned.remaining(), s.remaining());
    /// assert_eq!(cloned.pending(), s.pending());
    /// let _ = drawn;
    /// ~~~
    pub fn from_parts(members: &[AccountIdx], pending: &[AccountIdx]) -> Self {
        Self {
            members: members.to_vec(),
            pending: pending.to_vec(),
        }
    }

    /// 强制开始新一轮：用 current 作为全量清单，清空本轮进度。
    ///
    /// 与 [RoundSampler::sync] 的区别：reset 会把已经抽中的元素**放回**待抽集合，
    /// 而 sync 只做剔除。输入里的重复编号会被去重。
    pub fn reset(&mut self, current: &[AccountIdx]) {
        self.members = normalize(current);
        self.start_new_round();
    }

    /// 保证本轮还有候选；返回 false 表示全量清单为空、抽不出东西。
    ///
    /// draw / draw_backoff / draw_all 共用这一段，让「本轮已空则自动开新一轮」
    /// 的语义只有一处实现。
    fn ensure_round(&mut self) -> bool {
        if self.pending.is_empty() {
            if self.members.is_empty() {
                return false;
            }
            // 本轮已抽空：自动重置为全量，再抽。
            self.start_new_round();
        }
        true
    }

    /// 把待抽集合恢复成全部成员。
    fn start_new_round(&mut self) {
        self.pending.clear();
        self.pending.extend_from_slice(&self.members);
    }
}

/// 把原始评分换算成 f32 份额；只有真实份额下溢为 0 时才抬到 [SHARE_FLOOR]。
///
/// 中间的除法用 f64：`MIN_POSITIVE / MAX_SCORE ≈ 1.2e-47` 在 f32 里会直接
/// 变成 0，而在 f64 里仍是一个可表示的正数。转回 f32 后若仍为 0（真实份额
/// 低于 f32 最小次正规数），才启用下限——正常份额（例如 1/N）不会被改动。
fn normalized_share(score: Score, total: Score) -> Score {
    let share = (f64::from(score) / f64::from(total)) as Score;
    if share > 0.0 {
        share
    } else if score > 0.0 {
        SHARE_FLOOR
    } else {
        0.0
    }
}

/// 去重并升序排列，作为内部「全量清单」的规范形式。
///
/// 升序不影响均匀性（抽样下标本身是均匀随机的），但让状态可复现、可断言。
fn normalize(current: &[AccountIdx]) -> Vec<AccountIdx> {
    let mut v = current.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

// ===========================================================================
// 4. 退避抽样用到的内部工具（设计规格 4.9.4）
// ===========================================================================

/// 退避权重的指数上限。
///
/// 连败次数的差值再大，权重也只压到 2^-64（约 5.4e-20）：再往下压既没有
/// 统计意义，又会让 1/2^n 在 f64 里下溢成 0，逼出「全部权重都是 0」这种
/// 需要额外分支的退化情形。64 还保证 2^-64 是 f64 的**精确值**。
const BACKOFF_MAX_EXPONENT: u32 = 64;

/// 2^-k 的精确值（k <= BACKOFF_MAX_EXPONENT）。
///
/// 直接拼 IEEE-754 位模式：指数域 = 1023 - k、尾数域全 0。
/// 不经过任何浮点幂运算，因此没有舍入、也没有平台差异。
fn pow2_neg(k: u32) -> f64 {
    debug_assert!(k <= BACKOFF_MAX_EXPONENT, "2^-{k} 超出退避权重的取值范围");
    f64::from_bits(((1023 - k) as u64) << 52)
}

/// 退避权重：连败 streak 次、本轮候选里最小的连败次数为 min_streak 时的
/// **相对**权重。
///
/// 结果等于 2^-(streak - min_streak)，指数差截断在 BACKOFF_MAX_EXPONENT。
/// 用相对指数而不是绝对权重 2^-streak，是因为后者在 streak 很大时会在
/// f64 里下溢成 0；而相对权重与绝对权重只差一个公共因子，
/// 归一化之后每个候选被选中的概率完全相同。
fn backoff_weight(streak: u32, min_streak: u32) -> f64 {
    let diff = streak.saturating_sub(min_streak).min(BACKOFF_MAX_EXPONENT);
    pow2_neg(diff)
}

/// 产生 [0, 1) 内的均匀浮点数（53 位精度）。
///
/// 取 64 位输出的高 53 位再除以 2^53：被除数不超过 2^53-1、除数与除法都是
/// 2 的幂，所以每一步都能被 f64 精确表示，结果没有舍入，且 2^53 个可表示值
/// 等概率出现。
fn next_unit(rng: &mut Rng) -> f64 {
    // 2^53：f64 能精确表示 [0, 2^53] 内的全部整数。
    const SCALE: f64 = 9_007_199_254_740_992.0;
    (rng.next_u64() >> 11) as f64 / SCALE
}

/// 在给定的连败计数上，按 1/2^连败 的相对权重挑一个下标。
///
/// * 全部连败次数相同 → 权重全部相等 → 直接 next_below(len)，
///   与均匀抽样**逐位一致**（消费同一次随机数、给出同一个下标）；
/// * 否则按累积权重取一个，概率严格等于 `w[i] / Σw`；
/// * 权重和异常（理论上不可达：下限是 2^-64）时退回均匀抽取，
///   保证「权重太小导致抽不满」永远不会发生。
///
/// 调用方保证候选非空。
fn pick_weighted(streaks: &[u32], rng: &mut Rng) -> usize {
    debug_assert!(!streaks.is_empty(), "没有候选就不该调用 pick_weighted");

    let mut min = u32::MAX;
    let mut max = 0u32;
    for &s in streaks {
        min = min.min(s);
        max = max.max(s);
    }
    if min == max {
        // 权重全相等：退化为均匀抽样，与 RoundSampler::draw 走同一条路径。
        return rng.next_below(streaks.len());
    }

    // 第一遍：求权重和。权重都是精确的 2 的幂，求和只有一次舍入。
    let total: f64 = streaks.iter().map(|&s| backoff_weight(s, min)).sum();
    if total <= 0.0 || !total.is_finite() {
        // 理论不可达；仍然给出定义：在剩余候选里均匀补一个，绝不少抽。
        return rng.next_below(streaks.len());
    }

    // 第二遍：在 [0, total) 里落一个点，取第一个让累积和超过它的候选。
    let target = next_unit(rng) * total;
    let mut acc = 0.0;
    let mut last_positive = 0usize;
    let mut seen_positive = false;
    for (i, &s) in streaks.iter().enumerate() {
        let w = backoff_weight(s, min);
        if w > 0.0 {
            last_positive = i;
            seen_positive = true;
        }
        acc += w;
        if target < acc {
            return i;
        }
    }
    // 浮点累加的舍入可能让 target 略大于总和：退回最后一个正权重候选。
    if seen_positive {
        last_positive
    } else {
        rng.next_below(streaks.len())
    }
}

// ===========================================================================
// 5. Rng —— xorshift64
// ===========================================================================

/// 基于 xorshift64 的确定性伪随机数发生器。
///
/// 算法（Marsaglia 的 xorshift64，三元组 13/7/17）：
///
/// ~~~text
/// x ^= x << 13;  x ^= x >> 7;  x ^= x << 17;
/// ~~~
///
/// 性质：状态非零时周期为 2^64 - 1；状态 0 是不动点，
/// 因此 [Rng::new] 会把 0 换成内置常量种子。
///
/// 说明：这是统计意义上够用、且完全无依赖的伪随机源，
/// **不是**密码学安全的——本项目只需要抽样均匀，不需要抗预测。
///
/// # 例子
///
/// ~~~
/// use tm_engine::store::Rng;
///
/// let mut rng = Rng::new(2024);
/// assert!(rng.next_below(10) < 10);
///
/// // 相同种子 -> 相同序列
/// let mut a = Rng::new(7);
/// let mut b = Rng::new(7);
/// assert_eq!(a.next_u64(), b.next_u64());
/// ~~~
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

/// 种子为 0 时使用的替代种子（黄金分割比的高 64 位，分布良好）。
const FALLBACK_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

impl Rng {
    /// 用给定种子初始化。种子为 0 时自动替换为内置常量（0 是不动点）。
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 { FALLBACK_SEED } else { seed };
        Self { state }
    }

    /// 产生下一个 64 位随机数。
    ///
    /// 只要状态非零，输出序列就永不重复（周期 2^64 - 1）。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// 当前内部状态（持久化用）。
    ///
    /// 这是「精确恢复」的关键：xorshift64 的下一个输出只由这 64 位决定，
    /// 所以把状态存下来、再用 [Rng::from_state] 装回去，之后的随机序列
    /// 与原发生器**逐位相同**——不是「同一个种子」，而是「同一个位置」。
    ///
    /// 返回 0 也是合法的（它只会在状态真的被摆成 0 的畸形输入里出现）；
    /// 正常使用时状态恒非零（0 是 xorshift 的不动点，[Rng::new] 会把它换掉）。
    #[must_use]
    pub fn state(&self) -> u64 {
        self.state
    }

    /// 用一份**内部状态**（而不是种子）恢复发生器。
    ///
    /// 这是「把发生器停在中途、之后再从同一个位置继续」的入口：
    /// 快照里存的是 [Rng::state]，恢复时原样装回去即可，
    /// `Rng::from_state(a.state())` 之后的输出与 `a` **逐位相同**。
    ///
    /// 状态 0 与 [Rng::new] 的种子 0 得到同样的处理：换成内置常量
    /// 【FALLBACK_SEED】。0 是 xorshift 的不动点，一旦装进去，
    /// [Rng::next_u64] 恒返回 0，[Rng::next_below] 的拒绝采样会**死循环**
    /// （例如 n = 9 时拒绝区间是 [0, 7)，0 永远落在里面）。
    ///
    /// 一次**真实的**引擎运行永远走不到状态 0（xorshift 的非零状态不回到 0，
    /// 构造入口也都会把 0 换掉），所以这条规范化只影响畸形输入；
    /// 上层若要求「逐位恢复」，仍应把 `rng_state == 0` 当畸形快照拒绝
    /// （见 `tm_engine::EngineSnapshot::decode`）。
    ///
    /// [Rng::new] 与 `from_state` 在同一条 xorshift 递推上是**同一台机器**：
    /// `Rng::from_state(Rng::new(s).state())` 与 `Rng::new(s)` 逐位等价。
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::Rng;
    ///
    /// let mut a = Rng::new(2024);
    /// let _ = a.next_u64();
    /// let mut b = Rng::from_state(a.state());
    /// for _ in 0..10 {
    ///     assert_eq!(a.next_u64(), b.next_u64(), "恢复后序列必须完全相同");
    /// }
    ///
    /// // 状态 0 被换成内置常量，与 new(0) 同一台机器，不会死循环。
    /// assert_eq!(Rng::from_state(0).state(), Rng::new(0).state());
    /// ~~~
    ///
    /// # 例子
    ///
    /// ~~~
    /// use tm_engine::store::Rng;
    ///
    /// let mut a = Rng::new(2024);
    /// let _ = a.next_u64();
    /// let mut b = Rng::from_state(a.state());
    /// for _ in 0..10 {
    ///     assert_eq!(a.next_u64(), b.next_u64(), "恢复后序列必须完全相同");
    /// }
    /// ~~~
    #[must_use]
    pub fn from_state(state: u64) -> Self {
        // 0 是 xorshift 的不动点；装进去会让 next_below 的拒绝采样死循环。
        // 与 Rng::new 对种子 0 的处理完全一致（同一台机器的同一条规则）。
        let state = if state == 0 { FALLBACK_SEED } else { state };
        Self { state }
    }

    /// 产生 [0, n) 内的均匀随机整数。
    ///
    /// 用「拒绝采样」消除取模偏差：落在 [0, 2^64 mod n) 的样本被丢弃，
    /// 剩下的区间长度是 n 的整数倍，因此 x % n 严格均匀。
    /// 拒绝概率小于 n / 2^64，实际几乎不会发生。
    ///
    /// # Panics
    ///
    /// n == 0 时 panic（长度为零的集合没有合法下标）。
    pub fn next_below(&mut self, n: usize) -> usize {
        assert!(n > 0, "next_below(n) 要求 n > 0，收到 0");
        let n64 = n as u64;
        if n64 == 1 {
            return 0;
        }
        // 2^64 mod n == (-n) mod n，即被拒绝区间的上界。
        let zone = n64.wrapping_neg() % n64;
        loop {
            let x = self.next_u64();
            if x >= zone {
                return (x % n64) as usize;
            }
        }
    }
}

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造互不相同的测试公钥。
    fn make_key(n: u32) -> AccountKey {
        let mut k = [0u8; 32];
        k[0..4].copy_from_slice(&n.to_be_bytes());
        k
    }

    fn set_of(v: &[u32]) -> BTreeSet<AccountIdx> {
        v.iter().copied().collect()
    }

    // ------------------------------------------------------------------
    // AccountTable
    // ------------------------------------------------------------------

    #[test]
    fn account_table_new_is_empty() {
        let t = AccountTable::new();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        assert_eq!(t.lookup(&make_key(1)), None);
        assert_eq!(t.key_of(0), None);
    }

    #[test]
    fn intern_assigns_contiguous_indices() {
        let mut t = AccountTable::new();
        for n in 0..5u32 {
            assert_eq!(t.intern(make_key(n)), n, "编号必须从 0 连续递增");
            assert_eq!(t.len(), n as usize + 1);
        }
        assert!(!t.is_empty());
    }

    #[test]
    fn intern_same_key_is_idempotent() {
        let mut t = AccountTable::new();
        let first = t.intern(make_key(42));
        for _ in 0..100 {
            assert_eq!(t.intern(make_key(42)), first);
        }
        assert_eq!(t.len(), 1, "重复 intern 不得占用新编号");
    }

    #[test]
    fn intern_interleaved_keys_keep_first_seen_order() {
        let mut t = AccountTable::new();
        assert_eq!(t.intern(make_key(1)), 0);
        assert_eq!(t.intern(make_key(2)), 1);
        assert_eq!(t.intern(make_key(1)), 0);
        assert_eq!(t.intern(make_key(3)), 2);
        assert_eq!(t.intern(make_key(2)), 1);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn lookup_and_key_of_roundtrip() {
        let mut t = AccountTable::new();
        let keys: Vec<AccountKey> = (0..16u32).map(make_key).collect();
        for (i, k) in keys.iter().enumerate() {
            let idx = t.intern(*k);
            assert_eq!(idx as usize, i);
            assert_eq!(t.lookup(k), Some(idx));
            assert_eq!(t.key_of(idx), Some(*k));
        }
    }

    #[test]
    fn lookup_unknown_key_returns_none() {
        let mut t = AccountTable::new();
        t.intern(make_key(1));
        assert_eq!(t.lookup(&make_key(2)), None);
        assert_eq!(t.lookup(&[0xFFu8; 32]), None);
    }

    #[test]
    fn key_of_out_of_range_returns_none() {
        let mut t = AccountTable::new();
        t.intern(make_key(1));
        t.intern(make_key(2));
        assert_eq!(t.key_of(2), None, "len 处的编号尚未分配");
        assert_eq!(t.key_of(3), None);
        assert_eq!(t.key_of(AccountIdx::MAX), None);
    }

    #[test]
    fn keys_differing_in_one_bit_are_distinct() {
        let mut t = AccountTable::new();
        let a = make_key(7);
        let mut b = a;
        b[31] ^= 0b0000_0001;
        let ia = t.intern(a);
        let ib = t.intern(b);
        assert_ne!(ia, ib);
        assert_eq!(t.len(), 2);
        assert_eq!(t.key_of(ia), Some(a));
        assert_eq!(t.key_of(ib), Some(b));
    }

    #[test]
    fn zero_key_is_a_valid_account() {
        // 全零 key 是合法输入，不能被当作「空值」特殊处理。
        let mut t = AccountTable::new();
        let idx = t.intern([0u8; 32]);
        assert_eq!(idx, 0);
        assert_eq!(t.lookup(&[0u8; 32]), Some(0));
        assert_eq!(t.key_of(0), Some([0u8; 32]));
    }

    #[test]
    fn intern_many_accounts_stays_contiguous() {
        let mut t = AccountTable::new();
        for n in 0..2000u32 {
            assert_eq!(t.intern(make_key(n)), n);
        }
        assert_eq!(t.len(), 2000);
        for n in 0..2000u32 {
            assert_eq!(t.lookup(&make_key(n)), Some(n));
            assert_eq!(t.key_of(n), Some(make_key(n)));
        }
    }

    // ------------------------------------------------------------------
    // PositiveList
    // ------------------------------------------------------------------

    #[test]
    fn positive_list_new_is_empty() {
        let l = PositiveList::new();
        assert_eq!(l.len(), 0);
        assert!(l.is_empty());
        assert_eq!(l.get(0), None);
        assert_eq!(l.last_check(0), None);
        assert!(!l.has(0));
        assert_eq!(l.iter().count(), 0);
        assert!(l.indices().is_empty());
    }

    #[test]
    fn set_positive_inserts_entry() {
        let mut l = PositiveList::new();
        l.set(5, 0.25, 1000);
        assert_eq!(l.len(), 1);
        assert!(l.has(5));
        assert_eq!(l.get(5), Some(0.25));
        assert_eq!(l.last_check(5), Some(1000));
    }

    #[test]
    fn set_boundary_scores() {
        let mut l = PositiveList::new();
        l.set(0, f32::MIN_POSITIVE, 1); // 最小正数仍是正分
        assert_eq!(l.get(0), Some(f32::MIN_POSITIVE));
        l.set(1, MAX_SCORE, 1);
        assert_eq!(l.get(1), Some(MAX_SCORE), "上界本身可以存");
        l.set(2, 1.0, 1);
        assert_eq!(l.get(2), Some(1.0), "1.0 只是一个普通数值，不再是上限");
    }

    #[test]
    fn set_zero_removes_entry() {
        let mut l = PositiveList::new();
        l.set(3, 0.9, 10);
        l.set(3, 0.0, 20);
        assert_eq!(l.len(), 0);
        assert!(!l.has(3));
        assert_eq!(l.get(3), None);
        assert_eq!(l.last_check(3), None, "删除后不应留下任何痕迹");
    }

    #[test]
    fn set_negative_removes_entry() {
        let mut l = PositiveList::new();
        l.set(3, 0.9, 10);
        l.set(3, -0.5, 20);
        assert!(!l.has(3));
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn set_negative_zero_removes_entry() {
        // -0.0 == 0.0，同样表示「没表态」
        let mut l = PositiveList::new();
        l.set(3, 0.9, 10);
        l.set(3, -0.0, 20);
        assert!(!l.has(3));
    }

    #[test]
    fn set_nan_removes_entry() {
        // NaN 与任何数比较都为 false，必须显式处理，
        // 否则会以「正分」的形式污染后续加权平均。
        let mut l = PositiveList::new();
        l.set(3, 0.9, 10);
        l.set(3, f32::NAN, 20);
        assert!(!l.has(3), "NaN 必须等价于删除");
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn set_nan_does_not_create_entry() {
        let mut l = PositiveList::new();
        l.set(9, f32::NAN, 1);
        assert_eq!(l.len(), 0);
        assert_eq!(l.get(9), None);
    }

    #[test]
    fn set_score_above_max_is_clamped() {
        let mut l = PositiveList::new();
        // 应用层给的是 0~n 的原始数值，2.5 必须原样保留，不能被截成 1.0
        l.set(1, 2.5, 0);
        assert_eq!(l.get(1), Some(2.5), "0~n 的原始数值不得被截断");
        l.set(2, f32::INFINITY, 0);
        assert_eq!(l.get(2), Some(MAX_SCORE), "无穷大收敛到上界");
        l.set(3, 1.0e10, 0);
        assert_eq!(
            l.get(3),
            Some(MAX_SCORE),
            "1e10 必须存成 MAX_SCORE（1e9），不是 1.0"
        );
    }

    #[test]
    fn set_updates_existing_entry_and_refreshes_time() {
        let mut l = PositiveList::new();
        l.set(7, 0.2, 100);
        l.set(7, 0.7, 250);
        assert_eq!(l.len(), 1, "更新不得产生第二条");
        assert_eq!(l.get(7), Some(0.7));
        assert_eq!(l.last_check(7), Some(250));
    }

    #[test]
    fn set_same_score_still_refreshes_time() {
        let mut l = PositiveList::new();
        l.set(7, 0.5, 100);
        l.set(7, 0.5, 999);
        assert_eq!(l.get(7), Some(0.5));
        assert_eq!(l.last_check(7), Some(999));
    }

    #[test]
    fn set_time_can_move_backwards() {
        // 时间由上层给出，存储层不做单调性假设，只如实记录。
        let mut l = PositiveList::new();
        l.set(7, 0.5, 100);
        l.set(7, 0.5, 50);
        assert_eq!(l.last_check(7), Some(50));
    }

    #[test]
    fn set_remove_nonexistent_is_noop() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 10);
        l.set(99, 0.0, 10); // 本来就不存在
        assert_eq!(l.len(), 1);
        assert!(!l.has(99));
    }

    #[test]
    fn touch_updates_time_only() {
        let mut l = PositiveList::new();
        l.set(4, 0.6, 100);
        l.touch(4, 172);
        assert_eq!(l.last_check(4), Some(172));
        assert_eq!(l.get(4), Some(0.6), "touch 不得修改分数");
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn touch_missing_entry_is_noop() {
        let mut l = PositiveList::new();
        l.touch(99, 5);
        assert_eq!(l.len(), 0, "touch 不得凭空创建条目");
        assert!(!l.has(99));
        assert_eq!(l.last_check(99), None);
    }

    #[test]
    fn touch_after_removal_is_noop() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 10);
        l.set(1, 0.0, 20);
        l.touch(1, 30);
        assert_eq!(l.len(), 0);
        assert_eq!(l.last_check(1), None);
    }

    #[test]
    fn len_tracks_insert_and_remove() {
        let mut l = PositiveList::new();
        l.set(0, 0.1, 0);
        l.set(1, 0.2, 0);
        l.set(2, 0.3, 0);
        assert_eq!(l.len(), 3);
        l.set(1, 0.0, 0);
        assert_eq!(l.len(), 2);
        l.set(2, -1.0, 0);
        l.set(0, 0.0, 0);
        assert_eq!(l.len(), 0);
        assert!(l.is_empty());
    }

    #[test]
    fn iter_yields_all_entries() {
        let mut l = PositiveList::new();
        l.set(10, 0.4, 1);
        l.set(2, 0.8, 2);
        l.set(7, 0.1, 3);
        let got: Vec<(AccountIdx, Score, Time)> = l.iter().collect();
        assert_eq!(got, vec![(2, 0.8, 2), (7, 0.1, 3), (10, 0.4, 1)]);
    }

    #[test]
    fn iter_is_deterministic_and_sorted_by_index() {
        // 遍历顺序不得依赖哈希随机化：用 BTreeMap 保证按编号升序。
        let mut l = PositiveList::new();
        for idx in [9u32, 1, 5, 3, 7] {
            l.set(idx, 0.5, idx as Time);
        }
        let first: Vec<_> = l.iter().map(|(i, _, _)| i).collect();
        let second: Vec<_> = l.iter().map(|(i, _, _)| i).collect();
        assert_eq!(first, vec![1, 3, 5, 7, 9]);
        assert_eq!(first, second);
    }

    #[test]
    fn iter_reflects_touch_and_set() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 10);
        l.touch(1, 42);
        let got: Vec<_> = l.iter().collect();
        assert_eq!(got, vec![(1, 0.5, 42)]);
    }

    #[test]
    fn indices_are_sorted_and_match_iter() {
        let mut l = PositiveList::new();
        l.set(8, 0.2, 0);
        l.set(3, 0.6, 0);
        assert_eq!(l.indices(), vec![3, 8]);
        let from_iter: Vec<_> = l.iter().map(|(i, _, _)| i).collect();
        assert_eq!(l.indices(), from_iter);
    }

    #[test]
    fn remove_then_readd_uses_new_time() {
        let mut l = PositiveList::new();
        l.set(5, 0.5, 100);
        l.set(5, 0.0, 200);
        assert!(!l.has(5));
        l.set(5, 0.9, 300);
        assert_eq!(l.get(5), Some(0.9));
        assert_eq!(l.last_check(5), Some(300));
        assert_eq!(l.len(), 1);
    }

    // ------------------------------------------------------------------
    // RoundSampler
    // ------------------------------------------------------------------

    #[test]
    fn sampler_new_is_empty() {
        let mut s = RoundSampler::new();
        assert_eq!(s.remaining(), 0);
        let mut rng = Rng::new(1);
        assert!(s.draw(5, &mut rng).is_empty(), "空清单抽不出东西");
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn reset_fills_round() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3, 4]);
        assert_eq!(s.remaining(), 4);
        s.reset(&[1, 2]);
        assert_eq!(s.remaining(), 2, "reset 会丢弃上一轮的进度");
    }

    #[test]
    fn draw_full_round_has_no_duplicates() {
        let mut s = RoundSampler::new();
        let members: Vec<AccountIdx> = (0..10).collect();
        s.reset(&members);
        let mut rng = Rng::new(0xDEAD_BEEF);
        let got = s.draw(10, &mut rng);
        assert_eq!(got.len(), 10);
        assert_eq!(
            set_of(&got),
            set_of(&members),
            "一轮应恰好覆盖全部成员且不重复"
        );
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn draw_more_than_remaining_returns_remaining() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3, 4, 5]);
        let mut rng = Rng::new(3);
        let first = s.draw(3, &mut rng);
        assert_eq!(first.len(), 3);
        assert_eq!(s.remaining(), 2);
        let rest = s.draw(100, &mut rng);
        assert_eq!(rest.len(), 2, "k 大于剩余时只返回剩余的");
        assert_eq!(s.remaining(), 0);
        // 两次抽取合起来正好是全部成员，互不重复
        let mut all = first;
        all.extend(rest);
        assert_eq!(set_of(&all), set_of(&[1, 2, 3, 4, 5]));
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn draw_zero_returns_empty_without_touching_state() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3]);
        let mut rng = Rng::new(1);
        assert!(s.draw(0, &mut rng).is_empty());
        assert_eq!(s.remaining(), 3, "k=0 不应消耗本轮名额");
        // 抽空之后 draw(0) 也不应触发重置
        let _ = s.draw(3, &mut rng);
        assert_eq!(s.remaining(), 0);
        assert!(s.draw(0, &mut rng).is_empty());
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn draw_auto_resets_after_exhaustion() {
        let mut s = RoundSampler::new();
        s.reset(&[7, 8, 9, 10]);
        let mut rng = Rng::new(11);
        let r1 = s.draw(4, &mut rng);
        assert_eq!(r1.len(), 4);
        assert_eq!(s.remaining(), 0);
        // 自动开始新一轮
        let r2 = s.draw(2, &mut rng);
        assert_eq!(r2.len(), 2, "抽完后应自动重置");
        assert_eq!(s.remaining(), 2);
        assert!(r2.iter().all(|i| [7, 8, 9, 10].contains(i)));
    }

    #[test]
    fn draw_never_repeats_within_a_round_across_calls() {
        let mut s = RoundSampler::new();
        let members: Vec<AccountIdx> = (0..9).collect();
        s.reset(&members);
        let mut rng = Rng::new(0x5EED);
        let mut seen = BTreeSet::new();
        while s.remaining() > 0 {
            for idx in s.draw(2, &mut rng) {
                assert!(seen.insert(idx), "同一轮内出现重复：{idx}");
            }
        }
        assert_eq!(seen, set_of(&members));
    }

    #[test]
    fn reset_starts_new_round() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3]);
        let mut rng = Rng::new(5);
        let _ = s.draw(3, &mut rng);
        assert_eq!(s.remaining(), 0);
        s.reset(&[1, 2, 3]);
        assert_eq!(s.remaining(), 3, "reset 强制开新一轮");
        let again = s.draw(3, &mut rng);
        assert_eq!(set_of(&again), set_of(&[1, 2, 3]));
    }

    #[test]
    fn sync_prunes_removed_accounts() {
        let mut s = RoundSampler::new();
        s.reset(&[0, 1, 2, 3, 4, 5, 6, 7]);
        let mut rng = Rng::new(17);
        let drawn: BTreeSet<AccountIdx> = s.draw(3, &mut rng).into_iter().collect();
        assert_eq!(s.remaining(), 5);

        let all: BTreeSet<AccountIdx> = (0..8).collect();
        let pending_before: BTreeSet<AccountIdx> = all.difference(&drawn).copied().collect();

        // 清单里删掉 6、7
        s.sync(&[0, 1, 2, 3, 4, 5]);
        let expected: BTreeSet<AccountIdx> =
            pending_before.iter().copied().filter(|i| *i < 6).collect();
        assert_eq!(
            s.remaining(),
            expected.len(),
            "被删除的账户必须从待抽集合剔除"
        );

        // 抽干本轮，验证待抽集合恰好是期望集合
        let got: BTreeSet<AccountIdx> = s.draw(100, &mut rng).into_iter().collect();
        assert_eq!(got, expected);
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn sync_keeps_undrawn_accounts() {
        let mut s = RoundSampler::new();
        s.reset(&[10, 11, 12, 13]);
        let mut rng = Rng::new(23);
        let drawn: BTreeSet<AccountIdx> = s.draw(1, &mut rng).into_iter().collect();
        s.sync(&[10, 11, 12, 13]); // 清单没变
        assert_eq!(s.remaining(), 3, "sync 不得回退本轮进度");
        let rest: BTreeSet<AccountIdx> = s.draw(100, &mut rng).into_iter().collect();
        assert_eq!(rest.len(), 3);
        assert!(drawn.is_disjoint(&rest), "已经抽中的不应重新出现");
    }

    #[test]
    fn sync_does_not_reintroduce_drawn_accounts() {
        let mut s = RoundSampler::new();
        s.reset(&[0, 1, 2]);
        let mut rng = Rng::new(31);
        let drawn = s.draw(1, &mut rng)[0];
        for _ in 0..5 {
            s.sync(&[0, 1, 2]);
        }
        assert_eq!(s.remaining(), 2);
        let rest = s.draw(10, &mut rng);
        assert_eq!(rest.len(), 2);
        assert!(!rest.contains(&drawn), "sync 不得把已抽中的放回本轮");
    }

    #[test]
    fn sync_does_not_add_new_accounts_mid_round() {
        let mut s = RoundSampler::new();
        s.reset(&[0, 1]);
        let mut rng = Rng::new(37);
        let _ = s.draw(1, &mut rng);
        s.sync(&[0, 1, 2, 3]);
        assert_eq!(s.remaining(), 1, "本轮进行中不补充新成员");

        let rest = s.draw(10, &mut rng); // 抽完本轮
        assert_eq!(rest.len(), 1);

        let next_round = s.draw(10, &mut rng); // 自动重置，此时才包含新成员
        assert_eq!(next_round.len(), 4);
        assert_eq!(set_of(&next_round), set_of(&[0, 1, 2, 3]));
    }

    #[test]
    fn sync_on_fresh_sampler_then_draw_works() {
        let mut s = RoundSampler::new();
        s.sync(&[4, 5, 6]);
        assert_eq!(s.remaining(), 0, "sync 只剔除，不主动开轮");
        let mut rng = Rng::new(41);
        let out = s.draw(2, &mut rng);
        assert_eq!(out.len(), 2, "draw 会自动用 sync 记住的全量清单开新一轮");
        assert_eq!(s.remaining(), 1);
    }

    #[test]
    fn sync_is_idempotent() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3, 4]);
        let mut rng = Rng::new(43);
        let _ = s.draw(2, &mut rng);
        let before = s.remaining();
        s.sync(&[1, 2, 3, 4]);
        s.sync(&[1, 2, 3, 4]);
        assert_eq!(s.remaining(), before);
    }

    #[test]
    fn reset_dedups_inputs() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 1, 2, 2, 2, 3]);
        assert_eq!(s.remaining(), 3, "重复编号必须去重");
        let mut rng = Rng::new(47);
        let got = s.draw(10, &mut rng);
        assert_eq!(got.len(), 3);
        assert_eq!(set_of(&got), set_of(&[1, 2, 3]));
    }

    #[test]
    fn sync_dedups_inputs() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2]);
        s.sync(&[1, 1, 1, 2, 2]);
        let mut rng = Rng::new(53);
        let got = s.draw(2, &mut rng);
        assert_eq!(set_of(&got), set_of(&[1, 2]));
        assert_eq!(got.len(), 2);
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn reset_to_empty_then_draw_is_empty() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3]);
        s.reset(&[]);
        assert_eq!(s.remaining(), 0);
        let mut rng = Rng::new(59);
        assert!(s.draw(3, &mut rng).is_empty());
        assert_eq!(s.remaining(), 0);
    }

    #[test]
    fn removed_everything_sync_leaves_empty_round() {
        let mut s = RoundSampler::new();
        s.reset(&[1, 2, 3]);
        s.sync(&[]);
        assert_eq!(s.remaining(), 0);
        let mut rng = Rng::new(61);
        assert!(s.draw(2, &mut rng).is_empty(), "全量清单为空时抽不出东西");
    }

    #[test]
    fn draw_only_returns_members() {
        let mut s = RoundSampler::new();
        s.reset(&[100, 200, 300]);
        let mut rng = Rng::new(67);
        for _ in 0..50 {
            for idx in s.draw(2, &mut rng) {
                assert!([100, 200, 300].contains(&idx));
            }
        }
    }

    #[test]
    fn single_member_round() {
        let mut s = RoundSampler::new();
        s.reset(&[42]);
        let mut rng = Rng::new(71);
        assert_eq!(s.draw(1, &mut rng), vec![42]);
        assert_eq!(s.remaining(), 0);
        // 自动重置后再抽一次，还是它
        assert_eq!(s.draw(1, &mut rng), vec![42]);
    }

    #[test]
    fn draw_is_uniform_over_100k_samples() {
        // 均匀性检验：7 个成员，抽 10 万次 k=1。
        // 7 是质数，10 万不是 7 的整数倍，因此计数本身会自然浮动，
        // 这是一个真实的统计检验，而不是「每轮恰好各一次」的平凡结果。
        const N: usize = 7;
        const SAMPLES: usize = 100_000;
        let members: Vec<AccountIdx> = (0..N as AccountIdx).collect();
        let mut s = RoundSampler::new();
        s.reset(&members);

        let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
        let mut counts = [0usize; N];
        for _ in 0..SAMPLES {
            let out = s.draw(1, &mut rng);
            assert_eq!(out.len(), 1);
            let idx = out[0] as usize;
            assert!(idx < N);
            counts[idx] += 1;
        }

        let expected = SAMPLES as f64 / N as f64;
        let band = expected * 0.05; // 允许 5% 的相对偏差
        for (i, &c) in counts.iter().enumerate() {
            let dev = (c as f64 - expected).abs();
            assert!(
                dev < band,
                "元素 {i} 出现 {c} 次，期望 {expected:.1}，偏差 {dev:.1} 超过 5%（上限 {band:.1}）"
            );
        }
        assert_eq!(counts.iter().sum::<usize>(), SAMPLES);
    }

    #[test]
    fn draw_batches_are_uniform() {
        // 批量抽取同样均匀：8 个成员，每次抽 3 个，抽 2 万次 => 6 万个样本。
        const N: usize = 8;
        const CALLS: usize = 20_000;
        const PER_CALL: usize = 3;
        let members: Vec<AccountIdx> = (0..N as AccountIdx).collect();
        let mut s = RoundSampler::new();
        s.reset(&members);

        let mut rng = Rng::new(0x0BAD_C0DE_0BAD_C0DE);
        let mut counts = [0usize; N];
        for _ in 0..CALLS {
            let out = s.draw(PER_CALL, &mut rng);
            assert!(out.len() <= PER_CALL);
            for idx in out {
                counts[idx as usize] += 1;
            }
        }

        let total: usize = counts.iter().sum();
        let expected = total as f64 / N as f64;
        let band = expected * 0.05;
        for (i, &c) in counts.iter().enumerate() {
            let dev = (c as f64 - expected).abs();
            assert!(
                dev < band,
                "元素 {i} 出现 {c} 次，期望 {expected:.1}，偏差 {dev:.1} 超过 5%（上限 {band:.1}）"
            );
        }
    }

    // ------------------------------------------------------------------
    // Rng
    // ------------------------------------------------------------------

    #[test]
    fn rng_same_seed_same_sequence() {
        let mut a = Rng::new(12345);
        let mut b = Rng::new(12345);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_different_seeds_differ() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let sa: Vec<u64> = (0..10).map(|_| a.next_u64()).collect();
        let sb: Vec<u64> = (0..10).map(|_| b.next_u64()).collect();
        assert_ne!(sa, sb);
    }

    #[test]
    fn rng_matches_manual_xorshift() {
        // 逐步核对 xorshift64(13, 7, 17) 的递推式，防止实现被悄悄改掉。
        let mut rng = Rng::new(1);
        let mut x: u64 = 1;
        for _ in 0..64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            assert_eq!(rng.next_u64(), x);
        }
    }

    #[test]
    fn rng_seed_zero_is_remapped() {
        // 0 是 xorshift 的不动点；必须换成内置常量种子，否则永远返回 0。
        let mut rng = Rng::new(0);
        let first = rng.next_u64();
        assert_ne!(first, 0);
        let mut seen = BTreeSet::new();
        for _ in 0..100 {
            assert!(seen.insert(rng.next_u64()));
        }
    }

    #[test]
    fn rng_never_returns_zero() {
        // 非零状态在 xorshift 下永远走不到 0
        let mut rng = Rng::new(0xFFFF_FFFF_FFFF_FFFF);
        for _ in 0..10_000 {
            assert_ne!(rng.next_u64(), 0);
        }
    }

    #[test]
    fn rng_output_bit_diversity() {
        // 粗略检查高位/低位都有变化（防止移位方向写反导致某段恒定）。
        let mut rng = Rng::new(777);
        let mut low_or = 0u64;
        let mut high_or = 0u64;
        for _ in 0..1000 {
            let v = rng.next_u64();
            low_or |= v & 0xFF;
            high_or |= v >> 56;
        }
        assert_eq!(low_or, 0xFF, "低 8 位必须充分变化");
        assert_eq!(high_or, 0xFF, "高 8 位必须充分变化");
    }

    #[test]
    fn rng_next_below_in_range() {
        let mut rng = Rng::new(99);
        for n in 1..=64usize {
            for _ in 0..500 {
                assert!(rng.next_below(n) < n, "next_below({n}) 越界");
            }
        }
    }

    #[test]
    fn rng_next_below_one_is_zero() {
        let mut rng = Rng::new(5);
        for _ in 0..100 {
            assert_eq!(rng.next_below(1), 0);
        }
    }

    #[test]
    fn rng_next_below_large_modulus() {
        let mut rng = Rng::new(101);
        let n = (u32::MAX as usize) + 1;
        for _ in 0..1000 {
            assert!(rng.next_below(n) < n);
        }
    }

    #[test]
    #[should_panic(expected = "next_below")]
    fn rng_next_below_zero_panics() {
        let mut rng = Rng::new(1);
        let _ = rng.next_below(0);
    }

    #[test]
    fn rng_next_below_is_uniform() {
        const N: usize = 7;
        const SAMPLES: usize = 70_000;
        let mut rng = Rng::new(0xABCD_1234_5678_9EF0);
        let mut counts = [0usize; N];
        for _ in 0..SAMPLES {
            counts[rng.next_below(N)] += 1;
        }
        let expected = SAMPLES as f64 / N as f64;
        let band = expected * 0.05;
        for (i, &c) in counts.iter().enumerate() {
            let dev = (c as f64 - expected).abs();
            assert!(
                dev < band,
                "next_below 桶 {i} 出现 {c} 次，期望 {expected:.1}，偏差超过 5%"
            );
        }
    }

    #[test]
    fn rng_next_below_power_of_two_is_uniform() {
        // n 为 2 的幂时 zone == 0，不应进入拒绝分支；分布必须仍是均匀的。
        let mut rng = Rng::new(0xFEED_FACE_CAFE_BEEF);
        let mut counts = [0usize; 8];
        for _ in 0..80_000 {
            counts[rng.next_below(8)] += 1;
        }
        let expected = 10_000.0;
        for (i, &c) in counts.iter().enumerate() {
            assert!(
                (c as f64 - expected).abs() < expected * 0.05,
                "桶 {i} 偏差过大：{c}"
            );
        }
    }

    // ------------------------------------------------------------------
    // E1：连败计数与退避抽样的内部实现
    // ------------------------------------------------------------------

    #[test]
    fn pow2_neg_is_exact_for_every_supported_exponent() {
        // 位模式拼出来的 2^-k 必须与逐个乘 0.5 完全相等（不能有舍入）。
        let mut expected = 1.0f64;
        for k in 0..=BACKOFF_MAX_EXPONENT {
            assert_eq!(pow2_neg(k), expected, "2^-{k} 不是精确值");
            expected *= 0.5;
        }
    }

    #[test]
    fn backoff_weight_is_relative_and_never_zero() {
        // 权重只由「与最小连败的差」决定，公共偏移不影响结果。
        assert_eq!(backoff_weight(7, 7), 1.0);
        assert_eq!(backoff_weight(8, 7), 0.5);
        assert_eq!(backoff_weight(9, 7), 0.25);
        assert_eq!(
            backoff_weight(8, 7),
            backoff_weight(108, 107),
            "只与差值有关"
        );
        // 差值为负（正常不会出现）按 0 处理。
        assert_eq!(backoff_weight(3, 7), 1.0);
        // 极端差值被截断，但**永远大于 0**：这正是「不会少抽」的前提。
        let floor = pow2_neg(BACKOFF_MAX_EXPONENT);
        assert_eq!(backoff_weight(u32::MAX, 0), floor);
        assert!(floor > 0.0);
    }

    #[test]
    fn next_unit_is_in_range_and_well_spread() {
        let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
        let mut buckets = [0usize; 10];
        for _ in 0..100_000 {
            let v = next_unit(&mut rng);
            assert!((0.0..1.0).contains(&v), "越界：{v}");
            buckets[(v * 10.0) as usize] += 1;
        }
        // 10 个桶各 1 万，允许 5% 偏差（约 5.3 个标准差）。
        for (i, &c) in buckets.iter().enumerate() {
            assert!((c as f64 - 10_000.0).abs() < 500.0, "桶 {i} 偏了：{c}");
        }
    }

    #[test]
    fn pick_weighted_uses_uniform_path_when_weights_are_equal() {
        // 连败全相同 -> 必须与 draw 一样消费一次 next_below(len)。
        let mut a = Rng::new(0x5EED);
        let mut b = Rng::new(0x5EED);
        for len in 1..=16usize {
            let streaks = vec![3u32; len];
            assert_eq!(pick_weighted(&streaks, &mut a), b.next_below(len));
        }
    }

    #[test]
    fn record_failure_saturates_instead_of_wrapping() {
        let mut list = PositiveList::new();
        list.set(1, 0.5, 0);
        list.entries.get_mut(&1).unwrap().fail_streak = u32::MAX;
        assert_eq!(list.record_failure(1), u32::MAX, "饱和，不得回绕到 0");
        assert_eq!(list.fail_streak(1), u32::MAX);
    }

    #[test]
    fn set_preserves_fail_streak_of_existing_entry() {
        let mut list = PositiveList::new();
        list.set(2, 0.4, 10);
        assert_eq!(list.record_failure(2), 1);
        list.set(2, 0.9, 20);
        assert_eq!(list.fail_streak(2), 1, "改分不是成功的证据");
        assert_eq!(list.last_check(2), Some(20), "set 仍会刷新时间");
        // 删掉再加回来才是一条新关系，连败清零。
        list.set(2, 0.0, 30);
        list.set(2, 0.9, 40);
        assert_eq!(list.fail_streak(2), 0);
    }

    // ------------------------------------------------------------------
    // P4：持久化入口（items / from_items / state / from_state /
    //      pending / members / from_parts / keys / from_keys）
    // ------------------------------------------------------------------

    /// items() 必须一次给全四件套，且顺序与 iter() 一致。
    #[test]
    fn items_carry_score_time_and_fail_streak() {
        let mut list = PositiveList::new();
        list.set(7, 0.25, 100);
        list.set(3, 0.75, 300);
        list.record_failure(3);
        list.record_failure(3);
        let items = list.items();
        assert_eq!(items, vec![(3, 0.75, 300, 2), (7, 0.25, 100, 0)]);
        // 与 iter() 同一份数据（三件套部分逐项相同）。
        let from_iter: Vec<(AccountIdx, Score, Time)> = list.iter().collect();
        let from_items: Vec<(AccountIdx, Score, Time)> =
            items.iter().map(|&(i, s, t, _)| (i, s, t)).collect();
        assert_eq!(from_iter, from_items);
        assert_eq!(
            list.indices(),
            items.iter().map(|i| i.0).collect::<Vec<_>>()
        );
    }

    /// from_items 是 items 的精确逆操作：三件套 + 连败计数全部原样回来。
    #[test]
    fn items_roundtrip_through_from_items_is_lossless() {
        let mut list = PositiveList::new();
        list.set(1, 0.1, 10);
        list.set(2, 1.0e9, 20);
        list.set(5, f32::MIN_POSITIVE, 30);
        list.record_failure(1);
        list.record_failure(1);
        list.record_failure(5);
        let items = list.items();
        let restored = PositiveList::from_items(&items);
        assert_eq!(restored.items(), items, "恢复后必须逐项相同");
        assert_eq!(restored.len(), list.len());
        assert_eq!(restored.total(), list.total());
        assert_eq!(restored.shares(), list.shares());
        assert_eq!(restored.fail_streak(1), 2);
        assert_eq!(restored.last_check(5), Some(30), "时间不得被刷新成 0");
        assert_eq!(restored.fail_streak(5), 1);
    }

    /// 空清单 round-trip 仍是空清单（恢复分支不能凭空造出条目）。
    #[test]
    fn empty_list_roundtrips() {
        let list = PositiveList::new();
        let restored = PositiveList::from_items(&list.items());
        assert!(restored.is_empty());
        assert_eq!(restored.len(), 0);
        assert_eq!(restored.total(), 0.0);
    }

    /// from_items 必须与 set 共用同一套存储规则：非正/NaN 丢弃、超上限截断。
    #[test]
    fn from_items_applies_same_storage_rules_as_set() {
        let restored = PositiveList::from_items(&[
            (1, 0.5, 10, 0),
            (2, 0.0, 10, 0),           // 0 = 没表态 -> 丢弃
            (3, -1.0, 10, 7),          // 负数 -> 丢弃
            (4, f32::NAN, 10, 0),      // NaN -> 丢弃
            (5, f32::INFINITY, 10, 0), // 无穷 -> 截到 MAX_SCORE
            (6, 1.0e10, 10, 0),        // 超大 -> 截到 MAX_SCORE
            (7, 2.5, 10, 0),           // 0~n 的原始数值原样保留
        ]);
        assert_eq!(restored.len(), 4, "三条非法条目必须被丢弃");
        assert!(!restored.has(2) && !restored.has(3) && !restored.has(4));
        assert_eq!(restored.get(5), Some(MAX_SCORE));
        assert_eq!(restored.get(6), Some(MAX_SCORE));
        assert_eq!(restored.get(7), Some(2.5));
        assert_eq!(restored.get(1), Some(0.5));
    }

    /// 同一编号重复出现时后者覆盖前者（与 BTreeMap::insert 的语义一致）。
    #[test]
    fn from_items_last_wins_for_duplicate_index() {
        let restored = PositiveList::from_items(&[(9, 0.2, 10, 0), (9, 0.8, 40, 3)]);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.get(9), Some(0.8));
        assert_eq!(restored.last_check(9), Some(40));
        assert_eq!(restored.fail_streak(9), 3);
    }

    /// Rng::state/from_state：中途取出状态再装回去，后续序列必须逐位相同。
    #[test]
    fn rng_state_roundtrip_is_bit_identical_midstream() {
        let mut a = Rng::new(0xDEAD_BEEF_1234_5678);
        for _ in 0..7 {
            let _ = a.next_u64();
        }
        let mut b = Rng::from_state(a.state());
        for step in 0..100 {
            let (x, y) = (a.next_u64(), b.next_u64());
            assert_eq!(x, y, "第 {step} 步分叉");
        }
        // next_below 走同一条流（含拒绝采样），也必须一致。
        let mut c = Rng::from_state(a.state());
        for _ in 0..100 {
            assert_eq!(a.next_below(7), c.next_below(7));
        }
    }

    /// from_state 与 new 在同一条递推上等价（种子非 0 时）。
    #[test]
    fn rng_from_state_matches_new_for_nonzero_seed() {
        let fresh = Rng::new(2024);
        let mut a = Rng::from_state(fresh.state());
        let mut b = Rng::new(2024);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // 种子 0 经 new 被换成常量，from_state 拿到的正是那个常量。
        assert_eq!(
            Rng::new(0).state(),
            Rng::from_state(Rng::new(0).state()).state()
        );
    }

    /// from_state(0) 不 panic、也不会让拒绝采样死循环：0 换成内置常量。
    #[test]
    fn rng_from_state_zero_is_canonicalized() {
        let mut rng = Rng::from_state(0);
        assert_ne!(rng.state(), 0, "状态 0 是不动点，必须换成内置常量");
        assert_eq!(rng.state(), Rng::new(0).state());
        for _ in 0..10 {
            assert_ne!(rng.next_u64(), 0);
        }
        // 能正常抽下标（若状态是 0，这里会死循环，测试会直接挂住）。
        for n in [1usize, 3, 9, 64] {
            for _ in 0..100 {
                assert!(rng.next_below(n) < n);
            }
        }
    }

    /// pending()/members()/from_parts：重建后的抽样器必须抽出同一批人、同一个顺序。
    #[test]
    fn sampler_parts_roundtrip_replays_identical_sequence() {
        let members: Vec<AccountIdx> = (0..12).collect();
        let mut s = RoundSampler::new();
        s.reset(&members);
        let mut rng = Rng::new(0xABCD_EF01);
        let _ = s.draw(3, &mut rng);

        let mut twin = RoundSampler::from_parts(s.members(), s.pending());
        let mut rng2 = Rng::from_state(rng.state());
        for round in 0..4 {
            let a = s.draw(4, &mut rng);
            let b = twin.draw(4, &mut rng2);
            assert_eq!(a, b, "第 {round} 批抽取顺序分叉");
            assert_eq!(s.pending(), twin.pending(), "pending 必须逐项相同");
            assert_eq!(s.remaining(), twin.remaining());
        }
    }

    /// pending 永远是 members 的子集，且两者都不含重复编号。
    #[test]
    fn pending_is_a_deduped_subset_of_members() {
        let mut s = RoundSampler::new();
        s.sync(&[5, 3, 5, 9, 7]);
        assert_eq!(s.members(), &[3, 5, 7, 9], "members 恒为升序去重");
        assert_eq!(s.remaining(), 0, "sync 不主动开轮");
        let mut rng = Rng::new(1);
        let _ = s.draw(2, &mut rng);
        let pending = s.pending();
        assert_eq!(pending.len(), 2);
        let uniq: BTreeSet<AccountIdx> = pending.iter().copied().collect();
        assert_eq!(uniq.len(), pending.len(), "pending 不得有重复");
        assert!(uniq.iter().all(|i| s.members().contains(i)));
    }

    /// AccountTable::keys/from_keys：往返后编号布局、长度、后续 intern 全部一致。
    #[test]
    fn account_table_keys_roundtrip_and_continue_numbering() {
        let mut t = AccountTable::new();
        let keys: Vec<AccountKey> = (0..8u32).map(make_key).collect();
        for k in &keys {
            t.intern(*k);
        }
        let rebuilt = AccountTable::from_keys(&t.keys()).expect("合法快照");
        assert_eq!(rebuilt.len(), t.len());
        for i in 0..8u32 {
            assert_eq!(rebuilt.key_of(i), t.key_of(i));
            assert_eq!(rebuilt.lookup(&make_key(i)), Some(i));
        }
        // 继续 intern 新 key：接着分配下一个连续编号（与从未重启一样）。
        let mut a = t;
        let mut b = rebuilt;
        assert_eq!(a.intern(make_key(99)), 8);
        assert_eq!(b.intern(make_key(99)), 8);
        assert_eq!(a.intern(make_key(7)), 7, "已存在的 key 仍幂等");
        assert_eq!(b.intern(make_key(7)), 7);
        assert_eq!(b.len(), 9);
    }

    /// 空表 round-trip。
    #[test]
    fn empty_account_table_roundtrips() {
        let t = AccountTable::new();
        let rebuilt = AccountTable::from_keys(&t.keys()).expect("空表也是合法快照");
        assert!(rebuilt.is_empty());
        assert_eq!(rebuilt.key_of(0), None);
    }

    /// fit from_keys 必须拒绝重复公钥（编号布局会与原件不同）。
    #[test]
    fn from_keys_rejects_duplicate_keys() {
        let dup: Vec<AccountKey> = vec![make_key(1), make_key(2), make_key(1)];
        assert!(AccountTable::from_keys(&dup).is_none());
        let ok: Vec<AccountKey> = vec![make_key(1), make_key(2), make_key(3)];
        assert!(AccountTable::from_keys(&ok).is_some());
    }
}
