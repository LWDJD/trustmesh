//! 两级衰减、覆盖率与不可达来源（设计规格 4.9）的运行时行为。
//!
//! 这一组测试钉住五件事：
//!
//! 1. **覆盖率** `coverage = |live| ÷ |正分清单|`：live 判定 = `now − last_check < T_remove`；
//!    `T_remove = 0`（默认）表示不判定，覆盖率恒为 1；正分清单为空时按 0 处理。
//! 2. **群体级系数** `s = coverage^α · 0.5^(avg_age_live / H_group)`：
//!    新鲜时 = 1、一个群组半衰期时年龄项 = 0.5、极老时趋近 0、α = 1 时等于覆盖率、
//!    两个旋钮同时关闭（`H_group = 0` 且 `θ_remove = 0`）时恒为 1。
//! 3. **有替代才剔除**：只有 `coverage >= θ_remove` 时才把非 live 来源踢出群体项；
//!    覆盖率不足时保留它们（它们已被来源级衰减压过，那时 `s` 也很小）。
//! 4. **最低样本数**：live 来源数 < N_min 时群体项置空，声望退化为 β × 个人项。
//! 5. **回归**：默认配置（五个新参数全关）下结果与改造前的公式一致。

use tm_agg::{decay, AggMode};
use tm_engine::{Engine, EngineConfig};
use tm_store::{AccountIdx, AccountKey, Time};
use tm_view::{CompressedView, ViewParams};

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

/// 构造互不相同的公钥。
fn key(n: u8) -> AccountKey {
    let mut k = [0u8; 32];
    k[0] = n;
    k[1] = 0xC3;
    k
}

/// 测试基准配置：β = 0.3、来源级半衰期 72、其余全默认（五个新参数都是关闭态）。
fn base() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams::default(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        group_half_life: 0,
        coverage_exponent: 1.0,
        unreachable_after: 0,
        remove_when_coverage: 0.0,
        min_group_sources: 0,
    }
}

/// 固定种子，保证结果可复现。
fn engine(cfg: EngineConfig) -> Engine {
    Engine::with_seed(cfg, 42)
}

/// 一份「只把 target 评为满分」的观点：归一化后 target 的值精确等于 1.0。
fn full(target: AccountKey) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, 1.0)])
}

/// 一份「对 target 完全没有表态」的观点（查询结果 0）。
fn none_of(target: AccountKey) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, 0.0)])
}

/// 一份「按 t : (1−t) 分给 target 与 other」的观点。
///
/// 两个 key 都 intern 过时，接收方估出的总质量 = 1.0，归一化后 target 的值就是 t。
fn two_way(target: AccountKey, t: f32, other: AccountKey) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, t), (other, 1.0 - t)])
}

/// 造一个来源：`rating > 0` 就打分；观点把 `target` 评为 `opinion`（>0.5 记作 1，
/// 否则记作 0）；并在 `last_check` 时刻「取到」这份观点。
///
/// 注意：`is_live` 只看**观点**的 `last_check`，与评分写入的时间无关。
fn source(
    e: &mut Engine,
    n: u8,
    rating: f32,
    target: AccountKey,
    opinion: f32,
    last_check: Time,
) -> AccountIdx {
    let idx = e.intern(key(n));
    if rating > 0.0 {
        e.rate(idx, rating, last_check);
    }
    let view = if opinion > 0.5 {
        full(target)
    } else {
        none_of(target)
    };
    e.on_view(idx, view, 0, last_check);
    idx
}

// ---------------------------------------------------------------------------
// 1. 默认值：五个新参数全关
// ---------------------------------------------------------------------------

/// 默认配置必须把所有新机制关掉 —— 这是「默认行为与改造前逐位一致」的前提。
#[test]
fn default_config_keeps_every_new_knob_off() {
    let cfg = EngineConfig::default();
    assert_eq!(cfg.group_half_life, 0, "H_group 默认关闭");
    assert_eq!(cfg.coverage_exponent, 1.0, "α 默认 1.0");
    assert_eq!(cfg.unreachable_after, 0, "T_remove 默认关闭");
    assert_eq!(cfg.remove_when_coverage, 0.0, "θ_remove 默认永不剔除");
    assert_eq!(cfg.min_group_sources, 0, "N_min 默认不设限");

    // 默认配置下 s 恒为 1：哪怕来源全都很旧、覆盖率只有一点点。
    let mut e = engine(EngineConfig::default());
    let target = e.intern(key(9));
    let _ = target;
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e.group_scale(0), 1.0);
    assert_eq!(e.group_scale(1_000_000), 1.0, "关闭时 s 与时间无关");
    assert_eq!(e.coverage(1_000_000), 1.0, "T_remove = 0 时覆盖率恒为 1");
}

// ---------------------------------------------------------------------------
// 2. group_scale
// ---------------------------------------------------------------------------

