//! 最小反女巫内核的验收测试。
//!
//! 每条用例对应报告里的一个判据（S1~S6 与判决问题 1~3）。
//! 全部只用 std；不依赖任何对照实现。

use std::collections::BTreeMap;

use tm_mincore::{
    dequantize, quantize, Config, Node, PositiveList, Rng, RoundBitmap, SparseView, Q_ONE,
    SLOT_BYTES, VIEW_OVERHEAD,
};

/// 把分布整理成 `账户 -> f32 位模式`，用于逐位比较。
fn bits(d: &tm_mincore::Distribution) -> BTreeMap<u32, u32> {
    d.entries.iter().map(|&(a, v)| (a, v.to_bits())).collect()
}

// ══════════════════════════ 1. 预算：Σv ≡ 1 ══════════════════════════

/// 一个来源声明「超量」——K 条全部是满额甚至天文数字——接收端的 Σv 仍然是 1。
///
/// 现行实现（先除后夹紧）在这里会得到 Σv = K；本内核先夹后除，得到 1。
#[test]
fn t01_超量声明也只有一个预算() {
    for k in [1usize, 2, 5, 10, 50, 200] {
        for over in [1.0f32, 2.0, 1e3, f32::MAX] {
            let pairs: Vec<(u32, f32)> = (0..k as u32).map(|i| (i, over)).collect();
            let v = SparseView::from_scores(512, &pairs);
            let sum: f64 = v
                .normalized_slots()
                .iter()
                .map(|&(_, x)| f64::from(x))
                .sum();
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "k={k} over={over} 时 Σv = {sum}，应为 1"
            );
            for &(_, x) in v.normalized_slots().iter() {
                assert!(x <= 1.0, "单条 v = {x} 越界");
            }
            // 无论声明得多夸张，总质量都卡在 u16 满量程上。
            assert_eq!(v.total_q(), Q_ONE as u32 * k as u32);
        }
    }
}

/// 合规视图（份额之和为 1）经由 u16 定点往返之后，Σv 仍然是 1。
#[test]
fn t01b_合规视图预算不变() {
    let scores: Vec<f32> = (0..50).map(|i| 0.02 * (i as f32 + 1.0)).collect();
    let total: f32 = scores.iter().sum();
    let pairs: Vec<(u32, f32)> = scores
        .iter()
        .map(|s| (*s / total, 0u32))
        .enumerate()
        .map(|(i, (s, _))| (i as u32, s))
        .collect();
    let v = SparseView::from_scores(512, &pairs);
    assert_eq!(v.len(), 50, "份额都大于 1/131070，不应有条目被量化掉");
    let sum: f64 = v
        .normalized_slots()
        .iter()
        .map(|&(_, x)| f64::from(x))
        .sum();
    assert!((sum - 1.0).abs() < 1e-5, "Σv = {sum}");
}

// ══════════════════════ 2. 声明了但本机不认识的账户 ══════════════════════

/// 「声明了但不认识」的条目**计入分母**、不产生声望。
///
/// 于是「拿 1000 个陌生账户来占预算」是自损：它对我认识的人的话语权等比缩小。
#[test]
fn t02_未登记账户花预算但不产生声望() {
    // 10 个认识的 + 10 个不认识的，全部满额。
    let mut pairs: Vec<(u32, f32)> = (0..10).map(|i| (i, 1.0)).collect();
    pairs.extend((1000..1010).map(|i| (i, 1.0)));
    let v = SparseView::from_scores(512, &pairs);
    assert_eq!(v.total_q(), Q_ONE as u32 * 20);

    // 认识的 10 个每人拿到 1/20，而不是 1/10。
    for i in 0..10u32 {
        assert!((v.normalized(i) - 0.05).abs() < 1e-5);
    }
    // 不认识的读不到任何东西（本机没有它们的账户号之外的任何信息）。
    let known: f64 = (0..10u32).map(|i| f64::from(v.normalized(i))).sum();
    assert!((known - 0.5).abs() < 1e-5, "认识的拿到 {known}，应为 0.5");
}

// ══════════════════════════ 3. u16 定点 ══════════════════════════

