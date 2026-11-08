//! 模拟用的图生成器。
//!
//! 三种结构，覆盖不同的社交网络形态：
//! - BA：无标度，少数枢纽节点连接大量人（真实社交网络的典型）
//! - WS：小世界，局部密集加少量长边（熟人社会）
//! - ER：随机，没有结构（对照组）

use crate::graph::Graph;
use crate::rng::Rng;

/// Barabási–Albert 无标度图。
///
/// 每次加入新节点，按度数比例连接 m 个已有节点。
pub fn gen_ba(n: usize, m: usize, rng: &mut Rng) -> Graph {
    assert!(n > m && m >= 1);
    let mut g = Graph::new(n);

    // 初始完全图
    for i in 0..m {
        for j in (i + 1)..m {
            g.add_edge(i, j, 1.0);
            g.add_edge(j, i, 1.0);
        }
    }

    // 度数列表，用于加权抽样
    let mut degree: Vec<usize> = vec![1; n];
    for i in 0..m {
        degree[i] = m;
    }

    for v in m..n {
        let mut chosen = Vec::new();
        let mut total: usize = degree[..v].iter().sum();

        while chosen.len() < m.min(v) {
            let mut pick = rng.next_usize(total.max(1));
            let mut target = 0;
            for u in 0..v {
                if pick < degree[u] {
                    target = u;
                    break;
                }
                pick -= degree[u];
            }
            if !chosen.contains(&target) {
                chosen.push(target);
                g.add_edge(v, target, 1.0);
                g.add_edge(target, v, 1.0);
                degree[v] += 1;
                degree[target] += 1;
                total = degree[..v].iter().sum();
            }
        }
    }

    g.finalize();
    g
}

/// Watts–Strogatz 小世界图。
///
/// 环形格子，每个节点连 k 个近邻，以概率 p 重连。
pub fn gen_ws(n: usize, k: usize, p: f64, rng: &mut Rng) -> Graph {
    assert!(k % 2 == 0 && k < n);
    let mut g = Graph::new(n);
    let half = k / 2;

    for u in 0..n {
        for d in 1..=half {
            let v = (u + d) % n;
            g.add_edge(u, v, 1.0);
            g.add_edge(v, u, 1.0);
        }
    }

    // 重连
    for u in 0..n {
        for d in 1..=half {
            if !rng.chance(p) {
                continue;
            }
            let old = (u + d) % n;
            let new = rng.next_usize(n);
            if new != u && new != old {
                // 简化：直接加边，不删旧边（保持连通性）
                g.add_edge(u, new, 1.0);
                g.add_edge(new, u, 1.0);
            }
        }
    }

    g.finalize();
    g
}

/// Erdős–Rényi 随机图。
pub fn gen_er(n: usize, avg_degree: usize, rng: &mut Rng) -> Graph {
    let mut g = Graph::new(n);
    let p = (avg_degree as f64 / n as f64).min(1.0);
    for u in 0..n {
        for v in 0..n {
            if u != v && rng.chance(p) {
                g.add_edge(u, v, 1.0);
            }
        }
    }
    g.finalize();
    g
}

/// 给每个节点生成若干条直接评分。
///
/// 评分服从正态分布，均值 0.6，标准差 0.2，截断到 [0,1]。
pub fn random_ratings(g: &mut Graph, per_node: usize, rng: &mut Rng) {
    let n = g.n;
    for s in 0..n {
        for _ in 0..per_node {
            let u = rng.next_usize(n);
            let x = normal(rng, 0.6, 0.2);
            g.set_rating(s, u, x.clamp(0.0, 1.0));
        }
    }
}

/// Box–Muller 生成正态分布。
pub fn normal(rng: &mut Rng, mu: f64, sigma: f64) -> f64 {
    let u1 = rng.next_f64().max(1e-12);
    let u2 = rng.next_f64();
    let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
    mu + sigma * z
}
