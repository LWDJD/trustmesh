//! 攻击者不带归一化时，能放大多少影响力？
use tm_agg::AggMode;
use tm_exp::{run_case_full, PropConfig};
use tm_sim::{AttackKind, Kind, SimConfig};

fn main() {
    let base = SimConfig {
        n_initial: 400, steps: 120, seed: 7,
        follow_min: 30, follow_max: 45, max_follows: 60,
        ..Default::default()
    }.with_attack(AttackKind::Infiltrator, 0.08);

    let mut w0 = tm_sim::World::new(base.clone());
    w0.run();
    let inf = w0.accounts().iter().find(|a| matches!(a.kind, Kind::Infiltrator { .. })).map(|a| a.idx).unwrap();
    let alive = w0.alive_count();
    let atk = vec![inf];

    let modes: [(&str, bool, bool, bool); 4] = [
        ("A 都不归一化（基线）",           false, false, false),
        ("B 诚实归一化·攻击者不归一化",     true,  false, false),
        ("C 同 B + 接收方归一化（防御）",   true,  false, true),
        ("D 所有人都归一化（理想）",        true,  true,  false),
    ];

    for (label, norm_honest, atk_norm, rx_norm) in modes {
        println!("\n══════ {} ══════", label);
        println!("{:<8} {:>12} {:>12} {:>8}", "引入数", "女巫均分", "诚实均分", "比值");
        for n_sybil in [1usize, 5, 20, 50] {
            let mut inject = Vec::new();
            let mut sybils = Vec::new();
            for k in 0..n_sybil {
                let s = alive + k;
                inject.push((inf, s, 1.0));
                sybils.push(s);
            }
            let pc = PropConfig {
                beta: 0.3, samples: 16, rounds: 4, mode: AggMode::Mean,
                use_noise_floor: true, exclude_zero: true,
                banlist: false, ban_penalty: 0.5, ban_floor: 0.2,
                ban_exclude: 0.0, ban_lambda: 0.0,
                normalize_view: norm_honest,
                attacker_normalizes: atk_norm,
                receiver_normalizes: rx_norm,
            };
            let o = run_case_full(base.clone(), &pc, &inject, &sybils, &atk);
            let mut hsum = 0.0f64; let mut hcnt = 0usize;
            for &ob in &o.honest {
                if ob >= o.rep.len() { continue; }
                for &h in &o.honest { if h < o.rep[ob].len() { hsum += o.rep[ob][h] as f64; hcnt += 1; } }
            }
            let baseline = if hcnt > 0 { (hsum / hcnt as f64) as f32 } else { 0.0 };
            let mut ssum = 0.0f64; let mut scnt = 0usize;
            for &ob in &o.honest {
                if ob >= o.rep.len() { continue; }
                for &s in &sybils {
                    if s < o.rep[ob].len() { ssum += o.rep[ob][s] as f64; scnt += 1; }
                }
            }
            let smean = if scnt > 0 { (ssum / scnt as f64) as f32 } else { 0.0 };
            println!("{:<8} {:>12.5} {:>12.5} {:>8.2}",
                n_sybil, smean, baseline,
                if baseline > 0.0 { smean / baseline } else { 0.0 });
        }
    }
}
