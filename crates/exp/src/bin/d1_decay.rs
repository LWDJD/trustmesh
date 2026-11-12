//! D1 · 三种衰减模式下的潜伏攻击效果（真多 runtime）
//!
//! 模板 = crates/exp/src/bin/l2.rs：每个账户一个独立 tm_engine::Engine，
//! 离线节点不 tick、不发布观点；观点年龄真实增长；回归节点 3x 抽样快速刷新。
//!
//! 网格：3 衰减模式 x 离线时长 D 属于 {72,216,720}h x 24 seed = 216 格
//!      （规格要求 >=3 seed；主表给 24 seed，另附规格指定的 3 seed 子集）
//!   标准   decay_enabled=true,  decay_floor=0.0   factor = 0.5^(dt/72)
//!   有下限 decay_enabled=true,  decay_floor=0.5   factor = max(0.5^(dt/72), 0.5)
//!   关闭   decay_enabled=false, decay_floor=0.0   factor 恒为 1
//!
//! 固化：n_initial=300, steps=400, join_rate=0.05, leave_rate=0.001,
//!       return_rate=0.10, attack_ratio=1/300（1 个 Sleeper, normal_until=80）,
//!       enable_rhythm=false, sleep_hours=(D,D)。
//!
//! 口径（严格）：
//!   诚实   = 既非 Sleeper 也非 Promoted
//!   潜伏者 = Kind::Sleeper
//!   收益方 = Kind::Promoted（单独统计，不混进基线、不用 is_attacker 二分）
//!   三套计数器完全分开，不共用分母。
//!
//! 坑 1：新账户编号一律用 world.accounts().len()（只增不删），不用 alive_count()。
//! 坑 2：收益方质量极低、常活不到终点，存活/已离开分开报。
//!
//! 用法：
//!   d1_decay             全网格 + 汇总表
//!   d1_decay land        只打印 dormancy 标定全景（不跑引擎）
//!   d1_decay smoke       单格冒烟
//!   d1_decay threads=10  限制并发
//!
//! 输出：CAL / LAND / ROW / BUCK / CF 原始行 + 末尾 Markdown 汇总表。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

const N_INIT: usize = 300;
const STEPS: usize = 400;
const ROUNDS: u64 = 50; // 每 8h 一轮观点交换
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const BETA: f64 = 0.3;
const NORMAL_UNTIL: u64 = 80; // steps/5
const TARGET_OFF: f64 = 0.60;
const RETURN_RATE: f64 = 0.10;
const MIN_ALIVE: usize = 100; // 标定约束：不许把人口打死
const SEEDS: [u64; 24] = [
    11, 23, 37, 53, 71, 97, 113, 131, 149, 167, 181, 199,
    211, 223, 239, 251, 263, 277, 283, 293, 307, 313, 331, 349,
];
const SEEDS_SPEC: [u64; 3] = [11, 23, 37];
const DS: [u64; 3] = [72, 216, 720];

/// 副本年龄分桶（小时，左闭右开）
const BUCKETS: [(u64, u64); 5] = [(0, 24), (24, 72), (72, 168), (168, 336), (336, u64::MAX)];

#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    enabled: bool,
    floor: f32,
}

const MODES: [Mode; 3] = [
    Mode { name: "std", enabled: true, floor: 0.0 },
    Mode { name: "floor", enabled: true, floor: 0.5 },
    Mode { name: "off", enabled: false, floor: 0.0 },
];

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn make_cfg(d: u64, dorm: f64, seed: u64) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dorm,
        sleep_hours: (d, d), // 离线时长恰为 D
        return_rate: RETURN_RATE,
        enable_rhythm: false,
        attack_kind: AttackKind::Sleeper { normal_until: NORMAL_UNTIL },
        attack_ratio: 1.0 / N_INIT as f64,
        ..Default::default()
    }
}

// ───────────────────────── 统计小工具 ─────────────────────────

fn mean(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn std(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if xs.len() < 2 {
        return f64::NAN;
    }
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    (xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

fn median(v: &[f64]) -> f64 {
    let mut xs: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    }
}

fn pctl(v: &[f64], p: f64) -> f64 {
    let mut xs: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((xs.len() as f64 - 1.0) * p).round().clamp(0.0, xs.len() as f64 - 1.0) as usize;
    xs[idx]
}

/// Pearson 相关（nan 自动剔除）
fn corr(x: &[f64], y: &[f64]) -> f64 {
    let pairs: Vec<(f64, f64)> = x
        .iter()
        .copied()
        .zip(y.iter().copied())
        .filter(|(a, b)| !a.is_nan() && !b.is_nan())
        .collect();
    if pairs.len() < 3 {
        return f64::NAN;
    }
    let mx = pairs.iter().map(|p| p.0).sum::<f64>() / pairs.len() as f64;
    let my = pairs.iter().map(|p| p.1).sum::<f64>() / pairs.len() as f64;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in &pairs {
        let dx = a - mx;
        let dy = b - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        f64::NAN
    } else {
        sxy / (sxx * syy).sqrt()
    }
}

fn f3(x: f64) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else {
        format!("{:.3}", x)
    }
}

fn f4(x: f64) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else {
        format!("{:.4}", x)
    }
}

/// mean ± std 的紧凑写法
fn ms(v: &[f64]) -> String {
    if v.iter().filter(|x| !x.is_nan()).count() < 2 {
        return f3(mean(v));
    }
    format!("{:.3}±{:.3}", mean(v), std(v))
}

// ───────────────────────── 标定 ─────────────────────────

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
    let mut w = World::new(cfg.clone());
    w.run();
    let t = STEPS as u64;
    let alive: Vec<usize> = w.accounts().iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let online = alive.iter().filter(|&&i| w.was_online_at(i, t)).count();
    let off = if alive.is_empty() {
        0.0
    } else {
        (alive.len() - online) as f64 / alive.len() as f64
    };
    let n = w.accounts().len();
    let ever = (0..n)
        .filter(|&i| {
            w.life_traces
                .get(i)
                .map(|tr| tr.events().iter().any(|(_, l)| matches!(l, tm_sim::Life::Dormant { .. })))
                .unwrap_or(false)
        })
        .count();
    WorldStat { off, alive: alive.len(), online, accts: n, ever, leaves: w.leaves, dorms: w.dormancy_events }
}

fn dorm_grid() -> Vec<f64> {
    (0..=28).map(|k| 1e-5 * 5000f64.powf(k as f64 / 28.0)).collect() // 1e-5 .. 0.05
}

/// 全景观测：每个 dormancy 取值下一个世界。
fn landscape(d: u64, seed: u64) -> Vec<(f64, WorldStat)> {
    dorm_grid().into_iter().map(|dorm| (dorm, world_stat(&make_cfg(d, dorm, seed)))).collect()
}

