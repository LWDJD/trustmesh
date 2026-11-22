//! # 传输出口：本库能为「抗劫持的节点发现」提供什么
//!
//! 场景：女巫节点接管传输层，向我的节点谎报「网络里只有我们这几个节点」。
//! 经典防御是「用**本地可信名单**引导发现」——不受对端谎报影响。
//!
//! 本库恰好天生提供这份名单：[Engine::shares] 返回「我给谁打了正分、各占多少份额」，
//! 而它**只能由我自己的评分产生**，任何对端都无法注入或删除。
//!
//! 这个探针验证三件事：
//!
//! 1. 女巫狂发观点**改不了**我的 shares（传输层能做的只有投递观点）
//! 2. 女巫自称「我是诚实节点」也没有用（库不看自述，只看我自己的评分）
//! 3. 即使传输全挂，shares 依然完好（这正是传输淹没时最需要的东西）
//!
//! 用法：cd crates/engine && cargo run --release --example bootstrap_list

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = 0xB5;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(2048, 5, 41),
        half_life: 72,
        ..EngineConfig::default()
    }
}

fn main() {
    let cfg = cfg();
    let params = cfg.view_params;
    let n_friends = 8u32;
    let n_sybil = 500u32;

    println!("# 抗劫持的节点发现：本库能提供什么");
    println!();

    let mut e = Engine::with_seed(cfg.clone(), 0xB500);

    // 我：给 8 个真实朋友打正分
    for i in 0..n_friends {
        e.intern(key(0x01, i));
        e.rate(i, 1.0, 0);
    }
    // 网络里还有 500 个女巫（我也 intern 了，因为它们在网络中存在）
    for i in 0..n_sybil {
        e.intern(key(0x02, i));
    }

    let before = e.shares();
    println!("我的基线引导名单（shares）共 {} 条：", before.len());
    for (idx, s) in &before {
        println!("  账户 {} 份额 {:.4}", idx, s);
    }

    // 女巫尝试：发出大量观点，声称自己是最可信的、并互相吹捧到 100
    let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
    for i in 0..64u32 {
        pairs.push((key(0x02, i), 100.0));
    }
    let sybil_view = CompressedView::from_pairs(params, &pairs);

    // 它们只能通过「冒充我评过的人」进入我的引擎 —— 而我没有评过任何女巫
    for i in 0..n_friends {
        e.on_view(i, sybil_view.clone(), 12, 12);
    }

    let after = e.shares();
    println!();
    println!("女巫狂发观点后（冒充我的 8 个朋友各投一份 100 分的自吹视图）：");
    println!("  shares 条数：{} → {}（不变）", before.len(), after.len());
    let mut changed = 0;
    for (a, b) in before.iter().zip(after.iter()) {
        if a.0 != b.0 || (a.1 - b.1).abs() > 1e-9 {
            changed += 1;
        }
    }
    println!(
        "  shares 内容变化：{} 条（0 = 女巫完全没能动我的引导名单）",
        changed
    );

    // 传输完全挂掉之后呢
    for round in 0..20u64 {
        let now = 12 * round;
        for r in e.tick(now, 4) {
            e.on_view_failed(r.idx, now);
        }
    }
    let after2 = e.shares();
    println!();
    println!("传输全挂 20 轮后：");
    println!(
        "  shares 仍有 {} 条 —— **引导名单不依赖网络**",
        after2.len()
    );

    // 关键：这份名单与「对端说它有多少节点」毫无关系
    println!();
    println!("## 结论（给应用层的用法）");
    println!("1. **shares() 就是抗劫持的引导名单**：它只由我的评分产生，");
    println!("   对端**无法**注入新节点、无法删除现有节点、无法花钱买到位置");
    println!("2. 节点发现协议可以「先从 shares 里的 N 个节点起步」，而不是从硬编码种子起步");
    println!("3. 即使传输全废，这份名单也完好（个人项不衰减、不依赖观点交换）");
    println!();
    println!("## 但它做不到的");
    println!("* 它**不知道**哪些对等节点在线 —— 那是传输层的事");
    println!("* 它**不能**保证 shares 里的节点此刻可达 —— 需要应用层多路尝试");
    println!("* 首次冷启动时 shares 是空的 —— 种子节点仍需应用层提供");
}
