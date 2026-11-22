//! # 淹没实验：女巫/垃圾把正分清单撑满之后会发生什么
//!
//! 这个库**不排序内容**，它算的是「谁可信」。所以对「女巫用垃圾淹没正常信息」这个问题，
//! 在本库职责范围内能对应的只有两条真实路径：
//!
//! 1. **挤占抽样配额**：清单变长 + k 固定 ⇒ 刷新周期 (n/k)·T 变长 ⇒ 衰减因子全线塌陷（A10 的机制）
//! 2. **挤占可信度份额**：清单里垃圾变多 ⇒ 诚实账户的份额被摊薄 ⇒ 声望被稀释
//!
//! 这个探针把这两条分别量化，并且**同时报告第三种最坏情况**：
//! 垃圾账户是被**我信任的人**推荐进来的（渗透 → 垃圾），这条是这个库真正挡不住的。
//!
//! 用法：cd crates/engine && cargo run --example flood_probe

use tm_engine::view::ViewParams;
use tm_engine::{Engine, EngineConfig};

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0xF1;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(1024, 5, 21),
        half_life: 72,
        ..EngineConfig::default()
    }
}

/// 一个观察者的结果。
struct Row {
    n_list: usize,       // 正分清单长度
    n_garbage: usize,    // 其中垃圾账户数
    honest_share: f32,   // 诚实账户占清单的份额
    garbage_share: f32,  // 垃圾账户占清单的份额
    honest_score: f32,   // 诚实账户的声望
    garbage_score: f32,  // 垃圾账户的声望
    refresh_rounds: f32, // 一轮刷完清单需要几轮（n_list / k）
}

/// 场景：我自己评了 `n_honest` 个诚实账户（各 1.0）+ `n_garbage` 个垃圾账户（各 `own_garbage`），
/// 后者每个都被 `endorse` 个「我信任的来源」背书（= 渗透进来的垃圾）。
fn run(k: usize, n_honest: usize, n_garbage: usize, endorse: usize, own_garbage: f32) -> Row {
    let total = 1 + n_honest + n_garbage + endorse; // 0 是我
    let mut e = Engine::with_seed(cfg(), 0xF100);
    for i in 0..total as u32 {
        e.intern(key(i));
    }

    // 我给诚实账户打 1.0，给垃圾账户打 **0.5**（「不太确定，先给点」—— 这是现实中常见的情况）
    for i in 0..n_honest {
        e.rate(1 + i as u32, 1.0, 0);
    }
    for i in 0..n_garbage {
        e.rate(1 + n_honest as u32 + i as u32, own_garbage, 0);
    }

    // 背书者（我信任的来源）：编号在最后，我先给它们打 0.8（说明我信任它们）
    let endorse_start = 1 + n_honest + n_garbage;
    for i in 0..endorse {
        e.rate((endorse_start + i) as u32, 0.8, 0);
    }

    // 每个背书者发布一份观点：给每个垃圾账户打 1.0
    let params = cfg().view_params;
    let mut published = Vec::new();
    for _ in 0..endorse {
        let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
        for g in 0..n_garbage {
            pairs.push((key((1 + n_honest + g) as u32), 1.0));
        }
        // 也给自己和诚实账户一点正常读数，别让 total_mass 太偏
        for h in 0..n_honest {
            pairs.push((key((1 + h) as u32), 0.6));
        }
        published.push(tm_engine::view::CompressedView::from_pairs(params, &pairs));
    }

    // 收观点
    let now = 12;
    for (i, view) in published.iter().enumerate() {
        e.on_view((endorse_start + i) as u32, view.clone(), now, now);
    }

    // 一轮 tick 只抽 k 个（k 固定，清单变长 ⇒ 刷新周期变长）
    let _ = e.tick(now, k);

    let dist = e.trust_distribution(now);
    let mut honest_sum = 0.0f32;
    let mut garbage_sum = 0.0f32;
    for (idx, v) in &dist {
        let i = *idx as usize;
        if (1..1 + n_honest).contains(&i) {
            honest_sum += *v;
        } else if (1 + n_honest..1 + n_honest + n_garbage).contains(&i) {
            garbage_sum += *v;
        }
    }

    let n_list = e.positive_count();
    Row {
        n_list,
        n_garbage,
        honest_share: (n_honest as f32 / n_list as f32),
        garbage_share: (n_garbage as f32 / n_list as f32),
        honest_score: if n_honest > 0 {
            honest_sum / n_honest as f32
        } else {
            0.0
        },
        garbage_score: if n_garbage > 0 {
            garbage_sum / n_garbage as f32
        } else {
            0.0
        },
        refresh_rounds: n_list as f32 / k as f32,
    }
}

