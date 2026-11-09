//! D3 · 长期离线账户对网络的影响（标准 / 有下限 / 关闭 三种衰减模式）
//!
//! 核心问题：衰减的目的就是防「长期离线账户」。那关掉衰减 / 加下限（0.5）后果是什么？
//! **不放任何攻击者**（attack_kind = None），只测离线本身。
//!
//! 模板 = crates/exp/src/bin/l4_olduser.rs：真多 runtime，每个账户一个独立 Engine。
//! 离线节点不 tick、不发布观点；观点年龄真实增长（衰减生效）；回归节点当轮 3× 抽样。
//!
//! 固化（任务给定）：
//!   n_initial=300, steps=400, seed∈{11,23,37},
//!   join_rate=0.05, leave_rate=0.001, return_rate=0.10,
//!   enable_rhythm=false（离线只由休眠决定，离线 == 休眠），
//!   attack_kind=None（无攻击者）。
//!
//! 扫描：3 种衰减模式 × 离线比例 p∈{30,50,70,90}% × 3 seed = 36 次。
//!   std   = { decay_enabled: true,  decay_floor: 0.0 }   0.5^(Δt/72)
//!   floor = { decay_enabled: true,  decay_floor: 0.5 }   max(f, 0.5)
//!   off   = { decay_enabled: false, decay_floor: 0.0 }   factor 恒为 1
//!
//! 离线时长取 sleep_hours = (216, 720) = 3~10 个半衰期（长期离线）：
//! 上端 720h > 400h 窗口，窗口内醒不过来；下端 216h 会在窗口内醒来并以
//! return_rate=0.10 判定回归，90% 判定流失 → 于是「休眠」与「已离开」两类
//! 「占坑」都能被观察到，而不是只有一种。
//!
//! 标定：dormancy_rate 每格标定（对数网格粗扫 15 点 + 对数二分细化 8 次），
//! 目标是 t=400 时「存活账户里不在线的比例」≈ p。世界与引擎完全正交
//! （衰减模式只影响引擎），因此同一 p 下三种模式共用同一个 dormancy_rate、
//! 同一个世界轨迹，构成配对设计。
//!
//! 两个坑（已避开）：
//!   1. 账户编号一律取 world.accounts().len()，绝不用 alive_count()；
//!   2. Sleeper / Promoted / 诚实三套累加器完全分开（本实验无攻击者，
//!      所以 sleeper=0、promoted=0，但仍显式按口径统计，不用 is_attacker 二分）。
//!
//! 用法：
//!   d3_offline            全网格
//!   d3_offline smoke      单格冒烟（p=50%, seed=11, 三模式）
//!   d3_offline probe      只打衰减因子表

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, Life, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

const N_INIT: usize = 300;
const STEPS: u64 = 400;
const SEEDS: [u64; 3] = [11, 23, 37];
const PS: [f64; 4] = [0.30, 0.50, 0.70, 0.90];
const ROUND_LEN: u64 = 20; // 20 轮，tick = 20h
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const SLEEP: (u64, u64) = (216, 720);
const RETURN_RATE: f64 = 0.10;
const TOP_FRAC: f64 = 0.10;

// ───────────────────────── 三种衰减模式 ─────────────────────────

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

fn ecfg(m: Mode) -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: HALF_LIFE,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: m.enabled,
        decay_floor: m.floor,
    }
}

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

/// 每个账户一条独立的抽样随机流（不共用同一条流，避免所有观察者抽同一批人）。
fn engine_seed(i: usize) -> u64 {
    0x5EED_1234_ABCD_0001 ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn sim_cfg(dorm: f64, seed: u64) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS as usize,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dorm,
        sleep_hours: SLEEP,
        return_rate: RETURN_RATE,
        enable_rhythm: false,
        attack_kind: AttackKind::None,
        attack_ratio: 0.0,
        ..Default::default()
    }
}

// ───────────────────────── 世界层：标定 ─────────────────────────

#[derive(Clone, Copy, Default)]
struct WorldStat {
    p_alive_off: f64, // 存活账户里不在线的比例（= 任务口径的 p）
    p_all_off: f64,   // 全部账户里不在线的比例（含已离开）
    accounts: usize,
    alive: usize,
    online: usize,
    left: usize,
    dormant: usize,
}

fn world_stat(dorm: f64, seed: u64) -> WorldStat {
    let mut w = World::new(sim_cfg(dorm, seed));
    w.run();
    let t = STEPS;
    let n = w.accounts().len();
    let alive: Vec<usize> = (0..n).filter(|&i| w.accounts()[i].is_alive()).collect();
    let online = alive.iter().filter(|&&i| w.was_online_at(i, t)).count();
    let dormant = alive
        .iter()
        .filter(|&&i| matches!(w.accounts()[i].life, Life::Dormant { .. }))
        .count();
    let left = (0..n).filter(|&i| w.accounts()[i].is_left()).count();
    let p_alive_off = if alive.is_empty() {
        f64::NAN
    } else {
        (alive.len() - online) as f64 / alive.len() as f64
    };
    let p_all_off = if n == 0 { f64::NAN } else { (n - online) as f64 / n as f64 };
    WorldStat {
        p_alive_off,
        p_all_off,
        accounts: n,
        alive: alive.len(),
        online,
        left,
        dormant,
    }
}