/// 全部来源刚取到：覆盖率 100%、平均年龄 0 ⇒ s 精确等于 1。
#[test]
fn group_scale_is_one_when_every_source_is_fresh() {
    let mut e = engine(EngineConfig {
        group_half_life: 120,
        unreachable_after: 168,
        min_group_sources: 3,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e.live_count(0), 3);
    assert_eq!(e.coverage(0), 1.0);
    assert_eq!(e.avg_source_age(0), Some(0));
    assert_eq!(e.group_scale(0), 1.0, "新鲜且全覆盖时 s 必须精确为 1");
}

/// 平均年龄恰好等于 H_group 时年龄项 = 0.5；两个半衰期后 = 0.25。
#[test]
fn group_scale_halves_at_one_group_half_life() {
    // 不判定 live（T_remove = 0），这样来源不会因为太旧被踢出，只看年龄项。
    let mut e = engine(EngineConfig {
        group_half_life: 120,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e.coverage(120), 1.0);
    assert_eq!(e.avg_source_age(120), Some(120));
    assert_eq!(e.group_scale(120), 0.5, "平均年龄 = H_group ⇒ 0.5");
    assert_eq!(e.avg_source_age(240), Some(240));
    assert_eq!(e.group_scale(240), 0.25, "两倍 H_group ⇒ 0.25");
}

/// 平均年龄极大时 s 趋近 0，但永不精确归零（与 `tm_agg::decay` 同口径）。
#[test]
fn group_scale_tends_to_zero_for_very_old_sources() {
    let mut e = engine(EngineConfig {
        group_half_life: 120,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    let s = e.group_scale(u64::MAX);
    assert!(s > 0.0, "指数衰减永不精确归零，实际 {s}");
    assert!(s < 1e-9, "极老时 s 应趋近 0，实际 {s}");
    assert!(s.is_finite());
}

/// `H_group = 0` 且 `θ_remove = 0` 时 s 恒为 1，连空引擎也一样。
#[test]
fn group_scale_is_always_one_when_the_knobs_are_off() {
    // 空引擎
    let e = engine(base());
    for now in [0u64, 1, 1_000, u64::MAX] {
        assert_eq!(e.group_scale(now), 1.0, "now={now}");
    }

    // 只有 1/3 来源可达、且都很旧，s 仍然是 1（关闭就是关闭）
    let mut e2 = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e2, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e2.coverage(1_000), 0.0, "全部过期，覆盖率 0");
    assert_eq!(e2.group_scale(1_000), 1.0, "关闭时 s 不看覆盖率");
}

/// 没有 live 来源（或正分清单为空）时 s = 0、平均年龄为 None。
#[test]
fn group_scale_is_zero_when_nothing_is_live() {
    let e = engine(EngineConfig {
        group_half_life: 120,
        unreachable_after: 168,
        ..base()
    });
    assert_eq!(e.group_scale(0), 0.0, "空引擎没有群体意见");
    assert_eq!(e.avg_source_age(0), None);
    assert_eq!(e.live_count(0), 0);
    assert_eq!(e.coverage(0), 0.0, "分母为 0 时覆盖率按 0 处理");

    let mut e2 = engine(EngineConfig {
        group_half_life: 120,
        unreachable_after: 168,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e2, i, 1.0, key(9), 1.0, 0);
    }
    // 年龄恰好等于阈值 ⇒ 不算 live（严格小于才算）
    assert_eq!(e2.live_count(168), 0);
    assert_eq!(e2.coverage(168), 0.0);
    assert_eq!(e2.avg_source_age(168), None);
    assert_eq!(e2.group_scale(168), 0.0);
    // 差一小时就还在 live 里
    assert_eq!(e2.live_count(167), 3);
    assert_eq!(e2.avg_source_age(167), Some(167));
    assert!((e2.group_scale(167) - decay(167, 120)).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 3. coverage
// ---------------------------------------------------------------------------

/// 覆盖率 = live 来源数 ÷ 正分清单条目数。
#[test]
fn coverage_is_live_over_positive_list() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    // 4 个来源，now = 1000 时的年龄分别是 0 / 100 / 140 / 200；最后一个已过期
    source(&mut e, 1, 1.0, key(9), 1.0, 1_000);
    source(&mut e, 2, 1.0, key(9), 1.0, 900);
    source(&mut e, 3, 1.0, key(9), 1.0, 860);
    let fourth = source(&mut e, 4, 1.0, key(9), 1.0, 800);

    assert_eq!(e.positive_count(), 4);
    assert_eq!(e.live_count(1_000), 3);
    assert_eq!(e.coverage(1_000), 0.75, "3 / 4");

    // 时间再走 30 小时：第 3 个来源的年龄变成 170，也过期了
    assert_eq!(e.live_count(1_030), 2);
    assert_eq!(e.coverage(1_030), 0.5);

    // 撤销一个评分 → 正分清单变小，覆盖率随之上升
    e.rate(fourth, 0.0, 1_030);
    assert_eq!(e.positive_count(), 3);
    assert_eq!(e.live_count(1_030), 2);
    assert!((e.coverage(1_030) - 2.0 / 3.0).abs() < 1e-6);
}

/// live 判定关闭时，覆盖率恒为 1，与时间无关。
#[test]
fn coverage_is_one_when_live_judgement_is_disabled() {
    let mut e = engine(base());
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    for now in [0u64, 168, 100_000, u64::MAX] {
        assert_eq!(e.coverage(now), 1.0, "now={now}");
        assert_eq!(e.live_count(now), 3, "now={now}");
    }
}

// ---------------------------------------------------------------------------
// 4. live_count
// ---------------------------------------------------------------------------

/// 阈值边界：年龄 < T_remove 才算 live，等于阈值即退出。
#[test]
fn live_count_respects_the_threshold_boundary() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    let a = source(&mut e, 1, 1.0, key(9), 1.0, 1_000); // 年龄 0
    let b = source(&mut e, 2, 1.0, key(9), 1.0, 833); // 年龄 167
    let c = source(&mut e, 3, 1.0, key(9), 1.0, 832); // 年龄 168

    assert_eq!(e.view_age(a, 1_000), Some(0));
    assert_eq!(e.view_age(b, 1_000), Some(167));
    assert_eq!(e.view_age(c, 1_000), Some(168));
    assert_eq!(e.live_count(1_000), 2, "167 算 live，168 不算");

    // 阈值关掉之后三个都算
    let mut f = engine(base());
    for i in 1..=3u8 {
        source(&mut f, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(f.live_count(u64::MAX), 3, "T_remove = 0 时全部计入");
}

/// 只有「打过正分」且「缓存里有观点」的来源才算 live。
#[test]
fn live_count_counts_sources_with_rating_and_view_only() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    // ① 有评分 + 有观点 → 算
    let first = source(&mut e, 1, 1.0, key(9), 1.0, 0);
    // ② 有评分但从来没有观点 → 不算（它没有观点可参与群体项）
    let rated_only = e.intern(key(2));
    e.rate(rated_only, 1.0, 0);
    // ③ 有观点但没有评分 → 不算
    let view_only = e.intern(key(3));
    e.on_view(view_only, full(key(9)), 0, 0);

    assert_eq!(e.positive_count(), 2, "正分清单只有 ①②");
    assert_eq!(e.stats().2, 2, "观点缓存有 ①③");
    assert_eq!(e.live_count(0), 1);
    assert_eq!(e.coverage(0), 0.5);

    // 撤销 ① 的评分：它立刻退出 live，但观点还在缓存里
    e.rate(first, 0.0, 0);
    assert_eq!(e.live_count(0), 0);
    assert_eq!(e.view_age(first, 0), Some(0), "观点缓存不受影响");
}

// ---------------------------------------------------------------------------
// 5. avg_source_age
// ---------------------------------------------------------------------------

/// live 为空时返回 None（「没有观点」和「观点年龄为 0」是两件事）。
#[test]
fn avg_source_age_is_none_when_nothing_is_live() {
    let e = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    assert_eq!(e.avg_source_age(0), None, "完全没有来源");

    let mut e2 = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    for i in 1..=2u8 {
        source(&mut e2, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e2.avg_source_age(100), Some(100), "100 < 168，还在 live 里");
    assert_eq!(e2.avg_source_age(167), Some(167));
    assert_eq!(e2.avg_source_age(168), None, "全部退出 live");
    assert_eq!(e2.avg_source_age(999), None);
}

/// 单来源时平均年龄就是它自己的年龄。
#[test]
fn avg_source_age_of_a_single_source_equals_its_age() {
    let mut e = engine(base());
    source(&mut e, 1, 1.0, key(9), 1.0, 500);
    assert_eq!(e.avg_source_age(500), Some(0));
    assert_eq!(e.avg_source_age(572), Some(72));
    assert_eq!(e.avg_source_age(500 + 720), Some(720));
    // 极端值：f64 只有 53 位尾数，u64::MAX 附近会被舍入并饱和到 u64::MAX。
    // 这里只要求「不 panic、不回绕成 0」，不要求逐位等于 u64::MAX − 500。
    let huge = e.avg_source_age(u64::MAX).expect("来源仍然 live");
    assert!(
        huge >= u64::MAX - 1_000,
        "极端年龄应饱和在 u64::MAX 附近，实际 {huge}"
    );
}

/// 多来源时是加权平均，权重 = **未衰减的份额**（不带 fs）。
///
/// 注意这里与群体项刻意不同：群体项用 `份额 × fs`，平均年龄用裸份额。
/// 若这里也乘 fs，老来源的年龄贡献会被它自己的衰减压低 → avg_age 偏小 →
/// s 偏大 → 群体级衰减几乎失效。
#[test]
fn avg_source_age_is_weighted_by_undecayed_share() {
    let mut e = engine(base());
    // A：评分 3、年龄 72  → 份额 0.75
    let a = source(&mut e, 1, 3.0, key(9), 1.0, 0);
    // B：评分 1、刚取到  → 份额 0.25
    let b = source(&mut e, 2, 1.0, key(9), 1.0, 72);
    assert_eq!(e.share_of(a), Some(0.75));
    assert_eq!(e.share_of(b), Some(0.25));

    let now = 72u64;
    let expected = ((0.75f64 * 72.0 + 0.25f64 * 0.0) / (0.75f64 + 0.25f64)).round() as u64;
    assert_eq!(expected, 54, "(0.75×72) / 1.0 = 54");
    assert_eq!(e.avg_source_age(now), Some(expected));

    // 反例：若按「份额 × fs」加权，会得到 (0.375×72)/0.625 = 43 —— 明显偏小。
    // 这正是不能用衰减权重算平均年龄的原因。
    let fs72 = f64::from(decay(72, 72));
    let decayed = ((0.75f64 * fs72 * 72.0) / (0.75f64 * fs72 + 0.25f64)).round() as u64;
    assert_eq!(decayed, 43);
    assert!(
        expected > decayed,
        "未衰减份额口径必须给出更大的平均年龄，否则群体级衰减会被自己吃掉"
    );
}

/// 非 live 来源不参与平均年龄：一屋子新鲜 + 一个过期，平均年龄只看新鲜那些。
#[test]
fn avg_source_age_ignores_non_live_sources() {
    // T_remove = 168：200 小时前的来源已经退出 live
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        ..base()
    });
    source(&mut e, 1, 1.0, key(9), 1.0, 1_000); // 年龄 0
    source(&mut e, 2, 9.0, key(9), 0.0, 800); // 年龄 200（过期，但权重很大）
    assert_eq!(e.live_count(1_000), 1);
    assert_eq!(
        e.avg_source_age(1_000),
        Some(0),
        "过期来源不应把平均年龄拉高"
    );
    assert_eq!(e.group_scale(1_000), 1.0);

    // 反证：把阈值关掉，它就会把平均年龄拉高（份额 0.1 对 0.9）
    let mut f = engine(EngineConfig {
        group_half_life: 120,
        ..base()
    });
    let fresh = source(&mut f, 1, 1.0, key(9), 1.0, 1_000);
    let stale = source(&mut f, 2, 9.0, key(9), 0.0, 800);
    let w1 = f64::from(f.share_of(fresh).unwrap());
    let w2 = f64::from(f.share_of(stale).unwrap());
    let expected = ((w1 * 0.0 + w2 * 200.0) / (w1 + w2)).round() as u64;
    assert_eq!(expected, 180, "0.9 × 200 = 180");
    assert_eq!(f.avg_source_age(1_000), Some(expected));
    assert!(f.group_scale(1_000) < 1.0);
}

// ---------------------------------------------------------------------------
// 6. 覆盖率项：s = coverage^α
// ---------------------------------------------------------------------------

/// α = 1 时 s 就等于覆盖率（此时年龄项必须为 1）。
#[test]
fn coverage_term_equals_coverage_when_alpha_is_one() {
    // H_group = 0 关掉年龄项；θ_remove > 0 让 s 保持「开」的状态。
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.1,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 1_000); // 3 个 live
    }
    source(&mut e, 4, 1.0, key(9), 1.0, 800); // 1 个过期
    assert_eq!(e.coverage(1_000), 0.75);
    assert_eq!(
        e.group_scale(1_000),
        0.75,
        "α=1 且年龄项关闭 ⇒ s = coverage"
    );

    // 只剩 1/4 可达
    let mut f = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.1,
        ..base()
    });
    for i in 1..=4u8 {
        let last = if i == 1 { 1_000 } else { 0 };
        source(&mut f, i, 1.0, key(9), 1.0, last);
    }
    assert_eq!(f.coverage(1_000), 0.25);
    assert_eq!(f.group_scale(1_000), 0.25);
}

