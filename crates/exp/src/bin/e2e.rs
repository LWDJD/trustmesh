//! 影响力预算对全部攻击场景的效果。
use tm_agg::AggMode;
use tm_exp::{mean_rep_from_honest, run_case, win_rate, PropConfig};
use tm_sim::{AttackKind, SimConfig};

fn cfg(kind: &AttackKind) -> SimConfig {
    SimConfig {
        n_initial: 400, steps: 120, seed: 7,
        follow_min: 30, follow_max: 45, max_follows: 60,
        ..Default::default()
    }
    .with_attack(kind.clone(), if matches!(kind, AttackKind::None) { 0.0 } else { 0.08 })
}

fn main() {
    let scenarios: Vec<(&str, AttackKind)> = vec![
        ("none", AttackKind::None),
        ("sybil x3", AttackKind::Sybil { cluster: 3 }),
        ("sleeper 60h", AttackKind::Sleeper { normal_until: 60 }),
        ("infiltrator", AttackKind::Infiltrator),
        ("endorser 20%", AttackKind::Endorser { ratio: 0.2 }),
        ("spammer", AttackKind::Spammer),
    ];
    for norm in [false, true] {
        let pc = PropConfig {
            beta: 0.3, samples: 16, rounds: 4, mode: AggMode::Mean,
            use_noise_floor: true, exclude_zero: true,
            banlist: false, ban_penalty: 0.5, ban_floor: 0.2,
            ban_exclude: 0.0, ban_lambda: 0.0, normalize_view: norm,
        };
        println!("\n══════ 影响力预算 = {} ══════", if norm { "开" } else { "关" });
        println!("{:<14} {:>9} {:>9} {:>9} {:>10} {:>10}", "场景", "原始比", "计算比", "胜率", "诚实声望", "攻击声望");
        for (name, kind) in &scenarios {
            let o = run_case(cfg(kind), &pc);
            let ch = mean_rep_from_honest(&o, &o.honest);
            let ca = mean_rep_from_honest(&o, &o.others);
            let (win, _) = win_rate(&o);
            println!("{:<14} {:>9.3} {:>9.3} {:>8.1}% {:>10.4} {:>10.4}",
                name,
                if o.raw_honest > 0.0 { o.raw_others / o.raw_honest } else { 0.0 },
                if ch > 0.0 { ca / ch } else { 0.0 },
                win * 100.0, ch, ca);
        }
    }
}
