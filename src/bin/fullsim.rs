//! 全真模拟报告。

use social_trust::fullsim::{Sim, SimConfig, Snapshot};

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

    println!("\n完成。");
}