/// α 改变曲线形状：同样的覆盖率，α 越大 s 越小；α = 0 时完全不看覆盖率。
#[test]
fn coverage_exponent_shapes_the_s_curve() {
    fn build(alpha: f32) -> Engine {
        let mut e = engine(EngineConfig {
            unreachable_after: 168,
            coverage_exponent: alpha,
            remove_when_coverage: 0.1,
            ..base()
        });
        for i in 1..=4u8 {
            let last = if i == 1 { 1_000 } else { 0 };
            source(&mut e, i, 1.0, key(9), 1.0, last);
        }
        e
    }

    let cov = build(1.0).coverage(1_000);
    assert_eq!(cov, 0.25);
    assert!((build(1.0).group_scale(1_000) - cov).abs() < 1e-6);
    assert!(
        (build(2.0).group_scale(1_000) - cov * cov).abs() < 1e-6,
        "α=2 ⇒ coverage²"
    );
    assert!(
        (build(0.5).group_scale(1_000) - cov.sqrt()).abs() < 1e-6,
        "α=0.5 ⇒ √coverage"
    );
    assert_eq!(build(0.0).group_scale(1_000), 1.0, "α=0 ⇒ 覆盖率项恒为 1");
    // 单调性：α 越大 s 越小
    assert!(build(2.0).group_scale(1_000) < build(1.0).group_scale(1_000));
    assert!(build(1.0).group_scale(1_000) < build(0.5).group_scale(1_000));
    assert!(build(0.5).group_scale(1_000) < build(0.0).group_scale(1_000));
}

