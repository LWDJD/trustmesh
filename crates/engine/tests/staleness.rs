//! P6：观点陈旧（staleness）的运行时行为。
//!
//! 这一组测试钉死四件事：
//!
//! 1. **可观察**：`view_age` / `freshness_stats` 如实报出「某个来源的观点已经
//!    多久没更新」以及整体的年龄分布，且只读、不影响任何权重。
//! 2. **可清理**：`prune_stale` 按年龄显式丢弃观点，返回丢弃条数，
//!    丢弃后该来源彻底退出 `query` 的群体项；它不会碰正分清单。
//! 3. **基准是 last_check**：衰减看的是「我上次成功取到它的时间」，
//!    而不是对方声明的 `published_at`——伪造一个很新的发布时间毫无用处。
//! 4. **衰减是相对的**：加权平均对权重的均匀缩放免疫，所有来源一起变旧时
//!    结果纹丝不动；只有「谁比谁更新」才会改变话语权。

use tm_agg::decay;
use tm_engine::{Engine, EngineConfig};
use tm_store::{AccountKey, AccountIdx};
use tm_view::{CompressedView, ViewParams};

/// 构造一个容易区分、且不会互相撞车的公钥。
fn key(n: u8) -> AccountKey {
    let mut k = [0u8; 32];
    k[0] = n;
    k[1] = 0xA5;
    k
}

/// 默认口径：beta = 0.3、半衰期 72 小时。种子固定，结果可复现。
fn eng() -> Engine {
    Engine::with_seed(
        EngineConfig {
            beta: 0.3,
            half_life: 72,
            // 两级衰减的新机制显式关闭（默认值已经全是关闭态）：
            // 本文件钉的是「来源级衰减 + last_check 基准」这一旧语义。
            group_half_life: 0,
            coverage_exponent: 1.0,
            unreachable_after: 0,
            remove_when_coverage: 0.0,
            min_group_sources: 0,
            ..Default::default()
        },
        42,
    )
}

/// 一份「只对 target 表态」的观点。单条记录时 total_mass 恰好等于该分数，
/// 归一化后查询值就是 1.0，适合当「极端一票」用。
fn view_of(target: AccountKey, score: f32) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, score)])
}

/// 一份「对 target 打 t_score、对 other 打 o_score」的观点。
///
/// 两条记录且两个 key 都已 intern 时 total_mass = t_score + o_score，
/// 只要取和为 1.0，归一化后就得到精确的 t_score。
fn view_two(target: AccountKey, t_score: f32, other: AccountKey, o_score: f32) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, t_score), (other, o_score)])
}

// ===========================================================================
// 1. view_age —— 单个来源的年龄
// ===========================================================================

/// 刚收到的观点年龄为 0：哪怕对方声称它是 0 时刻发布的。
#[test]
fn view_age_is_zero_right_after_receiving() {
    let mut e = eng();
    let friend = e.intern(key(1));
    e.rate(friend, 1.0, 0);

    // published_at = 0（很早），last_check = 500（刚刚取到）
    e.on_view(friend, view_of(key(9), 1.0), 0, 500);
    assert_eq!(e.view_age(friend, 500), Some(0), "刚取到的观点年龄必须是 0");

    // 再取一次（内容不同），年龄同样归零
    e.on_view(friend, view_of(key(9), 0.5), 0, 900);
    assert_eq!(e.view_age(friend, 900), Some(0));
}

/// 推进时间后年龄线性增长。
#[test]
fn view_age_grows_with_elapsed_time() {
    let mut e = eng();
    let friend = e.intern(key(1));
    e.rate(friend, 1.0, 0);
    e.on_view(friend, view_of(key(9), 1.0), 0, 1_000);

    assert_eq!(e.view_age(friend, 1_000), Some(0));
    assert_eq!(e.view_age(friend, 1_001), Some(1));
    assert_eq!(e.view_age(friend, 1_072), Some(72));
    assert_eq!(e.view_age(friend, 1_000 + 72 * 10), Some(720));
    assert_eq!(e.view_age(friend, 50_000), Some(49_000));
    assert_eq!(e.view_age(friend, u64::MAX), Some(u64::MAX - 1_000));
}

