//! # 五分钟上手：一个最小可用的 trustmesh 网络
//!
//! 跑法：
//!
//! ```text
//! cd crates/engine && cargo run --example quickstart
//! ```
//!
//! 这个例子演示应用层**必须**做的五件事，顺序不能乱：
//!
//! 1. 给每个节点建一个 `Engine`，把认识的账户全部 `intern`（编号顺序必须全网一致）
//! 2. `rate` 打分（0~1，1 是满分）
//! 3. 每隔一段时间 `tick(now, k)` → 拿到「我需要谁的观点」
//! 4. 真正去取那些观点文件（网络 I/O 是你的责任，库不做），取到后 `on_view`
//! 5. 想查「我眼中某人有多可信」用 `query`；想发布自己的观点用 `export_view`
//!
//! 库**不负责**：网络、文件、签名、限流、DDoS 防护、内容托管。
//! 详细边界见仓库根目录的 `设计规格.md` §0.2。

use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

/// 账户号。真实部署里它是公钥的 32 字节表示；这里用便于阅读的短名。
type Key = [u8; 32];

fn key(name: &str) -> Key {
    // 真实部署不要这样做——直接用公钥字节。这里只是把可读名字塞进 32 字节。
    let mut k = [0u8; 32];
    k[..name.len()].copy_from_slice(name.as_bytes());
    k
}

fn main() {
    // 全网必须用**同一套** ViewParams（形状 + 种子）。不一致的观点会被整条丢弃。
    let view_params = ViewParams::new(2048, 5, 0);

    let cfg = EngineConfig {
        beta: 0.3,     // 个人判断 30% + 群体共识 70%（作者定，别改）
        half_life: 72, // 小时。这是「伤害时效」，不是反女巫机制
        view_params,
        ..EngineConfig::default()
    };

    // 一个 6 人的小网络。编号 i 对应 KEYS[i]，这个顺序全网一致。
    let keys: Vec<Key> = ["alice", "bob", "carol", "dave", "erin", "frank"]
        .iter()
        .map(|n| key(n))
        .collect();

    // 每个在线节点各有一个引擎。真实部署里这些引擎分布在不同的机器上。
    let mut engines: Vec<Engine> = keys
        .iter()
        .map(|_| {
            let mut e = Engine::with_seed(cfg.clone(), 7);
            for k in &keys {
                e.intern(*k); // 顺序必须一致：编号 = intern 的先后次序
            }
            e
        })
        .collect();

    // ---- 第 1 步：打分 ----
    // alice 信任 bob 和 carol；bob 信任 alice 和 frank……（0~1，1 是满分）
    engines[0].rate(1, 0.9, 0); // alice → bob
    engines[0].rate(2, 0.7, 0); // alice → carol
    engines[1].rate(0, 0.8, 0); // bob   → alice
    engines[1].rate(5, 0.6, 0); // bob   → frank
    engines[2].rate(0, 0.8, 0); // carol → alice
    engines[3].rate(1, 0.9, 0); // dave  → bob

    // ---- 第 2 步：抽样 + 交换观点，跑 3 轮 ----
    for round in 0..3 {
        let now = 12 * (round + 1); // 每轮 12 小时

        // 2a. 每个人先导出自己的观点（真实部署里是「上传到某处」）
        let mut published: Vec<CompressedView> = Vec::new();
        for e in &engines {
            published.push(e.export_view(now));
        }

        // 2b. 每个人问「我该取谁的观点」
        //     k 是本轮抽几个。A10 的硬约束：(清单长度/k)·T ≤ 半衰期/2
        let k = 3;
        let mut to_fetch: Vec<Vec<u32>> = Vec::new();
        for e in &mut engines {
            to_fetch.push(e.tick(now, k).into_iter().map(|r| r.idx).collect());
        }

        // 2c. 真正取观点（这里是内存直取；真实部署是网络 I/O）
        for (i, reqs) in to_fetch.iter().enumerate() {
            for &owner in reqs {
                let view = published[owner as usize].clone();
                // on_view(来源编号, 观点, 对方声明的发布时间, 我收到的时间)
                engines[i].on_view(owner, view, now, now);
            }
        }
    }

    // ---- 第 3 步：查声望 ----
    let now = 36;
    println!("alice 眼中的可信度分布（已归一化）：");
    let dist = engines[0].trust_distribution(now);
    for (idx, v) in &dist {
        let name = String::from_utf8_lossy(&keys[*idx as usize][..5]).to_string();
        println!("  {name:<8} {:>6.2}%", v * 100.0);
    }

    // ---- 第 4 步：自查观点新鲜度 ----
    // 违反 (n/k)·T ≤ H/2 时，衰减会变成噪声（A10 实测：90.7% 的来源 fs<0.1）
    let n_list = engines[0].positive_count();
    let half_life = 72;
    let period = 12; // 小时
    let stats = engines[0].freshness_stats(now);
    println!();
    println!(
        "alice 的正分清单 {n_list} 条，已缓存 {} 条观点",
        stats.count
    );
    println!(
        "其中 {} 条在半个半衰期（{}h）内仍算新鲜；年龄中位数 {}h",
        stats.fresh_within_half_life,
        half_life / 2,
        stats.median_age
    );
    // 硬约束：刷新周期 (n_list/k)·T ≤ H/2 ⟹ k ≥ 2·n_list·T/H
    let need_k = if half_life > 0 {
        (2 * n_list as u64 * period).div_ceil(half_life).max(1)
    } else {
        n_list as u64
    };
    println!("每 {period}h 一轮的话，k 至少要 {need_k}（本例子用了 k=3）");

    // ---- 第 5 步：跨平台可复现的检查 ----
    // 同一个 seed、同样的输入，任何平台上 query 的结果必须逐位相同。
    println!();
    println!(
        "alice 眼中 bob = {:.8}（f32 位模式 0x{:08x}）",
        engines[0].query(1, now),
        engines[0].query(1, now).to_bits()
    );
    println!("这个位模式在 x86_64 / aarch64 / wasm32 上都应该是同一个值。");
}
