//! 范围化系统：抽样、合并、打分。
//!
//! 与打分系统的区别：
//!
//! - 打分系统（`local_push`）在完整图上算 Π[s]，输入是整张图。
//! - 范围化系统只抽样一部分路径，得到可以合并的「范围」，从范围里对任意账户打分。
//!
//! # 原理
//!
//! Π[s][u] 等于「从 s 出发、每步以 β 概率停止的随机游走，终点 v 的 P[v][u] 的期望」。
//!
//! 证明：设 G 是几何分布的 Green 函数，
//! G[s][v] = Σ_t β(1-β)^t · W^t[s][v]，
//! 则 Π = β(I-(1-β)W)⁻¹P = G·P。
//!
//! 所以用 k 次独立游走的终点做蒙特卡洛估计：
//!
//! ```text
//! Π[s][u] ≈ (1/k) · Σ_i P[v_i][u]
//! ```
//!
//! 误差与 1/√k 成正比，与网络规模无关。这就是「一次抽样，任意查询」的数学基础。

use crate::graph::Graph;
use crate::rng::Rng;

/// 一次抽样得到的范围：一批节点，每个节点代表一次独立游走的终点。
///
/// 同一个节点可以被多次命中，权重表示命中次数，合并时直接相加。
#[derive(Clone, Debug)]
pub struct Scope {
    pub nodes: Vec<usize>,
    pub weights: Vec<f64>,
    /// 游走次数，用于计算均值
    pub total: f64,
}

impl Scope {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            weights: Vec::new(),
            total: 0.0,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// 从观察者 s 出发抽样 k 条路径。
///
/// 每条路径：每步以 β 概率停止，否则按边权重随机走一步。
/// 停止时记录终点，权重为 1。
pub fn sample(g: &Graph, s: usize, k: usize, beta: f64, rng: &mut Rng) -> Scope {
    assert!(s < g.n);
    let mut nodes = Vec::with_capacity(k);
    let mut weights = Vec::with_capacity(k);

    for _ in 0..k {
        let mut v = s;
        // 防止在自环或异常图上死循环
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            if rng.chance(beta) {
                break;
            }
            let nbrs = g.neighbors(v);
            if nbrs.is_empty() {
                break;
            }
            // 按权重选一个邻居
            let total: f64 = nbrs.iter().map(|(_, w)| w).sum();
            if total <= 0.0 {
                break;
            }
            let mut r = rng.next_f64() * total;
            let mut chosen = nbrs[0].0;
            for &(x, w) in nbrs {
                if r < w {
                    chosen = x;
                    break;
                }
                r -= w;
            }
            v = chosen;
        }
        nodes.push(v);
        weights.push(1.0);
    }

    Scope {
        nodes,
        weights,
        total: k as f64,
    }
}

/// 合并两个范围，按权重线性叠加。
///
/// 这样「自己的抽样」和「从别人那里收集的抽样」可以合成一份。
pub fn merge(a: &Scope, b: &Scope, wa: f64, wb: f64) -> Scope {
    let mut nodes = Vec::with_capacity(a.nodes.len() + b.nodes.len());
    let mut weights = Vec::with_capacity(a.weights.len() + b.weights.len());

    for (n, w) in a.nodes.iter().zip(a.weights.iter()) {
        nodes.push(*n);
        weights.push(w * wa);
    }
    for (n, w) in b.nodes.iter().zip(b.weights.iter()) {
        nodes.push(*n);
        weights.push(w * wb);
    }

    Scope {
        nodes,
        weights,
        total: a.total * wa + b.total * wb,
    }
}

/// 从范围中对账户 u 打分。
///
/// 就是范围里所有节点对 u 的评分的加权平均。
pub fn score(scope: &Scope, g: &Graph, u: usize) -> f64 {
    if scope.nodes.is_empty() {
        return 0.5;
    }
    let mut sum = 0.0;
    let mut wsum = 0.0;
    for (n, w) in scope.nodes.iter().zip(scope.weights.iter()) {
        sum += w * g.rating(*n, u);
        wsum += w;
    }
    if wsum <= 0.0 {
        0.5
    } else {
        sum / wsum
    }
}

