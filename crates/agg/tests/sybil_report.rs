//! 单点操纵实验：一个撒谎者能把「群体项」抬到多高？
//!
//! 场景固定为 **20 个来源，其中 1 个撒谎者**：撒谎者的观点是 1.0，
//! 其余 19 个诚实来源的观点是 0.0。然后不断放大撒谎者的权重，
//! 观察四种聚合口径分别被带到哪里。
//!
//! 全部数字都由本文件里的断言固定住 —— 谁改了实现，这里就会失败。
//!
//! # 结论速览
//!
//! * [AggMode::Mean]：结果**严格等于撒谎者的权重占比**，一点折扣都不打（线性上升）。
//! * [AggMode::Median]：完全不看权重，只要撒谎者只有 1 个，结果恒为 0。
//! * [AggMode::WeightedMedian]：撒谎者权重**严格过半**时翻到 1.0，50% 及以下仍是 0
//!   （下侧中位数，平分时判给多数派）。
//! * [AggMode::Trimmed]：只有 p 不小于撒谎者的权重占比时才完全免疫；
//!   而 p 被 [tm_agg::MAX_TRIM] 限制在 0.5 以下，所以撒谎者权重过半时它也救不了。
//!
//! 看表格：`cargo test --test sybil_report -- --nocapture --test-threads=1`

use tm_agg::{aggregate, combine, decay, AggMode};

/// 诚实来源的个数（再加 1 个撒谎者，共 20 个来源）。
const HONEST: usize = 19;

/// 每个诚实来源的固定权重。只放大撒谎者的权重，诚实来源保持 1.0 不动，
/// 这样 20 个来源从头到尾都在列表里，不会因为权重归零被剔除，
/// 「占比」的变化才是干净的对照实验。
const HONEST_W: f32 = 1.0;

/// 构造「1 个撒谎者 + 19 个诚实来源」的待聚合列表。
///
/// 撒谎者：(liar_weight, 1.0)；诚实来源：(1.0, 0.0) × 19。
fn scenario(liar_weight: f32) -> Vec<(f32, f32)> {
    let mut items = Vec::with_capacity(HONEST + 1);
    items.push((liar_weight, 1.0f32));
    for _ in 0..HONEST {
        items.push((HONEST_W, 0.0f32));
    }
    items
}

/// 撒谎者的**实际**权重占比（用 f32 权重算出来的真实值，不是设定值）。
fn share_of(liar_weight: f32) -> f64 {
    let w = liar_weight as f64;
    w / (w + HONEST as f64 * HONEST_W as f64)
}

/// 反解：想让撒谎者占到总权重的 s，它的权重该设成多少。
///
/// w / (w + 19) = s  =>  w = 19s / (1 - s)；s = 1 时无解，取 f32::MAX 代表「近似 100%」。
fn liar_weight_for_share(s: f64) -> f32 {
    if s >= 1.0 {
        return f32::MAX;
    }
    let honest_total = HONEST as f64 * HONEST_W as f64;
    (honest_total * s / (1.0 - s)) as f32
}

/// 一次算齐四种口径：(Mean, Median, WeightedMedian, Trimmed(p))。
fn four_modes(items: &[(f32, f32)], p: f32) -> (f32, f32, f32, f32) {
    (
        aggregate(items, AggMode::Mean).unwrap(),
        aggregate(items, AggMode::Median).unwrap(),
        aggregate(items, AggMode::WeightedMedian).unwrap(),
        aggregate(items, AggMode::Trimmed(p)).unwrap(),
    )
}

/// 截尾均值的**闭式解**，用来给实现做独立对拍。
///
/// 本实验的权重轴很规整：19 个 0 观点占 [0, 19)，撒谎者占 [19, 19+w)。
/// 裁剪后保留 [p·T, (1−p)·T]，结果就是「保留区间与撒谎者区间的重叠部分」占比。
fn trimmed_closed_form(liar_weight: f32, p: f32) -> f32 {
    let w = liar_weight as f64;
    let honest_end = HONEST as f64 * HONEST_W as f64;
    let total = w + honest_end;
    let lo = p as f64 * total;
    let hi = total - lo;
    let overlap = (hi.min(total) - lo.max(honest_end)).max(0.0);
    let kept = hi - lo;
    if kept <= 0.0 {
        0.0
    } else {
        (overlap / kept) as f32
    }
}