/// 三档覆盖率下 s 单调上升（1.0 > 0.5 > 0.25），且等于对应覆盖率。
#[test]
fn group_scale_rises_with_coverage() {
    fn build(live: u8) -> Engine {
        let mut e = engine(EngineConfig {
            group_half_life: 120,
            unreachable_after: 168,
            remove_when_coverage: 0.1,
            ..base()
        });
        // 4 个来源：`live` 个刚取到，其余 200 小时前（过期）
        for i in 1..=4u8 {
            let last = if i <= live { 1_000 } else { 800 };
            source(&mut e, i, 1.0, key(9), 1.0, last);
        }
        e
    }

    let full_cov = build(4).group_scale(1_000);
    let half_cov = build(2).group_scale(1_000);
    let quarter_cov = build(1).group_scale(1_000);
    // live 来源年龄都是 0 ⇒ 年龄项 = 1，s 完全由覆盖率决定
    assert_eq!(build(4).coverage(1_000), 1.0);
    assert_eq!(build(2).coverage(1_000), 0.5);
    assert_eq!(build(1).coverage(1_000), 0.25);
    assert!((full_cov - 1.0).abs() < 1e-6, "{full_cov}");
    assert!((half_cov - 0.5).abs() < 1e-6, "{half_cov}");
    assert!((quarter_cov - 0.25).abs() < 1e-6, "{quarter_cov}");
    assert!(
        full_cov > half_cov && half_cov > quarter_cov,
        "s 必须随覆盖率单调上升"
    );
}

// ---------------------------------------------------------------------------
// 7. 有替代才剔除
// ---------------------------------------------------------------------------

/// 覆盖率低于 θ_remove 时不剔除：过期来源仍然参与群体项。
#[test]
fn removal_stays_off_below_the_coverage_threshold() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.6,
        ..base()
    });
    // 2 个新鲜来源说 1.0（评分 1），2 个过期来源说 0.0（评分 9）
    for i in 1..=2u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 1_000);
    }
    for i in 3..=4u8 {
        source(&mut e, i, 9.0, key(9), 0.0, 800);
    }
    let target = e.intern(key(9));
    assert_eq!(e.live_count(1_000), 2);
    assert_eq!(e.coverage(1_000), 0.5, "2 / 4，低于 θ_remove = 0.6");

    // 过期来源仍然在群体项里：它们的 0.0 稀释了结果
    let fs = f64::from(decay(200, 72));
    let w_live = 2.0 * 1.0;
    let w_stale = 2.0 * 9.0 * fs;
    let group = w_live / (w_live + w_stale);
    let s = 0.5f64; // H_group = 0 关掉年龄项，θ_remove > 0 让 s = coverage^1
    let expected = 0.7 * s * group;
    let q = f64::from(e.query(target, 1_000));
    assert!(
        (q - expected).abs() < 1e-5,
        "不剔除时群体项应被过期来源稀释，期望 {expected}，实际 {q}"
    );

    // 对照：同样的数据，θ_remove = 0.5 时剔除生效，群体项变成纯 1.0
    let mut f = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.5,
        ..base()
    });
    for i in 1..=2u8 {
        source(&mut f, i, 1.0, key(9), 1.0, 1_000);
    }
    for i in 3..=4u8 {
        source(&mut f, i, 9.0, key(9), 0.0, 800);
    }
    let target = f.intern(key(9));
    let expected_removed = 0.7 * 0.5 * 1.0;
    let q = f64::from(f.query(target, 1_000));
    assert!(
        (q - expected_removed).abs() < 1e-5,
        "剔除后群体项 = 1.0（只剩新鲜来源），期望 {expected_removed}，实际 {q}"
    );
}

/// 覆盖率恰好等于 θ_remove 时剔除生效（>= 而不是 >）。
#[test]
fn removal_kicks_in_at_the_coverage_threshold() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.5,
        ..base()
    });
    for i in 1..=2u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 1_000);
    }
    for i in 3..=4u8 {
        source(&mut e, i, 9.0, key(9), 0.0, 800);
    }
    let target = e.intern(key(9));
    assert_eq!(e.coverage(1_000), 0.5, "恰好等于阈值");
    assert_eq!(e.group_scale(1_000), 0.5);
    // 剔除生效 ⇒ 群体项只由两个新鲜来源构成 = 1.0
    assert!((e.query(target, 1_000) - 0.35).abs() < 1e-5);

    // 阈值再高一点（0.51）就不再剔除，结果必须不同
    let mut f = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.51,
        ..base()
    });
    for i in 1..=2u8 {
        source(&mut f, i, 1.0, key(9), 1.0, 1_000);
    }
    for i in 3..=4u8 {
        source(&mut f, i, 9.0, key(9), 0.0, 800);
    }
    let target = f.intern(key(9));
    let q = f.query(target, 1_000);
    assert!(q < 0.35, "不剔除时应被稀释到 0.35 以下，实际 {q}");
}

