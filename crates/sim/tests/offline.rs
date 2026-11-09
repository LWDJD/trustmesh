//! 离线-回归轨迹：在线历史、离线时长、休眠/离开期间不产生评分。
//!
//! 这些测试守住的是潜伏场景的两条地基：
//! 1. `was_online_at` / `last_active_at` / `offline_duration` 必须能重建**过去**的在线状态；
//! 2. 离线（休眠 / 离开）期间，账户既不产生新评分，也不再被当作互动对象。

use tm_sim::{Life, SimConfig, World};

// ════════════════════════ 1. 历史与现状一致 ════════════════════════

/// 每步结束时，历史回放必须与账户当前状态逐位一致。
#[test]
fn 在线历史与当前状态逐步一致() {
    let cfg = SimConfig {
        n_initial: 60,
        steps: 40,
        join_rate: 0.4,
        leave_rate: 0.02,
        dormancy_rate: 0.08,
        sleep_hours: (3, 9),
        return_rate: 0.4,
        community_count: 3,
        seed: 11,
        ..SimConfig::default()
    };
    let rhythm = cfg.enable_rhythm;
    assert!(rhythm, "这个用例要覆盖昼夜节律");
    let mut w = World::new(cfg);
    for _ in 0..w.cfg.steps {
        w.step();
        let now = w.time;
        for i in 0..w.accounts.len() {
            let 现在的在线 = w.accounts[i].is_online(now, rhythm);
            let 最后在线 = w.last_active_at(i);
            assert_eq!(
                w.was_online_at(i, now),
                现在的在线,
                "账户 {i} 在 {now} 时的历史回放与现状不一致"
            );
            assert!(最后在线 <= now, "账户 {i} 的最后在线时刻跑到未来了");
            assert!(
                最后在线 >= w.accounts[i].joined_at,
                "账户 {i} 的最后在线时刻早于加入时刻"
            );
            if 现在的在线 {
                assert_eq!(w.offline_duration(i, now), 0, "在线时离线时长必须为 0");
            } else {
                assert!(
                    w.offline_duration(i, now) <= now - w.accounts[i].joined_at,
                    "账户 {i} 离线时长超过了它的年龄"
                );
                if !w.accounts[i].is_left() {
                    assert_eq!(
                        w.offline_duration(i, now),
                        now - 最后在线,
                        "账户 {i} 的离线时长与最后在线时刻对不上"
                    );
                }
            }
            if !w.accounts[i].is_left() && 最后在线 > w.accounts[i].joined_at {
                assert!(
                    w.was_online_at(i, 最后在线),
                    "账户 {i} 的最后在线时刻 {最后在线} 其实并不在线"
                );
            }
        }
    }
}