// ---------------------------------------------------------------------------
// 表 1：撒谎者权重占比 10% → 100%
// ---------------------------------------------------------------------------

#[test]
fn one_liar_weight_sweep() {
    println!();
    println!("===== 表 1：20 个来源，1 个撒谎者，权重占比从 10% 提到 100% =====");
    println!("撒谎者观点 1.0，19 个诚实来源观点 0.0；诚实来源权重恒为 1.0，只放大撒谎者的权重。");
    println!();
    println!(
        "{:>7}  {:>10}  {:>9}  {:>9}  {:>9}  {:>10}  {:>10}",
        "share%", "w_liar", "Mean", "Median", "WMed", "Trim(.05)", "Trim(.30)"
    );

    let mut rows: Vec<(f64, f32, f32, f32, f32, f32)> = Vec::new();
    for k in 1..=10 {
        let s = k as f64 / 10.0;
        let w = liar_weight_for_share(s);
        let items = scenario(w);
        let (mean, median, wmed, t05) = four_modes(&items, 0.05);
        let t30 = aggregate(&items, AggMode::Trimmed(0.30)).unwrap();
        let share = share_of(w);
        rows.push((share, mean, median, wmed, t05, t30));
        println!(
            "{:>6.1}%  {:>10.3e}  {:>9.6}  {:>9.6}  {:>9.6}  {:>10.6}  {:>10.6}",
            share * 100.0,
            w,
            mean,
            median,
            wmed,
            t05,
            t30
        );
    }

    // ---- 断言：把上面每一列的性质钉死 ----
    for &(share, mean, median, wmed, t05, t30) in &rows {
        // 1) Mean 严格等于撒谎者的权重占比：线性，没有任何缓冲
        assert!(
            (mean as f64 - share).abs() < 1e-6,
            "Mean 应当等于撒谎者权重占比 {share}，实际 {mean}"
        );

        // 2) Median 对权重完全免疫：撒谎者只有 1 票，中位数永远是 0
        assert_eq!(median, 0.0, "占比 {share} 时 Median 动了：{median}");

        // 3) WeightedMedian 在权重严格过半时翻转
        if share > 0.5 {
            assert_eq!(wmed, 1.0, "占比 {share} 时 WMed 应该翻到 1.0，实际 {wmed}");
        } else {
            assert_eq!(wmed, 0.0, "占比 {share} 时 WMed 不该动，实际 {wmed}");
        }

        // 4) 截尾均值：与闭式解对拍
        let w = liar_weight_for_share(share);
        assert!(
            (t05 - trimmed_closed_form(w, 0.05)).abs() < 1e-5,
            "占比 {share}、p = 0.05: Trimmed = {t05}，闭式解 = {}",
            trimmed_closed_form(w, 0.05)
        );
        assert!(
            (t30 - trimmed_closed_form(w, 0.30)).abs() < 1e-5,
            "占比 {share}、p = 0.30: Trimmed = {t30}，闭式解 = {}",
            trimmed_closed_form(w, 0.30)
        );
        // 5) 截尾的方向依赖占比：不到一半只会往下修，超过一半反而比 Mean 更高
        if share < 0.5 {
            assert!(t05 <= mean + 1e-6, "占比 {share}：Trimmed 不该比 Mean 高，{t05} > {mean}");
        } else if share > 0.5 {
            assert!(
                t05 >= mean - 1e-6,
                "占比 {share}：过半时截尾应当帮倒忙（不低于 Mean），实际 {t05} < {mean}"
            );
        }
    }

    println!();
    println!("注意 60%~90% 这几行：Trim(.05) 比 Mean 还高 —— 权重过半时截尾会帮倒忙。");
    println!("原因：两端各裁掉同样多的权重，而撒谎者那侧总权重更大，被裁掉的比例反而更小。");

    // ---- 报告里要引用的四行：10% / 30% / 50% / 100% ----
    println!();
    println!("---------- 关键四行（占比 10% / 30% / 50% / 100%）----------");
    for &(share, mean, median, wmed, t05, t30) in &rows {
        let pct = (share * 100.0).round();
        if (pct - 10.0).abs() < 0.5
            || (pct - 30.0).abs() < 0.5
            || (pct - 50.0).abs() < 0.5
            || (pct - 100.0).abs() < 0.5
        {
            println!(
                "占比 {:>3.0}%  ->  Mean {:.4}   Median {:.4}   WeightedMedian {:.4}   Trimmed(0.05) {:.4}   Trimmed(0.30) {:.4}",
                pct, mean, median, wmed, t05, t30
            );
        }
    }
}

