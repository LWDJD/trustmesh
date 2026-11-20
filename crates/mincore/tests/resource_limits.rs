//! 资源上限的**集成**断言（`Config::max_views` / `Config::max_list`）。
//!
//! 单元测试在 `src/node.rs` / `src/list.rs` 里逐条钉住语义；这个文件从**外部**
//! 视角再钉三类东西：
//!
//! 1. **默认值**是有限上限（4096 / 4096），而不是 `0`（无上限）；
//! 2. **超限策略是「拒绝新增」**：已缓存的 owner 永远接受、被拒的写入一个字段都不写；
//! 3. **逐位确定性**：同样的输入，两遍完全相同；且「上限够大」与「无上限」逐位相同。
//!
//! 为什么单独一个文件而不是塞进 `tests/e2e_asserts.rs`：那是 A12 的 S1~S6
//! 端到端判据（跑整张仿真图，秒级），这里的断言全是 O(上限) 的构造性检查，
//! 与图形状无关，混进去会让那个文件既慢又难读。

use tm_mincore::{Config, Node, SparseView};

// ══════════════════════════ 小工具 ══════════════════════════

/// 只改 max_views。
fn cfg_views(max_views: usize) -> Config {
    Config {
        max_views,
        ..Config::default()
    }
}

/// 只改 max_list。
fn cfg_list(max_list: usize) -> Config {
    Config {
        max_list,
        ..Config::default()
    }
}

/// 逐位可比较的分布快照：`(账户, 声望位模式)` + mass 的位模式。
fn bits(n: &Node, now: u64) -> (Vec<(u32, u32)>, u64) {
    let d = n.distribution(now);
    (
        d.entries.iter().map(|&(a, v)| (a, v.to_bits())).collect(),
        d.mass.to_bits(),
    )
}

// ══════════════════════════ 1. 默认值 ══════════════════════════

/// 守：默认上限是 **4096 / 4096**（有限），不是 `0`（无上限）。
///
/// 为什么默认打开：`mincore` 是要能上生产的最小内核，内存必须先天有界；
/// 而 4096 对正常使用有 16~20 倍以上余量（A12 图单观察者清单实测
/// 均值 105.1 / 中位 101 / 最大 202，CI 小图 54~61），正常永远碰不到。
#[test]
fn t01_默认上限是有限值4096() {
    let c = Config::default();
    assert_eq!(c.max_views, 4096);
    assert_eq!(c.max_list, 4096);
    assert_ne!(c.max_views, 0, "默认不许是「无上限」");
    assert_ne!(c.max_list, 0);
    // Node 的清单上限跟着 Config 走。
    let n = Node::new(Config::default(), 1);
    assert_eq!(n.claims().max_entries(), 4096);
    // 0 仍然可以显式传：那是「无上限」，判定分支不触发。
    let unlimited = Node::new(cfg_views(0), 1);
    assert_eq!(unlimited.claims().max_entries(), Config::default().max_list);
}

// ══════════════════════════ 2. 视图上限：拒绝新增 ══════════════════════════

/// 守：`max_views = 0` = 无上限；`max_views = n` 恰好收 n 份、第 n+1 份被拒。
#[test]
fn t02_视图上限的边界与0语义() {
    // 0 = 无上限：200 份全进。
    let mut unlimited = Node::new(cfg_views(0), 1);
    let view = SparseView::from_scores(16, &[(9, 1.0)]);
    for i in 0..200u32 {
        assert!(unlimited.try_rate(i, 0.5, 0));
        assert!(unlimited.on_view(i, view.clone(), 0, 0));
    }
    assert_eq!(unlimited.view_count(), 200);
    assert_eq!(unlimited.stat_rejected_limit(), 0);

    // n = 3：恰好满 / 多一条。
    let mut limited = Node::new(cfg_views(3), 1);
    for i in 0..5u32 {
        assert!(limited.try_rate(i, 0.5, 0));
    }
    for i in 0..3u32 {
        assert!(limited.on_view(i, view.clone(), 0, 0), "第 {i} 份");
    }
    assert_eq!(limited.view_count(), 3);
    assert!(!limited.on_view(3, view.clone(), 0, 0), "第 4 份必须被拒");
    assert!(!limited.on_view(4, view.clone(), 0, 0), "第 5 份必须被拒");
    assert_eq!(limited.view_count(), 3, "被拒的不占内存");
    assert_eq!(limited.stat_rejected_limit(), 2);
    assert_eq!(limited.stats(), (3, 2), "容量拒绝计入 stats 的被拒项");
}