/// 在 29 点网格上标定：先要求 alive >= MIN_ALIVE，再取 |off-0.60| 最小；
/// 若没有任何点满足 alive 约束，取存活最多的点。返回 (dorm, stat, 是否达成 60%, 全景)。
fn calibrate(d: u64, seed: u64) -> (f64, WorldStat, bool, Vec<(f64, WorldStat)>) {
    let grid = landscape(d, seed);
    let feasible: Vec<&(f64, WorldStat)> = grid.iter().filter(|(_, st)| st.alive >= MIN_ALIVE).collect();
    let pool: Vec<(f64, WorldStat)> = if feasible.is_empty() {
        let mut v: Vec<(f64, WorldStat)> = grid.clone();
        v.sort_by(|a, b| b.1.alive.cmp(&a.1.alive).then(a.1.off.partial_cmp(&b.1.off).unwrap()));
        vec![v[0]]
    } else {
        feasible.into_iter().copied().collect()
    };
    let mut best: Option<(f64, f64, WorldStat)> = None;
    for (dorm, st) in pool {
        let err = (st.off - TARGET_OFF).abs();
        let better = match &best {
            None => true,
            Some((be, _, bs)) => err < *be - 1e-9 || ((err - *be).abs() <= 1e-9 && st.alive > bs.alive),
        };
        if better {
            best = Some((err, dorm, st));
        }
    }
    let (err, dorm, st) = best.unwrap();
    (dorm, st, err <= 0.05, grid)
}

// ───────────────────────── 观测记录 ─────────────────────────

#[derive(Clone, Copy, Default)]
struct ObsRec {
    has_copy: bool,
    age: u64,
    post: bool,  // 副本最后一次成功抓取发生在潜伏者突变之后（含攻击载荷）
    fresh: bool, // post 且 age <= 72
    bucket: usize,
    p_ratio: f64,   // 收益方声望 / 诚实基线（合并项，主指标）
    p_g_ratio: f64, // 只看群体项
    s_ratio: f64,   // 潜伏者声望 / 诚实基线
    p_abs: f64,
    base: f64,
    wshare: f64, // 潜伏者副本占群体项权重的比例
    factor: f64,
    cf_ratio: f64, // 反事实：剔除潜伏者评分后重算的收益方声望 / 基线
}

#[derive(Clone)]
struct Cell {
    mode: &'static str,
    d: u64,
    seed: u64,
    dorm: f64,
    accts: usize,
    alive: usize,
    online: usize,
    off: f64,
    leaves: usize,
    dorms: usize,
    ever: usize,
    prom_n: usize,
    prom_alive: usize,
    prom_left: usize,
    prom_off_end: usize,
    slp_n: usize,
    slp_alive: usize,
    slp_off_end: usize,
    slp_offdur: f64,
    slp_on80: usize,
    hon_alive: usize,
    n_obs: usize,
    zero_base: usize,
    p_ratio_mean: f64,
    p_ratio_med: f64,
    p_ratio_p90: f64,
    p_win: f64,
    p_abs_mean: f64,
    base_mean: f64,
    p_g_ratio_mean: f64,
    s_ratio_mean: f64,
    s_abs_mean: f64,
    exp_n: usize,
    exp_frac: f64,
    pre_n: usize,
    fresh_n: usize,
    stale_n: usize,
    post_n: usize,
    p_exp: f64,
    p_pre: f64,
    p_fresh: f64,
    p_stale: f64,
    p_none: f64,
    lev_exp: f64,
    lev_fresh: f64,
    lev_stale: f64,
    s_exp: f64,
    s_none: f64,
    age_mean: f64,
    age_med: f64,
    age_max: f64,
    wshare_mean: f64,
    wshare_stale: f64,
    factor_mean: f64,
    cf_n: usize,
    cf_ratio_mean: f64,
    cf_delta_mean: f64,
    obs: Vec<ObsRec>,
}

impl Cell {
    fn landed(&self) -> bool {
        self.post_n > 0
    }

    fn pairs(&self) -> Vec<(String, String)> {
        let f = |x: f64| f4(x);
        vec![
            ("mode".into(), self.mode.to_string()),
            ("d".into(), self.d.to_string()),
            ("seed".into(), self.seed.to_string()),
            ("dorm".into(), format!("{:.6}", self.dorm)),
            ("accts".into(), self.accts.to_string()),
            ("alive".into(), self.alive.to_string()),
            ("online".into(), self.online.to_string()),
            ("off".into(), f(self.off)),
            ("leaves".into(), self.leaves.to_string()),
            ("dorms".into(), self.dorms.to_string()),
            ("ever".into(), self.ever.to_string()),
            ("prom_n".into(), self.prom_n.to_string()),
            ("prom_alive".into(), self.prom_alive.to_string()),
            ("prom_left".into(), self.prom_left.to_string()),
            ("prom_off_end".into(), self.prom_off_end.to_string()),
            ("slp_n".into(), self.slp_n.to_string()),
            ("slp_alive".into(), self.slp_alive.to_string()),
            ("slp_off_end".into(), self.slp_off_end.to_string()),
            ("slp_offdur".into(), f(self.slp_offdur)),
            ("slp_on80".into(), self.slp_on80.to_string()),
            ("hon_alive".into(), self.hon_alive.to_string()),
            ("n_obs".into(), self.n_obs.to_string()),
            ("zero_base".into(), self.zero_base.to_string()),
            ("base_mean".into(), format!("{:.6}", self.base_mean)),
            ("p_abs_mean".into(), format!("{:.6}", self.p_abs_mean)),
            ("p_ratio_mean".into(), f(self.p_ratio_mean)),
            ("p_ratio_med".into(), f(self.p_ratio_med)),
            ("p_ratio_p90".into(), f(self.p_ratio_p90)),
            ("p_win".into(), f(self.p_win)),
            ("p_g_ratio_mean".into(), f(self.p_g_ratio_mean)),
            ("s_ratio_mean".into(), f(self.s_ratio_mean)),
            ("s_abs_mean".into(), format!("{:.6}", self.s_abs_mean)),
            ("exp_n".into(), self.exp_n.to_string()),
            ("exp_frac".into(), f(self.exp_frac)),
            ("pre_n".into(), self.pre_n.to_string()),
            ("fresh_n".into(), self.fresh_n.to_string()),
            ("stale_n".into(), self.stale_n.to_string()),
            ("post_n".into(), self.post_n.to_string()),
            ("landed".into(), if self.landed() { "1".into() } else { "0".into() }),
            ("p_exp".into(), f(self.p_exp)),
            ("p_pre".into(), f(self.p_pre)),
            ("p_fresh".into(), f(self.p_fresh)),
            ("p_stale".into(), f(self.p_stale)),
            ("p_none".into(), f(self.p_none)),
            ("lev_exp".into(), f(self.lev_exp)),
            ("lev_fresh".into(), f(self.lev_fresh)),
            ("lev_stale".into(), f(self.lev_stale)),
            ("s_exp".into(), f(self.s_exp)),
            ("s_none".into(), f(self.s_none)),
            ("age_mean".into(), f(self.age_mean)),
            ("age_med".into(), f(self.age_med)),
            ("age_max".into(), f(self.age_max)),
            ("wshare_mean".into(), f(self.wshare_mean)),
            ("wshare_stale".into(), f(self.wshare_stale)),
            ("factor_mean".into(), f(self.factor_mean)),
            ("cf_n".into(), self.cf_n.to_string()),
            ("cf_ratio_mean".into(), f(self.cf_ratio_mean)),
            ("cf_delta_mean".into(), f(self.cf_delta_mean)),
        ]
    }
}