/// 对数网格粗扫 + 对数二分细化。目标：给定 seed 集合的 p 均值最接近 target。
fn calibrate(target: f64, seeds: &[u64]) -> (f64, Vec<WorldStat>) {
    let n_pts = 15usize;
    let l0 = 1e-5f64.ln();
    let l1 = 0.20f64.ln();
    let eval = |d: f64| -> (f64, Vec<WorldStat>) {
        let mut sts = vec![WorldStat::default(); seeds.len()];
        let mut acc = 0.0;
        let mut c = 0usize;
        for (j, &s) in seeds.iter().enumerate() {
            let st = world_stat(d, s);
            if st.p_alive_off.is_finite() {
                acc += st.p_alive_off;
                c += 1;
            }
            sts[j] = st;
        }
        (if c > 0 { acc / c as f64 } else { f64::NAN }, sts)
    };

    let mut xs: Vec<(f64, f64)> = Vec::with_capacity(n_pts + 1);
    let mut best: (f64, f64, Vec<WorldStat>) = (0.0, f64::INFINITY, Vec::new());
    for i in 0..=n_pts {
        let d = (l0 + (l1 - l0) * i as f64 / n_pts as f64).exp();
        let (p, sts) = eval(d);
        if p.is_finite() && (p - target).abs() < (best.1 - target).abs() {
            best = (d, p, sts);
        }
        xs.push((d, p));
    }
    if !best.1.is_finite() || best.2.is_empty() {
        return (best.0, vec![WorldStat::default(); seeds.len()]);
    }
    let idx = xs.iter().position(|&(d, _)| d == best.0).unwrap_or(0);
    let mut lo = if idx > 0 { xs[idx - 1].0 } else { xs[0].0 * 0.5 };
    let mut hi = if idx + 1 < xs.len() { xs[idx + 1].0 } else { *xs.last().map(|x| &x.0).unwrap() * 2.0 };
    for _ in 0..8 {
        let mid = (lo * hi).sqrt();
        let (p, sts) = eval(mid);
        if p.is_finite() && (p - target).abs() < (best.1 - target).abs() {
            best = (mid, p, sts);
        }
        if p.is_finite() && p < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (best.0, best.2)
}

// ───────────────────────── 统计小工具 ─────────────────────────

fn mean(v: &[f64]) -> f64 {
    let f: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if f.is_empty() { f64::NAN } else { f.iter().sum::<f64>() / f.len() as f64 }
}

fn mean_sd(v: &[f64]) -> (f64, f64) {
    let f: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    let n = f.len() as f64;
    if f.is_empty() { return (f64::NAN, f64::NAN); }
    let m = f.iter().sum::<f64>() / n;
    if f.len() < 2 { return (m, 0.0); }
    let var = f.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0);
    (m, var.sqrt())
}

fn median_u64(v: &[u64]) -> f64 {
    if v.is_empty() { return f64::NAN; }
    let mut x = v.to_vec();
    x.sort_unstable();
    let n = x.len();
    if n % 2 == 1 { x[n / 2] as f64 } else { 0.5 * (x[n / 2 - 1] as f64 + x[n / 2] as f64) }
}

/// 基尼系数（总体口径）：G = 2·Σ i·x_(i) / (n·Σx) − (n+1)/n，x 升序、i 从 1 起。
fn gini(v: &[f64]) -> f64 {
    let mut x: Vec<f64> = v.iter().copied().filter(|a| a.is_finite()).collect();
    let n = x.len();
    if n < 2 { return f64::NAN; }
    let tot: f64 = x.iter().sum();
    if tot <= 0.0 { return f64::NAN; }
    x.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut acc = 0.0f64;
    for (i, &xi) in x.iter().enumerate() {
        acc += (i as f64 + 1.0) * xi;
    }
    2.0 * acc / (n as f64 * tot) - (n as f64 + 1.0) / n as f64
}