/// 守：**已缓存的 owner 在满时仍然接受**（原地覆盖不增长内存）——
/// 这是「拒绝新增」与 LRU 的关键差别：丢刷新没有任何资源收益。
#[test]
fn t03_已缓存owner满时仍接受() {
    let mut n = Node::new(cfg_views(2), 1);
    assert!(n.try_rate(1, 0.5, 0) && n.try_rate(2, 0.5, 0));
    assert!(n.on_view(1, SparseView::from_scores(16, &[(30, 1.0)]), 0, 0));
    assert!(n.on_view(2, SparseView::from_scores(16, &[(31, 1.0)]), 0, 0));
    assert_eq!(n.view_count(), 2, "恰好满");
    // 反复刷新已缓存的两个 owner：永远接受、永远不增长内存。
    for round in 0..10u64 {
        let v1 = SparseView::from_scores(16, &[(30 + (round as u32 % 2), 1.0)]);
        assert!(n.on_view(1, v1, 0, round), "第 {round} 次刷新 1 号必须接受");
        assert!(
            n.on_view(2, SparseView::from_scores(16, &[(31, 1.0)]), 0, round),
            "第 {round} 次刷新 2 号必须接受"
        );
        assert_eq!(n.view_count(), 2, "刷新不增长缓存");
    }
    assert_eq!(n.stat_rejected_limit(), 0, "刷新不是容量拒绝");
    assert_eq!(n.stats(), (22, 0), "20 次刷新 + 2 次首次接收，全部成功");
    // 新 owner 依然进不来。
    n.rate(3, 0.5, 0);
    assert!(!n.on_view(3, SparseView::from_scores(16, &[(32, 1.0)]), 0, 10));
    assert_eq!(n.stat_rejected_limit(), 1);
}

/// 守：容量上限**不破坏 \(\Sigma\equiv 1\)**，被拒的来源在分布里精确为 0。
#[test]
fn t04_容量上限不破坏归一化() {
    let mut n = Node::new(cfg_views(3), 1);
    for i in 0..10u32 {
        n.rate(i, 0.5, 0);
    }
    for i in 0..10u32 {
        let accepted = n.on_view(i, SparseView::from_scores(16, &[(100 + i, 1.0)]), 0, 0);
        assert_eq!(accepted, i < 3, "第 {i} 份的接受性");
    }
    let d = n.distribution(0);
    assert!((d.sum() - 1.0).abs() < 1e-6, "Σ = {}", d.sum());
    assert_eq!(d.len(), 13, "10 个个人项 + 3 个群体项");
    for i in 0..3u32 {
        assert!(d.get(100 + i) > 0.0, "被接收的来源 {i} 背书有效");
    }
    for i in 3..10u32 {
        assert_eq!(d.get(100 + i), 0.0, "被拒的来源 {} 必须精确为 0", 100 + i);
    }
    assert_eq!(n.stat_rejected_limit(), 7);
}

// ══════════════════════════ 3. 清单上限 ══════════════════════════

