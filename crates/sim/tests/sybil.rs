//! 女巫攻击的结构性约束。
//!
//! 核心命题：女巫集群**只跟自己人互评满分**，对集群外的人一个分都不打；
//! 同时正常人也不会主动发现女巫（没有背书就没有曝光）。
//! 这两条合起来，就是「女巫不渗透就没有任何存在感」。

use tm_sim::{AttackKind, Kind, SimConfig, World};

fn 造世界(cluster: usize, ratio: f64, seed: u64) -> World {
    World::new(SimConfig {
        n_initial: 120,
        steps: 80,
        seed,
        attack_kind: AttackKind::Sybil { cluster },
        attack_ratio: ratio,
        ..SimConfig::default()
    })
}

#[test]
fn 女巫只对同集群成员打满分() {
    let mut w = 造世界(3, 0.2, 5);
    w.run();

    let mut 女巫评分 = 0usize;
    for (s, u, score) in w.ratings() {
        if let Kind::Sybil { cluster } = w.accounts[s].kind {
            女巫评分 += 1;
            assert_eq!(score, 1.0, "女巫给分必须是满分");
            match w.accounts[u].kind {
                Kind::Sybil { cluster: c2 } => {
                    assert_eq!(c2, cluster, "女巫 {s} 给别的集群打了分");
                }
                other => panic!("女巫 {s} 给集群外的账户 {u}（{other:?}）打了分"),
            }
        }
    }
    assert!(女巫评分 > 0, "80 步内女巫应该至少互评过一次");
}

#[test]
fn 非女巫不会给女巫打分() {
    let mut w = 造世界(2, 0.2, 6);
    w.run();
    for (s, u, _) in w.ratings() {
        if matches!(w.accounts[u].kind, Kind::Sybil { .. }) {
            assert!(
                matches!(w.accounts[s].kind, Kind::Sybil { .. }),
                "诚实账户 {s} 主动给女巫 {u} 打了分——女巫不该出现在正常社交发现里"
            );
        }
    }
}

#[test]
fn 女巫内部均分是满分() {
    let mut w = 造世界(2, 0.2, 7);
    w.run();
    let 女巫: Vec<usize> = w
        .accounts
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sybil { .. }))
        .map(|a| a.idx)
        .collect();
    assert_eq!(女巫.len(), 24); // 120 × 0.2
    for idx in 女巫 {
        let a = &w.accounts[idx];
        if a.received_count > 0 {
            assert!(
                (a.mean_received() - 1.0).abs() < 1e-6,
                "女巫 {idx} 收到均分 {}，应为 1.0",
                a.mean_received()
            );
        }
    }
}

#[test]
fn 女巫集群规模与数量正确() {
    let w = 造世界(3, 0.2, 8);
    assert_eq!(w.attack.clusters.len(), 3);
    let total: usize = w.attack.clusters.iter().map(|c| c.len()).sum();
    assert_eq!(total, 24);
    // 集群之间不重叠、成员都是女巫
    for (i, group) in w.attack.clusters.iter().enumerate() {
        for &idx in group {
            assert!(matches!(w.accounts[idx].kind, Kind::Sybil { .. }));
            assert_eq!(w.attack.cluster_of(idx), Some(i));
        }
    }
}

#[test]
fn 女巫集群内部全连接且不越界() {
    let w = 造世界(2, 0.2, 9);
    for group in &w.attack.clusters {
        for &s in group {
            for &u in group {
                if s != u {
                    assert!(w.accounts[s].follows.contains(&u), "集群内部没有接满");
                }
            }
            for &u in &w.accounts[s].follows {
                assert!(group.contains(&u), "女巫 {s} 关注了集群外的 {u}");
            }
        }
    }
}

#[test]
fn 攻击比例为零时没有女巫() {
    let w = 造世界(2, 0.0, 10);
    assert!(w.attack.clusters.is_empty());
    assert!(w.attack.attackers.is_empty());
    assert!(w.attack.target.is_none());
    assert!(w
        .accounts
        .iter()
        .all(|a| !matches!(a.kind, Kind::Sybil { .. })));
}

#[test]
fn 女巫比例越高集群越大() {
    let small = 造世界(2, 0.05, 11);
    let big = 造世界(2, 0.3, 11);
    let n_small: usize = small.attack.clusters.iter().map(|c| c.len()).sum();
    let n_big: usize = big.attack.clusters.iter().map(|c| c.len()).sum();
    assert_eq!(n_small, 6);
    assert_eq!(n_big, 36);
}
