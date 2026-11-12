//! D4：三种衰减模式（标准 / 有下限 / 关闭）× 潜伏者数量的网络整体健康对比
//!
//! 这是 L5 的三模式版本。骨架、统计口径、测量管线全部照抄 crates/exp/src/bin/l5_health.rs：
//! 真多 runtime（每个账户一个 tm_engine::Engine），离线节点不 tick、不收观点，
//! 观点年龄真实增长，衰减真实生效。
//!
//! 关键结构差异（为了配对）：World 的轨迹与衰减模式完全无关，
//! 所以同一个 (seed, s) 只跑一次 World，然后用同一份 ratings / online 轨迹
//! 分别喂给三种 EngineConfig。三种模式因此是**同一个世界的三次测量**，可以逐 seed 配对作差。
//!
//! 固化：n_initial=300, steps=400, join_rate=0.05, leave_rate=0.001, return_rate=0.10,
//!       enable_rhythm=false, sleep_hours=(200,600), dormancy 二分标定到离线 ≈ 60%
//! 网格：3 模式 × s ∈ {0,5,20} × seed ∈ {11,23,37} = 27 次
//!
//! 另带 --l5check：用 L5 的原始设置（return_rate=0.05，4 个 p × s∈{0,20} × 8 seed，标准衰减）
//! 复现 L5 表 2-3 的两个基准数字（0.3403 / 0.3352），用来核对口径。
use std::collections::HashSet;
use std::time::Instant;

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::ViewParams;

const N: usize = 300;
const STEPS: usize = 400;
const ROUNDS: u64 = 10;
const SAMPLES: usize = 12;
const SEEDS: [u64; 3] = [11, 23, 37];
const SS: [usize; 3] = [0, 5, 20];
/// 目标离线比例（任务：dormancy 标定到约 60% 离线）
const P_TARGET: f64 = 0.60;
/// 潜伏者突变时刻：总步数的 1/5（与 grid.rs / l5_health.rs 一致）
const NORMAL_UNTIL: u64 = (STEPS / 5) as u64;
/// 任务固化：return_rate = 0.10
const RET_MAIN: f64 = 0.10;
/// 与 L5 一致的休眠时长区间（任务未规定，沿用 L5 以便对比）
const SLEEP: (u64, u64) = (200, 600);

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

// ════════════════════════════ 三种衰减模式 ════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// 标准：factor = 0.5^(Δt/72)
    Std,
    /// 有下限：factor = max(0.5^(Δt/72), 0.5)，最多衰减一半
    Floor,
    /// 关闭：factor 恒为 1
    Off,
}

const MODES: [Mode; 3] = [Mode::Std, Mode::Floor, Mode::Off];

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Std => "标准",
            Mode::Floor => "有下限 0.5",
            Mode::Off => "关闭",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Mode::Std => "std",
            Mode::Floor => "floor0.5",
            Mode::Off => "off",
        }
    }
    /// (decay_enabled, decay_floor)
    fn knob(self) -> (bool, f32) {
        match self {
            Mode::Std => (true, 0.0),
            Mode::Floor => (true, 0.5),
            Mode::Off => (false, 0.0),
        }
    }
    fn ecfg(self) -> EngineConfig {
        let (en, fl) = self.knob();
        EngineConfig {
            beta: 0.3,
            half_life: 72,
            view_params: ViewParams {
                buckets: 2048,
                hashers: 5,
                seed: 99,
            },
            agg_mode: AggMode::Mean,
            noise_multiplier: 1.0,
            decay_enabled: en,
            decay_floor: fl,
        ..Default::default()
        }
    }
}

// ════════════════════════════ 配置与标定 ════════════════════════════

/// 网格底座。enable_rhythm=false：在线与否只由休眠决定。
fn cfg_with(seed: u64, dormancy: f64, sleep: (u64, u64), ret: f64) -> SimConfig {
    SimConfig {
        n_initial: N,
        steps: STEPS,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dormancy,
        sleep_hours: sleep,
        return_rate: ret,
        enable_rhythm: false,
        ..Default::default()
    }
}

/// 离线比例：存活账户里，在 t = STEPS 时刻不在线的占比。
fn offline_frac(seed: u64, dormancy: f64, sleep: (u64, u64), ret: f64) -> (f64, usize, usize) {
    let mut w = World::new(cfg_with(seed, dormancy, sleep, ret));
    w.run();
    let accs = w.accounts();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    if alive.is_empty() {
        return (0.0, 0, 0);
    }
    let off = alive
        .iter()
        .filter(|&&i| !w.was_online_at(i, STEPS as u64))
        .count();
    (off as f64 / alive.len() as f64, off, alive.len())
}

/// 二分找能命中目标离线比例的 dormancy_rate（D4 主网格：区间 [1e-6, 0.20]，20 次）。
fn calibrate(target: f64, seed: u64, sleep: (u64, u64), ret: f64) -> (f64, f64) {
    calibrate_with(target, seed, sleep, ret, 0.20, 20)
}

/// 与 l5_health.rs 逐字一致的标定（区间 [1e-6, 0.05]，14 次），只用于 L5 复现。
fn calibrate_l5(target: f64, seed: u64, sleep: (u64, u64), ret: f64) -> (f64, f64) {
    calibrate_with(target, seed, sleep, ret, 0.05, 14)
}

