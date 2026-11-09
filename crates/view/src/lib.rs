//! # tm_view —— Count Sketch 压缩观点
//!
//! 去中心化信任网络里，每个账户都有一份「我给别人的评分」。把整份评分表原样
//! 发布，体积随网络人数线性增长，不可接受。本 crate 把评分表压成**定长**草图
//! （Count Sketch）：
//!
//! - 体积只由参数决定：`buckets × hashers × 4` 字节，与账户数无关；
//! - 支持对任意 [`AccountKey`]（32 字节公钥）查询近似评分；
//! - 合并是逐元素加权线性相加，天然支持「我的视角 = Σ 别人的观点 × 权重」；
//! - 自带噪声底实测：[`CompressedView::noise_floor`] 用全新随机 key 查询，
//!   给出中位数 / p95 / 最大值。
//!
//! ## 结构
//!
//! 草图是 `hashers × buckets` 的 f32 矩阵。写入 `(key, score)` 时，第 j 行用
//! 第 j 个独立哈希把 key 映到某个桶，并按另一条独立哈希给出的 ±1 符号累加
//! `±score`。查询时把 j 行的桶值乘回符号，取 `hashers` 个估值的**中位数**——
//! 少数桶发生碰撞只会污染个别行，中位数把它们投出去。
//!
//! ## 误差从哪来
//!
//! 只有碰撞：别的 key 落进同一个桶，值相加后无法分离。这是有意的信息损失，
//! 换来定长与线性合并。碰撞噪声随写入量增长，可用
//! [`CompressedView::noise_floor`] 实测；在 2048×5 的默认参数下，写入量远小于
//! 桶数时大多数查询是精确的，噪声集中体现在 p95 / max 上（见 crate 的测试与
//! `examples/noise_report.rs`）。
//!
//! ## 编码
//!
//! [`CompressedView::encode`] 输出恰好 `bytes()` 字节：按行优先顺序把每个 f32
//! 写成小端 4 字节。参数（buckets/hashers/seed）不写进负载，由通信双方在带外
//! 约定；[`CompressedView::decode`] 用调用者给的参数校验长度，并拒绝 NaN/∞。
//!
//! ## 例子
//!
//! ```
//! use tm_view::{AccountKey, CompressedView, ViewParams};
//!
//! let mut view = CompressedView::new(ViewParams::default());
//! let alice: AccountKey = [1u8; 32];
//! view.add(&alice, 0.8);
//! assert!((view.query(&alice) - 0.8).abs() < 0.02);
//!
//! // 另一个节点对 alice 的评价是 0.2，我按 3:1 合成：
//! // 0.75 * 0.8 + 0.25 * 0.2 = 0.65。
//! let mut other = CompressedView::new(ViewParams::default());
//! other.add(&alice, 0.2);
//! view.merge(&other, 0.75, 0.25);
//! assert!((view.query(&alice) - 0.65).abs() < 0.02);
//!
//! // 编码后可以原样发到网上，解码需要同样的参数。
//! let bytes = view.encode();
//! assert_eq!(bytes.len(), view.bytes());
//! let decoded = CompressedView::decode(&bytes, ViewParams::default()).expect("长度正确");
//! assert_eq!(decoded.query(&alice), view.query(&alice));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod hash;
mod rng;

pub use rng::XorShift64;

use hash::{derive_seed, fmix64, fnv1a64_seeded, SIGN_SALT};
use std::fmt;

/// 账户公钥：32 字节定长标识。
pub type AccountKey = [u8; 32];

/// 评分：0 表示「没表态」，正数表示信任程度。
///
/// 协议层约定取值范围是 `[0, 1]`（0 即没表态）；本 crate 是底层原语，
/// 不替调用方做截断，但会忽略 NaN/∞，避免一条坏数据污染所有桶。
pub type Score = f32;

/// Count Sketch 的形状参数。
///
/// 三个字段都是公开的，可以直接用结构体字面量构造。参数是**全网约定**：
/// 只有 `buckets`、`hashers`、`seed` 完全一致的两个视图才能互相查询和合并。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewParams {
    /// 每个哈希函数的桶数（k）。越大越精确，体积线性增长。
    pub buckets: usize,
    /// 哈希函数个数（d），也是查询取中位数的样本数。通常取奇数。
    pub hashers: usize,
    /// 主种子。第 j 个 hasher 的种子由它派生；全网必须一致。
    pub seed: u64,
}