/// 占比 = 100%：诚实来源始终保持权重 1.0，撒谎者的权重设到 f32 上限。
///
/// 这时它的权重占比是 1 − 5.6e-38，f32 下就是 1.0；
/// 关键点是**20 个来源一个都没被剔除**，所以 Median 仍然只看人头。
#[test]
fn one_liar_at_full_weight_share() {
    let items = scenario(f32::MAX);
    let (mean, median, wmed, _t05) = four_modes(&items, 0.05);
    let share = share_of(f32::MAX);

    println!();
    println!("===== 表 1 补充：撒谎者权重占比 -> 100%（w = f32::MAX = {:e}）=====", f32::MAX);
    println!("实际占比 = {share:.17}（f32 下即 1.0）");
    println!("Mean = {mean}, Median = {median}, WeightedMedian = {wmed}");

    assert_eq!(mean, 1.0, "所有权重都在撒谎者手上时，Mean 必然是 1.0");
    assert_eq!(median, 0.0, "19 个 0 观点的来源还在列表里，Median 仍然是 0");
    assert_eq!(wmed, 1.0, "加权中位数这时早已被拿下");
}

/// 退化对照：如果「100%」的意思是**把诚实来源的权重压到 0**，
/// 它们会被全局规则「剔除权重 <= 0 的条目」整条剔除，四种口径一起失效。
///
/// 这个边界说明：协议里权重 = 我的评分 × 衰减因子，衰减恒大于 0，
/// 所以权重恰好为 0 只可能来自「我给他打了 0 分」，本就不该出现在待聚合列表里。
#[test]
fn degenerate_hundred_percent_share_drops_honest_sources() {
    // 撒谎者 (1.0, 1.0)；19 个诚实来源的权重直接被压到 0（等于「我给他们打了 0 分」）
    let mut items = vec![(1.0f32, 1.0f32)];
    for _ in 0..HONEST {
        items.push((0.0f32, 0.0f32));
    }
    let (mean, median, wmed, t05) = four_modes(&items, 0.05);
    println!();
    println!("===== 退化对照：诚实来源权重被压到 0（它们等于没表态）=====");
    println!("Mean = {mean}, Median = {median}, WeightedMedian = {wmed}, Trimmed(0.05) = {t05}");
    assert_eq!(mean, 1.0);
    assert_eq!(median, 1.0, "只剩撒谎者一个有效条目");
    assert_eq!(wmed, 1.0);
    assert_eq!(t05, 1.0);
}

// ---------------------------------------------------------------------------
// 表 2：人头攻击 —— Median 挡得住权重，挡不住人头
// ---------------------------------------------------------------------------