fn group_of(e: &Engine, t: usize, now: u64) -> f64 {
    let q = e.query(t as u32, now) as f64;
    let personal = e.share_of(t as u32).unwrap_or(0.0) as f64;
    (q - BETA * personal) / (1.0 - BETA)
}

// ───────────────────────── 一次完整仿真 ─────────────────────────

fn run_cell(mode: Mode, d: u64, seed: u64, dorm: f64) -> Cell {
    let t_end = STEPS as u64;
    let rn = ROUNDS;
    let tick = t_end / rn;

    let mut world = World::new(make_cfg(d, dorm, seed));
    world.run();
    let ratings = world.ratings();
    // 坑 1：编号一律用 accounts().len()（只增不删）
    let n = world.accounts().len();
    let accs = world.accounts();

    let is_slp: Vec<bool> = accs.iter().map(|a| matches!(a.kind, Kind::Sleeper { .. })).collect();
    let is_prom: Vec<bool> = accs.iter().map(|a| matches!(a.kind, Kind::Promoted)).collect();
    // 诚实 = 既非潜伏者也非收益方（绝不用 is_attacker 二分）
    let is_hon: Vec<bool> = (0..n).map(|i| !is_slp[i] && !is_prom[i]).collect();

    let alive: Vec<usize> = (0..n).filter(|&i| accs[i].is_alive()).collect();
    let hon_alive: Vec<usize> = (0..n).filter(|&i| is_hon[i] && accs[i].is_alive()).collect();
    let sleepers: Vec<usize> = (0..n).filter(|&i| is_slp[i]).collect();
    let promoted: Vec<usize> = (0..n).filter(|&i| is_prom[i]).collect();

    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| (0..=rn).map(|r| world.was_online_at(i, r * tick)).collect())
        .collect();
    let online_end: Vec<bool> = (0..n).map(|i| world.was_online_at(i, t_end)).collect();

    let ecfg = EngineConfig {
        beta: BETA as f32,
        half_life: HALF_LIFE,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: mode.enabled,
        decay_floor: mode.floor,
        coverage_exponent: 1.0,
        remove_when_coverage: 0.0,
        group_half_life: 0,
        unreachable_after: 0,
        min_group_sources: 0,
        ..Default::default()
    };

    // 正分清单（= 抽样池）
    let mut by_obs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for &(o, t, s) in &ratings {
        if o < n && t < n {
            by_obs[o].push((t, s));
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

    // 观点交换：离线不发布也不抽样；回归节点 3x 抽样快速刷新
    let mut had_gap = vec![false; n];
    let mut prev_on = vec![false; n];
    for r in 0..=rn {
        let now = r * tick;
        let mut views: Vec<Option<CompressedView>> = vec![None; n];
        for &a in &alive {
            if online[a][r as usize] {
                if let Some(e) = nodes[a].as_ref() {
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
            let reqs = nodes[a].as_mut().unwrap().tick(now, k);
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), now, now);
                }
            }
            prev_on[a] = true;
        }
    }

    // ── 观察者：存活 + 诚实 + 结束时在线 ──
    let obs: Vec<usize> = alive.iter().copied().filter(|&i| is_hon[i] && online_end[i]).collect();

    let mut recs: Vec<ObsRec> = Vec::new();
    let mut zero_base = 0usize;
    let mut base_v: Vec<f64> = Vec::new();
    let mut p_abs_v: Vec<f64> = Vec::new();

    for &o in &obs {
        let mut e = match nodes[o].take() {
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
        let base = if hon_alive.is_empty() {
            0.0
        } else {
            hon_alive.iter().map(|&h| v_of[h]).sum::<f64>() / hon_alive.len() as f64
        };
        if base <= 1e-12 {
            zero_base += 1;
            nodes[o] = Some(e);
            continue;
        }

        // 潜伏者副本：最早的一份
        let mut age: Option<u64> = None;
        for &s in &sleepers {
            if let Some(a) = e.view_age(s as u32, t_end) {
                age = Some(match age {
                    Some(x) => x.min(a),
                    None => a,
                });
            }
        }

        // 群体项权重：w = 我对来源的评分 x 衰减因子
        let mut w_total = 0.0f64;
        let mut w_slp = 0.0f64;
        for &(t, _r) in &by_obs[o] {
            if let Some(a) = e.view_age(t as u32, t_end) {
                let w = e.rating_of(t as u32).unwrap_or(0.0) as f64 * e.decay_factor(a) as f64;
                w_total += w;
                if is_slp[t] {
                    w_slp += w;
                }
            }
        }

        let p_abs = if promoted.is_empty() {
            0.0
        } else {
            promoted.iter().map(|&i| v_of[i]).sum::<f64>() / promoted.len() as f64
        };
        let s_abs = if sleepers.is_empty() {
            0.0
        } else {
            sleepers.iter().map(|&i| v_of[i]).sum::<f64>() / sleepers.len() as f64
        };

        // 只看群体项（把 β·个人项 减掉）
        let g_base = if hon_alive.is_empty() {
            f64::NAN
        } else {
            hon_alive.iter().map(|&h| group_of(&e, h, t_end)).sum::<f64>() / hon_alive.len() as f64
        };
        let g_p = if promoted.is_empty() {
            f64::NAN
        } else {
            promoted.iter().map(|&i| group_of(&e, i, t_end)).sum::<f64>() / promoted.len() as f64
        };

        let has_copy = age.is_some();
        let a0 = age.unwrap_or(0);
        let post = has_copy && a0 <= t_end - NORMAL_UNTIL;
        let bucket = BUCKETS
            .iter()
            .position(|&(lo, hi)| a0 >= lo && a0 < hi)
            .unwrap_or(BUCKETS.len() - 1);
        let mut rec = ObsRec {
            has_copy,
            age: a0,
            post,
            fresh: post && a0 <= HALF_LIFE,
            bucket: if has_copy { bucket } else { BUCKETS.len() },
            p_ratio: p_abs / base,
            p_g_ratio: if g_base.is_nan() || g_base <= 0.0 { f64::NAN } else { g_p / g_base },
            s_ratio: s_abs / base,
            p_abs,
            base,
            wshare: if w_total > 0.0 { w_slp / w_total } else { 0.0 },
            factor: if has_copy { e.decay_factor(a0) as f64 } else { f64::NAN },
            cf_ratio: f64::NAN,
        };

        // 反事实：把「我对潜伏者的评分」清零 → 它的观点副本不再进入群体项
        if has_copy {
            if let Some(&s) = sleepers.first() {
                e.rate(s as u32, 0.0, t_end);
                let dist2 = e.trust_distribution(t_end);
                let mut v2 = vec![0.0f64; n];
                for &(i, x) in &dist2 {
                    if (i as usize) < n {
                        v2[i as usize] = x as f64;
                    }
                }
                let base2 = if hon_alive.is_empty() {
                    0.0
                } else {
                    hon_alive.iter().map(|&h| v2[h]).sum::<f64>() / hon_alive.len() as f64
                };
                let p2 = if promoted.is_empty() {
                    0.0
                } else {
                    promoted.iter().map(|&i| v2[i]).sum::<f64>() / promoted.len() as f64
                };
                if base2 > 1e-12 {
                    rec.cf_ratio = p2 / base2;
                }
            }
        }

        base_v.push(base);
        p_abs_v.push(p_abs);
        recs.push(rec);
        nodes[o] = Some(e);
    }

    // ── 分组统计 ──
    let sel = |pred: &dyn Fn(&ObsRec) -> bool| -> Vec<ObsRec> {
        recs.iter().copied().filter(|r| pred(r)).collect()
    };
    let exp = sel(&|r| r.post); // 含攻击载荷的副本
    let pre = sel(&|r| r.has_copy && !r.post); // 突变前抓到的副本（无载荷）
    let fresh = sel(&|r| r.fresh);
    let stale = sel(&|r| r.post && !r.fresh);
    let none = sel(&|r| !r.has_copy);
    let v = |xs: &[ObsRec], f: fn(&ObsRec) -> f64| -> Vec<f64> { xs.iter().map(|r| f(r)).collect() };

    let p_all = v(&recs, |r| r.p_ratio);
    let p_exp = mean(&v(&exp, |r| r.p_ratio));
    let p_pre = mean(&v(&pre, |r| r.p_ratio));
    let p_fresh = mean(&v(&fresh, |r| r.p_ratio));
    let p_stale = mean(&v(&stale, |r| r.p_ratio));
    let p_none = mean(&v(&none, |r| r.p_ratio));
    let s_all = v(&recs, |r| r.s_ratio);

    let cfs: Vec<ObsRec> = recs.iter().copied().filter(|r| !r.cf_ratio.is_nan()).collect();

    let off = if alive.is_empty() {
        0.0
    } else {
        alive.iter().filter(|&&i| !online_end[i]).count() as f64 / alive.len() as f64
    };

    let ever = (0..n)
        .filter(|&i| {
            world
                .life_traces
                .get(i)
                .map(|tr| tr.events().iter().any(|(_, l)| matches!(l, tm_sim::Life::Dormant { .. })))
                .unwrap_or(false)
        })
        .count();

    let slp_offdur = if sleepers.is_empty() {
        f64::NAN
    } else {
        sleepers.iter().map(|&i| world.offline_duration(i, t_end) as f64).sum::<f64>()
            / sleepers.len() as f64
    };
    let slp_on80 = sleepers
        .iter()
        .map(|&i| (0..=rn).filter(|&r| r * tick >= NORMAL_UNTIL && online[i][r as usize]).count())
        .sum::<usize>();

    Cell {
        mode: mode.name,
        d,
        seed,
        dorm,
        accts: n,
        alive: alive.len(),
        online: alive.iter().filter(|&&i| online_end[i]).count(),
        off,
        leaves: world.leaves,
        dorms: world.dormancy_events,
        ever,
        prom_n: promoted.len(),
        prom_alive: promoted.iter().filter(|&&i| accs[i].is_alive()).count(),
        prom_left: promoted.iter().filter(|&&i| !accs[i].is_alive()).count(),
        prom_off_end: promoted.iter().filter(|&&i| !online_end[i]).count(),
        slp_n: sleepers.len(),
        slp_alive: sleepers.iter().filter(|&&i| accs[i].is_alive()).count(),
        slp_off_end: sleepers.iter().filter(|&&i| !online_end[i]).count(),
        slp_offdur,
        slp_on80,
        hon_alive: hon_alive.len(),
        n_obs: recs.len(),
        zero_base,
        p_ratio_mean: mean(&p_all),
        p_ratio_med: median(&p_all),
        p_ratio_p90: pctl(&p_all, 0.9),
        p_win: if p_all.is_empty() {
            f64::NAN
        } else {
            p_all.iter().filter(|&&x| x > 1.0).count() as f64 / p_all.len() as f64
        },
        p_abs_mean: mean(&p_abs_v),
        base_mean: mean(&base_v),
        p_g_ratio_mean: mean(&v(&recs, |r| r.p_g_ratio)),
        s_ratio_mean: mean(&s_all),
        s_abs_mean: mean(&v(&recs, |r| r.s_ratio * r.base)),
        exp_n: exp.len(),
        exp_frac: if recs.is_empty() { f64::NAN } else { exp.len() as f64 / recs.len() as f64 },
        pre_n: pre.len(),
        fresh_n: fresh.len(),
        stale_n: stale.len(),
        post_n: exp.len(),
        p_exp,
        p_pre,
        p_fresh,
        p_stale,
        p_none,
        lev_exp: if p_none > 0.0 { p_exp / p_none } else { f64::NAN },
        lev_fresh: if p_none > 0.0 { p_fresh / p_none } else { f64::NAN },
        lev_stale: if p_none > 0.0 { p_stale / p_none } else { f64::NAN },
        s_exp: mean(&v(&exp, |r| r.s_ratio)),
        s_none: mean(&v(&none, |r| r.s_ratio)),
        age_mean: mean(&v(&exp, |r| r.age as f64)),
        age_med: median(&v(&exp, |r| r.age as f64)),
        age_max: v(&exp, |r| r.age as f64).iter().copied().fold(f64::NAN, f64::max),
        wshare_mean: mean(&v(&exp, |r| r.wshare)),
        wshare_stale: mean(&v(&stale, |r| r.wshare)),
        factor_mean: mean(&v(&exp, |r| r.factor)),
        cf_n: cfs.len(),
        cf_ratio_mean: mean(&v(&cfs, |r| r.cf_ratio)),
        cf_delta_mean: if cfs.is_empty() {
            f64::NAN
        } else {
            mean(&cfs.iter().map(|r| r.p_ratio - r.cf_ratio).collect::<Vec<_>>())
        },
        obs: recs,
    }
}

