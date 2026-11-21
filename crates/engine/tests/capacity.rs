//! 安全加固 P2：观点缓存的**容量上限**与**非有限读数**防护（crates/engine）。
//!
//! 本文件钉死三件事，每条都对应一个「改前 → 改后」的最小复现：
//!
//! | 节 | 缺陷 | 改前 | 改后 |
//! | --- | --- | --- | --- |
//! | ① | views 无条目上限（审计 A13-spec U-01：规格 §0.3 承诺、代码没做） | 喂 10000 个不同 owner ⇒ 10000 条缓存 | max_views 顶住，超出整条丢弃 |
//! | ② | on_view 不校验 total_mass 有限性（审计 A8-attack D5） | NaN 求和的视图照样进缓存 | 整条丢弃，缓存与读数不受污染 |
//! | ③ | 兼容性 | —— | max_views = 0（默认）时行为与引入本字段之前**逐位一致** |
//!
//! # 丢弃语义（两条路径完全相同）
//!
//! 规格 §0.3 原文：「超出即丢弃并**视同失败**」。因此被拒的观点：
//!
//! * **不**进缓存、**不**刷新 last_check、**不**计 stats() 的命中；
//! * 只给 owner 累加**连败计数**（等价于调用 on_view_failed）——
//!   于是下一次 tick 会按 1/2^n 退避它，链路自然收敛；
//! * 只累加 rejected_count()（细分见 rejected_breakdown()）。
//!
//! 为什么 on_view 的返回类型保持 ()：返回值形状一改，所有既有调用点都要动，
//! 而调用方本来也分不清「成功 / 失败」（失败走的是另一个方法 on_view_failed）。
//! 可观测性通过 rejected_count() 单独提供，stats() 的四元组一个字节都不变。

use tm_engine::view::{AccountKey, CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig, Rejection};

fn key(i: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[0..4].copy_from_slice(&i.to_le_bytes());
    k
}

/// 小参数视图（512×5 = 10 KiB）：on_view 要扫全表 + 1000 次噪声采样，
/// 桶数越小测试越快，但碰撞噪声与上限判定与默认参数**同构**。
fn params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

fn cfg(max_views: usize) -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: params(),
        max_views,
        ..Default::default()
    }
}

fn eng(max_views: usize) -> Engine {
    Engine::with_seed(cfg(max_views), 42)
}

/// 一份「只对 target 表态」的观点（合规输入：单条、分数 ∈ (0,1]）。
fn view_of(target: u32, score: f32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(key(target), score)])
}

/// 全部目标的查询结果，按 to_bits() 取快照：逐位比较比 == 更严格，
/// 能把 -0.0 与 0.0、以及任何一个 ULP 的漂移都揪出来。
fn snapshot(e: &Engine, targets: std::ops::Range<u32>, now: u64) -> Vec<(u32, u32)> {
    targets.map(|t| (t, e.query(t, now).to_bits())).collect()
}

/// 造一份**含 NaN 读数**的视图，只用公开 API（decode 会拒绝 NaN 负载，
/// 所以走 merge 让浮点在进程内溢出）。
///
/// 三步（hashers 个桶会被写脏，其余桶保持 0）：
///
/// 1. add(key, f32::MAX)：该 key 的 5 个桶变成 ±MAX；
/// 2. merge(zero, 2.0, 2.0)：MAX×2 溢出成 ±∞，其余桶 0×2+0×2 = 0；
/// 3. merge(zero, 0.0, 0.0)：0×∞ 得到 NaN，其余桶仍是 0。
///
/// 于是**同一个 key** 的 query 取出 5 个 NaN 并返回 NaN（中位数也是 NaN），
/// 而 on_view 的质量求和就是 NaN。注意第 2、3 步的 merge 权重都必须是
/// 有限值——merge 会对非有限权重 panic，这里给的是 2.0 / 0.0。
fn nan_view_with_key(k: AccountKey) -> CompressedView {
    let zero = CompressedView::new(params());
    let mut v = CompressedView::new(params());
    v.add(&k, f32::MAX);
    v.merge(&zero, 2.0, 2.0); // MAX*2 -> ±inf
    v.merge(&zero, 0.0, 0.0); // 0*inf -> NaN
    v
}