#[test]
fn headcount_beats_median_but_weight_does_not() {
    println!();
    println!("===== 表 2：20 个来源，等权重 1.0，撒谎者从 0 个加到 20 个 =====");
    println!("说明 Median 抗的是「权重」，不是「人头」：撒谎者变成 10 个就能把中位数顶到 0.5。");
    println!();
    println!(
        "{:>10}  {:>9}  {:>9}  {:>9}  {:>10}",
        "liars", "Mean", "Median", "WMed", "Trim(.20)"
    );

    let total = HONEST + 1;
    let mut saw_median_half = false;
    for k in 0..=total {
        let items: Vec<(f32, f32)> = (0..total)
            .map(|i| (1.0f32, if i < k { 1.0f32 } else { 0.0f32 }))
            .collect();
        let mean = aggregate(&items, AggMode::Mean).unwrap();
        let median = aggregate(&items, AggMode::Median).unwrap();
        let wmed = aggregate(&items, AggMode::WeightedMedian).unwrap();
        let trim = aggregate(&items, AggMode::Trimmed(0.20)).unwrap();
        println!(
            "{:>10}  {:>9.4}  {:>9.4}  {:>9.4}  {:>10.4}",
            k, mean, median, wmed, trim
        );

        // Mean 一定等于撒谎者的**人头**占比
        let expect = k as f32 / total as f32;
        assert!((mean - expect).abs() < 1e-6, "k = {k}: Mean {mean} 应当是 {expect}");

        // 中位数：<= 9 个撒谎者时仍是 0；10 个时是 0.5（两侧各一半）；>= 11 个时是 1
        let expect_median = if k <= 9 {
            0.0
        } else if k == 10 {
            0.5
        } else {
            1.0
        };
        assert!(
            (median - expect_median).abs() < 1e-6,
            "k = {k}: Median {median} 应当是 {expect_median}"
        );
        if k == 10 {
            saw_median_half = true;
        }

        // 加权中位数需要**严格过半**的权重（11 个），才能翻过去
        let expect_wmed = if k <= 10 { 0.0 } else { 1.0 };
        assert!(
            (wmed - expect_wmed).abs() < 1e-6,
            "k = {k}: WMed {wmed} 应当是 {expect_wmed}"
        );
    }
    assert!(saw_median_half);

    // 一句话总结：1 个撒谎者拿 100% 权重也动不了 Median，
    // 但 10 个撒谎者各拿 1/20 的权重，Median 立刻变成 0.5。
    let one_liar_all_weight = scenario(f32::MAX);
    assert_eq!(aggregate(&one_liar_all_weight, AggMode::Median).unwrap(), 0.0);
    let ten_liars: Vec<(f32, f32)> = (0..total)
        .map(|i| (1.0f32, if i < 10 { 1.0f32 } else { 0.0f32 }))
        .collect();
    assert_eq!(aggregate(&ten_liars, AggMode::Median).unwrap(), 0.5);
    println!();
    println!("对照：1 个撒谎者拿 100% 权重 -> Median 0.0000；10 个撒谎者各拿 1/20 权重 -> Median 0.5000");
}

// ---------------------------------------------------------------------------
// 表 3：衰减把「陈旧」变成「没有」—— 单个新鲜撒谎者的放大器
// ---------------------------------------------------------------------------

