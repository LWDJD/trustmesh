//! # 放大器攻击：发布方把整份观点放大 k 倍，能不能突破影响力预算？
//!
//! A11 报告（`审计/A11-规模性能内存.md` §5）记录过一个**旧**缺陷：
//! `on_view` 的 `total_mass` 估计对每账户做 `clamp(0,1)`，所以发布方把整份观点
//! 放大 k 倍可以让质量估计饱和，而逐条查询值继续线性增长 ⇒ 配额被放大。
//! 实测「d=512 的攻击者用 1/200 的预算捕获 63.5% 的总信任（预算的 181 倍）」。
//!
//! P0② 的修复顺序是「**分子先夹紧，再除以同口径分母**」（`reading_of`）：
//!
//! ```text
//! total_mass = Σ_known min(q, 1)
//! reading    = min(min(q,1) / total_mass, 1)
//! ```
//!
//! 于是放大 k 倍时分子分母同时乘 k（被 min 截断后其实是同时饱和），比值不变。
//! **本文件钉住这个性质**：这是「影响力预算」能不能被绕过的最短路径。
//!
//! 如果哪天有人把 `reading_of` 改回「先除后夹」或改回「分母带 clamp 但分子不带」，
//! 这些测试会立刻红。

use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = (i >> 8) as u8;
    k[2] = 0xAB;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(2048, 5, 0),
        half_life: 72,
        ..EngineConfig::default()
    }
}

/// 建一个引擎：known 个账户全部 intern，observer 评过其中的 `rated` 个（各 1.0）。
fn engine_with(known: u32, rated: u32) -> Engine {
    let mut e = Engine::with_seed(cfg(), 0xA11A_0000_0000_0001);
    for i in 0..known {
        e.intern(key(i));
    }
    for i in 0..rated {
        e.rate(i, 1.0, 0);
    }
    e
}

/// 一份「把每个已知账户都声明成 mul 倍」的观点。
fn amplified_view(known: u32, mul: f32) -> CompressedView {
    let vals: Vec<([u8; 32], f32)> = (0..known).map(|i| (key(i), 1.0 * mul)).collect();
    CompressedView::from_pairs(ViewParams::new(2048, 5, 0), &vals)
}

/// 一份合规观点：每个账户 1.0（总和 = known，本来就不是归一化的，正用来测 total_mass）。
fn plain_view(known: u32) -> CompressedView {
    amplified_view(known, 1.0)
}

/// 核心性质：**放大系数不进最终读数**。
///
/// 同一个来源，喂合规视图 vs 放 1e6 倍的视图，群体项里这个来源的那一票必须相同。
#[test]
fn amplification_does_not_change_the_reading() {
    let known: u32 = 64;
    let rated: u32 = 16;

    let mut base = engine_with(known, rated);
    base.on_view(0, plain_view(known), 0, 0);
    let v1 = base.query(0, 0);

    let mut amp = engine_with(known, rated);
    amp.on_view(0, amplified_view(known, 1e6), 0, 0);
    let v2 = amp.query(0, 0);

    assert_eq!(
        v1.to_bits(),
        v2.to_bits(),
        "放大 1e6 倍后读数必须逐位不变（v1={v1}, v2={v2}）——否则影响力预算可被绕过"
    );
}

/// 多个放大来源同时存在时，读数也不变（分母饱和 + 分子饱和，比值守恒）。
#[test]
fn multiple_amplified_sources_do_not_break_the_budget() {
    let known: u32 = 64;
    let rated: u32 = 8;

    let mut normal = engine_with(known, rated);
    let mut attack = engine_with(known, rated);
    for owner in 0..rated {
        normal.on_view(owner, plain_view(known), 0, 0);
        attack.on_view(owner, amplified_view(known, 1e6), 0, 0);
    }

    // 逐账户比较整条声望向量
    for t in 0..known {
        assert_eq!(
            normal.query(t, 0).to_bits(),
            attack.query(t, 0).to_bits(),
            "账户 {t}: 放大攻击改变了声望（正常 {}, 攻击 {}）",
            normal.query(t, 0),
            attack.query(t, 0)
        );
    }
}

