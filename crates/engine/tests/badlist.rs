//! badlist（受害者反馈）的运行时行为测试（C2 · 一次性事件）。
//!
//! 覆盖：闭式解落地、无报告时逐位一致、均值以上统一折扣、重复报告不复合、
//! 多次报告取 min、边界与非法输入、确定性/顺序无关、export_view 传播、
//! 手动撤销、惩罚跨 tick 持久、tick 不自动生效。

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

fn eng() -> Engine {
    Engine::with_seed(cfg(), 42)
}

/// 只给一个账户满分的观点（归一化后它占 100%）。
fn full(i: u32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(key(i), 1.0)])
}

/// A 占 x、B 占 1-x 的观点。
fn mix(a: u32, x: f32, b: u32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(key(a), x), (key(b), 1.0 - x)])
}

/// 三来源场景：s0 只亲近 A、s1/s2 只亲近 filler；我给 s0 的评分为 3、其余为 1。
///
/// 读数 a = [1, 0, 0]，份额 w = [0.6, 0.2, 0.2]，ā = 1/3。
/// 闭式解 f = Σ_{S−} w(ā−a) / Σ_{S+} w(a−ā) = (0.4×1/3) / (0.6×2/3) = 1/3。
/// 压完后 A 的群体项 = (0.6×1/3) / (0.6×1/3 + 0.4) = 1/3。
fn scenario3_with(config: EngineConfig) -> (Engine, u32, u32) {
    let mut e = Engine::with_seed(config, 42);
    let a = e.intern(key(1));
    let filler = e.intern(key(2));
    let s0 = e.intern(key(10));
    let s1 = e.intern(key(11));
    let s2 = e.intern(key(12));
    e.rate(s0, 3.0, 0);
    e.rate(s1, 1.0, 0);
    e.rate(s2, 1.0, 0);
    e.on_view(s0, full(1), 0, 0);
    e.on_view(s1, full(2), 0, 0);
    e.on_view(s2, full(2), 0, 0);
    let _ = filler;
    (e, a, s0)
}

fn scenario3() -> (Engine, u32, u32) {
    scenario3_with(cfg())
}

fn bits(e: &Engine, now: u64) -> Vec<(u32, u32)> {
    e.trust_distribution(now)
        .into_iter()
        .map(|(i, v)| (i, v.to_bits()))
        .collect()
}

fn assert_close(a: f32, b: f32, what: &str) {
    assert!((a - b).abs() < 1e-5, "{what}: {a} != {b}");
}

/// 全表折扣（用于「没有任何惩罚」的断言）。
fn all_penalties_are_one(e: &Engine, n: u32) {
    for i in 0..n {
        assert_eq!(e.penalty_of(i), 1.0, "账户 {i} 不应有折扣");
    }
}

// ---------------------------------------------------------------------------
// 1. 闭式解落到引擎：折扣与查询值
// ---------------------------------------------------------------------------

#[test]
fn uniform_scenario_expected_discount_and_query() {
    let (mut e, a, s0) = scenario3();
    assert_close(e.query(a, 0), 0.42, "压前 A 的声望");

    // 只有 s0 在 S+（a=1 > ā=1/3），所以恰好压低了 1 个来源。
    assert_eq!(e.report_bad_account(a), 1, "应压低的来源数");

    assert_close(e.penalty_of(s0), 1.0 / 3.0, "s0 折扣");
    assert_eq!(e.penalty_of(e.lookup(&key(11)).unwrap()), 1.0, "S− 不动");
    assert_eq!(e.penalty_of(e.lookup(&key(12)).unwrap()), 1.0, "S− 不动");
    assert_close(e.query(a, 0), 0.7 / 3.0, "压后 A 的声望");

    // 闭式解的目标：压完后的群体项精确等于 ā = 1/3。
    // A 没有被我自己评分，所以个人项 = 0，群体项 = query / 0.7。
    let group = e.query(a, 0) / 0.7;
    assert!(
        (group - 1.0 / 3.0).abs() < 1e-5,
        "G_A 应回到 ā: {group}"
    );
}

/// 个人评分与份额必须原封不动：badlist 只动群体项权重。
#[test]
fn personal_ratings_are_untouched() {
    let (mut e, a, s0) = scenario3();
    let shares_before = e.shares();
    e.report_bad_account(a);
    assert_eq!(e.rating_of(s0), Some(3.0));
    assert_eq!(e.shares(), shares_before);
    assert_eq!(e.positive_count(), 3);
}

