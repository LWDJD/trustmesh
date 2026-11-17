//! # 机制验证：稠密视图对陌生 key 的 query 返回值不是 0。
//!
//! 上一支实验（`unbounded_mass_divergence`）观察到「同一份视图在账户表更大的节点上读数更小」。
//! 这一支把机制摊开：
//!
//! `Engine::on_view` 的 `total_mass = Σ_{我见过的全部账户} view.query(key).clamp(0,1)`。
//! 如果稠密视图对**不在它清单里的 key** 返回非零噪声，那么账户表越长，这个和越大，
//! 分母越大 ⇒ `reading = view.query(target) / total_mass` 越小。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_mass_mechanism

use tm_view::{CompressedView, ViewParams};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = ((i >> 8) & 0xFF) as u8;
    k[3] = ((i >> 16) & 0xFF) as u8;
    k
}

fn main() {
    println!("# 机制：稠密视图对陌生 key 的读数不是 0");
    println!();

    let params = ViewParams::new(2048, 5, 3);

    for &n_rated in &[3u32, 30, 300, 1000] {
        // 发布者给 n_rated 个账户各 0.6 分
        let rated: Vec<([u8; 32], f32)> = (1..=n_rated).map(|i| (key(0x01, i), 0.6)).collect();
        let view = CompressedView::from_pairs(params, &rated);

        // 统计：对**陌生 key**（0x02 组）的 query 返回值之和 / 均值 / 最大值
        let n_strangers = 1000u32;
        let mut sum = 0.0f64;
        let mut max = 0.0f32;
        let mut nonzero = 0u32;
        for i in 0..n_strangers {
            let q = view.query(&key(0x02, i));
            sum += f64::from(q);
            if q > max {
                max = q;
            }
            if q > 0.0 {
                nonzero += 1;
            }
        }
        // 也统计：对**清单里**的 key 的返回值之和（这是「真实总量」）
        let mut rated_sum = 0.0f64;
        for i in 1..=n_rated {
            rated_sum += f64::from(view.query(&key(0x01, i)));
        }

        let noise_mean = sum / f64::from(n_strangers);
        println!(
            "视图条目数 {:>5} | 清单内实际总量 {:>8.3} | 每个陌生 key 的平均噪声 {:>10.6} | 陌生 key 非零率 {:>6.2}% | 单点最高 {:.6}",
            n_rated, rated_sum, noise_mean,
            100.0 * f64::from(nonzero) / f64::from(n_strangers),
            max
        );
    }

    println!();
    println!("## 机制");
    println!("视图条目越多，桶越拥挤，对陌生 key 的碰撞噪声越大。");
    println!("`on_view` 把这份噪声对着**我账户表里的每一个账户**累加：");
    println!();
    println!("  total_mass = 真实总量（清单内）+ 账户表长度 × 每陌生 key 平均噪声");
    println!();
    println!("清单内真实总量是常数（约 180），但第二项随账户表长度线性增长：");
    println!("  · 账户表 1 万  → 噪声项 ≈ 1 万 × 噪声均值");
    println!("  · 账户表 100 万 → 噪声项 ≈ 100 万 × 噪声均值（远超真实总量）");
    println!();
    println!("于是 `reading = query(target) / total_mass` 被噪声项稀释——**读数随节点规模缩小**。");
    println!("这就是「同一份字节在不同节点上读数差 10 倍」的机制。");
}
