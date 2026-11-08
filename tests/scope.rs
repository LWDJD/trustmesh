//! 范围化系统验证。
//!
//! 验证三件事：
//! 1. 抽样估计能逼近精确解，误差随样本量下降。
//! 2. 合并后的范围仍能正确打分。
//! 3. 加噪后不泄露节点，但分数仍可用。

use trustmesh::graph::Graph;
use trustmesh::local_push::{solve_local, LocalPushParams};
use trustmesh::rng::Rng;
use trustmesh::scope::{merge, merge_published, publish, query, sample, score};
use trustmesh::sim;

fn make_graph(n: usize, seed: u64) -> Graph {
    let mut rng = Rng::new(seed);
    let mut g = sim::gen_ba(n, 3, &mut rng);
    sim::random_ratings(&mut g, 8, &mut rng);
    g
}

fn max_err(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f64, f64::max)
}

#[test]
fn sampling_converges_to_exact() {
    let n = 60;
    let beta = 0.7;
    let g = make_graph(n, 1001);

    // 基准：打分系统的结果
    let params = LocalPushParams {
        beta,
        eps: 1e-5,
        max_iter: 1_000_000,
    };
    let exact = solve_local(&g, 0, &params);

    let mut prev = f64::INFINITY;
    for &k in &[100usize, 400, 1600, 6400] {
        let mut rng = Rng::new(2000 + k as u64);
        let sc = sample(&g, 0, k, beta, &mut rng);
        let est: Vec<f64> = (0..n).map(|u| score(&sc, &g, u)).collect();
        let err = max_err(&est, &exact);
        assert!(
            err <= prev + 0.02,
            "k={} 误差 {} 没有下降（上一次 {}）",
            k,
            err,
            prev
        );
        prev = err;
    }

    // 大样本时误差应当很小
    assert!(prev < 0.05, "k=6400 误差仍然很大: {}", prev);
}

#[test]
fn merged_scope_scores_correctly() {
    let n = 60;
    let beta = 0.7;
    let g = make_graph(n, 1002);

    let params = LocalPushParams {
        beta,
        eps: 1e-5,
        max_iter: 1_000_000,
    };
    let exact = solve_local(&g, 0, &params);

    // 两份独立抽样，各 2000 次
    let mut r1 = Rng::new(3001);
    let mut r2 = Rng::new(3002);
    let a = sample(&g, 0, 2000, beta, &mut r1);
    let b = sample(&g, 0, 2000, beta, &mut r2);

    let merged = merge(&a, &b, 1.0, 1.0);
    assert_eq!(merged.len(), 4000);

    let est: Vec<f64> = (0..n).map(|u| score(&merged, &g, u)).collect();
    let err = max_err(&est, &exact);
    assert!(err < 0.06, "合并后误差过大: {}", err);
}

#[test]
fn publish_hides_nodes_but_keeps_scores() {
    let n = 40;
    let beta = 0.7;
    let g = make_graph(n, 1003);

    let mut rng = Rng::new(4001);
    let sc = sample(&g, 0, 4000, beta, &mut rng);

    // 低噪声，分数应接近抽样结果
    let low_noise = publish(&sc, &g, 100.0, &mut rng);
    let raw: Vec<f64> = (0..n).map(|u| score(&sc, &g, u)).collect();
    let err_low = max_err(&low_noise.scores, &raw);
    assert!(err_low < 0.08, "低噪声时误差过大: {}", err_low);

    // 高噪声，误差明显增大，但仍应落在合理范围
    let high_noise = publish(&sc, &g, 0.5, &mut rng);
    let err_high = max_err(&high_noise.scores, &raw);
    assert!(err_high > err_low, "噪声应当让误差变大");

    // 发布形式不含节点 ID
    assert_eq!(high_noise.scores.len(), n);
}

#[test]
fn merged_published_still_works() {
    let n = 40;
    let beta = 0.7;
    let g = make_graph(n, 1004);

    let params = LocalPushParams {
        beta,
        eps: 1e-5,
        max_iter: 1_000_000,
    };
    let exact = solve_local(&g, 0, &params);

    let mut rng = Rng::new(5001);
    let a = publish(&sample(&g, 0, 4000, beta, &mut rng), &g, 100.0, &mut rng);
    let b = publish(&sample(&g, 0, 4000, beta, &mut rng), &g, 100.0, &mut rng);

    let merged = merge_published(&a, &b, 1.0, 1.0);
    let err = max_err(&merged.scores, &exact);
    assert!(err < 0.10, "合并发布后误差过大: {}", err);

    // 任意查询
    for u in 0..n {
        let v = query(&merged, u);
        assert!(v.is_finite());
    }
}

#[test]
fn epsilon_controls_noise() {
    let n = 30;
    let beta = 0.7;
    let g = make_graph(n, 1005);

    let mut rng = Rng::new(6001);
    let sc = sample(&g, 0, 2000, beta, &mut rng);
    let raw: Vec<f64> = (0..n).map(|u| score(&sc, &g, u)).collect();

    let mut prev = 0.0f64;
    // epsilon 越小，噪声越大
    for &eps in &[10.0, 1.0, 0.5, 0.1] {
        let p = publish(&sc, &g, eps, &mut rng);
        let err = max_err(&p.scores, &raw);
        assert!(
            err >= prev - 0.02,
            "eps={} 误差 {} 没有随噪声增大",
            eps,
            err
        );
        prev = err;
    }
}