// ---------------------------------------------------------------------------
// 8. 最低样本数 N_min
// ---------------------------------------------------------------------------

/// live 数 < N_min 时群体项置空，声望退化为 β × 个人项。
#[test]
fn group_term_is_empty_below_min_sources() {
    let mut e = engine(EngineConfig {
        min_group_sources: 3,
        ..base()
    });
    let target = e.intern(key(9));
    let rival = e.intern(key(20));
    e.rate(target, 1.0, 0);
    e.rate(rival, 1.0, 0);
    for i in 1..=2u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 0);
    }
    assert_eq!(e.live_count(0), 2);
    let share = f64::from(e.share_of(target).unwrap());
    let expected = 0.3 * share;
    assert!(
        (f64::from(e.query(target, 0)) - expected).abs() < 1e-6,
        "N_min = 3 而 live 只有 2 个 ⇒ 声望只能是 β×个人项 = {expected}"
    );

    // 第三个来源出现 ⇒ 群体项恢复
    source(&mut e, 3, 1.0, key(9), 1.0, 0);
    assert_eq!(e.live_count(0), 3);
    let share = f64::from(e.share_of(target).unwrap());
    let expected = 0.3 * share + 0.7;
    assert!(
        (f64::from(e.query(target, 0)) - expected).abs() < 1e-6,
        "N_min 满足后群体项 = 1.0，期望 {expected}"
    );
}

/// N_min = 0 表示不设限，一个来源也算群体意见。
#[test]
fn zero_min_group_sources_disables_the_gate() {
    let mut e = engine(base());
    let target = e.intern(key(9));
    let rival = e.intern(key(20));
    e.rate(target, 1.0, 0);
    e.rate(rival, 1.0, 0);
    source(&mut e, 1, 1.0, key(9), 1.0, 0);
    assert_eq!(e.live_count(0), 1);
    let share = f64::from(e.share_of(target).unwrap());
    let expected = 0.3 * share + 0.7;
    assert!(
        (f64::from(e.query(target, 0)) - expected).abs() < 1e-6,
        "N_min = 0 时单个来源也能构成群体项，期望 {expected}"
    );
}

// ---------------------------------------------------------------------------
// 9. 与单级衰减的差异 + β 配比
// ---------------------------------------------------------------------------

/// 造一个「3 个新鲜来源说 1.0 + 1 个 120 小时前的来源说 0.0」的局面。
fn four_sources(group_half_life: Time, decay_enabled: bool) -> (Engine, AccountIdx) {
    let mut e = engine(EngineConfig {
        group_half_life,
        decay_enabled,
        ..base()
    });
    let target = e.intern(key(9));
    for i in 1..=4u8 {
        let last = if i <= 3 { 120 } else { 0 };
        let opinion = if i <= 3 { 1.0 } else { 0.0 };
        source(&mut e, i, 1.0, key(9), opinion, last);
    }
    (e, target)
}

/// 两级同时开启 ≠ 只开来源级 ≠ 只开群体级。
#[test]
fn two_level_decay_differs_from_each_single_level() {
    let (src_only, t1) = four_sources(0, true);
    let (both, t2) = four_sources(120, true);
    let (group_only, t3) = four_sources(120, false);
    let now = 120u64;

    // 手算三份期望值
    let fs4 = f64::from(decay(120, 72)); // 第 4 个来源的年龄是 120
    let w4 = 0.25 * fs4;
    let w = 0.25;
    let group_src_only = 3.0 * w / (3.0 * w + w4);
    let expected_src_only = 0.7 * group_src_only;

    // 平均年龄用**未衰减份额**（四个来源等权）：(0+0+0+120)/4 = 30
    let avg_both = (3.0f64 * 0.25 * 0.0 + 0.25f64 * 120.0) / (3.0f64 * 0.25 + 0.25f64);
    let s_both = f64::from(decay(avg_both.round() as u64, 120));
    let expected_both = 0.7 * s_both * group_src_only;

    // 四个来源等权，年龄 (0+0+0+120)/4 = 30
    let avg_group_only = 30.0;
    let s_group_only = f64::from(decay(avg_group_only as u64, 120));
    let expected_group_only = 0.7 * s_group_only * 0.75;

    let q1 = f64::from(src_only.query(t1, now));
    let q2 = f64::from(both.query(t2, now));
    let q3 = f64::from(group_only.query(t3, now));
    assert!(
        (q1 - expected_src_only).abs() < 1e-5,
        "只开来源级：{q1} vs {expected_src_only}"
    );
    assert!(
        (q2 - expected_both).abs() < 1e-5,
        "两级全开：{q2} vs {expected_both}"
    );
    assert!(
        (q3 - expected_group_only).abs() < 1e-5,
        "只开群体级：{q3} vs {expected_group_only}"
    );
    assert!((q1 - q2).abs() > 1e-3, "两级全开必须与只开来源级不同");
    assert!((q2 - q3).abs() > 1e-3, "两级全开必须与只开群体级不同");
    assert!((q1 - q3).abs() > 1e-3);
}

