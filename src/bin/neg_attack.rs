//! 负权重攻击：被讨厌的账户夸你，等于害你。
//!
//! # 攻击
//!
//! 1. 攻击者控制一批垃圾账户 G
//! 2. G 给目标 T 打满分
//! 3. 所有诚实账户给 G 打负分
//! 4. 结果：G 的 trust 是负的，G 对 T 的贡献也是负的 → T 声望下降
//!
//! # 两种筛选
//!
//! - 按 P 筛选：只要求「我给 G 的评分 > 0」
//! - 按 trust 筛选：要求「我对 G 的 trust > 0」
//!
//! 后者能挡住攻击，因为 G 的 trust 是负的。

use std::collections::HashMap;

use social_trust::rng::Rng;

const N: usize = 200;
const BETA: f64 = 0.3;

struct Net {
    edges: Vec<Vec<usize>>,
    ratings: Vec<HashMap<usize, f64>>,
    spammers: Vec<usize>,
    target: usize,
}

impl Net {
    fn build(attack: bool, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let n = N;

        // 环形 + 随机边，保证连通
        let mut edges = vec![Vec::new(); n];
        for i in 0..n {
            edges[i].push((i + 1) % n);
            edges[(i + 1) % n].push(i);
            edges[i].push((i + 7) % n);
            edges[(i + 7) % n].push(i);
        }

        let target = 3;
        let spammers: Vec<usize> = (0..20).map(|i| 100 + i).collect();

        let mut ratings: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];

        // 诚实账户：对邻居打中性偏正分
        for s in 0..n {
            if spammers.contains(&s) {
                continue;
            }
            for &v in &edges[s] {
                if !spammers.contains(&v) {
                    let p = 0.4 + rng.next_f64() * 0.3;
                    ratings[s].insert(v, p);
                }
            }
            // 80% 的诚实账户直接给垃圾账户打负分
            // 20% 不打分，靠传播知道它不好
            if attack && rng.chance(0.8) {
                for &g in &spammers {
                    ratings[s].insert(g, -0.8);
                }
            }
        }

        // 垃圾账户：给目标打满分，其他随机
        if attack {
            for &g in &spammers {
                ratings[g].insert(target, 1.0);
                for &v in &edges[g] {
                    if !spammers.contains(&v) {
                        ratings[g].insert(v, 0.6);
                    }
                }
                // 垃圾账户之间互打高分
                for &h in &spammers {
                    if g != h {
                        ratings[g].insert(h, 0.8);
                    }
                }
            }
        }

        Self {
            edges,
            ratings,
            spammers,
            target,
        }
    }

    fn rating(&self, s: usize, t: usize) -> f64 {
        self.ratings[s].get(&t).copied().unwrap_or(0.5)
    }

    /// 迭代 trust。
    ///
    /// `mode`：
    /// 0 = 按 P 筛选，权重用 P
    /// 1 = 按 trust > 0 筛选，权重用 P
    /// 2 = 按 P 筛选，权重用 trust（可能为负，会反向贡献）
    fn solve(&self, iters: usize, mode: u8) -> Vec<Vec<f64>> {
        let n = N;
        let mut trust = vec![vec![0.0; n]; n];

        for _ in 0..iters {
            let mut next = vec![vec![0.0; n]; n];
            for s in 0..n {
                // 构建池
                let mut pool: Vec<(usize, f64)> = Vec::new();
                for (&v, &p) in self.ratings[s].iter() {
                    // 模式 2 允许负分账户进入，用 trust 作权重
                    if mode != 2 && p <= 0.0 {
                        continue;
                    }
                    if p == 0.0 {
                        continue;
                    }
                    if mode == 1 && trust[s][v] <= 0.0 {
                        continue;
                    }
                    let w = if mode == 2 { trust[s][v] } else { p };
                    pool.push((v, w));
                }
                let total: f64 = pool.iter().map(|(_, w)| w.abs()).sum();

                for u in 0..n {
                    let mut val = BETA * self.rating(s, u);
                    if total > 0.0 {
                        for &(v, w) in &pool {
                            val += (1.0 - BETA) * (w / total) * trust[v][u];
                        }
                    }
                    next[s][u] = val;
                }
            }
            trust = next;
        }

        trust
    }

    /// 诚实观察者对目标的平均信任。
    fn target_trust(&self, trust: &[Vec<f64>]) -> f64 {
        let honest: Vec<usize> = (0..N).filter(|x| !self.spammers.contains(x)).collect();
        let mut sum = 0.0;
        for &s in &honest {
            sum += trust[s][self.target];
        }
        sum / honest.len() as f64
    }
}

fn main() {
    println!("负权重攻击实验");
    println!("========================================\n");
    println!("N={}, β={}（个人 {:.0} / 群体 {:.0}）\n", N, BETA, BETA * 10.0, (1.0 - BETA) * 10.0);

    println!("{:<16} {:<16} {:<16} {:<12}", "场景", "筛选方式", "目标信任", "相对变化");
    println!("{}", "-".repeat(64));

    // 基线：无攻击
    let base_net = Net::build(false, 42);
    let base_p = base_net.solve(20, 0);
    let base_trust = base_net.target_trust(&base_p);
    println!(
        "{:<16} {:<16} {:<16.4} {:<12}",
        "无攻击", "权重=P", base_trust, "基准"
    );

    // 攻击 + 三种模式
    let atk = Net::build(true, 42);
    for (mode, name) in [(0u8, "权重=P"), (1, "权重=P+过滤"), (2, "权重=trust")] {
        let t = atk.solve(20, mode);
        let v = atk.target_trust(&t);
        let drop = (v - base_trust) / base_trust * 100.0;
        println!(
            "{:<16} {:<16} {:<16.4} {:<12.1}%",
            "负权重攻击", name, v, drop
        );
    }

    println!("\n说明：");
    println!("  权重=P：只要求「我给对方的评分 > 0」");
    println!("  权重=P+过滤：额外要求「我对对方的 trust > 0」");
    println!("  权重=trust：用信任分作权重，负信任会反向贡献");
}
