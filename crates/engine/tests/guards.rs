//! # 参数误用护栏（L3 缺口）
//!
//! `docs/audit/工程可用性评估.md` §2.3 列了 6 个「陷阱」，其中 3 个是**静默**的 ——
//! 参数用错了不报错，只让结果悄悄变错。本文件钉住这一轮补上的三个出口：
//!
//! 1. [Engine::last_tick_warning] —— `tick` 的 k 违反 `(n/k)·T ≤ H/2` 时能看见；
//! 2. [Rejection::ParamsMismatch] —— ViewParams 与本地不一致时不再静默丢弃；
//! 3. [RejectedBreakdown] 的「三项之和 == rejected_count()」不变量。
//!
//! **本文件不测行为正确性**（那是其它测试文件的事），只测「原本静默的失败现在可观测」。
//! 每一条都对应一次真实的误用场景，不是为覆盖率凑数。

use tm_engine::budget::{is_healthy, recommended_k, refresh_period};
use tm_engine::view::{AccountKey, CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig, RejectedBreakdown};

fn key(i: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[0] = (i & 0xFF) as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0x9A;
    k
}

fn params() -> ViewParams {
    ViewParams::new(2048, 5, 11)
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: params(),
        half_life: 72,
        ..EngineConfig::default()
    }
}

/// 建一个「清单里有 n 个人、每人 1.0」的引擎（不改任何其它状态）。
fn engine_with_list(n: u32) -> Engine {
    let mut e = Engine::with_seed(cfg(), 0x600D_0000_0000_0001);
    for i in 0..n {
        e.intern(key(i));
        e.rate(i, 1.0, 0);
    }
    e
}

/// 一份形状与本地一致的视图，内容是把 target 声明成 1.0。
fn view_of(target: u32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(key(target), 1.0)])
}

// ===========================================================================
// ① tick 预算体检
// ===========================================================================

/// 一次都没 tick 过时无从判断 ⟹ `None`（不是 `Some` 的「健康」）。
///
/// 为什么钉这条：把「还没开始抽」报成健康，会让部署自检在第一轮就误判绿灯。
/// 与 `budget::assess` 的 `Empty` 判定同口径。
#[test]
fn no_tick_yet_means_no_verdict() {
    let e = engine_with_list(10);
    assert_eq!(e.last_tick_warning(30), None);
    assert_eq!(e.last_budget_k(), 0, "没 tick 过时 last_k 仍是 0");
}

/// tick 时清单为空 ⟹ `None`（没有可抽的对象，也就没有「刷新周期」可言）。
#[test]
fn empty_list_means_no_verdict() {
    let mut e = Engine::with_seed(cfg(), 7);
    let req = e.tick(1, 8);
    assert!(req.is_empty(), "空清单抽不出任何请求");
    assert_eq!(e.last_tick_warning(30), None);
}

/// 健康参数：n=600、k=60、T=30 ⟹ 刷新周期 ⌈600·30/60⌉ = 300h ≤ H/2 = 36h？**不成立**。
///
/// 这里刻意用一组**真的健康**的参数：n=60、k=60、T=30 ⟹ 刷新周期 = 30h ≤ 36h ✅。
/// 用 `budget` 的同一个函数独立复算，确保本测试与引擎判定同源。
#[test]
fn healthy_budget_yields_no_warning() {
    let n = 60u32;
    let mut e = engine_with_list(n);
    e.tick(1, n as usize); // k = n_list ⟹ 一轮全量覆盖
    let period = 30;
    let refresh = refresh_period(n as usize, n as usize, period);
    assert!(
        is_healthy(refresh, 72),
        "前提：这组参数必须真的是健康的（refresh={refresh}）"
    );
    assert_eq!(
        e.last_tick_warning(period),
        None,
        "健康的预算不该报警（refresh={refresh}h）"
    );
}

/// 病态参数：n=600、k=8、T=30 ⟹ 刷新周期 2250h，远超 H/2 = 36h ⟹ 必须报警。
///
/// 这是 `docs/audit/A10-抽样参数k与轮次节奏.md` §6.2 的实测场景
/// （90.7% 的来源衰减因子 < 0.1、ESS 600 → 55.8）。
#[test]
fn pathological_budget_warns() {
    let n = 600u32;
    let mut e = engine_with_list(n);
    e.tick(1, 8);
    let period = 30;
    let refresh = refresh_period(n as usize, 8, period);
    assert!(
        !is_healthy(refresh, 72),
        "前提：这组参数必须真的是病态的（refresh={refresh}）"
    );
    let w = e.last_tick_warning(period);
    assert!(w.is_some(), "病态预算必须报警（refresh={refresh}h）");
    // 文案里必须说明「增大 k 或缩短轮间隔」这两条出路，并能指向 recommended_k。
    let msg = w.unwrap();
    assert!(msg.contains("增大 k"), "文案要给出第一条出路：{msg}");
    assert!(
        msg.contains("recommended_k"),
        "文案要指向能算出具体数字的接口：{msg}"
    );
}

