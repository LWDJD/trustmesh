//! L1：用户更替速度 × 潜伏攻击
//!
//! 真多 runtime：每个账户一个 tm_engine::Engine，离线节点不 tick、不收观点，
//! 观点年龄真实增长（衰减生效），回归节点做快速刷新。模板是 crates/exp/src/bin/grid.rs。
//!
//! 网格：join_rate ∈ {0.0, 0.05, 0.2, 0.5} 新用户/小时
//!     × leave_rate ∈ {0.0, 0.001, 0.004, 0.01} 流失/小时
//!     × seed ∈ {11, 23, 37}
//!
//! 其余固定：n_initial=300, steps=400, sleep_hours=(216,260), return_rate=0.10,
//!          attack_ratio=1/300（单个 Sleeper，normal_until=80）, enable_rhythm=false。
//!
//! 用法：
//!   cargo run --release --bin l1_churn -- calib           # 标定 dormancy_rate ~= 50% 离线
//!   cargo run --release --bin l1_churn -- grid            # 跑满 16 格 × 3 seed
//!   cargo run --release --bin l1_churn -- grid12          # 跑满 16 格 × 12 seed（压噪声）
//!   cargo run --release --bin l1_churn -- cell 0.2 0.004  # 只跑一格（3 seed）
//!   cargo run --release --bin l1_churn -- seed 0.2 0.004 11  # 单次运行（诊断）
use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Life, SimConfig};
use tm_view::ViewParams;

const N_INIT: usize = 300;
const STEPS: usize = 400;
const ROUNDS: u64 = 10;
const TICK: u64 = STEPS as u64 / ROUNDS; // 40 小时一轮
const SAMPLES: usize = 12;
const SEEDS: [u64; 3] = [11, 23, 37];
/// 补充 seed：任务只要求 3 个，这里再加 9 个把 16 格的均值误差压下来。
const EXTRA_SEEDS: [u64; 9] = [41, 53, 67, 79, 83, 97, 101, 113, 127];
const NORMAL_UNTIL: u64 = STEPS as u64 / 5; // 80
const SLEEP: (u64, u64) = (216, 260);
const RETURN_RATE: f64 = 0.10;

/// 标定结果：join=leave=0 时在 T=400 得到约 50% 存活账户离线。
/// 由 calib 模式在 3 个 seed 上取均值后选定，全网格固定不变。
const DORMANCY: f64 = 0.00152685; // ← calib 模式二分得到

const JOINS: [f64; 4] = [0.0, 0.05, 0.2, 0.5];
const LEAVES: [f64; 4] = [0.0, 0.001, 0.004, 0.01];

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn ecfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
    }
}

fn cfg_of(join: f64, leave: f64, dormancy: f64, seed: u64, sleepers: f64) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS,
        seed,
        join_rate: join,
        leave_rate: leave,
        dormancy_rate: dormancy,
        sleep_hours: SLEEP,
        return_rate: RETURN_RATE,
        enable_rhythm: false,
        attack_kind: AttackKind::Sleeper { normal_until: NORMAL_UNTIL },
        attack_ratio: sleepers / N_INIT as f64,
        ..Default::default()
    }
}

// ════════════════════════ 标定：dormancy_rate → 离线比例 ════════════════════════

fn offline_frac(dormancy: f64, seed: u64) -> f64 {
    let mut w = tm_sim::World::new(cfg_of(0.0, 0.0, dormancy, seed, 1.0));
    w.run();
    let accs = w.accounts();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    if alive.is_empty() {
        return 0.0;
    }
    let t = STEPS as u64;
    let off = alive.iter().filter(|i| !w.was_online_at(**i, t)).count();
    off as f64 / alive.len() as f64
}

fn mean3(f: impl Fn(u64) -> f64) -> f64 {
    SEEDS.iter().map(|s| f(*s)).sum::<f64>() / SEEDS.len() as f64
}