/// 守：清单上限只拦**新增**；改分与撤销永远接受；撤销腾出的名额可以再用。
#[test]
fn t05_清单上限只拦新增() {
    let mut n = Node::new(cfg_list(3), 1);
    assert_eq!(n.claims().max_entries(), 3);
    for a in [1u32, 2, 3] {
        assert!(n.try_rate(a, 0.5, 0));
    }
    assert_eq!(n.claims().len(), 3);
    // ① 改分：永远接受。
    for a in [1u32, 2, 3] {
        assert!(n.try_rate(a, 0.9, 1), "改分不是新增");
    }
    assert_eq!(n.claims().len(), 3);
    assert_eq!(n.stat_rejected_list(), 0);
    // ② 新账户：被拒，且什么都不写。
    assert!(!n.try_rate(4, 1.0, 2), "清单满了，新账户必须被拒");
    assert_eq!(n.claims().len(), 3);
    assert_eq!(n.stat_rejected_list(), 1);
    assert!(!n.claims().has(4));
    // ③ 撤销：永远接受，并且腾出名额。
    assert!(n.try_rate(2, 0.0, 3), "撤销永远接受");
    assert_eq!(n.claims().len(), 2);
    assert!(n.try_rate(5, 0.5, 4), "腾出的名额必须能用");
    assert_eq!(n.claims().len(), 3);
    assert_eq!(n.stat_rejected_list(), 1, "撤销 / 改分 / 腾名额都不计数");
    // ④ rate 走同一条路径：静默，但可观测。
    n.rate(6, 0.5, 5);
    assert_eq!(n.claims().len(), 3, "rate 静默丢弃");
    assert_eq!(n.stat_rejected_list(), 2);
}

/// 守：视图上限与清单上限**互相独立**——只开清单上限不挡观点，反之亦然。
#[test]
fn t06_两个上限互相独立() {
    // 只开清单上限：视图没有上限。
    let mut list_only = Node::new(cfg_list(2), 1);
    let view = SparseView::from_scores(16, &[(9, 1.0)]);
    for i in 0..2u32 {
        list_only.rate(i, 0.5, 0);
        assert!(list_only.on_view(i, view.clone(), 0, 0));
    }
    list_only.rate(2, 0.5, 0); // 被清单上限拒
    assert_eq!(list_only.stat_rejected_list(), 1);
    assert_eq!(list_only.view_count(), 2);
    assert_eq!(
        list_only.stat_rejected_limit(),
        0,
        "视图上限没开，不该有容量拒绝"
    );

    // 只开视图上限：清单没有上限。
    let mut views_only = Node::new(cfg_views(1), 1);
    for i in 0..10u32 {
        assert!(views_only.try_rate(i, 0.5, 0), "清单无上限，10 条都进");
    }
    for i in 0..10u32 {
        let _ = views_only.on_view(i, view.clone(), 0, 0);
    }
    assert_eq!(views_only.claims().len(), 10);
    assert_eq!(views_only.view_count(), 1);
    assert_eq!(views_only.stat_rejected_limit(), 9);
    assert_eq!(
        views_only.stat_rejected_list(),
        0,
        "清单上限没开，不该有拒绝"
    );
}

// ══════════════════════════ 4. 确定性 ══════════════════════════

/// 守：同输入两遍**逐位相同**（含统计与两类拒绝计数）。
#[test]
fn t07_同输入两遍逐位相同() {
    let run = || {
        let mut n = Node::new(
            Config {
                max_views: 3,
                max_list: 5,
                ..Config::default()
            },
            2026,
        );
        for i in 0..12u32 {
            let _ = n.try_rate(i, 0.05 + (i as f32) * 0.07, 0);
        }
        for i in 0..12u32 {
            let slots: Vec<(u32, f32)> = (0..6u32)
                .map(|j| (300 + j, ((i + j) % 7 + 1) as f32))
                .collect();
            let _ = n.on_view(i, SparseView::from_scores(32, &slots), 0, u64::from(i));
        }
        (
            bits(&n, 12),
            n.stats(),
            n.stat_rejected_limit(),
            n.stat_rejected_list(),
            n.view_count(),
            n.claims().len(),
            n.claims().version(),
        )
    };
    let a = run();
    let b = run();
    assert_eq!(a, b, "同一输入两遍必须逐位相同");
    assert_eq!(a.4, 3, "视图被钉在 3");
    assert_eq!(a.5, 5, "清单被钉在 5");
}