// ---------------------------------------------------------------------------
// 2. 没人报告时与改造前逐位一致（基线是改造前代码采样的 to_bits）
// ---------------------------------------------------------------------------

/// 改造前的 key：第 0 字节是编号、第 1 字节固定 0x5A。
fn baseline_key(i: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i;
    k[1] = 0x5A;
    k
}

fn baseline_view(pairs: &[(u8, f32)]) -> CompressedView {
    let ps: Vec<([u8; 32], f32)> = pairs.iter().map(|(i, s)| (baseline_key(*i), *s)).collect();
    CompressedView::from_pairs(ViewParams::default(), &ps)
}

#[test]
fn no_report_is_bit_identical_to_pre_change() {
    let mut e = Engine::with_seed(EngineConfig::default(), 0xBAD1_1234_5678);
    for i in 1..=7u8 {
        e.intern(baseline_key(i));
    }
    e.rate(0, 1.0, 0);
    e.rate(1, 0.7, 0);
    e.rate(2, 0.4, 0);
    e.rate(3, 0.9, 10);

    e.tick(0, 8);
    e.on_view(
        0,
        baseline_view(&[(1, 0.2), (2, 0.3), (3, 0.1), (4, 0.4)]),
        0,
        0,
    );
    e.on_view(
        1,
        baseline_view(&[(1, 0.05), (2, 0.15), (3, 0.5), (4, 0.3)]),
        0,
        0,
    );
    e.on_view(
        2,
        baseline_view(&[(1, 0.6), (2, 0.1), (3, 0.2), (4, 0.1)]),
        0,
        0,
    );
    e.tick(37, 8);
    e.on_view(
        1,
        baseline_view(&[(1, 0.1), (2, 0.1), (3, 0.7), (4, 0.1)]),
        30,
        37,
    );
    e.tick(150, 8);
    e.on_view(
        2,
        baseline_view(&[(1, 1.0), (2, 0.0), (3, 0.0), (4, 0.0)]),
        140,
        150,
    );

    // 下面这些 to_bits 是**改造前**的二进制打印出来的，硬编码在这里当回归基准。
    assert_eq!(
        bits(&e, 0),
        vec![
            (0, 0x3ea58bf3),
            (1, 0x3e45f92c),
            (2, 0x3e7258bf),
            (3, 0x3e7c9630)
        ]
    );
    assert_eq!(
        bits(&e, 37),
        vec![
            (0, 0x3eaca60b),
            (1, 0x3e37338a),
            (2, 0x3e83f7e0),
            (3, 0x3e6790a2)
        ]
    );
    assert_eq!(
        bits(&e, 150),
        vec![
            (0, 0x3ef4bbc2),
            (1, 0x3e154649),
            (2, 0x3e441a65),
            (3, 0x3e3d27ce)
        ]
    );
    assert_eq!(bits(&e, 1000), bits(&e, 150), "全部陈旧时衰减不变");
    assert_eq!(e.query(0, 150).to_bits(), 0x3ef4bbc2);
    assert_eq!(e.query(1, 150).to_bits(), 0x3e154649);
    assert_eq!(e.query(2, 150).to_bits(), 0x3e441a65);
    assert_eq!(e.query(3, 150).to_bits(), 0x3e3d27ce);
    assert_eq!(e.query(4, 150).to_bits(), 0);
    assert_eq!(e.query(5, 150).to_bits(), 0);
    assert_eq!(e.query(6, 150).to_bits(), 0);

    // 没有报告 = 折扣表为空：每个账户的折扣都是 1.0。
    all_penalties_are_one(&e, 7);
}

// ---------------------------------------------------------------------------
// 3. 所有人态度一致 → S+ 为空 → 自动退出
// ---------------------------------------------------------------------------

#[test]
fn unanimous_sources_are_not_punished() {
    let mut e = eng();
    let a = e.intern(key(1));
    let mut srcs = Vec::new();
    for i in 0..20u32 {
        let s = e.intern(key(100 + i));
        e.rate(s, 1.0, 0);
        e.on_view(s, full(1), 0, 0);
        srcs.push(s);
    }
    assert_close(e.query(a, 0), 0.7, "全圈一致高分时 A 的声望");
    assert_eq!(e.report_bad_account(a), 0, "S+ 为空应直接退出");
    for s in srcs {
        assert_eq!(e.penalty_of(s), 1.0, "态度一致时不应罚任何人");
    }
    assert_close(e.query(a, 0), 0.7, "退出后 A 的声望不变");
}

