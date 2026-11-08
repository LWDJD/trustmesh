//! 精确解：直接解线性方程组。
//!
//! Π = β·P + (1-β)·W·Π
//!   ⇔ (I - (1-β)·W)·Π = β·P
//!
//! 复杂度 O(N³)，只用于小规模验证和正确性基准。

use crate::dense::Mat;
use crate::graph::Graph;

/// 求解 Π，返回 N×N 矩阵，第 s 行是观察者 s 视角下的分数向量。
pub fn solve_exact(g: &Graph, beta: f64) -> Vec<Vec<f64>> {
    assert!((0.0..1.0).contains(&beta), "beta 必须在 [0,1) 内");

    let n = g.n;

    // W：行归一的邻接矩阵
    let mut w = Mat::zeros(n, n);
    for u in 0..n {
        for &(v, wt) in g.neighbors(u) {
            w.set(u, v, wt);
        }
    }

    // M = I - (1-β)·W
    let m = Mat::identity(n).sub(&w.scale(1.0 - beta));

    // B = β·P
    let mut p = Mat::zeros(n, n);
    for s in 0..n {
        for u in 0..n {
            p.set(s, u, g.rating(s, u));
        }
    }
    let b = p.scale(beta);

    // Π = M⁻¹·B
    let pi = Mat::solve(&m, &b);

    let mut out = vec![vec![0.0; n]; n];
    for s in 0..n {
        for u in 0..n {
            out[s][u] = pi.get(s, u);
        }
    }
    out
}

/// 不动点方程的残差：max |Π - (βP + (1-β)WΠ)|。
///
/// 精确解应当接近 0；近似解应当接近 eps 量级。
pub fn residual(g: &Graph, pi: &[Vec<f64>], beta: f64) -> f64 {
    let n = g.n;
    let mut max_err: f64 = 0.0;

    for s in 0..n {
        for u in 0..n {
            // 右边 = β·P[s][u] + (1-β)·Σ_v W[s][v]·Π[v][u]
            let mut rhs = beta * g.rating(s, u);
            for &(v, w) in g.neighbors(s) {
                rhs += (1.0 - beta) * w * pi[v][u];
            }
            let err = (pi[s][u] - rhs).abs();
            if err > max_err {
                max_err = err;
            }
        }
    }

    max_err
}

/// 计算 Π 的整体质量（行和）用于自检。
pub fn row_mass(pi: &[Vec<f64>]) -> Vec<f64> {
    pi.iter().map(|row| row.iter().sum()).collect()
}