/// 钉住 f32 的 NaN 语义——它是缺陷 ② 真正的根因所在。
///
/// `NaN.max(1e-6)` 精确等于 `1e-6`（`f32::max` 返回**非 NaN**的那个操作数），
/// 所以「求和之后查 `total_mass.is_finite()`」这条路**永远查不出 NaN**，
/// 只会得到 1e-6 这个「放大器」档位（每条读数被抬到 1.0，审计 A6 D4）。
/// 也正因为如此，`on_view` 必须在求和**过程中**逐个校验 `query` 的原始返回值。
///
/// 这条断言不连着引擎：它钉的是 Rust 语言语义。哪天有人「优化」成
/// 求和后再查，这条测试仍然会绿——但那时 `nan_total_mass_is_rejected_whole`
/// 会立刻变红，因为脏视图会被当成 1e-6 收进缓存。
#[test]
fn nan_max_semantics_make_post_summation_check_useless() {
    let nan = f32::NAN;
    assert_eq!(nan.max(1e-6), 1e-6, "NaN.max(1e-6) 等于 1e-6，不是 NaN");
    assert!(nan.max(1e-6).is_finite(), "所以求和之后再查有限性没有用");
    assert!(nan.clamp(0.0, 1.0).is_nan(), "clamp 不会把 NaN 变成有限值");
    assert_eq!(
        f32::INFINITY.clamp(0.0, 1.0),
        1.0,
        "+∞ 会被 clamp 静默收成 1.0"
    );
    assert_eq!(
        f32::NEG_INFINITY.clamp(0.0, 1.0),
        0.0,
        "-∞ 会被 clamp 静默收成 0.0"
    );
}

/// 断言某份视图确实会算出非有限读数（前置条件自检）。
///
/// 这份构造如果哪天失效（例如 query 改成过滤 NaN），下面所有「整条丢弃」
/// 的断言都会变成空转，所以先把「视图确实是脏的」这件事单独钉住。
#[test]
fn nan_view_really_returns_nan() {
    let v = nan_view_with_key(key(9));
    assert!(
        !v.query(&key(9)).is_finite(),
        "构造出的视图必须对 key(9) 返回非有限值，否则后面的丢弃测试是空转"
    );
    assert_eq!(v.params(), params());
}

// ===========================================================================
// ① 容量上限：max_views
// ===========================================================================

/// 默认值必须是 0（= 无上限）。这条放在最前面：默认值一变，
/// 所有既有实验的口径就跟着变——那不是「安全加固」，是改行为。
#[test]
fn default_max_views_is_zero_meaning_unlimited() {
    assert_eq!(
        EngineConfig::default().max_views,
        0,
        "默认必须是无上限；改成任何正数都会悄悄改变既有实验/测试的口径"
    );
}

/// max_views = 0 时**一条都不拒**：喂 300 个不同 owner 全部进缓存。
///
/// 防的回归：把 0 误当成「一条都不许收」（0 >= 0 恒真），
/// 或者把上限判断写成 views.len() < max_views 之外的边界错误。
#[test]
fn max_views_zero_caches_everything() {
    let mut e = eng(0);
    const N: u32 = 300;
    let target = e.intern(key(9));
    for i in 0..N {
        e.intern(key(10_000 + i));
    }
    for i in 0..N {
        let owner = e.lookup(&key(10_000 + i)).expect("刚 intern 过");
        e.rate(owner, 1.0, 0);
        e.on_view(owner, view_of(9, 1.0), 0, i as u64);
    }
    assert_eq!(
        e.stats().2,
        N as usize,
        "0 必须表示无上限：{N} 条全部进缓存"
    );
    assert_eq!(e.rejected_count(), 0, "无上限时不得有任何丢弃");
    assert_eq!(e.rejected_breakdown(), (0, 0));
    assert!(e.query(target, 0) > 0.0, "引擎仍然正常工作");
}