#[test]
fn stale_honest_sources_let_one_liar_win() {
    println!();
    println!("===== 表 3：撒谎者永远新鲜（dt = 0），19 个诚实来源一起变陈旧 =====");
    println!("权重 = 我的评分(都是 1.0) × 衰减因子；H = 72 小时。");
    println!();
    // 阈值：19·d < 1 <=> 诚实来源整体权重低于撒谎者 <=> 加权中位数被拿下。
    // d = 0.5^(age/H)，解出 age > H·log2(19) = 72 × 4.2479 ≈ 305.85 小时 ≈ 12.7 天。
    let tipping_age = 72.0 * (19.0f64).log2();
    println!(
        "拐点：age > 72·log2(19) ≈ {tipping_age:.2} 小时（约 {:.1} 天）",
        tipping_age / 24.0
    );
    println!();
    println!(
        "{:>8}  {:>12}  {:>12}  {:>9}  {:>9}  {:>9}",
        "age(h)", "decay", "liar_share", "Mean", "Median", "WMed"
    );

    let cases = [0u64, 24, 72, 144, 216, 305, 306, 360, 720, 1440];

    for &age in &cases {
        let d = decay(age, 72);
        let mut items = vec![(1.0f32 * decay(0, 72), 1.0f32)];
        for _ in 0..HONEST {
            items.push((1.0f32 * d, 0.0f32));
        }
        let mean = aggregate(&items, AggMode::Mean).unwrap();
        let median = aggregate(&items, AggMode::Median).unwrap();
        let wmed = aggregate(&items, AggMode::WeightedMedian).unwrap();
        let share = 1.0 / (1.0 + HONEST as f64 * d as f64);

        println!(
            "{:>8}  {:>12.6}  {:>11.2}%  {:>9.6}  {:>9.6}  {:>9.6}",
            age,
            d,
            share * 100.0,
            mean,
            median,
            wmed
        );

        // Mean 的闭式解：1 / (1 + 19·d)
        let expect = (1.0 / (1.0 + HONEST as f64 * d as f64)) as f32;
        assert!(
            (mean - expect).abs() < 1e-6,
            "age = {age}: Mean {mean} 应当是 {expect}"
        );
        // Median 依然纹丝不动
        assert_eq!(median, 0.0, "age = {age}: Median 动了");
        // 加权中位数在 19d < 1 之后翻脸
        if (age as f64) < tipping_age {
            assert_eq!(wmed, 0.0, "age = {age}: 还没到拐点，WMed 不该动");
        } else {
            assert_eq!(wmed, 1.0, "age = {age}: 已过拐点，WMed 应当翻到 1.0");
        }
    }

    // 极旧的诚实来源：单个撒谎者几乎独占 Mean
    let d = decay(1440, 72);
    let mut items = vec![(1.0f32, 1.0f32)];
    for _ in 0..HONEST {
        items.push((d, 0.0f32));
    }
    let mean = aggregate(&items, AggMode::Mean).unwrap();
    assert!(
        mean > 0.999,
        "诚实来源陈旧 20 个半衰期后，Mean 应当超过 0.999，实际 {mean}"
    );
    println!();
    println!(
        "结论：诚实来源陈旧 20 个半衰期（1440 小时）后，单个新鲜撒谎者把 Mean 顶到 {mean:.6}，Median 仍是 0。"
    );
}

// ---------------------------------------------------------------------------
// 表 4：截尾均值的免疫条件
// ---------------------------------------------------------------------------

#[test]
fn trimmed_immunity_requires_p_at_least_the_attacker_share() {
    println!();
    println!("===== 表 4：Trimmed(p) 的免疫条件 —— p ≥ 撒谎者权重占比 =====");
    println!();
    println!(
        "{:>8}  {:>10}  {:>10}  {:>10}  {:>10}",
        "share%", "p=0.05", "p=0.10", "p=0.25", "p=0.40"
    );

    for k in 1..=4 {
        let w = liar_weight_for_share(k as f64 / 10.0);
        let items = scenario(w);
        let share = share_of(w);
        let mut cells = Vec::new();
        for &p in &[0.05f32, 0.10, 0.25, 0.40] {
            let r = aggregate(&items, AggMode::Trimmed(p)).unwrap();
            cells.push((p, r));
            // 与闭式解对拍：p 一旦不小于撒谎者的权重占比，撒谎者被整条裁掉
            let want = trimmed_closed_form(w, p);
            assert!(
                (r - want).abs() < 1e-5,
                "占比 {share:.3}、p = {p}: Trimmed = {r}，闭式解 = {want}"
            );
            if (p as f64) >= share {
                assert!(r < 1e-5, "占比 {share:.3}、p = {p} 时应当完全免疫，实际 {r}");
            } else {
                assert!(r > 0.0, "占比 {share:.3}、p = {p} 时不该完全免疫，实际 {r}");
            }
        }
        println!(
            "{:>7.1}%  {:>10.6}  {:>10.6}  {:>10.6}  {:>10.6}",
            share * 100.0,
            cells[0].1,
            cells[1].1,
            cells[2].1,
            cells[3].1
        );
    }

    // p 被 MAX_TRIM 限制在 0.5 以下，所以占比过半时 Trimmed 也救不了
    let w = liar_weight_for_share(0.6);
    let items = scenario(w);
    let r = aggregate(&items, AggMode::Trimmed(tm_agg::MAX_TRIM)).unwrap();
    assert!(r > 0.01, "占比 60%、p 取到上限时截尾均值仍应被污染，实际 {r}");
    println!();
    println!(
        "注意：p 被 MAX_TRIM（{:.7}）卡在 0.5 以下，所以撒谎者权重过半后，Trimmed 也救不回来。",
        tm_agg::MAX_TRIM
    );
}

