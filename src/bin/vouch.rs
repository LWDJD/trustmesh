//! 介绍人认证传播：新人如何获得信任。
//!
//! # 设计
//!
//! 1. 新人 N 由介绍人 V 认证，V 给 N 打 1.0
//! 2. V 的信任者收到信号，N 的 trust 上升
//! 3. 信号沿信任图衰减，距离越远越弱
//!
//! # 测量
//!
//! 对每个观察者 s，计算 s 到 V 的最短距离，统计 s 对 N 的 trust。

use std::collections::{HashMap, VecDeque};

use trustmesh::rng::Rng;

const N: usize = 500;
const BETA: f64 = 0.3;

struct Net {
    edges: Vec<Vec<usize>>,
    ratings: Vec<HashMap<usize, f64>>,
}

impl Net {
    fn build(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let mut edges = vec![Vec::new(); N];
        let mut degree = vec![1usize; N];

        // 小世界：环形 + 少量长边
        for i in 0..N {
            for d in 1..=3 {
                let j = (i + d) % N;
                edges[i].push(j);
                edges[j].push(i);
                degree[i] += 1;
                degree[j] += 1;
            }
            if rng.chance(0.05) {
                let j = rng.next_usize(N);
                if j != i {
                    edges[i].push(j);
                    edges[j].push(i);
                }
            }
        }

        // 评分：对邻居打中性偏正分
        let mut ratings = vec![HashMap::new(); N];
        for s in 0..N {
            for &v in &edges[s] {
                let p = 0.4 + rng.next_f64() * 0.3;
                ratings[s].insert(v, p);
            }
        }

        Self { edges, ratings }
    }

    fn rating(&self, s: usize, t: usize) -> f64 {
        self.ratings[s].get(&t).copied().unwrap_or(0.5)
    }

    fn solve(&self, iters: usize) -> Vec<Vec<f64>> {
        let mut trust = vec![vec![0.0; N]; N];
        for _ in 0..iters {
            let mut next = vec![vec![0.0; N]; N];
            for s in 0..N {
                let mut pool: Vec<(usize, f64)> = Vec::new();
                for (&v, &p) in self.ratings[s].iter() {
                    if p > 0.0 {
                        pool.push((v, p));
                    }
                }
                let total: f64 = pool.iter().map(|(_, p)| p).sum();
                for u in 0..N {
                    let mut val = BETA * self.rating(s, u);
                    if total > 0.0 {
                        for &(v, p) in &pool {
                            val += (1.0 - BETA) * (p / total) * trust[v][u];
                        }
                    }
                    next[s][u] = val;
                }
            }
            trust = next;
        }
        trust
    }

    /// 从 start 出发的 BFS 距离。
    fn distances(&self, start: usize) -> Vec<usize> {
        let mut dist = vec![usize::MAX; N];
        let mut q = VecDeque::new();
        dist[start] = 0;
        q.push_back(start);
        while let Some(u) = q.pop_front() {
            for &v in &self.edges[u] {
                if dist[v] == usize::MAX {
                    dist[v] = dist[u] + 1;
                    q.push_back(v);
                }
            }
        }
        dist
    }
}

fn main() {
    println!("介绍人认证传播实验");
    println!("========================================\n");
    println!("N={}, β={}（个人 {:.0} / 群体 {:.0}）\n", N, BETA, BETA * 10.0, (1.0 - BETA) * 10.0);

    let mut net = Net::build(2026);

    // 选 10 个新人，每个分配一个介绍人
    let mut rng = Rng::new(99);
    let mut newcomers = Vec::new();
    let mut vouchers = Vec::new();
    for _ in 0..10 {
        let n = rng.next_usize(N);
        let v = rng.next_usize(N);
        if n != v && !newcomers.contains(&n) && n != v {
            newcomers.push(n);
            vouchers.push(v);
        }
    }

    // 介绍人给新人打 1.0（认证）
    for (i, &n) in newcomers.iter().enumerate() {
        let v = vouchers[i];
        net.ratings[v].insert(n, 1.0);
    }

    let t0 = std::time::Instant::now();
    let trust = net.solve(20);
    println!("迭代 20 轮，耗时 {:?}\n", t0.elapsed());

    // 统计：按距离分桶
    println!("{:<10} {:<18} {:<16} {:<14}", "距离", "观察者数", "新人平均信任", "相对基准");
    let mut by_dist: HashMap<usize, Vec<f64>> = HashMap::new();

    for (i, &n) in newcomers.iter().enumerate() {
        let v = vouchers[i];
        let dist = net.distances(v);
        for s in 0..N {
            if s == n || s == v {
                continue;
            }
            let d = dist[s];
            if d < usize::MAX {
                by_dist.entry(d).or_default().push(trust[s][n]);
            }
        }
    }

    let baseline = 0.5;
    let mut keys: Vec<usize> = by_dist.keys().copied().collect();
    keys.sort();
    for k in keys {
        let vals = &by_dist[&k];
        if vals.len() < 5 {
            continue;
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let rel = (mean - baseline) / baseline * 100.0;
        println!(
            "{:<10} {:<18} {:<16.4} {:<14.1}%",
            k,
            vals.len(),
            mean,
            rel
        );
    }

    println!("\n说明：距离 = 观察者到介绍人的最短路径长度");
}
