//! 合并与精度的关系。
//!
//! 验证两件事：
//! 1. 合并 K 个用户，相对误差是否与 K 无关
//! 2. 最终桶数 B 是否决定精度

use social_trust::rng::Rng;
use social_trust::sketch::Sketch;

fn main() {
    println!("合并与精度报告");
    println!("========================================\n");

    let n_users = 300;
    let n_targets = 200;
    let d = 5;

    // 每个用户对部分目标有评分，其余中性
    let mut rng = Rng::new(2026);
    let mut ratings: Vec<Vec<f64>> = Vec::with_capacity(n_users);
    for _ in 0..n_users {
        let mut row = vec![0.5; n_targets];
        for _ in 0..30 {
            let u = rng.next_usize(n_targets);
            row[u] = 0.1 + rng.next_f64() * 0.8;
        }
        ratings.push(row);
    }

    // 精确平均（全量）
    let mut truth = vec![0.0; n_targets];
    for row in &ratings {
        for (u, &v) in row.iter().enumerate() {
            truth[u] += v;
        }
    }
    for t in truth.iter_mut() {
        *t /= n_users as f64;
    }

    // ---- 实验一：固定 B，K 变化 ----
    println!("[1] 固定 B=2048，抽查人数与误差");
    println!("{:<10} {:<16} {:<16} {:<14}", "K", "相对误差", "均方根误差", "耗时");

    for &k in &[10usize, 50, 100, 200, 300] {
        let t0 = std::time::Instant::now();
        let mut merged: Option<Sketch> = None;
        for i in 0..k {
            let mut sk = Sketch::new(d, 2048, 1);
            sk.add_row_sparse(&ratings[i], 0.5, 1.0);
            merged = Some(match merged {
                None => sk,
                Some(m) => m.merge(&sk, 1.0, 1.0),
            });
        }
        let m = merged.unwrap();
        let dt = t0.elapsed();

        let mut abs_err = 0.0;
        let mut sq_err = 0.0;
        for u in 0..n_targets {
            let est = 0.5 + m.query(u) / k as f64;
            let e = (est - truth[u]).abs();
            abs_err += e;
            sq_err += e * e;
        }
        let mae = abs_err / n_targets as f64;
        let rmse = (sq_err / n_targets as f64).sqrt();
        println!(
            "{:<10} {:<16.4e} {:<16.4e} {:<14.2?}",
            k, mae, rmse, dt
        );
    }

    // ---- 实验二：固定 K=100，B 变化 ----
    println!("\n[2] 固定 K=100，桶数与误差");
    println!("{:<10} {:<12} {:<16} {:<16}", "B", "大小(KB)", "相对误差", "均方根误差");

    for &b in &[512usize, 1024, 2048, 4096, 8192] {
        let mut merged: Option<Sketch> = None;
        for i in 0..100 {
            let mut sk = Sketch::new(d, b, 1);
            sk.add_row_sparse(&ratings[i], 0.5, 1.0);
            merged = Some(match merged {
                None => sk,
                Some(m) => m.merge(&sk, 1.0, 1.0),
            });
        }
        let m = merged.unwrap();

        let mut abs_err = 0.0;
        let mut sq_err = 0.0;
        for u in 0..n_targets {
            let est = 0.5 + m.query(u) / 100.0;
            let e = (est - truth[u]).abs();
            abs_err += e;
            sq_err += e * e;
        }
        let mae = abs_err / n_targets as f64;
        let rmse = (sq_err / n_targets as f64).sqrt();
        let size = b * d * 8 / 1024;
        println!(
            "{:<10} {:<12} {:<16.4e} {:<16.4e}",
            b, size, mae, rmse
        );
    }

    // ---- 实验三：二次压缩 ----
    println!("\n[3] 先合并 K 个，再压缩到固定 B=1024");
    println!("{:<10} {:<18} {:<16} {:<14}", "K", "合并后大小(KB)", "再压缩误差", "压缩耗时");

    for &k in &[10usize, 50, 100, 200] {
        let mut merged: Option<Sketch> = None;
        for i in 0..k {
            let mut sk = Sketch::new(d, 2048, 1);
            sk.add_row_sparse(&ratings[i], 0.5, 1.0);
            merged = Some(match merged {
                None => sk,
                Some(m) => m.merge(&sk, 1.0, 1.0),
            });
        }
        let big = merged.unwrap();
        let big_size = big.bytes() / 1024;

        // 再压缩：新建一个小 sketch，从大 sketch 逐目标读出后写入
        let t0 = std::time::Instant::now();
        let mut small = Sketch::new(d, 1024, 1);
        for u in 0..n_targets {
            let v = big.query(u);
            small.add(u, v);
        }
        let dt = t0.elapsed();

        let mut abs_err = 0.0;
        for u in 0..n_targets {
            let est = 0.5 + small.query(u) / k as f64;
            abs_err += (est - truth[u]).abs();
        }
        let mae = abs_err / n_targets as f64;
        println!(
            "{:<10} {:<18} {:<16.4e} {:<14.2?}",
            k, big_size, mae, dt
        );
    }

    // ---- 实验四：β 对抽样误差的影响 ----
    println!("\n[4] 个人权重 β 对抽样误差的影响（N=100, BA 图）");
    println!(
        "{:<10} {:<10} {:<14} {:<14}",
        "个人:群体", "K", "平均误差", "最大误差"
    );

    use social_trust::graph::Graph;
    use social_trust::local_push::{solve_local, LocalPushParams};
    use social_trust::scope::{sample, score};
    use social_trust::sim;

    for &beta in &[0.3f64, 0.5, 0.7] {
        let mut gen = Rng::new(777);
        let mut g: Graph = sim::gen_ba(100, 3, &mut gen);
        sim::random_ratings(&mut g, 8, &mut gen);

        let params = LocalPushParams {
            beta,
            eps: 1e-5,
            max_iter: 1_000_000,
        };
        let exact = solve_local(&g, 0, &params);

        for &k in &[10usize, 50, 100, 200] {
            let mut r = Rng::new(888 + k as u64);
            let sc = sample(&g, 0, k, beta, &mut r);
            let mut abs_err = 0.0;
            let mut max_err = 0.0f64;
            for u in 0..100 {
                let est = score(&sc, &g, u);
                let e = (est - exact[u]).abs();
                abs_err += e;
                if e > max_err {
                    max_err = e;
                }
            }
            println!(
                "{:<10} {:<10} {:<14.4e} {:<14.4e}",
                format!("{:.0}:{:.0}", beta * 10.0, (1.0 - beta) * 10.0),
                k,
                abs_err / 100.0,
                max_err
            );
        }
    }

    println!("\n完成。");
}
