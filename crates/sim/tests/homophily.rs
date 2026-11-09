//! 同质性（homophily）必须能被参数明显推动。
//!
//! 判定标准来自任务书：同质性 0 → 1 时，社团内关注边占比显著上升。

use tm_sim::{SimConfig, World};

fn 内占比(h: f64, seed: u64) -> f32 {
    let cfg = SimConfig {
        n_initial: 200,
        steps: 120,
        homophily: h,
        seed,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    w.report().community.follow_intra_share
}

#[test]
fn 同质性零到一显著提升社团内边占比() {
    for seed in [11u64, 12] {
        let low = 内占比(0.0, seed);
        let high = 内占比(1.0, seed);
        assert!(low < 0.45, "同质性 0 时不该高度圈子化，实测 {low}（seed {seed}）");
        assert!(high > 0.80, "同质性 1 时应高度圈子化，实测 {high}（seed {seed}）");
        assert!(
            high - low > 0.30,
            "同质性 0→1 的内占比提升只有 {:.3}（{low} → {high}，seed {seed}）",
            high - low
        );
    }
}

#[test]
fn 内占比随同质性单调上升() {
    let mut prev = 0.0f32;
    for step in 0..=4 {
        let h = step as f64 / 4.0;
        let share = 内占比(h, 11);
        assert!(
            share >= prev - 0.03,
            "同质性扫描不单调：h={h} 时为 {share}，上一步 {prev}"
        );
        prev = share;
    }
    assert!(prev > 0.80, "同质性 1 的内占比应该很高，实测 {prev}");
}

#[test]
fn 桥接节点比例提高跨社团关注() {
    let cfg = |bridge: f64| SimConfig {
        n_initial: 200,
        steps: 120,
        homophily: 0.6,
        bridge_ratio: bridge,
        seed: 21,
        ..SimConfig::default()
    };
    let mut 无桥 = World::new(cfg(0.0));
    无桥.run();
    let mut 多桥 = World::new(cfg(0.6));
    多桥.run();
    let a = 无桥.report().community.follow_inter;
    let b = 多桥.report().community.follow_inter;
    assert!(
        b > a,
        "桥接比例 0.6 的跨社团边（{b}）应明显多于 0.0（{a}）"
    );
    assert!((b as f32) > a as f32 * 1.15, "提升幅度太小：{a} → {b}");
}

#[test]
fn 单社团时全部是社内边() {
    let cfg = SimConfig {
        n_initial: 100,
        steps: 60,
        community_count: 1,
        seed: 31,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    let s = w.report();
    assert_eq!(s.community.count, 1);
    assert_eq!(s.community.follow_inter, 0);
    assert_eq!(s.community.rating_inter, 0);
    assert!((s.community.follow_intra_share - 1.0).abs() < 1e-6);
    assert_eq!(s.community.cliquishness, 0.0); // 没有社间评分，比值无定义
}

#[test]
fn 社团数量影响跨社团比例() {
    let cfg = |k: usize| SimConfig {
        n_initial: 160,
        steps: 100,
        community_count: k,
        homophily: 0.0,
        seed: 41,
        ..SimConfig::default()
    };
    let mut 两社团 = World::new(cfg(2));
    两社团.run();
    let mut 八社团 = World::new(cfg(8));
    八社团.run();
    // 社团越多，随机跨社团越容易（1-1/K 更大）
    let a = 两社团.report().community.follow_intra_share;
    let b = 八社团.report().community.follow_intra_share;
    assert!(b < a, "社团越多内占比应越低：2 个 {a}，8 个 {b}");
}