/// max_views = 0（显式）与 EngineConfig::default()（隐式）**逐位一致**。
///
/// 场景里既有观测目标、也有多个来源，覆盖 query 的个人项 / 群体项 /
/// 噪声底 / 覆盖上界（mass_covers）四条路径。
#[test]
fn max_views_zero_is_bitwise_identical_to_default_config() {
    // 两份配置除「显式写 0」之外逐字段相同（都用默认形状参数）：
    // 这条钉的是「新增字段没有引入第二套语义」，不是「两套参数能互操作」。
    let a_cfg = EngineConfig {
        max_views: 0,
        ..EngineConfig::default()
    };
    let b_cfg = EngineConfig::default();
    assert_eq!(a_cfg.max_views, b_cfg.max_views);
    let mut a = Engine::with_seed(a_cfg, 42);
    let mut b = Engine::with_seed(b_cfg, 42);

    for i in 0..40u32 {
        let sa = a.intern(key(i));
        let sb = b.intern(key(i));
        assert_eq!(sa, sb, "同一 seed 下编号分配必须一致");
        a.rate(sa, 1.0 + (i % 3) as f32, 0);
        b.rate(sb, 1.0 + (i % 3) as f32, 0);
    }
    let dp = ViewParams::default();
    for i in 0..40u32 {
        let owner = a.lookup(&key(i)).unwrap();
        let v =
            CompressedView::from_pairs(dp, &[(key(i + 100), 0.5), (key(3), 1.0), (key(7), 0.25)]);
        a.on_view(owner, v.clone(), i as u64, 100 + i as u64);
        b.on_view(owner, v, i as u64, 100 + i as u64);
    }

    assert_eq!(a.stats(), b.stats(), "统计必须逐位一致");
    assert_eq!(a.rejected_count(), 0);
    assert_eq!(b.rejected_count(), 0);
    assert_eq!(
        snapshot(&a, 0..140, 200),
        snapshot(&b, 0..140, 200),
        "max_views=0 与默认配置的输出必须逐位一致"
    );
}

/// 上限生效：只收前 max_views 个**不同** owner，其余整条丢弃。
///
/// 防的回归：上限判断写成「>= 就丢」却漏掉 contains_key 分支，
/// 或者丢弃路径仍然写进缓存（views 超出上限）。
#[test]
fn capacity_limit_keeps_at_most_max_views_entries() {
    const CAP: usize = 5;
    const N: u32 = 20;
    let mut e = eng(CAP);
    let target = e.intern(key(9));
    for i in 0..N {
        e.intern(key(10_000 + i));
    }
    for i in 0..N {
        let owner = e.lookup(&key(10_000 + i)).unwrap();
        e.on_view(owner, view_of(9, 0.5), 0, i as u64);
    }

    assert_eq!(e.stats().2, CAP, "缓存条数必须被上限钉住");
    assert_eq!(
        e.rejected_count(),
        (N as usize - CAP) as u64,
        "超出上限的每一个新 owner 都应当被丢弃"
    );
    assert_eq!(
        e.rejected_breakdown(),
        ((N as usize - CAP) as u64, 0),
        "容量丢弃不得被记成非有限丢弃"
    );
    // 前 CAP 个 owner 收到了观点，后面的一个都没有。
    for i in 0..N {
        let owner = e.lookup(&key(10_000 + i)).unwrap();
        let expected = i as usize >= CAP;
        assert_eq!(
            e.view_age(owner, 100).is_none(),
            expected,
            "owner #{i} 的缓存状态不对"
        );
    }
    // 引擎在丢弃之后仍然可用。
    assert!(e.query(target, 0).is_finite());
}

/// 已缓存的 owner **永远**能更新，哪怕缓存已满。
///
/// 更新是原地覆盖，内存不增长；丢掉它只会阻断正常的观点刷新，
/// 却一分资源都省不下来。这条同时钉住「刷新会重置 last_check 与连败」。
#[test]
fn existing_owner_is_always_accepted_even_when_full() {
    const CAP: usize = 3;
    let mut e = eng(CAP);
    let mut owners = Vec::new();
    for i in 0..CAP as u32 {
        let idx = e.intern(key(10_000 + i));
        e.rate(idx, 1.0, 0);
        e.on_view(idx, view_of(9, 0.5), 0, 0);
        owners.push(idx);
    }
    assert_eq!(e.stats().2, CAP);
    // 先让它失败一次，再看成功刷新是否把连败清零。
    e.on_view_failed(owners[0], 10);
    assert_eq!(e.fail_streak(owners[0]), 1);

    let before_hits = e.stats().1;
    for round in 1..=50u64 {
        let now = round * 10;
        e.on_view(owners[0], view_of(9, 0.25), round, now);
        assert_eq!(
            e.view_age(owners[0], now),
            Some(0),
            "第 {round} 次更新必须刷新 last_check（缓存已满也不例外）"
        );
    }
    assert_eq!(e.rejected_count(), 0, "更新既有条目不得触发容量丢弃");
    assert_eq!(e.stats().2, CAP, "更新不增长缓存");
    assert_eq!(e.stats().1, before_hits + 50, "50 次更新都算命中");
    assert_eq!(e.fail_streak(owners[0]), 0, "成功刷新必须清零连败");
}