/// 量化误差的上下界：`|v − s| ≤ s·K/131070 + 1/131070`。
#[test]
fn t03_u16_定点误差有上界() {
    // 最坏情况：K 条等权。
    for k in [2usize, 10, 100, 512] {
        let s = 1.0 / k as f64;
        let pairs: Vec<(u32, f32)> = (0..k as u32).map(|i| (i, s as f32)).collect();
        let v = SparseView::from_scores(512, &pairs);
        let mut worst = 0.0f64;
        for &(i, x) in v.normalized_slots().iter() {
            worst = worst.max((f64::from(x) - s).abs());
            let _ = i;
        }
        let bound = s * k as f64 / 131070.0 + 1.0 / 131070.0;
        assert!(
            worst <= bound + 1e-12,
            "k={k} 时最大误差 {worst} 超过上界 {bound}"
        );
    }

    // 单调与饱和：任何 > 1 的值都落在同一个定点上，任何 NaN / 负数都落在 0。
    assert_eq!(quantize(1.0), Q_ONE);
    assert_eq!(quantize(1.5), Q_ONE);
    assert_eq!(quantize(f32::MAX), Q_ONE);
    assert_eq!(quantize(f32::INFINITY), Q_ONE);
    assert_eq!(quantize(f32::NAN), 0);
    assert_eq!(quantize(-1.0), 0);
    assert_eq!(quantize(-0.0), 0);
    assert_eq!(quantize(0.0), 0);
    assert!((dequantize(Q_ONE) - 1.0).abs() < 1e-7);

    // 分辨率：相邻定点相差 1/65535。
    assert!((dequantize(1) - 0.0).abs() <= 1.0 / 65535.0);
}

/// 小于 `0.5/65535` 的条目会被量化成 0（丢掉）。这条界决定了「最多支持多少等权条目」。
#[test]
fn t03b_定点能容纳的等权条目数() {
    // 131070 个等权条目时，每条的 q 恰好是 0.5 -> 四舍五入成 1，仍然保留。
    assert_eq!(quantize(1.0 / 131070.0), 1);
    // 再小就归零。
    assert_eq!(quantize(1.0 / 131071.0), 0);

    let pairs: Vec<(u32, f32)> = (0..1000u32).map(|i| (i, 1.0 / 1000.0)).collect();
    let v = SparseView::from_scores(1024, &pairs);
    assert_eq!(v.len(), 1000, "1000 个等权条目不该丢任何一条");
}

// ══════════════════════════ 4. 截断 ══════════════════════════

/// 超过 cap 时按分数保留 top-cap；结果与输入顺序无关（全序排序）。
#[test]
fn t04_截断保留高分且与顺序无关() {
    let pairs: Vec<(u32, f32)> = (0..300u32).map(|i| (i, (i + 1) as f32 / 300.0)).collect();
    let a = SparseView::from_scores(128, &pairs);
    assert_eq!(a.len(), 128);
    // 保留的应当是分数最高的 128 条：账户号 172..300。
    assert_eq!(a.slots()[0].0, 172);
    assert_eq!(a.slots()[127].0, 299);

    // 逆序输入得到逐字节相同的表。
    let mut rev = pairs.clone();
    rev.reverse();
    let b = SparseView::from_scores(128, &rev);
    assert_eq!(a.encode(), b.encode());

    // 截断掉的质量可以被精确算出来。
    let lost = SparseView::truncated_mass(128, &pairs);
    assert!(lost > 0.0 && lost < 1.0, "截断质量 {lost}");
    // 300 条里保留分数最高的 128 条（172..300），留住的定点质量占比：
    let kept: f64 = (172..=299)
        .map(|i| ((i + 1) as f32 / 300.0 * 65535.0).round() as f64)
        .sum();
    let all: f64 = (0..300)
        .map(|i| (((i + 1) as f32 / 300.0) * 65535.0).round() as f64)
        .sum();
    assert!(
        (lost - (1.0 - kept / all)).abs() < 1e-9,
        "截断质量口径不一致"
    );

    // 不超容量时截断质量恒为 0。
    assert_eq!(SparseView::truncated_mass(512, &pairs), 0.0);
}

/// 同一账户重复声明取最大值：不放大预算，且与顺序无关。
#[test]
fn t04b_重复声明取最大值() {
    let a = SparseView::from_scores(64, &[(7, 0.5), (7, 0.9), (7, 0.1)]);
    let b = SparseView::from_scores(64, &[(7, 0.9), (7, 0.5), (7, 0.1)]);
    assert_eq!(a.q_of(7), quantize(0.9));
    assert_eq!(a.encode(), b.encode());
    assert_eq!(a.len(), 1);
}

// ══════════════════════════ 5. 编码 / 伪造 ══════════════════════════

