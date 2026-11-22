//! # 传输被接管后，攻击者能伪造到什么程度
//!
//! 传输层被女巫接管 = 攻击者可以**冒充任何对端**。除非应用层做了签名验证，
//! 否则攻击者可以把我收到的每一份观点都换成它编造的。
//!
//! 这是本库范围内**最坏的传输威胁**，比「请求失败」严重得多。
//! 这个探针把它推到极限：
//!
//! * 我信任的 8 个人，每一份观点都被攻击者换成伪造的；
//! * 伪造的观点里全是攻击者自己（500 个女巫，各 100 分）；
//! * 看我的声望会变成什么样。
//!
//! 用法：cd crates/engine && cargo run --release --example transport_mitm

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = 0xB6;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(2048, 5, 43),
        half_life: 72,
        ..EngineConfig::default()
    }
}

fn main() {
    let cfg = cfg();
    let params = cfg.view_params;
    let n_friends = 8u32;
    let n_sybil = 500u32;

    println!("# 传输被接管：攻击者冒充我全部 8 个朋友，投递伪造观点");
    println!();

    let mut e = Engine::with_seed(cfg.clone(), 0xB600);
    for i in 0..n_friends {
        e.intern(key(0x01, i));
        e.rate(i, 1.0, 0);
    }
    for i in 0..n_sybil {
        e.intern(key(0x02, i));
    }

    // 先让引擎正常收一轮「真」观点：朋友们互相给 0.8，对女巫一分不给
    for i in 0..n_friends {
        let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
        for j in 0..n_friends {
            if j != i {
                pairs.push((key(0x01, j), 0.8));
            }
        }
        let v = CompressedView::from_pairs(params, &pairs);
        e.on_view(i, v, 0, 0);
    }
    let honest_baseline: Vec<f32> = (0..n_friends).map(|i| e.query(i, 0)).collect();
    let sybil_baseline = e.query(n_friends, 0);
    println!(
        "正常状态：朋友声望均值 {:.6}，女巫声望 {:.6}",
        honest_baseline.iter().sum::<f32>() / n_friends as f32,
        sybil_baseline
    );

    // 攻击者接管传输：把每一份朋友的观点都换成「全给女巫 100 分」
    let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
    for i in 0..64u32 {
        pairs.push((key(0x02, i), 100.0));
    }
    let forged = CompressedView::from_pairs(params, &pairs);
    let now = 12;
    for i in 0..n_friends {
        e.on_view(i, forged.clone(), now, now);
    }

    let honest_after: Vec<f32> = (0..n_friends).map(|i| e.query(i, now)).collect();
    let sybil_after = e.query(n_friends, now);
    let honest_mean = honest_after.iter().sum::<f32>() / n_friends as f32;
    println!();
    println!(
        "传输被接管后：朋友声望均值 {:.6}，女巫声望 {:.6}",
        honest_mean, sybil_after
    );
    println!("  女巫/朋友 比值 = {:.4}", sybil_after / honest_mean);

    // 关键：攻击者能改的是**视图内容**，不能改的是**谁在我的清单里**
    println!();
    println!("但是：");
    println!("  正分清单仍为 {} 条（只有我评过的人）", e.positive_count());
    println!(
        "  女巫在清单里？{}（它们进不来，因为我没有给它们评分）",
        if (0..n_sybil).any(|i| e.share_of(n_friends + i).is_some()) {
            "是"
        } else {
            "不是"
        }
    );
    let shares = e.shares();
    println!(
        "  我的引导名单（shares）仍有 {} 条，全是我评过的朋友",
        shares.len()
    );

    println!();
    println!("## 结论");
    println!("1. **攻击者能伪造「朋友们说了什么」，但不能伪造「谁是我的朋友」**。");
    println!("   清单与份额由我自己的评分决定，传输层碰不到。");
    println!("2. 所以传输接管的最坏后果是 **群体项被污染** —— 这正是 S3/S5 已经量化的东西，");
    println!("   不是「我的名单被篡改」。");
    println!("3. **应用层必须做的**：观点签名验证。本库不做验签（设计规格 §0.2「加密与认证」归应用层），");
    println!("   所以「传输被冒充」在本库层面只能被**减损**，不能被消除。");
}