/// 推荐值要能被调用方独立算出来（文案里没拼数字，但接口必须自洽）。
///
/// 钉的是「报不合格」与「报改到多少」的分工：`last_tick_warning` 报前者，
/// `recommended_k` 报后者。实测病态那组参数下推荐 k 必须比当前 k 大得多。
#[test]
fn recommended_k_is_the_actionable_counterpart() {
    let n = 600usize;
    let rec = recommended_k(n, 30, 72);
    assert!(rec > 8, "病态参数下推荐 k 必须远超 8，实际 {rec}");
    assert!(rec <= n, "推荐 k 不得超过清单长度，实际 {rec}");
    // 用推荐 k 重算刷新周期，必须回到健康区间。
    let refresh = refresh_period(n, rec, 30);
    assert!(
        is_healthy(refresh, 72),
        "按推荐 k={rec} 重算后必须健康（refresh={refresh}）"
    );
}

/// `probe` 等价于 `k = n_list`，天然健康 ⟹ 不该被误报。
///
/// 引擎自己不维护定时器，`probe` 由应用层按 T_probe（建议 24h）调用。
/// 它把本轮剩余的条目全部返回，所以刷新周期就是一个轮间隔。
#[test]
fn probe_does_not_trip_the_warning() {
    let n = 100u32;
    let mut e = engine_with_list(n);
    // 先让 tick 记下一个很小的 k（病态）。
    e.tick(1, 1);
    assert!(e.last_tick_warning(30).is_some(), "前提：k=1 必须病态");
    // probe 不改 last_k ⟹ 警告仍然针对「上一轮 tick 的 k」，这是刻意的口径。
    let before = e.last_budget_k();
    let _ = e.probe(2);
    assert_eq!(
        e.last_budget_k(),
        before,
        "probe 不该改写 last_k：警告回答的是「上一轮 tick 的 k 够不够」"
    );
    assert!(
        e.last_tick_warning(30).is_some(),
        "probe 不改变上一次 tick 的判定"
    );
}

// ===========================================================================
// ② 参数不匹配的可观测性
// ===========================================================================

/// ViewParams 不一致的视图：不进缓存、不计命中、**不计连败**，但必须计数。
///
/// 「不计连败」是刻意的：这不是网络故障，给一个网络正常的邻居记连败会让
/// [Engine::tick] 的退避改变后续抽样序列 —— 那是把配置错误伪装成故障。
/// 本文件钉住这条语义，防止将来有人图省事把它并进 `reject_view` 的失败分支。
#[test]
fn params_mismatch_is_observable_but_does_not_touch_backoff() {
    let mut e = Engine::with_seed(cfg(), 3);
    let owner = e.intern(key(1000));
    e.rate(owner, 1.0, 0);
    // 形状不符：桶数不同（本地 2048，这份 64）。
    let wrong = CompressedView::from_pairs(ViewParams::new(64, 5, 11), &[(key(7), 1.0)]);
    e.on_view(owner, wrong, 0, 0);
    assert_eq!(e.stats().1, 0, "参数不符不得计命中");
    assert_eq!(e.stats().2, 0, "参数不符不得进缓存");
    assert_eq!(e.rejected_count(), 1, "必须可观测");
    let b = e.rejected_breakdown();
    assert_eq!(b.params_mismatch, 1);
    assert_eq!(b.capacity, 0, "不得被误记成容量丢弃");
    assert_eq!(b.nonfinite, 0, "不得被误记成非有限丢弃");
    assert_eq!(
        e.fail_streak(owner),
        0,
        "参数不符不计连败：退避序列必须与改动前逐位一致"
    );
}

/// 同一份参数但 **seed 不同** 也必须被拒（长度相同、dump 出来却读的是别的东西）。
///
/// 这是比 buckets 不同更隐蔽的一种错配：字节长度完全一样，光看长度发现不了。
#[test]
fn same_shape_different_seed_is_still_a_mismatch() {
    let mut e = Engine::with_seed(cfg(), 5);
    let owner = e.intern(key(1001));
    e.rate(owner, 1.0, 0);
    let wrong_seed = CompressedView::from_pairs(ViewParams::new(2048, 5, 99), &[(key(7), 1.0)]);
    assert_eq!(
        wrong_seed.bytes(),
        view_of(7).bytes(),
        "前提：两者字节长度必须相同（否则测的就不是 seed 这一维）"
    );
    e.on_view(owner, wrong_seed, 0, 0);
    let b = e.rejected_breakdown();
    assert_eq!(b.params_mismatch, 1, "seed 不同必须被识别为参数不匹配");
    assert_eq!(e.stats().2, 0, "不得进缓存");
}