/// 没有观点就是 None；「有评分」不等于「有观点」。
#[test]
fn view_age_is_none_without_view() {
    let mut e = eng();
    let friend = e.intern(key(1));

    assert_eq!(e.view_age(friend, 0), None, "从没取过观点");
    assert_eq!(e.view_age(friend, 123_456), None);

    e.rate(friend, 1.0, 0);
    assert_eq!(e.view_age(friend, 0), None, "只有评分不算有观点");

    assert_eq!(e.view_age(9_999, 0), None, "不存在的账户");
}

/// 只有「成功取到」才刷新年龄；取失败不更新（衰减自然发生）。
#[test]
fn only_successful_fetch_refreshes_age() {
    let mut e = eng();
    let friend = e.intern(key(1));
    e.rate(friend, 1.0, 0);
    e.on_view(friend, view_of(key(9), 1.0), 0, 0);

    assert_eq!(e.view_age(friend, 0), Some(0));
    assert_eq!(e.view_age(friend, 100), Some(100));

    // 取失败：时间戳不动，观点继续变旧
    e.on_view_failed(friend, 100);
    assert_eq!(e.view_age(friend, 100), Some(100), "失败不应刷新年龄");
    assert_eq!(e.view_age(friend, 172), Some(172));

    // 取成功：归零
    e.on_view(friend, view_of(key(9), 1.0), 100, 100);
    assert_eq!(e.view_age(friend, 100), Some(0));
    assert_eq!(e.view_age(friend, 172), Some(72));
}

// ===========================================================================
// 2. 陈旧如何改变权重
// ===========================================================================

/// 一条来源不更新、另一条持续更新：前者的**相对**权重持续下降，
/// 且下降速度精确等于半衰期（每 H 小时折半）。
#[test]
fn stale_source_loses_relative_weight_to_fresh_source() {
    let mut e = eng();
    let stale = e.intern(key(1));
    let fresh = e.intern(key(2));
    let target = e.intern(key(9));
    e.rate(stale, 1.0, 0);
    e.rate(fresh, 1.0, 0);

    // stale 说 target = 1.0（捧），fresh 说 target = 0.0（踩）
    e.on_view(stale, view_of(key(9), 1.0), 0, 0);
    e.on_view(fresh, view_of(key(9), 0.0), 0, 0);

    // 两人同样新鲜：group = (1×1.0 + 1×0.0) / 2 = 0.5，query = 0.7 × 0.5
    let at0 = e.query(target, 0);
    assert!((at0 - 0.35).abs() < 1e-5, "两个同样新鲜的来源应各占一半：{at0}");

    let mut prev = at0;
    for &t in &[72u64, 144, 216, 360, 720, 1_440] {
        // fresh 一直在更新，stale 从不更新
        e.on_view(fresh, view_of(key(9), 0.0), t, t);
        assert_eq!(e.view_age(fresh, t), Some(0));
        assert_eq!(e.view_age(stale, t), Some(t), "stale 的年龄应一路增长");

        // fresh 权重恒为 1；stale 权重 0.5^(t/H)，其相对份额 = w / (w + 1)
        let w_stale = f64::from(decay(t, 72));
        let expected_group = w_stale / (w_stale + 1.0);
        let expected = 0.7 * expected_group;

        let q = e.query(target, t);
        assert!(
            (f64::from(q) - expected).abs() < 1e-5,
            "t={t}: 期望 {expected}，实际 {q}"
        );
        assert!(q < prev, "t={t}: 陈旧的来源必须持续让位（{q} 应小于 {prev}）");
        prev = q;
    }

    // 一个半衰期处，stale 的相对份额正好是 1/3 —— 权重确实折半了一次
    e.on_view(fresh, view_of(key(9), 0.0), 72, 72);
    let one_hl = e.query(target, 72);
    assert!(
        (one_hl - 0.7 / 3.0).abs() < 1e-5,
        "一个半衰期后 group 应为 1/3，query = 0.23333，实际 {one_hl}"
    );
}