/// s 改变 β:(1−β) 的实际配比：群体权重只能下降。
#[test]
fn s_shifts_the_beta_ratio_downwards() {
    fn build(group_half_life: Time) -> (Engine, AccountIdx) {
        let mut e = engine(EngineConfig {
            group_half_life,
            ..base()
        });
        let target = e.intern(key(9));
        let rival = e.intern(key(20));
        e.rate(target, 1.0, 0);
        e.rate(rival, 1.0, 0);
        for i in 1..=3u8 {
            source(&mut e, i, 1.0, key(9), 1.0, 0);
        }
        (e, target)
    }

    let (off, t_off) = build(0); // s = 1
    let (on, t_on) = build(120); // now = 120 时 s = 0.5
    let now = 120u64;
    assert_eq!(on.coverage(now), 1.0);
    assert_eq!(on.group_scale(now), 0.5);

    let share = f64::from(off.share_of(t_off).unwrap()); // 1/5 = 0.2
    assert!((share - 0.2).abs() < 1e-6);
    let expected_off = 0.3 * share + 0.7 * 1.0;
    let expected_on = 0.3 * share + 0.7 * 0.5 * 1.0;
    let q_off = f64::from(off.query(t_off, now));
    let q_on = f64::from(on.query(t_on, now));
    assert!(
        (q_off - expected_off).abs() < 1e-5,
        "s=1：{q_off} vs {expected_off}"
    );
    assert!(
        (q_on - expected_on).abs() < 1e-5,
        "s=0.5：{q_on} vs {expected_on}"
    );
    assert!(q_on < q_off, "群体权重下降 ⇒ 声望下降：{q_on} < {q_off}");

    // 等效 β（个人项占比）应当上升
    let beta_eff_off = (0.3 * share) / (0.3 * share + 0.7 * 1.0);
    let beta_eff_on = (0.3 * share) / (0.3 * share + 0.7 * 0.5 * 1.0);
    assert!(
        beta_eff_on > beta_eff_off,
        "s 下降 ⇒ 个人项占比上升：{beta_eff_on} 应大于 {beta_eff_off}"
    );
}

// ---------------------------------------------------------------------------
// 10. 回归：默认关闭时与改造前一致
// ---------------------------------------------------------------------------

/// 造一个多来源、多年龄、有个人项也有群体项的场景，返回 (target, now)。
fn legacy_scenario(e: &mut Engine) -> (AccountIdx, Time) {
    let target = e.intern(key(9));
    let rival = e.intern(key(20));
    let _filler = e.intern(key(50));
    let s1 = e.intern(key(1));
    let s2 = e.intern(key(2));
    let s3 = e.intern(key(3));
    e.rate(target, 1.0, 0);
    e.rate(rival, 3.0, 0);
    e.rate(s1, 2.0, 0);
    e.rate(s2, 1.0, 0);
    e.rate(s3, 0.5, 0);
    e.on_view(s1, full(key(9)), 0, 200); // 刚刷新，说 1.0
    e.on_view(s2, none_of(key(9)), 0, 200); // 刚刷新，说 0.0
    e.on_view(s3, two_way(key(9), 0.5, key(50)), 0, 128); // 72 小时前，说 0.5
    (target, 200)
}

/// 五个新参数全关时，结果必须与 4.9 之前的公式一致（并且默认配置就是这样）。
#[test]
fn disabled_two_level_decay_matches_legacy_formula() {
    let mut e = engine(base());
    let (target, now) = legacy_scenario(&mut e);

    // 手写改造前的公式：group = Σ(份额×fs×观点) / Σ(份额×fs)，声望 = β·个人项 + (1−β)·group
    let s1 = e.intern(key(1));
    let s2 = e.intern(key(2));
    let s3 = e.intern(key(3));
    let share = |i: AccountIdx| f64::from(e.share_of(i).unwrap());
    let w = |i: AccountIdx, last: Time| share(i) * f64::from(decay(now - last, 72));
    let denom = w(s1, 200) + w(s2, 200) + w(s3, 128);
    let group = (w(s1, 200) * 1.0 + w(s2, 200) * 0.0 + w(s3, 128) * 0.5) / denom;
    let expected = 0.3 * share(target) + 0.7 * group;

    assert_eq!(e.group_scale(now), 1.0, "关闭时 s 恒为 1");
    assert_eq!(e.coverage(now), 1.0, "关闭时覆盖率恒为 1");
    assert_eq!(e.live_count(now), 3);
    assert!(
        (f64::from(e.query(target, now)) - expected).abs() < 1e-6,
        "期望 {expected}，实际 {}",
        e.query(target, now)
    );

    // EngineConfig::default() 也必须给出同一个结果（默认 = 全关）
    let mut d = engine(EngineConfig::default());
    let (target_d, now_d) = legacy_scenario(&mut d);
    assert_eq!(
        e.query(target, now),
        d.query(target_d, now_d),
        "默认配置必须与显式全关逐位一致"
    );
}

// ---------------------------------------------------------------------------
// 11. 分布归一化与时间回拨
// ---------------------------------------------------------------------------

/// 所有聚合口径 × 所有开关组合下，最终分布总和仍为 1。
#[test]
fn every_mode_keeps_distribution_normalized() {
    let modes = [
        AggMode::Mean,
        AggMode::Median,
        AggMode::WeightedMedian,
        AggMode::Trimmed(0.1),
    ];
    // (H_group, T_remove, θ_remove, N_min)
    let combos = [
        (0u64, 0u64, 0.0f32, 0usize),
        (120, 0, 0.0, 0),
        (0, 168, 0.5, 0),
        (120, 168, 0.6, 3),
        (120, 0, 0.0, 3),
        (240, 168, 0.25, 1),
    ];

    for mode in modes {
        for &(h, t, theta, n) in &combos {
            let mut e = engine(EngineConfig {
                agg_mode: mode,
                group_half_life: h,
                unreachable_after: t,
                remove_when_coverage: theta,
                min_group_sources: n,
                ..base()
            });
            let target = e.intern(key(9));
            let rival = e.intern(key(20));
            e.rate(target, 1.0, 0);
            e.rate(rival, 2.0, 0);
            for i in 1..=4u8 {
                let last = if i <= 2 { 1_000 } else { 0 };
                let opinion = if i % 2 == 0 { 1.0 } else { 0.0 };
                source(&mut e, i, 1.0, key(9), opinion, last);
            }

            let dist = e.trust_distribution(1_000);
            assert!(
                !dist.is_empty(),
                "{mode:?} / {h}-{t}-{theta}-{n}：分布不应为空"
            );
            let sum: f32 = dist.iter().map(|(_, v)| *v).sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "{mode:?} / {h}-{t}-{theta}-{n}：分布总和应为 1，实际 {sum}"
            );
            for (_, v) in &dist {
                assert!((0.0..=1.0).contains(v), "{mode:?}：份额越界 {v}");
            }
        }
    }
}

