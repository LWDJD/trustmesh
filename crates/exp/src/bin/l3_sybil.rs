//! L3 · 潜伏者批量引入女巫：影响力预算的稀释效果
//!
//! 真多 runtime：每个账户一个独立的 tm_engine::Engine，每轮导出/拉取观点，
//! 离线节点不 tick、不发布，观点年龄真实增长（衰减生效）。
//!
//! 网格：潜伏者 s ∈ {1,3,10,30} × 每个潜伏者引入的新女巫 N ∈ {0,1,5,20,100}
//! （N = 0 是对照：同一个世界、不注入任何女巫，用来量潜伏者自身声望的基准）
//! × seed ∈ {11,23,37}
//!
//! 关键约定：
//! - 新女巫起始编号 = world.accounts().len()（**不是** alive_count —— World 只增不删）
//! - 女巫只能被「引入它的那个潜伏者」打 1.0 分；它不进任何诚实账户的正分清单
//! - 统计诚实 / 潜伏者 / 女巫一律分开计数，不共用分母

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig};
use tm_view::ViewParams;

const N_INIT: usize = 300;
const STEPS: usize = 300;
const ROUNDS: u64 = 10;
const SAMPLES: usize = 12;
const SEEDS: [u64; 3] = [11, 23, 37];
const SLEEPER_SET: [usize; 4] = [1, 3, 10, 30];
/// 0 = 对照（不注入），其余为每个潜伏者引入的新女巫数
const SYBIL_SET: [usize; 5] = [0, 1, 5, 20, 100];
const TICK: u64 = (STEPS as u64) / ROUNDS;
const OFFLINE_TARGET: f64 = 0.60;
const RETURN_RATE: f64 = 0.10;
const SLEEP_HOURS: (u64, u64) = (168, 336);
const NORMAL_UNTIL: u64 = (STEPS / 5) as u64; // 60h 后突变

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn cfg_of(seed: u64, sleepers: usize, dormancy: f64) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        enable_rhythm: false,
        dormancy_rate: dormancy,
        sleep_hours: SLEEP_HOURS,
        return_rate: RETURN_RATE,
        attack_kind: if sleepers == 0 {
            AttackKind::None
        } else {
            AttackKind::Sleeper { normal_until: NORMAL_UNTIL }
        },
        attack_ratio: sleepers as f64 / N_INIT as f64,
        ..Default::default()
    }
}

/// 某个 dormancy_rate 下的离线比例（分母 = 存活账户数）
fn offline_frac(dormancy: f64, seed: u64, sleepers: usize) -> f64 {
    let mut w = tm_sim::World::new(cfg_of(seed, sleepers, dormancy));
    w.run();
    let accs = w.accounts();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    if alive.is_empty() {
        return 0.0;
    }
    let off = alive
        .iter()
        .filter(|&&i| !w.was_online_at(i, STEPS as u64))
        .count();
    off as f64 / alive.len() as f64
}

