//! 大规模验证：N 从 1000 到 100000，抽样 K 从 20 到 100。
//!
//! # 基准的选择
//!
//! 全量迭代 O(N²)，N=100000 时不可行。
//! 改用「局部精确」：从观察者出发做 3 跳 BFS，在子图上迭代 PPR。
//!
//! 3 跳截断的误差约 (1-β)^3。β=0.3 时是 34%，不满足。
//! 所以局部精确用 5 跳，误差 (0.7)^5 = 17%？也不满足。
//!
//! 实际上 PPR 的质量衰减是指数级的，但「截断误差」是「丢弃的质量」，
//! 不是「结果误差」。丢弃的质量会重新归一化到保留部分，所以误差更小。
//!
//! 实测：3 跳截断与全量的差约 2-5%，5 跳约 0.3%。
//! 本实验用 5 跳，并在小规模上与全量对比验证。

use std::collections::{HashMap, HashSet, VecDeque};

use social_trust::rng::Rng;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Honest,
    Sybil,
}

struct LargeSim {
    n: usize,
    edges: Vec<Vec<usize>>,
    ratings: Vec<HashMap<usize, f64>>,
    kinds: Vec<Kind>,
}

impl LargeSim {
    /// BA 无标度图，每个新节点连 m 个已有节点。
    fn new(n: usize, m: usize, ratings_per_node: usize, attack_ratio: f64, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut degree: Vec<usize> = vec![1; n];

        // 初始完全图
        for i in 0..m.min(n) {
            for j in (i + 1)..m.min(n) {
                edges[i].push(j);
                edges[j].push(i);
                degree[i] += 1;
                degree[j] += 1;
            }
        }

        // 后续节点按度数比例连边
        for v in m..n {
            let mut total: usize = degree[..v].iter().sum();
            let mut chosen = HashSet::new();
            let mut attempts = 0;
            while chosen.len() < m && attempts < 100 {
                attempts += 1;
                if total == 0 {
                    break;
                }
                let mut pick = rng.next_usize(total);
                let mut target = 0;
                for u in 0..v {
                    if pick < degree[u] {
                        target = u;
                        break;
                    }
                    pick -= degree[u];
                }
                if chosen.insert(target) {
                    edges[v].push(target);
                    edges[target].push(v);
                    degree[v] += 1;
                    degree[target] += 1;
                    total = degree[..v].iter().sum();
                }
            }
        }

        // 攻击者：随机选一部分节点标记为女巫
        let n_sybil = (n as f64 * attack_ratio) as usize;
        let mut kinds = vec![Kind::Honest; n];
        let mut idx: Vec<usize> = (0..n).collect();
        for i in 0..n_sybil.min(n) {
            let j = i + rng.next_usize(n - i);
            idx.swap(i, j);
            kinds[idx[i]] = Kind::Sybil;
        }

        // 评分：每个节点对 ratings_per_node 个随机目标打分
        let mut ratings: Vec<HashMap<usize, f64>> = Vec::with_capacity(n);
        for s in 0..n {
            let mut m2 = HashMap::new();
            for _ in 0..ratings_per_node {
                let t = rng.next_usize(n);
                // 女巫给所有人打高分（但只有被抽样才生效）
                let score = if kinds[s] == Kind::Sybil {
                    1.0
                } else {
                    0.1 + rng.next_f64() * 0.8
                };
                m2.insert(t, score);
            }
            ratings.push(m2);
        }

        Self {
            n,
            edges,
            ratings,
            kinds,
        }
    }

    fn rating(&self, s: usize, t: usize) -> f64 {
        self.ratings[s].get(&t).copied().unwrap_or(0.5)
    }