/// 时间回拨：年龄夹到 0，不 panic、不出负数、不误剔除。
#[test]
fn time_rollback_is_clamped_and_safe() {
    let mut e = engine(EngineConfig {
        group_half_life: 120,
        unreachable_after: 168,
        remove_when_coverage: 0.5,
        min_group_sources: 3,
        ..base()
    });
    for i in 1..=3u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 1_000 + i as Time);
    }
    let target = e.intern(key(9));

    // now 回退到 0：三条观点的年龄都按 0 算
    for s in 1..=3u8 {
        let idx = e.intern(key(s));
        assert_eq!(e.view_age(idx, 0), Some(0));
    }
    assert_eq!(e.live_count(0), 3, "回拨时全部 live，不会误剔除");
    assert_eq!(e.coverage(0), 1.0);
    assert_eq!(e.avg_source_age(0), Some(0));
    assert_eq!(e.group_scale(0), 1.0);

    let q = e.query(target, 0);
    assert!(q.is_finite() && (0.0..=1.0).contains(&q), "回拨后越界：{q}");

    // 极端回拨 + 极端前进都不炸
    for now in [0u64, 1, u64::MAX] {
        let q = e.query(target, now);
        assert!(
            q.is_finite() && (0.0..=1.0).contains(&q),
            "now={now} 越界：{q}"
        );
        let s = e.group_scale(now);
        assert!(
            s.is_finite() && (0.0..=1.0).contains(&s),
            "now={now} 的 s 越界：{s}"
        );
    }
}
// ---------------------------------------------------------------------------
// 12. 确定性（§8 不变量 1）：浮点累加顺序必须稳定
// ---------------------------------------------------------------------------

/// 一堆「评分各异、年龄各异」的来源：权重和不可能恰好可交换，
/// 一旦遍历顺序随机，结果就会抖。
fn staggered_scenario(e: &mut Engine) -> (AccountIdx, Time) {
    let target = e.intern(key(9));
    let _filler = e.intern(key(50));
    let ratings = [0.3f32, 0.7, 1.3, 0.9, 2.1, 0.5, 1.7];
    for (i, r) in ratings.iter().enumerate() {
        let n = (i + 1) as u8;
        let last = 1_000 - (i as Time) * 37;
        let opinion = if i % 2 == 0 { 1.0 } else { 0.0 };
        source(e, n, *r, key(9), opinion, last);
    }
    (target, 1_000)
}

/// 同一引擎连续查询必须逐位相同；两个独立构造的引擎也必须逐位相同。
///
/// 后半句才是重点：`views` 用 HashMap 时，每个实例的哈希种子不同，
/// 遍历顺序随之不同，浮点累加顺序一变，最后几位就抖了。
#[test]
fn query_is_bit_identical_across_calls_and_instances() {
    let mut a = engine(base());
    let (ta, now) = staggered_scenario(&mut a);
    let mut b = engine(base());
    let (tb, _) = staggered_scenario(&mut b);

    let first = a.query(ta, now).to_bits();
    for _ in 0..8 {
        assert_eq!(
            a.query(ta, now).to_bits(),
            first,
            "同一引擎连续调用必须逐位相同"
        );
    }
    assert_eq!(
        b.query(tb, now).to_bits(),
        first,
        "两个独立构造的引擎必须逐位相同（views 必须是确定性容器）"
    );

    // 完整分布同样要逐位一致
    let da = a.trust_distribution(now);
    let db = b.trust_distribution(now);
    assert_eq!(da.len(), db.len());
    for (x, y) in da.iter().zip(db.iter()) {
        assert_eq!(x.0, y.0, "分布里的账户顺序也必须一致");
        assert_eq!(x.1.to_bits(), y.1.to_bits(), "分布必须逐位一致");
    }
}

// ---------------------------------------------------------------------------
// 13. 退避与兜底探测（设计规格 4.9.4）
// ---------------------------------------------------------------------------

/// 失败计入连败、成功清零；改分不算「刚取到观点」，连败保留。
#[test]
fn failures_count_for_backoff_and_successes_clear_them() {
    let mut e = engine(base());
    let s = e.intern(key(1));
    e.rate(s, 1.0, 0);
    e.on_view(s, full(key(9)), 0, 0);
    assert_eq!(e.fail_streak(s), 0);
    assert_eq!(e.view_age(s, 0), Some(0));

    // 失败：连败 +1，但 last_check 不动（观点照旧变旧）
    e.on_view_failed(s, 0);
    e.on_view_failed(s, 0);
    assert_eq!(e.fail_streak(s), 2);
    assert_eq!(e.view_age(s, 100), Some(100), "失败不得刷新 last_check");

    // 改分不算「刚取到观点」：连败保留、观点年龄不变
    e.rate(s, 2.0, 200);
    assert_eq!(e.fail_streak(s), 2, "只有成功刷新才清零退避");
    assert_eq!(e.view_age(s, 200), Some(200));

    // 成功：清零 + 年龄归零
    e.on_view(s, full(key(9)), 0, 200);
    assert_eq!(e.fail_streak(s), 0);
    assert_eq!(e.view_age(s, 200), Some(0));

    // 没有评分就没有退避记录
    assert_eq!(e.fail_streak(999), 0);
}

