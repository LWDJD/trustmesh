//! 聚合口径的交叉验证与性质测试。
//!
//! 这里不重复验证「实现等于自己」，而是用**独立构造**的参照结果做对拍：
//! * 加权中位数用「最小可行阈值」的等价定义暴力求解；
//! * 截尾均值用等权解析解 + 分位数黎曼和两种方式逼近；
//! * 随机数据用自带的 xorshift64 生成（不引入任何外部 crate）。

use tm_agg::{aggregate, decay, AggMode, MAX_TRIM};

// ---------------------------------------------------------------------------
// 自带的 xorshift64：测试用随机源，不依赖外部 crate
// ---------------------------------------------------------------------------

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // 0 是 xorshift 的不动点，必须避开
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// 均匀分布 [0, 1)
    fn next_unit(&mut self) -> f32 {
        // 取高 24 位，正好是 f32 尾数的宽度
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }
}

const EPS: f32 = 1e-6;

fn valid(items: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut s: Vec<(f32, f32)> = items
        .iter()
        .copied()
        .filter(|&(w, v)| w.is_finite() && w > 0.0 && v.is_finite())
        .collect();
    s.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    s
}

// ---------------------------------------------------------------------------
// 独立参照实现
// ---------------------------------------------------------------------------

/// 加权中位数的等价定义：在「候选值」里找最小的 v，使得权重之和 {w | value <= v} >= 总权重的一半。
fn reference_weighted_median(items: &[(f32, f32)]) -> Option<f32> {
    let s = valid(items);
    if s.is_empty() {
        return None;
    }
    let total: f64 = s.iter().map(|&(w, _)| w as f64).sum();
    let half = total * 0.5;
    for &(_, candidate) in &s {
        let acc: f64 = s
            .iter()
            .filter(|&&(_, v)| v <= candidate)
            .map(|&(w, _)| w as f64)
            .sum();
        if acc >= half {
            return Some(candidate);
        }
    }
    s.last().map(|&(_, v)| v)
}

/// 截尾均值的参照值：对保留区间 [p, 1-p] 均匀取分位点求平均（黎曼和）。
/// 步长越小越接近真值，这里用 20 万步，误差量级 ~1e-3。
fn reference_trimmed_mean(items: &[(f32, f32)], p: f32) -> Option<f32> {
    let s = valid(items);
    if s.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, MAX_TRIM) as f64;
    let total: f64 = s.iter().map(|&(w, _)| w as f64).sum();
    let mut prefix = Vec::with_capacity(s.len());
    let mut acc = 0.0f64;
    for &(w, _) in &s {
        acc += w as f64;
        prefix.push(acc / total); // 归一化累积权重
    }
    let steps = 200_000usize;
    let lo = p;
    let hi = 1.0 - p;
    let mut sum = 0.0f64;
    for i in 0..steps {
        let u = lo + (i as f64 + 0.5) * (hi - lo) / (steps as f64);
        // 找第一个 prefix[j] >= u 的 j
        let j = prefix.partition_point(|&c| c < u).min(s.len() - 1);
        sum += s[j].1 as f64;
    }
    Some((sum / steps as f64) as f32)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[test]
fn mean_is_scale_invariant() {
    let items = [(0.7f32, 0.2f32), (1.9, 0.8), (3.1, 0.5)];
    let scaled: Vec<(f32, f32)> = items.iter().map(|&(w, v)| (w * 3.7, v)).collect();
    let a = aggregate(&items, AggMode::Mean).unwrap();
    let b = aggregate(&scaled, AggMode::Mean).unwrap();
    assert!((a - b).abs() < EPS, "{a} vs {b}");
}

#[test]
fn equal_weights_mean_equals_arithmetic_mean() {
    let items = [(1.0f32, 0.1f32), (1.0, 0.2), (1.0, 0.6)];
    let mean = aggregate(&items, AggMode::Mean).unwrap();
    let arithmetic = (0.1 + 0.2 + 0.6) / 3.0;
    assert!((mean - arithmetic).abs() < EPS);
}

