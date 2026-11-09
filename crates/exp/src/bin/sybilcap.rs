//! banlist 指数降权能否缓解"渗透者批量引入女巫"。
use tm_agg::AggMode;
use tm_exp::{run_case_inject, PropConfig};
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

    // 一个渗透者引入 20 个全新女巫
    let n_sybil = 20usize;
    let mut inject = Vec::new();
    let mut sybils = Vec::new();
    for k in 0..n_sybil {
        let s = alive + k;
        inject.push((inf, s, 1.0));
        sybils.push(s);
    }

    println!("渗透者 #{}, 引入 {} 个全新女巫（每个都被打 1.0）\n", inf, n_sybil);
    println!("{:<8} {:<12} {:>12} {:>12} {:>10} {:>10}", "λ", "有效女巫", "女巫均分", "诚实均分", "比值", "胜率");
    for lambda in [0.0f32, 2.0, 5.0, 10.0, 20.0, 40.0] {
        let pc = PropConfig {
            beta: 0.3, samples: 16, rounds: 4, mode: AggMode::Mean,
            use_noise_floor: true, exclude_zero: true,
            banlist: false, ban_penalty: 0.5, ban_floor: 0.2, ban_exclude: 0.0, ban_lambda: lambda,
        };
        // 注意：指数降权用的是 o.others，这里需要把女巫也算进 others
        let extras: Vec<usize> = sybils.clone();
        let o = run_case_inject(base.clone(), &pc, &inject, &extras);

        let mut hsum = 0.0f64; let mut hcnt = 0usize;
        for &ob in &o.honest {
            if ob >= o.rep.len() { continue; }
            for &h in &o.honest { if h < o.rep[ob].len() { hsum += o.rep[ob][h] as f64; hcnt += 1; } }
        }
        let baseline = if hcnt > 0 { (hsum / hcnt as f64) as f32 } else { 0.0 };

        let mut ssum = 0.0f64; let mut scnt = 0usize; let mut above = 0usize;
        let mut win = 0usize; let mut wtot = 0usize;
        for &ob in &o.honest {
            if ob >= o.rep.len() { continue; }
            let mut hm = 0.0f64; let mut hc = 0usize;
            for &h in &o.honest { if h < o.rep[ob].len() { hm += o.rep[ob][h] as f64; hc += 1; } }
            let mut sm = 0.0f64; let mut sc = 0usize;
            for &s in &sybils {
                if s < o.rep[ob].len() {
                    let v = o.rep[ob][s];
                    ssum += v as f64; scnt += 1;
                    if v > baseline { above += 1; }
                    sm += v as f64; sc += 1;
                }
            }
            if hc > 0 && sc > 0 { wtot += 1; if hm / hc as f64 > sm / sc as f64 { win += 1; } }
        }
        let smean = if scnt > 0 { (ssum / scnt as f64) as f32 } else { 0.0 };
        println!("{:<8.1} {:>12} {:>12.4} {:>12.4} {:>10.2} {:>9.1}%",
            lambda,
            format!("{}/{}", above / o.honest.len().max(1), n_sybil),
            smean, baseline,
            if baseline > 0.0 { smean / baseline } else { 0.0 },
            100.0 * win as f32 / wtot.max(1) as f32);
    }
}
