//! # tm_agg —— 衰减与聚合
//!
//! 本 crate 是 trustmesh 的数值层，只包含三个纯函数，没有任何 I/O、线程、全局状态与
//! 外部依赖（只用 std）：
//!
//! | 函数 | 作用 |
//! | --- | --- |
//! | [decay] | 时间衰减：0.5 的 (dt / half_life) 次方 |
//! | [aggregate] | 把多个来源的观点聚成一个数（四种口径） |
//! | [combine] | β × 个人项 + (1 − β) × 群体项 |
//!
//! ## 数值约定
//!
//! * 入参与返回值都是 [Score]（f32）；中间累加一律用 f64，只在出口转回 f32。
//!   理由：来源可能有几百个，f32 连加会积累可见误差，而聚合结果对精度敏感。
//! * 对外返回的声望类数值一律 clamp 到 [0, 1]；NaN 一律按 0.0 处理
//!   （0 在协议里就是「没表态」，是最保守的语义）。
//! * 衰减在极端 dt 下会下溢，用 [DECAY_FLOOR]（最小正 f32）兜底，
//!   保证「指数衰减永不精确归零」这条规格在工程上成立。
//!
//! ## 安全模型上的一句话
//!
//! 均值把话语权交给权重，加权中位数把话语权交给「过半权重」，
//! 中位数完全无视权重。正是这个差异决定了单点操纵能否成功，
//! 具体数字见 tests/sybil_report.rs 的实验。

#![forbid(unsafe_code)]

use std::cmp::Ordering;

/// 评分 / 观点 / 声望的统一数值类型。
///
/// 语义：0.0 表示「没表态」；(0, 1] 表示表态的强度；协议中不存在负分。
pub type Score = f32;

/// 衰减结果的工程下界：最小的正 f32（次正规数 2^-149 ≈ 1.4e-45）。
///
/// 数学上 0.5 的 x 次方对任意有限 x 都严格大于 0，但 f32 表示不了更小的正数。
/// 为了让「永不精确归零」在工程上成立，下溢时返回该值而不是 0.0。
pub const DECAY_FLOOR: f32 = f32::from_bits(1);

/// 截尾比例 p 的允许上界：严格小于 0.5 的一个安全值（0.5 − 2^-24）。
///
/// p = 0.5 会把全部权重裁光（两端各裁 50%），因此上界取 0.5 − 2^-24，
/// 保证中间至少留下约 2.4e-7 × 总权重的有效样本。
pub const MAX_TRIM: f32 = 0.5 - f32::EPSILON / 2.0;

// ---------------------------------------------------------------------------
// 【1】衰减
// ---------------------------------------------------------------------------

/// 指数衰减因子：factor = 0.5 ^ (dt / half_life)。
///
/// # 参数
/// * dt —— 当前时间减上次成功取到对方观点的时间（单位与 half_life 相同，通常是小时）。
/// * half_life —— 半衰期 H，默认 72；传 0 视为 1（避免除零）。
///
/// # 性质
/// * dt == 0 时精确返回 1.0（刚取到的观点不衰减）。
/// * dt == H 时精确返回 0.5；dt == 2H 精确返回 0.25。
/// * 对任意 dt 都严格大于 0（极端下溢时返回 [DECAY_FLOOR]），且不是 NaN、不是负数。
/// * 对 dt 单调不增，对 half_life 单调不减。
///
/// # 例子
///
/// ```
/// use tm_agg::decay;
/// assert_eq!(decay(0, 72), 1.0);
/// assert_eq!(decay(72, 72), 0.5);
/// assert_eq!(decay(144, 72), 0.25);
/// assert!(decay(u64::MAX, 1) > 0.0); // 下溢但永不归零
/// ```
#[must_use]
pub fn decay(dt: u64, half_life: u64) -> f32 {
    // half_life = 0 按 1 处理：等价于「每个时间单位衰减一半」，同时避免除零。
    let h = if half_life == 0 { 1 } else { half_life };

    // 0.5^0 必须精确等于 1.0，单独短路，不依赖 powf 的舍入。
    if dt == 0 {
        return 1.0;
    }

    // 用 f64 做除法与幂运算：u64 直接相乘会溢出，且 f64 的精度足够覆盖 u64 全量程。
    let exponent = dt as f64 / h as f64;

    // 底数固定为 0.5（精确可表示），指数只可能 >= 0，所以结果落在 (0, 1]。
    let value = 0.5f64.powf(exponent);

    // 极大 dt 会让 f64 直接下溢成 0.0（dt/H > 1074 左右）。
    // 规格要求永不精确归零，这里用最小正 f32 兜底；NaN / 非有限也一并兜底。
    if !value.is_finite() || value <= 0.0 {
        return DECAY_FLOOR;
    }

    let factor = value as f32;
    // f64 的正数转 f32 仍可能下溢成 0（f64 最小正数约 5e-324，远小于 f32 的 1.4e-45）。
    if factor > 0.0 && factor.is_finite() {
        factor
    } else {
        DECAY_FLOOR
    }
}