fn calibrate_with(
    target: f64,
    seed: u64,
    sleep: (u64, u64),
    ret: f64,
    hi0: f64,
    iters: usize,
) -> (f64, f64) {
    let (mut lo, mut hi) = (1e-6f64, hi0);
    let mut best = (lo, offline_frac(seed, lo, sleep, ret).0);
    for _ in 0..iters {
        let mid = 0.5 * (lo + hi);
        let f = offline_frac(seed, mid, sleep, ret).0;
        if (f - target).abs() < (best.1 - target).abs() {
            best = (mid, f);
        }
        if f < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
}

/// D4 主网格用：二分定位后，再在 ±20% 邻域做 41 点线性扫描取最近。
/// 离线比例是阶跃函数（存活数是整数），二分只能落在阶跃的一侧。
fn calibrate_fine(target: f64, seed: u64, sleep: (u64, u64), ret: f64) -> (f64, f64, f64, f64) {
    let (r0, f0) = calibrate(target, seed, sleep, ret);
    let mut best = (r0, f0);
    for k in -20i32..=20 {
        let r = r0 * (1.0 + k as f64 * 0.01);
        if r <= 0.0 {
            continue;
        }
        let f = offline_frac(seed, r, sleep, ret).0;
        if (f - target).abs() < (best.1 - target).abs() {
            best = (r, f);
        }
    }
    (r0, f0, best.0, best.1)
}

// ════════════════════════════ 统计工具 ════════════════════════════

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn sd(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let v = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64;
    v.sqrt()
}

/// 平均秩（处理并列）。
fn midrank(xs: &[f64]) -> Vec<f64> {
    let n = xs.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| xs[a].partial_cmp(&xs[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j + 1 < n && xs[idx[j + 1]] == xs[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            r[idx[k]] = avg;
        }
        i = j + 1;
    }
    r
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len();
    if n < 3 || b.len() != n {
        return None;
    }
    let ma = a.iter().sum::<f64>() / n as f64;
    let mb = b.iter().sum::<f64>() / n as f64;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let da = a[i] - ma;
        let db = b[i] - mb;
        sab += da * db;
        saa += da * da;
        sbb += db * db;
    }
    if saa <= 1e-12 || sbb <= 1e-12 {
        return None;
    }
    Some(sab / (saa.sqrt() * sbb.sqrt()))
}

/// Spearman 秩相关（并列取平均秩）。
fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 3 || x.len() != y.len() {
        return None;
    }
    pearson(&midrank(x), &midrank(y))
}

/// 基尼系数（输入为非负向量，内部按总和归一）。
fn gini(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let mut v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite() && *x >= 0.0).collect();
    if v.len() < 2 {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len() as f64;
    let sum: f64 = v.iter().sum();
    if sum <= 0.0 {
        return None;
    }
    let mut acc = 0.0;
    for (i, x) in v.iter().enumerate() {
        acc += (i as f64 + 1.0) * x;
    }
    Some((2.0 * acc) / (n * sum) - (n + 1.0) / n)
}

fn dist_share(dist: &[(u32, f32)], i: usize) -> f64 {
    dist.iter()
        .find(|(idx, _)| *idx as usize == i)
        .map(|(_, v)| *v as f64)
        .unwrap_or(0.0)
}

// ════════════════════════════ 世界（与衰减模式无关） ════════════════════════════

#[allow(dead_code)]
struct WorldData {
    seed: u64,
    s: usize,
    dormancy: f64,
    n: usize,
    quality: Vec<f64>,
    /// 潜伏者 = Sleeper（绝不用 is_attacker() 二分）
    is_att: Vec<bool>,
    /// 收益方 = Promoted（单独一类）
    is_prom: Vec<bool>,
    alive: Vec<usize>,
    alive_set: HashSet<usize>,
    ratings: Vec<(usize, usize, f32)>,
    /// online[i][r] = 第 r 个交换轮次 i 是否在线
    online: Vec<Vec<bool>>,
    p_actual: f64,
    n_sleepers: usize,
    n_slp_alive: usize,
    slp_rated: usize,
    target: Option<usize>,
}

fn build_world(seed: u64, s: usize, dormancy: f64, sleep: (u64, u64), ret: f64) -> WorldData {
    let mut cfg = cfg_with(seed, dormancy, sleep, ret);
    if s > 0 {
        cfg.attack_kind = AttackKind::Sleeper {
            normal_until: NORMAL_UNTIL,
        };
        cfg.attack_ratio = s as f64 / N as f64;
    } else {
        cfg.attack_kind = AttackKind::None;
        cfg.attack_ratio = 0.0;
    }

    let mut world = World::new(cfg);
    world.run();

    let ratings = world.ratings();
    let accs = world.accounts();
    // 只用 accounts().len()，绝不用 alive_count()（新账户编号必须覆盖全部账户）
    let n = accs.len();
    let quality: Vec<f64> = accs.iter().map(|a| a.quality as f64).collect();
    let is_att: Vec<bool> = accs
        .iter()
        .map(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .collect();
    let is_prom: Vec<bool> = accs.iter().map(|a| a.kind == Kind::Promoted).collect();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let alive_set: HashSet<usize> = alive.iter().copied().collect();
    let now = STEPS as u64;

    let off = alive
        .iter()
        .filter(|&&i| !world.was_online_at(i, now))
        .count();
    let p_actual = if alive.is_empty() {
        0.0
    } else {
        off as f64 / alive.len() as f64
    };

    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| {
            (0..=ROUNDS)
                .map(|r| world.was_online_at(i, r * (STEPS as u64 / ROUNDS)))
                .collect()
        })
        .collect();

    let n_sleepers = accs.iter().filter(|a| a.kind.is_attacker()).count();
    let n_slp_alive = alive.iter().filter(|&&i| is_att[i]).count();
    let slp_rated = ratings.iter().filter(|(_, t, _)| is_att[*t]).count();

    WorldData {
        seed,
        s,
        dormancy,
        n,
        quality,
        is_att,
        is_prom,
        alive,
        alive_set,
        ratings,
        online,
        p_actual,
        n_sleepers,
        n_slp_alive,
        slp_rated,
        target: world.attack.target,
    }
}