// ---------------------------------------------------------------------------
// 4. 重复报告不复合 / 多账户取 min
// ---------------------------------------------------------------------------

/// 8 个来源：s0 同时亲近 A/B，s1 只亲近 A，s2 只亲近 B，其余只亲近 filler。
fn scenario_multi_with(config: EngineConfig) -> (Engine, u32, u32, Vec<u32>) {
    let mut e = Engine::with_seed(config, 42);
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let filler = e.intern(key(3));
    let mut srcs = Vec::new();
    for i in 0..8u32 {
        srcs.push(e.intern(key(10 + i)));
    }
    e.rate(srcs[0], 4.0, 0);
    e.rate(srcs[1], 2.0, 0);
    e.rate(srcs[2], 4.0, 0);
    for s in &srcs[3..] {
        e.rate(*s, 1.0, 0);
    }
    e.on_view(srcs[0], mix(1, 0.5, 2), 0, 0);
    e.on_view(srcs[1], full(1), 0, 0);
    e.on_view(srcs[2], full(2), 0, 0);
    for s in &srcs[3..] {
        e.on_view(*s, full(3), 0, 0);
    }
    let _ = filler;
    (e, a, b, srcs)
}

fn scenario_multi() -> (Engine, u32, u32, Vec<u32>) {
    scenario_multi_with(cfg())
}

fn penalties(e: &Engine, srcs: &[u32]) -> Vec<f32> {
    srcs.iter().map(|&s| e.penalty_of(s)).collect()
}

#[test]
fn repeated_report_does_not_compound() {
    let (mut e, a, s0) = scenario3();
    assert_eq!(e.report_bad_account(a), 1);
    let first = e.penalty_of(s0);
    let first_bits = first.to_bits();
    let query_bits = e.query(a, 0).to_bits();
    assert!(first < 0.5);

    // min 语义：第二次算出同样的 f，但没有「更低」，所以不写表、返回 0。
    assert_eq!(e.report_bad_account(a), 0, "重复报告不应更狠");
    assert_eq!(e.penalty_of(s0).to_bits(), first_bits, "折扣逐位不变");
    assert_eq!(e.query(a, 0).to_bits(), query_bits, "查询逐位不变");
}

#[test]
fn multiple_reports_take_minimum() {
    let (mut ea, a, _, sa) = scenario_multi();
    // s0（0.5）与 s1（1.0）都高于 ā=0.1875，两个都在 S+。
    assert_eq!(ea.report_bad_account(a), 2);
    let da = penalties(&ea, &sa);

    let (mut eb, _, b, sb) = scenario_multi();
    eb.report_bad_account(b);
    let db = penalties(&eb, &sb);

    let (mut eab, _, _, sab) = scenario_multi();
    eab.report_bad_account(a);
    eab.report_bad_account(b);
    let dab = penalties(&eab, &sab);
    for i in 0..sa.len() {
        let want = da[i].min(db[i]);
        assert!(
            (dab[i] - want).abs() < 1e-6,
            "来源 {i}: 组合折扣 {} != min({}, {})",
            dab[i],
            da[i],
            db[i]
        );
    }
    // 至少要有一个来源同时被两个账户罚，且两个折扣不同，否则这条测试没验到 min。
    let both =
        (0..sa.len()).find(|&i| da[i] < 0.999 && db[i] < 0.999 && (da[i] - db[i]).abs() > 1e-3);
    assert!(
        both.is_some(),
        "构造应当让某个来源同时进入两个 S+ 且折扣不同"
    );
}

// ---------------------------------------------------------------------------
// 5. 边界：未知账户、无观点、强度 λ、非法配置
// ---------------------------------------------------------------------------

#[test]
fn unknown_account_does_nothing() {
    let (mut e, a, _) = scenario3();
    let q = e.query(a, 0).to_bits();
    // 没 intern 过的账户：读不到任何观点，所有 a_v=0 → ā=0 → S+ 为空。
    assert_eq!(e.report_bad_account(9999), 0);
    assert_eq!(e.penalty_of(9999), 1.0);
    assert_eq!(e.query(a, 0).to_bits(), q);
}