/// 守：**上限够大**与**无上限**逐位相同——容量判定不参与任何计算。
#[test]
fn t08_上限够大时与无上限逐位相同() {
    let build = |max_views: usize, max_list: usize| {
        let mut n = Node::new(
            Config {
                max_views,
                max_list,
                ..Config::default()
            },
            7,
        );
        for i in 0..20u32 {
            n.rate(i, 0.1 + (i as f32) * 0.04, 0);
        }
        for i in 0..20u32 {
            let slots: Vec<(u32, f32)> = (0..20u32)
                .map(|j| (100 + j, ((i + j) % 7 + 1) as f32))
                .collect();
            assert!(n.on_view(i, SparseView::from_scores(64, &slots), 0, 0));
        }
        bits(&n, 0)
    };
    let limited = build(4096, 4096);
    let unlimited = build(0, 0);
    assert_eq!(limited, unlimited, "上限够大时必须与无上限逐位相同");
}

/// 守：容量上限**不改变已经缓存下来的那部分结果**——把同一批观点喂给
/// 「上限刚好够」和「无上限」两个节点，分布逐位相同。
#[test]
fn t09_上限不改变已有缓存的计算结果() {
    let build = |max_views: usize| {
        let mut n = Node::new(cfg_views(max_views), 1);
        for i in 0..6u32 {
            n.rate(i, 0.5, 0);
        }
        for i in 0..6u32 {
            let slots: Vec<(u32, f32)> = (0..5u32)
                .map(|j| (50 + j, ((i + j) % 6 + 1) as f32))
                .collect();
            assert!(n.on_view(i, SparseView::from_scores(16, &slots), 0, 0));
        }
        assert_eq!(n.view_count(), 6);
        bits(&n, 0)
    };
    assert_eq!(build(6), build(0), "上限刚好够时结果必须与无上限逐位相同");
}

// ══════════════════════════ 5. 统计口径 ══════════════════════════

/// 守：`stat_rejected_limit() <= stats().1` 恒成立；
/// 两类拒绝计数都单调递增；`stats()` 的二元组形状不变。
#[test]
fn t10_统计口径的不变式与单调性() {
    let mut n = Node::new(
        Config {
            max_views: 2,
            max_list: 2,
            ..Config::default()
        },
        1,
    );
    assert_eq!(n.stats(), (0, 0));
    assert_eq!(n.stat_rejected_limit(), 0);
    assert_eq!(n.stat_rejected_list(), 0);
    let view = SparseView::from_scores(16, &[(99, 1.0)]);
    // 清单上限 2：账户 0、1 能进，2 号开始被拒。
    for i in 0..10u32 {
        let ok = n.try_rate(i, 0.5, 0);
        assert_eq!(ok, i < 2, "第 {i} 次 rate 的接受性");
        if !ok {
            assert!(!n.claims().has(i), "被拒的账户不该在清单里");
        }
    }
    assert_eq!(n.stat_rejected_list(), 8);
    // 视图上限 2：0、1 号能进，其余「没评过」走结构性拒绝 —— 所以这里
    // 只对已评过的 0/1 再刷一遍，验证「刷新永远接受」。
    assert!(n.on_view(0, view.clone(), 0, 0));
    assert!(n.on_view(1, view.clone(), 0, 0));
    assert_eq!(n.view_count(), 2);
    assert_eq!(n.stats(), (2, 0));
    // 结构性拒绝与容量拒绝分别累计，且不变式成立。
    for i in 2..10u32 {
        assert!(!n.on_view(i, view.clone(), 0, 0), "没评过 → 结构性拒绝");
    }
    let (succ, rej) = n.stats();
    assert_eq!(succ, 2);
    assert_eq!(rej, 8);
    assert_eq!(
        n.stat_rejected_limit(),
        0,
        "这 8 次是结构性拒绝，不是容量拒绝"
    );
    assert!(n.stat_rejected_limit() <= rej, "不变式：容量拒绝 ⊆ 总拒绝");
}