// ════════════════════════════ 一次测量（给定世界 + 衰减模式） ════════════════════════════

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct Run {
    mode: &'static str,
    seed: u64,
    s: usize,
    p_actual: f64,
    n_accounts: usize,
    n_alive: usize,
    n_obs: usize,
    n_obs_online: usize,
    n_slp_alive: usize,
    sp: f64,
    sp_n: usize,
    sp_alive: f64,
    sp_personal: f64,
    sp_on: f64,
    sp_on_n: usize,
    win: f64,
    win_n: usize,
    win_on: f64,
    win_on_n: usize,
    gini: f64,
    gini_on: f64,
    reach: f64,
    reach_on: f64,
    support: f64,
    sleeper_mass: f64,
    rel: f64,
    sleeper_visible: f64,
    best_sleeper_pct: f64,
    dead_mass: f64,
    sum_err: f64,
    prom_mass: f64,
    prom_pct: f64,
    prom_seen: f64,
    prom_quality: f64,
    prom_q_pct: f64,
    prom_alive: bool,
    n_prom: usize,
    mean_views: f64,
    mean_age: f64,
    mean_factor: f64,
    /// 群体项的 Kish 有效来源数（Σw）²/Σw²
    ess: f64,
}

fn run_pipeline(wd: &WorldData, mode: Mode) -> Run {
    let n = wd.n;
    let now = STEPS as u64;
    let ecfg = mode.ecfg();

    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &wd.alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n {
            e.intern(key(i));
        }
        for (o, t, sc) in &wd.ratings {
            if *o == a {
                e.rate(*t as u32, *sc, 0);
            }
        }
        nodes[a] = Some(e);
    }

    let mut had_gap = vec![false; n];
    let mut prev_on = vec![false; n];
    for r in 0..=ROUNDS {
        let t = r * (STEPS as u64 / ROUNDS);
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n];
        for &a in &wd.alive {
            if wd.online[a][r as usize] {
                if let Some(e) = &nodes[a] {
                    views[a] = Some(e.export_view(t));
                }
            }
        }
        for &a in &wd.alive {
            if !wd.online[a][r as usize] {
                prev_on[a] = false;
                had_gap[a] = true;
                continue;
            }
            let returning = had_gap[a] && !prev_on[a] && r > 0;
            let k = if returning { SAMPLES * 3 } else { SAMPLES };
            let reqs = { nodes[a].as_mut().unwrap().tick(t, k) };
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), t, t);
                }
            }
            prev_on[a] = true;
        }
    }

    // ─── 网络层面统计 ───
    let alive_n = wd.alive.len();
    let k_top = ((alive_n as f64) * 0.10).ceil() as usize;
    // 诚实 = 既不是 Sleeper，也不是 Promoted
    let honest_alive: Vec<usize> = wd
        .alive
        .iter()
        .copied()
        .filter(|&i| !wd.is_att[i] && !wd.is_prom[i])
        .collect();
    let sleeper_alive: Vec<usize> = wd.alive.iter().copied().filter(|&i| wd.is_att[i]).collect();
    let promoted_alive: Vec<usize> = wd.alive.iter().copied().filter(|&i| wd.is_prom[i]).collect();
    let promoted: Option<usize> = (0..n).find(|&i| wd.is_prom[i]);

    let mut sp_vec: Vec<f64> = Vec::new();
    let mut sp_alive_vec: Vec<f64> = Vec::new();
    let mut sp_personal_vec: Vec<f64> = Vec::new();
    let mut sp_on_vec: Vec<f64> = Vec::new();
    let mut gini_vec: Vec<f64> = Vec::new();
    let mut gini_on_vec: Vec<f64> = Vec::new();
    let (mut win, mut win_n) = (0usize, 0usize);
    let (mut win_on, mut win_on_n) = (0usize, 0usize);
    let mut reach: HashSet<usize> = HashSet::new();
    let mut reach_on: HashSet<usize> = HashSet::new();
    let mut support: Vec<f64> = Vec::new();
    let mut dead_mass_v: Vec<f64> = Vec::new();
    let mut sleeper_mass_v: Vec<f64> = Vec::new();
    let mut rel_v: Vec<f64> = Vec::new();
    let mut visible_n = 0usize;
    let mut best_pct: Vec<f64> = Vec::new();
    let mut prom_mass_v: Vec<f64> = Vec::new();
    let mut prom_pct_v: Vec<f64> = Vec::new();
    let (mut prom_seen_n, mut prom_obs_n) = (0usize, 0usize);
    let mut sum_err = 0.0f64;
    let mut views_cnt: Vec<f64> = Vec::new();
    let mut age_v: Vec<f64> = Vec::new();
    let mut factor_v: Vec<f64> = Vec::new();
    let mut ess_v: Vec<f64> = Vec::new();
    let mut own: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for (o, t, sc) in &wd.ratings {
        own[*o].push((*t, *sc));
    }

    for &obs in &honest_alive {
        let e = nodes[obs].as_ref().unwrap();
        let dist = e.trust_distribution(now);
        let total: f64 = dist.iter().map(|(_, v)| *v as f64).sum();
        sum_err = sum_err.max((total - 1.0).abs());
        let is_on = wd.online[obs][ROUNDS as usize];
        views_cnt.push(e.freshness_stats(now).count as f64);

        // 三种模式到底把权重压了多少：直接问 Engine::decay_factor
        {
            let ks = e.known_accounts();
            let mut a_sum = 0.0;
            let mut f_sum = 0.0;
            let mut cnt = 0usize;
            let (mut w_sum, mut w2_sum) = (0.0f64, 0.0f64);
            for o in ks {
                if let Some(age) = e.view_age(o, now) {
                    let f = e.decay_factor(age) as f64;
                    a_sum += age as f64;
                    f_sum += f;
                    w_sum += f;
                    w2_sum += f * f;
                    cnt += 1;
                }
            }
            if cnt > 0 {
                age_v.push(a_sum / cnt as f64);
                factor_v.push(f_sum / cnt as f64);
                // Kish 有效样本数：Σw² 越小，群体项实际被多少个来源主导
                if w2_sum > 1e-12 {
                    ess_v.push(w_sum * w_sum / w2_sum);
                }
            }
        }

        dead_mass_v.push(
            dist.iter()
                .filter(|(i, _)| !wd.alive_set.contains(&(*i as usize)))
                .map(|(_, v)| *v as f64)
                .sum::<f64>(),
        );

        let mut sup: Vec<(usize, f64)> = dist
            .iter()
            .filter(|(_, v)| *v > 0.0)
            .map(|(i, v)| (*i as usize, *v as f64))
            .collect();
        sup.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        support.push(sup.len() as f64);

        // 参照基线：只用自己的直接评分排序（等价于 β=1 的个人项）
        if own[obs].len() >= 3 {
            let xs: Vec<f64> = own[obs].iter().map(|(_, v)| *v as f64).collect();
            let ys: Vec<f64> = own[obs].iter().map(|(t, _)| wd.quality[*t]).collect();
            if let Some(r) = spearman(&xs, &ys) {
                sp_personal_vec.push(r);
            }
        }

        // ① Spearman（支持集上）
        if sup.len() >= 3 {
            let xs: Vec<f64> = sup.iter().map(|(_, v)| *v).collect();
            let ys: Vec<f64> = sup.iter().map(|(i, _)| wd.quality[*i]).collect();
            if let Some(r) = spearman(&xs, &ys) {
                sp_vec.push(r);
                if is_on {
                    sp_on_vec.push(r);
                }
            }
        }
        // ①b Spearman（全存活，未表态记 0）
        {
            let mut xs: Vec<f64> = vec![0.0; n];
            for (i, v) in &sup {
                xs[*i] = *v;
            }
            let xs_a: Vec<f64> = wd.alive.iter().map(|&i| xs[i]).collect();
            let ys_a: Vec<f64> = wd.alive.iter().map(|&i| wd.quality[i]).collect();
            if let Some(r) = spearman(&xs_a, &ys_a) {
                sp_alive_vec.push(r);
            }
        }

        // ② 胜率与潜伏者份额
        if !sleeper_alive.is_empty() && !honest_alive.is_empty() {
            let hm: f64 = honest_alive
                .iter()
                .map(|&i| dist_share(&dist, i))
                .sum::<f64>()
                / honest_alive.len() as f64;
            let sm: f64 = sleeper_alive
                .iter()
                .map(|&i| dist_share(&dist, i))
                .sum::<f64>()
                / sleeper_alive.len() as f64;
            win_n += 1;
            if hm > sm {
                win += 1;
                if is_on {
                    win_on += 1;
                }
            }
            if is_on {
                win_on_n += 1;
            }
            let smass: f64 = sleeper_alive
                .iter()
                .map(|&i| dist_share(&dist, i))
                .sum::<f64>();
            sleeper_mass_v.push(smass);
            if smass > 0.0 {
                visible_n += 1;
            }
            if hm > 0.0 {
                rel_v.push(sm / hm);
            }
        }

        // 排名：存活账户按 share 降序（同分按内部编号升序）
        let mut ranks: Vec<(usize, f64)> = wd
            .alive
            .iter()
            .map(|&i| (i, dist_share(&dist, i)))
            .collect();
        ranks.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let pct_of = |idx: usize| -> f64 {
            match ranks.iter().position(|(i, _)| *i == idx) {
                Some(pos) => 1.0 - pos as f64 / alive_n as f64,
                None => f64::NAN,
            }
        };
        if !sleeper_alive.is_empty() {
            let mut best = f64::NAN;
            for &i in &sleeper_alive {
                let x = pct_of(i);
                if !x.is_nan() && (best.is_nan() || x > best) {
                    best = x;
                }
            }
            if !best.is_nan() {
                best_pct.push(best);
            }
        }

        // ②b 收益方（Promoted）：单独统计
        if let Some(p) = promoted {
            prom_obs_n += 1;
            let pm = dist_share(&dist, p);
            prom_mass_v.push(pm);
            if pm > 0.0 {
                prom_seen_n += 1;
            }
            if promoted_alive.contains(&p) {
                let x = pct_of(p);
                if !x.is_nan() {
                    prom_pct_v.push(x);
                }
            }
        }

        // ③ 基尼（支持集上）
        if sup.len() >= 2 {
            let xs: Vec<f64> = sup.iter().map(|(_, v)| *v).collect();
            if let Some(g) = gini(&xs) {
                gini_vec.push(g);
                if is_on {
                    gini_on_vec.push(g);
                }
            }
        }

        // ④ 前 10% 覆盖
        for (i, _) in sup.iter().take(k_top) {
            reach.insert(*i);
            if is_on {
                reach_on.insert(*i);
            }
        }
    }

    let honest_alive_set: HashSet<usize> = honest_alive.iter().copied().collect();
    let reach_n = reach.intersection(&honest_alive_set).count() as f64;
    let reach_on_n = reach_on.intersection(&honest_alive_set).count() as f64;

    Run {
        mode: mode.tag(),
        seed: wd.seed,
        s: wd.s,
        p_actual: wd.p_actual,
        n_accounts: n,
        n_alive: alive_n,
        n_obs: honest_alive.len(),
        n_obs_online: honest_alive
            .iter()
            .filter(|&&i| wd.online[i][ROUNDS as usize])
            .count(),
        n_slp_alive: sleeper_alive.len(),
        sp: mean(&sp_vec),
        sp_n: sp_vec.len(),
        sp_alive: mean(&sp_alive_vec),
        sp_personal: mean(&sp_personal_vec),
        sp_on: mean(&sp_on_vec),
        sp_on_n: sp_on_vec.len(),
        win: if win_n > 0 {
            win as f64 / win_n as f64
        } else {
            f64::NAN
        },
        win_n,
        win_on: if win_on_n > 0 {
            win_on as f64 / win_on_n as f64
        } else {
            f64::NAN
        },
        win_on_n,
        gini: mean(&gini_vec),
        gini_on: mean(&gini_on_vec),
        reach: if honest_alive.is_empty() {
            0.0
        } else {
            reach_n / honest_alive.len() as f64
        },
        reach_on: if honest_alive.is_empty() {
            0.0
        } else {
            reach_on_n / honest_alive.len() as f64
        },
        support: mean(&support),
        sleeper_mass: mean(&sleeper_mass_v),
        rel: mean(&rel_v),
        sleeper_visible: if win_n > 0 {
            visible_n as f64 / win_n as f64
        } else {
            f64::NAN
        },
        best_sleeper_pct: mean(&best_pct),
        dead_mass: mean(&dead_mass_v),
        sum_err,
        prom_mass: mean(&prom_mass_v),
        prom_pct: if prom_pct_v.is_empty() {
            f64::NAN
        } else {
            mean(&prom_pct_v)
        },
        prom_seen: if prom_obs_n > 0 {
            prom_seen_n as f64 / prom_obs_n as f64
        } else {
            f64::NAN
        },
        prom_quality: match promoted {
            Some(p) => wd.quality[p],
            None => f64::NAN,
        },
        prom_q_pct: match promoted {
            Some(p) => {
                let q = wd.quality[p];
                wd.alive.iter().filter(|&&i| wd.quality[i] < q).count() as f64
                    / alive_n.max(1) as f64
            }
            None => f64::NAN,
        },
        prom_alive: !promoted_alive.is_empty(),
        n_prom: wd.is_prom.iter().filter(|x| **x).count(),
        mean_views: mean(&views_cnt),
        mean_age: mean(&age_v),
        mean_factor: mean(&factor_v),
        ess: mean(&ess_v),
    }
}