/// 单来源：所有来源一起变旧时 query 结果**完全不变**（加权平均对均匀缩放免疫）。
///
/// 这是已确认的性质，必须用测试钉死，防止有人误以为「陈旧会让分数向 0 掉」。
#[test]
fn single_source_uniform_decay_leaves_query_unchanged() {
    let mut e = eng();
    let friend = e.intern(key(1));
    let _other = e.intern(key(50)); // 必须登记，否则 total_mass 会漏掉这一条
    let target = e.intern(key(9));
    e.rate(friend, 1.0, 0);

    // total_mass = 0.6 + 0.4 = 1.0，所以观点里 target 的值就是 0.6
    e.on_view(friend, view_two(key(9), 0.6, key(50), 0.4), 0, 0);

    let q0 = e.query(target, 0);
    assert!((q0 - 0.42).abs() < 1e-4, "0.3×0 + 0.7×0.6 = 0.42，实际 {q0}");

    for &t in &[0u64, 1, 36, 72, 720, 72_000, 1_000_000, u64::MAX] {
        let q = e.query(target, t);
        assert!(
            (q - q0).abs() < 1e-6,
            "t={t}: 只有一个来源时变旧不应改变结果（{q} vs {q0}）"
        );
    }
}

/// 衰减到很多个半衰期后权重趋近 0，但不为负、不 NaN、不越界。
#[test]
fn extreme_age_decays_toward_zero_without_going_negative_or_nan() {
    let mut e = eng();
    let stale = e.intern(key(1));
    let fresh = e.intern(key(2));
    let target = e.intern(key(9));
    e.rate(stale, 1.0, 0);
    e.rate(fresh, 1.0, 0);
    e.on_view(stale, view_of(key(9), 1.0), 0, 0);
    e.on_view(fresh, view_of(key(9), 0.0), 0, 0);

    // stale 说 1.0，fresh 说 0.0：stale 越旧，group 越靠近 0
    let mut prev = f32::INFINITY;
    for k in [1u64, 10, 100, 1_000, 10_000, 1_000_000, u64::MAX / 72] {
        let t = k * 72;
        e.on_view(fresh, view_of(key(9), 0.0), t, t);
        let q = e.query(target, t);
        assert!(q.is_finite(), "k={k}, t={t}: 出现 NaN/∞：{q}");
        assert!((0.0..=1.0).contains(&q), "k={k}, t={t}: 越界 {q}");
        assert!(q <= prev, "k={k}, t={t}: 权重应单调趋零（{q} > {prev}）");
        prev = q;
    }
    assert!(prev < 1e-6, "很多个半衰期后应趋近 0，实际 {prev}");

    // 反向：陈旧的是低分来源，group 应单调趋近新鲜来源的 1.0，且永不超过
    let mut e2 = eng();
    let stale2 = e2.intern(key(1));
    let fresh2 = e2.intern(key(2));
    let target2 = e2.intern(key(9));
    e2.rate(stale2, 1.0, 0);
    e2.rate(fresh2, 1.0, 0);
    e2.on_view(stale2, view_of(key(9), 0.0), 0, 0);
    e2.on_view(fresh2, view_of(key(9), 1.0), 0, 0);

    let mut prev2 = 0.0f32;
    for k in [1u64, 10, 100, 1_000, 10_000] {
        let t = k * 72;
        e2.on_view(fresh2, view_of(key(9), 1.0), t, t);
        let q = e2.query(target2, t);
        assert!(q.is_finite() && (0.0..=1.0).contains(&q), "k={k}: 越界 {q}");
        assert!(q >= prev2, "k={k}: 应单调趋近新鲜来源（{q} < {prev2}）");
        assert!(q <= 0.7 + 1e-6, "k={k}: 不应超过新鲜来源的取值 {q}");
        prev2 = q;
    }
    assert!((prev2 - 0.7).abs() < 1e-5, "新鲜来源应完全主导，实际 {prev2}");

    // 时间戳顶到 u64 上界也不能炸
    let huge = e.query(target, u64::MAX);
    assert!(huge.is_finite() && (0.0..=1.0).contains(&huge), "极端时间戳：{huge}");
}

