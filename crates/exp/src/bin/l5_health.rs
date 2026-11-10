//! L5：潜伏攻击对网络整体可用性的影响（网络层面健康度）
//!
//! 真多 runtime：每个账户一个 tm_engine::Engine，模板见 crates/exp/src/bin/grid.rs。
//! 离线节点不 tick、不收观点，观点年龄真实增长（时间衰减生效）；回归节点做快速刷新。
//!
//! 固化：n_initial=300, steps=400, join_rate=0.05, leave_rate=0.001, seed in {11,23,37}
//! 扫描：离线比例 p in {30,50,70,90}%  ×  潜伏者数量 s in {0(对照),1,5,20}
//!
//! 网络层面测量（每个观察者一份 trust_distribution）：
//!   1. 排序相关性：信任分布排序 vs 账户真实 quality 的 Spearman
//!   2. 观察者胜率：诚实账户人均声望 > 潜伏者人均声望 的观察者比例
//!   3. 基尼系数：信任分布的集中度
//!   4. 可达性：诚实账户被至少一个观察者排进前 10% 的比例
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
const PS: [f64; 4] = [0.30, 0.50, 0.70, 0.90];
const SS: [usize; 4] = [0, 1, 5, 20];

/// 潜伏者突变时刻：总步数的 1/5（与 grid.rs 一致）
const NORMAL_UNTIL: u64 = (STEPS / 5) as u64;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

// ════════════════════════════ 配置 ════════════════════════════

/// 网格底座。enable_rhythm=false：在线与否只由休眠决定，不被昼夜节律混淆。
fn cfg_with(seed: u64, dormancy: f64, sleep: (u64, u64)) -> SimConfig {
    SimConfig {
        n_initial: N,
        steps: STEPS,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dormancy,
        sleep_hours: sleep,
        return_rate: 0.05,
        enable_rhythm: false,
        ..Default::default()
    }
}