/// 连败的来源被抽中的概率急剧下降（退避 = 权重 1/2^n）。
///
/// 注意「一轮不放回」的语义：退避改变的是**本轮谁先被问**，
/// 连败者会被排到最后 —— 一轮走完它仍然会被问到一次（这是有意的，
/// 否则离线节点永远回不到 live 集合）。所以这里每轮都从「全员待抽」开始，
/// 只看每一轮的**第一抽**。
#[test]
fn tick_backs_off_sources_that_keep_failing() {
    let mut e = engine(base());
    for i in 1..=3u8 {
        let s = e.intern(key(i));
        e.rate(s, 1.0, 0);
        e.on_view(s, full(key(9)), 0, 0);
    }
    let bad = e.intern(key(1)); // 编号 0
    for _ in 0..20 {
        e.on_view_failed(bad, 0);
    }
    assert_eq!(e.fail_streak(bad), 20);

    // 每轮的第一抽：连败 20 次的权重是 2^-20，六百分之一都不到
    let mut counts = [0usize; 3];
    for _ in 0..600 {
        e.restart_round();
        for req in e.tick(0, 1) {
            counts[req.idx as usize] += 1;
        }
    }
    assert!(
        counts[0] <= 2,
        "连败 20 次的来源权重是 2^-20，第一抽几乎抽不到，实际 {} 次",
        counts[0]
    );
    assert!(
        counts[1] > 250 && counts[2] > 250,
        "另外两个应各占约一半，实际 {:?}",
        counts
    );

    // 一轮走完：连败者必然被排到最后（这是「不放回」的必然结果，也是兜底恢复的前提）
    e.restart_round();
    let mut round = Vec::new();
    for _ in 0..3 {
        round.extend(e.tick(0, 1).into_iter().map(|r| r.idx));
    }
    assert_eq!(round.len(), 3);
    assert_ne!(round[0], 0, "连败最重的来源不该第一个被问：{round:?}");
    assert_eq!(round[2], 0, "它应当最后被问：{round:?}");
}

/// 兜底探测不看退避：本轮剩下的全部返回，按编号升序，且不重复已抽过的。
#[test]
fn probe_covers_the_whole_round_without_backoff() {
    let mut e = engine(base());
    for i in 1..=4u8 {
        let s = e.intern(key(i));
        e.rate(s, 1.0, 0);
        e.on_view(s, full(key(9)), 0, 0);
    }
    let bad = e.intern(key(3));
    for _ in 0..30 {
        e.on_view_failed(bad, 0);
    }

    let first = e.tick(0, 1);
    assert_eq!(first.len(), 1);
    assert_eq!(e.round_remaining(), 3);

    let rest = e.probe(0);
    assert_eq!(rest.len(), 3, "兜底探测必须覆盖本轮剩下的全部条目");
    let ids: Vec<u32> = rest.iter().map(|r| r.idx).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "探测结果按编号升序");
    assert!(
        ids.contains(&bad),
        "连败最严重的来源也必须被探测到，否则它永远回不到 live"
    );
    assert!(!ids.contains(&first[0].idx), "本轮抽过的不再出现");

    let mut all = ids;
    all.push(first[0].idx);
    all.sort_unstable();
    assert_eq!(all, vec![0, 1, 2, 3], "一轮里两次调用恰好覆盖全部条目");
    assert_eq!(e.round_remaining(), 0);
}
/// N_min 门槛按 **live 数**判定，而不是「参与聚合的条目数」。
///
/// 覆盖率低于 θ_remove 时非 live 来源仍参与聚合；此时若 live 只有 1 个，
/// 即便有 3 个来源在算，群体项也必须置空 —— 这就是「我只能看到 1/4 的世界，
/// 就不把群体意见当回事」的意思。
#[test]
fn min_sources_gate_counts_live_not_participating_sources() {
    let mut e = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.99, // 覆盖率永远达不到 ⇒ 永不剔除
        min_group_sources: 3,
        ..base()
    });
    let target = e.intern(key(9));
    let rival = e.intern(key(20));
    e.rate(target, 1.0, 0);
    e.rate(rival, 1.0, 0);
    source(&mut e, 1, 1.0, key(9), 1.0, 1_000); // 唯一 live 的来源
    source(&mut e, 2, 1.0, key(9), 1.0, 800); // 过期
    source(&mut e, 3, 1.0, key(9), 1.0, 800); // 过期

    assert_eq!(e.live_count(1_000), 1);
    assert_eq!(e.coverage(1_000), 0.2, "1 live / 5 条正分");
    let share = f64::from(e.share_of(target).unwrap());
    let q = f64::from(e.query(target, 1_000));
    assert!(
        (q - 0.3 * share).abs() < 1e-6,
        "live 只有 1 个 < N_min = 3 ⇒ 群体项置空，只剩 β×个人项，实际 {q}"
    );

    // 对照：N_min 降到 1，群体项立刻回来（三个来源都参与，衰减各不相同）
    let mut f = engine(EngineConfig {
        unreachable_after: 168,
        remove_when_coverage: 0.99,
        min_group_sources: 1,
        ..base()
    });
    let target = f.intern(key(9));
    let rival = f.intern(key(20));
    f.rate(target, 1.0, 0);
    f.rate(rival, 1.0, 0);
    source(&mut f, 1, 1.0, key(9), 1.0, 1_000);
    source(&mut f, 2, 1.0, key(9), 0.0, 800);
    source(&mut f, 3, 1.0, key(9), 0.0, 800);
    assert!(f.query(target, 1_000) > f64::from(0.3f32 * f.share_of(target).unwrap()) as f32);
}
/// 「认识却看不到」也会拉低覆盖率：给从没取过观点的账户打分，分母变大、分子不变。
///
/// 这是 live 判定取**观点缓存**的 last_check（而不是正分条目上的时间戳）的直接后果。
/// 语义上是对的：我认识它，但我听不到它说话。
#[test]
fn rating_an_unseen_account_lowers_coverage() {
    let mut e = engine(EngineConfig {
        group_half_life: 120,
        unreachable_after: 168,
        ..base()
    });
    for i in 1..=2u8 {
        source(&mut e, i, 1.0, key(9), 1.0, 1_000);
    }
    assert_eq!(e.coverage(1_000), 1.0, "两个来源都新鲜 ⇒ 2 / 2");
    assert_eq!(e.group_scale(1_000), 1.0);

    // 再给一个我从没取到过观点的账户打分：分母 3、分子还是 2
    let unseen = e.intern(key(77));
    e.rate(unseen, 1.0, 1_000);
    assert_eq!(e.positive_count(), 3);
    assert_eq!(e.live_count(1_000), 2, "没取到过观点的来源不算 live");
    assert!((e.coverage(1_000) - 2.0 / 3.0).abs() < 1e-6, "2 / 3");
    assert!(
        e.group_scale(1_000) < 1.0,
        "覆盖率下降 ⇒ s 下降，实际 {}",
        e.group_scale(1_000)
    );
}
