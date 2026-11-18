//! cap 截断损失 vs 清单长度：这决定 cap 默认值该设多少。
//!
//! 作者 2026-11-17 明确「文件越大精度越高」。这个探针把那条曲线画出来：
//! 固定社会图，逐账户用 SparseView::truncated_mass 统计「清单多长开始丢质量」。
//!
//! 跑：cargo run --release --example cap_loss

use tm_mincore::SparseView;
use tm_sim::{AttackKind, SimConfig, World};

fn main() {
    // 建一次图（5 个 seed 合并统计）
    let mut all: Vec<(usize, f64)> = Vec::new();
    for seed in 1..=5u64 {
        let cfg = SimConfig {
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
            attack_kind: AttackKind::None,
            attack_ratio: 0.1,
            ..SimConfig::default()
        };
        let mut w = World::new(cfg);
        w.run();
        // 按观察者分组评分
        let mut by_obs: std::collections::BTreeMap<usize, Vec<(u32, f32)>> = Default::default();
        for (obs, tgt, score) in w.ratings() {
            by_obs.entry(obs).or_default().push((tgt as u32, score));
        }
        for (_, pairs) in by_obs {
            if pairs.is_empty() { continue; }
            all.push((pairs.len(), 0.0));
        }
    }

    // 清单长度分布
    let mut lens: Vec<usize> = all.iter().map(|&(l, _)| l).collect();
    lens.sort_unstable();
    let n = lens.len();
    if n == 0 {
        println!("没有评分数据");
        return;
    }
    println!("## 正分清单长度分布（{} 个账户视图）", n);
    println!();
    println!("| 分位 | 长度 |");
    println!("|---|---:|");
    for (name, p) in [("min", 0.0), ("p25", 0.25), ("p50", 0.5), ("p75", 0.75), ("p90", 0.9), ("p99", 0.99), ("max", 1.0)] {
        let idx = ((n - 1) as f64 * p).round() as usize;
        println!("| {} | {} |", name, lens[idx]);
    }

    // 对每个 cap 统计截断损失
    for &cap in &[64usize, 128, 256, 512, 1024] {
        let mut loss_sum = 0.0f64;
        let mut truncated = 0usize;
        let mut loss_max = 0.0f64;
        for &(l, _) in &all {
            let pairs: Vec<(u32, f32)> = (0..l).map(|i| (i as u32, 1.0f32 / l as f32)).collect();
            let loss = SparseView::truncated_mass(cap, &pairs);
            if loss > 0.0 { truncated += 1; }
            loss_sum += loss;
            if loss > loss_max { loss_max = loss; }
        }
        println!();
        println!("## cap = {}", cap);
        println!("被截断 {} / {} = {:.1}%，平均损失 {:.2}%，最大损失 {:.2}%",
            truncated, all.len(), truncated as f64 / all.len() as f64 * 100.0,
            loss_sum / all.len() as f64 * 100.0, loss_max * 100.0);
    }
}