// ───────────────────────── 并行 ─────────────────────────

fn par_map<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let slots: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let th = threads.max(1).min(n.max(1));
    std::thread::scope(|s| {
        for _ in 0..th {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let v = f(i);
                slots.lock().unwrap()[i] = Some(v);
            });
        }
    });
    slots.into_inner().unwrap().into_iter().map(|x| x.unwrap()).collect()
}

// ───────────────────────── 汇总表 ─────────────────────────

fn md_table(head: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = String::new();
    s.push_str("| ");
    s.push_str(&head.join(" | "));
    s.push_str(" |\n|");
    for _ in head {
        s.push_str("---|");
    }
    s.push('\n');
    for r in rows {
        s.push_str("| ");
        s.push_str(&r.join(" | "));
        s.push_str(" |\n");
    }
    s
}

// ───────────────────────── 汇总：按 (模式, D) ─────────────────────────

fn pick<'a>(cells: &'a [Cell], seeds: &[u64]) -> Vec<&'a Cell> {
    cells.iter().filter(|c| seeds.contains(&c.seed)).collect()
}

fn col(g: &[&Cell], f: fn(&Cell) -> f64) -> Vec<f64> {
    g.iter().map(|c| f(c)).collect()
}

fn avg_usize(g: &[&Cell], f: fn(&Cell) -> usize) -> f64 {
    if g.is_empty() {
        f64::NAN
    } else {
        g.iter().map(|c| f(c) as f64).sum::<f64>() / g.len() as f64
    }
}