#[test]
fn order_does_not_matter() {
    let mut rng = XorShift64::new(0xC0FF_EE12_3456_789A);
    let mut base: Vec<(f32, f32)> = (0..40)
        .map(|_| (rng.next_unit() * 4.0, rng.next_unit()))
        .collect();

    let modes = [
        AggMode::Mean,
        AggMode::Median,
        AggMode::WeightedMedian,
        AggMode::Trimmed(0.2),
    ];
    let expected: Vec<Option<f32>> = modes.iter().map(|&m| aggregate(&base, m)).collect();

    for _ in 0..20 {
        // Fisher-Yates 洗牌
        for i in (1..base.len()).rev() {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            base.swap(i, j);
        }
        for (idx, &m) in modes.iter().enumerate() {
            let got = aggregate(&base, m);
            match (got, expected[idx]) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-5, "{m:?}: {a} vs {b}"),
                (None, None) => {}
                _ => panic!("{m:?} 的 Some/None 性质被顺序改变了"),
            }
        }
    }
}

#[test]
fn weighted_median_matches_reference_definition() {
    let mut rng = XorShift64::new(7);
    for _ in 0..200 {
        let n = (rng.next_u64() % 15) as usize;
        let items: Vec<(f32, f32)> = (0..n)
            .map(|_| (rng.next_unit() * 5.0, rng.next_unit()))
            .collect();
        let got = aggregate(&items, AggMode::WeightedMedian);
        let want = reference_weighted_median(&items);
        match (got, want) {
            (Some(a), Some(b)) => assert!((a - b).abs() < EPS, "n = {n}: {a} vs {b}"),
            (None, None) => {}
            _ => panic!("n = {n} 的 Some/None 不一致"),
        }
    }
}

#[test]
fn trimmed_mean_matches_quantile_reference() {
    let mut rng = XorShift64::new(99);
    let items: Vec<(f32, f32)> = (0..60)
        .map(|_| (rng.next_unit() * 3.0, rng.next_unit()))
        .collect();
    for p in [0.0f32, 0.05, 0.1, 0.25, 0.4] {
        let got = aggregate(&items, AggMode::Trimmed(p)).unwrap();
        let want = reference_trimmed_mean(&items, p).unwrap();
        assert!((got - want).abs() < 2e-3, "p = {p}: {got} vs {want}");
    }
}

#[test]
fn trimmed_mean_on_equal_weights_is_plain_trimmed_average() {
    // 10 个等权条目，p = 0.2 ⇒ 两端各去 2 个，剩中间 6 个的算术平均
    let values: Vec<f32> = (0..10).map(|i| i as f32 / 10.0).collect();
    let items: Vec<(f32, f32)> = values.iter().map(|&v| (1.0, v)).collect();
    let got = aggregate(&items, AggMode::Trimmed(0.2)).unwrap();
    let want = values[2..8].iter().sum::<f32>() / 6.0;
    assert!((got - want).abs() < EPS, "{got} vs {want}");
}

#[test]
fn trimmed_zero_is_exactly_mean_on_random_data() {
    let mut rng = XorShift64::new(4242);
    for _ in 0..50 {
        let n = (rng.next_u64() % 20) as usize;
        let items: Vec<(f32, f32)> = (0..n)
            .map(|_| (rng.next_unit() * 4.0 + 0.01, rng.next_unit()))
            .collect();
        let mean = aggregate(&items, AggMode::Mean);
        let trimmed = aggregate(&items, AggMode::Trimmed(0.0));
        match (mean, trimmed) {
            (Some(a), Some(b)) => assert!((a - b).abs() < EPS),
            (None, None) => {}
            _ => panic!("Trimmed(0) 与 Mean 的 Some/None 不一致"),
        }
    }
}