fn main() {
    println!("# 淹没实验：垃圾账户挤进正分清单 / 被渗透者背书之后");
    println!(
        "# k = 6（固定），H = 72h，T = 12h；A10 健康上限：刷完一轮 ≤ H/2 = 3 轮（即 n/k ≤ 3）"
    );
    println!();

    // 关键对照组：出问题不是因为「垃圾多」，而是因为「我信任的人里被收买的比例」。
    println!("## A. 垃圾靠**数量**硬挤（没人背书）");
    println!(
        "{:<8} {:<8} {:<10} {:<10} {:<12} {:<12} {:<10}",
        "垃圾数", "清单长", "垃圾份额", "诚实份额", "垃圾/诚实", "诚实声望", "刷完几轮"
    );
    for &g in &[0usize, 40, 200, 800] {
        let r = run(6, 40, g, 0, 0.5);
        let ratio = if r.honest_score > 0.0 {
            r.garbage_score / r.honest_score
        } else {
            0.0
        };
        println!(
            "{:<8} {:<8} {:<10.3} {:<10.3} {:<12.2} {:<12.5} {:<10.1}",
            r.n_garbage,
            r.n_list,
            r.garbage_share,
            r.honest_share,
            ratio,
            r.honest_score,
            r.refresh_rounds
        );
    }

    println!();
    println!("## B. 垃圾靠**我信任的人**背书（这才是真正的淹没路径）");
    println!(
        "{:<8} {:<8} {:<10} {:<10} {:<12} {:<12} {:<12}",
        "垃圾数", "背书者", "背书占比", "垃圾份额", "垃圾/诚实", "垃圾声望", "诚实声望"
    );
    for &g in &[40usize, 200] {
        for &e in &[0usize, 10, 20, 40] {
            let r = run(6, 40, g, e, 0.5);
            let ratio = if r.honest_score > 0.0 {
                r.garbage_score / r.honest_score
            } else {
                0.0
            };
            println!(
                "{:<8} {:<8} {:<10.2} {:<10.3} {:<12.2} {:<12.5} {:<12.5}",
                g,
                e,
                e as f32 / (40.0 + e as f32),
                r.garbage_share,
                ratio,
                r.garbage_score,
                r.honest_score
            );
        }
    }

    println!();
    println!("## C. 最坏情况：我自己就给垃圾打了高分（我误判）");
    println!(
        "{:<8} {:<10} {:<12} {:<12} {:<12}",
        "垃圾数", "我的评分", "垃圾/诚实", "垃圾声望", "诚实声望"
    );
    for &(g, own) in &[(40usize, 1.0f32), (200, 1.0), (200, 0.5), (200, 0.1)] {
        let r = run(6, 40, g, 0, own);
        let ratio = if r.honest_score > 0.0 {
            r.garbage_score / r.honest_score
        } else {
            0.0
        };
        println!(
            "{:<8} {:<10.2} {:<12.2} {:<12.5} {:<12.5}",
            g, own, ratio, r.garbage_score, r.honest_score
        );
    }

    println!();
    println!("读法：");
    println!("  A 组：垃圾**数量**再多，每个也只拿到「我给它评分」的那个比例 —— 影响力预算在守");
    println!("        但**刷完一轮要几轮**线性上升（清单被撑长），这才是配额被挤占");
    println!("  B 组：背书者占比越高，垃圾的群体项越被顶起来 —— 这是真正挡不住的路径");
    println!("  C 组：我自己误判给垃圾高分时，它当然拿高分 —— 这不是算法问题，是我评错了");
}
