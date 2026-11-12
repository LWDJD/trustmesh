//! L2：离线时长 × 潜伏者优势 · 真多 runtime 版
//!
//! 模板 = crates/exp/src/bin/grid.rs：每个账户一个独立的 tm_engine::Engine。
//! 离线节点不 tick、不发布观点；观点年龄真实增长（衰减生效）；回归节点做快速刷新（3× 抽样）。
//!
//! 固化（任务给定）：
//!   n_initial=300, steps=400, seed∈{11,23,37},
//!   join_rate=0.05, leave_rate=0.001,
//!   return_rate=0.12（对照臂 0）,
//!   attack_kind=Sleeper{normal_until: steps/5=80}, attack_ratio=1/300
//! 扫描：sleep_hours=(D, 1.2D)，D∈{24,72,216,720,2160} h
//!
//! 仿真器版本：含 27e970f 修复（Sleeper 额外 spawn 一个 Kind::Promoted 作为收益方）。
//! 三类对象各自独立累加（绝不共用分母）：
//!   sleeper  潜伏者本身（攻击者）
//!   promoted 被抬举的收益方（不是攻击者，但必须从诚实基线里剔除）
//!   honest   诚实账户（基线 + 观察者集合）
//!
//! 三条臂：
//!   FIX  固定 dormancy（在 D=216 上标定到 ~60% 离线），五格共用 → D 是唯一变量
//!   CAL  每格单独标定到 ~60% 离线（任务字面要求）
//!   A5   附加臂：5 个潜伏者（attack_ratio=5/300），否则同 FIX
//!
//! 用法：
//!   l2            全网格
//!   l2 smoke      每臂一格快速冒烟
//!   l2 rounds=100 改观点交换时间分辨率（默认 50 轮 = 8h）
//!
//! 两个坑（已避开）：
//!   1. 账户编号一律取 world.accounts().len()，绝不用 alive_count()；
//!   2. sleeper / promoted / honest 三个累加器完全分开。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig};
use tm_view::ViewParams;

const N_INIT: usize = 300;
const STEPS: usize = 400;
const SEEDS: [u64; 3] = [11, 23, 37];
const DS: [u64; 5] = [24, 72, 216, 720, 2160];
const DEFAULT_ROUNDS: u64 = 50; // 每 8 小时一次观点交换（必须 << D_min）
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const NORMAL_UNTIL: u64 = (STEPS / 5) as u64;
const TARGET_OFF: f64 = 0.60;

static mut ROUNDS_ARG: u64 = DEFAULT_ROUNDS;

fn rounds() -> u64 {
    unsafe { ROUNDS_ARG }
}

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn make_cfg(d: u64, dorm: f64, seed: u64, ret: f64, n_sleepers: usize) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dorm,
        sleep_hours: (d, d * 6 / 5),
        return_rate: ret,
        enable_rhythm: false, // 离线只由休眠决定：离线时长 == sleep_hours
        attack_kind: AttackKind::Sleeper { normal_until: NORMAL_UNTIL },
        attack_ratio: n_sleepers as f64 / N_INIT as f64,
        ..Default::default()
    }
}

// ───────────────────────── 世界层统计（快，不带引擎） ─────────────────────────

#[derive(Clone, Copy, Default)]
struct WorldStat {
    off: f64,
    alive: usize,
    online: usize,
    accts: usize,
    ever: usize,
    leaves: usize,
    dorms: usize,
}

fn world_stat(cfg: &SimConfig) -> WorldStat {
    let mut w = tm_sim::World::new(cfg.clone());
    w.run();
    let t = STEPS as u64;
    let alive: Vec<usize> = w.accounts().iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let online = alive.iter().filter(|&&i| w.was_online_at(i, t)).count();
    let off = if alive.is_empty() { 0.0 } else { (alive.len() - online) as f64 / alive.len() as f64 };
    let ever = (0..w.accounts().len())
        .filter(|&i| {
            w.life_traces
                .get(i)
                .map(|tr| tr.events().iter().any(|(_, l)| matches!(l, tm_sim::Life::Dormant { .. })))
                .unwrap_or(false)
        })
        .count();
    WorldStat { off, alive: alive.len(), online, accts: w.accounts().len(), ever, leaves: w.leaves, dorms: w.dormancy_events }
}

