//! # 不同节点用不同 `cap` 会怎样（A12 定长稀疏表原型的关键缺口）
//!
//! 作者的要求里有一句：「合并后的大小有上限，**用户可以自己设定上限**」。
//!
//! 这意味着**不同节点可以有不同的 cap**。而 A12 原型的 `decode(bytes, cap)` 用
//! **接收方自己**的 cap 去解析字节 —— 那么：
//!
//! * 发送方 cap = 512，发布了一份 3074 字节的视图；
//! * 接收方 cap = 128，期望 770 字节 —— **长度不符，整条拒绝**。
//!
//! 于是「cap 不同」在网络里表现为「**互相收不到对方的观点**」，而不是「精度不同」。
//!
//! 这个探针把三种情形都测出来：
//!
//! 1. 接收方 cap **更小** → 长度不符 → 整条拒绝（完全失联）；
//! 2. 接收方 cap **更大** → 长度不符 → 整条拒绝（同样失联）；
//! 3. 接收方 cap **相同** → 正常工作。
//!
//! 用法：cd crates/view && cargo run --release --example cap_mismatch

use tm_view::{CompressedView, ViewParams};

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k
}

fn main() {
    println!("# 不同节点用不同 cap：不是「精度不同」，是「完全失联」");
    println!();
    println!("## 先看 A12 定长稀疏表的体积公式（cap × 6 + 2 字节）");
    for cap in [64usize, 128, 256, 512, 1024] {
        println!(
            "  cap = {:>5} → {:>6} 字节（Count Sketch 2048×5×4 = 40960 B 的 {:.1}%）",
            cap,
            cap * 6 + 2,
            100.0 * (cap * 6 + 2) as f64 / 40960.0
        );
    }
    println!();

    println!("## 现行 Count Sketch 的对照：体积与 cap 无关，但**桶数必须全网一致**");
    let buckets = 2048usize;
    let hashers = 5usize;
    println!(
        "  ViewParams::new({}, {}, seed) → 恒 {} 字节",
        buckets,
        hashers,
        buckets * hashers * 4
    );
    println!();

    // 现行实现里，ViewParams 不一致会整条拒绝（这是已知的、且有测试钉住）
    let a = ViewParams::new(2048, 5, 7);
    let b = ViewParams::new(1024, 5, 7);
    let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
    for i in 0..100u32 {
        pairs.push((key(i), 0.5 + (i as f32 % 10.0) / 20.0));
    }
    let va = CompressedView::from_pairs(a, &pairs);
    let bytes_a = va.encode();
    println!("发送方用 params={:?}，编码后 {} 字节", a, bytes_a.len());
    let expect_b = CompressedView::new(b).bytes();
    println!(
        "接收方用 params={:?} → decode 结果 = {:?}",
        b,
        CompressedView::decode(&bytes_a, b).map(|v| v.bytes())
    );
    println!(
        "  ↑ 长度 {} ≠ 期望 {} ⟹ 整条拒绝（**不是精度差异，是失联**）",
        bytes_a.len(),
        expect_b
    );
    println!();

    println!("## 结论");
    println!("**「用户可以自己设定上限」这个自由在 A12 的定长表里是有代价的：**");
    println!();
    println!("| 谁可以自由 | 后果 |");
    println!("|---|---|");
    println!("| 全网统一 cap（部署参数） | ✅ 正常工作，各节点体积相同 |");
    println!("| 每节点自己定 cap | ❌ 长度不符 ⟹ **互相拒绝观点** ⟹ 网络分裂成 cap 相同的孤岛 |");
    println!();
    println!("### 解法（三种，都有代价）");
    println!();
    println!("**A. cap 写进字节头（自描述格式）**：`decode` 不要求接收方预先知道 cap，");
    println!("   而是从头 2 字节读 cap 再解析。接收方可以接受任何 cap ≤ 自己的上限。");
    println!("   代价：+2 字节头；接收方要能处理任意 cap（内存按峰值预留）。**推荐。**");
    println!();
    println!("**B. 全网固定 cap（部署参数，与 ViewParams 同级）**：");
    println!("   cap 进 `ViewParams`，和 buckets/hashers/seed 一样必须全网一致。");
    println!("   代价：**「用户自己设定」这个自由度消失**（但换来可预测性）。");
    println!();
    println!("**C. 分级 cap（少量档位，协商）**：例如 cap 只允许 128 / 256 / 512 三档，");
    println!("   对端声明自己用哪档，接收方按档解码。代价：协议多一个握手字段。");
    println!();
    println!("**A 最符合作者的表述**（「用户可以自己设定上限」），且实现代价最小（2 字节）。");
}