/// 刚加入、还落在离线窗口里的账户：最后在线时刻回退到加入时刻，离线时长为 0。
#[test]
fn 从未上线过的账户以加入时刻为起点() {
    let cfg = SimConfig {
        n_initial: 10,
        steps: 60,
        join_rate: 1.0,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        enable_rhythm: true,
        seed: 55,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    let mut 找到 = false;
    for _ in 0..w.cfg.steps {
        let 之前 = w.accounts.len();
        w.step();
        for i in 之前..w.accounts.len() {
            if w.was_online_at(i, w.time) {
                continue;
            }
            assert_eq!(w.accounts[i].joined_at, w.time, "只关心刚加入的账户");
            assert_eq!(
                w.last_active_at(i),
                w.accounts[i].joined_at,
                "还没上过线时，最后在线时刻应回退到加入时刻"
            );
            assert_eq!(w.offline_duration(i, w.time), 0);
            找到 = true;
        }
    }
    assert!(找到, "60 小时里应该有新账户刚加入就落在离线窗口里");
}

// ════════════════════════ 2. 复现性 ════════════════════════

/// 固定 seed 跑两遍，在线历史必须逐位相同。
#[test]
fn 固定种子两次运行在线历史一致() {
    let 跑 = |seed: u64| {
        let mut w = World::new(SimConfig::sleeper_scenario(80, 120, seed));
        w.run();
        let n = w.accounts.len();
        let mut 在线 = Vec::new();
        let mut 离线 = Vec::new();
        let mut 最后 = Vec::new();
        for i in 0..n {
            最后.push(w.last_active_at(i));
            for t in (0..=w.time).step_by(4) {
                在线.push(w.was_online_at(i, t));
                离线.push(w.offline_duration(i, t));
            }
        }
        (在线, 离线, 最后, w.ratings(), w.time)
    };
    let a = 跑(2024);
    let b = 跑(2024);
    assert_eq!(a.0, b.0, "同一时刻的在线与否必须完全一致");
    assert_eq!(a.1, b.1, "离线时长必须完全一致");
    assert_eq!(a.2, b.2, "最后在线时刻必须完全一致");
    assert_eq!(a.3, b.3, "评分明细必须完全一致");
    assert_eq!(a.4, b.4);
    // 换个种子必须换一个世界，否则「固定种子」没有意义
    let c = 跑(2025);
    assert_ne!(a.0, c.0, "换种子后在线轨迹不该完全相同");
}

// ════════════════════════ 3. 离线时长的时间口径 ════════════════════════

/// 在线时为 0；离线后每过一小时正好加一。
#[test]
fn 在线时离线时长为零离线后线性增长() {
    let cfg = SimConfig {
        n_initial: 120,
        steps: 400,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.05,
        sleep_hours: (200, 300),
        return_rate: 0.0,
        enable_rhythm: false,
        seed: 21,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(60);

    // 在线样本：离线时长 0，最后在线时刻就是现在
    let 在线者 = (0..w.accounts.len())
        .find(|&i| w.was_online_at(i, w.time))
        .expect("60 小时后应该还有人在线");
    assert_eq!(w.offline_duration(在线者, w.time), 0);
    assert_eq!(w.last_active_at(在线者), w.time);

    // 离线样本：离线时长 = 现在 − 最后在线时刻，然后线性增长
    let 休眠者 = (0..w.accounts.len())
        .find(|&i| w.accounts[i].is_dormant())
        .expect("应该有人睡去");
    let t0 = w.time;
    assert!(!w.was_online_at(休眠者, t0), "休眠者不该显示在线");
    let d0 = w.offline_duration(休眠者, t0);
    assert!(d0 > 0, "已经离线，时长不该是 0");
    assert_eq!(d0, t0 - w.last_active_at(休眠者));
    for k in 1..=20u64 {
        let t = t0 + k;
        assert!(!w.was_online_at(休眠者, t), "休眠者提前醒了");
        assert_eq!(
            w.offline_duration(休眠者, t),
            d0 + k,
            "离线时长必须随时间线性增长"
        );
    }
}

// ════════════════════════ 4. 离开 = 永久离线 ════════════════════════

/// 离开之后任意时刻都不在线，离线时长从离开时刻起算。
#[test]
fn 离开的账户永久离线() {
    let cfg = SimConfig {
        n_initial: 60,
        steps: 40,
        join_rate: 0.0,
        leave_rate: 0.6,
        dormancy_rate: 0.0,
        enable_rhythm: false,
        seed: 31,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(15);
    let 已离开: Vec<usize> = (0..w.accounts.len())
        .filter(|&i| w.accounts[i].is_left())
        .collect();
    assert!(!已离开.is_empty(), "离开率 0.6 时应该有人离开");
    let 现在 = w.time;
    for &i in &已离开 {
        let at = match w.accounts[i].life {
            Life::Left { at } => at,
            _ => unreachable!("刚筛过"),
        };
        assert!(at <= 现在);
        for t in at..=(at + 40) {
            assert!(!w.was_online_at(i, t), "账户 {i} 离开之后仍显示在线");
            assert_eq!(
                w.offline_duration(i, t),
                t - at,
                "账户 {i} 的离线时长应从离开时刻 {at} 起算"
            );
        }
        assert_eq!(w.last_active_at(i), at, "离开后最后在线时刻就是离开时刻");
        assert!(!w.was_online_at(i, at), "离开那一刻就已经离线");
        if at > 0 {
            assert!(w.was_online_at(i, at - 1), "离开前一小时应该还在线");
            assert_eq!(w.offline_duration(i, at - 1), 0);
        }
        // 远超总步数也一样：离开是永久的
        let far = 现在 + 10_000;
        assert!(!w.was_online_at(i, far));
        assert_eq!(w.offline_duration(i, far), far - at);
    }
}

/// 离开之后不再产生新的评分。
#[test]
fn 离开之后不再产生新的评分() {
    let cfg = SimConfig {
        n_initial: 80,
        steps: 60,
        join_rate: 0.0,
        leave_rate: 0.3,
        dormancy_rate: 0.05,
        sleep_hours: (5, 15),
        return_rate: 0.2,
        enable_rhythm: false,
        seed: 44,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(20);
    let 已离开: Vec<usize> = (0..w.accounts.len())
        .filter(|&i| w.accounts[i].is_left())
        .collect();
    assert!(!已离开.is_empty());
    let 评分快照: Vec<Vec<(usize, f32)>> = 已离开
        .iter()
        .map(|&i| w.accounts[i].ratings.clone())
        .collect();
    for _ in 0..30 {
        w.step();
    }
    for (k, &i) in 已离开.iter().enumerate() {
        assert!(!w.was_online_at(i, w.time), "离开之后不该在线");
        assert_eq!(w.accounts[i].ratings, 评分快照[k], "离开之后还在评分");
        assert_eq!(
            w.offline_duration(i, w.time),
            w.time - w.last_active_at(i),
            "离开后的离线时长口径不一致"
        );
    }
}

// ════════════════════════ 5. 休眠期间不产生评分 ════════════════════════

/// 同一个人睡着之后：自己不再打分，也不再被别人打分。
#[test]
fn 休眠期间不产生新的评分() {
    let cfg = SimConfig {
        n_initial: 120,
        steps: 500,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.04,
        sleep_hours: (300, 400),
        return_rate: 0.0,
        enable_rhythm: false,
        seed: 42,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    w.run_steps(80);
    let 目标 = (0..w.accounts.len())
        .find(|&i| w.accounts[i].is_dormant() && !w.accounts[i].ratings.is_empty())
        .expect("80 小时后应该有人带着评分睡去");
    let 评分快照 = w.accounts[目标].ratings.clone();
    let 收到条数 = w.accounts[目标].received_count;
    let 收到合计 = w.accounts[目标].received_sum;
    assert!(!评分快照.is_empty());
    for _ in 0..40 {
        w.step();
        assert!(w.accounts[目标].is_dormant(), "这个账户提前醒了");
        assert!(!w.was_online_at(目标, w.time), "休眠期间不该在线");
        assert_eq!(
            w.accounts[目标].ratings, 评分快照,
            "休眠期间账户 {目标} 产生了新评分"
        );
        assert_eq!(
            w.accounts[目标].received_count, 收到条数,
            "休眠期间账户 {目标} 还在被别人评分"
        );
        assert_eq!(
            w.accounts[目标].received_sum, 收到合计,
            "休眠期间账户 {目标} 的收到合计变了"
        );
    }
}

// ════════════════════════ 6. 回归 ════════════════════════

/// 回归概率拉满时，能观察到「睡去 → 醒来 → 重新在线」。
#[test]
fn 回归的账户重新变为在线() {
    let cfg = SimConfig {
        n_initial: 80,
        steps: 200,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.05,
        sleep_hours: (3, 10),
        return_rate: 1.0,
        enable_rhythm: false,
        seed: 33,
        ..SimConfig::default()
    };
    let mut w = World::new(cfg);
    let mut 回归者: Option<usize> = None;
    let mut 醒来时刻 = 0;
    for _ in 0..w.cfg.steps {
        let 之前: Vec<Life> = w.accounts.iter().map(|a| a.life).collect();
        w.step();
        for (i, &旧状态) in 之前.iter().enumerate() {
            if matches!(旧状态, Life::Dormant { .. }) && w.accounts[i].life == Life::Active {
                回归者 = Some(i);
                醒来时刻 = w.time;
                break;
            }
        }
        if 回归者.is_some() {
            break;
        }
    }
    let i = 回归者.expect("回归率 100% 时应该有人醒来");
    assert!(w.was_online_at(i, 醒来时刻), "回归者应立刻恢复在线");
    assert_eq!(w.offline_duration(i, 醒来时刻), 0);
    assert_eq!(w.last_active_at(i), 醒来时刻);
    assert!(!w.was_online_at(i, 醒来时刻 - 1), "醒来前一小时还在离线");
    assert!(w.offline_duration(i, 醒来时刻 - 1) > 0);
}

// ════════════════════════ 7. sleeper 场景 ════════════════════════

/// 场景要造出「大量用户长期离线、少数节点一直在线」的网络形态。
#[test]
fn sleeper_scenario_在线人数远少于存活人数() {
    let cfg = SimConfig::sleeper_scenario(200, 200, 7);
    let mut w = World::new(cfg.clone());
    let n = w.accounts.len();
    let mut 睡过 = vec![false; n];
    for _ in 0..cfg.steps {
        w.step();
        for (i, seen) in 睡过.iter_mut().enumerate() {
            if w.accounts[i].is_dormant() {
                *seen = true;
            }
        }
    }
    let 存活 = w.alive_count();
    let 在线 = w.online_count();
    assert!(存活 > 0, "场景不该把人全跑光");
    assert!(
        在线 * 2 < 存活,
        "在线人数必须显著少于存活人数：在线 {在线} / 存活 {存活}"
    );
    // 在线人数就是「历史接口逐个问一遍」的结果
    let 逐个 = (0..n).filter(|&i| w.was_online_at(i, w.time)).count();
    assert_eq!(在线, 逐个, "online_count 与 was_online_at 必须一致");
    // 大部分账户（≥70%）在仿真里睡过
    let 睡过数 = 睡过.iter().filter(|x| **x).count();
    let 比例 = 睡过数 as f64 / n as f64;
    assert!(比例 >= 0.70, "只有 {:.0}% 的账户休眠过", 比例 * 100.0);
}

/// 场景的评分边数非零且有限，且仍然满足「评分只落在关注对象上」。
#[test]
fn sleeper_scenario_评分边数非零且有限() {
    let mut w = World::new(SimConfig::sleeper_scenario(150, 150, 77));
    w.run();
    let ratings = w.ratings();
    assert!(!ratings.is_empty(), "场景里应该有人评分");
    let 上限 = w.accounts.len() * w.cfg.max_follows;
    assert!(
        ratings.len() <= 上限,
        "边数必须有限：{} > {}",
        ratings.len(),
        上限
    );
    assert_eq!(ratings.len(), w.rating_count());
    for &(s, u, score) in &ratings {
        assert!(
            score.is_finite() && score > 0.0 && score <= 1.0,
            "评分非法: {score}"
        );
        assert!(s < w.accounts.len() && u < w.accounts.len(), "编号越界");
        assert!(
            w.accounts[s].follows.contains(&u),
            "评分落在了没关注的人身上"
        );
    }
    assert!(w.report().all_finite());
}

// ════════════════════════ 8. 边界 ════════════════════════

/// t = 0、t 超过总步数、编号越界、u64::MAX 都不能 panic，且答案可解释。
#[test]
fn 边界时刻与越界编号安全() {
    let mut w = World::new(SimConfig::sleeper_scenario(30, 40, 5));
    // t = 0：这个场景关闭了节律，初始人口全员在线
    assert_eq!(w.time, 0);
    for i in 0..w.accounts.len() {
        assert!(w.was_online_at(i, 0), "sleeper 场景 t=0 应该全员在线");
        assert_eq!(w.offline_duration(i, 0), 0);
        assert_eq!(w.last_active_at(i), 0);
    }
    w.run();
    let 现在 = w.time;
    let n = w.accounts.len();

    // 一直活跃的账户投影到未来仍然在线；已离开的账户投影到未来仍然离线
    if let Some(i) = (0..n).find(|&i| w.accounts[i].life == Life::Active) {
        let far = 现在 + 10_000;
        assert!(w.was_online_at(i, far), "活跃账户投影到未来仍应在线");
        assert_eq!(w.offline_duration(i, far), 0);
        assert_eq!(w.last_active_at(i), 现在);
    }
    if let Some(i) = (0..n).find(|&i| w.accounts[i].is_left()) {
        let far = 现在 + 10_000;
        assert!(!w.was_online_at(i, far));
        assert_eq!(w.offline_duration(i, far), far - w.last_active_at(i));
    }

    // 编号越界：安全默认值
    assert!(!w.was_online_at(n, 0));
    assert!(!w.was_online_at(usize::MAX, 现在));
    assert_eq!(w.last_active_at(n), 0);
    assert_eq!(w.offline_duration(n, 现在), 0);
    assert_eq!(w.offline_duration(usize::MAX, u64::MAX), 0);
    // 极端时刻不溢出
    for i in 0..n {
        let _ = w.was_online_at(i, u64::MAX);
        let _ = w.offline_duration(i, u64::MAX);
    }

    // 空世界
    let mut empty = World::new(SimConfig {
        n_initial: 0,
        steps: 3,
        join_rate: 0.0,
        ..SimConfig::default()
    });
    empty.run();
    assert_eq!(empty.accounts.len(), 0);
    assert!(!empty.was_online_at(0, 0));
    assert_eq!(empty.last_active_at(0), 0);
    assert_eq!(empty.offline_duration(0, 100), 0);
}