/// 对数网格上找使「结束时离线比例」最接近 0.60 的 dormancy_rate。
fn calibrate(d: u64, seed: u64, ret: f64, n_sleepers: usize) -> (f64, WorldStat) {
    let mut best: Option<(f64, f64, WorldStat)> = None;
    for k in 0..=28 {
        let dorm = 1e-5 * 5000f64.powf(k as f64 / 28.0); // 1e-5 .. 0.05
        let st = world_stat(&make_cfg(d, dorm, seed, ret, n_sleepers));
        let err = (st.off - TARGET_OFF).abs();
        let better = match &best {
            None => true,
            Some((be, _, bs)) => err < *be - 1e-9 || ((err - *be).abs() <= 1e-9 && st.alive > bs.alive),
        };
        if better {
            best = Some((err, dorm, st));
        }
    }
    let (_, dorm, st) = best.unwrap();
    (dorm, st)
}

// ───────────────────────── 统计小工具 ─────────────────────────

fn median_u64(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        v[n / 2 - 1] / 2 + v[n / 2] / 2 + (v[n / 2 - 1] % 2 + v[n / 2] % 2) / 2
    }
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() { f64::NAN } else { v.iter().sum::<f64>() / v.len() as f64 }
}

fn corr(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 3 {
        return f64::NAN;
    }
    let mx = mean(&x[..n]);
    let my = mean(&y[..n]);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let dx = x[i] - mx;
        let dy = y[i] - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 { f64::NAN } else { sxy / (sxx * syy).sqrt() }
}

fn pct_rank(v: &[f64], x: f64) -> f64 {
    if v.is_empty() || x.is_nan() { return f64::NAN; }
    v.iter().filter(|&&y| y <= x).count() as f64 / v.len() as f64
}

#[derive(Default, Clone)]
struct TStat {
    set_n: usize,
    q: f64,
    alive: usize,
    indeg: f64,
    outdeg: f64,
    sleeps: f64,
    offdur: f64,
    r_mean: f64,
    ratio_mean: f64,
    ratio_med: f64,
    ret_ratio_mean: f64,
    ret_ratio_med: f64,
    ret_n: usize,
    pct: f64,
    resid_z: f64,
}

#[derive(Default)]
struct Row {
    arm: String,
    mode: String,
    d: u64,
    seed: u64,
    ret: f64,
    dorm: f64,
    rounds: u64,
    off: f64,
    alive: usize,
    online: usize,
    accts: usize,
    ever: usize,
    leaves: usize,
    dorms: usize,
    obs_h: usize,
    obs_ret: usize,
    zero_base: usize,
    hon_ratio_mean: f64,
    hon_ratio_std: f64,
    hon_n: usize,
    corr_q: f64,
    corr_indeg: f64,
    s: TStat,
    p: TStat,
    s_src_cover: f64,
    s_src_age: f64,
    s_src_fresh: f64,
    s_src_n: usize,
    h_src_age: f64,
    h_src_fresh: f64,
    n_cached_mean: f64,
    s_fresh_n: f64,
    s_fresh_frac: f64,
    s_med_age: f64,
    s_max_age: f64,
    h_fresh_frac: f64,
    h_med_age: f64,
    h_fresh_n: f64,
    p_ratio_fresh: f64,
    p_ratio_stale: f64,
    p_ratio_none: f64,
    p_n_fresh: usize,
    p_n_stale: usize,
    p_n_none: usize,
    tgt_is_promoted: usize,
    s_follows_tgt: f64,
    s_rate_tgt: f64,
}

const T_KEYS: [&str; 15] = [
    "n", "q", "alive", "indeg", "outdeg", "sleeps", "offdur", "r",
    "ratio_mean", "ratio_med", "ret_ratio_mean", "ret_ratio_med", "ret_n", "pct", "resid_z",
];

