//! 最小可跑示例：两个人互相评分，看各自眼里的声望。
//!
//! 跑：cargo run --release --example quickstart
//!
//! 注意一个容易被忽略的前提：**想收到某人的观点，你必须先给他打正分**。
//! 这是 `on_view` 的结构性拒绝条件（见 src/node.rs:223）——
//! 「没关系的来源，观点对我没有权重，存了也没用」。
//! 所以下面 bob 必须先 rate(alice) 才能 on_view(alice 的视图)。

use tm_mincore::{Config, Node, SparseView};

fn key(i: u32) -> u32 {
    i // mincore 的 Acc 就是 u32，不需要公钥
}

fn main() {
    let cfg = Config {
        cap: 128,
        ..Default::default()
    };
    let now: u64 = 0;

    // ---------- alice 这边 ----------
    let mut alice = Node::new(cfg, 1);
    alice.rate(key(2), 0.9, now); // bob
    alice.rate(key(3), 0.4, now); // carol

    let av = alice.publish(now);
    let abytes = av.encode();
    println!(
        "alice 的视图：{} 字节（cap×6+2 = {}×6+2 = {}）",
        abytes.len(),
        cfg.cap,
        cfg.cap * 6 + 2
    );
    assert_eq!(abytes.len(), cfg.cap * 6 + 2);

    // 编码规范性：decode→encode 逐字节相同
    let decoded = SparseView::decode(&abytes, cfg.cap).expect("alice 的视图应当合法");
    assert_eq!(decoded.encode(), abytes, "合法视图必须逐字节可往返");

    // ---------- bob 这边 ----------
    let mut bob = Node::new(cfg, 2);
    bob.rate(key(1), 0.8, now); // ← 关键：bob 必须先给 alice 打分，否则 on_view 会被拒

    assert!(
        bob.on_view(key(1), decoded, now, now),
        "bob 已给 alice 打分，应当接受她的视图"
    );

    let d = bob.distribution(now);
    println!("\nbob 眼里的声望分布（{} 条）：", d.entries.len());
    for &(acc, v) in &d.entries {
        println!("  账户 {:>3} : {:.6}", acc, v);
    }
    let sum: f32 = d.entries.iter().map(|&(_, v)| v).sum();
    println!("\nΣ = {:.6}（必须 = 1）", sum);
    assert!((sum - 1.0).abs() < 1e-6, "全局归一化必须精确到 1");

    println!("bob 问 carol 的声望：{:.6}", bob.query(key(3), now));
    println!(
        "bob 问一个陌生账户：{:.6}（精确 0）",
        bob.query(key(999), now)
    );
    assert_eq!(bob.query(key(999), now), 0.0, "陌生账户必须精确 0");

    // ---------- 反面：没打过分的来源，视图会被拒 ----------
    let mallory = Node::new(cfg, 3);
    let mv = mallory.publish(now);
    assert_eq!(mv.total_q(), 0, "mallory 没有正分，视图为空");
    assert!(!bob.on_view(key(9), mv, now, now), "空视图必须被拒");
    println!("\n✓ 编码可往返 / Σ≡1 / 陌生账户精确 0 / 空视图被拒");
}
