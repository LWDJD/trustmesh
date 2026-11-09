//! 长跑稳定性：1000 步不 panic、不越界、不爆内存。

use tm_sim::{AttackKind, SimConfig, World};

#[test]
fn 一千步稳定且内存有界() {
    let cfg = SimConfig {
        n_initial: 200,
        steps: 1000,
        seed: 9,
        attack_kind: AttackKind::Sybil { cluster: 3 },
        attack_ratio: 0.15,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    let mut 峰值账户 = 0;
    for k in 0..1000 {
        w.step();
        峰值账户 = 峰值账户.max(w.accounts.len());
        if k % 100 == 0 {
            let s = w.report();
            assert!(s.all_finite(), "第 {k} 步出现非有限指标");
            assert!(s.online <= s.alive, "在线数超过存活数");
            assert!(s.alive <= s.accounts, "存活数超过账户总数");
            assert!(s.accounts <= 20_000, "账户数爆炸：{}", s.accounts);
            assert!(
                s.rating_edges <= s.accounts * w.cfg.max_follows,
                "评分边超过理论上限"
            );
            assert!(s.dormant + s.left <= s.accounts);
        }
    }
    let s = w.report();
    assert_eq!(s.step, 1000);
    assert_eq!(s.time, 1000);
    assert!(s.rating_edges > 1000, "跑了一千步评分太少了：{}", s.rating_edges);
    assert!(s.alive > 50, "人口崩了：{}", s.alive);
    assert!(峰值账户 <= 20_000);
    // 内存上界估计：评分 12 字节/条 + 关注 8 字节/条
    let bytes = s.rating_edges * 12 + s.follow_edges * 8;
    assert!(bytes < 64 * 1024 * 1024, "内存估计 {bytes} 字节，超出预期");
}

#[test]
fn 极端参数不崩() {
    let 基础 = SimConfig {
        n_initial: 40,
        steps: 30,
        seed: 10,
        ..SimConfig::default()
    };
    let 场景: Vec<(&str, SimConfig)> = vec![
        (
            "全员第一小时离开",
            SimConfig {
                leave_rate: 1.0,
                dormancy_rate: 0.0,
                join_rate: 0.0,
                ..基础.clone()
            },
        ),
        (
            "全员休眠且永不回归",
            SimConfig {
                dormancy_rate: 1.0,
                return_rate: 0.0,
                leave_rate: 0.0,
                join_rate: 0.0,
                ..基础.clone()
            },
        ),
        (
            "单社团",
            SimConfig {
                community_count: 1,
                ..基础.clone()
            },
        ),
        (
            "关注上限 1",
            SimConfig {
                follow_min: 1,
                follow_max: 1,
                max_follows: 1,
                ..基础.clone()
            },
        ),
        (
            "满员攻击",
            SimConfig {
                attack_ratio: 1.0,
                attack_kind: AttackKind::Sybil { cluster: 1 },
                ..基础.clone()
            },
        ),
        (
            "空世界",
            SimConfig {
                n_initial: 0,
                join_rate: 0.0,
                ..基础.clone()
            },
        ),
        (
            "极端同质性与桥接",
            SimConfig {
                homophily: 1.0,
                bridge_ratio: 1.0,
                bridge_gain: 1.0,
                ..基础.clone()
            },
        ),
        (
            "二维兴趣",
            SimConfig {
                interest_dims: 2,
                ..基础.clone()
            },
        ),
        (
            "零互动零关注",
            SimConfig {
                interactions_per_step: 0.0,
                follow_rate: 0.0,
                ..基础.clone()
            },
        ),
    ];
    for (name, cfg) in 场景 {
        let mut w = World::new(cfg);
        w.run();
        let s = w.report();
        assert!(s.all_finite(), "{name}：出现非有限指标");
        assert!(s.online <= s.alive, "{name}");
        assert!(s.alive <= s.accounts, "{name}");
        assert!(w.ratings().iter().all(|(_, _, x)| *x > 0.0 && *x <= 1.0), "{name}");
        // 渲染也不能崩
        assert!(s.render().contains("快照"), "{name}");
    }
}

#[test]
fn 零步长也安全() {
    let cfg = SimConfig {
        n_initial: 30,
        steps: 0,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    assert_eq!(w.time, 0);
    assert_eq!(w.step, 0);
    assert_eq!(w.report().accounts, 30);
}

#[test]
fn 评分边数随时间近似线性增长不会爆炸() {
    let cfg = SimConfig {
        n_initial: 100,
        steps: 400,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        seed: 11,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(100);
    let a = w.rating_count();
    w.run_steps(100);
    let b = w.rating_count();
    assert!(b >= a, "评分边数倒退了");
    // 每人的关注数有上限，所以评分边被 max_follows × 账户数 卡住
    assert!(b <= w.accounts.len() * w.cfg.max_follows);
}
