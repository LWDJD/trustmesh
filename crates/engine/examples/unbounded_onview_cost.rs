//! # 无限规模的第二道墙：`on_view` 的代价是 O(账户表长度)，不是 O(1)。
//!
//! `Engine::on_view` 为了算 `total_mass`（影响力预算的分母），要遍历**整个账户表**：
//!
//! ```text
//! for i in 0..self.accounts.len() as u32 {          // ← 遍历**所有**见过的账户
//!     if let Some(k) = self.accounts.key_of(i) {
//!         total_mass += view.query(&k).clamp(0.0, 1.0);
//!     }
//! }
//! ```
//!
//! ## 为什么这在无限网络上致命
//!
//! 每次 `on_view` 的代价 = 账户表长度 × Count Sketch 查询。
//! 而账户表长度**是我见过的账户数** —— 在网络里，一个攻击者可以免费把别人的 key 喂给我
//! （每个 key 只是一条观点里的 32 字节），于是我的 `on_view` 会越来越慢。
//!
//! 这是**放大器**：我收 100 万条不同 key 的观点（每条约 40 KB），我的账户表就涨到 100 万，
//! 之后**每一条** `on_view` 都要扫 100 万次。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_onview_cost

use tm_engine::{Engine, EngineConfig};
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
    println!("# 无限规模第 2 道墙：on_view 的代价 = O(账户表长度)");
    println!();

    let cfg = EngineConfig {
        view_params: ViewParams::new(2048, 5, 11),
        half_life: 72,
        ..EngineConfig::default()
    };
    let params = cfg.view_params;

    // 我评过的一个来源（必须评过才能收它的观点）
    let mut owner_view_pairs: Vec<([u8; 32], f32)> = Vec::new();
    for i in 0..64u32 {
        owner_view_pairs.push((key(0x01, i), 1.0));
    }
    let view = CompressedView::from_pairs(params, &owner_view_pairs);

    println!("账户表长度      单次 on_view 耗时");

    for &n in &[100usize, 1_000, 10_000, 100_000, 500_000] {
        let mut e = Engine::with_seed(cfg.clone(), 0x0CBA);
        // intern n 个账户（我只评了其中 1 个，其余都是「听说过」）
        for i in 0..n as u32 {
            e.intern(key(0x02, i));
        }
        let owner = e.intern(key(0x03, 0));
        e.rate(owner, 1.0, 0);

        // 测量单次 on_view（重复 20 次取平均，因为 release 下太快）
        let reps = 20;
        let t0 = std::time::Instant::now();
        for r in 0..reps {
            e.on_view(owner, view.clone(), 12 + r as u64, 12);
        }
        let per = t0.elapsed().as_secs_f64() / reps as f64;
        println!("{:>12}      {:>10.3} ms", n, per * 1000.0);
    }

    println!();
    println!("## 结论");
    println!("单次 on_view 耗时**随账户表长度线性增长**。这不是优化问题，是算法复杂度问题：");
    println!("`total_mass` 的定义是「对**所有已知账户**的 query 求和」，所以它天然是 O(n)。");
    println!();
    println!("在无限网络上，这构成一个**免费的放大器**：");
    println!("  攻击者只需让我 intern 大量不同 key（每个 key 只占 32 字节），");
    println!("  我的每一条 on_view 就会变慢，直到我处理不过来。");
    println!();
    println!("**这是无限规模下必须解决的第一个问题**（比内存更急，因为它是 CPU 的）；");
    println!("解法方向见 审计/无限规模.md §3：`total_mass` 改为**增量维护**或**抽样估计**。");
}