// ===========================================================================
// 3. prune_stale —— 显式清理
// ===========================================================================

/// 按年龄丢弃，边界（age == max_age）保留，返回值等于丢弃条数。
#[test]
fn prune_stale_drops_by_age_boundary_and_counts() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let c = e.intern(key(3));
    let _target = e.intern(key(9)); // 让观点里的 key 是「已知账户」，与真实场景一致
    for &x in &[a, b, c] {
        e.rate(x, 1.0, 0);
    }
    e.on_view(a, view_of(key(9), 1.0), 0, 0); // now=200 时年龄 200
    e.on_view(b, view_of(key(9), 0.5), 0, 100); // 年龄 100
    e.on_view(c, view_of(key(9), 0.0), 0, 200); // 年龄 0

    assert_eq!(e.freshness_stats(200).count, 3);

    // 只丢 a（200 > 100）；b 的年龄恰好等于阈值，保留
    assert_eq!(e.prune_stale(200, 100), 1);
    assert_eq!(e.view_age(a, 200), None);
    assert_eq!(e.view_age(b, 200), Some(100));
    assert_eq!(e.view_age(c, 200), Some(0));

    // 再扫一次没有可丢的（幂等）
    assert_eq!(e.prune_stale(200, 100), 0);
    // 阈值放到 200：b 的年龄 100 <= 200，仍然保留
    assert_eq!(e.prune_stale(200, 200), 0);

    // max_age = 0 只保留「恰好在这一刻取到」的那条
    assert_eq!(e.prune_stale(200, 0), 1);
    assert_eq!(e.view_age(b, 200), None);
    assert_eq!(e.view_age(c, 200), Some(0));
    assert_eq!(e.prune_stale(200, 0), 0);

    // 时间继续走，最后一条也会过期
    assert_eq!(e.prune_stale(1_000_000, 999_000), 1);
    assert_eq!(e.freshness_stats(1_000_000).count, 0);
    assert_eq!(e.prune_stale(1_000_000, u64::MAX), 0, "空集合清理返回 0");
}

/// 清理后该来源彻底退出 query 的群体项，但正分清单原封不动。
#[test]
fn prune_stale_removes_source_from_query() {
    let mut e = eng();
    let friend = e.intern(key(1));
    let _other = e.intern(key(50)); // 必须登记，否则 total_mass 会漏掉这一条
    let target = e.intern(key(9));
    e.rate(friend, 1.0, 0);
    e.on_view(friend, view_two(key(9), 0.6, key(50), 0.4), 0, 0);

    assert!((e.query(target, 0) - 0.42).abs() < 1e-4);
    assert_eq!(e.prune_stale(0, 0), 0, "刚取到的不该被清掉");
    assert!((e.query(target, 0) - 0.42).abs() < 1e-4);

    // 48 小时后仍未超过 72 小时半衰期
    assert_eq!(e.prune_stale(48, 72), 0);
    assert!((e.query(target, 48) - 0.42).abs() < 1e-4, "单来源时衰减不改变结果");

    // 100 小时后按「超过 24 小时就算旧」清理
    assert_eq!(e.prune_stale(100, 24), 1);
    assert_eq!(e.view_age(friend, 100), None);
    assert!(
        e.query(target, 100).abs() < 1e-6,
        "观点被清掉后群体项应为 0（我没给 target 打分），实际 {}",
        e.query(target, 100)
    );
    assert_eq!(e.freshness_stats(100).count, 0);
    assert_eq!(e.stats().2, 0, "views 缓存应为空");
    assert_eq!(e.stats().3, 1, "正分清单不该被清理动到");
    assert_eq!(e.rating_of(friend), Some(1.0), "评分必须保留");
}

// ===========================================================================
// 4. freshness_stats —— 整体年龄分布
// ===========================================================================

