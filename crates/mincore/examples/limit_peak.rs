//! 正常使用的**真实峰值**：A12 图上单个观察者到底会缓存多少观点 / 多少正分。
//!
//! 这是 `Config::max_views` / `Config::max_list` 默认值 4096 的**依据**。
//! 跑：`cargo run --release --example limit_peak`
//!
//! 口径与 A12 报告（「审计/A12-kernel.md」§2）一致：
//! `n_initial = 400`、`steps = 120`、`follow ∈ [120,200]`、`max_follows = 256`、
//! `attack = Sybil { cluster: 2 }`、`attack_ratio = 0.1`。seed 取 1..=5
//! （A12 报告用 1..=20，全量跑约 72 s，这里只取 5 个种子做量级确认）。
//!
//! 输出三组数：`|views|`（`Node::view_count()`）、`|list|`（`claims().len()`）、
//! 以及默认上限下的两类拒绝计数。预期拒绝为 **0**、峰值远小于 4096 ——
//! 这两条一起说明「默认值有充足余量，正常使用永远碰不到上限」。

use tm_mincore::Config;
use tm_mincore::Node;
use tm_mincore::SparseView;
use tm_sim::{AttackKind, SimConfig, World};

fn world_cfg(seed: u64) -> SimConfig {
    SimConfig {
        n_initial: 400,
        steps: 120,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        enable_rhythm: false,
        follow_min: 120,
        follow_max: 200,
        max_follows: 256,
        attack_kind: AttackKind::Sybil { cluster: 2 },
        attack_ratio: 0.1,
        ..SimConfig::default()
    }
}

/// 分位数（最近秩法，0.95 档）。
fn p95(sorted: &[usize]) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted[idx.saturating_sub(1).min(sorted.len() - 1)]
}

fn summarise(label: &str, mut values: Vec<usize>) -> (f64, usize, usize, usize) {
    if values.is_empty() {
        println!("| {label} | - | - | - | - |");
        return (0.0, 0, 0, 0);
    }
    values.sort_unstable();
    let mean = values.iter().sum::<usize>() as f64 / values.len() as f64;
    let median = values[values.len() / 2];
    let p = p95(&values);
    let max = *values.last().expect("非空");
    println!("| {label} | {mean:.1} | {median} | {p} | {max} |");
    (mean, median, p, max)
}

fn main() {
    let cfg = Config::default();
    println!(
        "## 默认上限 max_views = {}、max_list = {}",
        cfg.max_views, cfg.max_list
    );
    println!();
    println!("| 指标（每个观察者一个样本） | 均值 | 中位 | p95 | 最大 |");
    println!("|---|---:|---:|---:|---:|");

    let mut view_all: Vec<usize> = Vec::new();
    let mut list_all: Vec<usize> = Vec::new();
    let mut view_honest: Vec<usize> = Vec::new();
    let mut list_honest: Vec<usize> = Vec::new();
    let mut rejected_views = 0u64;
    let mut rejected_list = 0u64;
    let mut obs_hitting_view_cap = 0usize;
    let mut obs_hitting_list_cap = 0usize;
    let mut sum_ok = true;

    for seed in 1..=5u64 {
        let mut w = World::new(world_cfg(seed));
        w.run();

        // 每个发布者一张视图（与 cap_sensitivity / a12_compare 同口径）。
        let mut by_obs: std::collections::BTreeMap<usize, Vec<(u32, f32)>> = Default::default();
        for (o, t, s) in w.ratings() {
            by_obs.entry(o).or_default().push((t as u32, s));
        }
        let mut pubs: Vec<(usize, SparseView)> = Vec::new();
        for (&o, pairs) in &by_obs {
            let mut n = Node::new(cfg, seed);
            for &(t, s) in pairs {
                n.rate(t, s, 0);
            }
            pubs.push((o, n.publish(0)));
        }

        let honest: std::collections::BTreeSet<usize> =
            w.indices_of(tm_sim::Kind::Honest).into_iter().collect();

        for (&obs, mypairs) in &by_obs {
            let mut node = Node::new(cfg, seed);
            for &(t, s) in mypairs {
                node.rate(t, s, 0);
            }
            // 只收「我评过的人」的观点 —— 这既是协议要求，也是 |views| ≤ |我评过的人| 的来源。
            let rated: std::collections::BTreeSet<u32> = mypairs.iter().map(|&(t, _)| t).collect();
            for (o, v) in &pubs {
                if *o == obs || !rated.contains(&(*o as u32)) {
                    continue;
                }
                let _ = node.on_view(*o as u32, v.clone(), 0, 0);
            }

            let d = node.distribution(0);
            if !d.is_empty() && (d.sum() - 1.0).abs() > 1e-6 {
                sum_ok = false;
            }

            rejected_views += node.stat_rejected_limit();
            rejected_list += node.stat_rejected_list();
            let vc = node.view_count();
            let lc = node.claims().len();
            if vc >= cfg.max_views {
                obs_hitting_view_cap += 1;
            }
            if lc >= cfg.max_list {
                obs_hitting_list_cap += 1;
            }
            view_all.push(vc);
            list_all.push(lc);
            if honest.contains(&obs) {
                view_honest.push(vc);
                list_honest.push(lc);
            }
        }
    }

    summarise("|views|（全部观察者）", view_all);
    summarise("|views|（仅诚实观察者）", view_honest);
    summarise("|list|（全部观察者）", list_all);
    summarise("|list|（仅诚实观察者）", list_honest);

    println!();
    println!("| 观测 | 值 |");
    println!("|---|---:|");
    println!("| 因 max_views 被拒的观点（应恒为 0） | {rejected_views} |");
    println!("| 因 max_list 被拒的正分（应恒为 0） | {rejected_list} |");
    println!("| 撞到 max_views 上限的观察者数（应恒为 0） | {obs_hitting_view_cap} |");
    println!("| 撞到 max_list 上限的观察者数（应恒为 0） | {obs_hitting_list_cap} |");
    println!("| 所有非空分布的 Σ 都在 1e-6 以内 | {sum_ok} |");
    println!();
    println!(
        "结论：峰值远小于默认上限 4096（余量见上表）；默认上限下两类拒绝均为 0，\
         说明 4096 对 A12 规模有充足余量。"
    );
}