/// 被容量拒掉的新 owner 按**失败**处理：连败 +1、last_check 不动。
///
/// 这是规格 §0.3「视同失败」的全部含义：只影响退避，不影响评分。
/// 防的回归：丢弃路径只 return 而不走失败语义，于是被拒的 owner 永远
/// 拿不到退避、还会被继续按 1 的权重反复抽。
#[test]
fn capacity_rejection_counts_as_a_failure_for_backoff() {
    const CAP: usize = 2;
    let mut e = eng(CAP);
    let owners: Vec<u32> = (0..5).map(|i| e.intern(key(10_000 + i))).collect();
    for &o in &owners {
        e.rate(o, 1.0, 0);
    }
    for &o in &owners[..CAP] {
        e.on_view(o, view_of(9, 0.5), 0, 0);
    }
    assert_eq!(e.stats().2, CAP);

    let victim = owners[CAP]; // 第一个被容量拒掉的新 owner
    assert_eq!(e.fail_streak(victim), 0);
    e.on_view(victim, view_of(9, 0.5), 0, 100);
    assert_eq!(
        e.fail_streak(victim),
        1,
        "被拒必须累计连败（= on_view_failed 语义）"
    );
    assert_eq!(
        e.view_age(victim, 100),
        None,
        "被拒的观点不得进缓存（否则退避与内存都不成立）"
    );
    // 再来一次：连败继续累加，不是只加一次。
    e.on_view(victim, view_of(9, 0.5), 0, 200);
    assert_eq!(e.fail_streak(victim), 2);
    assert_eq!(e.rejected_count(), 2);

    // 腾出空间后，同一个 owner 的正常观点可以被接受，连败随之清零。
    e.prune_stale(1000, 0);
    assert_eq!(e.stats().2, 0, "prune_stale 清空缓存");
    e.on_view(victim, view_of(9, 0.5), 0, 1000);
    assert_eq!(e.stats().2, 1, "空位出现后必须能收下");
    assert_eq!(e.fail_streak(victim), 0, "成功刷新清零连败");
    assert_eq!(
        e.rejected_count(),
        2,
        "丢弃计数是**累计事件**，prune_stale 不得让它回退"
    );
}

/// 容量丢弃不得动缓存内容：既有视图的 last_check 与 query 输出逐位不变。
///
/// 防的回归：为了「腾地方」而顺手踢掉别人的观点——
/// 那会把「丢弃新来的」偷换成「淘汰旧的」，是完全不同的策略。
#[test]
fn capacity_rejection_does_not_disturb_cached_views() {
    const CAP: usize = 3;
    let mut e = eng(CAP);
    let target = e.intern(key(9));
    let mut owners = Vec::new();
    for i in 0..CAP as u32 {
        let idx = e.intern(key(10_000 + i));
        e.rate(idx, 1.0, 0);
        e.on_view(idx, view_of(9, 0.5), 0, 0);
        owners.push(idx);
    }
    let before = snapshot(&e, 0..12, 500);
    let ages: Vec<_> = owners.iter().map(|&o| e.view_age(o, 500)).collect();

    // 这 50 个来源**不评分**：评分会改变群体项的分母（那是另一条路径），
    // 这里只想验证「被丢弃的观点一点缓存都没碰」。
    for i in 0..50u32 {
        let idx = e.intern(key(20_000 + i));
        e.on_view(
            idx,
            CompressedView::from_pairs(params(), &[(key(9), 0.9)]),
            0,
            500,
        );
    }
    assert_eq!(e.rejected_count(), 50);

    assert_eq!(snapshot(&e, 0..12, 500), before, "既有读数必须逐位不变");
    let after: Vec<_> = owners.iter().map(|&o| e.view_age(o, 500)).collect();
    assert_eq!(after, ages, "既有条目的 last_check 不得被丢弃路径改动");
    assert!(e.query(target, 500).is_finite());
}

