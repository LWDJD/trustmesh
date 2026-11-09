//! 指数降权审查：只看「它确实表过态的敌人」。
use tm_agg::AggMode;
use tm_exp::{mean_rep_from_honest, run_case, win_rate, PropConfig};
use tm_sim::{AttackKind, SimConfig};

fn cfg(kind: &AttackKind) -> SimConfig {
    SimConfig {
        n_initial: 600, steps: 120, seed: 7,
        follow_min: 400, follow_max: 500, max_follows: 600,
        ..Default::default()
    }
    .with_attack(kind.clone(), if matches!(kind, AttackKind::None) { 0.0 } else { 0.08 })
}

fn main() {
    let scenarios: Vec<(&str, AttackKind)> = vec![
        ("sybil x3", AttackKind::Sybil { cluster: 3 }),
        ("infiltrator", AttackKind::Infiltrator),
        ("endorser 20%", AttackKind::Endorser { ratio: 0.2 }),
    ];
    println!("规模：600 初始账户 × 120 步，每人关注 400~500，攻击比例 8%");
    println!("{:<8} {:<14} {:>10} {:>9} {:>10} {:>10}", "λ", "场景", "计算比", "胜率", "诚实声望", "攻击声望");
    for lambda in [0.0f32, 3.0, 8.0, 15.0, 30.0] {
        for (name, kind) in &scenarios {
            let pc = PropConfig {
                beta: 0.3, samples: 16, rounds: 3, mode: AggMode::Mean,
                use_noise_floor: true, exclude_zero: true,
                banlist: false, ban_penalty: 0.5, ban_floor: 0.2,
                ban_exclude: 0.0, ban_lambda: lambda,
            };
            let o = run_case(cfg(kind), &pc);
            let ch = mean_rep_from_honest(&o, &o.honest);
            let ca = mean_rep_from_honest(&o, &o.others);
            let (win, _) = win_rate(&o);
            println!("{:<8.1} {:<14} {:>10.3} {:>8.1}% {:>10.4} {:>10.4}",
                lambda, name, if ch > 0.0 { ca / ch } else { 0.0 }, win * 100.0, ch, ca);
        }
        println!();
    }
}