fn t_main(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for m in MODES {
        for &d in &DS {
            let g: Vec<&Cell> = pick(cells, seeds).into_iter().filter(|c| c.mode == m.name && c.d == d).collect();
            let pooled: Vec<f64> = g.iter().flat_map(|c| c.obs.iter().map(|r| r.p_ratio)).collect();
            rows.push(vec![
                m.name.to_string(),
                format!("{}", d),
                ms(&col(&g, |c| c.p_ratio_mean)),
                format!("{:.3}", median(&pooled)),
                ms(&col(&g, |c| c.exp_frac)),
                ms(&col(&g, |c| c.p_g_ratio_mean)),
                ms(&col(&g, |c| c.s_ratio_mean)),
                format!("{}/{}", g.iter().filter(|c| c.landed()).count(), g.len()),
                format!("{}/{}", g.iter().map(|c| c.prom_alive).sum::<usize>(), g.len()),
                format!("{}/{}", g.iter().map(|c| c.prom_left).sum::<usize>(), g.len()),
                ms(&col(&g, |c| c.alive as f64)),
                ms(&col(&g, |c| c.off)),
                ms(&col(&g, |c| c.n_obs as f64)),
            ]);
        }
    }
    md_table(
        &[
            "模式", "D(h)", "收益方/基线 均值±sd", "池化中位", "暴露占比", "群体项比", "潜伏者/基线",
            "攻击落地格", "收益方存活/格", "收益方已离开/格", "存活账户", "结束离线", "观察者数",
        ],
        &rows,
    )
}

fn t_group(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for m in MODES {
        for &d in &DS {
            let g: Vec<&Cell> = pick(cells, seeds).into_iter().filter(|c| c.mode == m.name && c.d == d).collect();
            rows.push(vec![
                m.name.to_string(),
                format!("{}", d),
                format!("{:.1}", avg_usize(&g, |c| c.exp_n)),
                ms(&col(&g, |c| c.p_exp)),
                format!("{:.1}", avg_usize(&g, |c| c.n_obs - c.exp_n)),
                ms(&col(&g, |c| c.p_none)),
                ms(&col(&g, |c| c.lev_exp)),
                format!("{:.1}", avg_usize(&g, |c| c.pre_n)),
                ms(&col(&g, |c| c.p_pre)),
                ms(&col(&g, |c| c.s_exp)),
                ms(&col(&g, |c| c.s_none)),
            ]);
        }
    }
    md_table(
        &[
            "模式", "D(h)", "暴露 n/格", "暴露组 收益方/基线", "未暴露 n/格", "未暴露组", "杠杆(暴露/未暴露)",
            "突变前副本 n/格", "突变前副本组", "暴露组 潜伏者/基线", "未暴露组 潜伏者/基线",
        ],
        &rows,
    )
}

fn t_fresh(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for m in MODES {
        for &d in &DS {
            let g: Vec<&Cell> = pick(cells, seeds).into_iter().filter(|c| c.mode == m.name && c.d == d).collect();
            rows.push(vec![
                m.name.to_string(),
                format!("{}", d),
                format!("{:.1}", avg_usize(&g, |c| c.fresh_n)),
                ms(&col(&g, |c| c.p_fresh)),
                ms(&col(&g, |c| c.lev_fresh)),
                format!("{:.1}", avg_usize(&g, |c| c.stale_n)),
                ms(&col(&g, |c| c.p_stale)),
                ms(&col(&g, |c| c.lev_stale)),
                ms(&col(&g, |c| c.age_med)),
                ms(&col(&g, |c| c.age_max)),
                ms(&col(&g, |c| c.wshare_mean)),
                ms(&col(&g, |c| c.factor_mean)),
            ]);
        }
    }
    md_table(
        &[
            "模式", "D(h)", "新鲜 n/格", "新鲜组 比", "新鲜杠杆", "陈旧 n/格", "陈旧组 比", "陈旧杠杆",
            "副本年龄中位(h)", "副本最大年龄(h)", "潜伏者副本权重占比", "副本平均衰减因子",
        ],
        &rows,
    )
}

fn t_profile(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let sel = pick(cells, seeds);
    for m in MODES {
        let gs: Vec<&&Cell> = sel.iter().filter(|c| c.mode == m.name).collect();
        for (bi, &(lo, hi)) in BUCKETS.iter().enumerate() {
            let xs: Vec<&ObsRec> = gs.iter().flat_map(|c| c.obs.iter().filter(|r| r.post && r.bucket == bi)).collect();
            if xs.is_empty() {
                continue;
            }
            let pr: Vec<f64> = xs.iter().map(|r| r.p_ratio).collect();
            let ws: Vec<f64> = xs.iter().map(|r| r.wshare).collect();
            let cf: Vec<f64> = xs.iter().map(|r| r.p_ratio - r.cf_ratio).collect();
            rows.push(vec![
                m.name.to_string(),
                format!("[{},{})", lo, if hi == u64::MAX { 9999 } else { hi }),
                format!("{}", xs.len()),
                format!("{:.3}", mean(&pr)),
                format!("{:.3}", median(&pr)),
                format!("{:.3}", pctl(&pr, 0.9)),
                format!("{:.3}", mean(&ws)),
                format!("{:.3}", mean(&cf)),
            ]);
        }
        // 突变前抓到的副本（无载荷）
        let xs: Vec<&ObsRec> = gs.iter().flat_map(|c| c.obs.iter().filter(|r| r.has_copy && !r.post)).collect();
        if !xs.is_empty() {
            let pr: Vec<f64> = xs.iter().map(|r| r.p_ratio).collect();
            let ws: Vec<f64> = xs.iter().map(|r| r.wshare).collect();
            rows.push(vec![
                m.name.to_string(),
                "抓取早于t=80".to_string(),
                format!("{}", xs.len()),
                format!("{:.3}", mean(&pr)),
                format!("{:.3}", median(&pr)),
                format!("{:.3}", pctl(&pr, 0.9)),
                format!("{:.3}", mean(&ws)),
                "—".to_string(),
            ]);
        }
        let xs: Vec<&ObsRec> = gs.iter().flat_map(|c| c.obs.iter().filter(|r| !r.has_copy)).collect();
        let pr: Vec<f64> = xs.iter().map(|r| r.p_ratio).collect();
        rows.push(vec![
            m.name.to_string(),
            "无副本".to_string(),
            format!("{}", xs.len()),
            format!("{:.3}", mean(&pr)),
            format!("{:.3}", median(&pr)),
            format!("{:.3}", pctl(&pr, 0.9)),
            "—".to_string(),
            "—".to_string(),
        ]);
    }
    md_table(
        &["模式", "副本年龄(h)", "n", "收益方/基线 均值", "中位", "p90", "副本权重占比", "剔除副本后落差"],
        &rows,
    )
}

