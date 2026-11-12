//! 五类攻击者的行为时序与收益。

use tm_sim::{AttackKind, Kind, SimConfig, World};

fn 攻击配置(kind: AttackKind, steps: usize, seed: u64) -> SimConfig {
    SimConfig {
        n_initial: 200,
        steps,
        seed,
        attack_kind: kind,
        attack_ratio: 0.1,
        ..SimConfig::default()
    }
}

#[test]
fn 潜伏者突变前不刷分突变后刷满分() {
    let cfg = SimConfig {
        attack_kind: AttackKind::Sleeper { normal_until: 40 },
        ..攻击配置(AttackKind::Sleeper { normal_until: 40 }, 120, 13)
    };
    let mut w = World::new(cfg);
    let target = w.attack.target.expect("潜伏者场景应有收益方");
    let sleepers: Vec<usize> = w
        .accounts
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    assert_eq!(sleepers.len(), 20);

    // 突变前
    w.run_steps(39);
    let 提前满分 = sleepers
        .iter()
        .filter(|&&s| w.accounts[s].rating_of(target) == Some(1.0))
        .count();
    assert!(提前满分 * 3 < sleepers.len(), "突变前就有 {提前满分} 个潜伏者打了满分");

    // 突变后
    w.run_steps(60);
    assert!(w.time >= 40);
    let 已突变 = sleepers
        .iter()
        .filter(|&&s| w.accounts[s].attack_active)
        .count();
    assert_eq!(已突变, sleepers.len(), "到点后所有潜伏者都应已突变");
    let alive: Vec<usize> = sleepers
        .iter()
        .copied()
        .filter(|&s| w.accounts[s].is_alive())
        .collect();
    let 满分比例 = alive
        .iter()
        .filter(|&&s| w.accounts[s].rating_of(target) == Some(1.0))
        .count() as f32
        / alive.len() as f32;
    assert!(满分比例 > 0.8, "突变后只有 {:.0}% 的潜伏者给收益方打满分", 满分比例 * 100.0);
    // 收益方确实被抬起来了
    let snap = w.report();
    assert!(
        snap.target_advantage > 1.1,
        "收益方优势只有 {}",
        snap.target_advantage
    );
}

#[test]
fn 渗透者按声誉挑选并在指定时刻收买() {
    let cfg = SimConfig {
        infiltrate_at: Some(50),
        ..攻击配置(AttackKind::Infiltrator, 120, 14)
    };
    let mut w = World::new(cfg);
    let target = w.attack.target.expect("渗透场景应有收益方");
    assert_eq!(w.accounts[target].kind, Kind::Promoted);

    w.run_steps(49);
    // 收买之前：没有任何渗透者
    assert!(w
        .accounts
        .iter()
        .all(|a| !matches!(a.kind, Kind::Infiltrator { .. })));
    assert_eq!(w.attack.purchased.len(), 0);

    // 记录收买前的声誉，验证挑选规则
    let mut 收买前: Vec<(usize, f32)> = w
        .accounts
        .iter()
        .filter(|a| a.kind == Kind::Honest && a.is_alive())
        .map(|a| (a.idx, a.reputation))
        .collect();
    收买前.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));

    w.step(); // t = 50，触发收买
    assert_eq!(w.attack.purchased.len(), 20);
    let mut 最高20: Vec<usize> = 收买前.iter().take(20).map(|x| x.0).collect();
    最高20.sort_unstable();
    let mut 实际: Vec<usize> = w.attack.purchased.clone();
    实际.sort_unstable();
    assert_eq!(实际, 最高20, "应该收买声誉最高的 20 个账户");

    for &c in &w.attack.purchased {
        assert!(matches!(w.accounts[c].kind, Kind::Infiltrator { bought_at: 50 }));
        assert!(w.accounts[c].attack_active);
        assert!(w.accounts[c].follows.contains(&target), "渗透者必须关注收益方");
    }

    // 再跑一段时间，多数渗透者会给收益方打满分
    w.run_steps(50);
    let 满分 = w
        .attack
        .purchased
        .iter()
        .filter(|&&c| w.accounts[c].is_alive() && w.accounts[c].rating_of(target) == Some(1.0))
        .count();
    assert!(满分 >= 16, "只有 {满分} 个渗透者给收益方打了满分");
    assert!(w.report().target_advantage > 1.2);
}

