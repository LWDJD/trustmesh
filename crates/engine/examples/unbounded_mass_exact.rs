//! # 精确量化：`total_mass` 随账户表长度增长到多少（对照真实总量）。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_mass_exact

use tm_engine::view::{CompressedView, ViewParams};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = ((i >> 8) & 0xFF) as u8;
    k[3] = ((i >> 16) & 0xFF) as u8;
    k
}

fn main() {
    let params = ViewParams::new(2048, 5, 3);
    let n_rated = 300u32;
    let rated: Vec<([u8; 32], f32)> = (1..=n_rated).map(|i| (key(0x01, i), 0.6)).collect();
    let view = CompressedView::from_pairs(params, &rated);

    let real_mass: f64 = (1..=n_rated)
        .map(|i| f64::from(view.query(&key(0x01, i))))
        .sum();
    println!("清单内真实总量 = {:.4}（300 个条目各 0.6）", real_mass);
    println!();
    println!("账户表长度 → total_mass（真实 + 噪声）→ 相对真实总量的膨胀倍数");

    for &n_extra in &[0u32, 1_000, 10_000, 100_000, 1_000_000] {
        // 模拟 on_view 的 total_mass 计算：对「我见过的全部账户」求 query 和
        let mut mass = 0.0f64;
        // 1) 清单内（发布者写的那 300 个）
        for i in 1..=n_rated {
            mass += f64::from(view.query(&key(0x01, i)).clamp(0.0, 1.0));
        }
        // 2) 陌生账户（我账户表里多出来的那些）
        for i in 0..n_extra {
            mass += f64::from(view.query(&key(0x02, i)).clamp(0.0, 1.0));
        }
        println!(
            "{:>10} → {:>10.4} → ×{:.2}",
            n_extra + n_rated,
            mass,
            mass / real_mass
        );
    }

    println!();
    println!("## 结论");
    println!("噪声项随账户表长度**线性增长**，很快超过真实总量。");
    println!("`reading = view.query(target) / total_mass` 因此被压缩到接近 0。");
    println!();
    println!(
        "注意：这不是「碰撞概率」问题，是「**把一份固定的字节除以一个随本机状态变化的数**」；"
    );
    println!("即使噪声很小，只要账户表足够长，累加后也会主导分母。");
}