/// 偶数个取中间两个的均值，奇数个取正中间；计数与新鲜数按半衰期切分。
#[test]
fn freshness_stats_counts_median_and_max() {
    let mut e = eng();
    let ages = [0u64, 10, 72, 100, 500, 1_000];
    let mut owners: Vec<AccountIdx> = Vec::new();
    for (i, &age) in ages.iter().enumerate() {
        let owner = e.intern(key((i + 1) as u8));
        e.rate(owner, 1.0, 0);
        // now = 1000 时这条观点的年龄恰好是 age
        e.on_view(owner, view_of(key(99), 0.5), 0, 1_000 - age);
        owners.push(owner);
    }

    let s = e.freshness_stats(1_000);
    assert_eq!(s.count, 6);
    // 年龄 <= 72 的算「仍有一半以上话语权」：0、10、72 三条
    assert_eq!(s.fresh_within_half_life, 3);
    // 排序后 [0,10,72,100,500,1000]，中间两个是 72 与 100
    assert_eq!(s.median_age, 86);
    assert_eq!(s.max_age, 1_000);

    for (i, &age) in ages.iter().enumerate() {
        assert_eq!(e.view_age(owners[i], 1_000), Some(age), "第 {i} 条年龄不符");
    }

    // 丢掉最老的那条后变成奇数个：中位数直接取正中间
    assert_eq!(e.prune_stale(1_000, 500), 1);
    let s = e.freshness_stats(1_000);
    assert_eq!(s.count, 5);
    assert_eq!(s.fresh_within_half_life, 3);
    assert_eq!(s.median_age, 72, "奇数个应取正中间");
    assert_eq!(s.max_age, 500);

    // 时间推到 ages 全部翻倍的位置，新鲜度统计随之下降
    let s = e.freshness_stats(1_072);
    assert_eq!(s.count, 5);
    // 年龄变成 [72, 82, 144, 172, 572]，中位数 144、最大 572
    assert_eq!(s.fresh_within_half_life, 1, "只有最新那条还在半衰期内");
    assert_eq!(s.median_age, 144);
    assert_eq!(s.max_age, 572);
}

/// 空集合返回全零，不返回 panic 或垃圾值。
#[test]
fn freshness_stats_empty_is_all_zero() {
    let e = eng();
    let s = e.freshness_stats(123_456);
    assert_eq!(s.count, 0);
    assert_eq!(s.fresh_within_half_life, 0);
    assert_eq!(s.median_age, 0);
    assert_eq!(s.max_age, 0);
    assert_eq!(s, Default::default());
}

/// 统计只覆盖「真的存下了观点」的账户，跟评分数量无关。
#[test]
fn freshness_stats_counts_only_cached_views() {
    let mut e = eng();
    let mut owners: Vec<AccountIdx> = Vec::new();
    for i in 0..5u8 {
        let x = e.intern(key(i + 1));
        e.rate(x, 1.0, 0); // 只有评分，没有观点
        owners.push(x);
    }
    assert_eq!(e.freshness_stats(100).count, 0);

    e.on_view(owners[0], view_of(key(9), 1.0), 0, 0);
    e.on_view(owners[1], view_of(key(9), 1.0), 0, 50);

    let s = e.freshness_stats(100);
    assert_eq!(s.count, 2);
    assert_eq!(s.max_age, 100);
    assert_eq!(s.median_age, 75, "(50 + 100) / 2");
    // 年龄 50 的那条仍在半衰期内，年龄 100 的已经超出了
    assert_eq!(s.fresh_within_half_life, 1);
    assert_eq!(e.stats().2, 2, "count 与 views 长度一致");
    assert_eq!(e.stats().3, 5, "评分条目数不受影响");
}

// ===========================================================================
// 5. 衰减基准 = last_check（不是 published_at）
// ===========================================================================