/// 从范围中对一组账户打分，返回向量。
pub fn score_all(scope: &Scope, g: &Graph) -> Vec<f64> {
    (0..g.n).map(|u| score(scope, g, u)).collect()
}

// ============================================================
// 模糊化之后的可发布形式
// ============================================================
//
// Scope 里的节点 ID 会泄露「你游走到了谁」，等价于泄露关注关系。
// 所以对外只发布「分数向量」，不发布节点列表。
//
// 为了掩盖稀疏性（如果只对少数人有非中性评分，分数向量本身会暴露关系），
// 加拉普拉斯噪声。
//
// 加噪和合并都是线性的，所以合并后的结果仍然正确：
//
//   merge(y1 + n1, y2 + n2) = (y1 + y2) + (n1 + n2)
//
// 噪声方差相加，信号也相加，合并 k 份后信噪比提升 √k。

/// 可发布的模糊化范围：只有分数向量，没有节点列表。
#[derive(Clone, Debug)]
pub struct PublishedScope {
    /// 加噪后的分数向量，长度 n
    pub scores: Vec<f64>,
    /// 隐私预算，越小噪声越大
    pub epsilon: f64,
    /// 参与的抽样次数，合并时用于加权
    pub weight: f64,
}

/// 把 Scope 转成可发布形式。
///
/// 先按权重归一，再逐维加拉普拉斯噪声。
pub fn publish(scope: &Scope, g: &Graph, epsilon: f64, rng: &mut Rng) -> PublishedScope {
    assert!(epsilon > 0.0);
    let n = g.n;
    let mut scores = vec![0.0; n];

    let wsum: f64 = scope.weights.iter().sum();
    if wsum <= 0.0 {
        return PublishedScope {
            scores: vec![0.5; n],
            epsilon,
            weight: 0.0,
        };
    }

    for (v, w) in scope.nodes.iter().zip(scope.weights.iter()) {
        let row = g.rating_row(*v);
        for u in 0..n {
            scores[u] += w * row[u];
        }
    }
    for s in scores.iter_mut() {
        *s /= wsum;
    }

    // 拉普拉斯噪声，尺度 1/epsilon
    let scale = 1.0 / epsilon;
    for s in scores.iter_mut() {
        *s += laplace(rng, scale);
    }

    PublishedScope {
        scores,
        epsilon,
        weight: wsum,
    }
}

/// 合并两份可发布范围。线性加权。
pub fn merge_published(a: &PublishedScope, b: &PublishedScope, wa: f64, wb: f64) -> PublishedScope {
    assert_eq!(a.scores.len(), b.scores.len());
    let total = wa + wb;
    let inv = if total > 0.0 { 1.0 / total } else { 0.0 };
    let mut scores = vec![0.0; a.scores.len()];
    for i in 0..scores.len() {
        scores[i] = (a.scores[i] * wa + b.scores[i] * wb) * inv;
    }
    PublishedScope {
        scores,
        epsilon: a.epsilon.min(b.epsilon),
        weight: total,
    }
}

/// 从可发布范围中对账户 u 打分。
pub fn query(p: &PublishedScope, u: usize) -> f64 {
    p.scores.get(u).copied().unwrap_or(0.5)
}

/// 拉普拉斯采样，尺度 scale。
fn laplace(rng: &mut Rng, scale: f64) -> f64 {
    let u = rng.next_f64() - 0.5;
    let sign = if u < 0.0 { -1.0 } else { 1.0 };
    -scale * sign * (1.0 - 2.0 * u.abs()).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scope_neutral() {
        let g = Graph::new(3);
        let s = Scope::empty();
        assert!((score(&s, &g, 0) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn merge_weights() {
        let a = Scope {
            nodes: vec![0],
            weights: vec![1.0],
            total: 1.0,
        };
        let b = Scope {
            nodes: vec![1],
            weights: vec![1.0],
            total: 1.0,
        };
        let m = merge(&a, &b, 1.0, 1.0);
        assert_eq!(m.len(), 2);
        assert!((m.total - 2.0).abs() < 1e-12);
    }
}
