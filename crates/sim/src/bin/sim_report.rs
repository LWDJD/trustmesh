//! 示例程序：跑 200 个账户、200 小时的仿真，打印指标与几组对照实验。
//!
//! 用法：
//!
//! ```text
//! cargo run --bin sim_report            # 全部（基线 + 攻击 + 同质性扫描）
//! cargo run --bin sim_report base       # 只跑基线
//! cargo run --bin sim_report attack     # 五类攻击对比
//! cargo run --bin sim_report homophily  # 同质性 0→1 扫描
//! cargo run --bin sim_report long       # 1000 步稳定性
//! ```

use tm_sim::{AttackKind, SimConfig, World};

/// 基线：200 账户 200 小时，无攻击。
fn base_config() -> SimConfig {
    SimConfig {
        n_initial: 200,
        steps: 200,
        seed: 7,
        ..SimConfig::default()
    }
}

fn run_base() {
    println!("═══ 基线：200 账户 × 200 步 ═══");
    let mut world = World::new(base_config());
    println!("{}", describe(&world));
    world.run();
    println!("{}", world.report().render());
    println!("累计：加入 {} 人，流失 {} 人，休眠 {} 次", world.joins, world.leaves, world.dormancy_events);
    println!("评分总量 {} 条，平均每人给出 {:.2} 条", world.rating_count(), world.rating_count() as f32 / world.accounts.len() as f32);
    println!();
}

fn run_attack() {
    println!("═══ 攻击对比：200 账户 × 200 步 ═══");
    // 列名用 ASCII，保证等宽对齐；含义见下面两行列说明
    println!("列说明：honest / attacker = 该类账户平均被评分；a/h = attacker ÷ honest");
    println!("        target(n) = 收益方（被推广账户）均分及其收到的评分条数");
    println!("        t-adv = 收益方 ÷ 诚实；win(n) = 观察者胜率及可比观察者数");
    println!();
    println!(
        "{:<15} {:>8} {:>8} {:>9} {:>7} {:>13} {:>7} {:>12}",
        "scenario", "ratings", "honest", "attacker", "a/h", "target(n)", "t-adv", "win(n)"
    );
    let scenarios: Vec<(&str, AttackKind)> = vec![
        ("none", AttackKind::None),
        ("sybil x3", AttackKind::Sybil { cluster: 3 }),
        ("sleeper 60h", AttackKind::Sleeper { normal_until: 60 }),
        ("infiltrator", AttackKind::Infiltrator),
        ("endorser 20%", AttackKind::Endorser { ratio: 0.2 }),
        ("spammer", AttackKind::Spammer),
    ];
    for (name, kind) in scenarios {
        let cfg = SimConfig {
            attack_kind: kind,
            attack_ratio: 0.1,
            ..base_config()
        };
        let mut world = World::new(cfg);
        world.run();
        let s = world.report();
        println!(
            "{:<15} {:>8} {:>8.3} {:>9.3} {:>7.3} {:>13} {:>7.3} {:>12}",
            name,
            s.rating_edges,
            s.honest().mean_received,
            s.attacker_mean,
            s.attacker_advantage,
            format!("{:.3}({})", s.target_mean, s.target_ratings),
            s.target_advantage,
            format!("{:.3}({})", s.observer_win_rate, s.observers_compared)
        );
    }
    println!();
    println!("场景说明：none 无攻击 ｜ sybil 女巫 3 集群 ｜ sleeper 60 小时后突变 ｜");
    println!("          infiltrator 1/3 处收买高分账户 ｜ endorser 20% 高质量账户背书 ｜ spammer 垃圾制造者");
    println!();
}

fn run_homophily() {
    println!("═══ 同质性扫描：社团内关注边占比 ═══");
    println!("列说明：h = 同质性；intra/inter = 社团内/间关注边；share = 社团内占比；avg = 人均关注");
    println!();
    println!(
        "{:<6} {:>8} {:>8} {:>8} {:>8}",
        "h", "intra", "inter", "share", "avg"
    );
    for step in 0..=4 {
        let h = step as f64 / 4.0;
        let cfg = SimConfig {
            homophily: h,
            ..base_config()
        };
        let mut world = World::new(cfg);
        world.run();
        let s = world.report();
        println!(
            "{:<6.2} {:>8} {:>8} {:>8.3} {:>8.2}",
            h,
            s.community.follow_intra,
            s.community.follow_inter,
            s.community.follow_intra_share,
            s.follow_edges as f32 / s.alive.max(1) as f32
        );
    }
    println!();
}

fn run_long() {
    println!("═══ 长跑稳定性：200 账户 + 1000 步 ═══");
    let cfg = SimConfig {
        steps: 1000,
        attack_kind: AttackKind::Sybil { cluster: 3 },
        ..base_config()
    };
    let mut world = World::new(cfg);
    let start = std::time::Instant::now();
    world.run();
    let elapsed = start.elapsed();
    let s = world.report();
    println!("耗时 {:.2}s", elapsed.as_secs_f64());
    println!("{}", s.render());
    println!("最终账户 {}，评分 {}，内存约 {:.1} KB", s.accounts, s.rating_edges, world.rating_count() as f32 * 12.0 / 1024.0);
    println!();
}

fn describe(world: &World) -> String {
    format!(
        "配置：初始 {} 账户，{} 步（小时），社团 {}，同质性 {:.2}，桥接比例 {:.2}，加入率 {:.2}/h，攻击 {}",
        world.cfg.n_initial,
        world.cfg.steps,
        world.cfg.community_count,
        world.cfg.homophily,
        world.cfg.bridge_ratio,
        world.cfg.join_rate,
        world.attack.describe()
    )
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    match which.as_str() {
        "base" => run_base(),
        "attack" => run_attack(),
        "homophily" => run_homophily(),
        "long" => run_long(),
        "all" => {
            run_base();
            run_attack();
            run_homophily();
        }
        other => {
            eprintln!("未知场景：{other}（可选 base / attack / homophily / long / all）");
            std::process::exit(2);
        }
    }
}