// ---------------------------------------------------------------------------
// 【2】聚合
// ---------------------------------------------------------------------------

/// 聚合口径。
///
/// items 里每一项是 (权重, 数值)，权重通常是我对来源的评分 × 衰减因子。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AggMode {
    /// 加权平均：Σ(w·v) / Σw。权重就是话语权，被单个大权重来源支配。
    Mean,
    /// 普通中位数：只忽略权重的**大小**（每个来源一票），完全看数值分布。
    /// 抗单点操纵最强，但挡不住人头攻击（见 tests/sybil_report.rs）。
    Median,
    /// 加权中位数：按数值升序，取累计权重首次达到总权重一半处的数值（下侧中位数）。
    WeightedMedian,
    /// 截尾加权平均：两端各裁掉 p 比例的总权重，再对剩下的部分求加权平均。
    ///
    /// p 会被 clamp 到 [0, MAX_TRIM]；p 在 [0, 0.5) 之外属于非法输入，按边界处理。
    Trimmed(f32),
}

/// 把 (权重, 数值) 列表聚合成一个分数；返回 None 表示没有有效来源。
///
/// # 有效样本的定义（四种口径完全一致）
/// * 权重必须是**有限的正数**：`w <= 0` 或 NaN / ±inf 一律剔除。
///   0 在协议里就是「没表态」，负分不存在，NaN/inf 是脏数据。
/// * 数值必须是**有限数**：NaN / ±inf 无法排序、也无法 clamp，一并剔除。
///
/// [AggMode::Median] 只是不理会权重的**数值大小**（每个来源一票），
/// 入参卫生检查与其它口径一样要做。这样做的理由有两条：
/// 一是「剔除权重 <= 0 的条目」是全局规则，对哪个口径都不该开例外；
/// 二是协议里权重 = 我的评分 × 衰减因子，衰减恒大于 0，
/// 所以权重为 0 只可能来自「我给他打了 0 分」，本就不该出现在待聚合列表里。
///
/// # 返回值
/// * 没有有效样本（含空列表）→ None。
/// * 否则返回 clamp 到 [0, 1] 的聚合值。
///
/// ```
/// use tm_agg::{aggregate, AggMode};
/// let items = [(1.0f32, 0.0f32), (1.0, 1.0)];
/// // 均值被单个极端值拉走，中位数纹丝不动
/// assert!((aggregate(&items, AggMode::Mean).unwrap() - 0.5).abs() < 1e-6);
/// assert_eq!(aggregate(&items, AggMode::Median).unwrap(), 0.5);
/// assert_eq!(aggregate(&[], AggMode::Mean), None);
/// // 权重为 0 = 没表态，任何口径都不该把它算进去
/// assert_eq!(aggregate(&[(0.0, 0.5)], AggMode::Median), None);
/// ```
#[must_use]
pub fn aggregate(items: &[(f32, f32)], mode: AggMode) -> Option<f32> {
    let samples = valid_samples(items);
    if samples.is_empty() {
        return None;
    }

    let value = match mode {
        AggMode::Mean => weighted_mean(&samples),
        AggMode::Median => median(&samples),
        AggMode::WeightedMedian => weighted_median(&samples),
        AggMode::Trimmed(p) => trimmed_mean(&samples, p),
    };
    Some(clamp01(value))
}

/// 过滤出加权聚合可用的样本，并按数值升序排好。
fn valid_samples(items: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut samples: Vec<(f32, f32)> = items
        .iter()
        .copied()
        // 权重必须是有限正数（0 = 没表态，负权重在协议里不存在），数值必须有限。
        .filter(|&(w, v)| w.is_finite() && w > 0.0 && v.is_finite())
        .collect();
    // 数值已保证有限，partial_cmp 必定 Some；用 unwrap_or 避免任何 panic 可能。
    samples.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    samples
}