fn t_overview(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let sel = pick(cells, seeds);
    for m in MODES {
        let gs: Vec<&&Cell> = sel.iter().filter(|c| c.mode == m.name).collect();
        let xs: Vec<&ObsRec> = gs.iter().flat_map(|c| c.obs.iter()).collect();
        let ex: Vec<&ObsRec> = xs.iter().copied().filter(|r| r.post).collect();
        let st: Vec<&ObsRec> = ex.iter().copied().filter(|r| !r.fresh).collect();
        let no: Vec<&ObsRec> = xs.iter().copied().filter(|r| !r.has_copy).collect();
        let pr = |s: &[&ObsRec]| -> f64 { mean(&s.iter().map(|r| r.p_ratio).collect::<Vec<_>>()) };
        let cf: Vec<f64> = ex.iter().map(|r| r.p_ratio - r.cf_ratio).collect();
        let age: Vec<f64> = ex.iter().map(|r| r.age as f64).collect();
        let prx: Vec<f64> = ex.iter().map(|r| r.p_ratio).collect();
        let ws: Vec<f64> = ex.iter().map(|r| r.wshare).collect();
        rows.push(vec![
            m.name.to_string(),
            format!("{}", xs.len()),
            format!("{}", ex.len()),
            format!("{:.3}", pr(&ex)),
            format!("{:.3}", pr(&st)),
            format!("{:.3}", pr(&no)),
            format!("{:.2}", if pr(&no) > 0.0 { pr(&ex) / pr(&no) } else { f64::NAN }),
            format!("{:.2}", if pr(&no) > 0.0 { pr(&st) / pr(&no) } else { f64::NAN }),
            format!("{:.3}", corr(&age, &prx)),
            format!("{:.3}", corr(&ws, &prx)),
            format!("{:.3}", mean(&cf)),
            format!("{:.3}", mean(&ex.iter().map(|r| r.factor).collect::<Vec<_>>())),
            format!("{:.3}", mean(&ws)),
        ]);
    }
    md_table(
        &[
            "模式", "观察者", "暴露", "暴露组", "陈旧暴露组", "未暴露组", "杠杆(暴露/未暴露)", "杠杆(陈旧/未暴露)",
            "corr(年龄, 比)", "corr(权重占比, 比)", "反事实落差", "平均衰减因子", "副本权重占比",
        ],
        &rows,
    )
}

fn t_landed(cells: &[Cell], seeds: &[u64]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for m in MODES {
        let g: Vec<&Cell> = pick(cells, seeds).into_iter().filter(|c| c.mode == m.name && c.landed()).collect();
        let xs: Vec<&ObsRec> = g.iter().flat_map(|c| c.obs.iter()).collect();
        let ex: Vec<&ObsRec> = xs.iter().copied().filter(|r| r.post).collect();
        let st: Vec<&ObsRec> = ex.iter().copied().filter(|r| !r.fresh).collect();
        let no: Vec<&ObsRec> = xs.iter().copied().filter(|r| !r.has_copy).collect();
        let pr = |s: &[&ObsRec]| -> f64 { mean(&s.iter().map(|r| r.p_ratio).collect::<Vec<_>>()) };
        rows.push(vec![
            m.name.to_string(),
            format!("{}", g.len()),
            format!("{}", ex.len()),
            format!("{:.3}", pr(&ex)),
            format!("{:.3}", pr(&st)),
            format!("{:.3}", pr(&no)),
            format!("{:.2}", if pr(&no) > 0.0 { pr(&ex) / pr(&no) } else { f64::NAN }),
            format!("{:.2}", if pr(&no) > 0.0 { pr(&st) / pr(&no) } else { f64::NAN }),
            format!("{:.3}", pr(&xs)),
        ]);
    }
    md_table(
        &["模式", "落地格数", "暴露", "暴露组", "陈旧暴露组", "未暴露组", "杠杆(暴露/未暴露)", "杠杆(陈旧/未暴露)", "全体均值"],
        &rows,
    )
}

fn t_detail(cells: &[Cell]) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for c in cells {
        rows.push(vec![
            c.mode.to_string(),
            format!("{}", c.d),
            format!("{}", c.seed),
            format!("{:.6}", c.dorm),
            format!("{:.3}", c.off),
            format!("{}", c.alive),
            format!("{}", c.n_obs),
            format!("{:.3}", c.p_ratio_mean),
            format!("{:.3}", c.p_ratio_med),
            format!("{}", c.post_n),
            format!("{:.3}", c.p_exp),
            format!("{:.3}", c.p_stale),
            format!("{:.3}", c.p_none),
            format!("{:.2}", c.lev_exp),
            format!("{:.2}", c.lev_stale),
            format!("{:.3}", c.s_ratio_mean),
            format!("{}/{}", c.prom_alive, c.prom_n),
            format!("{:.1}", c.age_med),
            format!("{:.3}", c.wshare_mean),
            format!("{:.3}", c.cf_delta_mean),
        ]);
    }
    md_table(
        &[
            "模式", "D", "seed", "dorm", "离线", "存活", "n_obs", "比均值", "比中位", "载荷副本", "暴露组", "陈旧组",
            "未暴露", "杠杆", "陈旧杠杆", "潜伏者比", "收益方存活", "年龄中位", "权重占比", "反事实落差",
        ],
        &rows,
    )
}

