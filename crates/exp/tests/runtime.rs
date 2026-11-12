//! runtime 级测试：每个账户一个真实的 `tm_engine::Engine`，走真实接口
//! （`rate` / `tick` / `export_view` / `on_view` / `trust_distribution`）。
//!
//! 这里断言的都是「真刀真枪」的性质，不是集中式近似。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

fn key(i: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i;
    k[1] = 0xA5;
    k
}

fn eng() -> Engine {
    Engine::new(EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams { buckets: 512, hashers: 5, seed: 7 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        coverage_exponent: 1.0,
        remove_when_coverage: 0.0,
        group_half_life: 0,
        unreachable_after: 0,
        min_group_sources: 0,
        ..Default::default()
    })
}

fn sum_of(dist: &[(u32, f32)]) -> f32 {
    dist.iter().map(|(_, v)| *v).sum()
}

/// ① 最终信任分布的总和恒为 1
#[test]
fn distribution_sums_to_one() {
    let mut e = eng();
    let a = e.intern(key(1));
    let b = e.intern(key(2));
    let c = e.intern(key(3));
    e.rate(a, 3.0, 0);
    e.rate(b, 1.0, 0);
    e.rate(c, 2.0, 0);

    let d = e.trust_distribution(0);
    assert_eq!(d.len(), 3, "三个评级对象都应出现");
    let s = sum_of(&d);
    assert!((s - 1.0).abs() < 1e-4, "分布总和应为 1，实际 {s}");
    for (_, v) in &d {
        assert!((0.0..=1.0).contains(v), "份额越界: {v}");
    }
}

/// ② 没给任何人评过分 → 空分布（不是全 0 的分布）
#[test]
fn empty_engine_has_empty_distribution() {
    let e = eng();
    assert!(e.trust_distribution(0).is_empty());
    assert!(e.shares().is_empty());
}

/// ③ 导出观点文件是归一化的：对全部已知账户查询后求和应接近 1
#[test]
fn export_view_is_normalized() {
    let mut e = eng();
    for i in 1..=5u8 {
        let id = e.intern(key(i));
        e.rate(id, i as f32, 0);
    }
    let v = e.export_view(0);
    let total: f32 = (1..=5u8).map(|i| v.query(&key(i)).max(0.0)).sum();
    assert!(
        (0.5..=1.5).contains(&total),
        "导出观点归一化后总和应接近 1（压缩有噪声），实际 {total}"
    );
}

/// ④ 多节点真实交换：3 轮之后每个节点的分布总和仍为 1
#[test]
fn multi_node_exchange_keeps_sum_one() {
    const N: usize = 6;
    let mut nodes: Vec<Engine> = (0..N)
        .map(|_| {
            let mut e = eng();
            for i in 0..N {
                e.intern(key(i as u8 + 1));
            }
            e
        })
        .collect();

    // 环形互评：i 给 (i+1)%N 打 1.0
    for i in 0..N {
        nodes[i].rate(((i + 1) % N) as u32, 1.0, 0);
    }

    for round in 0..3u64 {
        // 所有节点先导出观点快照
        let views: Vec<CompressedView> = nodes.iter().map(|e| e.export_view(round)).collect();
        // 再各自抽样并接收
        for i in 0..N {
            let reqs = nodes[i].tick(round, 4);
            assert!(!reqs.is_empty(), "节点 {i} 应有抽样请求");
            for r in reqs {
                let src = r.idx as usize;
                nodes[i].on_view(r.idx, views[src].clone(), round, round);
            }
        }
        // 每轮结束都校验守恒
        for (i, e) in nodes.iter().enumerate() {
            let s = sum_of(&e.trust_distribution(round));
            assert!((s - 1.0).abs() < 1e-3, "第 {round} 轮节点 {i} 总和 {s}");
        }
    }
}

/// ⑤ 没评过分的账户：可以经链条进入视野，但永远不进我的抽样池
#[test]
fn unrated_account_is_visible_but_never_sampled() {
    let mut e = eng();
    let a = e.intern(key(1));
    let ghost = e.intern(key(200));
    e.rate(a, 1.0, 0);

    // a 的观点抬举 ghost → ghost 通过链条进入我的群体项（这是设计允许的）
    let v = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(200), 1.0)],
    );
    e.on_view(a, v, 0, 0);

    // 但我从来没给 ghost 打过分 → 个人项必须为空
    assert!(e.share_of(ghost).is_none(), "未评分账户的个人项必须为空");

    // 抽一整个轮次，请求里绝不能出现 ghost
    let reqs = e.tick(0, 20);
    assert!(!reqs.is_empty());
    assert!(
        !reqs.iter().any(|r| r.idx == ghost),
        "未评分账户不应出现在抽样请求里"
    );
    assert!(
        reqs.iter().any(|r| r.idx == a),
        "我评过分的账户才应该被抽样"
    );
}

/// ⑥ 观点会随时间变旧
#[test]
fn view_age_grows_over_time() {
    let mut e = eng();
    let a = e.intern(key(1));
    e.rate(a, 1.0, 0);
    let v = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(1), 1.0)],
    );
    e.on_view(a, v, 0, 0);
    assert_eq!(e.view_age(a, 0), Some(0));
    assert_eq!(e.view_age(a, 240), Some(240));
    let other = e.intern(key(9));
    assert_eq!(e.view_age(other, 0), None, "没有观点应为 None");
}

/// ⑦ 所有来源一起变旧时，群体项结果不变（加权平均对均匀缩放免疫）
#[test]
fn uniform_staleness_does_not_change_result() {
    fn build() -> (Engine, u32) {
        let mut e = eng();
        let f1 = e.intern(key(1));
        let f2 = e.intern(key(2));
        let t = e.intern(key(9));
        e.rate(f1, 1.0, 0);
        e.rate(f2, 1.0, 0);
        let v1 = CompressedView::from_pairs(
            ViewParams { buckets: 512, hashers: 5, seed: 7 },
            &[(key(9), 1.0)],
        );
        let v2 = CompressedView::from_pairs(
            ViewParams { buckets: 512, hashers: 5, seed: 7 },
            &[(key(9), 0.0)],
        );
        e.on_view(f1, v1, 0, 0);
        e.on_view(f2, v2, 0, 0);
        (e, t)
    }
    let (e0, t0) = build();
    let q0 = e0.query(t0, 0);
    let (e1, t1) = build();
    let q1 = e1.query(t1, 72 * 20); // 20 个半衰期之后
    assert!(
        (q0 - q1).abs() < 1e-5,
        "全体一起变旧不应改变结果：{q0} vs {q1}"
    );
}

/// ⑧ 抽样是不放回的轮次制：一轮内不重复，抽完才重置
#[test]
fn sampling_is_round_based_without_replacement() {
    let mut e = eng();
    let ids: Vec<u32> = (1..=5).map(|i| e.intern(key(i))).collect();
    for id in &ids {
        e.rate(*id, 1.0, 0);
    }
    let mut seen = std::collections::BTreeSet::new();
    let reqs = e.tick(0, 5);
    assert_eq!(reqs.len(), 5, "一轮应能抽满 5 个");
    for r in &reqs {
        assert!(seen.insert(r.idx), "同一轮内出现重复: {}", r.idx);
    }
    // 再抽一次，说明这一轮已经抽完 → 应重新开始
    let more = e.tick(0, 5);
    assert_eq!(more.len(), 5, "抽完之后应重置并重新抽满");
}
