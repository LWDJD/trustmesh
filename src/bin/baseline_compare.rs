//! 严格对比：精确基线 vs 按评分加权抽样。
//!
//! # 正确的模型
//!
//! 每个账户 s 维护一组评分 P[s][v]，v 是「s 评过分的账户」。
//!
//! 信任分：
//!
//! ```text
//! trust[s][u] = β·P[s][u] + (1-β)·Σ_{v: P[s][v]>0} w(s,v)·trust[v][u]
//! w(s,v) = P[s][v] / Σ_{x: P[s][x]>0} P[s][x]
//! ```
//!
//! 精确基线：遍历所有评过分的 v。
//! 抽样估计：从同一个池里按 P[s][v] 概率抽 K 个，按 w(s,v) 加权。
//!
//! 两者用同一份 trust 真值，只差「用了几个样本」。

use std::collections::HashMap;

use social_trust::rng::Rng;

struct Net {
    n: usize,
    edges: Vec<Vec<usize>>,
    ratings: Vec<HashMap<usize, f64>>,
}

impl Net {
    fn new(n: usize, m: usize, rate_ratio: f64, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut degree: Vec<usize> = vec![1; n];

        for i in 0..m.min(n) {
            for j in (i + 1)..m.min(n) {
                edges[i].push(j);
                edges[j].push(i);
                degree[i] += 1;
                degree[j] += 1;
            }
        }

        for v in m..n {
            let mut total: usize = degree[..v].iter().sum();
            let mut chosen = std::collections::HashSet::new();
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

        // 评分：对每个邻居以 rate_ratio 概率评分
        let mut ratings: Vec<HashMap<usize, f64>> = Vec::with_capacity(n);
        for s in 0..n {
            let mut m2 = HashMap::new();
            for &v in &edges[s] {
                if rng.chance(rate_ratio) {
                    m2.insert(v, 0.1 + rng.next_f64() * 0.8);
                }
            }
            ratings.push(m2);
        }

        Self { n, edges, ratings }
    }

    fn rating(&self, s: usize, t: usize) -> f64 {
        self.ratings[s].get(&t).copied().unwrap_or(0.5)
    }

    /// 精确迭代：全量遍历所有评过分的账户。
    fn solve_exact(&self, beta: f64, iters: usize) -> Vec<Vec<f64>> {
        let n = self.n;
        let mut trust = vec![vec!(0.0; n); n];

        // 预先算好每个 s 的评分池和归一化权重
        let mut pools: Vec<Vec<(usize, f64)>> = Vec::with_capacity(n);
        for s in 0..n {
            let total: f64 = self.ratings[s].values().sum();
            let mut pool = Vec::new();
            if total > 0.0 {
                for (&v, &p) in self.ratings[s].iter() {
                    if p > 0.0 {
                        pool.push((v, p / total));
                    }
                }
            }
            pools.push(pool);
        }

        for _ in 0..iters {
            let mut next = vec![vec!(0.0; n); n];
            for s in 0..n {
                for u in 0..n {
                    let mut val = beta * self.rating(s, u);
                    for &(v, w) in &pools[s] {
                        val += (1.0 - beta) * w * trust[v][u];
                    }
                    next[s][u] = val;
                }
            }
            trust = next;
        }

        trust
    }

    /// 按评分概率从池里抽 k 个（有放回），返回 (v, 权重)。
    ///
    /// 权重用「命中次数 / k」，这样期望等于原始评分权重。
    fn sample_pool(&self, s: usize, k: usize, rng: &mut Rng) -> Vec<(usize, f64)> {
        let pool: Vec<(usize, f64)> = self.ratings[s]
            .iter()
            .filter(|(_, &p)| p > 0.0)
            .map(|(&v, &p)| (v, p))
            .collect();
        if pool.is_empty() {
            return Vec::new();
        }
        let total: f64 = pool.iter().map(|(_, p)| p).sum();
        if total <= 0.0 {
            return Vec::new();
        }

        // 累积分布
        let mut cum = Vec::with_capacity(pool.len());
        let mut acc = 0.0;
        for &(v, p) in &pool {
            acc += p / total;
            cum.push((v, acc));
        }

        let mut hits: HashMap<usize, usize> = HashMap::new();
        for _ in 0..k {
            let r = rng.next_f64();
            // 二分找第一个 cum >= r
            let mut lo = 0;
            let mut hi = cum.len() - 1;
            while lo < hi {
                let mid = (lo + hi) / 2;
                if cum[mid].1 < r {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            *hits.entry(cum[lo].0).or_insert(0) += 1;
        }

        hits.into_iter()
            .map(|(v, c)| (v, c as f64 / k as f64))
            .collect()
    }
}

fn main() {
    println!("严格对比：精确基线 vs 按评分加权抽样");
    println!("========================================\n");

    let n = 500;
    let beta = 0.3; // 个人 3，群体 7
    let iters = 20;
    let net = Net::new(n, 3, 0.5, 2026);

    println!("N={}, β={}（个人 {:.0} / 群体 {:.0}）", n, beta, beta * 10.0, (1.0 - beta) * 10.0);
    println!("评分：每个邻居 50% 概率被评分\n");

    let t0 = std::time::Instant::now();
    let exact = net.solve_exact(beta, iters);
    println!("精确迭代耗时：{:?}\n", t0.elapsed());

    // 真值的分布
    let mut vals = Vec::new();
    for s in 0..n {
        for u in 0..n {
            vals.push(exact[s][u]);
        }
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    let var = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / vals.len() as f64;
    println!("真值：均值 {:.4}，标准差 {:.4}\n", mean, var.sqrt());

    // 抽样估计
    println!("{:<8} {:<14} {:<14} {:<12} {:<12}", "K", "平均误差", "最大误差", "排序一致率", "覆盖率");
    let mut rng = Rng::new(777);
    for &k in &[10usize, 20, 50, 100, 200] {
        let mut abs_err = 0.0;
        let mut max_err = 0.0f64;
        let mut pairs = 0usize;
        let mut exacts: Vec<f64> = Vec::new();
        let mut ests: Vec<f64> = Vec::new();

        // 抽 100 个观察者，每个算 5 个目标
        for _ in 0..100 {
            let s = rng.next_usize(n);
            let samples = net.sample_pool(s, k, &mut rng);
            for _ in 0..5 {
                let u = rng.next_usize(n);
                let est: f64 = beta * net.rating(s, u)
                    + (1.0 - beta)
                        * samples
                            .iter()
                            .map(|&(v, w)| w * exact[v][u])
                            .sum::<f64>();
                let e = (est - exact[s][u]).abs();
                abs_err += e;
                if e > max_err {
                    max_err = e;
                }
                exacts.push(exact[s][u]);
                ests.push(est);
                pairs += 1;
            }
        }

        // 排序一致率
        let mut idx: Vec<usize> = (0..pairs).collect();
        idx.sort_by(|&a, &b| exacts[a].partial_cmp(&exacts[b]).unwrap());
        let mut agree = 0usize;
        let mut total = 0usize;
        for i in 0..pairs {
            for j in (i + 1)..pairs {
                let ea = exacts[idx[i]] - exacts[idx[j]];
                let eb = ests[idx[i]] - ests[idx[j]];
                if ea.abs() < 1e-9 {
                    continue;
                }
                total += 1;
                if ea * eb > 0.0 {
                    agree += 1;
                }
            }
        }
        let rate = if total > 0 {
            agree as f64 / total as f64
        } else {
            1.0
        };

        println!(
            "{:<8} {:<14.4e} {:<14.4e} {:<12.3} {:<12}",
            k,
            abs_err / pairs as f64,
            max_err,
            rate,
            format!("{}/{}", pairs, pairs)
        );
    }

    println!("\n[2] 多轮迭代：抽样结果作为下一轮输入");
    println!("{:<8} {:<8} {:<14} {:<14} {:<12}", "K", "轮数", "平均误差", "最大误差", "排序一致率");

    for &k in &[50usize, 100] {
        let mut rng2 = Rng::new(4242);
        let mut trust = vec![vec![0.0f64; n]; n];

        for round in 1..=iters {
            let mut next = vec![vec![0.0f64; n]; n];
            for s in 0..n {
                // 每轮重新抽样
                let samples = net.sample_pool(s, k, &mut rng2);
                for u in 0..n {
                    let mut val = beta * net.rating(s, u);
                    for &(v, w) in &samples {
                        val += (1.0 - beta) * w * trust[v][u];
                    }
                    next[s][u] = val;
                }
            }
            trust = next;

            // 每 5 轮报告一次
            if round % 5 == 0 {
                let mut abs_err = 0.0;
                let mut max_err = 0.0f64;
                let mut exacts = Vec::new();
                let mut ests = Vec::new();
                for s in 0..n.min(100) {
                    for u in 0..n.min(50) {
                        let e = (trust[s][u] - exact[s][u]).abs();
                        abs_err += e;
                        if e > max_err {
                            max_err = e;
                        }
                        exacts.push(exact[s][u]);
                        ests.push(trust[s][u]);
                    }
                }
                let pairs = exacts.len();

                let mut idx: Vec<usize> = (0..pairs).collect();
                idx.sort_by(|&a, &b| exacts[a].partial_cmp(&exacts[b]).unwrap());
                let mut agree = 0usize;
                let mut total = 0usize;
                for i in 0..pairs {
                    for j in (i + 1)..pairs {
                        let ea = exacts[idx[i]] - exacts[idx[j]];
                        let eb = ests[idx[i]] - ests[idx[j]];
                        if ea.abs() < 1e-9 {
                            continue;
                        }
                        total += 1;
                        if ea * eb > 0.0 {
                            agree += 1;
                        }
                    }
                }
                let rate = if total > 0 { agree as f64 / total as f64 } else { 1.0 };
                println!(
                    "{:<8} {:<8} {:<14.4e} {:<14.4e} {:<12.3}",
                    k,
                    round,
                    abs_err / pairs as f64,
                    max_err,
                    rate
                );
            }
        }
    }

    println!("\n完成。");
}
