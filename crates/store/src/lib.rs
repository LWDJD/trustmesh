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
//! * [RoundSampler]：轮次抽样器。从正分清单中等概率、**不放回**地抽取，
//!   一轮内不重复，抽完自动开启新一轮。
//!
//! 另外提供 [Rng]：一个基于 xorshift64 的确定性伪随机数发生器，
//! 满足「不引入任何外部 crate」的工程约束。
//!
//! ## 设计约束
//!
//! 1. 评分只有 (0, 1] 的正数，0 表示「没表态」；没有负分。
//! 2. 抽样只从「正分清单」里等概率抽取，**不按评分加权**。
//! 3. 抽样失败是正常路径，存储层不参与重试、不阻塞、也不更新时间戳
//!    （是否刷新由上层决定，存储层只提供 [PositiveList::touch]）。
//! 4. 编号只增不减、永不回收——同一个 key 多次 intern 必然返回同一编号。
//! 5. 全部实现只依赖 std。
//!
//! ## 快速上手
//!
//! ~~~rust
//! use tm_store::{AccountTable, PositiveList, RoundSampler, Rng};
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
//! ~~~

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 账户公钥：定长 32 字节（例如 Ed25519 公钥）。
pub type AccountKey = [u8; 32];

/// 账户编号：由 [AccountTable] 分配的紧凑整数，从 0 开始连续递增。
pub type AccountIdx = u32;

/// 评分：取值范围 (0, 1]，0 表示「没表态」。没有负分。
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
/// use tm_store::AccountTable;
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
}

/// 本地维护的「我给谁打过正分」清单。
///
/// 语义约定：
///
/// * 只保存 score > 0 的条目。set 传入 <= 0（含负数、-0.0、NaN）
///   等价于删除该条目，不会留下 score == 0 的占位项。
/// * 传入的分数若大于 [MAX_SCORE]（含 +∞）会被截断为 1.0，
///   以此在存储层边界上守住「评分 ∈ (0, 1]」这个不变量。
/// * set 无论新增还是修改，都会把 last_check 刷新为传入的 now。
/// * [PositiveList::touch] 只刷新时间、不改分数，用于「成功取到观点」；
///   「抽样失败」时上层不应调用它（失败不更新时间戳）。
/// * 遍历顺序按编号升序，结果不受哈希随机化影响，便于复现与测试。
///
/// 用 BTreeMap 而不是 HashMap 是为了给 [PositiveList::iter] 一个
/// **确定性**的顺序；条目规模是「我打过正分的人数」，量级有限，
/// O(log n) 足够。
///
/// # 例子
///
/// ~~~
/// use tm_store::PositiveList;
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
    /// score > 1.0 会被截断为 [MAX_SCORE]。
    pub fn set(&mut self, idx: AccountIdx, score: Score, now: Time) {
        // 注意：NaN 与任何数比较都是 false，所以 score <= 0.0 对 NaN 不成立，
        // 这里必须显式判 NaN，否则 NaN 会被当成「正分」存进去，污染后续加权。
        if score.is_nan() || score <= 0.0 {
            self.entries.remove(&idx);
            return;
        }
        // 允许任意正数（应用层给的 0~n 原始数值），只挡住无穷大/超大值。
        let score = if score.is_finite() { score.min(MAX_SCORE) } else { MAX_SCORE };
        self.entries.insert(
            idx,
            Entry {
                score,
                last_check: now,
            },
        );
    }

    /// 取原始评分（不做衰减）；未表态返回 None。
    pub fn get(&self, idx: AccountIdx) -> Option<Score> {
        self.entries.get(&idx).map(|e| e.score)
    }

    /// 全部评分的总和（影响力预算的分母）。
    pub fn total(&self) -> Score {
        self.entries.values().map(|e| e.score).sum()
    }

    /// 归一化后的份额：该账户占我总配额的比值，所有份额之和恒为 1。
    ///
    /// 这是「切蛋糕」的最终结果：应用层给 0~n 的原始数值，
    /// 这里把它换算成占比。没表过态返回 None。
    pub fn share(&self, idx: AccountIdx) -> Option<Score> {
        let s = self.entries.get(&idx)?.score;
        let total = self.total();
        if total > 0.0 {
            Some(s / total)
        } else {
            None
        }
    }

    /// 完整份额表，总和恒为 1（无条目时为空）。按编号升序。
    pub fn shares(&self) -> Vec<(AccountIdx, Score)> {
        let total = self.total();
        if total <= 0.0 {
            return Vec::new();
        }
        self.entries
            .iter()
            .map(|(k, e)| (*k, e.score / total))
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
///
/// # 实现
///
/// 待抽集合是一个数组；每次抽一个随机下标 j，把 pending[j] 与末尾元素交换
/// 后 pop 掉（即 swap_remove），因此单次抽取是 O(1)，
/// 且不依赖任何重试/拒绝式的「抽到重复就重抽」。
///
/// # 例子
///
/// ~~~
/// use tm_store::{RoundSampler, Rng};
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
        if self.pending.is_empty() {
            if self.members.is_empty() {
                return Vec::new();
            }
            // 本轮已抽空：自动重置为全量，再抽。
            self.start_new_round();
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

    /// 本轮还剩多少个没抽。
    ///
    /// 返回 0 表示本轮已抽完；下一次 draw(k > 0) 会自动开启新一轮。
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }

    /// 强制开始新一轮：用 current 作为全量清单，清空本轮进度。
    ///
    /// 与 [RoundSampler::sync] 的区别：reset 会把已经抽中的元素**放回**待抽集合，
    /// 而 sync 只做剔除。输入里的重复编号会被去重。
    pub fn reset(&mut self, current: &[AccountIdx]) {
        self.members = normalize(current);
        self.start_new_round();
    }

    /// 把待抽集合恢复成全部成员。
    fn start_new_round(&mut self) {
        self.pending.clear();
        self.pending.extend_from_slice(&self.members);
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
// 4. Rng —— xorshift64
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
/// use tm_store::Rng;
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
}