impl ViewParams {
    /// 允许的最大桶单元数：2^26 个 f32 = 256 MiB。
    ///
    /// 参数是公开输入，必须防止 `buckets × hashers` 溢出，
    /// 或因为一个手滑的配置申请巨量内存。
    pub const MAX_CELLS: usize = 1 << 26;

    /// 允许的最大哈希函数个数。
    ///
    /// 每次查询要按 hashers 个估值取中位数，会分配同样长度的临时向量；
    /// 不限的话，`buckets = 1, hashers = 2^26` 这样的参数会让每次查询
    /// 额外分配 256 MiB。4096 已远超任何合理取值（默认 5）。
    pub const MAX_HASHERS: usize = 4096;

    /// 直接构造参数，不做校验；校验发生在 [`CompressedView::new`]。
    pub const fn new(buckets: usize, hashers: usize, seed: u64) -> Self {
        Self {
            buckets,
            hashers,
            seed,
        }
    }

    /// 参数是否可用：两个维度都为正，且总单元数不超过 [`Self::MAX_CELLS`]。
    ///
    /// 单元数上限（2^26）远小于 `u32::MAX`，因此编码时把维度写成 u32 是安全的。
    pub fn is_valid(&self) -> bool {
        self.buckets > 0
            && self.hashers > 0
            && self.hashers <= Self::MAX_HASHERS
            && self
                .buckets
                .checked_mul(self.hashers)
                .is_some_and(|cells| cells <= Self::MAX_CELLS)
    }
}

impl Default for ViewParams {
    /// 默认 2048 个桶、5 个哈希函数、种子 0，共 40 KiB。
    ///
    /// 2048 是 2 的幂，取模可以用更便宜的运算；5 个哈希取中位数，
    /// 能容忍 2 个桶被碰撞污染。
    fn default() -> Self {
        Self {
            buckets: 2048,
            hashers: 5,
            seed: 0,
        }
    }
}

/// [`CompressedView::noise_floor`] 的统计结果，单位与评分相同。
///
/// 三个字段都是非负的噪声幅度（对查询结果取了绝对值）：
/// - `median`：典型噪声；
/// - `p95`：95% 的采样不超过它，反映尾部；
/// - `max`：采样中的最大值。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NoiseStats {
    /// 噪声幅度的中位数。
    pub median: Score,
    /// 噪声幅度的 95 分位（最近秩法）。
    pub p95: Score,
    /// 噪声幅度的最大值。
    pub max: Score,
}

impl fmt::Display for NoiseStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "median={:.4} p95={:.4} max={:.4}",
            self.median, self.p95, self.max
        )
    }
}

/// 定长的压缩观点（Count Sketch）。
///
/// 由 [`CompressedView::new`] 或 [`CompressedView::from_pairs`] 构造；
/// 用 [`CompressedView::add`] 增量写入；用 [`CompressedView::query`] 查询；
/// 用 [`CompressedView::merge`] 与他人观点加权合并。
///
/// 克隆得到完全相同的副本，两个副本可以独立演化后再合并。
#[derive(Clone)]
pub struct CompressedView {
    params: ViewParams,
    /// 每个 hasher 的独立 64 位种子，由 `params.seed` 派生。
    /// 预先算好，查询路径上不再重复派生。
    seeds: Vec<u64>,
    /// 行优先的 `hashers × buckets` 矩阵：`data[j * buckets + bucket]`。
    data: Vec<Score>,
}

impl fmt::Debug for CompressedView {
    /// 只打印形状，不打印上万个桶，避免日志爆炸。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompressedView")
            .field("params", &self.params)
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

impl CompressedView {
    /// 构造全零视图。
    ///
    /// # Panics
    ///
    /// 参数非法（维度为 0 或单元数超过 [`ViewParams::MAX_CELLS`]）时 panic，
    /// 并打印参数内容。参数通常来自配置或协议握手，属于编程错误而非运行时输入。
    pub fn new(params: ViewParams) -> Self {
        assert!(
            params.is_valid(),
            "ViewParams 非法：{:?}；buckets/hashers 必须为正，且 buckets*hashers <= {}",
            params,
            ViewParams::MAX_CELLS
        );
        let seeds = (0..params.hashers)
            .map(|j| derive_seed(params.seed, j))
            .collect();
        // is_valid 已保证不会溢出。
        let cells = params.buckets * params.hashers;
        Self {
            params,
            seeds,
            data: vec![0.0; cells],
        }
    }

    /// 从一批 `(key, score)` 构造视图。
    ///
    /// 同一个 key 出现多次会累加（Count Sketch 是线性结构）；
    /// 分数为 0 的条目等价于没写，NaN/∞ 会被忽略。
    pub fn from_pairs(params: ViewParams, pairs: &[(AccountKey, Score)]) -> Self {
        let mut view = Self::new(params);
        view.add_pairs(pairs);
        view
    }

