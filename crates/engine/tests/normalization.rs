//! 验证四阶段归一化：本地打分、群体项、3:7 混合、最终输出，每一层总和都是 1。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

fn key(i: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i;
    k[1] = 0x5A;
    k
}

fn eng() -> Engine {
    Engine::new(EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams { buckets: 512, hashers: 5, seed: 7 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
    })
}

/// ① 本地打分归一化：应用层给 0~n 的原始数值，份额 = 数值 ÷ 总和
#[test]
fn stage1_local_scores_are_normalized() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    e.rate(a, 3.0, 0);
    e.rate(b, 1.0, 0);

    let sa = e.share_of(a).unwrap();
    let sb = e.share_of(b).unwrap();
    assert!((sa - 0.75).abs() < 1e-6, "份额应为 0.75，实际 {sa}");
    assert!((sb - 0.25).abs() < 1e-6, "份额应为 0.25，实际 {sb}");
    assert!((sa + sb - 1.0).abs() < 1e-6, "份额之和必须为 1");
}

/// ①b 原始数值可以大于 1（应用层给的是 0~n，不是 0~1）
#[test]
fn stage1_accepts_values_above_one() {
    let mut e = eng();
    let a = e.intern(key(1));
    e.rate(a, 37.0, 0);
    assert!((e.share_of(a).unwrap() - 1.0).abs() < 1e-6, "唯一账户份额应为 1");
}

/// ①c 撤销某条评分后重新归一化
#[test]
fn stage1_revoking_renormalizes() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let c = e.intern(key(3));
    e.rate(a, 1.0, 0);
    e.rate(b, 1.0, 0);
    e.rate(c, 2.0, 0);
    assert!((e.share_of(c).unwrap() - 0.5).abs() < 1e-6);
    e.rate(a, 0.0, 0); // 撤销
    assert!((e.share_of(c).unwrap() - 2.0 / 3.0).abs() < 1e-6, "撤销后应重新归一化");
    let sum: f32 = e.shares().iter().map(|(_, s)| *s).sum();
    assert!((sum - 1.0).abs() < 1e-6, "剩余份额之和仍为 1");
}

/// ①d record 是累加语义
#[test]
fn stage1_record_accumulates() {
    let mut e = eng();
    let a = e.intern(key(1));
    e.record(a, 0, 3);
    e.record(a, 0, 2);
    assert!((e.share_of(a).unwrap() - 1.0).abs() < 1e-6, "只有一个对象时份额为 1");
    let b = e.intern(key(2));
    e.record(b, 0, 5);
    assert!((e.share_of(a).unwrap() - 0.5).abs() < 1e-6, "3+2 对 5，应各占一半");
}

/// ② 群体项归一化：每个来源的观点文件先归一化，再按权重平均
#[test]
fn stage2_group_terms_are_normalized() {
    let mut e = eng();
    let f1 = e.intern(key(1));
    let f2 = e.intern(key(2));
    let t = e.intern(key(9));
    let _x = e.intern(key(50)); // 登记，否则接收方估计总质量时看不到它
    e.rate(f1, 1.0, 0);
    e.rate(f2, 1.0, 0);

    // f1 的观点：t=2.0、另一个=2.0 → 归一化后 t 占 0.5
    let v1 = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(9), 0.5), (key(50), 0.5)],
    );
    // f2 的观点：t=1.0、另一个=9.0 → 归一化后 t 占 0.1
    let v2 = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(9), 0.1), (key(50), 0.9)],
    );
    e.on_view(f1, v1, 0, 0);
    e.on_view(f2, v2, 0, 0);

    // 群体项 = 加权平均 = (0.5 + 0.1) / 2 = 0.3（两者权重相同）
    // 个人项为 0，混合系数 0.7 → 0.21
    let q = e.query(t, 0);
    assert!((q - 0.21).abs() < 1e-4, "群体项应为 0.3，混合后 0.21，实际 {q}");
}

/// ③ 3:7 混合后仍要归一化
#[test]
fn stage3_mixed_result_is_renormalized() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    e.rate(a, 3.0, 0);
    e.rate(b, 1.0, 0);

    let dist = e.trust_distribution(0);
    let sum: f32 = dist.iter().map(|(_, v)| *v).sum();
    assert!(!dist.is_empty(), "应有非零分布");
    assert!((sum - 1.0).abs() < 1e-4, "最终分布总和必须为 1，实际 {sum}");
}

/// ④ 最终分布总和恒为 1（含群体项参与时）
#[test]
fn stage4_distribution_sums_to_one() {
    let mut e = eng();
    let f = e.intern(key(1));
    let t = e.intern(key(9));
    e.rate(f, 1.0, 0);
    let v = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(9), 0.6), (key(20), 0.4)],
    );
    e.on_view(f, v, 0, 0);

    let dist = e.trust_distribution(0);
    let sum: f32 = dist.iter().map(|(_, v)| *v).sum();
    assert!((sum - 1.0).abs() < 1e-4, "总和必须为 1，实际 {sum}");
    // 每个值都在 [0,1]
    for (_, v) in &dist {
        assert!(*v >= 0.0 && *v <= 1.0, "份额越界: {v}");
    }
}

/// ④b 空引擎返回空分布，不 panic
#[test]
fn stage4_empty_engine_is_safe() {
    let e = eng();
    assert!(e.trust_distribution(0).is_empty());
    assert!(e.shares().is_empty());
}

/// ④c 单一账户时份额为 1
#[test]
fn stage4_single_account_gets_everything() {
    let mut e = eng();
    let a = e.intern(key(1));
    e.rate(a, 5.0, 0);
    let dist = e.trust_distribution(0);
    assert_eq!(dist.len(), 1);
    assert!((dist[0].1 - 1.0).abs() < 1e-6);
}

/// ④d 份额与声望的换算一致：份额 × 100 = 置信度百分比
#[test]
fn stage4_share_times_hundred_is_percentage() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    e.rate(a, 7.0, 0);
    e.rate(b, 3.0, 0);
    let dist = e.trust_distribution(0);
    let total: f32 = dist.iter().map(|(_, v)| *v).sum();
    assert!((total - 1.0).abs() < 1e-4);
    for (_, v) in &dist {
        let pct = v * 100.0;
        assert!((0.0..=100.0).contains(&pct), "百分比应在 0~100，实际 {pct}");
    }
}