/// 普通中位数：只看数值、不看权重的大小。偶数个取中间两个的平均值。
///
/// `samples` 由 [valid_samples] 提供，已按数值升序排好且非空。
fn median(samples: &[(f32, f32)]) -> f32 {
    let n = samples.len();
    if n % 2 == 1 {
        samples[n / 2].1
    } else {
        // 先各除以 2 再相加，避免两个超大值相加溢出成 inf。
        samples[n / 2 - 1].1 * 0.5 + samples[n / 2].1 * 0.5
    }
}

/// 加权平均：Σ(w·v) / Σw，f64 累加。调用方保证 samples 非空且权重为正。
fn weighted_mean(samples: &[(f32, f32)]) -> f32 {
    let mut numerator = 0.0f64;
    let mut denominator = 0.0f64;
    for &(w, v) in samples {
        let w = w as f64;
        numerator += w * (v as f64);
        denominator += w;
    }
    // denominator > 0 由调用方保证（至少一个正权重）。
    (numerator / denominator) as f32
}

/// 加权中位数：samples 已按数值升序，取累计权重首次 >= 总权重一半处的数值。
///
/// 用「>=」而不是「>」，即下侧中位数：权重恰好卡在一半时，判给数值较小的一侧。
/// 对攻击者更不友好（想让中位数抬高，必须拿到严格过半的权重）。
fn weighted_median(samples: &[(f32, f32)]) -> f32 {
    let total: f64 = samples.iter().map(|&(w, _)| w as f64).sum();
    let half = total * 0.5;
    let mut cumulative = 0.0f64;
    for &(w, v) in samples {
        cumulative += w as f64;
        if cumulative >= half {
            return v;
        }
    }
    // 浮点累加理论上可能让最后一项也没跨过一半，兜底取最大值（等价于上侧中位数）。
    samples[samples.len() - 1].1
}

/// 截尾加权平均：两端各裁掉 p 比例的总权重，剩下的部分求加权平均。
///
/// 实现细节：把每个条目看成权重轴上的一段区间 [cum, cum + w)，
/// 只保留落在 [p·total, (1−p)·total) 内的那部分权重。
/// 也就是说边界上的条目会被「切片」而不是整条丢弃 ——
/// 当权重不等时，这比「整条丢弃」更贴近「裁掉 p 比例权重」的字面含义。
fn trimmed_mean(samples: &[(f32, f32)], p: f32) -> f32 {
    let total: f64 = samples.iter().map(|&(w, _)| w as f64).sum();
    let p = normalize_trim(p);
    let low = (p as f64) * total; // 低端裁掉的绝对权重
    let high = total - low; // 高端保留边界

    let mut numerator = 0.0f64;
    let mut kept = 0.0f64;
    let mut cumulative = 0.0f64;
    for &(w, v) in samples {
        let w = w as f64;
        let start = cumulative;
        let end = cumulative + w;
        cumulative = end;
        // 与保留区间 [low, high] 的重叠长度
        let lo = if start > low { start } else { low };
        let hi = if end < high { end } else { high };
        if hi > lo {
            let slice = hi - lo;
            kept += slice;
            numerator += slice * (v as f64);
        }
    }

    if kept > 0.0 {
        (numerator / kept) as f32
    } else {
        // p 被 clamp 在 [0, MAX_TRIM)，理论上 kept > 0；这里只是防御性兜底。
        weighted_median(samples)
    }
}

/// 把截尾比例规范到合法区间：NaN → 0，其余 clamp 到 [0, MAX_TRIM]。
fn normalize_trim(p: f32) -> f32 {
    if p.is_nan() {
        return 0.0;
    }
    if p <= 0.0 {
        return 0.0;
    }
    if p >= MAX_TRIM {
        return MAX_TRIM;
    }
    p
}

/// 把任意 f32 规范到 [0, 1]：NaN 视为 0，±inf 截断到边界。
fn clamp01(x: f32) -> f32 {
    if x.is_nan() || x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else {
        x
    }
}

// ---------------------------------------------------------------------------
// 【3】合成
// ---------------------------------------------------------------------------