// ---------------------------------------------------------------------------
// 表 5：β 合成本身不设防，只做稀释
// ---------------------------------------------------------------------------

#[test]
fn combine_only_dilutes_the_poisoned_group_term() {
    println!();
    println!("===== 表 5：群体项被顶到 1.0，个人项 0.0，β 只是稀释 =====");
    println!();
    println!("{:>6}  {:>10}", "beta", "result");

    for &beta in &[0.0f32, 0.2, 0.3, 0.5, 0.7, 0.9, 1.0] {
        let r = combine(beta, 0.0, Some(1.0));
        println!("{:>6.2}  {:>10.4}", beta, r);
        assert!((r - (1.0 - beta)).abs() < 1e-6, "beta = {beta}: {r} 应当是 {}", 1.0 - beta);
    }
    // 默认 β = 0.7：群体项里的 1.0 会有 30% 漏进最终声望
    assert!((combine(0.7, 0.0, Some(1.0)) - 0.3).abs() < 1e-6);
    println!();
    println!("默认 β = 0.7 时，被完全污染的群体项仍有 30% 漏进最终声望（0.3）。");
}

/// 汇总表：把「单点操纵」一路算到最终声望（β = 0.7，个人项 0）。
///
/// 这张表是给协议选型用的：同一个撒谎者、同样 20 个来源，
/// 换一个群体项口径，最终声望差多少。
#[test]
fn final_reputation_under_single_liar() {
    let beta = 0.7f32;
    println!();
    println!("===== 表 6：最终声望 = β·个人项 + (1−β)·群体项，β = 0.7、个人项 = 0 =====");
    println!();
    println!(
        "{:>8}  {:>10}  {:>10}  {:>10}  {:>10}",
        "share%", "Mean", "Median", "WMed", "Trim(.30)"
    );

    for &s in &[0.10f64, 0.30, 0.50, 1.00] {
        let w = liar_weight_for_share(s);
        let items = scenario(w);
        let share = share_of(w);
        let mean = aggregate(&items, AggMode::Mean).unwrap();
        let median = aggregate(&items, AggMode::Median).unwrap();
        let wmed = aggregate(&items, AggMode::WeightedMedian).unwrap();
        let trim = aggregate(&items, AggMode::Trimmed(0.30)).unwrap();

        let f_mean = combine(beta, 0.0, Some(mean));
        let f_median = combine(beta, 0.0, Some(median));
        let f_wmed = combine(beta, 0.0, Some(wmed));
        let f_trim = combine(beta, 0.0, Some(trim));

        println!(
            "{:>7.1}%  {:>10.6}  {:>10.6}  {:>10.6}  {:>10.6}",
            share * 100.0,
            f_mean,
            f_median,
            f_wmed,
            f_trim
        );

        // 最终声望同样被 clamp 在 [0,1]；β 只是等比缩放群体项
        assert!((f_median - 0.0).abs() < 1e-6, "Median 口径下最终声望必须仍是 0");
        assert!((f_mean - (1.0 - beta) * mean).abs() < 1e-6);
    }
}
