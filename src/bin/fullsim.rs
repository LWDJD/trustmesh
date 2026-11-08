//! 全真模拟报告。

use trustmesh::fullsim::{Sim, SimConfig, Snapshot};

fn run_case(name: &str, cfg: SimConfig) -> Vec<Snapshot> {
    let mut sim = Sim::new(cfg);
    let t0 = std::time::Instant::now();
    sim.run();
    let dt = t0.elapsed();
    let last = sim.stats.last().cloned();
    println!(
        "{:<22} 用时 {:<10.2?} 期末账户 {:<6} 在线 {:<5} 边 {:<6} 信任均值 {:+.4} 标准差 {:.4}",
        name,
        dt,
        last.as_ref().map(|s| s.n_alive).unwrap_or(0),
        last.as_ref().map(|s| s.n_online).unwrap_or(0),
        last.as_ref().map(|s| s.n_edges).unwrap_or(0),
        last.as_ref().map(|s| s.trust_mean).unwrap_or(0.0),
        last.as_ref().map(|s| s.trust_std).unwrap_or(0.0),
    );
    sim.stats
}

fn main() {
    println!("全真模拟报告");
    println!("========================================\n");

    // ---- 实验一：场景对比 ----
    println!("[1] 场景对比（N=200, 30 天）");
    println!("{:<22} {:<26} {:<12} {:<10}", "场景", "规模/边", "信任分布", "攻击提升");

    let base = SimConfig {
        n_initial: 200,
        days: 30,
        attack_ratio: 0.0,
        spam_ratio: 0.0,
        enable_rhythm: false,
        ..Default::default()
    };
    let base_stats = run_case("基线（无节律无攻击）", base);

    let rhythm = SimConfig {
        n_initial: 200,
        days: 30,
        attack_ratio: 0.0,
        spam_ratio: 0.0,
        enable_rhythm: true,
        ..Default::default()
    };
    let rhythm_stats = run_case("加昼夜节律", rhythm);

    let attack = SimConfig {
        n_initial: 200,
        days: 30,
        attack_ratio: 0.1,
        spam_ratio: 0.0,
        enable_rhythm: true,
        ..Default::default()
    };
    let attack_stats = run_case("加女巫攻击 10%", attack);

    let spam = SimConfig {
        n_initial: 200,
        days: 30,
        attack_ratio: 0.0,
        spam_ratio: 0.1,
        enable_rhythm: true,
        ..Default::default()
    };
    let spam_stats = run_case("加垃圾投放 10%", spam);

    let full = SimConfig {
        n_initial: 200,
        days: 30,
        attack_ratio: 0.1,
        spam_ratio: 0.1,
        enable_rhythm: true,
        ..Default::default()
    };
    let full_stats = run_case("全部开启", full);

    // ---- 实验二：攻击效果对比 ----
    println!("\n[2] 攻击与垃圾的影响（期末值）");
    println!(
        "{:<22} {:<14} {:<14} {:<14}",
        "场景", "目标信任", "垃圾信任", "新用户信任"
    );

    for (name, stats) in [
        ("基线", &base_stats),
        ("节律", &rhythm_stats),
        ("攻击", &attack_stats),
        ("垃圾", &spam_stats),
        ("全部", &full_stats),
    ] {
        if let Some(s) = stats.last() {
            println!(
                "{:<22} {:<14} {:<14} {:<14}",
                name,
                s.target_trust.map(|v| format!("{:+.4}", v)).unwrap_or("-".into()),
                format!("{:+.4}", s.spam_trust),
                format!("{:+.4}", s.new_user_trust),
            );
        }
    }

    // ---- 实验二 b：随机曝光的对比 ----
    println!("\n[2b] 随机曝光对冷启动的影响（N=200, 30 天, 10% 攻击）");
    println!(
        "{:<14} {:<16} {:<16} {:<16}",
        "曝光", "新用户信任", "目标信任", "垃圾信任"
    );

    for &enabled in &[false, true] {
        let cfg = SimConfig {
            n_initial: 200,
            days: 30,
            attack_ratio: 0.1,
            spam_ratio: 0.1,
            enable_rhythm: true,
            enable_exposure: enabled,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        sim.run();
        if let Some(s) = sim.stats.last() {
            println!(
                "{:<14} {:<16.4} {:<16} {:<16.4}",
                if enabled { "开" } else { "关" },
                s.new_user_trust,
                s.target_trust.map(|v| format!("{:+.4}", v)).unwrap_or("-".into()),
                s.spam_trust,
            );
        }
    }

    // ---- 实验三：规模对比 ----
    println!("\n[3] 规模对比（30 天，10% 攻击 + 10% 垃圾）");
    println!(
        "{:<10} {:<10} {:<12} {:<14} {:<14}",
        "初始 N", "期末 N", "边数", "信任标准差", "总用时"
    );

    for &n in &[100usize, 300, 600] {
        let cfg = SimConfig {
            n_initial: n,
            days: 30,
            attack_ratio: 0.1,
            spam_ratio: 0.1,
            enable_rhythm: true,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        let t0 = std::time::Instant::now();
        sim.run();
        let dt = t0.elapsed();
        if let Some(s) = sim.stats.last() {
            println!(
                "{:<10} {:<10} {:<12} {:<14.4} {:<14.2?}",
                n, s.n_alive, s.n_edges, s.trust_std, dt
            );
        }
    }

    // ---- 实验四：β 的影响 ----
    println!("\n[4] 个人权重 β 的影响（N=200, 10% 攻击）");
    println!(
        "{:<8} {:<14} {:<14} {:<14}",
        "β", "目标信任", "垃圾信任", "信任标准差"
    );

    for &b in &[0.3, 0.5, 0.7, 0.9] {
        let cfg = SimConfig {
            n_initial: 200,
            days: 30,
            attack_ratio: 0.1,
            spam_ratio: 0.1,
            beta: b,
            enable_rhythm: true,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        sim.run();
        if let Some(s) = sim.stats.last() {
            println!(
                "{:<8.1} {:<14} {:<14.4} {:<14.4}",
                b,
                s.target_trust.map(|v| format!("{:+.4}", v)).unwrap_or("-".into()),
                s.spam_trust,
                s.trust_std,
            );
        }
    }

    // ---- 实验五：收敛性 ----
    println!("\n[5] 收敛性：信任分布随时间的变化（N=200, 全部开启）");
    println!(
        "{:<8} {:<10} {:<12} {:<12} {:<12}",
        "小时", "在线", "信任均值", "标准差", "目标信任"
    );

    for s in full_stats.iter().filter(|s| s.hour >= 168).step_by(5) {
        println!(
            "{:<8} {:<10} {:<12.4} {:<12.4} {:<12}",
            s.hour,
            s.n_online,
            s.trust_mean,
            s.trust_std,
            s.target_trust.map(|v| format!("{:+.4}", v)).unwrap_or("-".into())
        );
    }

    // ---- 实验六：攻击者规模扫描（曝光开关对照）----
    println!("\n[6] 攻击者规模扫描（N=200 诚实, 30 天）");
    println!(
        "{:<12} {:<8} {:<10} {:<14} {:<16} {:<16}",
        "攻击者/诚实", "曝光", "期末N", "目标信任", "诚实→攻击者", "诚实→诚实"
    );

    for &exposure in &[false, true] {
        for &ratio in &[0.1f64, 0.5, 1.0, 2.0, 5.0] {
            let cfg = SimConfig {
                n_initial: 200,
                days: 30,
                attack_ratio: ratio,
                spam_ratio: 0.0,
                enable_rhythm: true,
                enable_exposure: exposure,
                ..Default::default()
            };
            let mut sim = Sim::new(cfg);
            sim.run();

            let honest: Vec<usize> = sim
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Honest))
                .map(|a| a.id)
                .collect();
            let attackers: Vec<usize> = sim
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Sybil { .. }))
                .map(|a| a.id)
                .collect();

            let mut h_to_a = 0.0;
            let mut h_to_a_n = 0;
            let mut h_to_h = 0.0;
            let mut h_to_h_n = 0;
            for &s in &honest {
                for &u in &attackers {
                    h_to_a += sim.trust[s][u];
                    h_to_a_n += 1;
                }
                for &u in &honest {
                    if s != u {
                        h_to_h += sim.trust[s][u];
                        h_to_h_n += 1;
                    }
                }
            }

            let target = sim
                .stats
                .last()
                .and_then(|s| s.target_trust)
                .map(|v| format!("{:+.4}", v))
                .unwrap_or("-".into());

            println!(
                "{:<12.1} {:<8} {:<10} {:<14} {:<16.4} {:<16.4}",
                ratio,
                if exposure { "开" } else { "关" },
                sim.accounts.len(),
                target,
                if h_to_a_n > 0 { h_to_a / h_to_a_n as f64 } else { 0.0 },
                if h_to_h_n > 0 { h_to_h / h_to_h_n as f64 } else { 0.0 },
            );
        }
    }

    // ---- 实验七：背书攻击 ----
    println!("\n[7] 背书攻击：高质量账户给攻击者打满分（N=200, 30 天, 攻击者 1 倍）");
    println!(
        "{:<14} {:<16} {:<16} {:<16}",
        "背书比例", "被背书攻击者", "普通攻击者", "诚实用户"
    );

    for &ratio in &[0.0f64, 0.1, 0.3, 0.5, 0.8] {
        let cfg = SimConfig {
            n_initial: 200,
            days: 30,
            attack_ratio: 1.0,
            spam_ratio: 0.0,
            enable_rhythm: true,
            enable_exposure: false,
            endorsement_ratio: ratio,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        sim.run();

        let honest: Vec<usize> = sim
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Honest))
            .map(|a| a.id)
            .collect();
        let sybils: Vec<usize> = sim
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Sybil { .. }))
            .map(|a| a.id)
            .collect();

        let endorsed = sim.endorsed_targets.first().copied();
        let mut endorsed_trust = 0.0;
        let mut endorsed_n = 0;
        let mut other_sybil = 0.0;
        let mut other_n = 0;
        let mut honest_trust = 0.0;
        let mut honest_n = 0;

        for &s in &honest {
            for &u in &sybils {
                if Some(u) == endorsed {
                    endorsed_trust += sim.trust[s][u];
                    endorsed_n += 1;
                } else {
                    other_sybil += sim.trust[s][u];
                    other_n += 1;
                }
            }
            for &u in &honest {
                if s != u {
                    honest_trust += sim.trust[s][u];
                    honest_n += 1;
                }
            }
        }

        println!(
            "{:<14.1} {:<16.4} {:<16.4} {:<16.4}",
            ratio,
            if endorsed_n > 0 { endorsed_trust / endorsed_n as f64 } else { 0.0 },
            if other_n > 0 { other_sybil / other_n as f64 } else { 0.0 },
            if honest_n > 0 { honest_trust / honest_n as f64 } else { 0.0 },
        );
    }

    // ---- 实验七：背书预算稀释 ----
    println!("\n[7] 背书预算稀释（背书比例 30%, 攻击者 1 倍, 30 天）");
    println!(
        "{:<10} {:<14} {:<14} {:<14} {:<14}",
        "背书数", "第1个目标", "第5个", "第20个", "第100个"
    );

    for &count in &[1usize, 5, 20, 100] {
        let cfg = SimConfig {
            n_initial: 200,
            days: 30,
            attack_ratio: 1.0,
            spam_ratio: 0.0,
            enable_rhythm: true,
            enable_exposure: false,
            endorsement_ratio: 0.3,
            endorse_count: count,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        sim.run();

        let honest: Vec<usize> = sim
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Honest))
            .map(|a| a.id)
            .collect();

        let sample = |idx: usize| -> String {
            if idx >= sim.endorsed_targets.len() {
                return "-".into();
            }
            let t = sim.endorsed_targets[idx];
            let mut sum = 0.0;
            for &s in &honest {
                sum += sim.trust[s][t];
            }
            format!("{:.4}", sum / honest.len().max(1) as f64)
        };

        println!(
            "{:<10} {:<14} {:<14} {:<14} {:<14}",
            count,
            sample(0),
            sample(4),
            sample(19),
            sample(99),
        );
    }

    // ---- 实验七：背书预算稀释 ----
    println!("\n[7] 背书预算稀释（背书比例 30%, 攻击者 1 倍, 30 天）");
    println!(
        "{:<12} {:<10} {:<14} {:<14} {:<14}",
        "影响力预算", "背书数", "第1个目标", "第20个", "第100个"
    );

    for &budget in &[10000.0f64, 50.0] {
        for &count in &[1usize, 20, 100] {
            let cfg = SimConfig {
                n_initial: 200,
                days: 30,
                attack_ratio: 1.0,
                spam_ratio: 0.0,
                enable_rhythm: true,
                enable_exposure: false,
                endorsement_ratio: 0.3,
                endorse_count: count,
                influence_budget: budget,
                ..Default::default()
            };
            let mut sim = Sim::new(cfg);
            sim.run();

            let honest: Vec<usize> = sim
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, trustmesh::fullsim::Kind::Honest))
                .map(|a| a.id)
                .collect();

            let sample = |idx: usize| -> String {
                if idx >= sim.endorsed_targets.len() {
                    return "-".into();
                }
                let t = sim.endorsed_targets[idx];
                let mut sum = 0.0;
                for &s in &honest {
                    sum += sim.trust[s][t];
                }
                format!("{:.4}", sum / honest.len().max(1) as f64)
            };

            println!(
                "{:<12.0} {:<10} {:<14} {:<14} {:<14}",
                budget,
                count,
                sample(0),
                sample(19),
                sample(99),
            );
        }
    }

    // ---- 实验八：个人/群体权重与圈子化 ----
    println!("\n[8] 个人/群体权重对圈子化与反女巫的影响（N=200, 10% 攻击）");
    println!(
        "{:<12} {:<14} {:<16} {:<12}",
        "个人:群体", "目标信任", "信任标准差", "圈子化"
    );

    for &beta in &[0.3f64, 0.5, 0.7, 0.9] {
        let cfg = SimConfig {
            n_initial: 200,
            days: 30,
            attack_ratio: 0.1,
            spam_ratio: 0.0,
            beta,
            enable_rhythm: true,
            enable_exposure: true,
            ..Default::default()
        };
        let mut sim = Sim::new(cfg);
        sim.run();
        if let Some(s) = sim.stats.last() {
            println!(
                "{:<12} {:<14} {:<16.4} {:<12.3}",
                format!("{:.0}:{:.0}", beta * 10.0, (1.0 - beta) * 10.0),
                s.target_trust.map(|v| format!("{:+.4}", v)).unwrap_or("-".into()),
                s.trust_std,
                s.homophily,
            );
        }
    }

    println!("\n完成。");
}
