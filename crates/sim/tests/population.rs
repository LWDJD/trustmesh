//! 人口动态：加入、离开、休眠与回归。

use tm_sim::{Life, SimConfig, World};

#[test]
fn 人口有加入也有离开() {
    let cfg = SimConfig {
        n_initial: 100,
        steps: 200,
        join_rate: 1.0,
        leave_rate: 0.01,
        seed: 2,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    assert!(w.joins > 0, "200 小时里应该有人加入");
    assert!(w.leaves > 0, "200 小时里应该有人离开");
    assert_eq!(w.accounts.len(), 100 + w.joins, "账户表只增不减");
    let s = w.report();
    assert_eq!(s.alive + s.left, s.accounts);
    assert_eq!(s.accounts, w.accounts.len());
    assert_eq!(s.left, s.accounts - s.alive);
}

#[test]
fn 休眠之后会回归() {
    let cfg = SimConfig {
        n_initial: 60,
        steps: 120,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.02,
        sleep_hours: (4, 12),
        return_rate: 1.0,
        seed: 3,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    let n = w.accounts.len();
    let mut ever_dormant = vec![false; n];
    for _ in 0..120 {
        w.step();
        for (i, seen) in ever_dormant.iter_mut().enumerate() {
            if w.accounts[i].is_dormant() {
                *seen = true;
            }
        }
    }
    let 睡过 = ever_dormant.iter().filter(|x| **x).count();
    assert!(睡过 > 5, "120 小时里应该有不少人休眠过，实测 {睡过}");
    // return_rate = 1 时，睡过的人最终都会回到 Active
    let 回归 = (0..n)
        .filter(|&i| ever_dormant[i] && matches!(w.accounts[i].life, Life::Active))
        .count();
    assert!(回归 > 0, "休眠者没有回归");
    assert_eq!(w.leaves, 0, "回归率 100% 时不该有人流失");
}

#[test]
fn 离开是永久的() {
    let cfg = SimConfig {
        n_initial: 50,
        steps: 40,
        join_rate: 0.0,
        leave_rate: 0.5,
        dormancy_rate: 0.0,
        seed: 4,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(20);
    let 已离开: Vec<usize> = w
        .accounts
        .iter()
        .filter(|a| a.is_left())
        .map(|a| a.idx)
        .collect();
    assert!(!已离开.is_empty(), "离开率 0.5 时应该有人离开");
    let 评分快照: Vec<usize> = 已离开.iter().map(|&i| w.accounts[i].ratings.len()).collect();
    let 状态快照: Vec<Life> = 已离开.iter().map(|&i| w.accounts[i].life).collect();

    w.run_steps(20);
    for (k, &i) in 已离开.iter().enumerate() {
        assert_eq!(w.accounts[i].life, 状态快照[k], "离开状态被改回去了");
        assert!(!w.accounts[i].is_online(w.time, true));
        assert_eq!(
            w.accounts[i].ratings.len(),
            评分快照[k],
            "已离开的账户还在产生新评分"
        );
    }
}

#[test]
fn 新账户的加入时间正确() {
    let cfg = SimConfig {
        n_initial: 20,
        steps: 50,
        join_rate: 2.0,
        leave_rate: 0.0,
        seed: 5,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    assert!(w.accounts.len() > 20);
    for a in &w.accounts {
        assert!(a.joined_at <= w.time, "加入时间晚于当前时间");
    }
    assert_eq!(
        w.accounts.iter().filter(|a| a.joined_at == 0).count(),
        20,
        "初始人口的加入时间都是 0"
    );
    assert!(w.accounts.iter().any(|a| a.joined_at > 0));
    // 后来的账户年龄更小
    let 最后 = w.accounts.last().unwrap();
    assert!(最后.joined_at > 0);
    assert_eq!(最后.age(w.time), w.time - 最后.joined_at);
}

#[test]
fn 账户总数有上限() {
    let cfg = SimConfig {
        n_initial: 10,
        steps: 200,
        join_rate: 20.0,
        leave_rate: 0.0,
        max_accounts: 50,
        seed: 6,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    assert_eq!(w.accounts.len(), 50, "账户数必须卡在上限");
}

#[test]
fn 活跃时段决定在线人数() {
    let cfg = SimConfig {
        n_initial: 40,
        steps: 48,
        join_rate: 0.0,
        enable_rhythm: true,
        seed: 7,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    let mut 在线序列 = Vec::new();
    for _ in 0..48 {
        w.step();
        在线序列.push(w.online_count());
    }
    let 不同 = {
        let mut v = 在线序列.clone();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    assert!(不同 > 3, "在线人数不随昼夜变化：{在线序列:?}");
    assert!(在线序列.iter().all(|&n| n <= 40));
    assert!(在线序列.iter().any(|&n| n > 5), "不该总是没人上线");
}

#[test]
fn 关闭昼夜节律后全员在线() {
    let cfg = SimConfig {
        n_initial: 30,
        steps: 10,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        enable_rhythm: false,
        seed: 8,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run();
    assert_eq!(w.online_count(), 30);
    assert_eq!(w.report().online, 30);
}