#[test]
fn report_is_an_event_using_current_views() {
    let mut e = eng();
    let a = e.intern(key(1));
    let s0 = e.intern(key(10));
    let s1 = e.intern(key(11));
    let s2 = e.intern(key(12));
    e.rate(s0, 3.0, 0);
    e.rate(s1, 1.0, 0);
    e.rate(s2, 1.0, 0);
    // 还没有任何观点：事件算不出读数，什么也不做，而且**不缓存**这次报告。
    assert_eq!(e.report_bad_account(a), 0, "没有观点时不应写折扣");
    assert_eq!(e.penalty_of(s0), 1.0);

    e.on_view(s0, full(1), 0, 0);
    e.on_view(s1, full(2), 0, 0);
    e.on_view(s2, full(2), 0, 0);
    // 观点到位后同一事件立刻生效：不需要「重新设置清单」。
    assert_eq!(e.report_bad_account(a), 1);
    assert_close(e.penalty_of(s0), 1.0 / 3.0, "后到的观点应参与计算");
}

#[test]
fn strength_zero_equals_no_call() {
    let mut c = cfg();
    c.penalty_strength = 0.0;
    let (mut e, a, s0) = scenario3_with(c);
    let before = bits(&e, 0);
    assert_eq!(e.report_bad_account(a), 0, "λ=0 不应写入任何折扣");
    assert_eq!(e.penalty_of(s0), 1.0);
    assert_eq!(bits(&e, 0), before, "λ=0 与从未报告逐位一致");
}

#[test]
fn strength_half_halves() {
    let mut c = cfg();
    c.penalty_strength = 0.5;
    let (mut e, a, s0) = scenario3_with(c);
    assert_eq!(e.report_bad_account(a), 1);
    assert_close(e.penalty_of(s0), 2.0 / 3.0, "λ=0.5 折半");
}

#[test]
fn illegal_config_falls_back_to_full_strength() {
    let mut c = cfg();
    c.penalty_strength = f32::NAN;
    let (mut e, a, s0) = scenario3_with(c);
    assert_eq!(e.report_bad_account(a), 1);
    assert_close(e.penalty_of(s0), 1.0 / 3.0, "非法配置回退到 λ=1");
}

// ---------------------------------------------------------------------------
// 6. 确定性 / 报告顺序无关
// ---------------------------------------------------------------------------

#[test]
fn report_is_deterministic_and_order_independent() {
    let (mut e1, a, b, s1) = scenario_multi();
    e1.report_bad_account(a);
    e1.report_bad_account(b);
    let first = penalties(&e1, &s1);
    let first_bits = bits(&e1, 0);

    // 重复报告（min 语义）：逐位相同，且不会再压低。
    let mut lowered = 0;
    lowered += e1.report_bad_account(a);
    lowered += e1.report_bad_account(b);
    assert_eq!(lowered, 0, "重复报告不应再压低任何来源");
    assert_eq!(penalties(&e1, &s1), first);
    assert_eq!(bits(&e1, 0), first_bits);

    // 逆序报告：min 可交换，结果必须完全一样。
    let (mut e2, _, _, s2) = scenario_multi();
    e2.report_bad_account(b);
    e2.report_bad_account(a);
    assert_eq!(penalties(&e2, &s2), first);
    assert_eq!(bits(&e2, 0), first_bits);

    // 中间夹一个未知账户：没有观点，不应影响已有结果。
    let (mut e3, _, _, s3) = scenario_multi();
    e3.report_bad_account(b);
    assert_eq!(e3.report_bad_account(9999), 0);
    e3.report_bad_account(a);
    assert_eq!(penalties(&e3, &s3), first);
    assert_eq!(bits(&e3, 0), first_bits);
}

// ---------------------------------------------------------------------------
// 7. 传播：export_view 里 A 的读数确实变低，且能传到下游
// ---------------------------------------------------------------------------

#[test]
fn export_view_propagates_penalty() {
    let (mut e, a, _) = scenario3();
    let akey = e.key_of(a).unwrap();
    let before = e.export_view(0);
    let before_a = before.query(&akey);

    e.report_bad_account(a);
    let after = e.export_view(0);
    let after_a = after.query(&akey);

    assert!(
        after_a < before_a - 0.01,
        "我导出的观点里 A 的读数应下降: {before_a} -> {after_a}"
    );

    // 下游节点 F 只信任我（s0），它拿到我导出的观点后，对 A 的查询也应下降。
    let mut f = eng();
    let fa = f.intern(akey);
    let _ffiller = f.intern(key(2));
    let me = f.intern(key(10));
    let _s1 = f.intern(key(11));
    let _s2 = f.intern(key(12));
    f.rate(me, 1.0, 0);
    f.on_view(me, before, 0, 0);
    let f_before = f.query(fa, 0);

    let mut f2 = eng();
    let fa2 = f2.intern(akey);
    let _ffiller2 = f2.intern(key(2));
    let me2 = f2.intern(key(10));
    let _s12 = f2.intern(key(11));
    let _s22 = f2.intern(key(12));
    f2.rate(me2, 1.0, 0);
    f2.on_view(me2, after, 0, 0);
    let f_after = f2.query(fa2, 0);

    assert!(
        f_after < f_before - 0.01,
        "惩罚必须能传播给下游: {f_before} -> {f_after}"
    );
}