    /// 批量写入，语义同 [`Self::add`]。
    pub fn add_pairs(&mut self, pairs: &[(AccountKey, Score)]) {
        for (key, score) in pairs {
            self.add(key, *score);
        }
    }

    /// 写入一条评分：对每个 hasher，把 `±score` 累加到 key 对应的桶。
    ///
    /// - `score == 0`（含 `-0.0`）表示没表态，直接返回，不改变任何桶；
    /// - 非有限值（NaN/∞）被忽略，避免一个坏分数污染所有查询；
    /// - 负分在数学上会正常累加，但协议层不允许，本 crate 不做截断。
    pub fn add(&mut self, key: &AccountKey, score: Score) {
        if !score.is_finite() || score == 0.0 {
            return;
        }
        for hasher in 0..self.params.hashers {
            let (bucket, sign) = self.locate(hasher, key);
            self.data[hasher * self.params.buckets + bucket] += sign * score;
        }
    }

    /// 查询某个 key 的近似评分：取各哈希桶估值的中位数。
    ///
    /// 空视图、或从没写过的 key（没有发生碰撞时）返回 0。
    /// 返回值是带符号的；噪声底请用 [`Self::noise_floor`]（那里取绝对值）。
    #[must_use]
    pub fn query(&self, key: &AccountKey) -> Score {
        // new 保证 hashers > 0，这里只是防御性写法。
        if self.params.hashers == 0 {
            return 0.0;
        }
        let mut estimates = Vec::with_capacity(self.params.hashers);
        for hasher in 0..self.params.hashers {
            let (bucket, sign) = self.locate(hasher, key);
            estimates.push(sign * self.data[hasher * self.params.buckets + bucket]);
        }
        estimates.sort_by(|a, b| a.total_cmp(b));
        median_sorted(&estimates)
    }

    /// 逐元素加权线性相加：`self = w_self * self + w_other * other`。
    ///
    /// 这是「合成观点」的入口：我的观点保留权重 `w_self`，对方观点按
    /// `w_other` 融入。逐元素相加保证结构仍是同一个 Count Sketch。
    ///
    /// 注意：合并的线性是**桶层面**的。查询取中位数不是线性运算，因此在有
    /// 碰撞时 `merged.query(k) != w_self * a.query(k) + w_other * b.query(k)`；
    /// 无碰撞时两者一致。
    ///
    /// # Panics
    ///
    /// 双方参数（含 seed）不一致，或权重不是有限值时 panic。参数不同的草图
    /// 桶布局不同，逐元素相加没有意义；权重 NaN 会污染整个视图。
    ///
    /// 权重按协议应当是非负且通常 ≤ 1。极大的有限权重会让桶值溢出成 ∞，
    /// 此时 [`Self::encode`] 的负载会被 [`Self::decode`] 拒绝（decode 只接受
    /// 有限值），调用方需要自己保证权重合理。
    pub fn merge(&mut self, other: &CompressedView, w_self: Score, w_other: Score) {
        assert_eq!(
            self.params, other.params,
            "merge 要求双方 ViewParams 完全一致（buckets/hashers/seed）"
        );
        assert!(
            w_self.is_finite() && w_other.is_finite(),
            "merge 的权重必须是有限值：w_self={}, w_other={}",
            w_self,
            w_other
        );
        for (mine, theirs) in self.data.iter_mut().zip(other.data.iter()) {
            *mine = w_self * *mine + w_other * *theirs;
        }
    }

    /// 当前参数。
    #[must_use]
    pub fn params(&self) -> ViewParams {
        self.params
    }

    /// 桶数（k）。
    #[must_use]
    pub fn buckets(&self) -> usize {
        self.params.buckets
    }

    /// 哈希函数个数（d）。
    #[must_use]
    pub fn hashers(&self) -> usize {
        self.params.hashers
    }

    /// 编码后的固定字节数：`buckets * hashers * 4`，与写入量无关。
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.data.len() * std::mem::size_of::<Score>()
    }