// ===========================================================================
// ③ 三项之和的不变量
// ===========================================================================

/// `RejectedBreakdown` 三项之和恒等于 `rejected_count()`，三类各喂一些。
///
/// 这条不变量的必要性：上层靠 `rejected_count()` 做总量告警、靠 `rejected_breakdown()`
/// 做分类决策。两者一旦对不上，运维看到的数字就是自相矛盾的。
#[test]
fn breakdown_sums_to_rejected_count_for_all_three_kinds() {
    let mut e = Engine::with_seed(cfg(), 9);
    e.intern(key(5));
    // 容量：max_views = 3，喂 6 个新 owner ⟹ 3 进缓存、3 被拒。
    let mut e_limited = Engine::with_seed(
        EngineConfig {
            max_views: 3,
            ..cfg()
        },
        9,
    );
    e_limited.intern(key(5));
    for i in 0..6u32 {
        let o = e_limited.intern(key(20_000 + i));
        e_limited.rate(o, 1.0, 0);
        e_limited.on_view(o, view_of(5), 0, 0);
    }
    // 参数不匹配：形状不符的两条（新 owner，不会撞容量）。
    let wrong = ViewParams::new(64, 5, 11);
    for i in 0..2u32 {
        let o = e_limited.intern(key(40_000 + i));
        e_limited.on_view(o, CompressedView::from_pairs(wrong, &[(key(5), 1.0)]), 0, 0);
    }
    let b: RejectedBreakdown = e_limited.rejected_breakdown();
    assert_eq!(b.capacity, 3, "前 3 个进缓存、后 3 个被容量拒");
    assert_eq!(b.params_mismatch, 2);
    assert_eq!(
        b.capacity + b.nonfinite + b.params_mismatch,
        e_limited.rejected_count(),
        "三项之和必须与总数自洽"
    );
    assert_eq!(e_limited.stats().2, 3, "缓存里只有 3 条");
    // 顺带钉住 e（未被拒过的引擎）的三项全 0。
    let clean = e.rejected_breakdown();
    assert_eq!(clean.capacity + clean.nonfinite + clean.params_mismatch, 0);
    assert_eq!(e.rejected_count(), 0);
}

// ===========================================================================
// ④ 恢复之后的计数还原
// ===========================================================================

/// `Engine::restore` 之后 `params_mismatch` 必须能由已有三个计数**精确还原**，
/// 否则「三项之和 == rejected_count()」这条不变量在恢复后就断了。
///
/// 这也是 `SNAPSHOT_VERSION` 不用从 2 抬到 3 的理由（见 snapshot.rs 的注释）：
/// 被拒原因只有三类且互斥 ⟹ 总数减两类就是第三类。本测试钉住这个前提。
#[test]
fn restored_engine_keeps_breakdown_invariant() {
    let mut e = Engine::with_seed(cfg(), 13);
    e.intern(key(5));
    // 制造 2 次参数不匹配 + 1 次容量。
    let limited = EngineConfig {
        max_views: 2,
        ..cfg()
    };
    let mut e = Engine::with_seed(limited.clone(), 13);
    e.intern(key(5));
    for i in 0..3u32 {
        let o = e.intern(key(60_000 + i));
        e.rate(o, 1.0, 0);
        e.on_view(o, view_of(5), 0, 0); // 前 2 条进缓存，第 3 条容量拒
    }
    let wrong = ViewParams::new(64, 5, 11);
    let o = e.intern(key(70_000));
    e.on_view(o, CompressedView::from_pairs(wrong, &[(key(5), 1.0)]), 0, 0);
    let before = e.rejected_breakdown();
    assert_eq!(before.capacity, 1);
    assert_eq!(before.params_mismatch, 1);
    let total = e.rejected_count();

    let snap = e.snapshot(0);
    let mut back = Engine::restore(limited, snap).expect("自产快照必须能恢复");
    let after = back.rejected_breakdown();
    assert_eq!(
        after.capacity + after.nonfinite + after.params_mismatch,
        back.rejected_count(),
        "恢复后三项之和仍必须等于总数"
    );
    assert_eq!(after.capacity, before.capacity, "容量计数必须原样恢复");
    assert_eq!(
        after.params_mismatch, before.params_mismatch,
        "参数不匹配计数必须由「总数 − 容量 − 非有限」精确还原"
    );
    assert_eq!(back.rejected_count(), total, "总数必须原样恢复");
    // 恢复后引擎仍能正常工作。
    assert!(back.query(5, 10).is_finite());
}