/// 平均秩（并列取平均），升序。
fn ranks(v: &[f64]) -> Vec<f64> {
    let n = v.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j + 1 < n && (v[idx[j + 1]] - v[idx[i]]).abs() <= 1e-12 {
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

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 3 { return f64::NAN; }
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

/// Spearman ρ = 平均秩上的 Pearson。
fn spearman(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 3 { return f64::NAN; }
    pearson(&ranks(&x[..n]), &ranks(&y[..n]))
}

// ───────────────────────── 单次运行 ─────────────────────────

#[derive(Default, Clone)]
struct Row {
    mode: &'static str,
    p_target: f64,
    seed: u64,
    dorm: f64,
    // 世界
    p_meas: f64,
    p_all: f64,
    accounts: usize,
    alive: usize,
    online: usize,
    left: usize,
    dormant: usize,
    // 观察者面板
    n_obs: usize,
    zero_base: usize,
    // 份额（相对观察者的完整信任分布，总和 1）
    left_share: f64,
    left_pers: f64,
    left_grp: f64,
    dorm_share: f64,
    dorm_pers: f64,
    dorm_grp: f64,
    online_share: f64,
    gini: f64,
    dist_nnz: f64,
    // 排序质量
    rho_all: f64,
    rho_on: f64,
    // 可达性
    reach_all: f64,
    reach_on: f64,
    reach_dorm: f64,
    topk: usize,
    n_hon_all: usize,
    n_hon_on: usize,
    // 观点年龄 / 来源
    age_all: f64,
    age_left: f64,
    age_dorm: f64,
    age_on: f64,
    n_views: f64,
    n_cached: f64,
    eff_src: f64,
    /// 来源权重里「年龄 > 一个半衰期」的占比（Σ share×factor 口径）。
    /// 这是三种模式差别最直接的那个量：std 把它压到很小，floor 砍一半，off 完全不动。
    w_old: f64,
    hit_rate: f64,
    // 配对校验：个人项分解在三种模式间必须逐位一致
    personal_sum: f64,
}

fn run_cell(m: Mode, p_target: f64, seed: u64, dorm: f64) -> Row {
    let t_end = STEPS;
    let rounds = (STEPS / ROUND_LEN) as usize;

    let mut world = World::new(sim_cfg(dorm, seed));
    let mut nodes: Vec<Option<Engine>> = Vec::new();
    let mut interned = 0usize;

    for r in 0..=rounds {
        let t = r as u64 * ROUND_LEN;
        let n = world.accounts().len();
        if nodes.len() < n {
            nodes.resize_with(n, || None);
        }
        let alive: Vec<bool> = (0..n).map(|i| world.accounts()[i].is_alive()).collect();
        let online: Vec<bool> = (0..n).map(|i| alive[i] && world.was_online_at(i, t)).collect();

        // 1. 引擎：存活账户才有 runtime（编号只增不减）
        for i in 0..n {
            if alive[i] && nodes[i].is_none() {
                let mut e = Engine::with_seed(ecfg(m), engine_seed(i));
                for j in 0..n {
                    e.intern(key(j));
                }
                nodes[i] = Some(e);
            }
        }
        if n > interned {
            for i in 0..n {
                if let Some(e) = nodes[i].as_mut() {
                    for j in interned..n {
                        e.intern(key(j));
                    }
                }
            }
            interned = n;
        }

        // 2. 正分清单按轮同步（世界在 t 时刻的真实状态）
        let ratings: Vec<Vec<(usize, f32)>> =
            (0..n).map(|i| world.accounts()[i].ratings.clone()).collect();
        for i in 0..n {
            if !alive[i] {
                continue;
            }
            if let Some(e) = nodes[i].as_mut() {
                for &(tgt, sc) in &ratings[i] {
                    e.rate(tgt as u32, sc, t);
                }
            }
        }

        // 3. 观点交换：在线者先发布，再抽样拉取；离线者既不发布也不抽
        let mut views: Vec<Option<CompressedView>> = vec![None; n];
        for i in 0..n {
            if online[i] {
                if let Some(e) = nodes[i].as_ref() {
                    views[i] = Some(e.export_view(t));
                }
            }
        }
        for i in 0..n {
            if !online[i] {
                continue;
            }
            let reqs = { nodes[i].as_mut().unwrap().tick(t, SAMPLES) };
            for req in reqs {
                let src = req.idx as usize;
                match views.get(src) {
                    Some(Some(v)) => {
                        let v = v.clone();
                        nodes[i].as_mut().unwrap().on_view(req.idx, v, t, t);
                    }
                    _ => nodes[i].as_mut().unwrap().on_view_failed(req.idx, t),
                }
            }
        }

        if r < rounds {
            world.run_steps(ROUND_LEN as usize);
        }
    }

    // ── 终局世界状态 ──
    let n = world.accounts().len();
    let accs = world.accounts();
    let alive: Vec<usize> = (0..n).filter(|&i| accs[i].is_alive()).collect();
    let left: Vec<usize> = (0..n).filter(|&i| accs[i].is_left()).collect();
    let online: Vec<bool> = (0..n).map(|i| accs[i].is_alive() && world.was_online_at(i, t_end)).collect();
    let alive_off: Vec<usize> = alive.iter().copied().filter(|&i| !online[i]).collect();
    let alive_on: Vec<usize> = alive.iter().copied().filter(|&i| online[i]).collect();

    // 口径：三类账户分开，绝不用 is_attacker 二分
    let is_slp: Vec<bool> = (0..n).map(|i| matches!(accs[i].kind, Kind::Sleeper { .. })).collect();
    let is_prom: Vec<bool> = (0..n).map(|i| matches!(accs[i].kind, Kind::Promoted)).collect();
    let is_hon: Vec<bool> = (0..n).map(|i| !is_slp[i] && !is_prom[i]).collect();
    let hon_alive: Vec<usize> = alive.iter().copied().filter(|&i| is_hon[i]).collect();
    let hon_on: Vec<usize> = alive_on.iter().copied().filter(|&i| is_hon[i]).collect();
    let hon_dorm: Vec<usize> = alive_off.iter().copied().filter(|&i| is_hon[i]).collect();

    // 观察者 = 存活 ∧ 诚实 ∧ 结束时在线 ∧ 休眠/离开不算
    let obs: Vec<usize> = hon_on.clone();

    let p_meas = if alive.is_empty() {
        f64::NAN
    } else {
        alive_off.len() as f64 / alive.len() as f64
    };
    let p_all = if n == 0 { f64::NAN } else { (n - alive_on.len()) as f64 / n as f64 };

    let k_top = ((n as f64 * TOP_FRAC).round() as usize).max(1);

    let mut left_share_v = Vec::new();
    let mut left_pers_v = Vec::new();
    let mut left_grp_v = Vec::new();
    let mut dorm_share_v = Vec::new();
    let mut dorm_pers_v = Vec::new();
    let mut dorm_grp_v = Vec::new();
    let mut on_share_v = Vec::new();
    let mut gini_v = Vec::new();
    let mut nnz_v = Vec::new();
    let mut rho_all_v = Vec::new();
    let mut rho_on_v = Vec::new();
    let mut age_all_v: Vec<u64> = Vec::new();
    let mut age_left_v: Vec<u64> = Vec::new();
    let mut age_dorm_v: Vec<u64> = Vec::new();
    let mut age_on_v: Vec<u64> = Vec::new();
    let mut nviews_v = Vec::new();
    let mut ncached_v = Vec::new();
    let mut eff_v = Vec::new();
    let mut w_old_v = Vec::new();
    let mut hit_v = Vec::new();
    let mut pers_sum_v = Vec::new();
    let mut zero_base = 0usize;
    let mut reach_hit = vec![0usize; n];
    let mut reach_n = vec![0usize; n];

    for &o in &obs {
        let e = match nodes[o].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let dist = e.trust_distribution(t_end);
        let mut v = vec![0.0f64; n];
        let mut raw = vec![0.0f64; n];
        let mut pers = vec![0.0f64; n];
        let mut tot_raw = 0.0f64;
        for i in 0..n {
            let q = e.query(i as u32, t_end) as f64;
            raw[i] = q;
            tot_raw += q;
            pers[i] = 0.3 * e.share_of(i as u32).unwrap_or(0.0) as f64;
        }
        if tot_raw <= 1e-12 || dist.is_empty() {
            zero_base += 1;
            continue;
        }
        for &(i, x) in &dist {
            if (i as usize) < n {
                v[i as usize] = x as f64;
            }
        }

        // 份额（分布层面，总和 1）
        let sum_of = |set: &[usize], vv: &[f64]| -> f64 { set.iter().map(|&i| vv[i]).sum() };
        left_share_v.push(sum_of(&left, &v));
        dorm_share_v.push(sum_of(&alive_off, &v));
        on_share_v.push(sum_of(&alive_on, &v));

        // 分解：v = β·personal / tot + (1−β)·group / tot
        let lp = sum_of(&left, &pers) / tot_raw;
        let dp = sum_of(&alive_off, &pers) / tot_raw;
        left_pers_v.push(lp);
        left_grp_v.push(sum_of(&left, &v) - lp);
        dorm_pers_v.push(dp);
        dorm_grp_v.push(sum_of(&alive_off, &v) - dp);
        pers_sum_v.push((sum_of(&hon_alive, &pers) + sum_of(&left, &pers)) / tot_raw);

        gini_v.push(gini(&v));
        nnz_v.push(dist.len() as f64);

        // 排序：信任 vs 真实 quality
        let qa: Vec<f64> = hon_alive.iter().map(|&i| accs[i].quality as f64).collect();
        let va: Vec<f64> = hon_alive.iter().map(|&i| v[i]).collect();
        rho_all_v.push(spearman(&va, &qa));
        let qo: Vec<f64> = hon_on.iter().map(|&i| accs[i].quality as f64).collect();
        let vo: Vec<f64> = hon_on.iter().map(|&i| v[i]).collect();
        rho_on_v.push(spearman(&vo, &qo));

        // 可达性：该观察者 top 10%（按份额降序，编号升序破平）
        let mut ord: Vec<usize> = (0..n).collect();
        ord.sort_by(|&a, &b| {
            v[b].partial_cmp(&v[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(&b))
        });
        for &i in ord.iter().take(k_top) {
            reach_hit[i] += 1;
        }
        for &i in &hon_alive {
            reach_n[i] += 1;
        }

        // 来源侧：观点年龄、缓存数、有效来源数
        let mut wsum = 0.0f64;
        let mut w_old_sum = 0.0f64;
        let mut n_cached = 0usize;
        let mut ws: Vec<f64> = Vec::new();
        for i in 0..n {
            if let Some(age) = e.view_age(i as u32, t_end) {
                n_cached += 1;
                age_all_v.push(age);
                if left.contains(&i) {
                    age_left_v.push(age);
                } else if online[i] {
                    age_on_v.push(age);
                } else {
                    age_dorm_v.push(age);
                }
                if let Some(sh) = e.share_of(i as u32) {
                    let w = sh as f64 * e.decay_factor(age) as f64;
                    if w > 0.0 {
                        wsum += w;
                        if age > HALF_LIFE {
                            w_old_sum += w;
                        }
                        ws.push(w);
                    }
                }
            }
        }
        if wsum > 0.0 {
            let s2: f64 = ws.iter().map(|w| (w / wsum) * (w / wsum)).sum();
            eff_v.push(1.0 / s2);
            w_old_v.push(w_old_sum / wsum);
        }
        ncached_v.push(n_cached as f64);
        let (rq, rh, nv, _) = e.stats();
        nviews_v.push(nv as f64);
        if rq > 0 {
            hit_v.push(rh as f64 / rq as f64);
        }
    }

    let reach = |set: &[usize]| -> f64 {
        let mut hit = 0usize;
        let mut c = 0usize;
        for &i in set {
            if reach_n[i] > 0 {
                c += 1;
                if reach_hit[i] > 0 {
                    hit += 1;
                }
            }
        }
        if c == 0 { f64::NAN } else { hit as f64 / c as f64 }
    };

    Row {
        mode: m.name,
        p_target,
        seed,
        dorm,
        p_meas,
        p_all,
        accounts: n,
        alive: alive.len(),
        online: alive_on.len(),
        left: left.len(),
        dormant: alive_off.len(),
        n_obs: obs.len(),
        zero_base,
        left_share: mean(&left_share_v),
        left_pers: mean(&left_pers_v),
        left_grp: mean(&left_grp_v),
        dorm_share: mean(&dorm_share_v),
        dorm_pers: mean(&dorm_pers_v),
        dorm_grp: mean(&dorm_grp_v),
        online_share: mean(&on_share_v),
        gini: mean(&gini_v),
        dist_nnz: mean(&nnz_v),
        rho_all: mean(&rho_all_v),
        rho_on: mean(&rho_on_v),
        reach_all: reach(&hon_alive),
        reach_on: reach(&hon_on),
        reach_dorm: reach(&hon_dorm),
        topk: k_top,
        n_hon_all: hon_alive.len(),
        n_hon_on: hon_on.len(),
        age_all: median_u64(&age_all_v),
        age_left: median_u64(&age_left_v),
        age_dorm: median_u64(&age_dorm_v),
        age_on: median_u64(&age_on_v),
        n_views: mean(&nviews_v),
        n_cached: mean(&ncached_v),
        eff_src: mean(&eff_v),
        w_old: mean(&w_old_v),
        hit_rate: mean(&hit_v),
        personal_sum: mean(&pers_sum_v),
    }
}

// ───────────────────────── 输出 ─────────────────────────

fn f6(x: f64) -> String {
    if x.is_finite() { format!("{:.6}", x) } else { "nan".to_string() }
}

const COLS: [&str; 31] = [
    "p_meas", "p_all", "accounts", "alive", "online", "left", "dormant", "n_obs", "zero_base",
    "left_share", "left_pers", "left_grp", "dorm_share", "dorm_pers", "dorm_grp", "online_share",
    "gini", "dist_nnz", "rho_all", "rho_on", "reach_all", "reach_on", "reach_dorm", "age_all",
    "age_left", "age_dorm", "age_on", "n_cached", "eff_src", "w_old", "hit_rate",
];

impl Row {
    fn vals(&self) -> Vec<String> {
        vec![
            f6(self.p_meas), f6(self.p_all), self.accounts.to_string(), self.alive.to_string(),
            self.online.to_string(), self.left.to_string(), self.dormant.to_string(),
            self.n_obs.to_string(), self.zero_base.to_string(),
            f6(self.left_share), f6(self.left_pers), f6(self.left_grp),
            f6(self.dorm_share), f6(self.dorm_pers), f6(self.dorm_grp), f6(self.online_share),
            f6(self.gini), f6(self.dist_nnz), f6(self.rho_all), f6(self.rho_on),
            f6(self.reach_all), f6(self.reach_on), f6(self.reach_dorm), f6(self.age_all),
            f6(self.age_left), f6(self.age_dorm), f6(self.age_on), f6(self.n_cached),
            f6(self.eff_src), f6(self.w_old), f6(self.hit_rate),
        ]
    }
}

fn decay_table() {
    println!("\n== DECAY 三种衰减模式下 Engine::decay_factor(dt) 的实测值 ==");
    println!("DECAY,dt_h,std,floor,off");
    for dt in [0u64, 6, 24, 72, 144, 216, 288, 360, 400, 720, 1440] {
        let fs: Vec<String> = MODES
            .iter()
            .map(|&m| {
                let e = Engine::new(ecfg(m));
                f6(e.decay_factor(dt) as f64)
            })
            .collect();
        println!("DECAY,{},{},{},{}", dt, fs[0], fs[1], fs[2]);
    }
}


/// 只在世界层做的队伍画像：t=400 时「在线 / 休眠 / 已离开」三组分别是谁。
/// 用来解释 p=90% 时在线集合的构成（是不是只剩新加入的账户）。
fn who() {
    println!("\n== WHO t={} 时三组账户的构成（年龄 = t − joined_at）==", STEPS);
    println!("WHO,p_target,seed,group,n,med_age_h,mean_quality,mean_raters,frac_rated");
    for &p in &PS {
        let (rate, _) = calibrate(p, &SEEDS);
        for &seed in SEEDS.iter() {
            let mut w = World::new(sim_cfg(rate, seed));
            w.run();
            let n = w.accounts().len();
            let mut raters = vec![0usize; n];
            for &(_, tgt, _) in w.ratings().iter() {
                if tgt < n {
                    raters[tgt] += 1;
                }
            }
            let mut groups: Vec<(&str, Vec<usize>)> = vec![("online", vec![]), ("dormant", vec![]), ("left", vec![])];
            for i in 0..n {
                let a = &w.accounts()[i];
                if a.is_left() {
                    groups[2].1.push(i);
                } else if w.was_online_at(i, STEPS) {
                    groups[0].1.push(i);
                } else {
                    groups[1].1.push(i);
                }
            }
            for (name, set) in groups {
                if set.is_empty() {
                    println!("WHO,{:.2},{},{},0,nan,nan,nan,nan", p, seed, name);
                    continue;
                }
                let mut ages: Vec<u64> = set.iter().map(|&i| STEPS - w.accounts()[i].joined_at).collect();
                ages.sort_unstable();
                let med = ages[ages.len() / 2] as f64;
                let q = set.iter().map(|&i| w.accounts()[i].quality as f64).sum::<f64>() / set.len() as f64;
                let r = set.iter().map(|&i| raters[i] as f64).sum::<f64>() / set.len() as f64;
                let fr = set.iter().filter(|&&i| raters[i] > 0).count() as f64 / set.len() as f64;
                println!("WHO,{:.2},{},{},{},{:.0},{:.4},{:.2},{:.4}", p, seed, name, set.len(), med, q, r, fr);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let smoke = args.iter().any(|a| a == "smoke");
    let probe = args.iter().any(|a| a == "probe");
    let t0 = std::time::Instant::now();

    println!("# D3 · 长期离线账户对网络的影响（三种衰减模式）");
    println!("# n_initial={} steps={} seeds={:?} join_rate=0.05 leave_rate=0.001 return_rate={}",
        N_INIT, STEPS, SEEDS, RETURN_RATE);
    println!("# sleep_hours={:?} enable_rhythm=false attack_kind=None（无攻击者）", SLEEP);
    println!("# 引擎 beta=0.3 half_life={} buckets=2048 hashers=5 agg=Mean samples={} 轮长={}h 轮数={}",
        HALF_LIFE, SAMPLES, ROUND_LEN, STEPS / ROUND_LEN);
    println!("# 口径：诚实 = 非 Sleeper 且非 Promoted；本实验 sleeper=0 promoted=0（仍分开统计）");
    println!("# p = 存活账户里不在线的比例（t={}）；p_all = 全部账户里不在线的比例（含已离开）", STEPS);

    decay_table();
    if probe {
        return;
    }

    if args.iter().any(|a| a == "who") {
        who();
        return;
    }

    // 地形扫描：dormancy_rate → 实测 p（标定可行性检查）
    if args.iter().any(|a| a == "land") {
        println!("\n== LAND dormancy_rate → 实测 p 地貌（3 seed 均值）==");
        println!("LAND,dormancy,p_mean,alive_mean,online_mean,left_mean,accounts_mean");
        for k in 0..=24 {
            let d = (1e-5f64.ln() + (0.20f64.ln() - 1e-5f64.ln()) * k as f64 / 24.0).exp();
            let sts: Vec<WorldStat> = SEEDS.iter().map(|&s| world_stat(d, s)).collect();
            println!(
                "LAND,{:.8},{},{:.1},{:.1},{:.1},{:.1}",
                d,
                f6(mean(&sts.iter().map(|s| s.p_alive_off).collect::<Vec<_>>())),
                mean(&sts.iter().map(|s| s.alive as f64).collect::<Vec<_>>()),
                mean(&sts.iter().map(|s| s.online as f64).collect::<Vec<_>>()),
                mean(&sts.iter().map(|s| s.left as f64).collect::<Vec<_>>()),
                mean(&sts.iter().map(|s| s.accounts as f64).collect::<Vec<_>>())
            );
        }
        return;
    }

    let ps: Vec<f64> = if smoke { vec![0.50] } else { PS.to_vec() };
    let seeds: Vec<u64> = if smoke { vec![SEEDS[0]] } else { SEEDS.to_vec() };

    // ── 标定 ──
    println!("\n== CAL 标定（每个 p 一个 dormancy_rate，3 seed 共用；世界与引擎正交）==");
    println!("CAL,p_target,dormancy,seed,p_meas,p_all,accounts,alive,online,left,dormant");
    let mut rates: Vec<(f64, f64, Vec<WorldStat>)> = Vec::new();
    for &p in &ps {
        let (rate, sts) = calibrate(p, &SEEDS);
        for (j, st) in sts.iter().enumerate() {
            println!(
                "CAL,{:.2},{:.8},{},{},{},{},{},{},{},{}",
                p, rate, SEEDS[j], f6(st.p_alive_off), f6(st.p_all_off), st.accounts, st.alive,
                st.online, st.left, st.dormant
            );
        }
        let pm = mean(&sts.iter().map(|s| s.p_alive_off).collect::<Vec<_>>());
        let pa = mean(&sts.iter().map(|s| s.p_all_off).collect::<Vec<_>>());
        println!(
            "CALSUM,{:.2},{:.8},{},{},{:.1},{:.1},{:.1},{:.1},{:.1}",
            p, rate, f6(pm), f6(pa),
            mean(&sts.iter().map(|s| s.accounts as f64).collect::<Vec<_>>()),
            mean(&sts.iter().map(|s| s.alive as f64).collect::<Vec<_>>()),
            mean(&sts.iter().map(|s| s.online as f64).collect::<Vec<_>>()),
            mean(&sts.iter().map(|s| s.left as f64).collect::<Vec<_>>()),
            mean(&sts.iter().map(|s| s.dormant as f64).collect::<Vec<_>>()),
        );
        rates.push((p, rate, sts));
        eprintln!("[cal p={:.0}%] rate={:.8} p={:.4} ({:.0}s)", p * 100.0, rate, pm, t0.elapsed().as_secs_f64());
    }

    // ── 主网格 ──
    println!("\n== MAIN 主表（每格 3 seed 原始值）==");
    println!("MAIN,mode,p_target,seed,dormancy,{}", COLS.join(","));
    let mut rows: Vec<Row> = Vec::new();
    for &m in &MODES {
        for &p in &ps {
            let (_, rate, _) = *rates.iter().find(|(pp, _, _)| *pp == p).unwrap();
            for &seed in &seeds {
                let r = run_cell(m, p, seed, rate);
                let mut v = r.vals();
                let mut head = vec![r.mode.to_string(), format!("{:.2}", r.p_target), r.seed.to_string(), f6(r.dorm)];
                head.extend(v.drain(..));
                println!("MAIN,{}", head.join(","));
                eprintln!("[{} p={:.0}% seed={}] obs={} left_s={:.3} dorm_s={:.3} rho={:.3} ({:.0}s)",
                    m.name, p * 100.0, seed, r.n_obs, r.left_share, r.dorm_share, r.rho_all,
                    t0.elapsed().as_secs_f64());
                rows.push(r);
            }
        }
    }

    // ── POOL：观察者与目标池规模（可达性的分母就是它）──
    println!("\n== POOL 观察者 / 目标池规模 ==");
    println!("POOL,mode,p_target,seed,k_top,n_hon_all,n_hon_on,n_obs,zero_base,dist_nnz,n_views");
    for r in rows.iter() {
        println!(
            "POOL,{},{:.2},{},{},{},{},{},{},{},{}",
            r.mode, r.p_target, r.seed, r.topk, r.n_hon_all, r.n_hon_on, r.n_obs, r.zero_base,
            f6(r.dist_nnz), f6(r.n_views)
        );
    }

    // ── AGG：mode × p，3 seed 均值±样本标准差 ──
    println!("\n== AGG mode × p（3 seed 均值±样本标准差）==");
    println!("AGG,mode,p_target,p_meas,{}", COLS[1..].iter().map(|c| format!("{}{}", c, "")).collect::<Vec<_>>().join(","));
    for &m in &MODES {
        for &p in &ps {
            let sub: Vec<&Row> = rows.iter().filter(|r| r.mode == m.name && r.p_target == p).collect();
            let pick = |f: &dyn Fn(&Row) -> f64| -> Vec<f64> { sub.iter().map(|r| f(r)).collect() };
            let g = |f: &dyn Fn(&Row) -> f64| -> String {
                let (a, b) = mean_sd(&pick(f));
                format!("{}±{}", f6(a), f6(b))
            };
            let mut out = vec![m.name.to_string(), format!("{:.2}", p)];
            out.push(g(&|r| r.p_meas));
            out.push(g(&|r| r.p_all));
            out.push(format!("{:.1}", mean(&pick(&|r| r.accounts as f64))));
            out.push(format!("{:.1}", mean(&pick(&|r| r.alive as f64))));
            out.push(format!("{:.1}", mean(&pick(&|r| r.online as f64))));
            out.push(format!("{:.1}", mean(&pick(&|r| r.left as f64))));
            out.push(format!("{:.1}", mean(&pick(&|r| r.dormant as f64))));
            out.push(format!("{:.1}", mean(&pick(&|r| r.n_obs as f64))));
            out.push(format!("{:.0}", mean(&pick(&|r| r.zero_base as f64))));
            out.push(g(&|r| r.left_share));
            out.push(g(&|r| r.left_pers));
            out.push(g(&|r| r.left_grp));
            out.push(g(&|r| r.dorm_share));
            out.push(g(&|r| r.dorm_pers));
            out.push(g(&|r| r.dorm_grp));
            out.push(g(&|r| r.online_share));
            out.push(g(&|r| r.gini));
            out.push(g(&|r| r.dist_nnz));
            out.push(g(&|r| r.rho_all));
            out.push(g(&|r| r.rho_on));
            out.push(g(&|r| r.reach_all));
            out.push(g(&|r| r.reach_on));
            out.push(g(&|r| r.reach_dorm));
            out.push(g(&|r| r.age_all));
            out.push(g(&|r| r.age_left));
            out.push(g(&|r| r.age_dorm));
            out.push(g(&|r| r.age_on));
            out.push(g(&|r| r.n_cached));
            out.push(g(&|r| r.eff_src));
            out.push(g(&|r| r.w_old));
            out.push(g(&|r| r.hit_rate));
            println!("AGG,{}", out.join(","));
        }
    }

    // ── 横截面：每个 p 上三种模式并排（便于直接看衰减的净效应）──
    println!("\n== XSEC 每个 p 上三种模式的对照（3 seed 均值）==");
    println!("XSEC,p,metric,std,floor,off");
    for &p in &ps {
        for (mi, label) in ["left_share", "left_pers", "left_grp", "dorm_share", "dorm_pers", "dorm_grp",
                            "online_share", "gini", "rho_all", "rho_on", "reach_all", "reach_on",
                            "age_all", "age_left", "age_dorm", "n_cached", "eff_src", "w_old", "hit_rate"].iter().enumerate() {
            let get = |r: &Row| -> f64 {
                match mi {
                    0 => r.left_share, 1 => r.left_pers, 2 => r.left_grp,
                    3 => r.dorm_share, 4 => r.dorm_pers, 5 => r.dorm_grp,
                    6 => r.online_share, 7 => r.gini, 8 => r.rho_all, 9 => r.rho_on,
                    10 => r.reach_all, 11 => r.reach_on, 12 => r.age_all, 13 => r.age_left,
                    14 => r.age_dorm, 15 => r.n_cached, 16 => r.eff_src, 17 => r.w_old, _ => r.hit_rate,
                }
            };
            let xs: Vec<String> = MODES
                .iter()
                .map(|&m| {
                    let sub: Vec<f64> = rows.iter().filter(|r| r.mode == m.name && r.p_target == p).map(get).collect();
                    f6(mean(&sub))
                })
                .collect();
            println!("XSEC,{:.2},{},{},{},{}", p, label, xs[0], xs[1], xs[2]);
        }
    }

    // ── 配对校验：个人项在三种模式间必须逐位一致（衰减只作用于群体项）──
    println!("\n== CHECK 个人项分解的模式不变性（同一 p,seed 的 left_pers / dorm_pers 最大值差）==");
    println!("CHECK,p_target,seed,left_pers_maxdiff,dorm_pers_maxdiff,personal_sum_maxdiff");
    for &p in &ps {
        for &seed in &seeds {
            let sub: Vec<&Row> = rows.iter().filter(|r| r.p_target == p && r.seed == seed).collect();
            if sub.len() < 3 {
                continue;
            }
            let d = |f: &dyn Fn(&Row) -> f64| -> f64 {
                let vs: Vec<f64> = sub.iter().map(|r| f(r)).collect();
                vs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                    - vs.iter().cloned().fold(f64::INFINITY, f64::min)
            };
            println!(
                "CHECK,{:.2},{},{},{},{}",
                p, seed, f6(d(&|r| r.left_pers)), f6(d(&|r| r.dorm_pers)), f6(d(&|r| r.personal_sum))
            );
        }
    }

    println!("\n# total {:.0}s", t0.elapsed().as_secs_f64());
}