/// published_at 很早、last_check 很新 → 完全不衰减；
/// 且 published_at 的取值对 query 没有任何影响。
#[test]
fn decay_basis_is_last_check_not_published_at() {
    // A：对方声称观点写于 0 时刻（很旧），但我是 1000 时刻刚取到的
    let mut a = eng();
    let friend_a = a.intern(key(1));
    let _other_a = a.intern(key(50)); // 必须登记，否则 total_mass 会漏掉这一条
    let target_a = a.intern(key(9));
    a.rate(friend_a, 1.0, 0);
    a.on_view(friend_a, view_two(key(9), 0.6, key(50), 0.4), 0, 1_000);

    // B：内容一模一样，只是 published_at 换成了「刚刚」
    let mut b = eng();
    let friend_b = b.intern(key(1));
    let _other_b = b.intern(key(50)); // 同上
    let target_b = b.intern(key(9));
    b.rate(friend_b, 1.0, 0);
    b.on_view(friend_b, view_two(key(9), 0.6, key(50), 0.4), 1_000, 1_000);

    let qa = a.query(target_a, 1_000);
    let qb = b.query(target_b, 1_000);
    assert!(
        (qa - qb).abs() < 1e-7,
        "published_at 不应参与衰减：{qa} vs {qb}"
    );
    // last_check 是刚刚，所以没有任何衰减：0.3×0 + 0.7×0.6 = 0.42
    assert!(
        (qa - 0.42).abs() < 1e-4,
        "声称很旧但刚取到的观点不该被衰减，实际 {qa}"
    );

    // 若按 published_at 衰减，A 的权重会掉到 0.5^(1000/72)≈6.5e-5，
    // 结果会远低于 0.42；用它作反证。
    let by_published = 0.7 * 0.6 * f64::from(decay(1_000, 72));
    assert!(
        (f64::from(qa) - by_published).abs() > 0.01,
        "结果不该符合「按 published_at 衰减」的假设"
    );

    // 反向：last_check 旧、published_at 新 → 必须按 last_check 衰减
    let mut c = eng();
    let stale_c = c.intern(key(1));
    let fresh_c = c.intern(key(2));
    let target_c = c.intern(key(9));
    c.rate(stale_c, 1.0, 0);
    c.rate(fresh_c, 1.0, 0);
    c.on_view(stale_c, view_of(key(9), 1.0), 1_000, 0); // 谎称刚发布
    c.on_view(fresh_c, view_of(key(9), 0.0), 1_000, 1_000);
    let qc = c.query(target_c, 1_000);
    assert!(
        qc < 1e-3,
        "陈旧的来源不会因为自称「刚发布」而保住权重，实际 {qc}"
    );
    assert_eq!(c.view_age(stale_c, 1_000), Some(1_000));
    assert_eq!(c.view_age(fresh_c, 1_000), Some(0));

    // 反过来（陈旧的是低分来源）则由新鲜来源主导
    let mut d = eng();
    let stale_d = d.intern(key(1));
    let fresh_d = d.intern(key(2));
    let target_d = d.intern(key(9));
    d.rate(stale_d, 1.0, 0);
    d.rate(fresh_d, 1.0, 0);
    d.on_view(stale_d, view_of(key(9), 0.0), 1_000, 0);
    d.on_view(fresh_d, view_of(key(9), 1.0), 1_000, 1_000);
    assert!(
        (d.query(target_d, 1_000) - 0.7).abs() < 1e-4,
        "新鲜来源应完全主导，实际 {}",
        d.query(target_d, 1_000)
    );
}

// ===========================================================================
// 6. 时间回拨
// ===========================================================================

