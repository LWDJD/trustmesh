//! # 负值模式（signed scores）
//!
//! `EngineConfig::signed_scores` 打开后，**群体项**的值域从 `[0, 1]` 放宽到 `[-1, 1]`：
//! 负值表示反向流动（资源分配场景里「我欠你」）。
//!
//! ## 两种模式互斥，不是叠加
//!
//! * `false`（**默认**）：分数是「信任度」，值域 `[0, 1]`。负分被静默夹成 0。
//!   这是反女巫场景的唯一正确语义 —— 声望没有负数。
//! * `true`：分数是「带符号的观点」，值域 `[-1, 1]`。**开启后不建议再用作反女巫**：
//!   Count Sketch 里负分与正分落在同一批桶里相加，攻击者若在信任白名单内
//!   就能用自己的负分压低别人（实测幅度 = 他的份额）。
//!
//! ## 本文件钉住的三组性质
//!
//! 1. **默认模式零漂移**：`signed_scores = false` 时，负分仍然静默失效（行为与改动前一致），
//!    且默认配置与「显式设 false」逐位相同；
//! 2. **signed 模式负值生效**：负分能穿透 `reading_of` / `aggregate` / `combine` 三道 clamp；
//! 3. **signed 模式的成本约束**：`total_mass = Σ|q|` 让「堆大量负值」不能把目标压到 -1。
//!
//! 不含：跨层安全保证。**谁能贡献由应用层的信任筛选决定**，本库只保证「负值能被正确表达」。