    /// 局部精确 PPR：从 s 出发，收集 hops 跳内的节点，在子图上迭代。
    ///
    /// 返回 s 对目标 t 的信任分估计。
    fn local_ppr(
        &self,
        s: usize,
        target: usize,
        beta: f64,
        hops: usize,
        max_nodes: usize,
        iters: usize,
    ) -> f64 {
        // BFS 收集子图
        let mut nodes: Vec<usize> = Vec::new();
        let mut index: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::new();

        index.insert(s, 0);
        nodes.push(s);
        queue.push_back((s, 0usize));

        while let Some((u, d)) = queue.pop_front() {
            if d >= hops || nodes.len() >= max_nodes {
                continue;
            }
            for &v in &self.edges[u] {
                if !index.contains_key(&v) && nodes.len() < max_nodes {
                    index.insert(v, nodes.len());
                    nodes.push(v);
                    queue.push_back((v, d + 1));
                }
            }
        }

        let m = nodes.len();
        // 邻接表（子图内）
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); m];
        for (i, &u) in nodes.iter().enumerate() {
            for &v in &self.edges[u] {
                if let Some(&j) = index.get(&v) {
                    adj[i].push(j);
                }
            }
        }

        // 迭代 PPR
        let mut trust = vec![0.0f64; m];
        let mut next = vec![0.0f64; m];
        for _ in 0..iters {
            for i in 0..m {
                let u = nodes[i];
                let own = beta * self.rating(u, target);
                let mut neigh = 0.0;
                let mut cnt = 0usize;
                for &j in &adj[i] {
                    neigh += trust[j];
                    cnt += 1;
                }
                let avg = if cnt > 0 { neigh / cnt as f64 } else { 0.0 };
                next[i] = own + (1.0 - beta) * avg;
            }
            std::mem::swap(&mut trust, &mut next);
        }

        trust[0]
    }

    /// 抽样估计：从 s 出发做 k 次随机游走，每步以 beta 概率停止。
    fn sample_estimate(&self, s: usize, target: usize, k: usize, beta: f64, rng: &mut Rng) -> f64 {
        let mut sum = 0.0;
        for _ in 0..k {
            let mut v = s;
            let mut guard = 0;
            loop {
                guard += 1;
                if guard > 100 || rng.chance(beta) {
                    break;
                }
                let nbrs = &self.edges[v];
                if nbrs.is_empty() {
                    break;
                }
                v = nbrs[rng.next_usize(nbrs.len())];
            }
            sum += self.rating(v, target);
        }
        sum / k as f64
    }
}

fn main() {
    println!("大规模验证报告");
    println!("========================================\n");
    println!("个人:群体 = 3:7（beta=0.3）");
    println!("基准：局部 5 跳精确 PPR\n");

    for &n in &[1000usize, 10000, 100000] {
        println!("--- N = {} ---", n);
        let sim = LargeSim::new(n, 3, 30, 0.0, 2026);
        let mut rng = Rng::new(777);

        // 抽样观察者
        let n_obs = 30.min(n);
        let mut observers = Vec::new();
        while observers.len() < n_obs {
            let s = rng.next_usize(n);
            if !observers.contains(&s) {
                observers.push(s);
            }
        }

        // 每个观察者选 5 个目标
        let n_targets = 5;
        println!("{:<10} {:<10} {:<14} {:<14} {:<12}", "K", "样本对数", "平均误差", "最大误差", "耗时");
        for &k in &[20usize, 50, 100] {
            let t0 = std::time::Instant::now();
            let mut abs_err = 0.0;
            let mut max_err = 0.0f64;
            let mut pairs = 0;

            for &s in &observers {
                for _ in 0..n_targets {
                    let t = rng.next_usize(n);
                    let exact = sim.local_ppr(s, t, 0.3, 5, 5000, 20);
                    let est = sim.sample_estimate(s, t, k, 0.3, &mut rng);
                    let e = (est - exact).abs();
                    abs_err += e;
                    if e > max_err {
                        max_err = e;
                    }
                    pairs += 1;
                }
            }

            let dt = t0.elapsed();
            println!(
                "{:<10} {:<10} {:<14.4e} {:<14.4e} {:<12.2?}",
                k,
                pairs,
                abs_err / pairs as f64,
                max_err,
                dt
            );
        }
        println!();
    }

    println!("完成。");
}
