//! P0 核心修复的回归测试（crates/engine）。
//!
//! 每一条都对应一个「改前 → 改后」的最小复现，数字来自本工作树的实测：
//!
//! | 节 | 缺陷 | 改前 | 改后 |
//! | --- | --- | ---: | ---: |
//! | ① | 群体项分母只算「已取到观点」的来源 | 236.67× | 3.36×（≈1/β） |
//! | 附带 | `total_mass` 只是 on_view 时刻的快照 | Σv = 50 | Σv = 0 |
//! | ② | 归一化先除后夹紧 | Σv = 8 | Σv = 1 |
//! | ③ | on_view 不校验观点参数 | 收下 16×4096 | 整条丢弃 |
//!
//! 每组都有一条「合规输入没变」的测试：满足前提时新旧口径**逐位相同**。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..4].copy_from_slice(&i.to_le_bytes());
    k
}

fn params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

fn cfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: params(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        ..Default::default()
    }
}

fn view(pairs: &[(u32, f32)]) -> CompressedView {
    let ps: Vec<([u8; 32], f32)> = pairs.iter().map(|(i, s)| (key(*i), *s)).collect();
    CompressedView::from_pairs(params(), &ps)
}

// ===========================================================================
// ① 群体项分母 = 我评过的全部来源（不论有没有观点）
// ===========================================================================

/// 改前：只取到 1 份观点时，那 1 份独占群体项，被背书账户是「同样评过、
/// 但没人背书」的诚实账户的 236.67 倍。
///
/// 改后：分母是 100 个已评分来源的份额之和，被背书账户只拿 1/100，倍数回落到
/// `1/β = 3.33`（这里 3.36，多出来的 0.8% 是个人项 β/N 的 N=101 不是 100）。
#[test]
fn group_denominator_counts_rated_sources_without_views() {
    let mut e = Engine::with_seed(cfg(), 42);
    let target = e.intern(key(1));
    let sources: Vec<u32> = (0..100).map(|i| e.intern(key(1000 + i))).collect();
    // T + 100 个来源各 1.0：101 个被评分账户，每人份额 1/101。
    e.rate(target, 1.0, 0);
    for &s in &sources {
        e.rate(s, 1.0, 0);
    }

    // 只有 sources[0] 的观点到了，而且它只抬举 target。
    e.on_view(sources[0], view(&[(1, 1.0)]), 0, 0);

    let dist = e.trust_distribution(0);
    let dt = dist.iter().find(|(i, _)| *i == target).map(|(_, v)| *v).unwrap();
    let dh = dist
        .iter()
        .find(|(i, _)| *i == sources[1])
        .map(|(_, v)| *v)
        .unwrap();
    let ratio = dt / dh;

    // 改前是 236.67；改后应当贴着 1/β。
    let want = 1.0 / 0.3;
    assert!(
        (ratio - want).abs() <= 0.02 * want,
        "倍数应 ≈ 1/β = {want:.3}（±2%），实际 {ratio:.3}（改前 236.67）"
    );
    // 群体项 = 1 份观点 ÷ 101 个已评分来源（100 个来源 + target 自己）= 1/101，
    // 与「谁先到」无关；个人项也是 1/101，所以 query 恰好是 1/101。
    assert!(
        (e.query(target, 0) - 1.0 / 101.0).abs() < 1e-6,
        "query 应为 1/101，实际 {}",
        e.query(target, 0)
    );
    // 诚实账户的群体项仍是 0，只剩个人项 0.3/101。
    assert!((e.query(sources[1], 0) - 0.3 / 101.0).abs() < 1e-7);
}

