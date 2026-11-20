//! cap 敏感性：cap 变大，反女巫效果和分布覆盖怎么变。
//!
//! `cap_loss` 回答「截断丢多少质量」；这个回答「丢了之后反女巫还强不强」。
//!
//! 注意：mincore 的 S1（女巫声望）**恒为精确 0**，与 cap 无关 —— 这是定长表的结构性质，
//! 不是参数调出来的。所以这张表不看 S1，看的是「分布里出现多少账户」和「诚实账户的分化程度」。
//!
//! 跑：cargo run --release --example cap_sensitivity

use tm_mincore::{Config, Node, SparseView};
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

fn main() {
    println!("## cap 敏感性（5 seed × 440 账户，女巫场景）");
    println!();
    println!(
        "| cap | 字节 | 分布条数 | 覆盖账户 | S1 女巫均值 | S3 诚实最大值÷均值 | 被截断视图 |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");

    for &cap in &[64usize, 128, 256, 512] {
        let cfg = Config {
            cap,
            ..Default::default()
        };
        let mut dist_len = 0usize;
        let mut coverage = 0.0f64;
        let mut s1 = 0.0f64;
        let mut s3 = 0.0f64;
        let mut truncated = 0usize;
        let mut n_obs = 0usize;

        for seed in 1..=5u64 {
            let mut w = World::new(world_cfg(seed));
            w.run();

            let mut by_obs: std::collections::BTreeMap<usize, Vec<(u32, f32)>> = Default::default();
            for (o, t, s) in w.ratings() {
                by_obs.entry(o).or_default().push((t as u32, s));
            }
            // 注意：Kind::Sybil { cluster } 的 cluster 是身份的一部分，
            // 所以 indices_of(Sybil{cluster:2}) 只拿到第 3 个集群。这里要全部集群。
            let sybils: Vec<usize> = (0..4)
                .flat_map(|c| w.indices_of(tm_sim::Kind::Sybil { cluster: c }))
                .collect();
            let honest = w.indices_of(tm_sim::Kind::Honest);
            if sybils.is_empty() || honest.is_empty() {
                continue;
            }
            let n_acct = w.accounts().len();

            // 先为每个发布者算一份视图
            let mut pubs: Vec<(usize, SparseView, usize)> = Vec::with_capacity(by_obs.len());
            for (&o, pairs) in &by_obs {
                let mut n = Node::new(cfg, seed);
                for &(t, s) in pairs {
                    n.rate(t, s, 0);
                }
                let v = n.publish(0);
                pubs.push((o, v, pairs.len()));
            }

            // 每个观察者收「我评过的人」的视图
            // ⚠️ 只统计**诚实观察者**：女巫观察者会看到互相刷分的女巫，
            // 那是设计上预期的（他们互评），不是 S1 要测的东西。
            let honest_only: std::collections::BTreeSet<usize> = honest.iter().cloned().collect();
            for (&obs, mypairs) in &by_obs {
                if !honest_only.contains(&obs) {
                    continue;
                }
                let mut node = Node::new(cfg, seed);
                for &(t, s) in mypairs {
                    node.rate(t, s, 0);
                }
                let rated: std::collections::BTreeSet<u32> =
                    mypairs.iter().map(|&(t, _)| t).collect();
                for (o, v, len) in &pubs {
                    if *o == obs || !rated.contains(&(*o as u32)) {
                        continue;
                    }
                    if v.len() < *len {
                        truncated += 1;
                    }
                    let _ = node.on_view(*o as u32, v.clone(), 0, 0);
                }

                let d = node.distribution(0);
                dist_len += d.entries.len();
                coverage += d.entries.len() as f64 / n_acct as f64;

                let sv: f64 = sybils
                    .iter()
                    .map(|&i| f64::from(node.query(i as u32, 0)))
                    .sum::<f64>()
                    / sybils.len() as f64;
                s1 += sv;

                let h: Vec<f64> = honest
                    .iter()
                    .map(|&i| f64::from(node.query(i as u32, 0)))
                    .collect();
                let mean = h.iter().sum::<f64>() / h.len() as f64;
                let mx = h.iter().cloned().fold(0.0f64, f64::max);
                if mean > 0.0 {
                    s3 += mx / mean;
                }
                n_obs += 1;
            }
        }

        if n_obs == 0 {
            println!("| {} | - | - | - | - | - | - |  (无观察者)", cap);
            continue;
        }
        let k = n_obs as f64;
        let bytes = SparseView::new(cap).bytes();
        println!(
            "| {} | {} | {} | {:.1}% | {:.6} | {:.4} | {} |",
            cap,
            bytes,
            dist_len / n_obs,
            coverage / k * 100.0,
            s1 / k,
            s3 / k,
            truncated
        );
    }
}