/// 离线比例：存活账户里，在 t = STEPS 时刻不在线的占比。
fn offline_frac(seed: u64, dormancy: f64, sleep: (u64, u64)) -> (f64, usize, usize) {
    let mut w = World::new(cfg_with(seed, dormancy, sleep));
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

/// 二分找能命中目标离线比例 p 的 dormancy_rate。
fn calibrate(target: f64, seed: u64, sleep: (u64, u64)) -> (f64, f64) {
    let (mut lo, mut hi) = (1e-6f64, 0.05f64);
    let mut best = (lo, offline_frac(seed, lo, sleep).0);
    for _ in 0..14 {
        let mid = 0.5 * (lo + hi);
        let f = offline_frac(seed, mid, sleep).0;
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

// ════════════════════════════ 统计工具 ════════════════════════════

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

// ════════════════════════════ 一次完整运行 ════════════════════════════

#[derive(Clone, Debug, Default)]
struct Run {
    seed: u64,
    p_target: f64,
    p_actual: f64,
    dormancy: f64,
    s: usize,
    n_accounts: usize,
    n_alive: usize,
    n_obs: usize,
    n_obs_online: usize,
    n_sleepers: usize,
    n_slp_alive: usize,
    slp_rated: usize,
    target_idx: i64,
    sp: f64,
    sp_alive: f64,
    sp_personal: f64,
    sp_n: usize,
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
    sleeper_mass_all: f64,
    sleeper_visible: f64,
    dead_mass: f64,
    win_all: f64,
    rel_all: f64,
    rel: f64,
    target_mass: f64,
    prom_mass: f64,
    prom_pct: f64,
    prom_seen: f64,
    n_prom: usize,
    n_prom_alive: usize,
    prom_rated: usize,
    prom_recv: usize,
    prom_q_pct: f64,
    prom_quality: f64,
    best_sleeper_pct: f64,
    sum_err: f64,
    mean_views: f64,
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn dist_share(dist: &[(u32, f32)], i: usize) -> f64 {
    dist.iter()
        .find(|(idx, _)| *idx as usize == i)
        .map(|(_, v)| *v as f64)
        .unwrap_or(0.0)
}

fn run_case(seed: u64, s: usize, p_target: f64, dormancy: f64, sleep: (u64, u64)) -> Run {
    let mut cfg = cfg_with(seed, dormancy, sleep);
    if s > 0 {
        cfg.attack_kind = AttackKind::Sleeper { normal_until: NORMAL_UNTIL };
        cfg.attack_ratio = s as f64 / N as f64;
    } else {
        cfg.attack_kind = AttackKind::None;
        cfg.attack_ratio = 0.0;
    }

    let mut world = World::new(cfg);
    world.run();

    let ratings = world.ratings();
    let accs = world.accounts();
    let n = accs.len(); // 只用 accounts().len()，绝不用 alive_count()
    let quality: Vec<f64> = accs.iter().map(|a| a.quality as f64).collect();
    // 潜伏者 = 攻击者；Promoted = 被抬举的收益方（is_attacker() 对它返回 false，
    // 所以必须单独一类，否则会被算进"诚实账户"里）。
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
    let p_actual = if alive.is_empty() { 0.0 } else { off as f64 / alive.len() as f64 };

    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| {
            (0..=ROUNDS)
                .map(|r| world.was_online_at(i, r * (STEPS as u64 / ROUNDS)))
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
    };

    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n {
            e.intern(key(i));
        }
        for (o, t, sc) in &ratings {
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
        for &a in &alive {
            if online[a][r as usize] {
                if let Some(e) = &nodes[a] {
                    views[a] = Some(e.export_view(t));
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
    let alive_n = alive.len();
    let k_top = ((alive_n as f64) * 0.10).ceil() as usize;
    // 诚实 = 既不是潜伏者，也不是被推广的收益方
    let honest_alive: Vec<usize> = alive
        .iter()
        .copied()
        .filter(|&i| !is_att[i] && !is_prom[i])
        .collect();
    let sleeper_alive: Vec<usize> = alive.iter().copied().filter(|&i| is_att[i]).collect();
    let promoted_all: Vec<usize> = (0..n).filter(|&i| is_prom[i]).collect();
    let promoted_alive: Vec<usize> = alive.iter().copied().filter(|&i| is_prom[i]).collect();
    let promoted: Option<usize> = promoted_all.first().copied();

    let mut sp_vec: Vec<f64> = Vec::new();
    let mut sp_alive_vec: Vec<f64> = Vec::new();
    let mut sp_personal_vec: Vec<f64> = Vec::new();
    let mut own: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for (o, t, sc) in &ratings {
        own[*o].push((*t, *sc));
    }
    let (mut win, mut win_n) = (0usize, 0usize);
    let mut gini_vec: Vec<f64> = Vec::new();
    let mut sp_on_vec: Vec<f64> = Vec::new();
    let (mut win_on, mut win_on_n) = (0usize, 0usize);
    let mut gini_on_vec: Vec<f64> = Vec::new();
    let mut reach_on: HashSet<usize> = HashSet::new();
    let mut reach: HashSet<usize> = HashSet::new();
    let mut support: Vec<f64> = Vec::new();
    let mut dead_mass_v: Vec<f64> = Vec::new();
    let (mut win_allc, mut win_all_n) = (0usize, 0usize);
    let mut rel_all_v: Vec<f64> = Vec::new();
    let mut n_att_all = 0usize;
    let mut n_hon_all = 0usize;
    let mut sleeper_mass: Vec<f64> = Vec::new();
    let mut sleeper_mass_all_v: Vec<f64> = Vec::new();
    let mut visible_n = 0usize;
    let mut rel: Vec<f64> = Vec::new();
    let mut target_mass: Vec<f64> = Vec::new();
    let mut prom_pct_v: Vec<f64> = Vec::new();
    let mut prom_mass_v: Vec<f64> = Vec::new();
    let mut prom_seen_n = 0usize;
    let mut prom_obs_n = 0usize;
    let mut best_pct: Vec<f64> = Vec::new();
    let mut sum_err = 0.0f64;
    let mut views_cnt: Vec<f64> = Vec::new();

    let target = world.attack.target;
    n_att_all = is_att.iter().filter(|x| **x).count();
    let n_prom_all = is_prom.iter().filter(|x| **x).count();
    n_hon_all = n - n_att_all - n_prom_all;
    let sleeper_all: Vec<usize> = (0..n).filter(|&i| is_att[i]).collect();

    for &obs in &honest_alive {
        let e = nodes[obs].as_ref().unwrap();
        let dist = e.trust_distribution(now);
        let total: f64 = dist.iter().map(|(_, v)| *v as f64).sum();
        sum_err = sum_err.max((total - 1.0).abs());
        let is_on = online[obs][ROUNDS as usize];
        views_cnt.push(e.freshness_stats(now).count as f64);
        dead_mass_v.push(
            dist.iter()
                .filter(|(i, _)| !alive_set.contains(&(*i as usize)))
                .map(|(_, v)| *v as f64)
                .sum::<f64>(),
        );
        if !sleeper_all.is_empty() && n_hon_all > 0 {
            sleeper_mass_all_v.push(
                sleeper_all.iter().map(|&i| dist_share(&dist, i)).sum::<f64>(),
            );
            let hm_all: f64 = (0..n)
                .filter(|&i| !is_att[i] && !is_prom[i])
                .map(|i| dist_share(&dist, i))
                .sum::<f64>()
                / n_hon_all as f64;
            let sm_all: f64 =
                sleeper_all.iter().map(|&i| dist_share(&dist, i)).sum::<f64>() / n_att_all as f64;
            win_all_n += 1;
            if hm_all > sm_all {
                win_allc += 1;
            }
            if hm_all > 0.0 {
                rel_all_v.push(sm_all / hm_all);
            }
        }

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

        // 参照基线：只用自己的直接评分排序（等价于 beta=1 的个人项）
        if own[obs].len() >= 3 {
            let xs: Vec<f64> = own[obs].iter().map(|(_, v)| *v as f64).collect();
            let ys: Vec<f64> = own[obs].iter().map(|(t, _)| quality[*t]).collect();
            if let Some(r) = spearman(&xs, &ys) {
                sp_personal_vec.push(r);
            }
        }

        // ① Spearman（支持集上）
        if sup.len() >= 3 {
            let xs: Vec<f64> = sup.iter().map(|(_, v)| *v).collect();
            let ys: Vec<f64> = sup.iter().map(|(i, _)| quality[*i]).collect();
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
            let xs_a: Vec<f64> = alive.iter().map(|&i| xs[i]).collect();
            let ys_a: Vec<f64> = alive.iter().map(|&i| quality[i]).collect();
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
            sleeper_mass.push(smass);
            if smass > 0.0 {
                visible_n += 1;
            }
            if hm > 0.0 {
                rel.push(sm / hm);
            }
            if let Some(t) = target {
                target_mass.push(dist_share(&dist, t as usize));
            }
        }

        // 排名：存活账户按 share 降序（同分按内部编号升序）
        let mut ranks: Vec<(usize, f64)> =
            alive.iter().map(|&i| (i, dist_share(&dist, i))).collect();
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

        // ②b 收益方（Promoted）：单独统计，不能混进"潜伏者"
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
        seed,
        p_target,
        p_actual,
        dormancy,
        s,
        n_accounts: n,
        n_alive: alive_n,
        n_obs: honest_alive.len(),
        n_obs_online: honest_alive
            .iter()
            .filter(|&&i| online[i][ROUNDS as usize])
            .count(),
        n_sleepers: accs.iter().filter(|a| a.kind.is_attacker()).count(),
        n_slp_alive: sleeper_alive.len(),
        slp_rated: ratings.iter().filter(|(_, t, _)| is_att[*t]).count(),
        target_idx: target.map(|t| t as i64).unwrap_or(-1),
        sp: mean(&sp_vec),
        sp_alive: mean(&sp_alive_vec),
        sp_personal: mean(&sp_personal_vec),
        sp_n: sp_vec.len(),
        sp_on: mean(&sp_on_vec),
        sp_on_n: sp_on_vec.len(),
        win: if win_n > 0 { win as f64 / win_n as f64 } else { f64::NAN },
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
        sleeper_mass: mean(&sleeper_mass),
        sleeper_mass_all: mean(&sleeper_mass_all_v),
        sleeper_visible: if win_n > 0 { visible_n as f64 / win_n as f64 } else { f64::NAN },
        dead_mass: mean(&dead_mass_v),
        win_all: if win_all_n > 0 { win_allc as f64 / win_all_n as f64 } else { f64::NAN },
        rel_all: mean(&rel_all_v),
        rel: mean(&rel),
        target_mass: mean(&target_mass),
        prom_mass: mean(&prom_mass_v),
        prom_pct: if prom_pct_v.is_empty() { f64::NAN } else { mean(&prom_pct_v) },
        prom_seen: if prom_obs_n > 0 {
            prom_seen_n as f64 / prom_obs_n as f64
        } else {
            f64::NAN
        },
        n_prom: n_prom_all,
        n_prom_alive: promoted_alive.len(),
        prom_rated: match promoted {
            Some(p) => ratings.iter().filter(|(_, t, _)| *t == p).count(),
            None => 0,
        },
        prom_recv: match promoted {
            Some(p) => accs[p].received_count,
            None => 0,
        },
        prom_q_pct: match promoted {
            Some(p) => {
                let q = quality[p];
                alive.iter().filter(|&&i| quality[i] < q).count() as f64 / alive_n.max(1) as f64
            }
            None => f64::NAN,
        },
        prom_quality: match promoted {
            Some(p) => quality[p],
            None => f64::NAN,
        },
        best_sleeper_pct: mean(&best_pct),
        sum_err,
        mean_views: mean(&views_cnt),
    }
}

// ════════════════════════════ 输出 ════════════════════════════

fn sd(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let v = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64;
    v.sqrt()
}

fn fmt(v: f64, nan: &str) -> String {
    if v.is_nan() {
        nan.to_string()
    } else {
        format!("{v:.4}")
    }
}

fn pick<'a>(all: &'a [Run], seeds: &[u64], p: f64, s: usize) -> Vec<&'a Run> {
    all.iter()
        .filter(|r| (r.p_target - p).abs() < 1e-9 && r.s == s && seeds.contains(&r.seed))
        .collect()
}

fn agg(rs: &[&Run], f: fn(&Run) -> f64) -> (f64, f64) {
    let v: Vec<f64> = rs.iter().map(|r| f(r)).filter(|x| !x.is_nan()).collect();
    (mean(&v), sd(&v))
}

fn table_core(all: &[Run], seeds: &[u64], ps: &[f64], ss: &[usize], title: &str) {
    println!("\n## {title}（{} seed，均值 ± 样本标准差）", seeds.len());
    println!(
        "{:>4} {:>4} {:>7} {:>16} {:>16} {:>16} {:>16} {:>16} {:>16} {:>17} {:>17} {:>8} {:>8}",
        "p%", "s", "off%", "Spearman", "Spearman0", "只靠自己", "胜率", "基尼", "可达性",
        "潜伏者份额", "潜/诚人均", "被看见", "死账户"
    );
    for &p in ps {
        for &s in ss {
            let rs = pick(all, seeds, p, s);
            if rs.is_empty() {
                continue;
            }
            let (a, b) = agg(&rs, |r| r.sp);
            let (c, d) = agg(&rs, |r| r.sp_alive);
            let (le, lf) = agg(&rs, |r| r.sp_personal);
            let (e, f) = agg(&rs, |r| r.win);
            let (g, h) = agg(&rs, |r| r.gini);
            let (i, j) = agg(&rs, |r| r.reach);
            let (k, l) = agg(&rs, |r| r.sleeper_mass);
            let (m, n) = agg(&rs, |r| r.rel);
            let (o, _) = agg(&rs, |r| r.sleeper_visible);
            let (q, _) = agg(&rs, |r| r.dead_mass);
            let offm = mean(&rs.iter().map(|r| r.p_actual * 100.0).collect::<Vec<_>>());
            println!(
                "{:>4.0} {:>4} {:>7.1} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>8.5}±{:>7.5} {:>8.4}±{:>7.4} {:>8} {:>8.3}",
                p * 100.0,
                s,
                offm,
                a,
                b,
                c,
                d,
                le,
                lf,
                fmt(e, "-"),
                if f.is_nan() { 0.0 } else { f },
                g,
                h,
                i,
                j,
                k,
                l,
                fmt(m, "-"),
                if n.is_nan() { 0.0 } else { n },
                fmt(o, "-"),
                q
            );
        }
    }
}

fn table_online(all: &[Run], seeds: &[u64], ps: &[f64], ss: &[usize], title: &str) {
    println!("\n## {title}（只看末轮在线的诚实观察者，{} seed，均值）", seeds.len());
    println!(
        "{:>4} {:>4} {:>10} {:>16} {:>16} {:>16} {:>16} {:>10}",
        "p%", "s", "在线观察者", "Spearman", "胜率", "基尼", "可达性", "被看见"
    );
    for &p in ps {
        for &s in ss {
            let rs = pick(all, seeds, p, s);
            if rs.is_empty() {
                continue;
            }
            let (a, b) = agg(&rs, |r| r.sp_on);
            let (e, f) = agg(&rs, |r| r.win_on);
            let (g, h) = agg(&rs, |r| r.gini_on);
            let (i, j) = agg(&rs, |r| r.reach_on);
            let (o, _) = agg(&rs, |r| r.sleeper_visible);
            let on = mean(&rs.iter().map(|r| r.n_obs_online as f64).collect::<Vec<_>>());
            println!(
                "{:>4.0} {:>4} {:>10.1} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>7.4}±{:>7.4} {:>10}",
                p * 100.0,
                s,
                on,
                a,
                b,
                fmt(e, "-"),
                if f.is_nan() { 0.0 } else { f },
                g,
                h,
                i,
                j,
                fmt(o, "-")
            );
        }
    }
}

fn table_allwin(all: &[Run], seeds: &[u64], ps: &[f64], ss: &[usize]) {
    println!("\n## 附加 B：把已离开账户也计入分母的胜率（{} seed，均值）", seeds.len());
    println!(
        "{:>4} {:>4} {:>16} {:>17} {:>10}",
        "p%", "s", "胜率(全体)", "潜/诚人均(全体)", "潜份额(全体)"
    );
    for &p in ps {
        for &s in ss {
            let rs = pick(all, seeds, p, s);
            if rs.is_empty() {
                continue;
            }
            let (e, f) = agg(&rs, |r| r.win_all);
            let (m, n) = agg(&rs, |r| r.rel_all);
            let (o, _) = agg(&rs, |r| r.sleeper_mass_all);
            println!(
                "{:>4.0} {:>4} {:>7.4}±{:>7.4} {:>8.4}±{:>7.4} {:>10.5}",
                p * 100.0,
                s,
                fmt(e, "-"),
                if f.is_nan() { 0.0 } else { f },
                fmt(m, "-"),
                if n.is_nan() { 0.0 } else { n },
                o
            );
        }
    }
}

fn table_aux(all: &[Run], seeds: &[u64], ps: &[f64], ss: &[usize]) {
    println!("\n## 附加 C：计数（{} seed 均值）", seeds.len());
    println!(
        "{:>4} {:>4} {:>8} {:>8} {:>9} {:>8} {:>8} {:>9} {:>11} {:>10}",
        "p%", "s", "账户数", "存活", "观察者", "末轮在线", "缓存视图", "支持集", "总和误差", "dormancy"
    );
    for &p in ps {
        for &s in ss {
            let rs = pick(all, seeds, p, s);
            if rs.is_empty() {
                continue;
            }
            let m = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
            println!(
                "{:>4.0} {:>4} {:>8.1} {:>8.1} {:>9.1} {:>8.1} {:>8.1} {:>9.1} {:>11.2e} {:>10.7}",
                p * 100.0,
                s,
                m(|r| r.n_accounts as f64),
                m(|r| r.n_alive as f64),
                m(|r| r.n_obs as f64),
                m(|r| r.n_obs_online as f64),
                m(|r| r.mean_views),
                m(|r| r.support),
                m(|r| r.sum_err),
                m(|r| r.dormancy)
            );
        }
    }
}

fn table_prom(all: &[Run], seeds: &[u64], ps: &[f64], ss: &[usize]) {
    println!("\n## 表 D：收益方 Promoted（{} seed，均值）", seeds.len());
    println!(
        "{:>4} {:>4} {:>9} {:>10} {:>16} {:>13} {:>13} {:>10} {:>9} {:>10} {:>10}",
        "p%", "s", "潜伏存活", "潜伏/存活", "潜伏者份额", "潜伏人均份额", "收益方份额",
        "收益方分位", "收益方被看见", "收益方质量", "质量分位"
    );
    for &p in ps {
        for &s in ss {
            let rs = pick(all, seeds, p, s);
            if rs.is_empty() || s == 0 {
                continue;
            }
            let m = |f: fn(&Run) -> f64| mean(&rs.iter().map(|r| f(r)).collect::<Vec<_>>());
            let alive = m(|r| r.n_alive as f64);
            let slp = m(|r| r.n_slp_alive as f64);
            let smass = m(|r| r.sleeper_mass);
            let per = if slp > 0.0 { smass / slp } else { f64::NAN };
            println!(
                "{:>4.0} {:>4} {:>9.1} {:>9.1}% {:>16.5} {:>13.5} {:>13.5} {:>10} {:>9.4} {:>10.3} {:>10.3}",
                p * 100.0,
                s,
                slp,
                if alive > 0.0 { slp / alive * 100.0 } else { 0.0 },
                smass,
                per,
                m(|r| r.prom_mass),
                fmt(m(|r| r.prom_pct), "-"),
                m(|r| r.prom_seen),
                m(|r| r.prom_quality),
                m(|r| r.prom_q_pct)
            );
        }
    }
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
    let sleep: (u64, u64) = match args.iter().position(|a| a == "--sleep") {
        Some(i) => (args[i + 1].parse().unwrap(), args[i + 2].parse().unwrap()),
        None => (200, 600),
    };
    let ps: Vec<f64> = if args.iter().any(|a| a == "--quick") {
        vec![PS[0]]
    } else {
        PS.to_vec()
    };
    let ss: Vec<usize> = match args.iter().position(|a| a == "--s") {
        Some(i) => args[i + 1]
            .split(',')
            .map(|x| x.trim().parse().unwrap())
            .collect(),
        None => SS.to_vec(),
    };

    let t0 = Instant::now();
    println!("# L5 潜伏攻击 × 网络整体可用性");
    println!(
        "# n_initial={N} steps={STEPS} join_rate=0.05 leave_rate=0.001 return_rate=0.05 sleep_hours=({},{}) enable_rhythm=false",
        sleep.0, sleep.1
    );
    println!(
        "# 交换协议：{ROUNDS} 轮 × 每轮 {SAMPLES} 个来源（回归节点 ×3），半衰期 72h，beta=0.3，view 2048x5 seed=99"
    );
    println!("# 潜伏者突变时刻 normal_until={NORMAL_UNTIL}（steps/5）");
    println!("# seeds = {seeds:?}\n");

    let mut all: Vec<Run> = Vec::new();
    for &p in &ps {
        for &seed in &seeds {
            let (rate, achieved) = calibrate(p, seed, sleep);
            println!(
                "calib p={:.0}% seed={} -> dormancy_rate={:.7} 实测离线={:.1}%",
                p * 100.0, seed, rate, achieved * 100.0
            );
            for &s in &ss {
                all.push(run_case(seed, s, p, rate, sleep));
            }
        }
    }

    println!("\n## 明细（每个 seed 一行）");
    println!(
        "{:>4} {:>5} {:>4} {:>6} {:>6} {:>6} {:>6} {:>4} {:>3} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10} {:>8} {:>9} {:>8} {:>8} {:>8} {:>9} {:>8} {:>8} {:>6}",
        "p%", "seed", "s", "off%", "acct", "alive", "obs", "slp", "slpA", "slpR", "sp", "spAll", "spOwn", "win",
        "gini", "reach", "slpMass", "rel", "tgtMass", "bestPct", "seen", "dead", "promMass", "promPct", "promSeen", "promRated"
    );
    for r in &all {
        println!(
            "{:>4.0} {:>5} {:>4} {:>6.1} {:>6} {:>6} {:>6} {:>4} {:>3} {:>6} {:>8.4} {:>8.4} {:>8.4} {:>8.4} {:>8.4} {:>8.4} {:>10.5} {:>8.3} {:>9.5} {:>8.4} {:>8.4} {:>8.4} {:>9.5} {:>8.4} {:>8.4} {:>6}",
            r.p_target * 100.0,
            r.seed,
            r.s,
            r.p_actual * 100.0,
            r.n_accounts,
            r.n_alive,
            r.n_obs,
            r.n_sleepers,
            r.n_slp_alive,
            r.slp_rated,
            r.sp,
            r.sp_alive,
            r.sp_personal,
            fmt(r.win, "-"),
            r.gini,
            r.reach,
            r.sleeper_mass,
            r.rel,
            r.target_mass,
            r.best_sleeper_pct,
            fmt(r.sleeper_visible, "-"),
            r.dead_mass,
            r.prom_mass,
            fmt(r.prom_pct, "-"),
            fmt(r.prom_seen, "-"),
            r.prom_rated
        );
    }

    let fixed: Vec<u64> = SEEDS.iter().copied().filter(|s| seeds.contains(s)).collect();
    table_core(&all, &fixed, &ps, &ss, "主表 A：固化 3 seed {11,23,37}");
    if seeds.len() > fixed.len() {
        table_core(&all, &seeds, &ps, &ss, "主表 B：全部 seed");
    }
    table_online(&all, &seeds, &ps, &ss, "表 C：末轮在线观察者子集");

    table_prom(&all, &seeds, &ps, &ss);
    table_allwin(&all, &seeds, &ps, &ss);
    table_aux(&all, &seeds, &ps, &ss);
    println!("\n总耗时 {:.1}s", t0.elapsed().as_secs_f64());
}