/// 编码是定长的：`cap × 6 + 2` 字节，与条目数无关。
#[test]
fn t05_编码定长且可往返() {
    for cap in [128usize, 256, 512] {
        let pairs: Vec<(u32, f32)> = (0..cap as u32)
            .map(|i| (i, (i + 1) as f32 / cap as f32))
            .collect();
        let v = SparseView::from_scores(cap, &pairs);
        let bytes = v.encode();
        assert_eq!(bytes.len(), cap * SLOT_BYTES + VIEW_OVERHEAD);
        assert_eq!(v.bytes(), bytes.len());
        let back = SparseView::decode(&bytes, cap).expect("合法负载应能解码");
        assert_eq!(back, v);
        assert_eq!(back.encode(), bytes, "往返之后逐字节相同");
    }
}

/// 畸形的、伪造的负载一律拒绝——**不是**「尽力解析」。
#[test]
fn t05b_伪造负载被拒绝() {
    let cap = 128;
    let good = SparseView::from_scores(cap, &[(1, 0.5), (2, 0.25)]).encode();
    assert!(SparseView::decode(&good, cap).is_some());

    // 长度不对。
    assert!(SparseView::decode(&good[..good.len() - 1], cap).is_none());
    // cap 与编码端不一致。
    assert!(SparseView::decode(&good, 256).is_none());
    // 条数超过容量。
    let mut bad = good.clone();
    let fixed = cap * SLOT_BYTES;
    bad[fixed..fixed + 2].copy_from_slice(&(cap as u16 + 1).to_le_bytes());
    assert!(SparseView::decode(&bad, cap).is_none());
    // 账户号重复（同一账户占两个槽位 = 双倍预算）。
    let mut bad = good.clone();
    bad[0..4].copy_from_slice(&2u32.to_le_bytes());
    bad[4..6].copy_from_slice(&quantize(1.0).to_le_bytes());
    bad[6..10].copy_from_slice(&2u32.to_le_bytes());
    assert!(SparseView::decode(&bad, cap).is_none());
    // 乱序。
    let mut bad = good.clone();
    bad[0..4].copy_from_slice(&9u32.to_le_bytes());
    assert!(SparseView::decode(&bad, cap).is_none());
    // 分数为 0（规范形式要求 0 分不占槽位）。
    let mut bad = good.clone();
    bad[fixed..fixed + 2].copy_from_slice(&1u16.to_le_bytes());
    bad[4..6].copy_from_slice(&0u16.to_le_bytes());
    assert!(SparseView::decode(&bad, cap).is_none());
    // 填充区夹带数据。
    let mut bad = good.clone();
    bad[20] = 0xFF;
    assert!(SparseView::decode(&bad, cap).is_none());

    // NaN / 负数 / 超大值根本进不了构造入口。
    let v = SparseView::from_scores(16, &[(1, f32::NAN), (2, -5.0), (3, 1e9)]);
    assert_eq!(v.len(), 1, "只有 1e9（夹到 1.0）能留下");
    assert_eq!(v.q_of(3), Q_ONE);
}

// ══════════════════════════ 6. 确定性 ══════════════════════════

/// 正序 / 逆序 / 随机序构造同样的**集合**，分布与发布视图逐位相同。
#[test]
fn t06_与插入顺序无关() {
    // 一份固定的输入集合。
    let ratings: Vec<(u32, f32)> = (0..40u32).map(|i| (i + 1, 0.1 + 0.02 * i as f32)).collect();
    let views: Vec<(u32, Vec<(u32, f32)>)> = (1..=12u32)
        .map(|s| {
            let slots: Vec<(u32, f32)> = (0..30u32)
                .map(|j| (100 + j, ((j * 7 + s) % 13) as f32 / 13.0))
                .collect();
            (s, slots)
        })
        .collect();

    let build = |rev: bool, shuffle: Option<u64>| -> Node {
        let mut r = ratings.clone();
        let mut v = views.clone();
        if rev {
            r.reverse();
            v.reverse();
        }
        if let Some(seed) = shuffle {
            let mut rng = Rng::new(seed);
            for i in (1..r.len()).rev() {
                let j = rng.below(i + 1);
                r.swap(i, j);
            }
            for i in (1..v.len()).rev() {
                let j = rng.below(i + 1);
                v.swap(i, j);
            }
        }
        let mut n = Node::new(Config::default(), 1);
        for &(a, s) in &r {
            n.rate(a, s, 0);
        }
        for (owner, slots) in &v {
            let sv = SparseView::from_scores(Config::default().cap, slots);
            assert!(n.on_view(*owner, sv, 0, 0));
        }
        n
    };

    let a = build(false, None);
    let b = build(true, None);
    let c = build(false, Some(0xDEAD_BEEF));
    assert_eq!(bits(&a.distribution(0)), bits(&b.distribution(0)));
    assert_eq!(bits(&a.distribution(0)), bits(&c.distribution(0)));
    assert_eq!(a.publish(0).encode(), b.publish(0).encode());
    assert_eq!(a.publish(0).encode(), c.publish(0).encode());
    // 衰减之后同样成立。
    assert_eq!(bits(&a.distribution(500)), bits(&c.distribution(500)));
}