fn run_calibration() {
    println!("# 标定 dormancy_rate：join=0, leave=0, sleep=(216,260), return=0.10, T=400");
    let (mut lo, mut hi) = (1.0e-6f64, 0.05f64);
    let mut best = (lo, mean3(|s| offline_frac(lo, s)));
    for _ in 0..12 {
        let mid = 0.5 * (lo + hi);
        let f = mean3(|s| offline_frac(mid, s));
        if (f - 0.5).abs() < (best.1 - 0.5).abs() {
            best = (mid, f);
        }
        if f < 0.5 { lo = mid } else { hi = mid }
    }
    for &r in &[1.0e-4f64, 5.0e-4, 1.0e-3, 2.0e-3, 3.0e-3, 4.0e-3, 6.0e-3, 1.0e-2] {
        let fs: Vec<f64> = SEEDS.iter().map(|&s| offline_frac(r, s)).collect();
        println!(
            "rate={:.6} offline={:.4} (seed11={:.4} seed23={:.4} seed37={:.4})",
            r,
            fs.iter().sum::<f64>() / 3.0,
            fs[0], fs[1], fs[2]
        );
    }
    let fs: Vec<f64> = SEEDS.iter().map(|&s| offline_frac(best.0, s)).collect();
    println!();
    println!("# 选定 dormancy_rate = {:.8} -> 平均离线 {:.4}", best.0, best.1);
    for (i, &s) in SEEDS.iter().enumerate() {
        println!("#   seed={} offline={:.4}", s, fs[i]);
    }
}

// ════════════════════════ 单次运行 ════════════════════════

#[derive(Clone, Default)]
struct Raw {
    total: usize,
    alive: usize,
    online: usize,
    sl_state: f64, // 0=Active 1=Dormant 2=Left
    sl_left_at: u64,
    sl_quality: f32,
    obs_n: usize,
    vis_n: usize, // 潜伏者份额 > 0 的观察者数
    sl_share: f64,
    honest_mean: f64,
    ratio: f64,
    ret_n: usize,
    ret_sl: f64,
    ret_hn: f64,
    ret_ratio: f64,
    new_n: usize,
    new_sl: f64,
    new_hn: f64,
    new_ratio: f64,
    old_n: usize,
    old_sl: f64,
    old_hn: f64,
    old_ratio: f64,
    indeg: usize,
    indeg_new: usize,
    indeg_online: usize,
    matched_n: usize,
    matched_mean: f64,
    matched_ratio: f64,
    coh_old: f64,
    coh_new: f64,
    coh_ratio: f64,
    joiners: usize,
    leavers: usize,
}