/// 守：两类拒绝计数**只增不减**——删除条目、缩小上限、清空视图缓冲都别想让它们回退。
#[test]
fn t11_拒绝计数单调不回退() {
    let mut n = Node::new(cfg_list(1), 1);
    assert!(n.try_rate(1, 0.5, 0));
    for _ in 0..3 {
        assert!(!n.try_rate(2, 0.5, 0));
    }
    let before = n.stat_rejected_list();
    assert_eq!(before, 3);
    // 撤销原条目（清单变空）→ 计数不回退。
    assert!(n.try_rate(1, 0.0, 1));
    assert_eq!(n.claims().len(), 0);
    assert_eq!(n.stat_rejected_list(), before, "腾空清单不该让计数回退");
    // 新条目进来（现在有位置了）→ 仍然不回退。
    assert!(n.try_rate(3, 0.5, 2));
    assert_eq!(n.stat_rejected_list(), before);
    // 视图那边同理：拒绝只增不减。
    let mut m = Node::new(cfg_views(1), 1);
    let view = SparseView::from_scores(16, &[(7, 1.0)]);
    m.rate(1, 0.5, 0);
    assert!(m.on_view(1, view.clone(), 0, 0));
    m.rate(2, 0.5, 0);
    for _ in 0..4 {
        assert!(!m.on_view(2, view.clone(), 0, 0));
    }
    let cap_before = m.stat_rejected_limit();
    assert_eq!(cap_before, 4);
    assert!(
        m.on_view(1, view.clone(), 0, 1),
        "刷新已缓存 owner 永远接受"
    );
    assert_eq!(m.stat_rejected_limit(), cap_before, "刷新不该改变拒绝计数");
}

// ══════════════════════════ 6. 内存上界的形状 ══════════════════════════

/// 守：**视图内存上界 = max_views × 每份的绝对上界**，且每份的绝对上界是
/// `cap×6 + 2`（序列化口径）——这是「条目数上限即内存上限」的依据。
///
/// 本测试只做**算术上的**验证（不测量 RSS）：它钉住的是「上限 × 单份上界」
/// 这个乘积不会随喂入次数增长，与实现里存了多少是两件事。
#[test]
fn t12_内存上界只由上限与cap决定() {
    let cap = 512usize;
    let per_view = cap * 6 + 2;
    let max_views = 4096usize;
    let upper = max_views * per_view;
    assert_eq!(per_view, 3074, "cap = 512 时单份的序列化长度");
    assert_eq!(upper, 12_591_104, "4096 × 3074 = 12.6 MB 绝对上界");
    // 喂多少份都不改变这个上界（它是上限 × 常数，与喂入次数无关）。
    let mut n = Node::new(cfg_views(max_views), 1);
    let view = SparseView::from_scores(cap, &[(1, 1.0)]);
    for i in 0..20_000u32 {
        if i as usize >= max_views {
            let _ = n.try_rate(i - max_views as u32, 0.0, 0);
        }
        let _ = n.try_rate(i, 0.5, 0);
        let _ = n.on_view(i, view.clone(), 0, 0);
    }
    assert_eq!(n.view_count(), max_views, "上限被顶满");
    assert!(
        n.view_count() <= max_views,
        "喂了 20000 份之后视图数依然 ≤ {max_views}"
    );
    assert_eq!(n.claims().len(), Config::default().max_list, "清单同样有界");
    assert_eq!(n.stat_rejected_limit(), 20_000 - 4096);
}