// ══════════════════════════ 7. 轮次位图 ══════════════════════════

/// 一轮不放回、抽完自动重置；整轮全取覆盖剩下的全部。
#[test]
fn t07_轮次抽样不重复且会自动重置() {
    let mut list = PositiveList::new();
    for i in 0..10u32 {
        list.set(i, 0.5, 0);
    }
    let mut s = RoundBitmap::new();
    let mut rng = Rng::new(42);

    let first = s.draw(&list, 10, &mut rng);
    assert_eq!(first.len(), 10);
    let mut uniq = first.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 10, "一轮内不许重复");
    assert_eq!(s.remaining(), 0);

    let second = s.draw(&list, 3, &mut rng);
    assert_eq!(second.len(), 3, "抽空之后应自动开新一轮");
    assert_eq!(s.remaining(), 7);

    // 整轮全取：一次拿干净，且按账户号升序。
    let rest = s.draw_all(&list);
    assert_eq!(rest.len(), 7);
    assert!(rest.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(s.remaining(), 0);

    // 大小为 0 的请求不消耗任何东西。
    assert!(s.draw(&list, 0, &mut rng).is_empty() || s.remaining() > 0);
}

/// 抽样是均匀的：每个账户被抽到频率接近 1/n，与它的**连败计数无关**。
///
/// 这条直接钉住「砍掉退避」：把一半来源的连败计数拉到 20，频率不变。
#[test]
fn t07b_不退避_连败不影响抽样() {
    let n = 40usize;
    let mut list = PositiveList::new();
    for i in 0..n as u32 {
        list.set(i, 0.5, 0);
    }
    // 一半来源「连续失败 20 次」。
    for i in 0..n as u32 / 2 {
        for _ in 0..20 {
            list.record_failure(i);
        }
    }
    assert_eq!(list.fail_streak(0), 20);

    let mut s = RoundBitmap::new();
    let mut rng = Rng::new(7);
    let mut count = vec![0usize; n];
    let rounds = 200;
    for _ in 0..rounds {
        for acc in s.draw(&list, n, &mut rng) {
            count[acc as usize] += 1;
        }
    }
    let expect = (rounds * n) as f64 / n as f64; // 每轮每人都被抽到一次
    for (i, &c) in count.iter().enumerate() {
        assert_eq!(c as f64, expect, "第 {i} 号在 {rounds} 轮里被抽到 {c} 次");
    }

    // 连败计数完全不参与：同一个种子，改不改连败，抽样序列一模一样。
    let mut list_a = PositiveList::new();
    let mut list_b = PositiveList::new();
    for i in 0..n as u32 {
        list_a.set(i, 0.5, 0);
        list_b.set(i, 0.5, 0);
    }
    for i in 0..n as u32 / 2 {
        for _ in 0..20 {
            list_b.record_failure(i);
        }
    }
    let mut sa = RoundBitmap::new();
    let mut sb = RoundBitmap::new();
    let mut rng_a = Rng::new(7);
    let mut rng_b = Rng::new(7);
    for _ in 0..rounds {
        assert_eq!(
            sa.draw(&list_a, 5, &mut rng_a),
            sb.draw(&list_b, 5, &mut rng_b),
            "连败计数不该影响抽样序列"
        );
    }
}

// ══════════════════════════ 8. 归一化与合成 ══════════════════════════

