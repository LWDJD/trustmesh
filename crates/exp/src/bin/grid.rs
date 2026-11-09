//! 潜伏完整网格 · 真多 runtime 版
//!
//! 每个账户一个独立的 tm_engine::Engine；离线节点不 tick、不收观点；
//! 观点年龄真实增长（衰减生效）；回归节点做快速刷新。
//!
//! 网格：离线比例 p ∈ {30,50,70,90}% × 离线时长 D ∈ {72,216,720}h × 潜伏者 s ∈ {1,5,20}
use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, SimConfig};
use tm_view::ViewParams;

const N: usize = 200;
const STEPS: usize = 300;
const ROUNDS: u64 = 10;
const SAMPLES: usize = 12;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn cfg_base(dormancy: f64, d: u64, seed: u64) -> SimConfig {
    SimConfig {
        n_initial: N, steps: STEPS, seed,
        join_rate: 0.0, leave_rate: 0.0,
        enable_rhythm: false,
        dormancy_rate: dormancy,
        sleep_hours: (d, d * 6 / 5),
        return_rate: 0.05,
        ..Default::default()
    }
}

/// 量一下某个 dormancy_rate 下的离线比例
fn offline_frac(dormancy: f64, d: u64, seed: u64) -> f64 {
    let mut w = tm_sim::World::new(cfg_base(dormancy, d, seed));
    w.run();
    let accs = w.accounts();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    if alive.is_empty() { return 0.0; }
    let off = alive.iter().filter(|i| !w.was_online_at(**i, STEPS as u64)).count();
    off as f64 / alive.len() as f64
}

/// 二分找能命中目标离线比例的 dormancy_rate
fn calibrate(target: f64, d: u64, seed: u64) -> (f64, f64) {
    let (mut lo, mut hi) = (0.0005f64, 0.5f64);
    let mut best = (lo, offline_frac(lo, d, seed));
    for _ in 0..8 {
        let mid = (lo + hi) / 2.0;
        let f = offline_frac(mid, d, seed);
        if (f - target).abs() < (best.1 - target).abs() { best = (mid, f); }
        if f < target { lo = mid; } else { hi = mid; }
    }
    best
}

struct Res { honest: f64, sleeper: f64, ret_honest: f64, ret_sleeper: f64, ret_n: usize, onl: usize, alive: usize }

fn run_world(dormancy: f64, d: u64, seed: u64, sleepers: usize, with_sleeper: bool) -> Res {
    let mut cfg = cfg_base(dormancy, d, seed);
    if with_sleeper {
        cfg.attack_kind = AttackKind::Sleeper { normal_until: (STEPS / 5) as u64 };
        cfg.attack_ratio = sleepers as f64 / N as f64;
    } else {
        cfg.attack_kind = AttackKind::None;
        cfg.attack_ratio = 0.0;
    }
    let mut world = tm_sim::World::new(cfg);
    world.run();
    let ratings = world.ratings();
    let accs = world.accounts();
    let n = accs.len();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let is_att: Vec<bool> = accs.iter().map(|a| a.kind.is_attacker()).collect();
    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| (0..=ROUNDS).map(|r| world.was_online_at(i, r * (STEPS as u64 / ROUNDS))).collect())
        .collect();

    let ecfg = EngineConfig {
        beta: 0.3, half_life: 72,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean, noise_multiplier: 1.0,
    };
    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n { e.intern(key(i)); }
        for (o, t, s) in &ratings { if *o == a { e.rate(*t as u32, *s, 0); } }
        nodes[a] = Some(e);
    }

    let mut had_gap = vec![false; n];
    let mut prev_on = vec![false; n];
    for r in 0..=ROUNDS {
        let now = r * (STEPS as u64 / ROUNDS);
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n];
        for &a in &alive {
            if online[a][r as usize] { if let Some(e) = &nodes[a] { views[a] = Some(e.export_view(now)); } }
        }
        for &a in &alive {
            if !online[a][r as usize] { prev_on[a] = false; had_gap[a] = true; continue; }
            let returning = had_gap[a] && !prev_on[a] && r > 0;
            let k = if returning { SAMPLES * 3 } else { SAMPLES };
            let reqs = { nodes[a].as_mut().unwrap().tick(now, k) };
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), now, now);
                }
            }
            prev_on[a] = true;
        }
    }

    let now = ROUNDS * (STEPS as u64 / ROUNDS);
    let (mut hs, mut hc, mut ss, mut sc) = (0.0f64, 0usize, 0.0f64, 0usize);
    let (mut rhs, mut rhc, mut rss, mut rsc) = (0.0f64, 0usize, 0.0f64, 0usize);
    let mut onl = 0usize;
    for &obs in &alive {
        if is_att[obs] || !online[obs][ROUNDS as usize] { continue; }
        onl += 1;
        let e = nodes[obs].as_ref().unwrap();
        let dist = e.trust_distribution(now);
        let ret = had_gap[obs];
        for (idx, v) in dist {
            let a = idx as usize;
            if is_att[a] { ss += v as f64; sc += 1; if ret { rss += v as f64; rsc += 1; } }
            else { hs += v as f64; hc += 1; if ret { rhs += v as f64; rhc += 1; } }
        }
    }
    Res {
        honest: if hc > 0 { hs / hc as f64 } else { 0.0 },
        sleeper: if sc > 0 { ss / sc as f64 } else { 0.0 },
        ret_honest: if rhc > 0 { rhs / rhc as f64 } else { 0.0 },
        ret_sleeper: if rsc > 0 { rss / rsc as f64 } else { 0.0 },
        ret_n: rhc,
        onl, alive: alive.len(),
    }
}

fn main() {
    let sw = std::time::Instant::now();
    println!("潜伏完整网格 · 真多 runtime（每节点一个 Engine）");
    println!("{} 账户 × {} 步，{} 轮交换，每轮抽 {} 个\n", N, STEPS, ROUNDS, SAMPLES);
    println!("{:>6} {:>7} {:>5} {:>8} {:>10} {:>10} {:>8} {:>10} {:>10} {:>7}",
        "p目标", "p实测", "D(h)", "潜伏者", "诚实", "潜伏", "比值", "回归看诚", "回归看潜", "回归n");

    for (p, d) in [(0.30f64, 72u64), (0.30, 216), (0.30, 720),
                   (0.50, 72), (0.50, 216), (0.50, 720),
                   (0.70, 72), (0.70, 216), (0.70, 720),
                   (0.90, 72), (0.90, 216), (0.90, 720)] {
        let (rate, achieved) = calibrate(p, d, 21);
        for s in [1usize, 5, 20] {
            let r = run_world(rate, d, 21, s, true);
            println!("{:>5.0}% {:>6.0}% {:>5} {:>8} {:>10.6} {:>10.6} {:>8.2} {:>10.6} {:>10.6} {:>7}",
                p * 100.0, achieved * 100.0, d, s,
                r.honest, r.sleeper,
                if r.honest > 0.0 { r.sleeper / r.honest } else { 0.0 },
                r.ret_honest, r.ret_sleeper, r.ret_n);
        }
    }
    println!("\n总耗时 {:.0}s", sw.elapsed().as_secs_f64());
}