// ---------------------------------------------------------------------------
// 8. 持久性：没有自动恢复；只有手动 clear_penalties 能撤销
// ---------------------------------------------------------------------------

#[test]
fn clear_penalties_is_manual_and_immediate() {
    let (mut e, a, s0) = scenario3();
    let q_before = e.query(a, 0).to_bits();
    e.report_bad_account(a);
    assert!(e.penalty_of(s0) < 0.5);

    e.clear_penalties();
    assert_eq!(e.penalty_of(s0), 1.0, "手动撤销应立刻恢复");
    assert_eq!(
        e.query(a, 0).to_bits(),
        q_before,
        "撤销后查询值逐位回到压前"
    );

    // 撤销后 tick 不会把折扣「重新算回来」。
    for _ in 0..10 {
        e.tick(0, 2);
        assert_eq!(e.penalty_of(s0), 1.0, "tick 不应恢复任何惩罚");
    }

    // 再次报告会重新写入。
    assert_eq!(e.report_bad_account(a), 1);
    assert_close(e.penalty_of(s0), 1.0 / 3.0, "再次报告应重新生效");
}

#[test]
fn penalties_persist_across_ticks() {
    let (mut e, a, s0) = scenario3();
    e.report_bad_account(a);
    let d_bits = e.penalty_of(s0).to_bits();
    let q_bits = e.query(a, 0).to_bits();
    assert!(e.penalty_of(s0) < 0.5);

    // 没有自动漂移：1 / 5 / 20 / 100 个 tick 之后，折扣逐位不变。
    let checkpoints = [1usize, 5, 20, 100];
    let mut done = 0usize;
    for target in checkpoints {
        while done < target {
            e.tick(0, 2);
            done += 1;
        }
        assert_eq!(
            e.penalty_of(s0).to_bits(),
            d_bits,
            "{target} 个 tick 后折扣不应变化"
        );
        assert_eq!(
            e.query(a, 0).to_bits(),
            q_bits,
            "{target} 个 tick 后查询不应变化"
        );
    }
}

#[test]
fn tick_does_not_create_penalties() {
    let (mut e, a, _) = scenario3();
    let before = bits(&e, 0);
    for _ in 0..100 {
        e.tick(0, 2);
    }
    // tick 既不自动重算，也不自动报告：折扣表必须仍然为空。
    all_penalties_are_one(&e, 20);
    assert_eq!(bits(&e, 0), before, "tick 不应改变任何查询结果");
    assert_eq!(e.query(a, 0).to_bits(), before[0].1);
}

// ---------------------------------------------------------------------------
// 9. 时间口径：报告用调用时刻的衰减
// ---------------------------------------------------------------------------

#[test]
fn report_uses_decay_of_each_source() {
    let (mut e, a, s0) = scenario3();
    // 只刷新 s1（a=0 的低分来源）到 t=72：s0/s2 衰减一半，s1 权重相对变大
    // → 分子变大 → f 变大（惩罚变弱）。
    let s1 = e.lookup(&key(11)).unwrap();
    e.on_view(s1, full(2), 72, 72);
    e.report_bad_account(a);
    let d = e.penalty_of(s0);
    assert!(
        d > 1.0 / 3.0 + 1e-3,
        "低分来源变新鲜后惩罚应减弱（f 变大）: {d}"
    );
    assert!(d < 1.0);
}

// ---------------------------------------------------------------------------
// 10. 默认配置
// ---------------------------------------------------------------------------

#[test]
fn default_config_has_single_penalty_knob() {
    let c = EngineConfig::default();
    assert_eq!(c.penalty_strength, 1.0, "默认应为完整惩罚");
    // 没报告时默认引擎的折扣表为空，且 query 不进入折扣分支。
    let (e, a, _) = scenario3();
    assert!((e.query(a, 0) - 0.42).abs() < 1e-6);
}