/// now 比 last_check 还小：年龄按 0 处理，不 panic、不出负数、不误删。
#[test]
fn time_rollback_is_clamped_to_zero_age() {
    let mut e = eng();
    let stale = e.intern(key(1));
    let fresh = e.intern(key(2));
    let target = e.intern(key(9));
    e.rate(stale, 1.0, 0);
    e.rate(fresh, 1.0, 0);
    e.on_view(stale, view_of(key(9), 1.0), 0, 1_000);
    e.on_view(fresh, view_of(key(9), 0.0), 0, 900);

    // 回拨到 500：两条都「还没取到呢」，年龄夹到 0
    assert_eq!(e.view_age(stale, 500), Some(0));
    assert_eq!(e.view_age(fresh, 500), Some(0));

    let s = e.freshness_stats(500);
    assert_eq!(s.count, 2);
    assert_eq!(s.median_age, 0);
    assert_eq!(s.max_age, 0);
    assert_eq!(s.fresh_within_half_life, 2);

    // 不回绕：与「两条都在 500 时刻取到」的引擎结果完全一致
    let mut same = eng();
    let stale2 = same.intern(key(1));
    let fresh2 = same.intern(key(2));
    let target2 = same.intern(key(9));
    same.rate(stale2, 1.0, 0);
    same.rate(fresh2, 1.0, 0);
    same.on_view(stale2, view_of(key(9), 1.0), 0, 500);
    same.on_view(fresh2, view_of(key(9), 0.0), 0, 500);
    let q = e.query(target, 500);
    assert!((q - same.query(target2, 500)).abs() < 1e-6, "回拨应按 dt=0 处理");
    assert!((q - 0.35).abs() < 1e-5, "两条同样新鲜时 group = 0.5，实际 {q}");

    // 回拨时清理更保守：max_age = 0 也不会误删
    assert_eq!(e.prune_stale(500, 0), 0);
    assert_eq!(e.freshness_stats(500).count, 2);

    // 极端回拨（now = 0）同样安全
    assert_eq!(e.view_age(stale, 0), Some(0));
    assert_eq!(e.prune_stale(0, u64::MAX), 0);
    assert!(e.query(target, u64::MAX).is_finite());
}

// ===========================================================================
// 7. 边界
// ===========================================================================

/// 半衰期为 0 是非法降级输入：按 1 处理（与 `tm_agg::decay` 的约定一致），
/// 这样「半衰期内」的判定与衰减因子严格对齐。
#[test]
fn freshness_stats_treats_zero_half_life_as_one() {
    let mut e = Engine::with_seed(
        EngineConfig {
            beta: 0.3,
            half_life: 0,
            // 同上：两级衰减全部关闭。
            group_half_life: 0,
            coverage_exponent: 1.0,
            unreachable_after: 0,
            remove_when_coverage: 0.0,
            min_group_sources: 0,
            ..Default::default()
        },
        7,
    );
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    e.rate(a, 1.0, 0);
    e.rate(b, 1.0, 0);
    e.on_view(a, view_of(key(9), 1.0), 0, 0); // 年龄 0
    e.on_view(b, view_of(key(9), 0.0), 0, 5); // 年龄 5

    let s = e.freshness_stats(5);
    assert_eq!(s.count, 2);
    assert_eq!(s.fresh_within_half_life, 1, "H=0 按 1 算，年龄 5 已超出");
    assert_eq!(s.median_age, 2, "(0 + 5) / 2 向下取整");
    assert_eq!(s.max_age, 5);
    // 衰减同样把 H=0 当作 1：一个时间单位就折半，因此边界就是 1
    assert_eq!(decay(1, 0), 0.5);
}

/// 撤销评分不会删掉缓存的观点：观察接口如实反映「我手上还有这份观点」，
/// 但它已经不再参与加权——query 只认正分清单里的来源。
#[test]
fn revoked_rating_keeps_view_but_drops_its_weight() {
    let mut e = eng();
    let friend = e.intern(key(1));
    let target = e.intern(key(9));
    e.rate(friend, 1.0, 0);
    e.on_view(friend, view_of(key(9), 1.0), 0, 0);
    assert!((e.query(target, 0) - 0.7).abs() < 1e-5);

    // 打 0 分 = 删除条目，这个来源立刻失去话语权
    e.rate(friend, 0.0, 10);
    assert!(
        e.query(target, 10).abs() < 1e-6,
        "撤销评分后该来源不该再有权重，实际 {}",
        e.query(target, 10)
    );

    // 但观点本身还在缓存里，年龄照常可观察
    assert_eq!(e.view_age(friend, 10), Some(10), "观点缓存不受评分变动影响");
    assert_eq!(e.freshness_stats(10).count, 1);

    // 清理照常生效
    assert_eq!(e.prune_stale(10, 5), 1);
    assert_eq!(e.view_age(friend, 10), None);
    assert_eq!(e.freshness_stats(10).count, 0);
}
