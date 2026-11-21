//! 引擎侧的自适应抽样预算接口：`Engine::freshness_budget` / `Engine::recommended_k`。
//!
//! 这些用例钉的是**引擎口径**（权重 = 份额 × 衰减、来源集合 = 正分清单、
//! 衰减基准 = `last_check`），纯算术部分由 `src/budget.rs` 的单元测试覆盖。
//! 每条用例的注释写明「它在防什么回归」。

use tm_engine::agg::AggMode;
use tm_engine::budget::{self, BudgetVerdict};
use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

/// 默认口径：β = 0.3、H = 72h、Mean、衰减开。
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

fn key(n: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = n;
    k[1] = 0xA5;
    k
}

fn eng() -> Engine {
    Engine::with_seed(cfg(), 42)
}

/// 一份「只对 target 表态」的观点：单条记录时 total_mass 恰好等于该分数。
fn view_of(target: [u8; 32], score: f32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(target, score)])
}

/// 建一个「正分清单有 n 条、每条份额相同」的引擎，
/// 返回 (引擎, 编号列表, 观点里被表态的目标 key)。
///
/// 每条评分都是 1.0，因此份额 = 1/n：ESS 的手工反推就靠这个已知系数。
/// 目标 key 由调用方先 intern（编号不影响观点内容，观点是按 key 写的）。
fn eng_with_sources(n: usize) -> (Engine, Vec<u32>, [u8; 32]) {
    let mut e = eng();
    let mut idxs = Vec::with_capacity(n);
    for i in 0..n {
        let idx = e.intern(key((i + 1) as u8));
        e.rate(idx, 1.0, 0);
        idxs.push(idx);
    }
    (e, idxs, key(99))
}

// ===========================================================================
// 1. 判定边界
// ===========================================================================

/// 防回归：空清单被判成健康或有推荐值。没有正分清单时一切都无从谈起，
/// 判定必须是 `Empty`，推荐 k 必须是 0。
#[test]
fn 空清单返回_empty() {
    let e = eng();
    let b = e.freshness_budget(0, 12);
    assert_eq!(b.verdict, BudgetVerdict::Empty);
    assert_eq!(b.n_list, 0);
    assert_eq!(b.refresh_period, 0);
    assert_eq!(b.coverage, 0.0);
    assert_eq!(b.ess, 0.0);
    assert_eq!(e.recommended_k(12), 0);
}

/// 防回归：全员刚取到观点、预算也够，却被判成「衰减被稀释」。
/// 10 条清单、k = 8、T = 12h ⇒ 刷新周期 ⌈10·12/8⌉ = 15h ≤ 36h = H/2，
/// 覆盖率 1.0 ⇒ `Healthy`。
#[test]
fn 全员新鲜判健康() {
    let (mut e, idxs, target) = eng_with_sources(10);
    e.intern(target);
    for &i in &idxs {
        e.on_view(i, view_of(target, 1.0), 0, 0);
    }
    e.tick(0, 8);
    let b = e.freshness_budget(0, 12);
    assert_eq!(b.verdict, BudgetVerdict::Healthy);
    assert_eq!(b.n_list, 10);
    assert_eq!(b.k, 8);
    assert_eq!(b.refresh_period, 15);
    assert!(b.healthy);
    assert!((b.coverage - 1.0).abs() < 1e-6);
    assert_eq!(b.fresh_within_halflife, 10);
    // 所有来源同龄（都在 0 时刻取到），权重完全相等 ⇒ ESS = 10
    assert!((b.ess - 10.0).abs() < 1e-5, "ESS 应为 10，实际 {}", b.ess);
}