/// 二分标定：让「12 个基础世界」的平均离线比例命中目标
fn calibrate(target: f64) -> f64 {
    let score = |d: f64| -> f64 {
        let mut sum = 0.0;
        let mut n = 0usize;
        for &s in &SLEEPER_SET {
            for &seed in &SEEDS {
                sum += offline_frac(d, seed, s);
                n += 1;
            }
        }
        sum / n as f64
    };
    let (mut lo, mut hi) = (1e-4f64, 0.6f64);
    let mut best = (lo, score(lo));
    for _ in 0..10 {
        let mid = 0.5 * (lo + hi);
        let f = score(mid);
        eprintln!("  calibrate d={mid:.5} -> offline={f:.3}");
        if (f - target).abs() < (best.1 - target).abs() {
            best = (mid, f);
        }
        if f < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    eprintln!("  calibrate best d={:.5} -> offline={:.4}", best.0, best.1);
    best.0
}

#[derive(Clone, Copy, Default)]
struct Row {
    n_total: usize,
    alive: usize,
    offline: f64,
    obs_n: usize,
    sleeper_online: usize,
    honest_base: f64,
    honest_p50: f64,
    sybil_mean: f64,
    ratio: f64,
    effective: usize,
    effective_p50: usize,
    ratio_exposed: f64,
    ratio_own: f64,
    syb_hit: f64,
    sleeper_mean: f64,
    sleeper_ratio: f64,
    target_promoted: f64,
    syb_share: f64,
    s0: f64,
    sleeper_raters: f64,
    honest_raters: f64,
    exposure: f64,
}

/// 世界已经跑完；本函数注入女巫、跑引擎、统计。
fn engine_phase(world: &tm_sim::World, n_per: usize) -> Row {
    let accs = world.accounts();
    // 坑 1：新账户编号必须用 accounts().len()，绝不能用 alive_count()
    let n0 = accs.len();
    let sleepers: Vec<usize> = accs
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    let n_syb = sleepers.len() * n_per;
    let n_total = n0 + n_syb;
    let sybils: Vec<usize> = (0..n_syb).map(|x| n0 + x).collect();

    // 修复 27e970f 之后：潜伏者的收益方是一个独立的 Promoted 账户（全world一个）
    let promoted: Option<usize> = accs
        .iter()
        .find(|a| a.kind == Kind::Promoted)
        .map(|a| a.idx);

    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let honest_alive: Vec<usize> = accs
        .iter()
        .filter(|a| a.kind == Kind::Honest && a.is_alive())
        .map(|a| a.idx)
        .collect();
    let offline = {
        let off = alive
            .iter()
            .filter(|&&i| !world.was_online_at(i, STEPS as u64))
            .count();
        if alive.is_empty() { 0.0 } else { off as f64 / alive.len() as f64 }
    };

    // 诊断：平均被多少人打过分
    let honest_raters = if honest_alive.is_empty() {
        0.0
    } else {
        honest_alive.iter().map(|&i| accs[i].received_count as f64).sum::<f64>()
            / honest_alive.len() as f64
    };
    let sleeper_raters = if sleepers.is_empty() {
        0.0
    } else {
        sleepers.iter().map(|&i| accs[i].received_count as f64).sum::<f64>()
            / sleepers.len() as f64
    };

    // 每个观察者的正分清单（世界给出的部分）
    let mut per_obs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n0];
    let s0 = {
        let mut sums = vec![0.0f64; n0];
        for (o, t, s) in world.ratings() {
            per_obs[o].push((t, s));
            sums[o] += s as f64;
        }
        let mut acc = 0.0;
        for &sl in &sleepers {
            acc += sums[sl];
        }
        if sleepers.is_empty() { 0.0 } else { acc / sleepers.len() as f64 }
    };
    // 注入：第 k 个潜伏者 -> 它的 N 个女巫，各 1.0
    for (k, &sl) in sleepers.iter().enumerate() {
        for j in 0..n_per {
            per_obs[sl].push((n0 + k * n_per + j, 1.0));
        }
    }

    let online: Vec<Vec<bool>> = (0..n0)
        .map(|i| {
            (0..=ROUNDS)
                .map(|r| world.was_online_at(i, r * TICK))
                .collect()
        })
        .collect();

    let ecfg = EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        coverage_exponent: 1.0,
        remove_when_coverage: 0.0,
        group_half_life: 0,
        unreachable_after: 0,
        min_group_sources: 0,
        ..Default::default()
    };
    let mut nodes: Vec<Option<Engine>> = (0..n_total).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n_total {
            e.intern(key(i));
        }
        for &(t, s) in &per_obs[a] {
            e.rate(t as u32, s, 0);
        }
        nodes[a] = Some(e);
    }

    let mut had_gap = vec![false; n0];
    let mut prev_on = vec![false; n0];
    for r in 0..=ROUNDS {
        let now = r * TICK;
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n_total];
        for &a in &alive {
            if online[a][r as usize] {
                if let Some(e) = &nodes[a] {
                    views[a] = Some(e.export_view(now));
                }
            }
        }
        for &a in &alive {
            if !online[a][r as usize] {
                prev_on[a] = false;
                had_gap[a] = true;
                continue;
            }
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

    // ── 统计：诚实 / 潜伏者 / 女巫 三套独立计数器 ──
    let now = ROUNDS * TICK;
    let mut buf = vec![0.0f32; n_total];
    let mut honest_vals: Vec<f64> = Vec::new();
    let (mut hs, mut hc) = (0.0f64, 0usize); // 诚实目标
    let (mut ss, mut sc) = (0.0f64, 0usize); // 女巫（全体观察者）
    let (mut es, mut ec) = (0.0f64, 0usize); // 女巫（只看「认识至少一个潜伏者」的观察者）
    let (mut os, mut oc) = (0.0f64, 0usize); // 女巫（只看「认识引入它的那个潜伏者」的观察者）
    let (mut ls, mut lc) = (0.0f64, 0usize); // 潜伏者
    let (mut ts, mut tc) = (0.0f64, 0usize); // Promoted 收益方
    let mut per_syb = vec![0.0f64; n_syb];
    let mut obs_n = 0usize;
    let mut expo = 0usize;
    let mut syb_hit = 0usize;

    for &obs in &honest_alive {
        if !online[obs][ROUNDS as usize] {
            continue;
        }
        let e = match nodes[obs].as_ref() {
            Some(e) => e,
            None => continue,
        };
        obs_n += 1;
        let exposed = sleepers.iter().any(|&sl| e.share_of(sl as u32).is_some());
        if exposed {
            expo += 1;
        }
        for v in buf.iter_mut() {
            *v = 0.0;
        }
        for (u, v) in e.trust_distribution(now) {
            buf[u as usize] = v;
        }
        for &t in &honest_alive {
            if t != obs {
                let v = buf[t] as f64;
                hs += v;
                hc += 1;
                honest_vals.push(v);
            }
        }
        let mut hit = false;
        for (k, &sy) in sybils.iter().enumerate() {
            let v = buf[sy] as f64;
            per_syb[k] += v;
            ss += v;
            sc += 1;
            if v > 0.0 {
                hit = true;
            }
            if exposed {
                es += v;
                ec += 1;
            }
            if n_per > 0 && e.share_of(sleepers[k / n_per] as u32).is_some() {
                os += v;
                oc += 1;
            }
        }
        if hit {
            syb_hit += 1;
        }
        for &sl in &sleepers {
            ls += buf[sl] as f64;
            lc += 1;
        }
        if let Some(t0) = promoted {
            ts += buf[t0] as f64;
            tc += 1;
        }
    }

    honest_vals.sort_by(|a, b| a.total_cmp(b));
    let honest_p50 = if honest_vals.is_empty() {
        0.0
    } else {
        honest_vals[honest_vals.len() / 2]
    };
    let honest_base = if hc > 0 { hs / hc as f64 } else { 0.0 };
    let sybil_mean = if sc > 0 { ss / sc as f64 } else { 0.0 };
    let sybil_exposed = if ec > 0 { es / ec as f64 } else { 0.0 };
    let sybil_own = if oc > 0 { os / oc as f64 } else { 0.0 };
    let sleeper_mean = if lc > 0 { ls / lc as f64 } else { 0.0 };
    let target_promoted = if tc > 0 { ts / tc as f64 } else { 0.0 };
    let effective = if obs_n > 0 {
        per_syb.iter().filter(|&&v| v / obs_n as f64 > honest_base).count()
    } else {
        0
    };
    let effective_p50 = if obs_n > 0 {
        per_syb.iter().filter(|&&v| v / obs_n as f64 > honest_p50).count()
    } else {
        0
    };

    // 潜伏者给自己女巫的「份额」（影响力预算的直接读法）
    let (mut sh, mut shc) = (0.0f64, 0usize);
    for (k, &sl) in sleepers.iter().enumerate() {
        if let Some(e) = nodes[sl].as_ref() {
            for j in 0..n_per {
                if let Some(x) = e.share_of((n0 + k * n_per + j) as u32) {
                    sh += x as f64;
                    shc += 1;
                }
            }
        }
    }

    Row {
        n_total,
        alive: alive.len(),
        offline,
        obs_n,
        sleeper_online: sleepers.iter().filter(|&&i| online[i][ROUNDS as usize]).count(),
        honest_base,
        honest_p50,
        sybil_mean,
        ratio: if honest_base > 0.0 { sybil_mean / honest_base } else { 0.0 },
        effective,
        effective_p50,
        ratio_exposed: if honest_base > 0.0 { sybil_exposed / honest_base } else { 0.0 },
        ratio_own: if honest_base > 0.0 { sybil_own / honest_base } else { 0.0 },
        syb_hit: if obs_n > 0 { syb_hit as f64 / obs_n as f64 } else { 0.0 },
        sleeper_mean,
        sleeper_ratio: if honest_base > 0.0 { sleeper_mean / honest_base } else { 0.0 },
        target_promoted,
        syb_share: if shc > 0 { sh / shc as f64 } else { 0.0 },
        s0,
        sleeper_raters,
        honest_raters,
        exposure: if obs_n > 0 { expo as f64 / obs_n as f64 } else { 0.0 },
    }
}