fn t_paired(cells: &[Cell], seeds: &[u64]) -> String {
    // 同一 (D, seed) 下三种衰减模式共用同一个世界与同一套抽样序列，只有衰减因子不同。
    let mut rows: Vec<Vec<String>> = Vec::new();
    for &d in &DS {
        for &s in seeds {
            let g: Vec<&Cell> = cells.iter().filter(|c| c.d == d && c.seed == s).collect();
            if g.len() < 3 {
                continue;
            }
            let get = |m: &str| -> Option<&&Cell> { g.iter().find(|c| c.mode == m) };
            let (cs, cf, co) = match (get("std"), get("floor"), get("off")) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                _ => continue,
            };
            if cs.post_n == 0 && cf.post_n == 0 && co.post_n == 0 {
                continue;
            }
            rows.push(vec![
                format!("{}", d),
                format!("{}", s),
                format!("{}", cs.post_n),
                format!("{:.0}", cs.age_med),
                format!("{:.3}", cs.p_exp),
                format!("{:.3}", cf.p_exp),
                format!("{:.3}", co.p_exp),
                format!("{:.3}", co.p_exp - cs.p_exp),
                format!("{:.3}", cs.p_none),
            ]);
        }
    }
    md_table(
        &["D(h)", "seed", "载荷副本 n", "副本年龄中位", "标准 暴露组", "有下限 暴露组", "关闭 暴露组", "关闭−标准", "未暴露组"],
        &rows,
    )
}

fn t_channel(cells: &[Cell], seeds: &[u64]) -> String {
    // 三模式共用同一个世界，潜伏者的在线时间表在三种模式下相同，故只统计 std 格，避免三重计数。
    let mut rows: Vec<Vec<String>> = Vec::new();
    for &d in &DS {
        let g: Vec<&Cell> = cells.iter().filter(|c| c.d == d && c.mode == "std" && seeds.contains(&c.seed)).collect();
        let n = g.len().max(1);
        let offdur: Vec<f64> = g.iter().map(|c| c.slp_offdur).collect();
        rows.push(vec![
            format!("{}", d),
            format!("{}", g.len()),
            format!("{}", g.iter().filter(|c| c.slp_on80 > 0).count()),
            format!("{:.1}", g.iter().map(|c| c.post_n).sum::<usize>() as f64 / n as f64),
            format!("{}", g.iter().filter(|c| c.landed()).count()),
            format!("{}", g.iter().filter(|c| c.slp_alive > 0).count()),
            format!("{}", g.iter().filter(|c| c.slp_off_end > 0).count()),
            format!("{:.0}", median(&offdur)),
        ]);
    }
    md_table(
        &["D(h)", "格数(每模式)", "潜伏者 80h 后曾在线", "载荷副本 个/格", "落地格", "潜伏者存活格", "潜伏者结束离线格", "潜伏者离线时长中位(h)"],
        &rows,
    )
}

fn t_land72(cals: &[(u64, u64, f64, WorldStat, bool, Vec<(f64, WorldStat)>)]) -> String {
    let d = 72u64;
    let grids: Vec<&Vec<(f64, WorldStat)>> = cals.iter().filter(|c| c.0 == d && SEEDS_SPEC.contains(&c.1)).map(|c| &c.5).collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    if grids.is_empty() {
        return String::new();
    }
    for i in 0..grids[0].len() {
        let mut r = vec![format!("{:.6}", grids[0][i].0)];
        for g in &grids {
            r.push(format!("{:.3} / {}", g[i].1.off, g[i].1.alive));
        }
        rows.push(r);
    }
    md_table(&["dormancy_rate", "seed 11 离线/存活", "seed 23", "seed 37"], &rows)
}

