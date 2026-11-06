//! Sketch 报告：大小、精度、规模特性。

use social_trust::rng::Rng;
use social_trust::sketch::Sketch;

fn sparse_row(n: usize, per_node: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut v = vec![0.5; n];
    for _ in 0..per_node {
        v[rng.next_usize(n)] = rng.next_f64();
    }
    v
}

fn dense_row(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| rng.next_f64()).collect()
}

fn eval_sparse(sk: &Sketch, row: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut cnt = 0;
    for (u, &v) in row.iter().enumerate() {
        if (v - 0.5).abs() > 1e-9 {
            sum += (sk.query_with_baseline(u, 0.5) - v).abs();
            cnt += 1;
        }
    }
    if cnt == 0 { 0.0 } else { sum / cnt as f64 }
}

fn main() {
    println!("Sketch 报告");
    println!("========================================\n");

    let n = 10_000;
    let row = sparse_row(n, 20, 7);

    println!("[1] 桶数 k 与精度（稀疏：N={}, 每人 20 个非中性）", n);
    println!("{:<10} {:<12} {:<14} {:<12}", "k", "大小(KB)", "平均误差", "用时");

    for &k in &[1024usize, 2048, 4096, 8192, 16384] {
        let mut sk = Sketch::new(5, k, 2);
        let t0 = std::time::Instant::now();
        sk.add_row_sparse(&row, 0.5, 1.0);
        let dt = t0.elapsed();
        let err = eval_sparse(&sk, &row);
        println!(
            "{:<10} {:<12.1} {:<14.4e} {:<12.2?}",
            k,
            sk.bytes() as f64 / 1024.0,
            err,
            dt
        );
    }

    println!("\n[2] 规模无关性（k=4096, 每人 20 个非中性）");
    println!("{:<10} {:<12} {:<14} {:<12}", "N", "大小(KB)", "平均误差", "构建用时");

    for &nn in &[1_000usize, 10_000, 100_000, 1_000_000] {
        let r = sparse_row(nn, 20, 11);
        let mut sk = Sketch::new(5, 4096, 2);
        let t0 = std::time::Instant::now();
        sk.add_row_sparse(&r, 0.5, 1.0);
        let dt = t0.elapsed();
        let err = eval_sparse(&sk, &r);
        println!(
            "{:<10} {:<12.1} {:<14.4e} {:<12.2?}",
            nn,
            sk.bytes() as f64 / 1024.0,
            err,
            dt
        );
    }

    println!("\n[3] 稠密场景（每个人都对所有账户评分）");
    println!("{:<10} {:<12} {:<14}", "N", "大小(KB)", "平均误差");

    for &nn in &[1_000usize, 10_000, 100_000] {
        let r = dense_row(nn, 13);
        let mut sk = Sketch::new(5, 4096, 2);
        sk.add_row_sparse(&r, 0.0, 1.0);
        let mut sum = 0.0;
        for (u, &v) in r.iter().enumerate() {
            sum += (sk.query_with_baseline(u, 0.0) - v).abs();
        }
        println!(
            "{:<10} {:<12.1} {:<14.4e}",
            nn,
            sk.bytes() as f64 / 1024.0,
            sum / nn as f64
        );
    }

    println!("\n[4] 合并：个人 + 群体的加权");
    let personal = sparse_row(n, 20, 21);
    let group = sparse_row(n, 20, 22);

    let mut a = Sketch::new(5, 4096, 3);
    a.add_row_sparse(&personal, 0.5, 1.0);
    let mut b = Sketch::new(5, 4096, 3);
    b.add_row_sparse(&group, 0.5, 1.0);

    let blended = a.merge(&b, 0.7, 0.3);
    println!("  个人权重 0.7，群体权重 0.3");
    println!("  大小：{} KB（与单份相同）", blended.bytes() as f64 / 1024.0);
    println!("  样本查询（前 5 个非中性账户）：");

    let mut shown = 0;
    for u in 0..n {
        if shown >= 5 {
            break;
        }
        if (personal[u] - 0.5).abs() > 1e-9 || (group[u] - 0.5).abs() > 1e-9 {
            let expect = 0.7 * personal[u] + 0.3 * group[u];
            let got = blended.query_with_baseline(u, 0.5);
            println!(
                "    u={:<8} 期望 {:.4} 实际 {:.4} 误差 {:.4}",
                u, expect, got, (got - expect).abs()
            );
            shown += 1;
        }
    }

    println!("\n[5] 关注数量 s 与误差（N=100000）");
    println!("{:<8} {:<10} {:<12} {:<12}", "s", "k", "大小(KB)", "平均误差");

    for &s in &[20usize, 100, 500, 2000, 10000] {
        let r = sparse_row(100_000, s, 31);
        for &k in &[1024usize, 4096, 16384] {
            let mut sk = Sketch::new(5, k, 2);
            sk.add_row_sparse(&r, 0.5, 1.0);
            let err = eval_sparse(&sk, &r);
            println!(
                "{:<8} {:<10} {:<12.1} {:<12.4e}",
                s,
                k,
                sk.bytes() as f64 / 1024.0,
                err
            );
        }
    }

    println!("\n完成。");
}