/// 防回归：观点整体过期却仍被报成健康。`last_check` 是衰减基准，
/// 3000h 前取到的观点在 H = 72h 下早已衰减到 0 附近，刷新节奏的问题必须先被报出来。
///
/// n = 10、k = 8、T = 30h ⇒ 刷新周期 ⌈300/8⌉ = 38h > 36h ⇒ `DecayDiluted`，
/// 推荐 k = ⌈2·10·30/72⌉ = 9（≤ 清单长度，可达）。
#[test]
fn 刻意过期报稀释并给推荐值() {
    let (mut e, idxs, target) = eng_with_sources(10);
    e.intern(target);
    // 全部观点都在 0 时刻取到，体检时刻推到 3000h 之后
    for &i in &idxs {
        e.on_view(i, view_of(target, 1.0), 0, 0);
    }
    e.tick(0, 8);
    let b = e.freshness_budget(3000, 30);
    assert!(
        matches!(b.verdict, BudgetVerdict::DecayDiluted { recommended_k } if recommended_k == 9),
        "实际判定 {:?}",
        b.verdict
    );
    assert_eq!(b.refresh_period, 38);
    assert!(!b.healthy);
    // 「推荐 k 合理」：用推荐的 k 算出来确实达标，用 k-1 确实不达标
    assert!(budget::is_healthy(budget::refresh_period(10, 9, 30), 72));
    assert!(!budget::is_healthy(
        budget::refresh_period(10, 9 - 1, 30),
        72
    ));
    // 3000h 至少 41 个半衰期 ⇒ 每个来源的衰减因子 ≈ 2^-41，ESS 仍然是 10
    // （同龄权重整体缩小不改变 ESS —— 但这里全体下溢到 DECAY_FLOOR 也会保持等权）
    assert!(b.ess >= 1.0 && b.ess <= 11.0, "ESS 越界：{}", b.ess);
    assert_eq!(b.fresh_within_halflife, 0, "3000h 已远超一个半衰期");
}

/// 防回归：覆盖率不足却给了「把 k 调大」的建议 —— 真正的问题是观点没取回来。
/// 10 条清单只取到 4 条（覆盖 0.4），即使刷新节奏健康也必须是 `LowCoverage`。
#[test]
fn 覆盖率不足报低覆盖() {
    let (mut e, idxs, target) = eng_with_sources(10);
    e.intern(target);
    for &i in &idxs[..4] {
        e.on_view(i, view_of(target, 1.0), 0, 0);
    }
    e.tick(0, 8);
    let b = e.freshness_budget(0, 12);
    assert!(
        matches!(b.verdict, BudgetVerdict::LowCoverage { coverage } if (coverage - 0.4).abs() < 1e-6),
        "实际判定 {:?}",
        b.verdict
    );
    assert!(b.healthy, "刷新节奏本身是健康的，覆盖不足是另一维");
    assert_eq!(b.refresh_period, 15);
    // 覆盖不足时 ESS 只可能小于等于实际覆盖到的来源数
    assert!(b.ess <= 4.0 + 1e-5, "ESS 越界：{}", b.ess);
}

// ===========================================================================
// 2. ESS 的手工反推
// ===========================================================================

/// 防回归：ESS 的权重口径走了别的路径（例如漏乘衰减、用了原始评分而不是份额）。
/// 两个来源、份额各 1/2、都刚取到 ⇒ 权重 [0.5, 0.5] ⇒ ESS = 2。
///
/// 换成原始评分 1.0 / 1.0 也得到 2，所以这里专门再造一组**份额不同**的：
/// 评分 1.0 与 3.0 ⇒ 份额 0.25 / 0.75 ⇒ ESS = 1²/(0.0625+0.5625) = 1.6。
#[test]
fn ess_手工构造可反推() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let target = key(99);
    e.rate(a, 1.0, 0);
    e.rate(b, 1.0, 0);
    e.on_view(a, view_of(target, 1.0), 0, 0);
    e.on_view(b, view_of(target, 0.0), 0, 0);
    e.tick(0, 2);
    let budget = e.freshness_budget(0, 12);
    assert_eq!(budget.n_list, 2);
    assert!(
        (budget.ess - 2.0).abs() < 1e-5,
        "两个等权来源的 ESS 应为 2，实际 {}",
        budget.ess
    );

    // 份额 1/4 与 3/4：ESS = 1 / (0.25² + 0.75²) = 1.6
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let target = key(99);
    e.rate(a, 0.25, 0);
    e.rate(b, 0.75, 0);
    e.on_view(a, view_of(target, 1.0), 0, 0);
    e.on_view(b, view_of(target, 1.0), 0, 0);
    e.tick(0, 2);
    let budget = e.freshness_budget(0, 12);
    assert!(
        (budget.ess - 1.6).abs() < 1e-4,
        "份额 1/4 与 3/4 的 ESS 应为 1.6，实际 {}",
        budget.ess
    );
}