    /// 序列化为恰好 [`Self::bytes`] 字节。
    ///
    /// 布局：行优先遍历桶矩阵，每个 f32 写成小端 4 字节。参数不写进负载，
    /// 由通信双方在带外约定；[`Self::decode`] 会用调用者给的参数校验长度。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes());
        for value in &self.data {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    /// 反序列化。长度必须恰好等于 `params.bytes()`，负载中不允许出现 NaN/∞。
    ///
    /// 返回 `None` 的情况：参数非法、长度不符、负载含非有限值。
    /// 参数（含 seed）必须在编码端和解码端一致；本格式不带参数自描述，
    /// 传错参数时长度仍可能对得上，但那不是本函数能检测的。
    #[must_use]
    pub fn decode(bytes: &[u8], params: ViewParams) -> Option<Self> {
        if !params.is_valid() {
            return None;
        }
        let cells = params.buckets.checked_mul(params.hashers)?;
        let expected = cells.checked_mul(std::mem::size_of::<Score>())?;
        if bytes.len() != expected {
            return None;
        }

        let mut data = Vec::with_capacity(cells);
        for chunk in bytes.chunks_exact(std::mem::size_of::<Score>()) {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(chunk);
            let value = f32::from_le_bytes(raw);
            // 一条 NaN 就会让所有查询变成 NaN，直接拒绝整份负载。
            if !value.is_finite() {
                return None;
            }
            data.push(value);
        }

        Some(Self {
            params,
            seeds: (0..params.hashers)
                .map(|j| derive_seed(params.seed, j))
                .collect(),
            data,
        })
    }

    /// 实测噪声底：随机生成 `samples` 个全新的 32 字节 key，逐个查询并统计幅度。
    ///
    /// 全新 key 的真实观点是 0，所以查询结果就是纯粹的碰撞噪声。统计的是
    /// `|query|`（噪声幅度，非负）。
    ///
    /// 注意稀疏情形：默认参数下写入 1000 条时，每个桶平均只有约 0.5 条记录，
    /// 绝大多数查询的 5 个估值里至少 3 个恰好是 0，噪声幅度的中位数就是 0——
    /// 这恰恰说明视图接近精确。即使碰撞开始出现，中位数也可能继续是 0：查询取的是
    /// 5 个**带符号**估值的中位数，空桶足够多、其余行正负相抵时，中位数仍会精确
    /// 落在 0。要看碰撞的尾部影响，请看 `p95` 和 `max`；写入量接近或超过桶数时，
    /// `median` 才会明显非零。
    ///
    /// `seed` 只影响采样哪些 key，同一种子结果完全可复现。`samples == 0` 时
    /// 返回全零。
    #[must_use]
    pub fn noise_floor(&self, samples: usize, seed: u64) -> NoiseStats {
        if samples == 0 {
            return NoiseStats::default();
        }
        let mut rng = XorShift64::new(seed);
        let mut magnitudes = Vec::with_capacity(samples);
        for _ in 0..samples {
            let key = rng.next_key();
            magnitudes.push(self.query(&key).abs());
        }
        magnitudes.sort_by(|a, b| a.total_cmp(b));
        NoiseStats {
            median: median_sorted(&magnitudes),
            p95: percentile95_sorted(&magnitudes),
            max: magnitudes[magnitudes.len() - 1],
        }
    }

    /// 第 `hasher` 个哈希函数对 key 的落点：桶下标与 ±1 符号。
    ///
    /// 定桶用一条带种子的 FNV-1a，再过 splitmix64 终混合改善低位分布；
    /// 定符号用另一条带盐的 FNV-1a，取最高位（低位与 key 的奇偶性相关，不能用）。
    #[inline]
    fn locate(&self, hasher: usize, key: &AccountKey) -> (usize, Score) {
        let seed = self.seeds[hasher];
        let hash_bucket = fmix64(fnv1a64_seeded(key, seed));
        let bucket = (hash_bucket % self.params.buckets as u64) as usize;

        let hash_sign = fnv1a64_seeded(key, seed ^ SIGN_SALT);
        let sign = if (hash_sign >> 63) & 1 == 0 {
            1.0
        } else {
            -1.0
        };
        (bucket, sign)
    }
}

/// 有序切片的中位数。空切片返回 0，偶数个取中间两个的平均。
fn median_sorted(sorted: &[Score]) -> Score {
    match sorted.len() {
        0 => 0.0,
        len if len % 2 == 1 => sorted[len / 2],
        len => (sorted[len / 2 - 1] + sorted[len / 2]) * 0.5,
    }
}

/// 有序切片的 p95，最近秩法：取第 `ceil(0.95 * n)` 个（1 起算）。
fn percentile95_sorted(sorted: &[Score]) -> Score {
    if sorted.is_empty() {
        return 0.0;
    }
    let len = sorted.len();
    // ceil(0.95 * len) = ceil(95 * len / 100)
    let rank = len
        .saturating_mul(95)
        .saturating_add(99)
        .saturating_div(100);
    let index = rank.saturating_sub(1).min(len - 1);
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> AccountKey {
        [byte; 32]
    }

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median_sorted(&[]), 0.0);
        assert_eq!(median_sorted(&[1.0]), 1.0);
        assert_eq!(median_sorted(&[1.0, 3.0]), 2.0);
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn percentile_matches_nearest_rank() {
        let values: Vec<f32> = (1..=100).map(|v| v as f32).collect();
        // 100 个值，p95 = 第 95 个。
        assert_eq!(percentile95_sorted(&values), 95.0);
        assert_eq!(percentile95_sorted(&[7.0]), 7.0);
        assert_eq!(percentile95_sorted(&[1.0, 2.0]), 2.0);
        assert_eq!(percentile95_sorted(&[]), 0.0);
    }