// ───────────────────────── main ─────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let smoke = args.iter().any(|a| a == "smoke");
    let land_only = args.iter().any(|a| a == "land");
    let threads: usize = args
        .iter()
        .find_map(|a| a.strip_prefix("threads=").and_then(|x| x.parse().ok()))
        .unwrap_or(18);

    let t0 = std::time::Instant::now();

    if smoke {
        let (dorm, st, ok, _) = calibrate(216, 11);
        println!("# smoke D=216 seed=11 dorm={:.6} off={:.4} alive={} online={} ok={}", dorm, st.off, st.alive, st.online, ok);
        let c = run_cell(MODES[0], 216, 11, dorm);
        let keys: Vec<String> = c.pairs().into_iter().map(|(k, _)| k).collect();
        println!("#HEAD,{}", keys.join(","));
        let vals: Vec<String> = c.pairs().into_iter().map(|(_, v)| v).collect();
        println!("ROW,{}", vals.join(","));
        println!("# elapsed {:.1}s", t0.elapsed().as_secs_f64());
        return;
    }

    let tasks: Vec<(u64, u64)> = DS.iter().flat_map(|&d| SEEDS.iter().map(move |&s| (d, s))).collect();
    println!("# D1 三衰减模式 x 离线时长 x 潜伏攻击 · n={} steps={} rounds={} samples={}", N_INIT, STEPS, ROUNDS, SAMPLES);
    println!("# 口径：诚实=非 Sleeper 非 Promoted；潜伏者=Sleeper；收益方=Promoted（分开统计）");
    println!("# 标定：每 (D,seed) 在 29 点对数网格上取 alive>={} 且 |off-{:.2}| 最小的 dormancy_rate", MIN_ALIVE, TARGET_OFF);

    let cals = par_map(tasks.len(), threads, |i| {
        let (d, s) = tasks[i];
        let (dorm, st, ok, grid) = calibrate(d, s);
        eprintln!("[cal] D={} seed={} dorm={:.6} off={:.4} alive={} online={} ok={}", d, s, dorm, st.off, st.alive, st.online, ok);
        (d, s, dorm, st, ok, grid)
    });

    for (d, s, dorm, st, ok, _) in &cals {
        println!(
            "CAL,{},{},{:.6},{:.4},{},{},{},{},{},{},{}",
            d, s, dorm, st.off, st.accts, st.alive, st.online, st.ever, st.leaves, st.dorms,
            if *ok { 1 } else { 0 }
        );
    }
    for (d, s, _, _, _, grid) in &cals {
        if SEEDS_SPEC.contains(s) {
            for (i, (g, gs)) in grid.iter().enumerate() {
                println!(
                    "LAND,{},{},{},{:.6},{:.4},{},{},{},{}",
                    d, s, i, g, gs.off, gs.alive, gs.online, gs.leaves, gs.dorms
                );
            }
        }
    }

    if land_only {
        println!("\n### 表 B2 · D=72 的 dormancy 全景（离线比例 / 存活账户）\n");
        println!("{}", t_land72(&cals));
        println!("# elapsed {:.1}s", t0.elapsed().as_secs_f64());
        return;
    }

    // 阶段 2：54 格
    let spec: Vec<(u64, u64, Mode)> = DS
        .iter()
        .flat_map(|&d| SEEDS.iter().flat_map(move |&s| MODES.iter().map(move |&m| (d, s, m))))
        .collect();
    let cells = par_map(spec.len(), threads, |i| {
        let (d, s, m) = spec[i];
        let dorm = cals.iter().find(|c| c.0 == d && c.1 == s).unwrap().2;
        let t = std::time::Instant::now();
        let c = run_cell(m, d, s, dorm);
        eprintln!(
            "[cell] mode={} D={} seed={} p_ratio={:.3} exp={}/{} landed={} lev={:.2} ({:.1}s)",
            m.name,
            d,
            s,
            c.p_ratio_mean,
            c.exp_n,
            c.n_obs,
            c.post_n,
            c.lev_exp,
            t.elapsed().as_secs_f64()
        );
        c
    });

    let keys: Vec<String> = cells[0].pairs().into_iter().map(|(k, _)| k).collect();
    println!("#HEAD,{}", keys.join(","));
    for c in &cells {
        let vals: Vec<String> = c.pairs().into_iter().map(|(_, v)| v).collect();
        println!("ROW,{}", vals.join(","));
    }

    println!("#BUCKHEAD,mode,d,seed,lo,hi,n,p_ratio_mean,p_ratio_med,wshare_mean,factor_mean,cf_ratio_mean");
    for c in &cells {
        for (bi, &(lo, hi)) in BUCKETS.iter().enumerate() {
            let xs: Vec<&ObsRec> = c.obs.iter().filter(|r| r.post && r.bucket == bi).collect();
            if xs.is_empty() {
                continue;
            }
            let pr: Vec<f64> = xs.iter().map(|r| r.p_ratio).collect();
            let ws: Vec<f64> = xs.iter().map(|r| r.wshare).collect();
            let fa: Vec<f64> = xs.iter().map(|r| r.factor).collect();
            let cf: Vec<f64> = xs.iter().map(|r| r.cf_ratio).collect();
            println!(
                "BUCK,{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4}",
                c.mode, c.d, c.seed, lo,
                if hi == u64::MAX { 9999 } else { hi },
                xs.len(), mean(&pr), median(&pr), mean(&ws), mean(&fa), mean(&cf)
            );
        }
    }

    println!("#CFHEAD,mode,d,seed,n,cf_ratio_mean,cf_delta_mean");
    for c in &cells {
        println!("CF,{},{},{},{},{:.4},{:.4}", c.mode, c.d, c.seed, c.cf_n, c.cf_ratio_mean, c.cf_delta_mean);
    }

    // ───────────────────────── 汇总 ─────────────────────────
    println!("\n<!-- SUMMARY -->\n");

    {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let dts = [0u64, 12, 24, 48, 72, 96, 120, 168, 216, 240, 336, 480, 720, 1440, 2880];
        for dt in dts {
            let mut r = vec![format!("{}", dt), format!("{:.4}", 0.5f64.powf(dt as f64 / HALF_LIFE as f64))];
            for m in MODES {
                let f = if !m.enabled {
                    1.0
                } else {
                    0.5f64.powf(dt as f64 / HALF_LIFE as f64).max(m.floor as f64)
                };
                r.push(format!("{:.4}", f));
            }
            rows.push(r);
        }
        println!("### 表 A · 三种模式的衰减因子（Engine::decay_factor）\n");
        println!("{}", md_table(&["Δt(h)", "理论 0.5^(Δt/72)", "标准", "有下限(0.5)", "关闭"], &rows));
    }

    {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for (d, s, dorm, st, ok, _) in &cals {
            rows.push(vec![
                format!("{}", d),
                format!("{}", s),
                format!("{:.6}", dorm),
                format!("{:.4}", st.off),
                format!("{}", st.alive),
                format!("{}", st.online),
                format!("{}", st.accts),
                format!("{}", st.leaves),
                format!("{}", st.dorms),
                if *ok { "是".to_string() } else { "否（人口约束）".to_string() },
            ]);
        }
        println!("\n### 表 B · dormancy 标定（每 (D,seed) 独立标定；约束 alive>={}）\n", MIN_ALIVE);
        println!(
            "{}",
            md_table(
                &["D(h)", "seed", "dormancy_rate", "结束离线", "存活", "在线", "账户总数", "离开", "休眠事件", "达成 60%"],
                &rows
            )
        );
    }

    println!("\n### 表 B2 · D=72 的 dormancy 全景（离线比例 / 存活账户）\n");
    println!("{}", t_land72(&cals));

    println!("\n## 全部 seed（{} 格）\n", cells.len());
    println!("### 表 1 · 主表：收益方声望 ÷ 诚实基线（格间均值±标准差）\n");
    println!("{}", t_main(&cells, &SEEDS));
    println!("\n注：暴露占比 = 结束时在线诚实观察者中，缓存里有「突变后（含攻击载荷）潜伏者观点副本」的比例；群体项比 = 扣掉 β·个人项 后只按群体项算的同一比值；攻击落地格 = 该格至少有一个观察者持有含载荷副本。\n");

    println!("### 表 2 · 按「观察者是否持有潜伏者观点副本」分组\n");
    println!("{}", t_group(&cells, &SEEDS));
    println!("\n注：暴露 = 持有潜伏者观点副本、且副本年龄 ≤ 320h（即最后一次成功抓取发生在 t=80 潜伏者突变之后）。「突变前副本」= 年龄 > 320h 的极旧副本，最后一次抓取早于 t=80，单独列出。\n");

    println!("### 表 3 · 暴露组再分「新鲜副本（年龄≤72h）/ 陈旧副本（>72h）」\n");
    println!("{}", t_fresh(&cells, &SEEDS));

    println!("\n### 表 4 · 副本年龄剖面（池化全部格，逐观察者）\n");
    println!("{}", t_profile(&cells, &SEEDS));
    println!("\n注：剔除副本后落差 = 同一观察者 p_ratio 减去「把对潜伏者的评分清零」后重算的 p_ratio（同观察者内反事实，不受暴露组自选择影响）。\n");

    println!("### 表 5 · 三模式总览（每模式池化全部 D × seed，逐观察者）\n");
    println!("{}", t_overview(&cells, &SEEDS));

    println!("\n### 表 5b · 只保留「攻击已落地」的格\n");
    println!("{}", t_landed(&cells, &SEEDS));

    println!("\n### 表 7 · 成对对照：同一 (D,seed) 下三种衰减模式的暴露组收益方声望\n");
    println!("{}", t_paired(&cells, &SEEDS));
    println!("\n注：同一 (D,seed) 的三种模式共用同一个世界、同一套抽样序列（每账户 Engine 种子固定），差别只有衰减因子，因此逐行可直接相减。\n");

    println!("\n## 规格指定的 3 seed（27 格）\n");
    println!("### 表 1s · 主表（seed 11/23/37）\n");
    println!("{}", t_main(&cells, &SEEDS_SPEC));
    println!("\n### 表 2s · 暴露/未暴露分组（seed 11/23/37）\n");
    println!("{}", t_group(&cells, &SEEDS_SPEC));

    println!("\n### 表 8 · 攻击通道：载荷副本是怎么（没）出现的\n");
    println!("{}", t_channel(&cells, &SEEDS));

    println!("\n### 表 6 · 规格 27 格逐格明细（全 216 格的原始行在 d1_result.txt 的 ROW 段）\n");
    let spec_cells: Vec<Cell> = cells.iter().filter(|c| SEEDS_SPEC.contains(&c.seed)).cloned().collect();
    println!("{}", t_detail(&spec_cells));

    println!("\n# elapsed {:.1}s", t0.elapsed().as_secs_f64());
}