#[test]
fn 背书者按比例挑选高质量账户() {
    let cfg = SimConfig {
        attack_kind: AttackKind::Endorser { ratio: 0.2 },
        ..攻击配置(AttackKind::Endorser { ratio: 0.2 }, 120, 15)
    };
    let w = World::new(cfg);
    let target = w.attack.target.expect("背书场景应有收益方");
    let endorsers: Vec<usize> = w
        .accounts
        .iter()
        .filter(|a| a.kind == Kind::Endorser)
        .map(|a| a.idx)
        .collect();
    assert_eq!(endorsers.len(), 40, "200 × 0.2 = 40 个背书者");
    assert_eq!(w.attack.endorsers, 40);

    // 背书者的质量应高于其余诚实账户
    let mut min_end = f32::MAX;
    for &e in &endorsers {
        min_end = min_end.min(w.accounts[e].quality);
        assert!(w.accounts[e].attack_active);
        assert!(w.accounts[e].follows.contains(&target));
    }
    let max_other = w
        .accounts
        .iter()
        .filter(|a| a.kind == Kind::Honest)
        .map(|a| a.quality)
        .fold(0.0f32, f32::max);
    assert!(min_end >= max_other - 1e-6, "背书者不是质量最高的那批：{min_end} vs {max_other}");
}

#[test]
fn 背书者最终给收益方打满分() {
    let cfg = SimConfig {
        attack_kind: AttackKind::Endorser { ratio: 0.1 },
        ..攻击配置(AttackKind::Endorser { ratio: 0.1 }, 160, 16)
    };
    let mut w = World::new(cfg);
    let target = w.attack.target.unwrap();
    w.run();
    let endorsers = w.attack.endorsers;
    let 满分 = w
        .accounts
        .iter()
        .filter(|a| a.kind == Kind::Endorser && a.rating_of(target) == Some(1.0))
        .count();
    assert!(满分 * 10 >= endorsers * 9, "{满分}/{endorsers} 个背书者打了满分");
    let s = w.report();
    assert!(s.target_advantage > 1.3, "收益方优势 {}", s.target_advantage);
    assert!(
        s.observer_win_rate < 0.5,
        "多数观察者仍认为诚实账户更好（胜率 {}），攻击没生效",
        s.observer_win_rate
    );
}

#[test]
fn 垃圾制造者拿不到高分() {
    let cfg = 攻击配置(AttackKind::Spammer, 160, 17);
    let mut w = World::new(cfg);
    w.run();
    let s = w.report();
    let spam = s.class("spammer").unwrap();
    let honest = s.honest();
    assert_eq!(spam.accounts, 20);
    assert!(spam.ratings_in > 0, "垃圾账户应该收到过评分");
    assert!(
        spam.mean_received < honest.mean_received * 0.5,
        "垃圾均分 {} 没有被压住（诚实 {}）",
        spam.mean_received,
        honest.mean_received
    );
    assert!(spam.quality_mean < 0.1);

    // 垃圾账户自己乱打高分（不看质量）
    for (sidx, _, score) in w.ratings() {
        if w.accounts[sidx].kind == Kind::Spammer {
            assert!(score >= 0.55, "垃圾账户的给分 {} 低于刷量下限", score);
        }
    }
}

#[test]
fn 无攻击组的攻击指标为零() {
    let cfg = SimConfig {
        attack_kind: AttackKind::None,
        ..攻击配置(AttackKind::None, 80, 18)
    };
    let mut w = World::new(cfg);
    w.run();
    let s = w.report();
    assert_eq!(s.attacker_mean, 0.0);
    assert_eq!(s.attacker_advantage, 0.0);
    assert_eq!(s.target_advantage, 0.0);
    assert_eq!(s.target_ratings, 0);
    assert!(w.attack.target.is_none());
}

/// 回归测试：**单个**潜伏者也必须有真实的收益方。
///
/// 早期实现把 `attack.target` 设成「第一个潜伏者」，单个潜伏者时就指向它自己；
/// 而 `push_follow` 拒绝自环，于是攻击载荷完全空转——
/// 测出来的只是它作为普通账户的结构位置，不是攻击效果。
#[test]
fn 单个潜伏者也有真实收益方() {
    // attack_ratio 0.1 × n_initial 200 = 20 个，这里显式压成 1 个
    let cfg = SimConfig {
        n_initial: 200,
        steps: 140,
        seed: 13,
        attack_kind: AttackKind::Sleeper { normal_until: 40 },
        attack_ratio: 1.0 / 200.0,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);

    let target = w.attack.target.expect("单个潜伏者也必须有收益方");
    let sleepers: Vec<usize> = w
        .accounts
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    assert_eq!(sleepers.len(), 1, "本例应只有 1 个潜伏者");
    assert_ne!(target, sleepers[0], "收益方绝不能是潜伏者自己");
    assert!(
        w.accounts[sleepers[0]].follows(target),
        "潜伏者必须关注收益方，否则评分写不进去"
    );
    assert!(
        !w.accounts.iter().any(|a| matches!(a.kind, Kind::Promoted) && a.idx != target),
        "收益方应当就是那个 Promoted 账户"
    );

    // 突变之后必须真的给目标打满分
    w.run_steps(120);
    assert!(w.accounts[sleepers[0]].attack_active, "突变应已生效");
    assert_eq!(
        w.accounts[sleepers[0]].rating_of(target),
        Some(1.0),
        "突变后必须给收益方打满分"
    );
}
