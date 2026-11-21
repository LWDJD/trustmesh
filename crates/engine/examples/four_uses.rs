//! # 一套结构，四组参数：同一份代码做四种用途
//!
//! 作者的原始判断是：「这个架构主体不仅可以做反女巫，还可以做价值流动、内容推荐等，
//! 只是参数不同」。
//!
//! 这个例子**不实现任何新机制**，只是把同一份 `Engine` 用四组参数配出来，
//! 然后在**同一个社会里**跑，观察四种用途给出的排序差异有多大。
//!
//! 用法：
//!
//! ~~~powershell
//! cd crates/engine && cargo run --example four_uses
//! ~~~
//!
//! ## 四组参数（依据见 README「参数化：一套结构，多种用途」）
//!
//! | 用途 | β | 半衰期 | 聚合 | 语义 |
//! |---|---:|---:|---|---|
//! | 反女巫 | 0.3 | 72h | Mean | 尽量不分裂的一个大圈 |
//! | 内容推荐 | 0.9 | 720h | Mean | 兴趣相近的小圈，重个人判断 |
//! | 价值流动 | 0.5 | 24h | Median | 交易对手圈，抗单点操纵 |
//! | 反垃圾 | 0.5 | 168h | Trimmed | 信誉圈 |
//!
//! 关键观察：**同一个账户在四种用途下的「可信度」完全不同，而且这不是 bug**。
//! 反女巫想问的是「这个人会不会刷分」，价值流动想问的是「跟这个人交易安不安全」。
//! 它们本来就该给出不同的答案。

use std::collections::BTreeMap;

use tm_engine::agg::AggMode;
use tm_engine::view::ViewParams;
use tm_engine::{Engine, EngineConfig};

type Key = [u8; 32];

fn key(i: usize) -> Key {
    let mut k = [0u8; 32];
    k[0] = (i & 0xFF) as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0x4A;
    k
}

fn name(i: usize) -> String {
    format!("账户{i:02}")
}

/// 四种用途的参数。
fn configs() -> Vec<(&'static str, EngineConfig)> {
    let base = ViewParams::new(512, 5, 7);
    vec![
        (
            "反女巫  β=0.3 H=72h  Mean",
            EngineConfig {
                beta: 0.3,
                half_life: 72,
                view_params: base,
                agg_mode: AggMode::Mean,
                ..EngineConfig::default()
            },
        ),
        (
            "内容推荐 β=0.9 H=720h Mean",
            EngineConfig {
                beta: 0.9,
                half_life: 720,
                view_params: base,
                agg_mode: AggMode::Mean,
                ..EngineConfig::default()
            },
        ),
        (
            "价值流动 β=0.5 H=24h  Median",
            EngineConfig {
                beta: 0.5,
                half_life: 24,
                view_params: base,
                agg_mode: AggMode::Median,
                ..EngineConfig::default()
            },
        ),
        (
            "反垃圾  β=0.5 H=168h Trimmed",
            EngineConfig {
                beta: 0.5,
                half_life: 168,
                view_params: base,
                agg_mode: AggMode::Trimmed(0.1),
                ..EngineConfig::default()
            },
        ),
    ]
}

/// 造一个 12 人的社会：三种典型角色。
///
/// * 0..4：**高质量**（很多人给它打高分）
/// * 4..8：**普通**
/// * 8..12：**低质量**（被少数人打高分，但大部分人评价低）—— 这就是女巫/垃圾内容的典型形状
fn build_society() -> (Vec<Key>, Vec<(usize, usize, f32)>) {
    let keys: Vec<Key> = (0..12).map(key).collect();
    let mut ratings: Vec<(usize, usize, f32)> = Vec::new();

    // 高质量账户：被 8 个人打 0.8~0.95 分
    for t in 0..4 {
        for o in 0..8 {
            if o != t {
                ratings.push((o, t, 0.80 + 0.05 * ((o + t) % 4) as f32));
            }
        }
    }
    // 普通账户：被 3~4 个人打 0.5 分
    for t in 4..8 {
        for o in 0..4 {
            if o != t {
                ratings.push((o, t, 0.50 + 0.02 * (o as f32)));
            }
        }
    }
    // 低质量账户：被 2 个同伙打 1.0，其余 6 个人打 0.1
    for t in 8..12 {
        ratings.push((8 + (t % 4), t, 1.0));
        for o in 0..6 {
            ratings.push((o, t, 0.10));
        }
    }
    (keys, ratings)
}

fn main() {
    let (keys, ratings) = build_society();

    // ⚠️ 这里的关键：**每个账户一个引擎**（真实网络的样子），不是让观察者 0 一个人自言自语。
    // 之前那一版只建一个引擎、把它自己导出的视图喂给自己，群体项永远等于自己的读数，
    // 所以四组参数输出完全一样——那是个错误示范，不是这个库的行为。
    for (label, cfg) in configs() {
        let now = 12;
        let mut engines: Vec<Engine> = (0..keys.len())
            .map(|i| {
                let mut e = Engine::with_seed(cfg.clone(), 0xF0F0_0000_0000_0001 + i as u64);
                for k in &keys {
                    e.intern(*k);
                }
                for &(o, t, s) in &ratings {
                    if o == i {
                        e.rate(t as u32, s, 0);
                    }
                }
                e
            })
            .collect();

        // 三轮传播：每轮导出 → 抽样 → 收取。k=4 与 A10 对小清单的推荐值一致。
        for round in 0..3u64 {
            let t_now = now + round * 12;
            let published: Vec<_> = engines.iter().map(|e| e.export_view(t_now)).collect();
            let fetches: Vec<Vec<u32>> = engines
                .iter_mut()
                .map(|e| e.tick(t_now, 4).into_iter().map(|r| r.idx).collect())
                .collect();
            for (i, reqs) in fetches.iter().enumerate() {
                for &owner in reqs {
                    engines[i].on_view(owner, published[owner as usize].clone(), t_now, t_now);
                }
            }
        }

        println!("{label}");
        println!("  {:<10} {:>8}  {:>4}  角色", "账户", "声望", "排名");

        // 观察者 0 眼中每个人的可信度
        let dist: BTreeMap<usize, f32> = engines[0]
            .trust_distribution(now + 24)
            .into_iter()
            .map(|(i, v)| (i as usize, v))
            .collect();
        let mut sorted: Vec<(usize, f32)> = (0..12)
            .map(|i| (i, *dist.get(&i).unwrap_or(&0.0)))
            .collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (i, v)) in sorted.iter().enumerate() {
            let tag = if *i < 4 {
                "高质量"
            } else if *i < 8 {
                "普通"
            } else {
                "低质量（2 个同伙打 1.0 + 6 个陌生人打 0.1）"
            };
            println!(
                "  {:<10} {:>7.2}%  {:>4}  {}",
                name(*i),
                v * 100.0,
                rank + 1,
                tag
            );
        }
        println!();
    }

    println!("怎么读这四组数：");
    println!("  反女巫（β=0.3，群体 70%）：低质量账户被大多数人的差评压住 —— 这是防女巫的形状");
    println!(
        "  内容推荐（β=0.9，个人 90%）：『我自己怎么评的就怎么算』主导 —— 个性化推荐要的正是这个"
    );
    println!("  价值流动（Median）：完全不听权重，只看多数人的判断 —— 抗单点操纵");
    println!("  反垃圾（Trimmed 削 10% 两端）：介于 Mean 与 Median 之间的折中");
    println!();
    println!("这不是四个算法，是**同一个引擎四组参数**。差异全部来自 β / H / 聚合方式。");
}