/// 防回归：衰减因子没有进入 ESS 的权重。同龄来源 ESS 不变，**年龄不同**时必须变小。
///
/// 两个等份额来源，一个刚取到（w = 0.5×1 = 0.5），另一个一个半衰期前取到
/// （w = 0.5×0.5 = 0.25）⇒ ESS = (0.75)²/(0.25+0.0625) = 1.8。
#[test]
fn ess_随年龄差下降() {
    let mut e = eng();
    let fresh = e.intern(key(1));
    let stale = e.intern(key(2));
    let target = key(99);
    e.rate(fresh, 1.0, 0);
    e.rate(stale, 1.0, 0);
    // stale 的 last_check 是 -72（相对体检时刻 0 即一个半衰期前）。
    // on_view 的 now 参数就是 last_check，所以先在最老的时刻取，再取新的。
    e.on_view(stale, view_of(target, 1.0), 0, 0);
    e.on_view(fresh, view_of(target, 0.0), 0, 72);
    e.tick(72, 2);
    let budget = e.freshness_budget(72, 12);
    // stale 年龄 72 ⇒ 衰减 0.5；fresh 年龄 0 ⇒ 衰减 1.0
    // 权重 0.25 与 0.5 ⇒ ESS = 0.5625/0.3125 = 1.8
    assert!(
        (budget.ess - 1.8).abs() < 1e-4,
        "一个半衰期差的 ESS 应为 1.8，实际 {}",
        budget.ess
    );
    assert_eq!(
        budget.fresh_within_halflife, 2,
        "年龄恰好 H 也算新鲜（闭区间）"
    );
}

// ===========================================================================
// 3. 与既有接口的口径一致性
// ===========================================================================

/// 防回归：`fresh_within_halflife` 与 `freshness_stats` 用了不同的年龄边界
/// （一个用 `<` 一个用 `<=`，或者一个忘了把 H = 0 归一化）。
/// 这里让所有观点都由正分清单里的来源产生，于是两个口径的来源集合完全相同，
/// 两个计数必须**逐点相等**。
#[test]
fn 新鲜计数与_freshness_stats_一致() {
    let mut e = eng();
    let mut idxs = Vec::new();
    for i in 0..6u8 {
        let idx = e.intern(key(i + 1));
        e.rate(idx, 1.0, 0);
        idxs.push(idx);
    }
    let target = key(99);
    // 年龄 0 / 1 / 36 / 71 / 72 / 73：恰好 72（一个半衰期）必须算新鲜
    let ages = [0u64, 1, 36, 71, 72, 73];
    let now = 100u64;
    for (&idx, &age) in idxs.iter().zip(ages.iter()) {
        e.on_view(idx, view_of(target, 1.0), 0, now - age);
    }
    e.tick(now, 4);
    let stats = e.freshness_stats(now);
    let budget = e.freshness_budget(now, 12);
    assert_eq!(stats.count, 6);
    assert_eq!(budget.fresh_within_halflife, 5, "年龄 0/1/36/71/72 共 5 条");
    assert_eq!(budget.fresh_within_halflife, stats.fresh_within_half_life);
    assert_eq!(budget.coverage, 1.0);

    // 撤销其中两条评分后：它们不再算「覆盖」，但观点缓存还在——
    // freshness_stats 仍然数 6 条（它数的是缓存），budget 只数还在清单里的 4 条。
    e.rate(idxs[0], 0.0, now);
    e.rate(idxs[1], 0.0, now);
    let stats = e.freshness_stats(now);
    let budget = e.freshness_budget(now, 12);
    assert_eq!(stats.count, 6, "缓存不清：撤销评分不动观点缓存");
    assert_eq!(budget.n_list, 4);
    assert!((budget.coverage - 4.0 / 4.0).abs() < 1e-6);
    assert_eq!(
        budget.fresh_within_halflife, 3,
        "0 与 1 岁那两条已不在清单里"
    );
}

