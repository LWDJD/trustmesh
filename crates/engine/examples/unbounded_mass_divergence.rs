//! # 无限规模的**最严重**的问题：同一份视图，不同节点读出不同的值。
//!
//! ## 根因
//!
//! `Engine::on_view` 用「**我 intern 过的全部账户**」当分母去估计这份观点的总质量：
//!
//! ```text
//! total_mass = Σ_{i ∈ 我见过的全部账户} view.query(key_of(i)).clamp(0, 1)
//! ```
//!
//! 然后 `query` 用 `view.query(target) / total_mass` 归一化。
//!
//! **问题是：不同节点见过的账户数不同，于是 total_mass 不同，于是同一个桶值被除以不同的分母。**
//!
//! 规格 §4.2 的原意是「估计这份观点文件的**总质量**」——那应该是一个**与接收方无关**的量
//! （发布者写了多少内容，就是多少）。但当前实现把它变成了「**接收方的账户表长度**」。
//!
//! 在小网络、且所有节点 intern 同一批账户时，这个偏差恒为 0，所以从没暴露。
//! 在无限网络上，它必然非零。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_mass_divergence

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = ((i >> 8) & 0xFF) as u8;
    k[3] = ((i >> 16) & 0xFF) as u8;
    k
}

fn main() {
    println!("# 同一份视图，两个节点读出不同的声望（因为 total_mass 依赖局部账户表）");
    println!();

    let cfg = EngineConfig {
        view_params: ViewParams::new(2048, 5, 3),
        half_life: 72,
        ..EngineConfig::default()
    };
    let params = cfg.view_params;

    // 发布者 P 的观点：给 **300 个**账户各 0.6 分 —— 这是真实清单长度（稠密视图）。
    // 稀疏视图（3 个条目）不会触发这个问题：Count Sketch 对不在视图里的 key 返回精确 0。
    // 稠密视图会给**每一个** key 返回一点碰撞噪声，于是接收方的额外账户会累加噪声进分母。
    let n_rated = 300u32;
    let targets: Vec<[u8; 32]> = (1..=n_rated).map(|i| key(0x01, i)).collect();
    let pairs: Vec<([u8; 32], f32)> = targets.iter().map(|k| (*k, 0.6)).collect();
    let view = CompressedView::from_pairs(params, &pairs);
    println!(
        "发布者 P 的视图：给 {} 个账户各 0.6 分（稠密视图，encode 后 {:?} 字节）",
        n_rated,
        view.encode().len()
    );
    println!("  （这正是真实清单长度 m=300 的情形，不是刻意构造的极端）");
    println!();

    // 观察者 A：只认识 {p, t1, t2, t3}（4 个账户）
    // 观察者 B：认识 {p, t1, t2, t3} + 996 个陌生账户（1000 个账户）
    // 两个观察者收到**完全同一份** view 字节。
    println!(
        "{:<10} {:<12} {:<16} {:<16}",
        "观察者", "账户表长度", "t1 的声望", "相对 A 的比值"
    );

    let mut base = 0.0f32;
    for &n_extra in &[0u32, 96, 996, 9996, 99996, 999996] {
        let mut e = Engine::with_seed(cfg.clone(), 0xA0 + n_extra as u64);
        // 先 intern {p, t1, t2, t3}
        let p = e.intern(key(0x02, 0));
        for t in &targets {
            e.intern(*t);
        }
        // 再 intern n_extra 个陌生账户（与这份视图无关，但它们在账户表里）
        for i in 0..n_extra {
            e.intern(key(0x03, i));
        }
        // 我评过 p（才能收它的观点）
        e.rate(p, 1.0, 0);
        // 收**同一份** view
        e.on_view(p, view.clone(), 12, 12);

        // 读 t1 的声望
        let t1 = e.lookup(&targets[0]).unwrap();
        let q = e.query(t1, 12);
        if n_extra == 0 {
            base = q;
        }
        println!(
            "{:<10} {:<12} {:<16.8} {:<16.2}",
            format!("n_extra={}", n_extra),
            e.account_count(),
            q,
            q / base
        );
    }

    println!();
    println!("## 结论（这是无限规模下**最严重**的问题）");
    println!("**同一份视图字节，在不同节点上算出不同的声望。** 这不是精度差异，是**语义不一致**：");
    println!("分母 `total_mass` 被定义成「接收方见过的账户数」，而它与发送方的视图内容无关。");
    println!();
    println!("后果：");
    println!("* 节点间的声望**不可比较**（S6 跨节点一致性失效）；");
    println!("* 新节点（账户表小）看到的声望**系统性偏高**，老节点偏低——攻击者只要造一个新节点就能放大读数；");
    println!("* 快照重放会不一致：`mass_covers` 记的是当时的账户表长度，但它的语义是错的。");
    println!();
    println!("**修法（见 审计/无限规模.md §4）**：`total_mass` 必须改成**与接收方无关**的量。");
    println!("最直接的方案：发布者在 `encode` 之前就把总量**算好并写进字节**，接收方只做**校验**不做估计。");
}