fn run_one(join: f64, leave: f64, dormancy: f64, seed: u64, sleepers: f64) -> Raw {
    let mut world = tm_sim::World::new(cfg_of(join, leave, dormancy, seed, sleepers));
    world.run();

    let ratings = world.ratings();
    let accs = world.accounts();
    // 坑 1：账户编号基数必须用 accounts().len()，alive_count() 更小会撞号。
    let n = accs.len();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let is_att: Vec<bool> = accs.iter().map(|a| a.kind.is_attacker()).collect();
    let joined: Vec<u64> = accs.iter().map(|a| a.joined_at).collect();
    let quality: Vec<f32> = accs.iter().map(|a| a.quality).collect();
    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| (0..=ROUNDS).map(|r| world.was_online_at(i, r * TICK)).collect())
        .collect();

    let sleeper = *world.attack.attackers.first().expect("必须有潜伏者");
    let sl_q = quality[sleeper];
    let (sl_state, sl_left_at) = match accs[sleeper].life {
        Life::Active => (0.0, 0u64),
        Life::Dormant { .. } => (1.0, 0u64),
        Life::Left { at } => (2.0, at),
    };

    let alive_set: std::collections::HashSet<usize> = alive.iter().copied().collect();
    let online_final: Vec<usize> = (0..n)
        .filter(|&i| alive_set.contains(&i) && online[i][ROUNDS as usize] && !is_att[i])
        .collect();
    let alive_honest: Vec<usize> = alive.iter().copied().filter(|&i| !is_att[i]).collect();
    let matched: Vec<usize> = alive_honest
        .iter()
        .copied()
        .filter(|&i| (quality[i] - sl_q).abs() <= 0.05)
        .collect();
    let alive_old: Vec<usize> = alive_honest.iter().copied().filter(|&i| joined[i] == 0).collect();
    let alive_new: Vec<usize> = alive_honest.iter().copied().filter(|&i| joined[i] > 0).collect();

    // ── 运行时：每账户一个 Engine ──
    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg());
        for i in 0..n {
            e.intern(key(i));
        }
        for (o, t, s) in &ratings {
            if *o == a {
                e.rate(*t as u32, *s, 0);
            }
        }
        nodes[a] = Some(e);
    }

    // 断网历史：只看「它已经存在」的那些轮
    let mut had_gap = vec![false; n];
    for i in 0..n {
        let mut fr = 0u64;
        while fr < ROUNDS && fr * TICK < joined[i] {
            fr += 1;
        }
        had_gap[i] = (fr..ROUNDS).any(|r| !online[i][r as usize]);
    }

    for r in 0..=ROUNDS {
        let now = r * TICK;
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n];
        for &a in &alive {
            if online[a][r as usize] {
                if let Some(e) = &nodes[a] {
                    views[a] = Some(e.export_view(now));
                }
            }
        }
        for &a in &alive {
            if !online[a][r as usize] {
                continue;
            }
            let reqs = { nodes[a].as_mut().unwrap().tick(now, SAMPLES) };
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), now, now);
                }
            }
        }
    }

    // ── 末轮测量 ──
    let now = ROUNDS * TICK;
    let mut out = Raw {
        total: n,
        alive: alive.len(),
        online: online_final.len(),
        sl_state,
        sl_left_at,
        sl_quality: sl_q,
        obs_n: online_final.len(),
        indeg: accs[sleeper].received_count,
        joiners: (0..n).filter(|&i| joined[i] > 0).count(),
        leavers: accs.iter().filter(|a| a.is_left()).count(),
        ..Default::default()
    };

    // 入度拆分：谁给潜伏者打过正分
    {
        let mut indeg_new = 0usize;
        let mut indeg_online = 0usize;
        for &o in &alive {
            if !is_att[o] && accs[o].rating_of(sleeper).is_some() {
                if joined[o] > 0 {
                    indeg_new += 1;
                }
                if online[o][ROUNDS as usize] {
                    indeg_online += 1;
                }
            }
        }
        out.indeg_new = indeg_new;
        out.indeg_online = indeg_online;
    }

    let (mut sl_s, mut hn_s) = (0.0f64, 0.0f64);
    let mut vis_n = 0usize;
    let (mut rs, mut rh, mut rn) = (0.0f64, 0.0f64, 0usize);
    let (mut ns, mut nh, mut nn) = (0.0f64, 0.0f64, 0usize);
    let (mut os, mut oh, mut on_) = (0.0f64, 0.0f64, 0usize);
    let (mut ms, mut mn) = (0.0f64, 0usize);
    let (mut co_old, mut co_new, mut co_n) = (0.0f64, 0.0f64, 0usize);
    let mut obs_n = 0usize;

    for &obs in &online_final {
        let e = match nodes[obs].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let dist = e.trust_distribution(now);
        let mut buf = vec![0.0f32; n];
        for (idx, v) in &dist {
            buf[*idx as usize] = *v;
        }
        let sl = buf[sleeper] as f64;
        // 坑 2：潜伏者与诚实账户分开计数，绝不共用一个分母
        let h_sum: f64 = alive_honest.iter().map(|&i| buf[i] as f64).sum();
        let h_mean = if alive_honest.is_empty() { 0.0 } else { h_sum / alive_honest.len() as f64 };
        obs_n += 1;
        if sl > 0.0 {
            vis_n += 1;
        }
        sl_s += sl;
        hn_s += h_mean;

        if had_gap[obs] {
            rn += 1;
            rs += sl;
            rh += h_mean;
        }
        if joined[obs] > 0 {
            nn += 1;
            ns += sl;
            nh += h_mean;
        } else {
            on_ += 1;
            os += sl;
            oh += h_mean;
        }
        if !matched.is_empty() {
            let m_sum: f64 = matched.iter().map(|&i| buf[i] as f64).sum();
            ms += m_sum / matched.len() as f64;
            mn += 1;
        }
        // 老账户 / 新账户各自的平均份额（不含攻击者）
        co_n += 1;
        if !alive_old.is_empty() {
            let s: f64 = alive_old.iter().map(|&i| buf[i] as f64).sum();
            co_old += s / alive_old.len() as f64;
        }
        if !alive_new.is_empty() {
            let s: f64 = alive_new.iter().map(|&i| buf[i] as f64).sum();
            co_new += s / alive_new.len() as f64;
        }
    }

    out.obs_n = obs_n;
    out.vis_n = vis_n;
    out.sl_share = div(sl_s, obs_n as f64);
    out.honest_mean = div(hn_s, obs_n as f64);
    out.ratio = div(out.sl_share, out.honest_mean);
    out.ret_n = rn;
    out.ret_sl = div(rs, rn as f64);
    out.ret_hn = div(rh, rn as f64);
    out.ret_ratio = div(out.ret_sl, out.ret_hn);
    out.new_n = nn;
    out.new_sl = div(ns, nn as f64);
    out.new_hn = div(nh, nn as f64);
    out.new_ratio = div(out.new_sl, out.new_hn);
    out.old_n = on_;
    out.old_sl = div(os, on_ as f64);
    out.old_hn = div(oh, on_ as f64);
    out.old_ratio = div(out.old_sl, out.old_hn);
    out.matched_n = mn;
    out.matched_mean = div(ms, mn as f64);
    out.matched_ratio = div(out.sl_share, out.matched_mean);
    out.coh_old = div(co_old, co_n as f64);
    out.coh_new = div(co_new, co_n as f64);
    out.coh_ratio = div(out.coh_old, out.coh_new);
    out
}

