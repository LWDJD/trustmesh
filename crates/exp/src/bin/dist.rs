//! β 扫描：个人项权重能不能让"近邻"自己识别出被污染的目标？
use std::collections::VecDeque;
use tm_agg::AggMode;
use tm_exp::{run_case, PropConfig};
use tm_sim::{AttackKind, Kind, SimConfig};

fn main() {
    let cfg = SimConfig {
        n_initial: 400, steps: 120, seed: 7,
        follow_min: 30, follow_max: 45, max_follows: 60,
        ..Default::default()
    }.with_attack(AttackKind::Infiltrator, 0.08);

    let mut world = tm_sim::World::new(cfg.clone());
    world.run();
    let ratings = world.ratings();
    let accs = world.accounts();
    let (mut inf, mut prom) = (None, None);
    for a in accs {
        match a.kind {
            Kind::Infiltrator { .. } => { if inf.is_none() { inf = Some(a.idx); } }
            Kind::Promoted => { if prom.is_none() { prom = Some(a.idx); } }
            _ => {}
        }
    }
    let (inf, prom) = match (inf, prom) { (Some(i), Some(p)) => (i, p), _ => return };
    let n = accs.len();
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (o, t, _) in &ratings { if *o < n && *t < n { rev[*t].push(*o); } }
    let mut dist = vec![usize::MAX; n];
    dist[inf] = 0;
    let mut q = VecDeque::new(); q.push_back(inf);
    while let Some(u) = q.pop_front() {
        for &v in &rev[u] { if dist[v] == usize::MAX { dist[v] = dist[u] + 1; q.push_back(v); } }
    }

    println!("{:<6} {:>14} {:>14} {:>12} {:>10}", "beta", "1-2跳 污染比", "全体 污染比", "目标声望", "诚实声望");
    for beta in [0.3f32, 0.5, 0.7, 0.9] {
        let pc = PropConfig {
            beta, samples: 16, rounds: 4, mode: AggMode::Mean,
            use_noise_floor: true, exclude_zero: true,
            banlist: false, ban_penalty: 0.5, ban_floor: 0.2, ban_exclude: 0.0, ban_lambda: 0.0,
            normalize_view: true, attacker_normalizes: true, receiver_normalizes: true,
        };
        let o = run_case(cfg.clone(), &pc);

        let mut near_t = 0.0f64; let mut near_h = 0.0f64; let mut near_c = 0usize;
        let mut all_t = 0.0f64; let mut all_h = 0.0f64; let mut all_c = 0usize;
        for ob in 0..n {
            if ob >= o.rep.len() { continue; }
            let mut hs = 0.0f64; let mut hc = 0usize;
            for &h in &o.honest { if h < o.rep[ob].len() { hs += o.rep[ob][h] as f64; hc += 1; } }
            if hc == 0 { continue; }
            let hm = hs / hc as f64;
            let tv = o.rep[ob][prom] as f64;
            all_t += tv; all_h += hm; all_c += 1;
            if dist[ob] <= 2 { near_t += tv; near_h += hm; near_c += 1; }
        }
        let (nt, nh) = if near_c > 0 { (near_t / near_c as f64, near_h / near_c as f64) } else { (0.0, 0.0) };
        let (at, ah) = if all_c > 0 { (all_t / all_c as f64, all_h / all_c as f64) } else { (0.0, 0.0) };
        println!("{:<6.1} {:>14.2} {:>14.2} {:>12.4} {:>10.4}",
            beta,
            if nh > 0.0 { nt / nh } else { 0.0 },
            if ah > 0.0 { at / ah } else { 0.0 },
            at, ah);
    }
}