    #[test]
    fn query_takes_median_and_rejects_minority_outliers() {
        let params = ViewParams::new(64, 5, 0);
        let mut view = CompressedView::new(params);
        let target = key(9);

        // 直接摆布桶值：两行是 100，三行是 0.5。中位数应当无视那两行。
        for hasher in 0..params.hashers {
            let (bucket, sign) = view.locate(hasher, &target);
            let planted = if hasher < 2 { 100.0 } else { 0.5 };
            view.data[hasher * params.buckets + bucket] = sign * planted;
        }
        assert_eq!(view.query(&target), 0.5);

        // 三行污染、两行干净时，中位数被拉走——这是中位数的容忍边界。
        for hasher in 0..params.hashers {
            let (bucket, sign) = view.locate(hasher, &target);
            let planted = if hasher < 3 { 2.0 } else { 0.5 };
            view.data[hasher * params.buckets + bucket] = sign * planted;
        }
        assert_eq!(view.query(&target), 2.0);
    }

    #[test]
    fn merge_is_elementwise_linear() {
        let params = ViewParams::new(128, 5, 3);
        let mut left = CompressedView::new(params);
        let mut right = CompressedView::new(params);
        left.add(&key(1), 0.9);
        left.add(&key(2), 0.4);
        right.add(&key(2), 0.7);
        right.add(&key(3), 0.2);

        let left_before = left.data.clone();
        let right_before = right.data.clone();
        left.merge(&right, 0.25, 0.75);

        for i in 0..left.data.len() {
            let expected = 0.25f32 * left_before[i] + 0.75f32 * right_before[i];
            assert_eq!(left.data[i], expected, "第 {i} 个桶不是加权和");
        }
    }

    #[test]
    fn encode_uses_little_endian() {
        let mut view = CompressedView::new(ViewParams::new(1, 1, 0));
        view.add(&key(5), 1.0);
        let bytes = view.encode();
        assert_eq!(bytes.len(), 4);
        let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        // 符号取决于哈希，幅度必须是 1.0。
        assert_eq!(value.abs(), 1.0);
        assert_eq!(&bytes, &value.to_le_bytes());
    }

    #[test]
    fn seeds_differ_per_hasher_and_follow_params() {
        let a = CompressedView::new(ViewParams::new(16, 6, 0));
        let b = CompressedView::new(ViewParams::new(16, 6, 0));
        let c = CompressedView::new(ViewParams::new(16, 6, 1));
        assert_eq!(a.seeds, b.seeds);
        assert_ne!(a.seeds, c.seeds);
        assert_eq!(a.seeds.len(), 6);
        let unique: std::collections::HashSet<u64> = a.seeds.iter().copied().collect();
        assert_eq!(unique.len(), 6, "各 hasher 的种子必须互不相同");

        // 同一个 key 在不同 hasher 下不应总落在同一个桶。
        let target = key(3);
        let buckets: std::collections::HashSet<usize> =
            (0..6).map(|j| a.locate(j, &target).0).collect();
        assert!(buckets.len() > 1);
    }

    #[test]
    fn invalid_params_are_rejected() {
        assert!(!ViewParams::new(0, 5, 0).is_valid());
        assert!(!ViewParams::new(2048, 0, 0).is_valid());
        assert!(!ViewParams::new(ViewParams::MAX_CELLS, 2, 0).is_valid());
        assert!(!ViewParams::new(1, ViewParams::MAX_HASHERS + 1, 0).is_valid());
        assert!(ViewParams::new(1, ViewParams::MAX_HASHERS, 0).is_valid());
        assert!(ViewParams::new(1, 1, 0).is_valid());
        assert!(ViewParams::default().is_valid());
    }
}