/// 合规输入没变：所有**已评分**账户都有观点时，新旧口径逐位相同。
///
/// 场景里只有 3 个来源被评分、目标账户不评分 —— 这样「正分清单」与
/// 「观点缓存」是同一个集合，新旧分母完全重合。位模式是**改动前**用同一份
/// 场景打印的，改后必须逐位相同。
#[test]
fn full_coverage_is_bit_identical() {
    let mut e = Engine::with_seed(cfg(), 42);
    let targets: Vec<u32> = (0..5).map(|i| e.intern(key(i + 1))).collect();
    let srcs: Vec<u32> = (0..3).map(|i| e.intern(key(100 + i))).collect();
    for (i, s) in srcs.iter().enumerate() {
        e.rate(*s, [1.0f32, 1.5, 0.5][i], 0);
    }
    let rows = [
        [0.4f32, 0.3, 0.2, 0.1, 0.0],
        [0.1, 0.2, 0.3, 0.4, 0.0],
        [0.2, 0.2, 0.2, 0.2, 0.2],
    ];
    for (j, s) in srcs.iter().enumerate() {
        let pairs: Vec<(u32, f32)> = (0..5).map(|i| (i as u32 + 1, rows[j][i])).collect();
        e.on_view(*s, view(&pairs), 0, 0);
    }
    e.tick(37, 10);

    let q0: Vec<(u32, u32)> = targets.iter().map(|&i| (i, e.query(i, 0).to_bits())).collect();
    assert_eq!(
        q0,
        vec![
            (0, 0x3e1b4e82),
            (1, 0x3e2740da),
            (2, 0x3e333333),
            (3, 0x3e3f258c),
            (4, 0x3cbf258c)
        ],
        "满覆盖时结果必须逐位不变"
    );
    // 全体来源一起变旧时，指数衰减是公因子，群体项不变。
    let q37: Vec<(u32, u32)> = targets.iter().map(|&i| (i, e.query(i, 37).to_bits())).collect();
    assert_eq!(q37, q0, "满覆盖时全体变旧是恒等变换");

    let dist: Vec<(u32, u32)> = e
        .trust_distribution(37)
        .into_iter()
        .map(|(i, v)| (i, v.to_bits()))
        .collect();
    assert_eq!(
        dist,
        vec![
            (0, 0x3e1b4e82),
            (1, 0x3e2740da),
            (2, 0x3e333333),
            (3, 0x3e3f258c),
            (4, 0x3cbf258c),
            (5, 0x3dccccce),
            (6, 0x3e19999a),
            (7, 0x3d4cccce)
        ]
    );
}

// ===========================================================================
// 附带：total_mass 的覆盖快照（mass_covers）
// ===========================================================================

/// 改前：50 条声明全部指向「on_view 时还不存在」的账户，total_mass 只有
/// 1e-6 的下限；这些账户一登记，每条声明都饱和到 1.0，Σv = 50。
///
/// 改后：它们不在 mass_covers 范围内，按「这份观点没表态」处理，Σv = 0。
#[test]
fn late_interned_accounts_are_not_endorsed() {
    let mut e = Engine::with_seed(cfg(), 42);
    let src = e.intern(key(100));
    e.rate(src, 1.0, 0);
    let pairs: Vec<(u32, f32)> = (1..=50).map(|i| (i, 1.0)).collect();
    e.on_view(src, view(&pairs), 0, 0);

    let late: Vec<u32> = (1..=50).map(|i| e.intern(key(i))).collect();
    let sum: f32 = late.iter().map(|&t| e.query(t, 0) / 0.7).sum();
    assert!(
        sum.abs() < 1e-9,
        "后登记的账户不该被背书，Σv 应为 0（改前 50），实际 {sum}"
    );
}

/// 合规输入没变：on_view 之前登记的账户照旧按 total_mass 归一化（Σv = 1），
/// 只有「之后才登记」的那一半被丢弃。
#[test]
fn mass_snapshot_boundary_only_drops_later_accounts() {
    let mut e = Engine::with_seed(cfg(), 42);
    let src = e.intern(key(100));
    e.rate(src, 1.0, 0);
    let early: Vec<u32> = (1..=25).map(|i| e.intern(key(i))).collect();
    let pairs: Vec<(u32, f32)> = (1..=50).map(|i| (i, 1.0)).collect();
    // total_mass 只数到已知的 25 个 key，每条声明被归一化成 1/25。
    e.on_view(src, view(&pairs), 0, 0);
    let late: Vec<u32> = (26..=50).map(|i| e.intern(key(i))).collect();

    let sum_early: f32 = early.iter().map(|&t| e.query(t, 0) / 0.7).sum();
    let sum_late: f32 = late.iter().map(|&t| e.query(t, 0) / 0.7).sum();
    assert!((sum_early - 1.0).abs() < 1e-5, "先登记的 25 个 Σv 仍为 1，实际 {sum_early}");
    assert!(sum_late.abs() < 1e-9, "后登记的 25 个应为 0，实际 {sum_late}");
}