// ════════════════════════════ 输出 ════════════════════════════

fn f4(v: f64) -> String {
    if v.is_nan() {
        "—".to_string()
    } else {
        format!("{v:.4}")
    }
}

/// 配对差：均值 ± sd，并给出 t = 均值 / 标准误。
fn pt(v: &[f64], dec: usize) -> String {
    let v: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if v.is_empty() {
        return "—".to_string();
    }
    let n = v.len();
    let s = sd(&v);
    let se = if n > 1 {
        s / (n as f64).sqrt()
    } else {
        f64::NAN
    };
    let t = if se.is_finite() && se > 1e-12 {
        format!("t={:+.1}", mean(&v) / se)
    } else {
        "t=—".to_string()
    };
    format!("{:.*} ± {:.*} ({t}, n={n})", dec, mean(&v), dec, s)
}

fn pm(v: &[f64], dec: usize) -> String {
    let v: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if v.is_empty() {
        return "—".to_string();
    }
    format!(
        "{:.*} ± {:.*}",
        dec,
        mean(&v),
        dec,
        sd(&v)
    )
}

fn ms(rs: &[&Run], f: fn(&Run) -> f64, dec: usize) -> String {
    let v: Vec<f64> = rs.iter().map(|r| f(r)).collect();
    pm(&v, dec)
}