/// view.params() 不匹配的视图**不计入** rejected_count()。
///
/// 这是刻意的语义分层：形状不符 = 协议层「无法解释的字节」（静默丢弃）；
/// 容量 / 非有限 = 「可解释但被拒」（计数 + 失败语义）。
/// 两种都不改缓存，但上层要看的是后者（它才是安全信号）。
#[test]
fn params_mismatch_is_not_counted_as_capacity_rejection() {
    let mut e = eng(1);
    let owner = e.intern(key(10_000));
    e.rate(owner, 1.0, 0);
    // 形状不符（桶数不同）
    let wrong = CompressedView::from_pairs(
        ViewParams {
            buckets: 64,
            hashers: 5,
            seed: 7,
        },
        &[(key(9), 1.0)],
    );
    e.on_view(owner, wrong, 0, 0);
    assert_eq!(e.stats().1, 0, "形状不符不计命中");
    assert_eq!(e.stats().2, 0, "形状不符不进缓存");
    assert_eq!(e.rejected_count(), 0, "形状不符不计入丢弃计数");
    assert_eq!(e.fail_streak(owner), 0, "形状不符不计连败");
}

// ===========================================================================
// ② 非有限读数（total_mass 为 NaN / ±∞）
// ===========================================================================

/// NaN 视图**整条丢弃**：不进缓存、不计命中、引擎状态一点没被污染。
///
/// 改前：total_mass 变成 NaN 之后照样插入 views，
/// 于是 query 拿 q / NaN 去算读数，NaN 一路往聚合层走。
/// 改后：与容量丢弃走同一条 reject_view，缓存里根本没有它。
#[test]
fn nan_total_mass_is_rejected_whole() {
    let mut e = eng(0);
    let target = e.intern(key(9));
    let owner = e.intern(key(10_000));
    e.rate(owner, 1.0, 0);
    // 先放一份正常观点，得到可以逐位比对的基准。
    e.on_view(owner, view_of(9, 1.0), 0, 0);
    let before = snapshot(&e, 0..12, 0);
    let before_stats = e.stats();

    // 再用脏视图刷新（owner 已在缓存里，走的是更新路径——也必须拒）。
    e.on_view(owner, nan_view_with_key(key(9)), 0, 10);

    assert_eq!(e.stats().1, before_stats.1, "被拒的观点不计命中");
    assert_eq!(
        e.rejected_breakdown(),
        (0, 1),
        "非有限丢弃必须计在 nonfinite 那一栏"
    );
    assert_eq!(e.rejected_count(), 1);
    assert_eq!(
        e.view_age(owner, 10),
        Some(10),
        "last_check 必须停在旧观点上（= 没有刷新），不能被脏视图推前"
    );
    assert_eq!(
        snapshot(&e, 0..12, 0),
        before,
        "既有读数必须逐位不变：脏视图不得污染引擎"
    );
    assert!(e.query(target, 0).is_finite());
}

/// 整张视图全 NaN 的新 owner 不会被缓存。
#[test]
fn nan_view_from_new_owner_is_not_cached() {
    let mut e = eng(0);
    e.intern(key(9));
    let owner = e.intern(key(10_000));
    e.on_view(owner, nan_view_with_key(key(9)), 0, 5);
    assert_eq!(e.stats().2, 0, "脏视图不得进缓存");
    assert_eq!(e.stats().1, 0);
    assert_eq!(e.view_age(owner, 5), None);
    assert_eq!(e.rejected_breakdown(), (0, 1));
}

/// 非有限读数按**失败**处理：连败 +1，退避随之生效。
#[test]
fn nonfinite_rejection_counts_as_a_failure_for_backoff() {
    let mut e = eng(0);
    let owner = e.intern(key(10_000));
    e.rate(owner, 1.0, 0);
    e.intern(key(9));
    e.on_view(owner, nan_view_with_key(key(9)), 0, 0);
    assert_eq!(e.fail_streak(owner), 1, "脏视图必须计连败（视同失败）");
    e.on_view(owner, nan_view_with_key(key(9)), 0, 1);
    assert_eq!(e.fail_streak(owner), 2, "连败必须继续累加");
    // 之后一份正常观点应当被接受并清零连败。
    e.on_view(owner, view_of(9, 1.0), 0, 2);
    assert_eq!(e.fail_streak(owner), 0, "正常观点必须清零连败");
    assert_eq!(e.stats().2, 1);
    assert_eq!(e.stats().1, 1, "只有正常观点算命中");
}

