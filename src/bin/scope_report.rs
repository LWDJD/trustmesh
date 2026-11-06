//! 范围化系统报告：抽样、合并、模糊化。

use social_trust::local_push::{solve_local, LocalPushParams};
use social_trust::rng::Rng;
use social_trust::scope::{merge, merge_published, publish, sample, score};
use social_trust::sim;

fn main() {
    println!("范围化系统报告");
    println!("========================================\n");

    let n = 100;
    let beta = 0.7;
    let mut rng = Rng::new(42);
    let mut g = sim::gen_ba(n, 3, &mut rng);
    sim::random_ratings(&mut g, 8, &mut rng);

    // 基准：打分系统
    let params = LocalPushParams {
        beta,
        eps: 1e-5,
        max_iter: 1_000_000,
    };
    let exact = solve_local(&g, 0, &params);
    println!("基准：打分系统（Local Push, beta={}）\n", beta);

    // ---- 抽样量 ----
    println!("[1] 抽样量与误差（单份范围）");
    println!("{:<10} {:<14} {:<14}", "k", "平均绝对误差", "最大误差");

    let mut k_list = vec![100usize, 400, 1600, 6400, 25600];
    let mut errors = Vec::new();
    for &k in &k_list {
        let mut r = Rng::new(1000 + k as u64);
        let sc = sample(&g, 0, k, beta, &mut r);
        let est: Vec<f64> = (0..n).map(|u| score(&sc, &g, u)).collect();
        let mae = est
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>()
            / n as f64;
        let me = est
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        errors.push(mae);
        println!("{:<10} {:<14.4e} {:<14.4e}", k, mae, me);
    }
    k_list.clear();

    // ---- 合并份数 ----
    println!("\n[2] 合并份数与误差（每份 k=1600）");
    println!("{:<10} {:<14} {:<14}", "份数", "总样本", "平均绝对误差");

    let mut merged = sample(&g, 0, 1600, beta, &mut Rng::new(2001));
    for round in 1..=5 {
        if round > 1 {
            let more = sample(&g, 0, 1600, beta, &mut Rng::new(2000 + round as u64));
            merged = merge(&merged, &more, 1.0, 1.0);
        }
        let est: Vec<f64> = (0..n).map(|u| score(&merged, &g, u)).collect();
        let mae = est
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>()
            / n as f64;
        println!("{:<10} {:<14} {:<14.4e}", round, merged.len(), mae);
    }

    // ---- 模糊化 ----
    println!("\n[3] 模糊化：epsilon 与误差（k=6400）");
    println!("{:<10} {:<14} {:<14}", "epsilon", "平均绝对误差", "最大误差");

    let sc = sample(&g, 0, 6400, beta, &mut Rng::new(777));
    let raw: Vec<f64> = (0..n).map(|u| score(&sc, &g, u)).collect();
    let mut r = Rng::new(888);

    for &eps in &[100.0, 50.0, 20.0, 10.0, 5.0, 1.0] {
        let p = publish(&sc, &g, eps, &mut r);
        let mae = p
            .scores
            .iter()
            .zip(raw.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>()
            / n as f64;
        let me = p
            .scores
            .iter()
            .zip(raw.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        println!("{:<10.0} {:<14.4e} {:<14.4e}", eps, mae, me);
    }

    // ---- 模糊化后合并 ----
    println!("\n[4] 模糊化后合并（epsilon=100, 每份 k=3200）");
    println!("{:<10} {:<14} {:<14}", "份数", "平均绝对误差", "相对单份");

    let base_mae = {
        let p = publish(&sample(&g, 0, 3200, beta, &mut Rng::new(9001)), &g, 100.0, &mut Rng::new(1));
        p.scores
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>()
            / n as f64
    };

    let mut acc = publish(&sample(&g, 0, 3200, beta, &mut Rng::new(9001)), &g, 100.0, &mut Rng::new(2));
    for round in 1..=4 {
        if round > 1 {
            let more = publish(
                &sample(&g, 0, 3200, beta, &mut Rng::new(9000 + round as u64)),
                &g,
                100.0,
                &mut Rng::new(10 + round as u64),
            );
            acc = merge_published(&acc, &more, 1.0, 1.0);
        }
        let mae = acc
            .scores
            .iter()
            .zip(exact.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>()
            / n as f64;
        println!("{:<10} {:<14.4e} {:<14.2}", round, mae, mae / base_mae);
    }

    // ---- 隐私检查 ----
    println!("\n[5] 发布数据的结构");
    let p = publish(&sc, &g, 50.0, &mut Rng::new(555));
    println!("  字段: scores[{}], epsilon, weight", p.scores.len());
    println!("  不含: 节点 ID、边、抽样路径");
    println!("  单份范围里的节点数: {}（仅在本地，不发布）", sc.len());

    println!("\n完成。");
}
