//! 硬排除 vs 软降权：banlist 审查到底怎么做才有用。
use tm_agg::AggMode;
use tm_exp::{mean_rep_from_honest, run_case, win_rate, PropConfig};
use tm_sim::{AttackKind, SimConfig};

fn main() {
    let scenarios: Vec<(&str, AttackKind)> = vec![
        ("infiltrator", AttackKind::Infiltrator),
        ("endorser 20%", AttackKind::Endorser { ratio: 0.2 }),
        ("sybil x3", AttackKind::Sybil { cluster: 3 }),
    ];
    println!("{:<26} {:<14} {:>9} {:>8} {:>10} {:>10}", "策略", "场景", "计算比", "胜率", "诚实声望", "攻击声望");
    for (label, banlist, exclude) in [
        ("基线（无审查）", false, 0.0f32),
        ("软降权 0.5", true, 0.0),
        ("硬排除 0.5", true, 0.5),
        ("硬排除 0.8", true, 0.8),
        ("硬排除 0.95", true, 0.95),
    ] {
        for (name, kind) in &scenarios {
            let pc = PropConfig {
                beta: 0.3, samples: 8, rounds: 6, mode: AggMode::Mean,
                use_noise_floor: true, exclude_zero: true,
                banlist, ban_penalty: 0.5, ban_floor: 0.2, ban_exclude: exclude,
            };
            let cfg = SimConfig { n_initial: 200, steps: 200, seed: 7, ..Default::default() }
                .with_attack(kind.clone(), 0.1);
            let o = run_case(cfg, &pc);
            let ch = mean_rep_from_honest(&o, &o.honest);
            let ca = mean_rep_from_honest(&o, &o.others);
            let (win, _n) = win_rate(&o);
            println!("{:<26} {:<14} {:>9.3} {:>7.1}% {:>10.4} {:>10.4}",
                label, name,
                if ch > 0.0 { ca / ch } else { 0.0 },
                win * 100.0, ch, ca);
        }
        println!();
    }
}
