//! badlist（受害者反馈）的运行时行为测试。
//!
//! 覆盖：闭式解落地、空清单逐位一致、均值以上统一折扣、多攻击者取 min、
//! 边界与非法输入、确定性/顺序无关、export_view 传播、EMA 恢复、变体 P 对照。

use tm_agg::AggMode;
use tm_engine::{BadlistMode, Engine, EngineConfig};
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

// ---------------------------------------------------------------------------
// 1. 闭式解落到引擎：折扣与查询值
// ---------------------------------------------------------------------------

#[test]
fn uniform_scenario_expected_discount_and_query() {
    let (mut e, a, s0) = scenario3();
    assert_close(e.query(a, 0), 0.42, "压前 A 的声望");

    e.set_badlist(&[a]);
    e.recompute_badlist();

    assert_close(e.badlist_discount(s0), 1.0 / 3.0, "s0 折扣");
    assert_close(e.query(a, 0), 0.7 / 3.0, "压后 A 的声望");
}

/// 个人评分与份额必须原封不动：badlist 只动群体项权重。
#[test]
fn personal_ratings_are_untouched() {
    let (mut e, a, s0) = scenario3();
    let shares_before = e.shares();
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert_eq!(e.rating_of(s0), Some(3.0));
    assert_eq!(e.shares(), shares_before);
    assert_eq!(e.positive_count(), 3);
}

// ---------------------------------------------------------------------------
// 2. 空 badlist 与改造前逐位一致（基线是改造前代码采样的 to_bits）
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
fn empty_badlist_is_bit_identical_to_pre_change() {
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

    assert!(e.badlist().is_empty());
    for i in 0..7u32 {
        assert_eq!(e.badlist_discount(i), 1.0);
    }
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
    e.set_badlist(&[a]);
    e.recompute_badlist();
    for s in srcs {
        assert_eq!(e.badlist_discount(s), 1.0, "态度一致时不应罚任何人");
    }
    assert_close(e.query(a, 0), 0.7, "退出后 A 的声望不变");
}

// ---------------------------------------------------------------------------
// 4. 多攻击者取 min（不连乘）
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

fn discounts(e: &Engine, srcs: &[u32]) -> Vec<f32> {
    srcs.iter().map(|&s| e.badlist_discount(s)).collect()
}

#[test]
fn multiple_attackers_take_minimum() {
    let (mut ea, a, _, sa) = scenario_multi();
    ea.set_badlist(&[a]);
    ea.recompute_badlist();

    let (mut eb, _, b, sb) = scenario_multi();
    eb.set_badlist(&[b]);
    eb.recompute_badlist();

    let (mut eab, _, _, sab) = scenario_multi();
    eab.set_badlist(&[a, b]);
    eab.recompute_badlist();

    let da = discounts(&ea, &sa);
    let db = discounts(&eb, &sb);
    let dab = discounts(&eab, &sab);
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
    // 至少要有一个来源同时被两个攻击者罚，且两个折扣不同，否则这条测试没验到 min。
    let both =
        (0..sa.len()).find(|&i| da[i] < 0.999 && db[i] < 0.999 && (da[i] - db[i]).abs() > 1e-3);
    assert!(
        both.is_some(),
        "构造应当让某个来源同时进入两个 S+ 且折扣不同"
    );
}

// ---------------------------------------------------------------------------
// 5. 边界：未知账户、强度 λ、EMA 非法值
// ---------------------------------------------------------------------------

#[test]
fn unknown_or_empty_badlist_does_nothing() {
    let (mut e, a, s0) = scenario3();
    let q = e.query(a, 0).to_bits();
    e.set_badlist(&[]);
    e.recompute_badlist();
    assert_eq!(e.query(a, 0).to_bits(), q);
    assert_eq!(e.badlist_discount(s0), 1.0);

    // 没 intern 过的账户：读不到任何观点，所有 a_v=0 → ā=0 → S+ 为空。
    e.set_badlist(&[9999]);
    e.recompute_badlist();
    assert_eq!(e.query(a, 0).to_bits(), q);
    assert_eq!(e.badlist_discount(s0), 1.0);
    assert_eq!(e.badlist_discount(9999), 1.0);
}

