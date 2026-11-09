//! 无攻击时，评分必须反映内容质量。
//!
//! 这是「社会细节建模是否正确」的基本检验：如果质量高的账户拿不到高分，
//! 后面的攻击实验就没有意义。

use tm_sim::{SimConfig, World};

/// 取出「至少收到 3 条评分」的诚实账户的 (质量, 收到均分)。
fn 质量与均分(w: &World) -> Vec<(f32, f32)> {
    let mut v: Vec<(f32, f32)> = w
        .accounts
        .iter()
        .filter(|a| a.kind == tm_sim::Kind::Honest && a.received_count >= 3)
        .map(|a| (a.quality, a.mean_received()))
        .collect();
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    v
}

#[test]
fn 诚实账户的收到均分与质量强相关() {
    let cfg = SimConfig {
        n_initial: 300,
        steps: 150,
        seed: 3,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    let snap = w.report();
    assert!(
        snap.honest_quality_corr > 0.75,
        "质量与均分的相关只有 {}，评分没反映出质量",
        snap.honest_quality_corr
    );

    let items = 质量与均分(&w);
    assert!(items.len() > 100, "样本太少：{}", items.len());
    let q = items.len() / 4;
    let 低质量: f32 = items[..q].iter().map(|x| x.1).sum::<f32>() / q as f32;
    let 高质量: f32 = items[items.len() - q..].iter().map(|x| x.1).sum::<f32>() / q as f32;
    assert!(
        高质量 > 低质量 * 1.3,
        "质量前 1/4 的均分 {高质量:.3} 没有显著高于后 1/4 的 {低质量:.3}"
    );
    // 质量最高的账户，均分必须落在合理区间（不能是 0，也不能全都顶到 1）
    assert!(高质量 > 0.55 && 高质量 <= 1.0, "高质量组均分 {高质量}");
    assert!(低质量 > 0.05, "低质量组均分 {低质量} 不该接近 0");
}

#[test]
fn 评分全部落在开区间零到一() {
    let cfg = SimConfig {
        n_initial: 120,
        steps: 100,
        seed: 4,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    let ratings = w.ratings();
    assert!(!ratings.is_empty());
    for (s, u, score) in &ratings {
        assert!(score.is_finite(), "{s}->{u} 的评分不是有限值");
        assert!(*score > 0.0, "{s}->{u} 出现 0 分（0 应该表示没表态，不写入）");
        assert!(*score <= 1.0, "{s}->{u} 评分 {} 超过 1", score);
        assert!(*score >= w.cfg.min_stance - 1e-6, "低于表态阈值的分不该被记录");
    }
    let d = w.report().distribution;
    assert_eq!(d.count, ratings.len());
    assert!(d.min > 0.0);
    assert!((0.3..0.7).contains(&d.mean), "整体均分 {} 偏离常识区间", d.mean);
    assert!(d.std > 0.05, "评分完全没有差异（std={}）", d.std);
    assert!(d.p10 <= d.p25 && d.p25 <= d.p50 && d.p50 <= d.p75);
    assert!(d.p75 <= d.p90 && d.p90 <= d.p99);
}

#[test]
fn 兴趣匹配影响评分() {
    // 社团内的人兴趣更接近 → 社内评分应高于社间
    let cfg = SimConfig {
        n_initial: 400,
        steps: 120,
        homophily: 0.3,
        seed: 5,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    let s = w.report();
    assert!(s.community.rating_intra > 100);
    assert!(s.community.rating_inter > 30);
    assert!(
        s.community.intra_mean > s.community.inter_mean,
        "社团内均分 {:.3} 应高于社团间 {:.3}",
        s.community.intra_mean,
        s.community.inter_mean
    );
    assert!(s.community.cliquishness > 1.0);
}

#[test]
fn 关闭噪声后质量排序仍保持() {
    // 噪声为 0、内容抖动为 0 时，评分 = 质量 × 兴趣匹配，质量排序必须保留
    let cfg = SimConfig {
        n_initial: 250,
        steps: 120,
        rating_noise: 0.0,
        content_noise: 0.0,
        seed: 6,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    let items = 质量与均分(&w);
    assert!(items.len() > 80);
    // 质量分 5 档，均值应严格单调上升
    let mut means = Vec::new();
    let chunk = items.len() / 5;
    for k in 0..5 {
        let slice = &items[k * chunk..(k + 1) * chunk];
        let m: f32 = slice.iter().map(|x| x.1).sum::<f32>() / slice.len() as f32;
        means.push(m);
    }
    for i in 1..means.len() {
        assert!(
            means[i] > means[i - 1],
            "去掉噪声后按质量分档的均分没有单调上升：{means:?}"
        );
    }
}