/// 合成最终声望：β × 个人项 + (1 − β) × 群体项。
///
/// # 规则
/// * group 为 None 时按 0.0 处理（没有取到任何观点 ⇒ 群体项为 0）。
/// * beta 先 clamp 到 [0, 1]；beta = 1 时结果精确等于 personal，beta = 0 时精确等于 group。
/// * personal / group 里的 NaN 一律按 0.0 处理，结果最后再 clamp 到 [0, 1]。
/// * 只衰减别人的观点，不衰减自己的评分 —— 所以 personal 是裸值，直接参与合成。
///
/// ```
/// use tm_agg::combine;
/// assert_eq!(combine(1.0, 0.7, Some(0.2)), 0.7);
/// assert_eq!(combine(0.0, 0.7, Some(0.2)), 0.2);
/// assert_eq!(combine(0.5, 0.8, None), 0.4); // 没有群体项 ⇒ 群体项按 0 算
/// ```
#[must_use]
pub fn combine(beta: f32, personal: Score, group: Option<Score>) -> Score {
    let beta = clamp01(beta);
    let personal = clamp01(personal);
    let group = clamp01(group.unwrap_or(0.0));
    // beta = 1 时 0.0 * group 精确为 0.0，beta = 0 时结果精确为 group。
    clamp01(beta * personal + (1.0 - beta) * group)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-6;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= EPS
    }

    // ---------------- 衰减 ----------------

    #[test]
    fn decay_zero_dt_is_exactly_one() {
        assert_eq!(decay(0, 72), 1.0);
        assert_eq!(decay(0, 1), 1.0);
        assert_eq!(decay(0, 0), 1.0);
        assert_eq!(decay(0, u64::MAX), 1.0);
    }

    #[test]
    fn decay_integer_half_lives_are_exact() {
        for h in [1u64, 7, 72, 100, 1_000, 1_000_000] {
            assert_eq!(decay(h, h), 0.5, "dt == H 时必须是 0.5，H = {h}");
            assert_eq!(decay(2 * h, h), 0.25, "dt == 2H 时必须是 0.25，H = {h}");
            assert_eq!(decay(3 * h, h), 0.125, "dt == 3H 时必须是 0.125，H = {h}");
            assert_eq!(decay(4 * h, h), 0.0625, "dt == 4H 时必须是 0.0625，H = {h}");
        }
    }

    #[test]
    fn decay_half_life_is_the_classic_value() {
        // 72 小时半衰期下，36 小时应该衰减到 1/sqrt(2)
        let v = decay(36, 72);
        assert!(close(v, std::f32::consts::FRAC_1_SQRT_2), "得到 {v}");
        // 24 小时 = 1/3 个半衰期
        let v3 = decay(24, 72);
        assert!(close(v3, 0.793_700_5), "得到 {v3}");
    }

    #[test]
    fn decay_is_monotone_non_increasing_in_dt() {
        let h = 72u64;
        let mut prev = decay(0, h);
        assert_eq!(prev, 1.0);
        let mut dt = 0u64;
        while dt < 2000 {
            dt += 7;
            let cur = decay(dt, h);
            assert!(cur <= prev, "dt = {dt} 时衰减值反而上升了：{prev} -> {cur}");
            assert!(cur > 0.0 && cur <= 1.0, "衰减值越界：{cur}");
            prev = cur;
        }
    }

    #[test]
    fn decay_is_monotone_non_decreasing_in_half_life() {
        let dt = 100u64;
        let mut prev = decay(dt, 1);
        for h in 2..=400u64 {
            let cur = decay(dt, h);
            assert!(cur >= prev, "H = {h} 时衰减值反而下降：{prev} -> {cur}");
            assert!(cur <= 1.0);
            prev = cur;
        }
        // 半衰期无穷大 ⇒ 几乎不衰减
        assert!(close(decay(dt, u64::MAX), 1.0));
    }

    #[test]
    fn decay_never_reaches_exact_zero() {
        // 覆盖 f64 正常区间、f64 下溢区间和 u64 极值
        let cases = [1u64, 1074, 1075, 5000, 100_000, u64::MAX / 2, u64::MAX];
        for &dt in &cases {
            for &h in &[0u64, 1, 72, 1000] {
                let v = decay(dt, h);
                assert!(v > 0.0, "dt = {dt}, H = {h} 时衰减值归零了");
                assert!(!v.is_nan() && !v.is_infinite(), "dt = {dt}, H = {h} 时非有限");
                assert!(v <= 1.0);
            }
        }
        // 明确的极端情况：0.5^(1024) 已低于 f64 最小正数的一半，必然下溢
        assert_eq!(decay(5000, 1), DECAY_FLOOR);
        assert_eq!(decay(u64::MAX, 1), DECAY_FLOOR);
        let floor = DECAY_FLOOR;
        assert!(floor > 0.0 && floor.is_finite(), "DECAY_FLOOR 必须是正的有限数");
        // 但还留在 f32 次正规区间内的值不该被兜底抬高成 1.4e-45
        let tiny = decay(144, 1); // 2^-144 ≈ 4.5e-44，f32 可表示
        assert!(tiny > floor, "得到 {tiny}");
        // 2^-144 = 2^-149 × 2^5，位模式正好是 32（f32 最小次正规数左移 5 位）。
        // 注意：DECAY_FLOOR 是 1e-45 量级，用 1e-6 的绝对容差比较等于什么都没验，
        // 这里必须用相对误差。
        let two_pow_144 = f32::from_bits(32);
        assert!(
            (tiny - two_pow_144).abs() <= two_pow_144 * 1e-6,
            "2^-144 应当 ≈ {two_pow_144:e}，实际 {tiny:e}"
        );
    }

    #[test]
    fn decay_zero_half_life_is_treated_as_one() {
        assert_eq!(decay(0, 0), 1.0);
        assert_eq!(decay(1, 0), 0.5);
        assert_eq!(decay(2, 0), 0.25);
        assert_eq!(decay(10, 0), decay(10, 1));
        assert_eq!(decay(1024, 0), DECAY_FLOOR);
    }

    #[test]
    fn decay_squaring_identity_holds() {
        // decay(2dt) 应当等于 decay(dt) 的平方
        for dt in [1u64, 3, 12, 36, 71, 72, 200] {
            let a = decay(dt, 72);
            let b = decay(2 * dt, 72);
            assert!(close(b, a * a), "dt = {dt}: {b} vs {}", a * a);
        }
    }

    #[test]
    fn decay_output_always_in_unit_interval() {
        for dt in 0..300u64 {
            let v = decay(dt, 72);
            assert!((0.0..=1.0).contains(&v));
        }
    }

    // ---------------- 聚合：通用边界 ----------------

    #[test]
    fn aggregate_empty_returns_none() {
        assert_eq!(aggregate(&[], AggMode::Mean), None);
        assert_eq!(aggregate(&[], AggMode::Median), None);
        assert_eq!(aggregate(&[], AggMode::WeightedMedian), None);
        assert_eq!(aggregate(&[], AggMode::Trimmed(0.1)), None);
    }

    #[test]
    fn aggregate_all_invalid_returns_none() {
        // 权重要么是 0，要么是负数，要么是 NaN/inf
        let bad = [
            (0.0f32, 0.5f32),
            (-1.0, 0.5),
            (f32::NAN, 0.5),
            (f32::INFINITY, 0.5),
        ];
        assert_eq!(aggregate(&bad, AggMode::Mean), None);
        assert_eq!(aggregate(&bad, AggMode::Median), None);
        assert_eq!(aggregate(&bad, AggMode::WeightedMedian), None);
        assert_eq!(aggregate(&bad, AggMode::Trimmed(0.1)), None);
    }

    #[test]
    fn zero_weight_entries_are_dropped_for_every_mode() {
        // 「剔除权重 <= 0 的条目」是全局规则：Median 也不例外。
        assert_eq!(aggregate(&[(0.0f32, 0.5f32)], AggMode::Median), None);
        // 混进一个 0 权重的高分，不能影响中位数
        let items = [(1.0f32, 0.1f32), (1.0, 0.2), (0.0, 1.0)];
        assert!(close(aggregate(&items, AggMode::Median).unwrap(), 0.15));
        // 负权重同样剔除
        assert_eq!(aggregate(&[(-1.0f32, 0.5f32)], AggMode::Median), None);
        // NaN 权重同样剔除
        assert_eq!(aggregate(&[(f32::NAN, 0.5f32)], AggMode::Median), None);
    }

    #[test]
    fn median_with_exactly_one_valid_sample_returns_some() {
        // 20 个条目里只有 1 个权重为正：Median 仍应返回该条目的数值
        let mut items = vec![(0.0f32, 9.9f32); 19];
        items.push((0.25, 0.75));
        assert_eq!(aggregate(&items, AggMode::Median), Some(0.75));
        // 只剩一个有效条目时，其它口径同样返回它
        assert_eq!(aggregate(&items, AggMode::Mean), Some(0.75));
        assert_eq!(aggregate(&items, AggMode::WeightedMedian), Some(0.75));
        assert_eq!(aggregate(&items, AggMode::Trimmed(0.4)), Some(0.75));
    }

    #[test]
    fn aggregate_all_nan_values_return_none() {
        let bad = [(1.0f32, f32::NAN), (2.0, f32::INFINITY)];
        assert_eq!(aggregate(&bad, AggMode::Mean), None);
        assert_eq!(aggregate(&bad, AggMode::Median), None);
        assert_eq!(aggregate(&bad, AggMode::WeightedMedian), None);
        assert_eq!(aggregate(&bad, AggMode::Trimmed(0.1)), None);
    }

    #[test]
    fn aggregate_single_sample_returns_it() {
        let one = [(3.0f32, 0.25f32)];
        assert!(close(aggregate(&one, AggMode::Mean).unwrap(), 0.25));
        assert!(close(aggregate(&one, AggMode::Median).unwrap(), 0.25));
        assert!(close(aggregate(&one, AggMode::WeightedMedian).unwrap(), 0.25));
        assert!(close(aggregate(&one, AggMode::Trimmed(0.4)).unwrap(), 0.25));
    }

    // ---------------- 均值 ----------------

    #[test]
    fn mean_is_weighted_and_correct() {
        // (1*0.2 + 3*0.6) / 4 = 2.0 / 4 = 0.5
        let items = [(1.0f32, 0.2f32), (3.0, 0.6)];
        assert!(close(aggregate(&items, AggMode::Mean).unwrap(), 0.5));
    }

    #[test]
    fn mean_drops_nonpositive_and_nonfinite_weights() {
        let items = [
            (1.0f32, 0.2f32),
            (0.0, 1.0),        // 权重 0：没表态，剔除
            (-5.0, 1.0),       // 负权重：协议里不存在，剔除
            (f32::NAN, 1.0),   // NaN 权重：剔除
            (f32::INFINITY, 1.0), // inf 权重：剔除
            (1.0, 0.4),
        ];
        // 只剩 0.2 和 0.4 等权 ⇒ 0.3
        assert!(close(aggregate(&items, AggMode::Mean).unwrap(), 0.3));
    }

    #[test]
    fn mean_drops_nonfinite_values() {
        let items = [(1.0f32, 0.5f32), (1.0, f32::NAN), (1.0, f32::NEG_INFINITY)];
        assert!(close(aggregate(&items, AggMode::Mean).unwrap(), 0.5));
    }

    #[test]
    fn mean_is_clamped_to_unit_interval() {
        assert_eq!(aggregate(&[(1.0f32, 2.0f32)], AggMode::Mean).unwrap(), 1.0);
        assert_eq!(aggregate(&[(1.0f32, -3.0f32)], AggMode::Mean).unwrap(), 0.0);
        let items = [(1.0f32, -1.0f32), (1.0, 1.0)];
        assert_eq!(aggregate(&items, AggMode::Mean).unwrap(), 0.0);
    }

    // ---------------- 中位数 ----------------

    #[test]
    fn median_odd_and_even() {
        let odd = [(1.0f32, 0.3f32), (1.0, 0.1), (1.0, 0.2)];
        assert!(close(aggregate(&odd, AggMode::Median).unwrap(), 0.2));
        let even = [(1.0f32, 0.4f32), (1.0, 0.1), (1.0, 0.3), (1.0, 0.2)];
        assert!(close(aggregate(&even, AggMode::Median).unwrap(), 0.25));
    }

    #[test]
    fn median_ignores_weight_magnitude() {
        // 权重几乎全压给 0.1 那个来源，中位数依然落在数值居中的 0.5。
        // 注意这里必须用**正**权重：权重 <= 0 的条目会被全局规则剔除。
        let items = [(1000.0f32, 0.1f32), (0.000_1, 0.9), (0.5, 0.5)];
        assert!(close(aggregate(&items, AggMode::Median).unwrap(), 0.5));
    }

    #[test]
    fn median_even_count_uses_midpoint() {
        let items = [(1.0f32, 0.0f32), (1.0, 1.0)];
        assert_eq!(aggregate(&items, AggMode::Median).unwrap(), 0.5);
    }

    // ---------------- 加权中位数 ----------------

    #[test]
    fn weighted_median_basic() {
        // 累积权重 1, 3（总 3，半 1.5）⇒ 第二次跨过 ⇒ 0.9
        let items = [(1.0f32, 0.1f32), (2.0, 0.9)];
        assert!(close(aggregate(&items, AggMode::WeightedMedian).unwrap(), 0.9));
    }

    #[test]
    fn weighted_median_ties_go_to_lower_value() {
        // 各占一半：下侧中位数判给小值
        let items = [(1.0f32, 0.1f32), (1.0, 0.9)];
        assert!(close(aggregate(&items, AggMode::WeightedMedian).unwrap(), 0.1));
    }

    #[test]
    fn weighted_median_is_order_independent() {
        let a = [(1.0f32, 0.5f32), (3.0, 0.1), (2.0, 0.9)];
        let b = [(3.0f32, 0.1f32), (2.0, 0.9), (1.0, 0.5)];
        assert!(close(
            aggregate(&a, AggMode::WeightedMedian).unwrap(),
            aggregate(&b, AggMode::WeightedMedian).unwrap()
        ));
    }

    // ---------------- 截尾均值 ----------------

    #[test]
    fn trimmed_zero_equals_weighted_mean() {
        let items = [(1.0f32, 0.0f32), (2.0, 0.5), (1.0, 1.0)];
        let mean = aggregate(&items, AggMode::Mean).unwrap();
        let trimmed = aggregate(&items, AggMode::Trimmed(0.0)).unwrap();
        assert!(close(mean, trimmed));
    }

    #[test]
    fn trimmed_drops_equal_weight_extremes() {
        // 4 个等权，p = 0.25 ⇒ 两端各去掉 1 个 ⇒ 中间两个的均值
        let items = [(1.0f32, 0.0f32), (1.0, 0.1), (1.0, 0.2), (1.0, 0.3)];
        assert!(close(
            aggregate(&items, AggMode::Trimmed(0.25)).unwrap(),
            0.15
        ));
    }

    #[test]
    fn trimmed_slices_boundary_items_proportionally() {
        // 两个等权条目，p = 0.25 ⇒ 低端裁 0.5、高端裁 0.5，
        // 0.0 剩下 0.5 权重、1.0 剩下 0.5 权重 ⇒ 0.5
        let items = [(1.0f32, 0.0f32), (1.0, 1.0)];
        assert!(close(aggregate(&items, AggMode::Trimmed(0.25)).unwrap(), 0.5));
    }

    #[test]
    fn trimmed_handles_weight_skew() {
        // 3 个 0.0 + 1 个 1.0，权重 3:1；p = 0.25 时低端裁 1、高端裁 1，
        // 0.0 的区间 [0,3) 留下 2，1.0 的区间 [3,4) 整个被裁掉 ⇒ 0.0
        let items = [(3.0f32, 0.0f32), (1.0, 1.0)];
        assert_eq!(aggregate(&items, AggMode::Trimmed(0.25)).unwrap(), 0.0);
    }

    #[test]
    fn trimmed_clamps_out_of_range_p() {
        let items = [(1.0f32, 0.0f32), (1.0, 0.5), (1.0, 1.0)];
        let hi = aggregate(&items, AggMode::Trimmed(5.0)).unwrap();
        let clamp = aggregate(&items, AggMode::Trimmed(MAX_TRIM)).unwrap();
        assert!(close(hi, clamp));
        let neg = aggregate(&items, AggMode::Trimmed(-2.0)).unwrap();
        let zero = aggregate(&items, AggMode::Trimmed(0.0)).unwrap();
        assert!(close(neg, zero));
        let nan = aggregate(&items, AggMode::Trimmed(f32::NAN)).unwrap();
        assert!(close(nan, zero));
        // 极端 p 不会 panic，也不会返回 None
        assert!(aggregate(&items, AggMode::Trimmed(0.5)).is_some());
    }

    #[test]
    fn trimmed_never_returns_none_for_valid_samples() {
        let items = [(1.0f32, 0.0f32), (1.0, 1.0)];
        for p in [0.0f32, 0.1, 0.25, 0.49, 0.5, 1.0, f32::INFINITY] {
            assert!(aggregate(&items, AggMode::Trimmed(p)).is_some(), "p = {p}");
        }
    }

    // ---------------- 四种口径在同一组数据上的差异 ----------------

    #[test]
    fn four_modes_disagree_on_same_data() {
        // 一个高权重的极端值 0.9（权重 5），三个低权重的小值
        let items = [
            (1.0f32, 0.0f32),
            (1.0, 0.2),
            (1.0, 0.4),
            (5.0, 0.9),
        ];
        let mean = aggregate(&items, AggMode::Mean).unwrap();
        let median = aggregate(&items, AggMode::Median).unwrap();
        let wmed = aggregate(&items, AggMode::WeightedMedian).unwrap();
        let trim = aggregate(&items, AggMode::Trimmed(0.125)).unwrap();

        assert!(close(mean, 0.6375), "均值 {mean}");
        assert!(close(median, 0.3), "中位数 {median}");
        assert!(close(wmed, 0.9), "加权中位数 {wmed}");
        assert!(close(trim, 0.7), "截尾均值 {trim}");

        // 四种口径两两不同，说明差异是真实的，不是数值巧合
        assert!((mean - median).abs() > 0.1);
        assert!((median - wmed).abs() > 0.1);
        assert!((trim - mean).abs() > 0.01);
    }

    // ---------------- 合成 ----------------

    #[test]
    fn combine_endpoints_are_exact() {
        assert_eq!(combine(1.0, 0.7, Some(0.2)), 0.7);
        assert_eq!(combine(0.0, 0.7, Some(0.2)), 0.2);
        assert_eq!(combine(1.0, 0.0, None), 0.0);
        assert_eq!(combine(0.0, 0.9, None), 0.0);
    }

    #[test]
    fn combine_none_group_is_zero() {
        assert!(close(combine(0.5, 0.8, None), 0.4));
        assert!(close(combine(0.25, 0.8, None), 0.2));
        assert_eq!(combine(1.0, 0.8, None), 0.8);
    }

    #[test]
    fn combine_is_linear_midpoint() {
        assert!(close(combine(0.5, 0.8, Some(0.2)), 0.5));
        assert!(close(combine(0.5, 0.6, Some(0.1)), 0.35));
    }

    #[test]
    fn combine_clamps_beta() {
        assert_eq!(combine(2.0, 0.7, Some(0.2)), 0.7);
        assert_eq!(combine(-1.0, 0.7, Some(0.2)), 0.2);
        assert_eq!(combine(f32::INFINITY, 0.7, Some(0.2)), 0.7);
        assert_eq!(combine(f32::NEG_INFINITY, 0.7, Some(0.2)), 0.2);
    }

    #[test]
    fn combine_clamps_scores() {
        assert_eq!(combine(1.0, 1.5, Some(0.2)), 1.0);
        assert_eq!(combine(1.0, -0.5, Some(0.2)), 0.0);
        assert_eq!(combine(0.0, 0.5, Some(3.0)), 1.0);
        assert_eq!(combine(0.0, 0.5, Some(-3.0)), 0.0);
    }

    #[test]
    fn combine_nan_inputs_are_treated_as_zero() {
        assert_eq!(combine(f32::NAN, 0.7, Some(0.2)), 0.2); // beta NaN → 0 ⇒ 只剩群体项
        assert_eq!(combine(1.0, f32::NAN, Some(0.2)), 0.0); // personal NaN → 0
        assert_eq!(combine(0.0, 0.7, Some(f32::NAN)), 0.0); // group NaN → 0
        let r = combine(0.5, f32::NAN, Some(f32::NAN));
        assert!(!r.is_nan());
        assert_eq!(r, 0.0);
    }

    #[test]
    fn combine_monotone_in_both_terms() {
        let mut prev = combine(0.3, 0.0, Some(0.5));
        for k in 1..=10 {
            let cur = combine(0.3, k as f32 / 10.0, Some(0.5));
            assert!(cur >= prev);
            assert!(cur <= 1.0);
            prev = cur;
        }
        let mut prev = combine(0.3, 0.5, Some(0.0));
        for k in 1..=10 {
            let cur = combine(0.3, 0.5, Some(k as f32 / 10.0));
            assert!(cur >= prev);
            prev = cur;
        }
    }

    #[test]
    fn combine_output_always_in_unit_interval() {
        let mut beta = 0.0f32;
        while beta <= 1.0 {
            for p in [0.0f32, 0.3, 1.0] {
                for g in [None, Some(0.0), Some(0.4), Some(1.0)] {
                    let r = combine(beta, p, g);
                    assert!((0.0..=1.0).contains(&r));
                }
            }
            beta += 0.125;
        }
    }
}