fn div(a: f64, b: f64) -> f64 {
    if b.abs() < 1e-12 { 0.0 } else { a / b }
}

// ════════════════════════ 输出 ════════════════════════

fn state_label(v: f64) -> &'static str {
    match v as i32 {
        0 => "active",
        1 => "dormant",
        _ => "left",
    }
}

fn raw_line(join: f64, leave: f64, seed: u64, r: &Raw) -> String {
    format!(
        "RAW,{join},{leave},{seed},{},{},{},{:.4},{},{},{:.4},{},{},{},{:.6},{:.6},{:.4},{},{:.6},{:.6},{:.4},{},{:.6},{:.6},{:.4},{},{:.6},{:.6},{:.4},{},{},{},{:.6},{:.4},{:.6},{:.6},{:.4},{},{}",
        r.total, r.alive, r.online, 1.0 - div(r.online as f64, r.alive as f64),
        state_label(r.sl_state), r.sl_left_at, r.sl_quality,
        r.obs_n, r.vis_n, r.sl_share, r.honest_mean, r.ratio,
        r.ret_n, r.ret_sl, r.ret_hn, r.ret_ratio,
        r.new_n, r.new_sl, r.new_hn, r.new_ratio,
        r.old_n, r.old_sl, r.old_hn, r.old_ratio,
        r.indeg, r.indeg_new, r.indeg_online,
        r.matched_n, r.matched_mean, r.matched_ratio,
        r.coh_old, r.coh_new, r.coh_ratio,
        r.joiners, r.leavers
    )
}

fn agg(v: &[f64]) -> (f64, f64) {
    let n = v.len() as f64;
    let m = v.iter().sum::<f64>() / n;
    let sd = if v.len() < 2 {
        0.0
    } else {
        (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0)).sqrt()
    };
    (m, sd)
}

fn cell_line(tag: &str, join: f64, leave: f64, rows: &[Raw]) -> String {
    let g = |f: fn(&Raw) -> f64| agg(&rows.iter().map(f).collect::<Vec<_>>());
    let fields: Vec<(&str, (f64, f64))> = vec![
        ("total", g(|r| r.total as f64)),
        ("alive", g(|r| r.alive as f64)),
        ("online", g(|r| r.online as f64)),
        ("obs", g(|r| r.obs_n as f64)),
        ("vis", g(|r| r.vis_n as f64)),
        ("vis_frac", g(|r| div(r.vis_n as f64, r.obs_n as f64))),
        ("indeg", g(|r| r.indeg as f64)),
        ("indeg_new", g(|r| r.indeg_new as f64)),
        ("indeg_online", g(|r| r.indeg_online as f64)),
        ("sl_share", g(|r| r.sl_share)),
        ("h_mean", g(|r| r.honest_mean)),
        ("ratio", g(|r| r.ratio)),
        ("ret_n", g(|r| r.ret_n as f64)),
        ("ret_sl", g(|r| r.ret_sl)),
        ("ret_h", g(|r| r.ret_hn)),
        ("ret_ratio", g(|r| r.ret_ratio)),
        ("new_sl", g(|r| r.new_sl)),
        ("new_h", g(|r| r.new_hn)),
        ("new_ratio", g(|r| r.new_ratio)),
        ("old_sl", g(|r| r.old_sl)),
        ("old_h", g(|r| r.old_hn)),
        ("old_ratio", g(|r| r.old_ratio)),
        ("match_mean", g(|r| r.matched_mean)),
        ("match_ratio", g(|r| r.matched_ratio)),
        ("coh_old", g(|r| r.coh_old)),
        ("coh_new", g(|r| r.coh_new)),
        ("coh_ratio", g(|r| r.coh_ratio)),
        ("joiners", g(|r| r.joiners as f64)),
        ("leavers", g(|r| r.leavers as f64)),
    ];
    let mut s = format!("CELL,{tag},{join},{leave}");
    for (name, (m, sd)) in &fields {
        s.push_str(&format!(",{name}={m:.6}/{sd:.6}"));
    }
    s
}

fn run_cell_all_seeds(join: f64, leave: f64, seeds: &[u64]) -> Vec<Raw> {
    let mut rows = Vec::new();
    for &seed in seeds {
        rows.push(run_one(join, leave, DORMANCY, seed, 1.0));
    }
    rows
}

