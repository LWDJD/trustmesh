//! 影响力预算：来源打的人越多，每份越轻。能否限制"一个渗透点批量引入女巫"？
use tm_agg::AggMode;
use tm_exp::{run_case_inject, PropConfig, view_params};
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
    let _ = view_params();

    println!("渗透者 #{}\n", inf);
    for norm in [false, true] {
        println!("══════ 影响力预算 = {} ══════", if norm { "开（每份 = 评分/总和）" } else { "关（每份 = 原始评分）" });
        println!("{:<8} {:>10} {:>12} {:>12} {:>10}", "引入数", "有效女巫", "女巫均分", "诚实均分", "比值");
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
                ban_exclude: 0.0, ban_lambda: 0.0, normalize_view: norm,
                attacker_normalizes: true, receiver_normalizes: true,
            };
            let o = run_case_inject(base.clone(), &pc, &inject, &sybils);
            let mut hsum = 0.0f64; let mut hcnt = 0usize;
            for &ob in &o.honest {
                if ob >= o.rep.len() { continue; }
                for &h in &o.honest { if h < o.rep[ob].len() { hsum += o.rep[ob][h] as f64; hcnt += 1; } }
            }
            let baseline = if hcnt > 0 { (hsum / hcnt as f64) as f32 } else { 0.0 };
            let mut ssum = 0.0f64; let mut scnt = 0usize; let mut above = 0usize;
            for &ob in &o.honest {
                if ob >= o.rep.len() { continue; }
                for &s in &sybils {
                    if s < o.rep[ob].len() {
                        let v = o.rep[ob][s];
                        ssum += v as f64; scnt += 1;
                        if v > baseline { above += 1; }
                    }
                }
            }
            let smean = if scnt > 0 { (ssum / scnt as f64) as f32 } else { 0.0 };
            println!("{:<8} {:>10} {:>12.4} {:>12.4} {:>10.2}",
                n_sybil, format!("{}/{}", above / o.honest.len().max(1), n_sybil),
                smean, baseline, if baseline > 0.0 { smean / baseline } else { 0.0 });
        }
        println!();
    }
}