use tm_engine::agg::{aggregate, aggregate_signed, clamp_signed, combine, combine_signed, AggMode};
use tm_engine::view::{AccountKey, CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(i: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[0] = (i & 0xFF) as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0xC7;
    k
}

fn params() -> ViewParams {
    ViewParams::new(2048, 5, 0)
}

fn cfg(signed: bool) -> EngineConfig {
    EngineConfig {
        view_params: params(),
        half_life: 72,
        signed_scores: signed,
        ..EngineConfig::default()
    }
}

/// 建一个「已 intern 若干账户、评过 1 个来源」的引擎。
fn eng(signed: bool, known: u32) -> Engine {
    let mut e = Engine::with_seed(cfg(signed), 0x51_6E_00_00_00_00_00_01);
    for i in 0..known {
        e.intern(key(i));
    }
    // 来源账户单独 intern（编号 known），评 1.0
    let owner = e.intern(key(known));
    e.rate(owner, 1.0, 0);
    e
}

// ===========================================================================
// ① 默认模式：负值仍然静默失效（钉住现状，证明没偷偷改语义）
// ===========================================================================

/// 默认配置（`signed_scores = false`）下，负分视图对读数**没有任何影响**。
///
/// 这条是**刻意的现状钉住**：负值功能默认关闭时，行为必须与引入该功能之前
/// 完全相同 —— 包括「静默吞掉负分」这个已知的坏行为。
/// 如果有人把默认值改成 `true`，这条会立刻红。
#[test]
fn default_mode_still_swallows_negative_scores() {
    let mut e = eng(false, 5);
    let owner = e.lookup(&key(5)).unwrap();
    // 喂一份只有负值的视图：目标 0 写 -1.0
    let neg = CompressedView::from_pairs(params(), &[(key(0), -1.0)]);
    e.on_view(owner, neg, 0, 0);
    // 默认模式下 reading_of 把 -1.0 clamp 成 0 ⟹ 读数恒为 0
    assert_eq!(
        e.query(0, 0).to_bits(),
        0.0f32.to_bits(),
        "默认模式下负分必须被静默夹成 0（现状不许变）"
    );
    // 视图本身**被接受了**（进缓存、计命中）—— 这也是现状的一部分
    assert_eq!(e.stats().1, 1, "视图被接受并计命中");
    assert_eq!(e.stats().2, 1, "视图进缓存");
    assert_eq!(e.rejected_count(), 0, "默认模式不把负分视图算作拒绝");
}

/// `EngineConfig::default()` 与「显式写 signed_scores = false」逐位相同。
///
/// 这条为什么重要：61 个 `EngineConfig` 字面量里有很多带 `..Default::default()`。
/// 如果 `Default` 的 `signed_scores` 不是 `false`，那这 61 处会静默变成 signed 模式。
#[test]
fn default_config_equals_explicit_false() {
    assert!(!EngineConfig::default().signed_scores, "默认必须是 false");
    // 两个引擎用同一 seed、同一批输入，读数必须逐位相同
    let mut a = eng(false, 5);
    let mut b = eng(false, 5);
    let v = CompressedView::from_pairs(params(), &[(key(0), -1.0), (key(1), 0.5)]);
    a.on_view(a.lookup(&key(5)).unwrap(), v.clone(), 0, 0);
    b.on_view(b.lookup(&key(5)).unwrap(), v, 0, 0);
    for i in 0..5u32 {
        assert_eq!(
            a.query(i, 0).to_bits(),
            b.query(i, 0).to_bits(),
            "默认配置与显式 false 必须逐位相同（账户 {i}）"
        );
    }
}

// ===========================================================================
// ② signed 模式：负值能穿透三道 clamp
// ===========================================================================

/// signed 模式下负分视图能产生**负读数**。
///
/// 这是本功能的核心：`reading_of` 的 clamp 从 `[0,1]` 变成 `[-1,1]`。
#[test]
fn signed_mode_admits_negative_readings() {
    let mut e = eng(true, 5);
    let owner = e.lookup(&key(5)).unwrap();
    let neg = CompressedView::from_pairs(params(), &[(key(0), -1.0)]);
    e.on_view(owner, neg, 0, 0);
    let q = e.query(0, 0);
    assert!(q < 0.0, "signed 模式下负分必须产生负读数，实际 {q}");
    assert!(q >= -1.0, "读数不得越界，实际 {q}");
}

/// `clamp_signed` 的边界行为。
#[test]
fn clamp_signed_boundaries() {
    assert_eq!(
        clamp_signed(f32::NAN).to_bits(),
        0.0f32.to_bits(),
        "NaN → 0"
    );
    assert_eq!(clamp_signed(f32::INFINITY), 1.0, "+∞ → 1");
    assert_eq!(clamp_signed(f32::NEG_INFINITY), -1.0, "−∞ → −1");
    assert_eq!(clamp_signed(2.0), 1.0, "超上界截断");
    assert_eq!(clamp_signed(-2.0), -1.0, "超下界截断");
    // 区间内的值必须**逐位原样**返回（不是「接近」，是 to_bits 相同）
    assert_eq!(
        clamp_signed(-0.5).to_bits(),
        (-0.5f32).to_bits(),
        "−0.5 逐位不变"
    );
    assert_eq!(
        clamp_signed(0.5).to_bits(),
        0.5f32.to_bits(),
        "0.5 逐位不变"
    );
    assert_eq!(clamp_signed(0.0).to_bits(), 0.0f32.to_bits(), "0 逐位不变");
}

/// `aggregate_signed` 与 `aggregate` 在**全正值**输入下结果逐位相同。
///
/// 这条证明 signed 变体只放宽了出口值域，**没有改聚合口径**。
#[test]
fn aggregate_signed_matches_aggregate_on_positive_input() {
    let items = [(0.3f32, 0.2f32), (0.5, 0.8), (0.2, 1.0)];
    for mode in [
        AggMode::Mean,
        AggMode::Median,
        AggMode::WeightedMedian,
        AggMode::Trimmed(0.2),
    ] {
        let a = aggregate(&items, mode);
        let b = aggregate_signed(&items, mode);
        assert_eq!(
            a.map(|x| x.to_bits()),
            b.map(|x| x.to_bits()),
            "全正值输入下两种聚合必须逐位相同（{mode:?}）"
        );
    }
    assert_eq!(aggregate(&[], AggMode::Mean), None);
    assert_eq!(aggregate_signed(&[], AggMode::Mean), None);
}

/// `aggregate_signed` 能返回负值。
#[test]
fn aggregate_signed_admits_negative_result() {
    let items = [(1.0f32, -1.0f32), (1.0, 0.0)];
    assert_eq!(aggregate_signed(&items, AggMode::Mean).unwrap(), -0.5);
    assert_eq!(aggregate_signed(&items, AggMode::Median).unwrap(), -0.5);
    // 对照：普通 aggregate 会把它 clamp 成 0
    assert_eq!(aggregate(&items, AggMode::Mean).unwrap(), 0.0);
}

/// `combine_signed` 里 **personal 仍非负**，只有 group 可取负。
///
/// 这是刻意的语义分层：个人项是「我对你的信任」，永远不该是负数；
/// 它的来源是 `ratings`（正分清单），而那张表的语义是 `score <= 0 = 撤销`。
#[test]
fn combine_signed_keeps_personal_non_negative() {
    // group 取负，personal 即使传负数也应被 clamp 成 0
    assert_eq!(combine_signed(0.5, -0.5, Some(-1.0)), -0.5);
    // personal = 0, group = -1 ⟹ 结果 -1
    assert_eq!(
        combine_signed(1.0, 0.0, Some(-1.0)),
        0.0,
        "beta=1 时只剩个人项"
    );
    assert_eq!(
        combine_signed(0.0, 0.0, Some(-1.0)),
        -1.0,
        "beta=0 时只剩群体项"
    );
    // 全正输入下与 combine 逐位相同
    for (b, p, g) in [(0.3f32, 0.5f32, 0.8f32), (1.0, 0.7, 0.2), (0.0, 0.9, 0.0)] {
        assert_eq!(
            combine(b, p, Some(g)).to_bits(),
            combine_signed(b, p, Some(g)).to_bits(),
            "全正输入下必须与 combine 逐位相同"
        );
    }
}

// ===========================================================================
// ③ signed 模式的成本约束：堆大量负值不能把目标压到 -1
// ===========================================================================

/// skeptical 场景：攻击者写「1000 个账户各 -1.0」的视图，
/// 目标账户的读数必须**远大于** -1（因为分母 `Σ|q|` 也是 1000）。
///
/// 这是 `total_mass` 改成 `Σ|q|` 的**唯一理由**：
/// 如果分母仍用 `Σ min(q,1)`，负值不进分母 ⟹ 分母落到 `1e-6` 下限
/// ⟹ 单个视图就能把任何目标压到 -1。
#[test]
fn mass_of_negatives_cannot_floor_the_target() {
    const KNOWN: u32 = 1000;
    let mut e = eng(true, KNOWN);
    let owner = e.lookup(&key(KNOWN)).unwrap();
    // 攻击者把 1000 个已知账户各写 -1.0
    let pairs: Vec<(AccountKey, f32)> = (0..KNOWN).map(|i| (key(i), -1.0)).collect();
    let attack = CompressedView::from_pairs(params(), &pairs);
    e.on_view(owner, attack, 0, 0);
    let q = e.query(0, 0);
    // 分母 = Σ|q| ≈ 1000 ⟹ 读数 ≈ -1/1000 = -0.001，绝不是 -1
    assert!(q < 0.0, "signed 模式下负值应生效，实际 {q}");
    assert!(
        q > -0.01,
        "堆 1000 个负值不能把目标压到底：实际 {q}（分母若用 Σmin(q,1) 会得到 -1）"
    );
}

/// 单个可信来源的负值贡献被**份额**限制（不超出他的权重比例）。
#[test]
fn single_source_negative_contribution_is_bounded_by_share() {
    // 3 个来源各评 1.0（份额各 1/3），其中 1 个写 -1.0、2 个写 +1.0
    let mut e = eng(true, 4);
    for i in 100..103u32 {
        let owner = e.intern(key(i));
        e.rate(owner, 1.0, 0);
    }
    let neg_owner = e.lookup(&key(100)).unwrap();
    let pos1 = e.lookup(&key(101)).unwrap();
    let pos2 = e.lookup(&key(102)).unwrap();
    e.on_view(
        neg_owner,
        CompressedView::from_pairs(params(), &[(key(0), -1.0)]),
        0,
        0,
    );
    e.on_view(
        pos1,
        CompressedView::from_pairs(params(), &[(key(0), 1.0)]),
        0,
        0,
    );
    e.on_view(
        pos2,
        CompressedView::from_pairs(params(), &[(key(0), 1.0)]),
        0,
        0,
    );
    let q = e.query(0, 0);
    // 群体项 = (1·(-1) + 1·1 + 1·1)/3 = 1/3 ⟹ 读数 1/3（被 clamp 到 [-1,1] 内）
    assert!(q > 0.0, "2 正 1 负 ⟹ 净效果仍为正，实际 {q}");
    assert!(q < 1.0, "单个负值来源拉低了它，实际 {q}");
}

// ===========================================================================
// ④ 噪声底按幅度判（signed 模式）
// ===========================================================================

/// **判别点**：signed 模式下噪声底按**幅度**判，负读数能通过门槛。
///
/// 这条测试的构造是本文件最讲究的一条 —— 我踩过两次坑：
///
/// **坑 1：`noise_multiplier` 抬不起稀疏视图的噪声底。**
/// `floor = noise.median * noise_multiplier`，而默认参数（2048x5）下写几百条
/// 的视图 `noise.median` 实测恒为 0 —— **0 乘任何数仍是 0**，门根本不存在。
/// 必须用**小块**（本例 64x5）才能让 median 非零。实测：b=64/h=5 写 200 条，median=1.0。
///
/// **坑 2：`noise_multiplier` 太大时测试失去判别力。**
/// 若 floor = 5.0，那么「正确实现（按幅度）」与「错误实现（按有符号值）」都输出 0，
/// 断言通过但测不出任何区别 —— **空转测试**。
/// 必须让 floor **小于** |v|：本例 floor = 1.0 * 0.01 = 0.01，而 |v| 约 0.056，
/// 正确实现保留负值、错误实现（`v < floor`，负数恒小于正数）会清零并导致断言失败。
#[test]
fn signed_noise_floor_uses_magnitude_not_signed_value() {
    let tiny = ViewParams::new(64, 5, 7);
    let cfg = EngineConfig {
        view_params: tiny,
        half_life: 72,
        noise_multiplier: 0.01, // floor = median(1.0) * 0.01 = 0.01
        signed_scores: true,
        ..EngineConfig::default()
    };
    let mut e = Engine::with_seed(cfg, 0x51_6E_00_00_00_00_00_04);
    let src = e.intern(key(1));
    let target = e.intern(key(2));
    for i in 100..118u32 {
        e.intern(key(i));
    }
    e.rate(src, 1.0, 0);
    // 关键构造：写 200 条**未登记**的 key（只制造碰撞噪声、不进 total_mass），
    // 再给目标写一个负分。这样 total_mass 小，读数绝对值大，高于 floor。
    let mut view = CompressedView::new(tiny);
    for i in 0..200u32 {
        view.add(&key(100_000 + i), 1.0);
    }
    view.add(&key(2), -0.5);
    e.on_view(src, view, 0, 0);

    // 注意：AccountIdx 是 intern 的先后次序，不是 key 里的数字！
    // key(2) 是第 2 个 intern 的 ⟹ 编号 1，不是 2。必须用 lookup 拿编号。
    let q = e.query(target, 0);
    // 正确实现（v.abs() < floor）：|0.056| > 0.01，保留，q 仍为负
    // 错误实现（v < floor）：-0.056 < 0.01，被清零，q == 0，本断言失败
    assert!(
        q < 0.0,
        "噪声底必须按幅度判：|读数| 高于 floor 时负值必须保留，实际 {q}"
    );
}

/// 对照组：噪声底**足够大**时，负读数被清零（证明门槛确实在起作用，不是永久失效）。
///
/// 与上一条配对使用才完整：单看「负值保留」无法区分「按幅度判」和「门槛根本没生效」。
#[test]
fn signed_noise_floor_can_still_zero_a_negative_reading() {
    let tiny = ViewParams::new(64, 5, 7);
    let cfg = EngineConfig {
        view_params: tiny,
        half_life: 72,
        // floor = 1.0 * 0.5 = 0.5，远大于 |读数| 约 0.056，必须清零
        noise_multiplier: 0.5,
        signed_scores: true,
        ..EngineConfig::default()
    };
    let mut e = Engine::with_seed(cfg, 0x51_6E_00_00_00_00_00_04);
    let src = e.intern(key(1));
    let target = e.intern(key(2));
    for i in 100..118u32 {
        e.intern(key(i));
    }
    e.rate(src, 1.0, 0);
    let mut view = CompressedView::new(tiny);
    for i in 0..200u32 {
        view.add(&key(100_000 + i), 1.0);
    }
    view.add(&key(2), -0.5);
    e.on_view(src, view, 0, 0);

    let q = e.query(target, 0);
    assert_eq!(
        q.to_bits(),
        0.0f32.to_bits(),
        "|读数| 低于 floor 时必须清零（按幅度判），实际 {q}"
    );
}
// ===========================================================================
// ⑤ 快照往返
// ===========================================================================

/// signed 模式的引擎，快照恢复后 `query` 逐位一致。
///
/// 注意：`signed_scores` 是 `EngineConfig` 字段，**不进快照**（cfg 不序列化，见
/// `snapshot.rs` 的「为什么快照不含 cfg」）。恢复时由调用方传入同一份 cfg。
#[test]
fn signed_mode_snapshot_roundtrip_is_bitwise_identical() {
    let c = cfg(true);
    let mut e = Engine::with_seed(c.clone(), 0x51_6E_00_00_00_00_00_03);
    for i in 0..6u32 {
        e.intern(key(i));
    }
    let owner = e.lookup(&key(5)).unwrap();
    e.rate(owner, 1.0, 0);
    let mixed = CompressedView::from_pairs(params(), &[(key(0), -1.0), (key(1), 0.5)]);
    e.on_view(owner, mixed, 0, 0);

    let snap = e.snapshot(0);
    let back = Engine::restore(c, snap).expect("自产快照必须能恢复");
    for i in 0..6u32 {
        assert_eq!(
            e.query(i, 0).to_bits(),
            back.query(i, 0).to_bits(),
            "恢复后读数必须逐位一致（账户 {i}）"
        );
    }
}

// ===========================================================================
// ⑥ 正负混合：净效果与符号
// ===========================================================================

/// 两个来源各写 +1 / -1 时，群体项是两者的加权平均（不是绝对值相加）。
#[test]
fn mixed_signs_average_to_zero() {
    let mut e = eng(true, 4);
    for i in 200..202u32 {
        let o = e.intern(key(i));
        e.rate(o, 1.0, 0);
    }
    let a = e.lookup(&key(200)).unwrap();
    let b = e.lookup(&key(201)).unwrap();
    e.on_view(
        a,
        CompressedView::from_pairs(params(), &[(key(0), 1.0)]),
        0,
        0,
    );
    e.on_view(
        b,
        CompressedView::from_pairs(params(), &[(key(0), -1.0)]),
        0,
        0,
    );
    let q = e.query(0, 0);
    assert!(q.abs() < 1e-6, "等权重的 +1 与 -1 应相消为 0，实际 {q}");
}