fn main() {
    // 诊断模式：只跑 12 个世界，不建引擎；用于核对收益方/潜伏者的存活与曝光
    if std::env::args().any(|a| a == "diag") {
        let dorm = calibrate(OFFLINE_TARGET);
        println!("# 标定 dormancy_rate = {dorm:.6}");
        println!(
            "{:>3} {:>5} {:>6} {:>5} {:>9} {:>7} {:>7} {:>7} {:>9} {:>9} {:>9} {:>11} {:>9}",
            "s", "seed", "n_tot", "存活", "收益方idx", "活着", "在线", "休眠", "被打分", "收到均分", "质量", "潜伏在线/活", "认识潜者"
        );
        for &s in &SLEEPER_SET {
            for &seed in &SEEDS {
                let mut w = tm_sim::World::new(cfg_of(seed, s, dorm));
                w.run();
                let accs = w.accounts();
                let sleepers: Vec<usize> = accs
                    .iter()
                    .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
                    .map(|a| a.idx)
                    .collect();
                let promoted = accs.iter().find(|a| a.kind == Kind::Promoted).map(|a| a.idx);
                let alive = accs.iter().filter(|a| a.is_alive()).count();
                let honest_online: Vec<usize> = accs
                    .iter()
                    .filter(|a| a.kind == Kind::Honest && a.is_alive() && w.was_online_at(a.idx, STEPS as u64))
                    .map(|a| a.idx)
                    .collect();
                let knowers = honest_online
                    .iter()
                    .filter(|&&i| accs[i].ratings.iter().any(|(t, _)| sleepers.contains(t)))
                    .count();
                let (p_idx, p_alive, p_online, p_dorm, p_cnt, p_mean, p_q) = match promoted {
                    Some(pi) => {
                        let a = &accs[pi];
                        (
                            pi.to_string(),
                            if a.is_alive() { "是" } else { "否" },
                            if w.was_online_at(pi, STEPS as u64) { "是" } else { "否" },
                            if a.is_dormant() { "是" } else { "否" },
                            a.received_count.to_string(),
                            format!("{:.3}", a.mean_received()),
                            format!("{:.3}", a.quality),
                        )
                    }
                    None => ("-".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into(), "-".into()),
                };
                let sl_alive = sleepers.iter().filter(|&&i| accs[i].is_alive()).count();
                let sl_online = sleepers.iter().filter(|&&i| w.was_online_at(i, STEPS as u64)).count();
                println!(
                    "{:>3} {:>5} {:>6} {:>5} {:>9} {:>7} {:>7} {:>7} {:>9} {:>9} {:>9} {:>11} {:>9}",
                    s, seed, accs.len(), alive, p_idx, p_alive, p_online, p_dorm, p_cnt, p_mean, p_q,
                    format!("{sl_online}/{sl_alive}"),
                    format!("{knowers}/{}", honest_online.len())
                );
            }
        }
        return;
    }
    let t_all = std::time::Instant::now();
    println!("L3 · 潜伏者批量引入女巫 · 真多 runtime（每账户一个 Engine + 时间衰减）");
    println!("n_initial={N_INIT} steps={STEPS} seed∈{SEEDS:?} join=0 leave=0 return_rate={RETURN_RATE}");
    println!("sleep_hours={SLEEP_HOURS:?} normal_until={NORMAL_UNTIL}h 轮数={ROUNDS}（每轮 {TICK}h）抽样={SAMPLES}");
    println!("β=0.3 half_life=72h buckets=2048 hashers=5 agg=Mean\n");

    let dorm = calibrate(OFFLINE_TARGET);
    println!("标定 dormancy_rate = {dorm:.6}（目标离线 {:.0}%）\n", OFFLINE_TARGET * 100.0);

    // 每个 (s, seed) 只跑一次世界；N 只影响注入，不影响世界
    let mut raw: Vec<(usize, usize, u64, Row)> = Vec::new();
    println!(
        "{:>3} {:>4} {:>5} {:>6} {:>5} {:>6} {:>6} {:>6} {:>10} {:>10} {:>10} {:>7} {:>9} {:>4} {:>4} {:>7} {:>7} {:>9} {:>9} {:>9} {:>6} {:>6}",
        "s", "N", "seed", "n_tot", "存活", "离线%", "观察者", "潜在线", "诚实均值", "诚实中位", "女巫均值", "女巫/诚实", "女巫/诚实知引者",
        "有效", "有效中位", "认识潜者%", "女巫命中%", "潜均值", "潜/诚实", "收益方Promoted", "份额1/(S0+N)", "S0"
    );
    for &s in &SLEEPER_SET {
        for &seed in &SEEDS {
            let mut world = tm_sim::World::new(cfg_of(seed, s, dorm));
            world.run();
            for &n_per in &SYBIL_SET {
                let r = engine_phase(&world, n_per);
                println!(
                    "{:>3} {:>4} {:>5} {:>6} {:>5} {:>6.1} {:>6} {:>6} {:>10.6} {:>10.6} {:>10.6} {:>7.3} {:>9.3} {:>4} {:>4} {:>7.1} {:>7.1} {:>9.6} {:>9.2} {:>9.6} {:>9.4} {:>6.2}",
                    s, n_per, seed, r.n_total, r.alive, r.offline * 100.0, r.obs_n, r.sleeper_online,
                    r.honest_base, r.honest_p50, r.sybil_mean, r.ratio, r.ratio_own, r.effective, r.effective_p50,
                    r.exposure * 100.0, r.syb_hit * 100.0,
                    r.sleeper_mean, r.sleeper_ratio, r.target_promoted, r.syb_share, r.s0
                );
                raw.push((s, n_per, seed, r));
            }
        }
    }

    // 诊断：潜伏者 vs 诚实账户的平均被打分次数
    println!("\n=== 诊断（每个 s×seed 一行，世界跑完后统计）===");
    println!("{:>3} {:>5} {:>6} {:>8} {:>10} {:>10} {:>10}", "s", "seed", "存活", "离线%", "诚实被打分数", "潜伏被打分数", "S0(潜伏评分和)");
    for &s in &SLEEPER_SET {
        for &seed in &SEEDS {
            if let Some((_, _, _, r)) = raw.iter().find(|(a, b, c, _)| *a == s && *b == 0 && *c == seed) {
                println!("{:>3} {:>5} {:>6} {:>8.1} {:>13.2} {:>13.2} {:>13.2}",
                    s, seed, r.alive, r.offline * 100.0, r.honest_raters, r.sleeper_raters, r.s0);
            }
        }
    }

    // ── 跨 seed 汇总：均值 ± 样本标准差 ──
    let stat = |v: &[f64]| -> (f64, f64) {
        let n = v.len() as f64;
        let m = v.iter().sum::<f64>() / n;
        let var = if v.len() > 1 {
            v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        (m, var.sqrt())
    };
    println!("\n=== 跨 seed 汇总（均值 ± 样本标准差，n=3）===");
    println!(
        "{:>3} {:>4} {:>19} {:>19} {:>15} {:>11} {:>15} {:>15} {:>19}",
        "s", "N", "诚实均值", "女巫均值", "女巫/诚实", "有效女巫", "认识潜者%", "潜/诚实", "份额(实测)"
    );
    println!("#TSV\ts\tN\thonest_base\thonest_p50\tsybil_mean\tratio\tratio_own\teffective\teffective_p50\tratio_exposed\tsybil_share\tsleeper_ratio\ttarget_promoted\texposure\tsyb_hit\tn_total\tobs");
    for &s in &SLEEPER_SET {
        for &n_per in &SYBIL_SET {
            let rows: Vec<&Row> = raw
                .iter()
                .filter(|(a, b, _, _)| *a == s && *b == n_per)
                .map(|(_, _, _, r)| r)
                .collect();
            let g = |f: fn(&Row) -> f64| -> Vec<f64> { rows.iter().map(|r| f(r)).collect() };
            let (hb, hb_sd) = stat(&g(|r| r.honest_base));
            let (h5, h5_sd) = stat(&g(|r| r.honest_p50));
            let (sm, sm_sd) = stat(&g(|r| r.sybil_mean));
            let (ra, ra_sd) = stat(&g(|r| r.ratio));
            let (ef, ef_sd) = stat(&g(|r| r.effective as f64));
            let (ef5, ef5_sd) = stat(&g(|r| r.effective_p50 as f64));
            let (re, re_sd) = stat(&g(|r| r.ratio_exposed));
            let (ro, ro_sd) = stat(&g(|r| r.ratio_own));
            let (sh, sh_sd) = stat(&g(|r| r.syb_share));
            let (lr, lr_sd) = stat(&g(|r| r.sleeper_ratio));
            let (tg, tg_sd) = stat(&g(|r| r.target_promoted));
            let (ex, ex_sd) = stat(&g(|r| r.exposure));
            let (hi, hi_sd) = stat(&g(|r| r.syb_hit));
            println!(
                "{:>3} {:>4} {:>8.3e}±{:<8.1e} {:>8.3e}±{:<8.1e} {:>6.3}±{:<6.3} {:>5.2}±{:<5.2} {:>6.1}±{:<6.1} {:>6.2}±{:<6.2} {:>8.4}±{:<8.4}",
                s, n_per, hb, hb_sd, sm, sm_sd, ra, ra_sd, ef, ef_sd, ex * 100.0, ex_sd * 100.0, lr, lr_sd, sh, sh_sd
            );
            println!(
                "#TSV\t{s}\t{n_per}\t{hb:.8e}\t{h5:.8e}\t{sm:.8e}\t{ra:.6}\t{ro:.6}\t{ef:.3}\t{ef5:.3}\t{re:.6}\t{sh:.8e}\t{lr:.6}\t{tg:.8e}\t{ex:.6}\t{hi:.6}\t{}\t{}",
                rows[0].n_total, rows[0].obs_n
            );
            let _ = (h5_sd, ef5_sd, re_sd, ro_sd, tg_sd, hi_sd, lr_sd);
        }
    }
    println!("\n总耗时 {:.1}s", t_all.elapsed().as_secs_f64());
}