fn sel<'a>(all: &'a [Run], tag: &str, s: usize, seeds: &[u64]) -> Vec<&'a Run> {
    all.iter()
        .filter(|r| r.mode == tag && r.s == s && seeds.contains(&r.seed))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut seeds: Vec<u64> = SEEDS.to_vec();
    if let Some(i) = args.iter().position(|a| a == "--seeds") {
        seeds = args[i + 1]
            .split(',')
            .map(|x| x.trim().parse().unwrap())
            .collect();
    }
    let ss: Vec<usize> = match args.iter().position(|a| a == "--s") {
        Some(i) => args[i + 1]
            .split(',')
            .map(|x| x.trim().parse().unwrap())
            .collect(),
        None => SS.to_vec(),
    };
    let l5check = args.iter().any(|a| a == "--l5check");
    if args.iter().any(|a| a == "--l5only") {
        l5_reproduce();
        return;
    }
    let t0 = Instant::now();

    println!("# D4 三种衰减模式 × 网络整体健康");
    println!(
        "# n_initial={N} steps={STEPS} join_rate=0.05 leave_rate=0.001 return_rate={RET_MAIN} sleep_hours=({},{}) enable_rhythm=false",
        SLEEP.0, SLEEP.1
    );
    println!(
        "# 交换协议：{ROUNDS} 轮 × 每轮 {SAMPLES} 个来源（回归节点 ×3），半衰期 72h，beta=0.3，view 2048x5 seed=99"
    );
    println!("# 潜伏者突变时刻 normal_until={NORMAL_UNTIL}（steps/5）");
    println!("# 离线目标 = {:.0}%（逐 seed 二分标定 dormancy_rate）", P_TARGET * 100.0);
    println!("# seeds = {seeds:?}   s = {ss:?}\n");

    // ── 0. 三种模式的衰减因子本身 ──
    println!("## 表 0：Engine::decay_factor(dt) 实测（半衰期 72h）\n");
    print!("| 模式 | decay_enabled | decay_floor |");
    let dts: [u64; 10] = [0, 12, 24, 36, 48, 72, 96, 144, 216, 400];
    for d in dts {
        print!(" Δt={d} |");
    }
    println!();
    print!("|---|---|---|");
    for _ in dts {
        print!("---|");
    }
    println!();
    for m in MODES {
        let e = Engine::new(m.ecfg());
        let (en, fl) = m.knob();
        print!(
            "| {} | {} | {} |",
            m.label(),
            if en { "true" } else { "false" },
            if fl == 0.0 { "0.0".to_string() } else { format!("{fl:.1}") }
        );
        for d in dts {
            print!(" {:.4} |", e.decay_factor(d));
        }
        println!();
    }
    println!();

    // ── 1. 标定 ──
    println!("## 标定：dormancy_rate → {:.0}% 离线\n", P_TARGET * 100.0);
    println!("| seed | 二分dormancy | 二分实测 | 扫描后dormancy | 最终实测 | 离线/存活 |");
    println!("|---:|---:|---:|---:|---:|---:|");
    let mut calib: Vec<(u64, f64)> = Vec::new();
    for &seed in &seeds {
        let (r0, f0, rate, achieved) = calibrate_fine(P_TARGET, seed, SLEEP, RET_MAIN);
        let (_, off, alive) = offline_frac(seed, rate, SLEEP, RET_MAIN);
        println!(
            "| {seed} | {r0:.7} | {:.1}% | {rate:.7} | {:.1}% | {off}/{alive} |",
            f0 * 100.0,
            achieved * 100.0
        );
        calib.push((seed, rate));
    }
    println!();
    println!("粒度核对（同一 seed 在最优 dormancy 附近 ±2% 的离线比例，说明阶跃）：
");
    println!("| seed | ×0.96 | ×0.98 | ×1.00 | ×1.02 | ×1.04 |");
    println!("|---:|---:|---:|---:|---:|---:|");
    for &(seed, rate) in &calib {
        print!("| {seed} |");
        for k in [-4i32, -2, 0, 2, 4] {
            let r = rate * (1.0 + k as f64 * 0.01);
            let (f, _, _) = offline_frac(seed, r, SLEEP, RET_MAIN);
            print!(" {:.1}% |", f * 100.0);
        }
        println!();
    }
    println!();

    // ── 2. 世界只跑一次，三种模式共享 ──
    let mut all: Vec<Run> = Vec::new();
    println!("## 明细（每个 模式 × s × seed 一行，共 {} 次测量）\n", calib.len() * ss.len() * MODES.len());
    println!("| 模式 | seed | s | off% | 账户 | 存活 | 观察者 | 末轮在线 | 潜伏存活 | Spearman | Spearman0 | 只靠自己 | Spearman(在线) | 胜率 | 基尼 | 可达性 | 可达性(在线) | 支持集 | 平均观点龄 | 平均因子 | 潜伏份额 | 潜/诚人均 | 被看见 | 收益方份额 | 收益方分位 | 收益方存活 | 死账户 | 总和误差 |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|");
    for (seed, rate) in &calib {
        for &s in &ss {
            let wd = build_world(*seed, s, *rate, SLEEP, RET_MAIN);
            for m in MODES {
                let r = run_pipeline(&wd, m);
                println!(
                    "| {} | {} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.4} | {:.5} | {} | {} | {} | {} | {} | {} | {:.2e} |",
                    r.mode,
                    r.seed,
                    r.s,
                    r.p_actual * 100.0,
                    r.n_accounts,
                    r.n_alive,
                    r.n_obs,
                    r.n_obs_online,
                    r.n_slp_alive,
                    f4(r.sp),
                    f4(r.sp_alive),
                    f4(r.sp_personal),
                    f4(r.sp_on),
                    f4(r.win),
                    f4(r.gini),
                    f4(r.reach),
                    f4(r.reach_on),
                    r.support,
                    r.mean_age,
                    r.mean_factor,
                    r.sleeper_mass,
                    f4(r.rel),
                    f4(r.sleeper_visible),
                    f4(r.prom_mass),
                    f4(r.prom_pct),
                    if r.s == 0 { "—".to_string() } else if r.prom_alive { "存活".to_string() } else { "已离开".to_string() },
                    f4(r.dead_mass),
                    r.sum_err
                );
                all.push(r);
            }
        }
    }
    println!();

    // ── 3. 主表：模式 × s ──
    println!("## 表 1：主表（{} seed 均值 ± 样本标准差）\n", seeds.len());
    println!("| 模式 | s | off% | 存活 | 观察者 | 末轮在线 | **Spearman** | Spearman0 | 只靠自己 | **基尼** | **可达性** | 可达性(在线) | 支持集 | 有效来源(ESS) | 平均观点龄 | 平均因子 | 死账户 |");
    println!("|---|---:|---:|---:|---:|---:|---|---|---|---|---|---|---:|---:|---:|---:|---:|");
    for m in MODES {
        for &s in &ss {
            let rs = sel(&all, m.tag(), s, &seeds);
            if rs.is_empty() {
                continue;
            }
            let off = mean(&rs.iter().map(|r| r.p_actual * 100.0).collect::<Vec<_>>());
            let a = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
            println!(
                "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.4} | {} |",
                m.label(),
                s,
                off,
                a(|r| r.n_alive as f64),
                a(|r| r.n_obs as f64),
                a(|r| r.n_obs_online as f64),
                ms(&rs, |r| r.sp, 4),
                ms(&rs, |r| r.sp_alive, 4),
                ms(&rs, |r| r.sp_personal, 4),
                ms(&rs, |r| r.gini, 4),
                ms(&rs, |r| r.reach, 4),
                ms(&rs, |r| r.reach_on, 4),
                a(|r| r.support),
                a(|r| r.ess),
                a(|r| r.mean_age),
                a(|r| r.mean_factor),
                ms(&rs, |r| r.dead_mass, 3),
            );
        }
    }
    println!();

    // ── 4. 配对对比：同一世界三次测量 ──
    println!(
        "## 表 2：配对对比（同一 (seed,s) 世界内，相对「标准」模式的差；{} seed 均值 ± sd，t = 均值/标准误）\n",
        seeds.len()
    );
    println!("| s | 对比 | ΔSpearman | Δ基尼 | Δ可达性 | Δ可达性(在线) | Δ支持集 | Δ平均因子 |");
    println!("|---:|---|---|---|---|---|---|---|");
    for &s in &ss {
        for target in [Mode::Floor, Mode::Off] {
            let mut d_sp = Vec::new();
            let mut d_gi = Vec::new();
            let mut d_re = Vec::new();
            let mut d_ro = Vec::new();
            let mut d_su = Vec::new();
            let mut d_fa = Vec::new();
            for &seed in &seeds {
                let base = all
                    .iter()
                    .find(|r| r.mode == "std" && r.s == s && r.seed == seed);
                let tr = all
                    .iter()
                    .find(|r| r.mode == target.tag() && r.s == s && r.seed == seed);
                if let (Some(b), Some(t)) = (base, tr) {
                    d_sp.push(t.sp - b.sp);
                    d_gi.push(t.gini - b.gini);
                    d_re.push(t.reach - b.reach);
                    d_ro.push(t.reach_on - b.reach_on);
                    d_su.push(t.support - b.support);
                    d_fa.push(t.mean_factor - b.mean_factor);
                }
            }
            println!(
                "| {s} | 标准 → {} | {} | {} | {} | {} | {} | {} |",
                target.label(),
                pt(&d_sp, 4),
                pt(&d_gi, 4),
                pt(&d_re, 4),
                pt(&d_ro, 4),
                pm(&d_su, 1),
                pt(&d_fa, 4)
            );
        }
    }
    println!();

    // ── 4b. 全网格合并的配对差 ──
    println!(
        "## 表 2b：把 {} 个 s 全部合并的配对差（每模式 {} 对，t = 均值/标准误）\n",
        ss.len(),
        ss.len() * seeds.len()
    );
    println!("| 对比 | ΔSpearman | Δ基尼 | Δ可达性 | Δ可达性(在线) | ΔESS |");
    println!("|---|---|---|---|---|---|");
    for target in [Mode::Floor, Mode::Off] {
        let (mut d_sp, mut d_gi, mut d_re, mut d_ro, mut d_es) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for &s in &ss {
            for &seed in &seeds {
                let base = all
                    .iter()
                    .find(|r| r.mode == "std" && r.s == s && r.seed == seed);
                let tr = all
                    .iter()
                    .find(|r| r.mode == target.tag() && r.s == s && r.seed == seed);
                if let (Some(b), Some(t)) = (base, tr) {
                    d_sp.push(t.sp - b.sp);
                    d_gi.push(t.gini - b.gini);
                    d_re.push(t.reach - b.reach);
                    d_ro.push(t.reach_on - b.reach_on);
                    d_es.push(t.ess - b.ess);
                }
            }
        }
        println!(
            "| 标准 → {} | {} | {} | {} | {} | {} |",
            target.label(),
            pt(&d_sp, 4),
            pt(&d_gi, 4),
            pt(&d_re, 4),
            pt(&d_ro, 4),
            pt(&d_es, 2)
        );
    }
    println!();

    // ── 5. 潜伏者视角 ──
    println!(
        "## 表 3：潜伏者（Sleeper）视角（{} seed 均值 ± sd；s=0 无定义）\n",
        seeds.len()
    );
    println!("| 模式 | s | 胜率 | 胜率(在线) | 潜伏者份额 | 潜伏/诚实人均 | 被看见 | 最高潜伏者分位 |");
    println!("|---|---:|---|---|---|---|---|---|");
    for m in MODES {
        for &s in &ss {
            if s == 0 {
                continue;
            }
            let rs = sel(&all, m.tag(), s, &seeds);
            if rs.is_empty() {
                continue;
            }
            let a = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                m.label(),
                s,
                ms(&rs, |r| r.win, 4),
                ms(&rs, |r| r.win_on, 4),
                ms(&rs, |r| r.sleeper_mass, 5),
                ms(&rs, |r| r.rel, 3),
                ms(&rs, |r| r.sleeper_visible, 4),
                ms(&rs, |r| r.best_sleeper_pct, 4),
            );
            let _ = a;
        }
    }
    println!();

    // ── 6. 收益方 ──
    println!("## 表 4：收益方（Promoted，单独统计；存活/已离开分开报）\n");
    println!("| 模式 | s | 存活 seed | 已离开 seed | 收益方份额 | 收益方分位(仅存活) | 收益方被看见 | 收益方质量 | 质量分位 |");
    println!("|---|---:|---:|---:|---|---|---|---|---|");
    for m in MODES {
        for &s in &ss {
            if s == 0 {
                continue;
            }
            let rs = sel(&all, m.tag(), s, &seeds);
            if rs.is_empty() {
                continue;
            }
            let alive_n = rs.iter().filter(|r| r.prom_alive).count();
            let left_n = rs.len() - alive_n;
            let a = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
            let pv: Vec<f64> = rs
                .iter()
                .filter(|r| r.prom_alive)
                .map(|r| r.prom_pct)
                .filter(|x| !x.is_nan())
                .collect();
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.4} | {:.4} |",
                m.label(),
                s,
                alive_n,
                left_n,
                ms(&rs, |r| r.prom_mass, 5),
                pm(&pv, 4),
                ms(&rs, |r| r.prom_seen, 4),
                a(|r| r.prom_quality),
                a(|r| r.prom_q_pct),
            );
        }
    }
    println!();

    // ── 7. 同一 seed 下三模式并排（人眼核对用） ──
    println!("## 表 5：逐 seed 三模式并排（Spearman / 基尼 / 可达性）\n");
    println!("| seed | s | 标准 ρ | 有下限 ρ | 关闭 ρ | 标准 G | 有下限 G | 关闭 G | 标准 R | 有下限 R | 关闭 R |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for &seed in &seeds {
        for &s in &ss {
            let get = |t: &str| {
                all.iter()
                    .find(|r| r.mode == t && r.s == s && r.seed == seed)
                    .cloned()
            };
            let (a, b, c) = (get("std"), get("floor0.5"), get("off"));
            if let (Some(a), Some(b), Some(c)) = (a, b, c) {
                println!(
                    "| {seed} | {s} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    f4(a.sp),
                    f4(b.sp),
                    f4(c.sp),
                    f4(a.gini),
                    f4(b.gini),
                    f4(c.gini),
                    f4(a.reach),
                    f4(b.reach),
                    f4(c.reach)
                );
            }
        }
    }
    println!();

    // ── 8. 档位汇总：把 3 个 s 合并，看模式的平均表现 ──
    println!(
        "## 表 6：把 {} 个 s 合并（每模式 {} 个世界，均值 ± 标准误 / sd）\n",
        ss.len(),
        ss.len() * seeds.len()
    );
    println!("| 模式 | Spearman | 基尼 | 可达性 | 可达性(在线) | 支持集 | 有效来源(ESS) | 平均观点龄 | 平均因子 |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for m in MODES {
        let rs: Vec<&Run> = all.iter().filter(|r| r.mode == m.tag()).collect();
        let se = |f: fn(&Run) -> f64| -> String {
            let v: Vec<f64> = rs.iter().map(|r| f(r)).filter(|x| !x.is_nan()).collect();
            if v.is_empty() {
                return "—".to_string();
            }
            format!(
                "{:.4} ± {:.4} (sd {:.4})",
                mean(&v),
                sd(&v) / (v.len() as f64).sqrt(),
                sd(&v)
            )
        };
        let a = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
        println!(
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.4} |",
            m.label(),
            se(|r| r.sp),
            se(|r| r.gini),
            se(|r| r.reach),
            se(|r| r.reach_on),
            a(|r| r.support),
            a(|r| r.ess),
            a(|r| r.mean_age),
            a(|r| r.mean_factor)
        );
    }
    println!();

    println!("总耗时 {:.1}s", t0.elapsed().as_secs_f64());

    // ════════════════ L5 基准复现 ════════════════
    if l5check {
        l5_reproduce();
    }
}

