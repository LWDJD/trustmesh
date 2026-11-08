//! 近似解：Local Push。
//!
//! 对单个观察者 s，计算 Π[s]（即 s 视角下所有人的分数）。
//!
//! 原理：把 Π[s] 看成从 s 出发的 Personalized PageRank 分布作用在 P 上。
//!
//! ```text
//! Π[s][u] = β · Σ_v G[s][v] · P[v][u]
//! ```
//!
//! 其中 G = (I - (1-β)W)⁻¹ 是 Green 函数，用 push 迭代近似。
//!
//! 每次从残差最大的节点 v 推送：
//! - 把 β·r[v]·P[v] 加进结果
//! - 把 (1-β)·r[v]·W[v] 分给邻居
//!
//! 复杂度 O(1/eps)，与图的规模无关。

use crate::graph::Graph;

#[derive(Clone, Debug)]
pub struct LocalPushParams {
    /// 个人权重 β，取值 [0,1)
    pub beta: f64,
    /// 残差阈值，越小越精确
    pub eps: f64,
    /// 最大迭代次数，防止不收敛时死循环
    pub max_iter: usize,
}

impl Default for LocalPushParams {
    fn default() -> Self {
        Self {
            beta: 0.7,
            eps: 1e-3,
            max_iter: 50_000,
        }
    }
}

/// 返回 Π[s]，长度为 n。
pub fn solve_local(g: &Graph, s: usize, params: &LocalPushParams) -> Vec<f64> {
    assert!(s < g.n);
    assert!((0.0..1.0).contains(&params.beta));

    let n = g.n;
    let alpha = 1.0 - params.beta;

    let mut pi = vec![0.0; n];
    let mut r = vec![0.0; n];
    r[s] = 1.0;

    for _ in 0..params.max_iter {
        // 找残差最大的节点
        let mut v = usize::MAX;
        let mut best = 0.0;
        for i in 0..n {
            if r[i] > best {
                best = r[i];
                v = i;
            }
        }

        if v == usize::MAX || best < params.eps {
            break;
        }

        // 贡献到结果：β·r[v]·P[v][*]
        let rv = r[v];
        let base = params.beta * rv;
        let row = g.rating_row(v);
        for u in 0..n {
            pi[u] += base * row[u];
        }

        // 传播残差：(1-β)·r[v]·W[v][*]
        let spread = alpha * rv;
        for &(x, w) in g.neighbors(v) {
            r[x] += spread * w;
        }

        r[v] = 0.0;
    }

    // 未处理完的残差补给结果，保证质量守恒
    for v in 0..n {
        if r[v] > 0.0 {
            let base = params.beta * r[v];
            let row = g.rating_row(v);
            for u in 0..n {
                pi[u] += base * row[u];
            }
        }
    }

    pi
}

/// 只取单个目标 u 的分数，跳过其余计算。
///
/// 与 `solve_local(g,s,..)[u]` 结果相同，但当 u 只有一个时省一点内存。
pub fn solve_local_single(g: &Graph, s: usize, u: usize, params: &LocalPushParams) -> f64 {
    assert!(s < g.n && u < g.n);
    let n = g.n;
    let alpha = 1.0 - params.beta;

    let mut acc = 0.0;
    let mut r = vec![0.0; n];
    r[s] = 1.0;

    for _ in 0..params.max_iter {
        let mut v = usize::MAX;
        let mut best = 0.0;
        for i in 0..n {
            if r[i] > best {
                best = r[i];
                v = i;
            }
        }
        if v == usize::MAX || best < params.eps {
            break;
        }

        acc += params.beta * r[v] * g.rating(v, u);
        let spread = alpha * r[v];
        for &(x, w) in g.neighbors(v) {
            r[x] += spread * w;
        }
        r[v] = 0.0;
    }

    for v in 0..n {
        if r[v] > 0.0 {
            acc += params.beta * r[v] * g.rating(v, u);
        }
    }

    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node() {
        let mut g = Graph::new(1);
        g.finalize();
        g.set_rating(0, 0, 0.9);
        let p = LocalPushParams::default();
        let pi = solve_local(&g, 0, &p);
        // 孤立节点没有群体输入，结果 = β·P[0][0] = 0.7 × 0.9
        assert!((pi[0] - 0.63).abs() < 1e-6);
    }
}
