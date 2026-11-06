//! Sketch 的规模特性验证。
//!
//! 三个问题：
//! 1. 大小是否与 N 无关
//! 2. 误差是否与 N 无关（稀疏评分时）
//! 3. 稠密评分时误差是否爆炸

use social_trust::rng::Rng;
use social_trust::sketch::Sketch;

fn build_dense(n: usize, per_node: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut v = vec![0.5; n];
    for _ in 0..per_node {
        let i = rng.next_usize(n);
        v[i] = rng.next_f64();
    }
    v
}

fn build_full(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    // 每个人都对所有账户有评分，模拟稠密场景
    (0..n).map(|_| rng.next_f64()).collect()
}

#[test]
fn size_independent_of_n() {
    for &n in &[100usize, 1_000, 10_000, 100_000] {
        let mut sk = Sketch::new(5, 1024, 1);
        let row = build_dense(n, 10, 42);
        sk.add_row(&row, 1.0);
        assert_eq!(sk.size(), 5120, "n={} 时大小变了", n);
    }
}

#[test]
fn sparse_error_independent_of_n() {
    // 稀疏：每人对 10 个账户有强评分，其余中性
    let mut prev = 0.0f64;
    for &n in &[100usize, 1_000, 10_000] {
        let row = build_dense(n, 10, 7);
        let mut sk = Sketch::new(5, 1024, 2);
        sk.add_row(&row, 1.0);

        // 只统计被抽中的那 10 个，误差应该稳定
        let mut err_sum = 0.0;
        let mut cnt = 0;
        for (u, &v) in row.iter().enumerate() {
            if (v - 0.5).abs() > 1e-9 {
                err_sum += (sk.query(u) - v).abs();
                cnt += 1;
            }
        }
        let mae = err_sum / cnt as f64;
        // 允许小幅波动
        assert!(
            mae < 0.05,
            "n={} 稀疏误差过大: {}",
            n,
            mae
        );
        prev = mae;
    }
    let _ = prev;
}

#[test]
fn dense_error_grows_with_n() {
    // 稠密：每个人都对所有账户有评分
    // 总质量随 N 增长，误差也随之增长
    let mut errors = Vec::new();
    for &n in &[100usize, 1_000, 10_000] {
        let row = build_full(n, 9);
        let mut sk = Sketch::new(5, 1024, 3);
        sk.add_row(&row, 1.0);

        let mut err_sum = 0.0;
        for (u, &v) in row.iter().enumerate() {
            err_sum += (sk.query(u) - v).abs();
        }
        errors.push(err_sum / n as f64);
    }
    // 稠密时误差随 N 增长，这是 sketch 的固有限制
    assert!(
        errors[2] > errors[0] * 2.0,
        "稠密场景误差应当明显增长: {:?}",
        errors
    );
}
