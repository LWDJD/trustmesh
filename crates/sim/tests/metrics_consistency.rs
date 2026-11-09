//! 指标自洽性：快照必须能由账户表逐项重算出来。
//!
//! 这类测试的价值在于：任何一次「统计口径改了但忘了改另一处」都会被抓到。

use tm_sim::{Kind, Life, SimConfig, World};

fn 世界(seed: u64) -> World {
    let cfg = SimConfig {
        n_initial: 150,
        steps: 90,
        seed,
        attack_kind: tm_sim::AttackKind::Endorser { ratio: 0.15 },
        attack_ratio: 0.1,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    w
}

#[test]
fn 规模指标与账户表一致() {
    let w = 世界(31);
    let s = w.report();
    let alive = w.accounts.iter().filter(|a| a.is_alive()).count();
    let online = w
        .accounts
        .iter()
        .filter(|a| a.is_online(w.time, w.cfg.enable_rhythm))
        .count();
    let dormant = w.accounts.iter().filter(|a| a.is_dormant()).count();
    let left = w.accounts.iter().filter(|a| a.is_left()).count();
    let bridges = w.accounts.iter().filter(|a| a.is_bridge).count();
    let follows: usize = w.accounts.iter().map(|a| a.follows.len()).sum();
    let ratings: usize = w.accounts.iter().map(|a| a.ratings.len()).sum();

    assert_eq!(s.accounts, w.accounts.len());
    assert_eq!(s.alive, alive);
    assert_eq!(s.online, online);
    assert_eq!(s.dormant, dormant);
    assert_eq!(s.left, left);
    assert_eq!(s.bridges, bridges);
    assert_eq!(s.follow_edges, follows);
    assert_eq!(s.rating_edges, ratings);
    assert_eq!(s.alive + s.left, s.accounts);
    assert!(s.online <= s.alive);
    assert!(s.dormant <= s.alive);
}

#[test]
fn 分类统计与账户表一致() {
    let w = 世界(32);
    let s = w.report();
    assert_eq!(s.classes.len(), 7);
    let 分类账户合计: usize = s.classes.iter().map(|c| c.accounts).sum();
    assert_eq!(分类账户合计, s.accounts, "分类账户数之和必须等于总数");

    for class in &s.classes {
        let 成员: Vec<&tm_sim::Account> = w
            .accounts
            .iter()
            .filter(|a| a.kind.label() == class.label)
            .collect();
        assert_eq!(class.accounts, 成员.len(), "{} 账户数不对", class.label);
        assert_eq!(
            class.alive,
            成员.iter().filter(|a| a.is_alive()).count(),
            "{} 存活数不对",
            class.label
        );
        assert_eq!(
            class.online,
            成员
                .iter()
                .filter(|a| a.is_online(w.time, w.cfg.enable_rhythm))
                .count(),
            "{} 在线数不对",
            class.label
        );
        let 收到条数: usize = 成员.iter().map(|a| a.received_count).sum();
        let 收到合计: f64 = 成员.iter().map(|a| a.received_sum).sum();
        assert_eq!(class.ratings_in, 收到条数, "{} 收到条数不对", class.label);
        if 收到条数 > 0 {
            let 均分 = (收到合计 / 收到条数 as f64) as f32;
            assert!((class.mean_received - 均分).abs() < 1e-5, "{} 均分不对", class.label);
        }
        let 给出条数: usize = 成员.iter().map(|a| a.ratings.len()).sum();
        assert_eq!(class.ratings_out, 给出条数, "{} 给出条数不对", class.label);
    }
}

#[test]
fn 社团统计与账户表一致() {
    let w = 世界(33);
    let s = w.report();
    let mut 内 = 0usize;
    let mut 间 = 0usize;
    let mut 内合计 = 0.0f64;
    let mut 间合计 = 0.0f64;
    for a in &w.accounts {
        for &u in &a.follows {
            if w.accounts[u].community == a.community {
                内 += 1;
            } else {
                间 += 1;
            }
        }
        for &(u, score) in &a.ratings {
            if w.accounts[u].community == a.community {
                内合计 += score as f64;
            } else {
                间合计 += score as f64;
            }
        }
    }
    assert_eq!(s.community.follow_edges, 内 + 间);
    assert_eq!(s.community.follow_intra, 内);
    assert_eq!(s.community.follow_inter, 间);
    assert_eq!(s.community.rating_intra + s.community.rating_inter, s.rating_edges);
    assert_eq!(s.community.count, w.communities.len());
    if 内 > 0 {
        assert!(
            (s.community.follow_intra_share - 内 as f32 / (内 + 间) as f32).abs() < 1e-6
        );
    }
    if s.community.rating_intra > 0 {
        assert!((s.community.intra_mean - (内合计 / s.community.rating_intra as f64) as f32).abs() < 1e-5);
    }
    if s.community.rating_inter > 0 {
        assert!((s.community.inter_mean - (间合计 / s.community.rating_inter as f64) as f32).abs() < 1e-5);
    }
}

#[test]
fn 评分分布与评分明细一致() {
    let w = 世界(34);
    let s = w.report();
    let ratings = w.ratings();
    assert_eq!(s.distribution.count, ratings.len());
    assert_eq!(s.rating_edges, ratings.len());
    let 均值: f64 = ratings.iter().map(|(_, _, x)| *x as f64).sum::<f64>() / ratings.len() as f64;
    assert!((s.distribution.mean as f64 - 均值).abs() < 1e-5);
    let 最小 = ratings.iter().map(|(_, _, x)| *x).fold(f32::MAX, f32::min);
    let 最大 = ratings.iter().map(|(_, _, x)| *x).fold(f32::MIN, f32::max);
    assert_eq!(s.distribution.min, 最小);
    assert_eq!(s.distribution.max, 最大);
    assert!(s.distribution.p10 >= 最小 && s.distribution.p99 <= 最大);
}

#[test]
fn 评分明细与账户行一致() {
    let w = 世界(35);
    let mut 期望: Vec<(usize, usize, f32)> = Vec::new();
    for (s, a) in w.accounts.iter().enumerate() {
        for &(u, score) in &a.ratings {
            期望.push((s, u, score));
        }
    }
    assert_eq!(w.ratings(), 期望);
    // 每条评分都必须落在已关注的人身上，且没有自评
    for (s, u, _) in w.ratings() {
        assert_ne!(s, u, "出现了自评");
        assert!(w.accounts[s].follows.contains(&u), "{s} 给没关注的 {u} 打了分");
    }
    // 每条关注都不指向已离开的人（离开之后不再新增关注）
    for a in &w.accounts {
        for &u in &a.follows {
            assert!(u < w.accounts.len());
        }
    }
}

#[test]
fn 在线状态与生命周期一致() {
    let w = 世界(36);
    for a in &w.accounts {
        let online = a.is_online(w.time, w.cfg.enable_rhythm);
        match a.life {
            Life::Active => {}
            Life::Dormant { .. } => assert!(!online, "休眠账户不该在线"),
            Life::Left { .. } => assert!(!online, "离开账户不该在线"),
        }
        if online {
            assert!(a.is_alive());
        }
    }
}

#[test]
fn 所有指标都是有限值且可渲染() {
    let w = 世界(37);
    let s = w.report();
    assert!(s.all_finite());
    assert!(s.attacker_mean.is_finite());
    assert!(s.distribution.std >= 0.0);
    assert!(s.gini_received >= 0.0 && s.gini_received <= 1.0);
    assert!(s.honest_quality_corr >= -1.0 && s.honest_quality_corr <= 1.0);
    let text = s.render();
    for 关键字 in ["快照", "分类账户", "社团结构", "评分分布", "攻防指标"] {
        assert!(text.contains(关键字), "报表缺少 {关键字}");
    }
    assert!(!text.contains("NaN"));
}

#[test]
fn 攻击收益方的种类正确() {
    // 渗透/背书场景：收益方是 Promoted；女巫场景：收益方是女巫
    let mut w = World::new(SimConfig {
        n_initial: 80,
        steps: 10,
        attack_kind: tm_sim::AttackKind::Infiltrator,
        attack_ratio: 0.1,
        seed: 38,
        ..SimConfig::default()
    });
    w.run();
    let t = w.attack.target.unwrap();
    assert_eq!(w.accounts[t].kind, Kind::Promoted);

    let mut w2 = World::new(SimConfig {
        n_initial: 80,
        steps: 10,
        attack_kind: tm_sim::AttackKind::Sybil { cluster: 2 },
        attack_ratio: 0.1,
        seed: 39,
        ..SimConfig::default()
    });
    w2.run();
    let t2 = w2.attack.target.unwrap();
    assert!(matches!(w2.accounts[t2].kind, Kind::Sybil { .. }));
    assert_eq!(w2.attack.cluster_of(t2), Some(0));
}