/// 放大攻击者不能靠放大拿到「总和 > 1」的份额：
/// 整个分布归一化之后，攻击者自己（也在被评分者里）的份额不应因此升高。
#[test]
fn amplification_cannot_inflate_the_attacker_promoted_target() {
    let known: u32 = 64;
    // 观察者评过 0..8；其中 0 号是攻击者，1 号是被抬举目标，9..64 是没被评过的旁观者。
    let rated: u32 = 8;

    let mut normal = engine_with(known, rated);
    let mut attack = engine_with(known, rated);
    for owner in 0..rated {
        // 每个来源的视图都一样：都给所有人 1.0 —— 这是「大家被平等声明」的基线
        normal.on_view(owner, plain_view(known), 0, 0);
        attack.on_view(owner, amplified_view(known, 1e6), 0, 0);
    }

    let promoted_normal = normal.query(1, 0);
    let promoted_attack = attack.query(1, 0);
    assert_eq!(
        promoted_normal.to_bits(),
        promoted_attack.to_bits(),
        "被抬举目标在放大攻击下的声望必须不变"
    );
}

/// 反过来的形状：**只有一条声明被放大**（其余合规）。
///
/// 这是最刁钻的一种：分子 `min(q,1)` 对那条声明饱和到 1，但分母因为那条从 0.x 涨到 1
/// 而变大 ⇒ 读数应当**下降**，绝不能上升。
#[test]
fn amplifying_a_single_declaration_never_increases_it() {
    let known: u32 = 64;
    let rated: u32 = 8;

    // 合规：来源 0 的视图里只给 1 号账户 1.0，其余都不写
    let single = vec![(key(1), 1.0f32)];
    let view_single = CompressedView::from_pairs(ViewParams::new(2048, 5, 0), &single);

    // 放大：同一条声明写成 1e6
    let single_amp = vec![(key(1), 1e6f32)];
    let view_single_amp = CompressedView::from_pairs(ViewParams::new(2048, 5, 0), &single_amp);

    let mut a = engine_with(known, rated);
    a.on_view(0, view_single, 0, 0);
    let normal = a.query(1, 0);

    let mut b = engine_with(known, rated);
    b.on_view(0, view_single_amp, 0, 0);
    let amplified = b.query(1, 0);

    assert!(
        amplified <= normal,
        "单条放大后读数不能上升（正常 {normal}, 放大 {amplified}）"
    );
}

/// 预算的绝对上限：一份观点对一个账户最多只能拿满 1 份份额。
///
/// 这是影响力预算的**定义**：`Σ_v min(q_v,1)/total_mass <= 1` 对任意单来源成立。
/// 用「一个来源声明 1e9」这种极端值验证。
#[test]
fn a_single_source_can_never_exceed_one_full_share() {
    let known: u32 = 32;
    // 观察者评过 0..4，其中 0 号是那个「唯一来源」
    let rated: u32 = 4;

    let mut e = engine_with(known, rated);
    let vals = vec![(key(0), 1e9f32)];
    e.on_view(0, CompressedView::from_pairs(ViewParams::new(2048, 5, 0), &vals), 0, 0);

    // 来源 0 自己的份额是 1/4；它给 0 号（自己）的读数最多 1 ⇒ 群体项 <= 1
    // 于是 query(0) <= β·(1/4) + (1−β)·1
    let q = e.query(0, 0);
    let beta = cfg().beta;
    let upper = beta * (1.0 / rated as f32) + (1.0 - beta) * 1.0;
    assert!(
        q <= upper + 1e-6,
        "单来源读数超过 1 份份额：query={q}, 上限={upper}"
    );
}

/// 极端放大（inf）不能让引擎产生非有限读数。
///
/// 注意：`CompressedView::from_pairs` 会把 inf 原样写进桶里（它是纯粹的数据容器），
/// 引擎侧必须在 `reading_of` 把非有限值当 0。这条测试钉住那个兜底。
#[test]
fn infinite_amplification_is_treated_as_no_statement() {
    let known: u32 = 32;
    let rated: u32 = 4;

    let mut e = engine_with(known, rated);
    let vals: Vec<([u8; 32], f32)> = (0..known).map(|i| (key(i), f32::INFINITY)).collect();
    e.on_view(0, CompressedView::from_pairs(ViewParams::new(2048, 5, 0), &vals), 0, 0);

    // 不管怎么算，读数必须是有限的、且在 [0,1]
    for t in 0..known {
        let q = e.query(t, 0);
        assert!(q.is_finite(), "账户 {t} 的声望非有限：{q}");
        assert!((0.0..=1.0).contains(&q), "账户 {t} 的声望越界：{q}");
    }
}