impl Row {
    fn pairs(&self) -> Vec<(String, String)> {
        let f = |x: f64| if x.is_nan() { "nan".to_string() } else { format!("{:.6}", x) };
        let t = |prefix: &str, st: &TStat| -> Vec<(String, String)> {
            let vals = vec![
                st.set_n.to_string(), f(st.q), st.alive.to_string(), f(st.indeg), f(st.outdeg),
                f(st.sleeps), f(st.offdur), f(st.r_mean), f(st.ratio_mean), f(st.ratio_med),
                f(st.ret_ratio_mean), f(st.ret_ratio_med), st.ret_n.to_string(), f(st.pct), f(st.resid_z),
            ];
            T_KEYS.iter().zip(vals).map(|(k, v)| (format!("{}{}", prefix, k), v)).collect()
        };
        let mut out: Vec<(String, String)> = vec![
            ("arm".into(), self.arm.clone()),
            ("mode".into(), self.mode.clone()),
            ("d".into(), self.d.to_string()),
            ("seed".into(), self.seed.to_string()),
            ("ret".into(), format!("{:.4}", self.ret)),
            ("dorm".into(), format!("{:.6}", self.dorm)),
            ("rounds".into(), self.rounds.to_string()),
            ("off".into(), f(self.off)),
            ("alive".into(), self.alive.to_string()),
            ("online".into(), self.online.to_string()),
            ("accts".into(), self.accts.to_string()),
            ("ever".into(), self.ever.to_string()),
            ("leaves".into(), self.leaves.to_string()),
            ("dorms".into(), self.dorms.to_string()),
            ("obs_h".into(), self.obs_h.to_string()),
            ("obs_ret".into(), self.obs_ret.to_string()),
            ("zero_base".into(), self.zero_base.to_string()),
            ("hon_ratio_mean".into(), f(self.hon_ratio_mean)),
            ("hon_ratio_std".into(), f(self.hon_ratio_std)),
            ("hon_n".into(), self.hon_n.to_string()),
            ("corr_q".into(), f(self.corr_q)),
            ("corr_indeg".into(), f(self.corr_indeg)),
            ("s_src_cover".into(), f(self.s_src_cover)),
            ("s_src_age".into(), f(self.s_src_age)),
            ("s_src_fresh".into(), f(self.s_src_fresh)),
            ("s_src_n".into(), self.s_src_n.to_string()),
            ("h_src_age".into(), f(self.h_src_age)),
            ("h_src_fresh".into(), f(self.h_src_fresh)),
            ("n_cached_mean".into(), f(self.n_cached_mean)),
            ("s_fresh_n".into(), f(self.s_fresh_n)),
            ("s_fresh_frac".into(), f(self.s_fresh_frac)),
            ("s_med_age".into(), f(self.s_med_age)),
            ("s_max_age".into(), f(self.s_max_age)),
            ("h_fresh_frac".into(), f(self.h_fresh_frac)),
            ("h_med_age".into(), f(self.h_med_age)),
            ("h_fresh_n".into(), f(self.h_fresh_n)),
            ("p_ratio_fresh".into(), f(self.p_ratio_fresh)),
            ("p_ratio_stale".into(), f(self.p_ratio_stale)),
            ("p_ratio_none".into(), f(self.p_ratio_none)),
            ("p_n_fresh".into(), self.p_n_fresh.to_string()),
            ("p_n_stale".into(), self.p_n_stale.to_string()),
            ("p_n_none".into(), self.p_n_none.to_string()),
            ("tgt_is_promoted".into(), self.tgt_is_promoted.to_string()),
            ("s_follows_tgt".into(), f(self.s_follows_tgt)),
            ("s_rate_tgt".into(), f(self.s_rate_tgt)),
        ];
        out.extend(t("sleeper_", &self.s));
        out.extend(t("promoted_", &self.p));
        out
    }
}
// ───────────────────────── 一次完整仿真 ─────────────────────────