/// 3 seed（任务要求）与 12 seed（补强）都从同一次运行里取子集，不重复跑。
/// [start,end) 是格子下标（join 外层、leave 内层），用来把 16 格切给多个进程并行跑。
fn run_grid(extra: bool, start: usize, end: usize, out: Option<&str>) {
    let sw = std::time::Instant::now();
    let mut all: Vec<u64> = SEEDS.to_vec();
    if extra {
        all.extend_from_slice(&EXTRA_SEEDS);
    }
    let mut buf = String::new();
    buf.push_str(&format!("# L1 churn grid: n_initial={N_INIT} steps={STEPS} rounds={ROUNDS} tick={TICK} samples={SAMPLES}\n"));
    buf.push_str(&format!("# dormancy={DORMANCY} sleep={SLEEP:?} return_rate={RETURN_RATE} normal_until={NORMAL_UNTIL}\n"));
    buf.push_str(&format!("# attack=Sleeper attack_ratio=1/{N_INIT} seeds3={SEEDS:?} extra={EXTRA_SEEDS:?}\n"));
    buf.push_str("# RAW 字段：total,alive,online,offline%,sl_state,sl_left_at,sl_quality,obs,vis,sl_share,h_mean,ratio,ret_n,ret_sl,ret_h,ret_ratio,new_n,new_sl,new_h,new_ratio,old_n,old_sl,old_h,old_ratio,indeg,indeg_new,indeg_online,match_n,match_mean,match_ratio,coh_old,coh_new,coh_ratio,joiners,leavers\n");
    let mut idx = 0usize;
    for &j in &JOINS {
        for &l in &LEAVES {
            let i = idx;
            idx += 1;
            if i < start || i >= end {
                continue;
            }
            let cw = std::time::Instant::now();
            let rows = run_cell_all_seeds(j, l, &all);
            for (k, r) in rows.iter().enumerate() {
                buf.push_str(&raw_line(j, l, all[k], r));
                buf.push('\n');
            }
            buf.push_str(&cell_line("3", j, l, &rows[..SEEDS.len()]));
            buf.push('\n');
            if extra {
                buf.push_str(&cell_line("12", j, l, &rows));
                buf.push('\n');
            }
            println!("cell {i} done: join={j} leave={l} in {:.0}s", cw.elapsed().as_secs_f64());
        }
    }
    match out {
        Some(p) => std::fs::write(p, buf).expect("写结果文件失败"),
        None => print!("{buf}"),
    }
    println!("# 总耗时 {:.1}s", sw.elapsed().as_secs_f64());
}

fn run_smoke(join: f64, leave: f64, seed: u64) {
    let mut world = tm_sim::World::new(cfg_of(join, leave, DORMANCY, seed, 1.0));
    world.run();
    let accs = world.accounts();
    let sleeper = *world.attack.attackers.first().unwrap();
    let a = &accs[sleeper];
    println!("sleeper idx={sleeper} kind={:?} quality={:.4} joined={}", a.kind, a.quality, a.joined_at);
    println!("attack_active={} target={:?} life={:?}", a.attack_active, world.attack.target, a.life);
    println!("follows={} ratings={} rated_self={}", a.follows.len(), a.ratings.len(), a.rating_of(sleeper).is_some());
    println!("received_count={} received_sum={:.4}", a.received_count, a.received_sum);
    println!("total={} alive={} online={}", accs.len(), world.alive_count(), world.online_count());
    let r = run_one(join, leave, DORMANCY, seed, 1.0);
    println!("{}", raw_line(join, leave, seed, &r));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()).unwrap_or("grid") {
        "calib" => run_calibration(),
        "cell" => {
            let j: f64 = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.0);
            let l: f64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(0.0);
            let rows = run_cell_all_seeds(j, l, &SEEDS);
            for (k, r) in rows.iter().enumerate() {
                println!("{}", raw_line(j, l, SEEDS[k], r));
            }
            println!("{}", cell_line("3", j, l, &rows));
        }
        "seed" => {
            let j: f64 = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.0);
            let l: f64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(0.0);
            let s: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(11);
            run_smoke(j, l, s);
        }
        "grid" => run_grid(false, 0, JOINS.len() * LEAVES.len(), None),
        "grid12" => run_grid(true, 0, JOINS.len() * LEAVES.len(), None),
        "grid12part" => {
            let a: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(0);
            let b: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(16);
            let o: Option<&str> = args.get(3).map(|s| s.as_str());
            run_grid(true, a, b, o);
        }
        other => eprintln!("unknown mode: {other}"),
    }
}