#[test]
fn strength_zero_disables_and_half_halves() {
    let mut c = cfg();
    c.badlist_mu_max = 0.0;
    let (mut e, a, s0) = scenario3_with(c);
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert_eq!(e.badlist_discount(s0), 1.0, "λ=0 应完全关闭");

    let mut c = cfg();
    c.badlist_mu_max = 0.5;
    let (mut e, a, s0) = scenario3_with(c);
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert_close(e.badlist_discount(s0), 2.0 / 3.0, "λ=0.5 折半");
}

#[test]
fn illegal_config_falls_back_to_no_smoothing_and_full_strength() {
    let mut c = cfg();
    c.badlist_ema = f32::NAN;
    c.badlist_mu_max = f32::NAN;
    let (mut e, a, s0) = scenario3_with(c);
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert_close(e.badlist_discount(s0), 1.0 / 3.0, "非法配置回退到 α=1、λ=1");
}

// ---------------------------------------------------------------------------
// 6. 确定性 / 顺序无关
// ---------------------------------------------------------------------------

#[test]
fn determinism_and_order_independence() {
    let (mut e1, a, b, s1) = scenario_multi();
    e1.set_badlist(&[a, b]);
    e1.recompute_badlist();
    let first = discounts(&e1, &s1);
    let first_bits = bits(&e1, 0);

    // 重复重算（α=1）：逐位相同。
    e1.recompute_badlist();
    assert_eq!(discounts(&e1, &s1), first);
    assert_eq!(bits(&e1, 0), first_bits);

    // 逆序 + 重复 + 追加：结果必须完全一样。
    let (mut e2, _, _, s2) = scenario_multi();
    e2.add_badlist(b);
    e2.add_badlist(a);
    e2.add_badlist(b);
    e2.recompute_badlist();
    assert_eq!(e2.badlist(), &[a, b][..]);
    assert_eq!(discounts(&e2, &s2), first);
    assert_eq!(bits(&e2, 0), first_bits);

    let (mut e3, _, _, s3) = scenario_multi();
    e3.set_badlist(&[b, a, b, 9999, a]);
    e3.recompute_badlist();
    assert_eq!(e3.badlist(), &[a, b, 9999][..]);
    // 多出来的 9999 没有观点，不应影响前两个攻击者的结果。
    assert_eq!(discounts(&e3, &s3), first);
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

    e.set_badlist(&[a]);
    e.recompute_badlist();
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
// 8. 恢复：α=1 立刻，α<1 逐轮
// ---------------------------------------------------------------------------

#[test]
fn recovery_alpha_one_is_immediate() {
    let (mut e, a, s0) = scenario3();
    let q_before = e.query(a, 0).to_bits();
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert!(e.badlist_discount(s0) < 0.5);

    e.clear_badlist();
    assert_eq!(e.badlist_discount(s0), 1.0, "α=1 清空后立刻恢复");
    e.recompute_badlist();
    assert_eq!(
        e.query(a, 0).to_bits(),
        q_before,
        "恢复后查询值逐位回到压前"
    );
}

#[test]
fn recovery_alpha_below_one_is_gradual() {
    let mut c = cfg();
    c.badlist_ema = 0.5;
    let (mut e, a, s0) = scenario3_with(c);

    e.set_badlist(&[a]);
    e.recompute_badlist();
    let punished = e.badlist_discount(s0);
    assert!(
        punished > 1.0 / 3.0 && punished < 1.0,
        "首次 EMA 应介于 f 与 1 之间: {punished}"
    );

    e.clear_badlist();
    let mut prev = e.badlist_discount(s0);
    assert_eq!(prev, punished, "α<1 时清空本身不应立刻重置");
    for _ in 0..20 {
        e.recompute_badlist();
        let now = e.badlist_discount(s0);
        assert!(now >= prev - 1e-9, "恢复必须单调: {prev} -> {now}");
        assert!(now < 1.0 + 1e-9, "折扣不能超过 1: {now}");
        prev = now;
    }
    assert!(prev > 0.999, "20 轮后应基本恢复: {prev}");
}

// ---------------------------------------------------------------------------
// 9. 变体 P 对照：逐条压到均值、会过冲
// ---------------------------------------------------------------------------

fn scenario_p_with(config: EngineConfig) -> (Engine, u32, Vec<u32>) {
    let mut e = Engine::with_seed(config, 42);
    let a = e.intern(key(1));
    let filler = e.intern(key(2));
    let s0 = e.intern(key(10));
    let s1 = e.intern(key(11));
    let s2 = e.intern(key(12));
    let s3 = e.intern(key(13));
    e.rate(s0, 2.0, 0);
    e.rate(s1, 1.0, 0);
    e.rate(s2, 1.0, 0);
    e.rate(s3, 1.0, 0);
    e.on_view(s0, full(1), 0, 0);
    e.on_view(s1, mix(1, 0.6, 2), 0, 0);
    e.on_view(s2, full(2), 0, 0);
    e.on_view(s3, full(2), 0, 0);
    let _ = filler;
    (e, a, vec![s0, s1, s2, s3])
}

#[test]
fn per_source_variant_penalises_each_and_overshoots() {
    let (mut eu, au, su) = scenario_p_with(cfg());
    eu.set_badlist(&[au]);
    eu.recompute_badlist();
    let du = discounts(&eu, &su);

    let mut c = cfg();
    c.badlist_mode = BadlistMode::PerSource;
    let (mut ep, ap, sp) = scenario_p_with(c);
    ep.set_badlist(&[ap]);
    ep.recompute_badlist();
    let dp = discounts(&ep, &sp);

    // U：所有 S+ 来源共享同一个折扣。
    assert_close(du[0], du[1], "U 的 s0/s1 应当同折扣");
    // P：每条各自压到 ā，a 越高压得越狠。
    assert!(
        dp[0] < dp[1] - 1e-3,
        "P 的 s0 应比 s1 罚得更重: {} vs {}",
        dp[0],
        dp[1]
    );
    assert!(dp[0] < 1.0 && dp[1] < 1.0);
    // P 会把总加权结果压到 ā 以下（过冲）；U 精确落在 ā。
    let qu = eu.query(au, 0);
    let qp = ep.query(ap, 0);
    assert!(qp < qu - 1e-4, "P 应比 U 过冲得更低: U={qu}, P={qp}");
}

// ---------------------------------------------------------------------------
// 10. tick 自动重算；时钟与衰减口径
// ---------------------------------------------------------------------------

#[test]
fn tick_recomputes_badlist_at_its_clock() {
    let (mut e, a, s0) = scenario3();
    e.set_badlist(&[a]);
    // tick 开头会自动重算，不需要手动调用。
    let _ = e.tick(0, 1);
    assert_close(e.badlist_discount(s0), 1.0 / 3.0, "tick 应自动重算");
}

#[test]
fn recompute_uses_decay_of_each_source() {
    let (mut e, a, s0) = scenario3();
    e.set_badlist(&[a]);
    e.recompute_badlist();
    assert_close(e.badlist_discount(s0), 1.0 / 3.0, "t=0 全新鲜");

    // 只刷新 s1（a=0 的低分来源）：它的权重相对变大 → 分子变大 → f 变大。
    let s1 = e.lookup(&key(11)).unwrap();
    e.on_view(s1, full(2), 72, 72);
    e.recompute_badlist();
    let d = e.badlist_discount(s0);
    assert!(d > 1.0 / 3.0 + 1e-3, "低分来源变新鲜后惩罚应减弱: {d}");
}