/// 丢弃脏视图之后，同一份「干净字节」必须给出与从未见过脏视图时**逐位相同**的结果。
///
/// 这是「污染」的最严格定义：脏视图只能产生一个计数，不能留下任何别的痕迹
/// （不能改时钟基准、不能改缓存、不能改退避以外的状态）。
#[test]
fn dropped_nonfinite_view_leaves_no_trace_but_counts() {
    let mut clean = eng(0);
    let mut dirty = eng(0);
    let mut owners = Vec::new();
    let target = clean.intern(key(9));
    let _ = dirty.intern(key(9));
    for i in 0..4u32 {
        let a = clean.intern(key(10_000 + i));
        let b = dirty.intern(key(10_000 + i));
        clean.rate(a, 1.0, 0);
        dirty.rate(b, 1.0, 0);
        owners.push((a, b));
    }
    // dirty 引擎先收一堆脏视图（既有 owner 更新路径 + 新 owner 路径都覆盖）。
    for (_, b) in &owners {
        dirty.on_view(*b, nan_view_with_key(key(9)), 0, 0);
    }
    let newcomer = dirty.intern(key(20_000));
    dirty.on_view(newcomer, nan_view_with_key(key(9)), 0, 0);
    assert_eq!(dirty.rejected_count(), 5);
    assert_eq!(dirty.stats().2, 0, "一台引擎里一条脏视图都没留下");

    // 两台引擎现在各收同一批干净观点。
    for (i, (a, b)) in owners.iter().enumerate() {
        let v = CompressedView::from_pairs(params(), &[(key((i + 1) as u32), 1.0), (key(9), 0.75)]);
        clean.on_view(*a, v.clone(), 0, 100);
        dirty.on_view(*b, v, 0, 100);
    }
    assert_eq!(clean.stats(), dirty.stats(), "缓存 / 命中必须一致");
    assert_eq!(
        snapshot(&clean, 0..12, 100),
        snapshot(&dirty, 0..12, 100),
        "脏视图除了一个计数之外不得留下任何痕迹"
    );
    assert!(clean.query(target, 100).is_finite());
}

/// 合规的**全零视图**不能被误杀：它是「我谁都不表态」，不是脏数据。
///
/// 全零视图的 total_mass 是 0，被 max(1e-6) 兜住后仍是有限值，
/// 必须正常进缓存（噪声底通常也是 0，读数照样是 0）。
/// 防的回归：把「非有限」写成「<= 0」或「不是正数」。
#[test]
fn all_zero_view_is_not_mistaken_for_nonfinite() {
    let mut e = eng(0);
    let owner = e.intern(key(10_000));
    e.rate(owner, 1.0, 0);
    e.intern(key(9));
    e.on_view(owner, CompressedView::new(params()), 0, 0);
    assert_eq!(e.stats().2, 1, "全零视图是合规输入，必须进缓存");
    assert_eq!(e.rejected_count(), 0);
    assert_eq!(e.view_age(owner, 0), Some(0));
}

/// rejected_breakdown() 的两项之和恒等于 rejected_count()（两类混合喂入）。
#[test]
fn rejected_breakdown_sums_to_total() {
    let mut e = eng(4);
    e.intern(key(9));
    // 先制造 4 次容量丢弃（缓存写满后再来 4 个新 owner）。
    for i in 0..8u32 {
        let o = e.intern(key(10_000 + i));
        e.rate(o, 1.0, 0);
        e.on_view(o, view_of(9, 0.5), 0, 0);
    }
    // 再制造 3 次非有限丢弃（对已缓存的 owner 更新，不会撞容量）。
    for i in 0..3u32 {
        let o = e.lookup(&key(10_000 + i)).unwrap();
        e.on_view(o, nan_view_with_key(key(9)), 0, 10);
    }
    let (cap, nonfinite) = e.rejected_breakdown();
    assert_eq!(cap, 4, "前 4 个进缓存、后 4 个被容量拒");
    assert_eq!(nonfinite, 3);
    assert_eq!(
        cap + nonfinite,
        e.rejected_count(),
        "细分计数必须与总数自洽"
    );
    assert_eq!(e.stats().2, 4);
}

// ===========================================================================
// ③ 丢弃原因的枚举
// ===========================================================================

/// Rejection 的两种原因都要能被外部匹配（上层据此决定告警还是扩容）。
#[test]
fn rejection_kind_is_public_and_matchable() {
    let reasons = [Rejection::Capacity, Rejection::NonFinite];
    let mut seen = Vec::new();
    for r in reasons {
        match r {
            Rejection::Capacity => seen.push("capacity"),
            Rejection::NonFinite => seen.push("nonfinite"),
        }
    }
    assert_eq!(seen, vec!["capacity", "nonfinite"]);
}