fn run_cell(arm: &str, mode: &str, d: u64, seed: u64, ret: f64, dorm: f64, n_sleepers: usize) -> Row {
    let t_end = STEPS as u64;
    let rn = rounds();
    let tick = t_end / rn;

    let mut world = tm_sim::World::new(make_cfg(d, dorm, seed, ret, n_sleepers));
    world.run();
    let ratings = world.ratings();
    let accs = world.accounts();
    // 坑 1：编号一律用 accounts().len()（只增不删）；alive_count() 更小会撞号。
    let n = accs.len();

    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let is_att: Vec<bool> = accs.iter().map(|a| a.kind.is_attacker()).collect();
    let is_prom: Vec<bool> = accs.iter().map(|a| matches!(a.kind, Kind::Promoted)).collect();
    let is_slp: Vec<bool> = accs.iter().map(|a| matches!(a.kind, Kind::Sleeper { .. })).collect();
    // 诚实 = 既不是攻击者，也不是收益方（Promoted 必须从基线里剔除）
    let is_msr: Vec<bool> = (0..n).map(|i| !is_att[i] && !is_prom[i]).collect();
    let hon_alive: Vec<usize> = (0..n).filter(|&i| is_msr[i] && accs[i].is_alive()).collect();
    let sleepers: Vec<usize> = (0..n).filter(|&i| is_slp[i]).collect();
    let promoted: Vec<usize> = (0..n).filter(|&i| is_prom[i]).collect();
    let attack_target = world.attack.target;

    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| (0..=rn).map(|r| world.was_online_at(i, r * tick)).collect())
        .collect();
    let online_end: Vec<bool> = (0..n).map(|i| world.was_online_at(i, t_end)).collect();

    let ecfg = EngineConfig {
        beta: 0.3,
        half_life: HALF_LIFE,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        ..Default::default()
    };

    // 正分清单（= 引擎里的抽样池）+ 入度
    let mut by_obs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for &(o, t, s) in &ratings {
        if o < n && t < n {
            by_obs[o].push((t, s));
        }
    }
    let mut indeg = vec![0usize; n];
    for &(_, t, _) in &ratings {
        if t < n {
            indeg[t] += 1;
        }
    }

    // 每账户一个 Engine（只有存活账户参与；离开的账户不再发布观点）
    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n {
            e.intern(key(i));
        }
        for &(t, s) in &by_obs[a] {
            e.rate(t as u32, s, 0);
        }
        nodes[a] = Some(e);
    }

    // 观点交换：离线不发布也不抽样；回归节点 3× 抽样快速刷新
    let mut had_gap = vec![false; n];
    let mut prev_on = vec![false; n];
    for r in 0..=rn {
        let now = r * tick;
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

    // ── 观察者：存活 + 诚实 + 结束时在线 ──
    let obs: Vec<usize> = alive.iter().copied().filter(|&i| is_msr[i] && online_end[i]).collect();

    let mut s_r_v: Vec<f64> = Vec::new();
    let mut s_ratio_v: Vec<f64> = Vec::new();
    let mut s_ret_ratio_v: Vec<f64> = Vec::new();
    let mut p_r_v: Vec<f64> = Vec::new();
    let mut p_ratio_v: Vec<f64> = Vec::new();
    let mut p_ret_ratio_v: Vec<f64> = Vec::new();
    let mut sum_ratio = vec![0.0f64; n];
    let mut cnt_ratio = 0usize;
    let mut zero_base = 0usize;
    let mut p_fresh_v: Vec<f64> = Vec::new();
    let mut p_stale_v: Vec<f64> = Vec::new();
    let mut p_none_v: Vec<f64> = Vec::new();
    let mut s_src_age_v: Vec<f64> = Vec::new();
    let mut s_src_fresh_c = 0usize;
    let mut h_src_age_v: Vec<f64> = Vec::new();
    let mut h_src_fresh_v: Vec<f64> = Vec::new();
    let mut n_cached_v: Vec<f64> = Vec::new();
    let mut h_fresh_frac_v: Vec<f64> = Vec::new();
    let mut h_med_age_v: Vec<f64> = Vec::new();
    let mut h_fresh_n = 0usize;

    for &o in &obs {
        let e = match nodes[o].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let dist = e.trust_distribution(t_end);
        let mut v_of = vec![0.0f64; n];
        for &(i, x) in &dist {
            if (i as usize) < n {
                v_of[i as usize] = x as f64;
            }
        }

        // 诚实基线：全体存活诚实目标（含离线者）的均值
        let mut hsum = 0.0f64;
        for &h in &hon_alive {
            hsum += v_of[h];
        }
        let base = if hon_alive.is_empty() { 0.0 } else { hsum / hon_alive.len() as f64 };
        if base <= 1e-12 {
            zero_base += 1;
            continue;
        }

        // 坑 2：三类目标各自算，不共用分母
        let s_v = if sleepers.is_empty() {
            0.0
        } else {
            mean(&sleepers.iter().map(|&i| v_of[i]).collect::<Vec<_>>())
        };
        let p_v = if promoted.is_empty() {
            0.0
        } else {
            mean(&promoted.iter().map(|&i| v_of[i]).collect::<Vec<_>>())
        };
        s_r_v.push(s_v);
        s_ratio_v.push(s_v / base);
        p_r_v.push(p_v);
        p_ratio_v.push(p_v / base);
        if had_gap[o] {
            s_ret_ratio_v.push(s_v / base);
            p_ret_ratio_v.push(p_v / base);
        }
        for &h in &hon_alive {
            sum_ratio[h] += v_of[h] / base;
        }
        cnt_ratio += 1;

        // 观察者侧新鲜度
        let fs = e.freshness_stats(t_end);
        if fs.count > 0 {
            h_fresh_frac_v.push(fs.fresh_within_half_life as f64 / fs.count as f64);
            h_med_age_v.push(fs.median_age as f64);
            h_fresh_n += fs.count;
        }

        // 来源侧新鲜度 + 话语权份额
        let mut n_cached = 0usize;
        for &(t, _rt) in &by_obs[o] {
            if e.view_age(t as u32, t_end).is_some() {
                n_cached += 1;
            }
        }
        n_cached_v.push(n_cached as f64);

        // 潜伏者观点在观察者缓存里的最年轻年龄（有就取最小）
        let mut s_age: Option<u64> = None;
        for &s in &sleepers {
            if let Some(age) = e.view_age(s as u32, t_end) {
                s_age = Some(match s_age {
                    Some(x) => x.min(age),
                    None => age,
                });
            }
        }
        if let Some(age) = s_age {
            s_src_age_v.push(age as f64);
            if age <= HALF_LIFE {
                s_src_fresh_c += 1;
            }
        }

        // 机制分解：收益方声望 × 潜伏者观点新鲜度
        let pr = p_v / base;
        match s_age {
            Some(a) if a <= HALF_LIFE => p_fresh_v.push(pr),
            Some(_) => p_stale_v.push(pr),
            None => p_none_v.push(pr),
        }

        let mut ages: Vec<u64> = Vec::new();
        for &h in &hon_alive {
            if let Some(a) = e.view_age(h as u32, t_end) {
                ages.push(a);
            }
        }
        if !ages.is_empty() {
            let fresh = ages.iter().filter(|&&a| a <= HALF_LIFE).count();
            h_src_fresh_v.push(fresh as f64 / ages.len() as f64);
            h_src_age_v.push(median_u64(ages) as f64);
        }
    }

    // ── 零假设：每个诚实目标自己的平均比值 ──
    let mut hon_ratios: Vec<f64> = Vec::new();
    if cnt_ratio > 0 {
        for &h in &hon_alive {
            hon_ratios.push(sum_ratio[h] / cnt_ratio as f64);
        }
    }
    let hon_ratio_mean = mean(&hon_ratios);
    let hon_ratio_std = if hon_ratios.len() > 1 {
        let m = hon_ratio_mean;
        (hon_ratios.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (hon_ratios.len() - 1) as f64).sqrt()
    } else {
        f64::NAN
    };

    let qs: Vec<f64> = hon_alive.iter().map(|&h| accs[h].quality as f64).collect();
    let ds: Vec<f64> = hon_alive.iter().map(|&h| indeg[h] as f64).collect();
    let corr_q = corr(&qs, &hon_ratios);
    let corr_indeg = corr(&ds, &hon_ratios);

    // OLS: ratio = a + b*quality（只用诚实目标）
    let (mut ols_a, mut ols_b, mut ols_sd) = (f64::NAN, f64::NAN, f64::NAN);
    if qs.len() >= 5 && !hon_ratios.is_empty() {
        let (mq, my) = (mean(&qs), mean(&hon_ratios));
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for i in 0..qs.len() {
            sxy += (qs[i] - mq) * (hon_ratios[i] - my);
            sxx += (qs[i] - mq) * (qs[i] - mq);
        }
        if sxx > 0.0 {
            ols_b = sxy / sxx;
            ols_a = my - ols_b * mq;
            let resid: Vec<f64> = (0..qs.len()).map(|i| hon_ratios[i] - (ols_a + ols_b * qs[i])).collect();
            let dof = (resid.len() as f64 - 2.0).max(1.0);
            ols_sd = (resid.iter().map(|x| x * x).sum::<f64>() / dof).sqrt();
        }
    }
    let resid_z = |q: f64, ratio: f64| -> f64 {
        if ols_sd.is_nan() || ols_sd <= 0.0 || q.is_nan() || ratio.is_nan() {
            f64::NAN
        } else {
            (ratio - (ols_a + ols_b * q)) / ols_sd
        }
    };

    let mk_target = |set: &[usize], r_v: &[f64], ratio_v: &[f64], ret_v: &[f64]| -> TStat {
        if set.is_empty() {
            return TStat::default();
        }
        let q = mean(&set.iter().map(|&i| accs[i].quality as f64).collect::<Vec<_>>());
        let alive_n = set.iter().filter(|&&i| accs[i].is_alive()).count();
        let indeg_m = mean(&set.iter().map(|&i| indeg[i] as f64).collect::<Vec<_>>());
        let outdeg_m = mean(&set.iter().map(|&i| accs[i].follows.len() as f64).collect::<Vec<_>>());
        let sleeps_m = mean(
            &set.iter()
                .map(|&i| {
                    world
                        .life_traces
                        .get(i)
                        .map(|tr| tr.events().iter().filter(|(_, l)| matches!(l, tm_sim::Life::Dormant { .. })).count() as f64)
                        .unwrap_or(0.0)
                })
                .collect::<Vec<_>>(),
        );
        let offdur_m = mean(&set.iter().map(|&i| world.offline_duration(i, t_end) as f64).collect::<Vec<_>>());
        let ratio_mean = mean(ratio_v);
        TStat {
            set_n: set.len(),
            q,
            alive: alive_n,
            indeg: indeg_m,
            outdeg: outdeg_m,
            sleeps: sleeps_m,
            offdur: offdur_m,
            r_mean: mean(r_v),
            ratio_mean,
            ratio_med: median_f64(ratio_v.to_vec()),
            ret_ratio_mean: mean(ret_v),
            ret_ratio_med: median_f64(ret_v.to_vec()),
            ret_n: ret_v.len(),
            pct: pct_rank(&hon_ratios, ratio_mean),
            resid_z: resid_z(q, ratio_mean),
        }
    };

    let s = mk_target(&sleepers, &s_r_v, &s_ratio_v, &s_ret_ratio_v);
    let p = mk_target(&promoted, &p_r_v, &p_ratio_v, &p_ret_ratio_v);

    // 潜伏者的 freshness_stats（作为观察者）
    let mut s_fresh_n = 0.0;
    let mut s_fresh_frac_v: Vec<f64> = Vec::new();
    let mut s_med: Vec<f64> = Vec::new();
    let mut s_max: Vec<f64> = Vec::new();
    for &i in &sleepers {
        if let Some(e) = nodes[i].as_ref() {
            let fs = e.freshness_stats(t_end);
            if fs.count > 0 {
                s_fresh_n += fs.count as f64;
                s_fresh_frac_v.push(fs.fresh_within_half_life as f64 / fs.count as f64);
                s_med.push(fs.median_age as f64);
                s_max.push(fs.max_age as f64);
            }
        }
    }

    // 攻击通道验证
    let tgt_is_promoted = if attack_target.map(|t| is_prom[t]).unwrap_or(false) { 1 } else { 0 };
    let s_follows_tgt = if sleepers.is_empty() {
        f64::NAN
    } else {
        let c = sleepers
            .iter()
            .filter(|&&i| attack_target.map(|t| accs[i].follows.contains(&t)).unwrap_or(false))
            .count();
        c as f64 / sleepers.len() as f64
    };
    let s_rate_tgt = if sleepers.is_empty() {
        f64::NAN
    } else {
        let c = sleepers
            .iter()
            .filter(|&&i| match attack_target {
                Some(t) => accs[i].rating_of(t) == Some(1.0),
                None => false,
            })
            .count();
        c as f64 / sleepers.len() as f64
    };

    let off = if alive.is_empty() {
        0.0
    } else {
        alive.iter().filter(|&&i| !online_end[i]).count() as f64 / alive.len() as f64
    };

    Row {
        arm: arm.to_string(),
        mode: mode.to_string(),
        d,
        seed,
        ret,
        dorm,
        rounds: rn,
        off,
        alive: alive.len(),
        online: alive.iter().filter(|&&i| online_end[i]).count(),
        accts: n,
        ever: (0..n)
            .filter(|&i| {
                world
                    .life_traces
                    .get(i)
                    .map(|tr| tr.events().iter().any(|(_, l)| matches!(l, tm_sim::Life::Dormant { .. })))
                    .unwrap_or(false)
            })
            .count(),
        leaves: world.leaves,
        dorms: world.dormancy_events,
        obs_h: cnt_ratio,
        obs_ret: s_ret_ratio_v.len(),
        zero_base,
        hon_ratio_mean,
        hon_ratio_std,
        hon_n: hon_ratios.len(),
        corr_q,
        corr_indeg,
        s,
        p,
        s_src_cover: if obs.is_empty() { f64::NAN } else { s_src_age_v.len() as f64 / obs.len() as f64 },
        s_src_age: mean(&s_src_age_v),
        s_src_fresh: if s_src_age_v.is_empty() { f64::NAN } else { s_src_fresh_c as f64 / s_src_age_v.len() as f64 },
        s_src_n: s_src_age_v.len(),
        h_src_age: mean(&h_src_age_v),
        h_src_fresh: mean(&h_src_fresh_v),
        n_cached_mean: mean(&n_cached_v),
        s_fresh_n,
        s_fresh_frac: mean(&s_fresh_frac_v),
        s_med_age: mean(&s_med),
        s_max_age: mean(&s_max),
        h_fresh_frac: mean(&h_fresh_frac_v),
        h_med_age: mean(&h_med_age_v),
        h_fresh_n: if h_fresh_frac_v.is_empty() { f64::NAN } else { h_fresh_n as f64 / h_fresh_frac_v.len() as f64 },
        p_ratio_fresh: mean(&p_fresh_v),
        p_ratio_stale: mean(&p_stale_v),
        p_ratio_none: mean(&p_none_v),
        p_n_fresh: p_fresh_v.len(),
        p_n_stale: p_stale_v.len(),
        p_n_none: p_none_v.len(),
        tgt_is_promoted,
        s_follows_tgt,
        s_rate_tgt,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let smoke = args.iter().any(|a| a == "smoke");
    let land = args.iter().any(|a| a == "land");
    let only_arm: Option<String> = args.iter().skip(1).find(|a| a.starts_with("arm=")).map(|a| a[4..].to_string());
    let r_override: Option<u64> = args.iter().find_map(|a| a.strip_prefix("rounds=").and_then(|x| x.parse().ok()));
    unsafe {
        ROUNDS_ARG = r_override.unwrap_or(DEFAULT_ROUNDS);
    }
    let n_rounds = rounds();
    assert_eq!(STEPS as u64 % n_rounds, 0, "steps 必须被 rounds 整除");

    println!("# L2 离线时长 × 潜伏者优势（仿真器含 27e970f 修复）");
    println!(
        "# n_initial={} steps={} rounds={} tick={}h samples={} half_life={} normal_until={}",
        N_INIT, STEPS, n_rounds, STEPS as u64 / n_rounds, SAMPLES, HALF_LIFE, NORMAL_UNTIL
    );
    println!("# 离线只由 dormancy 决定（enable_rhythm=false）");

    if land || !smoke {
        println!("\n# LANDSCAPE seed=11 ret=0.12");
        for d in DS {
            for &dorm in &[0.0005f64, 0.001, 0.002, 0.004, 0.008, 0.016, 0.032] {
                let st = world_stat(&make_cfg(d, dorm, 11, 0.12, 1));
                println!(
                    "LAND,{},{:.6},{:.4},{},{},{},{},{},{}",
                    d, dorm, st.off, st.accts, st.alive, st.online, st.ever, st.leaves, st.dorms
                );
            }
        }
    }

    let mut header_done = false;
    let mut emit = |r: Row| {
        if !header_done {
            let keys: Vec<String> = r.pairs().into_iter().map(|(k, _)| k).collect();
            println!("#HEAD,{}", keys.join(","));
            header_done = true;
        }
        let vals: Vec<String> = r.pairs().into_iter().map(|(_, v)| v).collect();
        println!("ROW,{}", vals.join(","));
    };

    if smoke {
        for (arm, ns) in [("a1", 1usize), ("a5", 5)] {
            let (dorm, st) = calibrate(216, 11, 0.12, ns);
            println!("# smoke arm={} D=216 seed=11 dorm={:.6} off={:.3} alive={} online={}", arm, dorm, st.off, st.alive, st.online);
            let r = run_cell(arm, "fix", 216, 11, 0.12, dorm, ns);
            emit(r);
        }
        return;
    }

    // 标定：CAL 臂每格单独标定；FIX 臂取 D=216 的值（同一 seed 全 D 共用）
    let mut cal: Vec<(String, u64, u64, f64, WorldStat)> = Vec::new();
    let mut fix: Vec<(String, u64, u64, f64, WorldStat)> = Vec::new();
    for (arm, ns) in [("a1", 1usize), ("a5", 5)] {
        if let Some(f) = &only_arm {
            if f != arm {
                continue;
            }
        }
        for seed in SEEDS {
            let (dorm216, st216) = calibrate(216, seed, 0.12, ns);
            fix.push((arm.to_string(), seed, 216, dorm216, st216));
            println!("CALF,{},fix,{},{:.6},{:.4},{},{}", arm, seed, dorm216, st216.off, st216.alive, st216.online);
            for d in DS {
                let (dorm, st) = if d == 216 { (dorm216, st216) } else { calibrate(d, seed, 0.12, ns) };
                cal.push((arm.to_string(), seed, d, dorm, st));
                println!("CALC,{},cal,{},{},{:.6},{:.4},{},{}", arm, seed, d, dorm, st.off, st.alive, st.online);
            }
        }
    }

    for (arm, ns) in [("a1", 1usize), ("a5", 5)] {
        if let Some(f) = &only_arm {
            if f != arm {
                continue;
            }
        }
        for seed in SEEDS {
            let dorm_fix = fix.iter().find(|(a, s, _, _, _)| a == arm && *s == seed).map(|x| x.3).unwrap();
            for d in DS {
                for ret in [0.12f64, 0.0] {
                    let r = run_cell(arm, "fix", d, seed, ret, dorm_fix, ns);
                    emit(r);
                    eprintln!("done arm={} mode=fix D={} seed={} ret={}", arm, d, seed, ret);
                }
                let c = cal.iter().find(|(a, s, dd, _, _)| a == arm && *s == seed && *dd == d).unwrap();
                if c.4.alive >= 100 && (c.4.off - TARGET_OFF).abs() <= 0.05 {
                    let r = run_cell(arm, "cal", d, seed, 0.12, c.3, ns);
                    emit(r);
                    eprintln!("done arm={} mode=cal D={} seed={}", arm, d, seed);
                } else {
                    println!("SKIP,{},cal,{},{},{:.6},{:.4},{},{}", arm, seed, d, c.3, c.4.off, c.4.alive, c.4.online);
                }
            }
        }
    }
    println!("# done");
}
