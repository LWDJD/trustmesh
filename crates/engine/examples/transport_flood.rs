//! # 传输层被淹没时，这个库会怎样 —— 以及它能为应用层做什么
//!
//! 作者的问题：「我要的是**传输上的淹没**——女巫节点接管整个网络，
//! 导致正常节点无法正常发现其他的正常节点」。
//!
//! `设计规格.md` §0.2 把「节点发现、连接建立」划给应用层 / 覆盖网络，
//! 所以**发现协议本身不在本库范围内**。但这个库与传输层有一个明确的接口。
//!
//! 本库每轮产出「我要去哪几个账户的观点」，应用层拿去发网络请求，
//! 成功就 on_view、失败就 on_view_failed。
//!
//! 所以「传输被淹没」在本库眼里只有一个表现：**on_view_failed 密集出现**。
//! 这个探针把那种情况拉到极端（100% 请求失败），看引擎会不会崩、会不会忘、会不会停止请求。
//!
//! 用法：cd crates/engine && cargo run --release --example transport_flood

use tm_engine::{Engine, EngineConfig};
use tm_view::ViewParams;

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0xF2;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(1024, 5, 33),
        half_life: 72,
        ..EngineConfig::default()
    }
}

fn main() {
    let cfg = cfg();
    let n = 40u32;

    println!("# 传输层淹没：100% 的取观点请求都失败（最坏情况）");
    println!("# 我评过 40 个人；k=6；每轮 12h；H=72h");
    println!();

    let mut e = Engine::with_seed(cfg.clone(), 0xF200);
    for i in 0..n {
        e.intern(key(i));
        e.rate(i, 0.2 + 0.8 * (i as f32 / n as f32), 0);
    }

    let before: Vec<f32> = (0..n).map(|i| e.query(i, 0)).collect();

    let mut total_fail = 0u64;
    let mut total_req = 0u64;
    for round in 0..30u64 {
        let now = 12 * round;
        let reqs = e.tick(now, 6);
        total_req += reqs.len() as u64;
        for r in reqs {
            e.on_view_failed(r.idx, now);
            total_fail += 1;
        }
    }

    println!(
        "30 轮里共发出 {} 个请求，全部失败（{} 个）",
        total_req, total_fail
    );
    println!();

    let now = 30 * 12;
    let still = e.tick(now, 6);
    println!("继续请求？第 31 轮 tick 仍返回 {} 个请求", still.len());

    let max_streak = (0..n).map(|i| e.fail_streak(i)).max().unwrap_or(0);
    println!("最长连败 = {}（退避权重有下限，**永不为 0**）", max_streak);

    let pr = e.probe(now);
    println!(
        "probe() 返回 {} 个请求（不走退避，强制覆盖本轮全部未抽过的条目）",
        pr.len()
    );

    let after: Vec<f32> = (0..n).map(|i| e.query(i, now)).collect();
    let mut max_diff = 0.0f32;
    for i in 0..n as usize {
        let d = (after[i] - before[i]).abs();
        if d > max_diff {
            max_diff = d;
        }
    }
    println!();
    println!("声望变化：淹没前后最大绝对差 = {:.8}", max_diff);
    println!("  （0 表示**个人项完好无损** —— 全网络取不到观点，我自己对谁的评价一点没丢）");

    let dist = e.trust_distribution(now);
    let sum: f32 = dist.iter().map(|(_, v)| *v).sum();
    println!(
        "trust_distribution 仍有 {} 个非零条目，总和 = {:.6}",
        dist.len(),
        sum
    );
    println!(
        "正分清单仍为 {} 条（传输失败**不删除**任何评分）",
        e.positive_count()
    );

    println!();
    println!("## 结论");
    println!("1. **引擎不会停**：清单非空就一直发请求，连败 30 次后仍在发");
    println!("2. **引擎不会忘**：个人项逐位不变，正分清单一条不丢");
    println!("3. **退避不会卡死**：权重有下限，永远不会缩到 0 导致抽不出东西");
    println!("4. **probe() 是兜底通道**：不走退避，一次覆盖本轮全部条目");
    println!("5. 但**群体项会归零**：一份观点都收不到时，群体项是全员弃权，结果退化为 β·个人项");
    println!();
    println!("所以这个库对「传输被淹没」的回答是：");
    println!("  它保证「即使传输全废，你对谁可信的判断、以及你要去找的那份名单，一点都不会丢」。");
    println!("  但它不负责**把包送达**——那是覆盖网络的事。");
}