/// 防回归：撤销评分的来源仍然被算进覆盖率。`query` 的群体项分母是**正分清单**，
/// 撤销了评分的来源权重不变也不进分母；一个观点缓存若还留着它，
/// 覆盖率报 1.0 会让运维误以为「全都覆盖到了」。
#[test]
fn 撤销评分的来源不计入覆盖() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let target = key(99);
    e.rate(a, 1.0, 0);
    e.rate(b, 1.0, 0);
    e.on_view(a, view_of(target, 1.0), 0, 0);
    e.on_view(b, view_of(target, 1.0), 0, 0);
    e.tick(0, 2);
    assert!((e.freshness_budget(0, 12).coverage - 1.0).abs() < 1e-6);

    // 撤销 a 的评分：观点缓存仍然在，但它已不在群体项里
    e.rate(a, 0.0, 0);
    let budget = e.freshness_budget(0, 12);
    assert_eq!(budget.n_list, 1);
    assert_eq!(budget.coverage, 1.0, "唯一还在清单里的来源有观点");
    assert_eq!(budget.fresh_within_halflife, 1);
    assert_eq!(budget.ess, 1.0, "只剩一个来源 ⇒ ESS = 1");
    // 与 stats 对比：缓存里还有 2 条
    assert_eq!(e.stats().2, 2);

    // 再撤掉 b：清单为空，判定 Empty
    e.rate(b, 0.0, 0);
    let budget = e.freshness_budget(0, 12);
    assert_eq!(budget.n_list, 0);
    assert_eq!(budget.verdict, BudgetVerdict::Empty);
}

/// 防回归：`refresh_ratio` 与 `healthy` 用了不同判据（浮点 0.5 比较的舍入边界）。
/// 约定 healthy ⟺ ratio ≤ 0.5，必须逐点一致。
#[test]
fn 刷新比值与健康判据一致() {
    for half_life in [1u64, 24, 36, 72, 168] {
        for (n, k, t) in [
            (10usize, 1usize, 1u64),
            (10, 8, 12),
            (600, 8, 30),
            (600, 200, 12),
        ] {
            // 直接比较纯函数：引擎字段只是它的搬运
            let p = budget::refresh_period(n, k, t);
            let ratio = budget::refresh_ratio(p, half_life);
            let healthy = budget::is_healthy(p, half_life);
            assert_eq!(healthy, ratio <= 0.5, "n={n} k={k} T={t} H={half_life}");
        }
    }
    // 引擎字段与纯函数同值
    let (mut e, idxs, target) = eng_with_sources(10);
    e.intern(target);
    for &i in &idxs {
        e.on_view(i, view_of(target, 1.0), 0, 0);
    }
    e.tick(0, 8);
    let b = e.freshness_budget(0, 12);
    assert_eq!(b.refresh_period, budget::refresh_period(10, 8, 12));
    assert_eq!(b.healthy, budget::is_healthy(b.refresh_period, 72));
    assert_eq!(b.refresh_ratio, budget::refresh_ratio(b.refresh_period, 72));
}

/// 防回归：`recommended_k` 用了别的清单长度（例如 views.len() 或 account_count）。
/// 它必须等价于「以 `ratings.len()` 为清单长度调 `budget::recommended_k`」。
#[test]
fn 引擎推荐值与纯函数一致() {
    let (mut e, idxs, target) = eng_with_sources(10);
    e.intern(target);
    // 观点缓存只有 4 条：清单长度仍是 10，推荐值不能被它带偏
    for &i in &idxs[..4] {
        e.on_view(i, view_of(target, 1.0), 0, 0);
    }
    e.tick(0, 8);
    for t in [0u64, 6, 12, 30, 100] {
        assert_eq!(
            e.recommended_k(t),
            budget::recommended_k(10, t, 72),
            "T={t}"
        );
    }
    // 10 · 12 / 36 = 3.33 ⇒ 最小可行的 k = 4：`⌈10·12/4⌉ = 30 ≤ 36`，
    // 而 k = 3 时 `⌈120/3⌉ = 40 > 36`。
    assert_eq!(e.recommended_k(12), 4);
    assert!(budget::is_healthy(budget::refresh_period(10, 4, 12), 72));
    assert!(!budget::is_healthy(budget::refresh_period(10, 3, 12), 72));
}