/// 用 L5 的原始设置复现 L5 表 2-3 的两个基准数字（标准衰减）：
///   s=0 → 0.3403，s=20 → 0.3352
/// L5 的 p ∈ {30,50,70,90}、8 seed、return_rate=0.05、标准衰减，合并 32 个世界的均值。
fn l5_reproduce() {
    const PS: [f64; 4] = [0.30, 0.50, 0.70, 0.90];
    const SEEDS8: [u64; 8] = [11, 23, 37, 53, 71, 89, 97, 113];
    const SS2: [usize; 2] = [0, 20];
    const RET_L5: f64 = 0.05;
    let t0 = Instant::now();

    println!("\n\n# 附：L5 基准复现（标准衰减，return_rate=0.05，4 p × 2 s × 8 seed，合并 32 世界）\n");
    let mut rows: Vec<(f64, Run)> = Vec::new();
    for &p in &PS {
        for &seed in &SEEDS8 {
            let (rate, _ach) = calibrate_l5(p, seed, SLEEP, RET_L5);
            for &s in &SS2 {
                let wd = build_world(seed, s, rate, SLEEP, RET_L5);
                let r = run_pipeline(&wd, Mode::Std);
                rows.push((p, r));
            }
        }
        println!("  p={:.0}% 完成（{:.0}s）", p * 100.0, t0.elapsed().as_secs_f64());
    }

    println!("\n## 表 7：L5 复现 —— 逐 p（8 seed 均值 ± sd）\n");
    println!("| p | s | 实测 off% | Spearman | 基尼 | 可达性 | 观察者 |");
    println!("|---:|---:|---:|---|---|---|---:|");
    for &p in &PS {
        for &s in &SS2 {
            let rs: Vec<&Run> = rows
                .iter()
                .filter(|(q, r)| (*q - p).abs() < 1e-9 && r.s == s)
                .map(|(_, r)| r)
                .collect();
            println!(
                "| {:.0}% | {} | {:.1} | {} | {} | {} | {:.1} |",
                p * 100.0,
                s,
                mean(&rs.iter().map(|r| r.p_actual * 100.0).collect::<Vec<_>>()),
                ms(&rs, |r| r.sp, 4),
                ms(&rs, |r| r.gini, 4),
                ms(&rs, |r| r.reach, 4),
                mean(&rs.iter().map(|r| r.n_obs as f64).collect::<Vec<_>>())
            );
        }
    }

    println!("\n## 表 8：L5 复现 —— 合并 32 个世界（均值 ± 标准误，sd）\n");
    println!("| s | Spearman | L5 报告值 | 差 | Spearman0 | 基尼 | 可达性 |");
    println!("|---:|---|---|---:|---|---|---|");
    for &s in &SS2 {
        let rs: Vec<&Run> = rows.iter().filter(|(_, r)| r.s == s).map(|(_, r)| r).collect();
        let se = |f: fn(&Run) -> f64| -> String {
            let v: Vec<f64> = rs.iter().map(|r| f(r)).filter(|x| !x.is_nan()).collect();
            format!(
                "{:.4} ± {:.4} (sd {:.4})",
                mean(&v),
                sd(&v) / (v.len() as f64).sqrt(),
                sd(&v)
            )
        };
        let got = mean(&rs.iter().map(|r| r.sp).collect::<Vec<_>>());
        let want = if s == 0 { 0.3403 } else { 0.3352 };
        println!(
            "| {s} | {} | {want:.4} | {:+.4} | {} | {} | {} |",
            se(|r| r.sp),
            got - want,
            se(|r| r.sp_alive),
            se(|r| r.gini),
            se(|r| r.reach)
        );
    }
    println!("\nL5 复现耗时 {:.1}s", t0.elapsed().as_secs_f64());
}