#[test]
fn randomized_invariants_hold() {
    let mut rng = XorShift64::new(2024);
    let modes = [
        AggMode::Mean,
        AggMode::Median,
        AggMode::WeightedMedian,
        AggMode::Trimmed(0.1),
        AggMode::Trimmed(0.35),
    ];
    for case in 0..300 {
        let n = (rng.next_u64() % 12) as usize;
        let items: Vec<(f32, f32)> = (0..n)
            .map(|_| (rng.next_unit() * 3.0, rng.next_unit()))
            .collect();
        let samples = valid(&items);
        let values: Vec<f32> = items.iter().map(|&(_, v)| v).collect();
        let (vmin, vmax) = (
            values.iter().cloned().fold(f32::INFINITY, f32::min),
            values.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        );

        for &m in &modes {
            let got = aggregate(&items, m);
            if samples.is_empty() {
                assert_eq!(got, None, "第 {case} 组空样本却返回了值");
                continue;
            }
            let v = got.expect("非空样本不应返回 None");
            assert!((0.0..=1.0).contains(&v), "第 {case} 组 {m:?} 越界：{v}");
            assert!(v >= vmin - EPS && v <= vmax + EPS, "第 {case} 组 {m:?} 不在值域内：{v}");

            // 加权中位数必须恰好落在某个来源的数值上
            if matches!(m, AggMode::WeightedMedian) {
                assert!(
                    values.iter().any(|&x| (x - v).abs() < EPS),
                    "第 {case} 组加权中位数 {v} 不是任何来源的数值"
                );
            }
        }
    }
}

#[test]
fn monotone_in_one_source_value() {
    // 把某个来源的数值从 0.0 一路抬到 1.0，任何口径的结果都不应该下降。
    // 注意：基准值 0.3 不能当作扫描起点，否则第一次比较就成了「下调数值」，
    // 会误判成实现不单调。起始的 prev 必须取自同一个 k = 0 的配置。
    let base = [(1.0f32, 0.1f32), (2.0, 0.3), (1.0, 0.9)];
    for &m in &[
        AggMode::Mean,
        AggMode::Median,
        AggMode::WeightedMedian,
        AggMode::Trimmed(0.1),
    ] {
        let mut prev = f32::NEG_INFINITY;
        for k in 0..=10 {
            let mut items = base;
            items[1].1 = k as f32 / 10.0;
            let cur = aggregate(&items, m).unwrap();
            assert!(
                cur >= prev - EPS,
                "{m:?}: 抬高数值后结果反而降低 {prev} -> {cur}（k = {k}）"
            );
            prev = cur;
        }
        // 扫到底之后必须真的到达最大值上界附近，避免「一直返回常数」也能通过
        assert!(prev > 0.1, "{m:?} 扫到 1.0 之后结果仍然是 {prev}");
    }
}

#[test]
fn handles_large_input_without_drift() {
    // 5000 个来源，权重都是 1，值交替 0/1 ⇒ 均值必然 0.5
    let items: Vec<(f32, f32)> = (0..5000)
        .map(|i| (1.0f32, if i % 2 == 0 { 1.0f32 } else { 0.0 }))
        .collect();
    let mean = aggregate(&items, AggMode::Mean).unwrap();
    assert!((mean - 0.5).abs() < EPS, "{mean}");
    assert_eq!(aggregate(&items, AggMode::Median).unwrap(), 0.5);
}

#[test]
fn decay_weights_feed_aggregation() {
    // 端到端：两个来源分别在 0 小时和 72 小时前被取到观点，
    // 权重 = 我的评分 × 衰减因子；72 小时前的观点权重减半。
    let score_a = 1.0f32;
    let score_b = 0.5f32;
    let items = [
        (score_a * decay(0, 72), 0.2f32),
        (score_b * decay(72, 72), 0.8),
    ];
    assert_eq!(items[0].0, 1.0);
    assert_eq!(items[1].0, 0.25);
    // (1.0*0.2 + 0.25*0.8) / 1.25 = 0.4 / 1.25 = 0.32
    let mean = aggregate(&items, AggMode::Mean).unwrap();
    assert!((mean - 0.32).abs() < EPS, "{mean}");
}

#[test]
fn agg_mode_is_copy_and_comparable() {
    let m = AggMode::Trimmed(0.1);
    let copied = m; // Copy
    assert_eq!(m, copied);
    assert_ne!(AggMode::Mean, AggMode::Median);
    assert!(format!("{m:?}").contains("Trimmed"));
}
