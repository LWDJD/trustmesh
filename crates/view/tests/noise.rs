//! 噪声底实测：空视图、100/1000/10000 条写入，以及不同种子的量级一致性。

use tm_view::{AccountKey, CompressedView, NoiseStats, ViewParams, XorShift64};

/// 生成 n 条 (key, score)，分数均匀落在 [0.1, 1.0]。
fn pairs(n: usize, seed: u64) -> Vec<(AccountKey, f32)> {
    let mut rng = XorShift64::new(seed);
    (0..n)
        .map(|_| {
            let key = rng.next_key();
            let unit = (rng.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            (key, 0.1 + 0.9 * unit)
        })
        .collect()
}

fn build(n: usize, view_seed: u64, entries_seed: u64) -> CompressedView {
    CompressedView::from_pairs(ViewParams::new(2048, 5, view_seed), &pairs(n, entries_seed))
}

#[test]
fn empty_view_has_zero_noise() {
    let view = CompressedView::new(ViewParams::default());
    let stats = view.noise_floor(1000, 1);
    assert_eq!(
        stats,
        NoiseStats {
            median: 0.0,
            p95: 0.0,
            max: 0.0,
        }
    );
}

#[test]
fn zero_samples_returns_zero_stats() {
    let view = build(100, 0, 1);
    assert_eq!(view.noise_floor(0, 7), NoiseStats::default());
}

#[test]
fn noise_floor_is_reproducible() {
    let view = build(2000, 0, 2);
    let a = view.noise_floor(300, 42);
    let b = view.noise_floor(300, 42);
    assert_eq!(a, b, "同一种子的噪声采样必须完全可复现");

    let c = view.noise_floor(300, 43);
    assert_ne!(a, c, "换采样种子应得到不同样本");
}

#[test]
fn hundred_entries_are_almost_exact() {
    // 100 条对 2048×5：绝大多数桶是空的，查询基本精确。
    let entries = pairs(100, 3);
    let view = CompressedView::from_pairs(ViewParams::default(), &entries);

    let mut error_sum = 0.0f32;
    let mut max_error = 0.0f32;
    let mut exact = 0usize;
    for (key, score) in &entries {
        let error = (view.query(key) - *score).abs();
        error_sum += error;
        max_error = max_error.max(error);
        if error < 1e-6 {
            exact += 1;
        }
    }
    let mean = error_sum / entries.len() as f32;
    println!("100 条写入：平均误差 {mean:.6}，最大误差 {max_error:.6}，精确 {exact}/100");

    assert!(mean < 0.01, "100 条写入的平均误差过大: {mean}");
    assert!(max_error < 0.6, "100 条写入的最大误差过大: {max_error}");
    assert!(exact >= 90, "精确查询太少: {exact}/100");
}

#[test]
fn noise_stats_are_ordered_and_density_grows_noise() {
    let sparse = build(1000, 0, 4);
    let sparse_stats = sparse.noise_floor(2000, 5);
    let dense = build(10000, 0, 4);
    let dense_stats = dense.noise_floor(2000, 5);

    println!(
        "1000 条：median={:.4} p95={:.4} max={:.4}",
        sparse_stats.median, sparse_stats.p95, sparse_stats.max
    );
    println!(
        "10000 条：median={:.4} p95={:.4} max={:.4}",
        dense_stats.median, dense_stats.p95, dense_stats.max
    );

    for stats in [sparse_stats, dense_stats] {
        assert!(stats.median <= stats.p95, "中位数不应超过 p95: {stats}");
        assert!(stats.p95 <= stats.max, "p95 不应超过 max: {stats}");
        assert!(stats.max > 0.0, "有写入就应有碰撞噪声: {stats}");
    }
    assert!(
        dense_stats.p95 > sparse_stats.p95,
        "写入量增加后 p95 噪声应变大: {sparse_stats} vs {dense_stats}"
    );
}

#[test]
fn dense_view_has_clearly_nonzero_median_noise() {
    // 10000 条对 2048 个桶：平均每桶约 4.9 条，碰撞是常态。
    let view = build(10000, 0, 6);
    let stats = view.noise_floor(2000, 7);
    assert!(
        stats.median > 0.1,
        "10000 条写入的中位噪声应明显非零: {stats}"
    );
}

#[test]
fn sparse_view_noise_lives_in_the_tail() {
    // 1000 条对 2048 个桶：平均每桶不到 0.5 条，多数查询的 5 行里
    // 至少 3 行没有碰撞，中位数为 0；碰撞只体现在尾部。
    let view = build(1000, 0, 8);
    let stats = view.noise_floor(2000, 9);
    assert!(stats.p95 > 0.05, "p95 应能测到碰撞: {stats}");
    assert!(stats.max > 0.5, "max 应能测到碰撞: {stats}");
}

#[test]
fn different_sampling_seeds_agree_on_magnitude() {
    let view = build(10000, 0, 10);
    let a = view.noise_floor(2000, 100);
    let b = view.noise_floor(2000, 200);
    let ratio = b.p95 / a.p95;
    assert!(
        (0.8..=1.25).contains(&ratio),
        "p95 随采样种子漂移过大: {a} vs {b}"
    );
}

#[test]
fn different_view_seeds_agree_on_magnitude() {
    // 同一批评分，换一个主种子：噪声量级应当同阶。
    let a_view = build(10000, 1, 11);
    let b_view = build(10000, 2, 11);
    let a = a_view.noise_floor(2000, 12);
    let b = b_view.noise_floor(2000, 12);
    let median_ratio = if a.median > 0.0 {
        b.median / a.median
    } else {
        1.0
    };
    let p95_ratio = b.p95 / a.p95;
    assert!(
        (0.7..=1.4).contains(&median_ratio),
        "不同视图种子的中位噪声差异过大: {a} vs {b}"
    );
    assert!(
        (0.7..=1.4).contains(&p95_ratio),
        "不同视图种子的 p95 噪声差异过大: {a} vs {b}"
    );
}