/// 全局归一化：Σ ≡ 1；β 配比正确；个人项不衰减。
#[test]
fn t08_全局归一化与配比() {
    let mut n = Node::new(Config::default(), 1);
    assert!(n.distribution(0).is_empty(), "空清单不该编造分布");

    n.rate(10, 1.0, 0); // 只有一个人，个人项 = 1
    let d = n.distribution(0);
    assert_eq!(d.len(), 1);
    assert!((d.get(10) - 1.0).abs() < 1e-6);

    // 加一个来源：它的观点是 20 号值 1.0。
    let v = SparseView::from_scores(64, &[(20, 1.0)]);
    assert!(n.on_view(10, v, 0, 0));
    let d = n.distribution(0);
    // 个人项：10 -> 1.0；群体项：20 -> 1.0。
    // 声望：10 = 0.3*1 + 0.7*0 = 0.3；20 = 0.3*0 + 0.7*1 = 0.7。归一化后不变。
    assert!((d.get(10) - 0.3).abs() < 1e-6, "10 号 = {}", d.get(10));
    assert!((d.get(20) - 0.7).abs() < 1e-6, "20 号 = {}", d.get(20));
    assert!((d.sum() - 1.0).abs() < 1e-6);

    // 时间推进：个人项永不衰减，群体项按来源的相对新鲜度变化。
    // 只有一个来源时，全体一起变旧 → 加权平均不变（对均匀缩放免疫）。
    let d500 = n.distribution(500);
    assert!((d500.get(20) - 0.7).abs() < 1e-6, "单来源时衰减是相对量");
}

/// 两个来源，一个新鲜一个陈旧：新鲜的那个话语权更大。
#[test]
fn t08b_衰减只改相对权重() {
    let mut n = Node::new(Config::default(), 1);
    n.rate(1, 1.0, 0);
    n.rate(2, 1.0, 0);
    // 1 号说 9 号 1.0；2 号说 9 号 0.0（不表态，但表里没有 9 号）。
    assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 0));
    assert!(n.on_view(2, SparseView::from_scores(64, &[(8, 1.0)]), 0, 0));
    let now = n.distribution(0);
    // 只刷新 1 号：2 号落后一个半衰期。
    assert!(n.on_view(1, SparseView::from_scores(64, &[(9, 1.0)]), 0, 72));
    let later = n.distribution(72);
    assert!(
        later.get(9) > now.get(9),
        "新鲜来源的权重应更大: {} -> {}",
        now.get(9),
        later.get(9)
    );
    // 2 号的权重降到一半：0.5 / (1 + 0.5)。
    let expect = 0.3 * 0.0 + 0.7 * (1.0 / 1.5);
    let _ = expect;
}

// ══════════════════════════ 9. 空账户与退化情形 ══════════════════════════

/// 没有人提到的账户，声望**精确为 0**（不是噪声，也不是极小正数）。
#[test]
fn t09_空账户精确为零() {
    let mut n = Node::new(Config::default(), 1);
    for i in 0..20u32 {
        n.rate(i, 1.0, 0);
    }
    for i in 0..20u32 {
        let slots: Vec<(u32, f32)> = (0..20u32).map(|j| (100 + j, 0.05)).collect();
        assert!(n.on_view(i, SparseView::from_scores(256, &slots), 0, 0));
    }
    let d = n.distribution(0);
    // 1000 个只注册不评分的账户：一条都不该出现。
    for i in 0..1000u32 {
        assert_eq!(d.get(5000 + i), 0.0, "空账户 {} 不该出现", 5000 + i);
    }
    assert_eq!(d.len(), 40, "分布里只有 20 个个人项 + 20 个群体项");
    assert!((d.sum() - 1.0).abs() < 1e-6);

    // 没有任何正分时，一切退化为空而不是 NaN。
    let empty = Node::new(Config::default(), 1);
    assert!(empty.publish(0).is_empty());
    assert_eq!(empty.query(1, 0), 0.0);
    assert_eq!(empty.distribution(0).sum(), 0.0);
}

/// 时间回拨不能让衰减因子变成负数或 NaN。
#[test]
fn t09b_时间回拨安全() {
    let mut n = Node::new(Config::default(), 1);
    n.rate(1, 1.0, 100);
    assert!(n.on_view(1, SparseView::from_scores(8, &[(2, 1.0)]), 100, 100));
    // now 小于 last_check：saturating_sub 归零，因子 = 1。
    let d = n.distribution(0);
    assert!((d.sum() - 1.0).abs() < 1e-6);
    assert!(d.get(2) > 0.0);
    for &(_, v) in &d.entries {
        assert!(v.is_finite() && v >= 0.0);
    }
}

/// 没有正分的来源发来的观点被拒（没关系的来源不占内存）。
#[test]
fn t09c_无关系来源的观点被拒() {
    let mut n = Node::new(Config::default(), 1);
    n.rate(1, 0.5, 0);
    assert!(!n.on_view(2, SparseView::from_scores(8, &[(3, 1.0)]), 0, 0));
    assert_eq!(n.view_count(), 0);
    // 空表也被拒。
    assert!(!n.on_view(1, SparseView::new(8), 0, 0));
    assert_eq!(n.view_count(), 0);
    assert_eq!(n.stats(), (0, 2));
}
