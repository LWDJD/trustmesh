//! 数学验证。
//!
//! 每条测试对应一个数学性质，不是随意的冒烟测试。
//!
//! 1. 精确解满足不动点方程
//! 2. 近似解与精确解一致
//! 3. 有界性：评分在 [0,1] 内，结果也在 [0,1] 内
//! 4. 单调性：提高评分，结果不会下降
//! 5. 极限：β=1 时退化为直接评分
//! 6. 收敛：eps 越小，误差越小
//! 7. 一致性：求解与行顺序无关

use social_trust::exact::{residual, solve_exact};
use social_trust::graph::Graph;
use social_trust::local_push::{solve_local, solve_local_single, LocalPushParams};
use social_trust::rng::Rng;
use social_trust::sim;

/// 构造一个可复现的小图。
fn small_graph() -> Graph {
    let mut rng = Rng::new(20261106);
    let mut g = sim::gen_ba(15, 2, &mut rng);
    sim::random_ratings(&mut g, 5, &mut rng);
    g
}

fn max_diff(a: &[Vec<f64>], b: &[Vec<f64>]) -> f64 {
    let mut m = 0.0f64;
    for i in 0..a.len() {
        for j in 0..a[i].len() {
            let d = (a[i][j] - b[i][j]).abs();
            if d > m {
                m = d;
            }
        }
    }
    m
}

#[test]
fn exact_satisfies_fixed_point() {
    let g = small_graph();
    let beta = 0.7;
    let pi = solve_exact(&g, beta);
    let r = residual(&g, &pi, beta);
    assert!(r < 1e-10, "精确解残差过大: {}", r);
}

#[test]
fn local_matches_exact() {
    let g = small_graph();
    let beta = 0.7;
    let exact = solve_exact(&g, beta);

    let params = LocalPushParams {
        beta,
        eps: 1e-4,
        max_iter: 200_000,
    };

    for s in 0..g.n {
        let approx = solve_local(&g, s, &params);
        for u in 0..g.n {
            let err = (approx[u] - exact[s][u]).abs();
            assert!(
                err < 5e-3,
                "s={} u={} 近似 {:.6} 精确 {:.6} 误差 {:.6}",
                s,
                u,
                approx[u],
                exact[s][u],
                err
            );
        }
    }
}

#[test]
fn local_single_matches_row() {
    let g = small_graph();
    let params = LocalPushParams::default();
    let row = solve_local(&g, 3, &params);
    for u in 0..g.n {
        let single = solve_local_single(&g, 3, u, &params);
        assert!((row[u] - single).abs() < 1e-12);
    }
}

#[test]
fn bounded_output() {
    let g = small_graph();
    let beta = 0.7;
    let pi = solve_exact(&g, beta);
    for row in &pi {
        for &v in row {
            assert!(
                (-1e-12..=1.0 + 1e-12).contains(&v),
                "越界: {}",
                v
            );
        }
    }
}

#[test]
fn monotone_in_rating() {
    let mut g = small_graph();
    let beta = 0.7;

    let before = solve_exact(&g, beta);

    // 提高 0 号对所有目标的评分
    for u in 0..g.n {
        let v = g.rating(0, u);
        g.set_rating(0, u, (v + 0.2).min(1.0));
    }
    let after = solve_exact(&g, beta);

    // 0 号自己的分数不应当下降
    for u in 0..g.n {
        assert!(
            after[0][u] >= before[0][u] - 1e-12,
            "单调性破坏: u={} before={} after={}",
            u,
            before[0][u],
            after[0][u]
        );
    }
}

#[test]
fn beta_one_is_identity() {
    // β=1 时，Π = P，群体权重为零
    let g = small_graph();
    let pi = solve_exact(&g, 0.999999);
    for s in 0..g.n {
        for u in 0..g.n {
            let expected = g.rating(s, u);
            assert!(
                (pi[s][u] - expected).abs() < 1e-4,
                "β→1 应退化为直接评分: s={} u={} got={} want={}",
                s,
                u,
                pi[s][u],
                expected
            );
        }
    }
}

#[test]
fn smaller_eps_smaller_error() {
    let g = small_graph();
    let beta = 0.7;
    let exact = solve_exact(&g, beta);

    let mut prev_err = f64::INFINITY;
    for &eps in &[1e-2, 1e-3, 1e-4, 1e-5] {
        let params = LocalPushParams {
            beta,
            eps,
            max_iter: 500_000,
        };
        let approx = solve_local(&g, 0, &params);
        let err = (0..g.n)
            .map(|u| (approx[u] - exact[0][u]).abs())
            .fold(0.0f64, f64::max);
        assert!(
            err <= prev_err + 1e-9,
            "eps={} 时误差 {} 反而大于上一次的 {}",
            eps,
            err,
            prev_err
        );
        prev_err = err;
    }
}

#[test]
fn local_residual_within_bound() {
    let g = small_graph();
    let beta = 0.7;
    let eps = 1e-4;

    let params = LocalPushParams {
        beta,
        eps,
        max_iter: 200_000,
    };

    // 拼成完整矩阵再算残差
    let n = g.n;
    let mut pi = vec![vec![0.0; n]; n];
    for s in 0..n {
        pi[s] = solve_local(&g, s, &params);
    }

    let r = residual(&g, &pi, beta);
    // 残差应显著小于 1，与 eps 同量级
    assert!(r < 1e-3, "近似解残差过大: {}", r);
}

#[test]
fn isolated_node_gets_neutral() {
    let mut g = Graph::new(3);
    g.add_edge(0, 1, 1.0);
    g.add_edge(1, 0, 1.0);
    g.finalize();
    // 节点 2 孤立
    g.set_rating(2, 2, 0.9);

    let pi = solve_exact(&g, 0.7);
    // 孤立节点只受自己影响
    assert!((pi[2][2] - 0.9 * 0.7).abs() < 1e-9);
}

#[test]
fn deterministic_same_seed() {
    let a = small_graph();
    let b = small_graph();
    let pa = solve_exact(&a, 0.7);
    let pb = solve_exact(&b, 0.7);
    assert!(max_diff(&pa, &pb) < 1e-15);
}
