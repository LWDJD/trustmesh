//! L4 · 潜伏攻击对「旧用户自己」的影响
//!
//! 真多 runtime：每个账户一个独立 Engine；离线节点不 tick、不取观点；
//! 观点年龄按 20h 一轮真实增长；回归节点当轮 3× 抽样快速刷新（同 grid.rs）。
//!
//! 相对 grid.rs 模板只做「更严格的时间对齐」，不改任何算法：
//!   1. 正分清单按轮从世界同步（不是一次性灌最终快照）；
//!   2. 世界与引擎按 20h 一轮交替推进，回归事件在线检测；
//!   3. 观测窗 = 400h 固定窗 + 回归后 240h 跟踪；
//!      前 400h 的轨迹与 steps=400 的世界逐位一致（cfg.steps 不参与 step，--check 验证）。
//!
//! 每轮对全部在线诚实观察者算一次完整 trust_distribution（归一化，总和恒为 1），
//! 于是「声望」= 别人分给它的份额，且能拿到它在全体用户中的百分位。
use tm_agg::{decay, AggMode};
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, Life, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

const N_INIT: usize = 300;
const STEPS: u64 = 400;
const ROUND_LEN: u64 = 20;
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const NK: usize = 10; // 第 1 轮 .. 第 10 轮
const MARGIN: u64 = 240; // 回归后至少 12 轮跟踪
const CTRL_CAP: usize = 48;
const CTRL_SLEEP_CAP: usize = 16;

const PS: [f64; 4] = [0.30, 0.50, 0.70, 0.90];
const DS: [u64; 3] = [72, 216, 720];
const SEEDS: [u64; 3] = [11, 23, 37];

// 每个 k 上累计的指标
const M_RATIO: usize = 0; // 声望 / 同期一直在线均值
const M_REP: usize = 1; // 声望绝对值（归一化份额）
const M_PRE: usize = 2; // 声望 / 自己离线前那一轮
const M_PCT: usize = 3; // 声望在全体诚实在线用户中的百分位
const M_ADOPT: usize = 4; // 观点在别人群体项里的权重占比
const M_ADOPT_C: usize = 5;
const M_COVER: usize = 6; // 观点被多少人缓存
const M_COVER_C: usize = 7;
const M_SLEEP: usize = 8; // 潜伏者排名百分位（1 = 第一）
const M_SLEEP_C: usize = 9;
const M_SLEEPV: usize = 10; // 潜伏者在回归者眼中的份额
const M_AGE: usize = 11; // 自己观点缓存的中位年龄
const M_AGE_C: usize = 12;
const M_ON: usize = 13; // 该轮自己是否在线
const M_SLEEPV_C: usize = 14; // 潜伏者在一直在线组眼中的份额
const M_PROM: usize = 15; // 收益方（Promoted）排名百分位（回归者）
const M_PROM_C: usize = 16; // 收益方排名百分位（一直在线）
const M_PROMV: usize = 17; // 收益方份额（回归者）
const M_PROMV_C: usize = 18; // 收益方份额（一直在线）
const NM: usize = 19;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn ecfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: HALF_LIFE,
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
    }
}

fn sim_cfg(dormancy: f64, d: u64, seed: u64, steps: usize, attack: bool) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps,
        seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: dormancy,
        sleep_hours: (d, d),
        return_rate: 1.0,
        enable_rhythm: false,
        attack_kind: if attack {
            AttackKind::Sleeper { normal_until: STEPS / 5 }
        } else {
            AttackKind::None
        },
        attack_ratio: if attack { 1.0 / N_INIT as f64 } else { 0.0 },
        ..Default::default()
    }
}

fn alive_at(w: &World, i: usize, t: u64) -> bool {
    if i >= w.accounts.len() || t < w.accounts[i].joined_at {
        return false;
    }
    matches!(w.life_traces[i].state_at(t), Some(Life::Active) | Some(Life::Dormant { .. }))
}

// ─────────────────────────── 标定 ───────────────────────────

/// 某个 dormancy_rate 下的离线比例；对多个 seed 取平均，压掉「换参数就换整条轨迹」的噪声。
fn offline_at_end(dormancy: f64, d: u64, seeds: &[u64]) -> (f64, usize, usize) {
    let mut ps = 0.0f64;
    let mut off_sum = 0usize;
    let mut alive_sum = 0usize;
    for &seed in seeds {
        let mut w = World::new(sim_cfg(dormancy, d, seed, STEPS as usize, true));
        w.run();
        let alive: Vec<usize> = (0..w.accounts.len()).filter(|&i| alive_at(&w, i, STEPS)).collect();
        if alive.is_empty() {
            continue;
        }
        let off = alive.iter().filter(|&&i| !w.was_online_at(i, STEPS)).count();
        ps += off as f64 / alive.len() as f64;
        off_sum += off;
        alive_sum += alive.len();
    }
    let n = seeds.len().max(1) as f64;
    (ps / n, off_sum / seeds.len().max(1), alive_sum / seeds.len().max(1))
}

/// 标定 dormancy_rate：先在对数网格上粗扫（离线比例对 rate 高度非线性，
/// 线性二分会被饱和段困住），再在最接近的两点之间做对数二分细化。
fn calibrate(target: f64, d: u64, seeds: &[u64]) -> (f64, f64, usize, usize) {
    let dbg = std::env::var("L4_DEBUG").is_ok();
    let l0 = 1e-5f64.ln();
    let l1 = 0.05f64.ln();
    let n = 14usize;
    let mut best = (0.0f64, f64::INFINITY, 0usize, 0usize);
    let mut xs: Vec<(f64, f64)> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let x = (l0 + (l1 - l0) * i as f64 / n as f64).exp();
        let (f, o, a) = offline_at_end(x, d, seeds);
        if dbg {
            eprintln!("  sweep {:.6} -> {:.4}", x, f);
        }
        if (f - target).abs() < (best.1 - target).abs() {
            best = (x, f, o, a);
        }
        xs.push((x, f));
    }
    if best.1 == f64::INFINITY {
        return best;
    }
    let idx = xs.iter().position(|&(x, _)| x == best.0).unwrap_or(0);
    let mut lo = if idx > 0 { xs[idx - 1].0 } else { xs[0].0 * 0.5 };
    let mut hi = if idx + 1 < xs.len() { xs[idx + 1].0 } else { xs[xs.len() - 1].0 * 2.0 };
    for _ in 0..7 {
        let mid = (lo * hi).sqrt();
        let (f, o, a) = offline_at_end(mid, d, seeds);
        if dbg {
            eprintln!("  refine {:.6} -> {:.4}", mid, f);
        }
        if (f - target).abs() < (best.1 - target).abs() {
            best = (mid, f, o, a);
        }
        if f < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
}

// ─────────────────────────── 单次运行 ───────────────────────────

#[derive(Default, Clone, Copy)]
struct MainSnap {
    alive: usize,
    online: usize,
    n_ret_all: usize,
    n_ret_fresh: usize,
    n_ctrl: usize,
    rep_ret_all: f64,
    rep_ret_fresh: f64,
    rep_ctrl: f64,
    pct_ret_fresh: f64,
    pct_ctrl: f64,
    act_ret: f64,
    act_ctrl: f64,
    s_mean: f64,
}

struct RunOut {
    p_target: f64,
    d: u64,
    p_meas: f64,
    main: MainSnap,
    k_sum: [[f64; NM]; NK],
    k_cnt: [usize; NK],
    n_events: usize,
    gap_mean: f64,
}

fn stride_sample(v: &[usize], cap: usize) -> Vec<usize> {
    if v.len() <= cap {
        return v.to_vec();
    }
    let step = (v.len() + cap - 1) / cap;
    v.iter().copied().step_by(step).take(cap).collect()
}

fn w_total(e: &Engine, t: u64) -> f64 {
    let mut tot = 0.0f64;
    for (s, sh) in e.shares() {
        if let Some(age) = e.view_age(s, t) {
            tot += (sh * decay(age, HALF_LIFE)) as f64;
        }
    }
    tot
}

fn w_of(e: &Engine, src: usize, t: u64) -> f64 {
    match (e.share_of(src as u32), e.view_age(src as u32, t)) {
        (Some(sh), Some(age)) => (sh * decay(age, HALF_LIFE)) as f64,
        _ => 0.0,
    }
}

fn cover_of(src: usize, observers: &[usize], nodes: &[Option<Engine>], t: u64) -> f64 {
    let mut c = 0usize;
    let mut hit = 0usize;
    for &o in observers {
        if let Some(e) = nodes[o].as_ref() {
            c += 1;
            if e.view_age(src as u32, t).is_some() {
                hit += 1;
            }
        }
    }
    if c > 0 {
        hit as f64 / c as f64
    } else {
        0.0
    }
}

fn adopt_of(src: usize, observers: &[usize], nodes: &[Option<Engine>], wtot: &[f64], t: u64) -> f64 {
    let mut s = 0.0f64;
    let mut c = 0usize;
    for (j, &o) in observers.iter().enumerate() {
        if o == src {
            continue;
        }
        if let Some(e) = nodes[o].as_ref() {
            let wt = wtot[j];
            if wt > 0.0 {
                s += w_of(e, src, t) / wt;
                c += 1;
            }
        }
    }
    if c > 0 {
        s / c as f64
    } else {
        0.0
    }
}

/// 目标在观察者眼里的排名百分位（1 = 排第一）与「份额 ÷ 该分布中位数」。
///
/// 后者是形状无关的：把「这个人的分布整体更平」这件事除掉，
/// 只剩「相对普通人，它被抬高了多少」。
fn sleep_rank(e: &Engine, target: usize, n: usize, t: u64) -> (f64, f64) {
    if n <= 1 {
        return (0.0, 0.0);
    }
    let sv = e.query(target as u32, t);
    let mut better = 0usize;
    let mut vals: Vec<f32> = Vec::with_capacity(n);
    for i in 0..n {
        let v = if i == target { sv } else { e.query(i as u32, t) };
        if i != target && v > sv {
            better += 1;
        }
        vals.push(v);
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = vals[vals.len() / 2];
    let ratio = if med > 0.0 { sv as f64 / med as f64 } else { 0.0 };
    (1.0 - better as f64 / (n - 1) as f64, ratio)
}

/// 在 rep_row 里，x 的百分位（1 = 最高）
fn pct_in(rep_row: &[f32], idx: &[usize], x: usize) -> f64 {
    if idx.len() <= 1 {
        return 0.0;
    }
    let v = rep_row[x];
    let better = idx.iter().filter(|&&i| i != x && rep_row[i] > v).count();
    1.0 - better as f64 / (idx.len() - 1) as f64
}

fn run_case(p_target: f64, d: u64, seed: u64, dormancy: f64, attack: bool) -> RunOut {
    let rounds = ((STEPS + d + MARGIN + ROUND_LEN - 1) / ROUND_LEN) as usize;
    let steps_ext = rounds as u64 * ROUND_LEN;
    let mut world = World::new(sim_cfg(dormancy, d, seed, steps_ext as usize, attack));

    let mut nodes: Vec<Option<Engine>> = Vec::new();
    let mut interned_n = 0usize;
    let mut prev_off: Vec<u64> = Vec::new();
    let mut ever_gap: Vec<bool> = Vec::new();
    let mut ret_ever: Vec<bool> = Vec::new();
    let mut last_ret: Vec<usize> = Vec::new();
    let mut last_on: Vec<usize> = Vec::new();
    let mut rep_hist: Vec<Vec<f32>> = Vec::new(); // rep_hist[r][x] 相对中位用户的声望
    let mut on_hist: Vec<Vec<bool>> = Vec::new(); // on_hist[r][x] 该轮是否在线
    let mut events: Vec<(usize, usize, u64)> = Vec::new(); // (账户, 回归轮, 间隔小时)
    let mut k_sum = [[0.0f64; NM]; NK];
    let mut k_cnt = [0usize; NK];
    let g_min = (d * 8 / 10).max(1);
    let main_round = (STEPS / ROUND_LEN) as usize;
    let mut main = MainSnap::default();
    let mut gap_sum = 0.0f64;
    let mut s_sum = 0.0f64;
    let mut s_cnt = 0usize;

    for r in 0..=rounds {
        let t = r as u64 * ROUND_LEN;
        let n = world.accounts().len();
        if nodes.len() < n {
            nodes.resize_with(n, || None);
            prev_off.resize(n, 0);
            ever_gap.resize(n, false);
            ret_ever.resize(n, false);
            last_ret.resize(n, 0);
            last_on.resize(n, 0);
        }

        let alive: Vec<bool> = world.accounts().iter().map(|a| a.is_alive()).collect();
        let is_att: Vec<bool> = world.accounts().iter().map(|a| a.kind.is_attacker()).collect();
        let is_prom: Vec<bool> = world
            .accounts()
            .iter()
            .map(|a| matches!(a.kind, Kind::Promoted))
            .collect();
        let joined: Vec<u64> = world.accounts().iter().map(|a| a.joined_at).collect();
        let act: Vec<f64> = world.accounts().iter().map(|a| a.activity).collect();
        let online: Vec<bool> = (0..n).map(|i| alive[i] && world.was_online_at(i, t)).collect();

        // 1. 引擎：新建 + intern 新编号（编号只增不减，绝不用 alive_count）
        for i in 0..n {
            if alive[i] && nodes[i].is_none() {
                let mut e = Engine::new(ecfg());
                for j in 0..n {
                    e.intern(key(j));
                }
                nodes[i] = Some(e);
            }
        }
        if n > interned_n {
            for i in 0..n {
                if let Some(e) = nodes[i].as_mut() {
                    for j in interned_n..n {
                        e.intern(key(j));
                    }
                }
            }
            interned_n = n;
        }

        // 2. 回归检测（在线，间隔 ≥ 0.8D）
        let mut just_returned: Vec<usize> = Vec::new();
        for i in 0..n {
            if !alive[i] || !online[i] {
                continue;
            }
            if prev_off[i] > 0 && prev_off[i] + ROUND_LEN >= g_min {
                let gap = prev_off[i] + ROUND_LEN;
                events.push((i, r, gap));
                gap_sum += gap as f64;
                ever_gap[i] = true;
                ret_ever[i] = true;
                last_ret[i] = r;
                just_returned.push(i);
            }
        }

        // 3. 同步正分清单（本轮 t 时刻世界的状态）
        let ratings: Vec<Vec<(usize, f32)>> =
            world.accounts().iter().map(|a| a.ratings.clone()).collect();
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

        // 4. 全量面板：每个在线诚实观察者的归一化信任分布
        // 诚实 = 非攻击者且不是被推广的收益方
        let honest_online: Vec<usize> = (0..n)
            .filter(|&i| online[i] && !is_att[i] && !is_prom[i])
            .collect();
        let mut rep_row: Vec<f32> = vec![0.0; n];
        if !honest_online.is_empty() {
            let mut acc = vec![0.0f64; n];
            let mut cnt = vec![0usize; n];
            let mut selfv = vec![0.0f64; n];
            for &o in &honest_online {
                let e = match nodes[o].as_ref() {
                    Some(e) => e,
                    None => continue,
                };
                let mut sum = 0.0f32;
                let mut row: Vec<f32> = vec![0.0; n];
                for i in 0..n {
                    let v = e.query(i as u32, t);
                    row[i] = v;
                    sum += v;
                }
                if sum <= 0.0 {
                    continue;
                }
                let inv = 1.0 / sum as f64;
                for i in 0..n {
                    acc[i] += row[i] as f64 * inv;
                    cnt[i] += 1;
                }
                selfv[o] = row[o] as f64 * inv;
            }
            let mut in_obs = vec![false; n];
            for &o in &honest_online {
                in_obs[o] = true;
            }
            for i in 0..n {
                let (num, den) = if in_obs[i] {
                    (acc[i] - selfv[i], cnt[i].saturating_sub(1))
                } else {
                    (acc[i], cnt[i])
                };
                if den > 0 {
                    rep_row[i] = (num / den as f64) as f32;
                }
            }
            // 诊断：观测者 query 总和（归一化前）
            if r == main_round {
                for &o in honest_online.iter().take(8) {
                    if let Some(e) = nodes[o].as_ref() {
                        s_sum += (0..n).map(|i| e.query(i as u32, t) as f64).sum::<f64>();
                        s_cnt += 1;
                    }
                }
            }
        }
        // 声望按「同期诚实在线用户的中位数」归一化（中位数比均值稳，不受在线集合大小波动影响）
        let mut pv: Vec<f32> = honest_online.iter().map(|&i| rep_row[i]).collect();
        pv.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pop_mean = if pv.is_empty() { 0.0 } else { pv[pv.len() / 2] as f64 };
        let rel_row: Vec<f32> = if pop_mean > 0.0 {
            rep_row.iter().map(|&v| (v as f64 / pop_mean) as f32).collect()
        } else {
            rep_row.clone()
        };
        rep_hist.push(rel_row);
        on_hist.push(online.clone());

        // 5. 观测（交换之前）
        if r >= 1 {
            let ctrl_all: Vec<usize> = honest_online
                .iter()
                .copied()
                .filter(|&i| joined[i] == 0 && !ever_gap[i])
                .collect();
            let ctrl = stride_sample(&ctrl_all, CTRL_CAP);
            let sleeper = world
                .accounts()
                .iter()
                .find(|a| matches!(a.kind, Kind::Sleeper { .. }))
                .map(|a| a.idx);
            let promoted = world
                .accounts()
                .iter()
                .find(|a| matches!(a.kind, Kind::Promoted))
                .map(|a| a.idx);
            let active: Vec<(usize, usize, u64)> = events
                .iter()
                .copied()
                .filter(|&(_, r0, _)| r >= r0 && r < r0 + NK)
                .collect();

            if !active.is_empty() || r == main_round {
                let wtot: Vec<f64> = honest_online
                    .iter()
                    .map(|&o| nodes[o].as_ref().map(|e| w_total(e, t)).unwrap_or(0.0))
                    .collect();
                let mean_rep = |xs: &[usize]| -> f64 {
                    if xs.is_empty() {
                        return 0.0;
                    }
                    xs.iter().map(|&x| rep_row[x] as f64).sum::<f64>() / xs.len() as f64
                };
                let rep_ctrl = mean_rep(&ctrl);
                let adopt_ctrl = if ctrl.is_empty() {
                    0.0
                } else {
                    ctrl.iter()
                        .map(|&x| adopt_of(x, &honest_online, &nodes, &wtot, t))
                        .sum::<f64>()
                        / ctrl.len() as f64
                };
                let cover_ctrl = if ctrl.is_empty() {
                    0.0
                } else {
                    ctrl.iter()
                        .map(|&x| cover_of(x, &honest_online, &nodes, t))
                        .sum::<f64>()
                        / ctrl.len() as f64
                };
                let sleep_ctrl = if let Some(sp) = sleeper {
                    let sub = stride_sample(&ctrl, CTRL_SLEEP_CAP);
                    if sub.is_empty() {
                        (0.0, 0.0)
                    } else {
                        let mut a = 0.0;
                        let mut b = 0.0;
                        for &x in &sub {
                            if let Some(e) = nodes[x].as_ref() {
                                let (p, v) = sleep_rank(e, sp, n, t);
                                a += p;
                                b += v;
                            }
                        }
                        (a / sub.len() as f64, b / sub.len() as f64)
                    }
                } else {
                    (f64::NAN, f64::NAN)
                };
                let prom_ctrl = if let Some(pr) = promoted {
                    let sub = stride_sample(&ctrl, CTRL_SLEEP_CAP);
                    if sub.is_empty() {
                        (0.0, 0.0)
                    } else {
                        let mut a = 0.0;
                        let mut b = 0.0;
                        for &x in &sub {
                            if let Some(e) = nodes[x].as_ref() {
                                let (p, v) = sleep_rank(e, pr, n, t);
                                a += p;
                                b += v;
                            }
                        }
                        (a / sub.len() as f64, b / sub.len() as f64)
                    }
                } else {
                    (f64::NAN, f64::NAN)
                };
                let age_ctrl = if ctrl.is_empty() {
                    0.0
                } else {
                    ctrl.iter()
                        .map(|&x| {
                            nodes[x]
                                .as_ref()
                                .map(|e| e.freshness_stats(t).median_age as f64)
                                .unwrap_or(0.0)
                        })
                        .sum::<f64>()
                        / ctrl.len() as f64
                };

                // 5a. 主表快照（t = 400）
                if r == main_round {
                    let ret_all: Vec<usize> = (0..n)
                        .filter(|&i| online[i] && !is_att[i] && !is_prom[i] && ret_ever[i])
                        .collect();
                    let ret_fresh: Vec<usize> = ret_all
                        .iter()
                        .copied()
                        .filter(|&i| last_ret[i] + 2 > r)
                        .collect();
                    main.alive = (0..n).filter(|&i| alive[i]).count();
                    main.online = (0..n).filter(|&i| online[i]).count();
                    main.n_ret_all = ret_all.len();
                    main.n_ret_fresh = ret_fresh.len();
                    main.n_ctrl = ctrl.len();
                    main.rep_ret_all = mean_rep(&ret_all);
                    main.rep_ret_fresh = mean_rep(&ret_fresh);
                    main.rep_ctrl = rep_ctrl;
                    main.pct_ret_fresh = if ret_fresh.is_empty() {
                        0.0
                    } else {
                        ret_fresh
                            .iter()
                            .map(|&x| pct_in(&rep_row, &honest_online, x))
                            .sum::<f64>()
                            / ret_fresh.len() as f64
                    };
                    main.pct_ctrl = if ctrl.is_empty() {
                        0.0
                    } else {
                        ctrl.iter()
                            .map(|&x| pct_in(&rep_row, &honest_online, x))
                            .sum::<f64>()
                            / ctrl.len() as f64
                    };
                    main.act_ret = if ret_all.is_empty() {
                        0.0
                    } else {
                        ret_all.iter().map(|&x| act[x]).sum::<f64>() / ret_all.len() as f64
                    };
                    main.act_ctrl = if ctrl.is_empty() {
                        0.0
                    } else {
                        ctrl.iter().map(|&x| act[x]).sum::<f64>() / ctrl.len() as f64
                    };
                    main.s_mean = if s_cnt > 0 { s_sum / s_cnt as f64 } else { 0.0 };
                }

                // 5b. 事件对齐的恢复曲线
                for &(a, r0, _gap) in &active {
                    let j = r - r0;
                    let rep_a = rep_row[a] as f64;
                    let ratio = if rep_ctrl > 0.0 { rep_a / rep_ctrl } else { 0.0 };
                    // 离线前基线：离线前最后一个在线轮往前共 3 轮里「在线轮」的平均（降噪）
                    let pre_r = last_on[a];
                    let mut ps = 0.0f64;
                    let mut pc = 0usize;
                    for rr in pre_r.saturating_sub(2)..=pre_r {
                        if rr >= 1 && rr < rep_hist.len() && a < on_hist[rr].len() && on_hist[rr][a] {
                            ps += rep_hist[rr][a] as f64;
                            pc += 1;
                        }
                    }
                    let pre = if pc > 0 { ps / pc as f64 } else { 0.0 };
                    let rel_now = if pop_mean > 0.0 { rep_a / pop_mean } else { 0.0 };
                    let pre_ratio = if pre > 0.0 { rel_now / pre } else { 0.0 };
                    let pct = pct_in(&rep_row, &honest_online, a);
                    let adopt_a = adopt_of(a, &honest_online, &nodes, &wtot, t);
                    let cover_a = cover_of(a, &honest_online, &nodes, t);
                    let (sleep_a, sleepv_a, age_a) = match nodes[a].as_ref() {
                        Some(e) => {
                            let (p, v) = sleeper
                                .map(|sp| sleep_rank(e, sp, n, t))
                                .unwrap_or((f64::NAN, f64::NAN));
                            (p, v, e.freshness_stats(t).median_age as f64)
                        }
                        None => (f64::NAN, f64::NAN, 0.0),
                    };
                    let (prom_a, promv_a) = match nodes[a].as_ref() {
                        Some(e) => promoted
                            .map(|pr| sleep_rank(e, pr, n, t))
                            .unwrap_or((f64::NAN, f64::NAN)),
                        None => (f64::NAN, f64::NAN),
                    };
                    let ks = &mut k_sum[j];
                    ks[M_RATIO] += ratio;
                    ks[M_REP] += rep_a;
                    ks[M_PRE] += pre_ratio;
                    ks[M_PCT] += pct;
                    ks[M_ADOPT] += adopt_a;
                    ks[M_ADOPT_C] += adopt_ctrl;
                    ks[M_COVER] += cover_a;
                    ks[M_COVER_C] += cover_ctrl;
                    if sleep_a.is_nan() {
                        ks[M_SLEEP] += 0.0;
                        ks[M_SLEEP_C] += 0.0;
                        ks[M_SLEEPV] += 0.0;
                        ks[M_SLEEPV_C] += 0.0;
                    } else {
                        ks[M_SLEEP] += sleep_a;
                        ks[M_SLEEP_C] += sleep_ctrl.0;
                        ks[M_SLEEPV] += sleepv_a;
                        ks[M_SLEEPV_C] += sleep_ctrl.1;
                    }
                    if prom_a.is_nan() {
                        ks[M_PROM] += 0.0;
                        ks[M_PROM_C] += 0.0;
                        ks[M_PROMV] += 0.0;
                        ks[M_PROMV_C] += 0.0;
                    } else {
                        ks[M_PROM] += prom_a;
                        ks[M_PROM_C] += prom_ctrl.0;
                        ks[M_PROMV] += promv_a;
                        ks[M_PROMV_C] += prom_ctrl.1;
                    }
                    ks[M_AGE] += age_a;
                    ks[M_AGE_C] += age_ctrl;
                    ks[M_ON] += if online[a] { 1.0 } else { 0.0 };
                    k_cnt[j] += 1;
                }
            }
        }

        // 6. 观点交换：在线者先发布，再按轮抽样拉取
        let mut views: Vec<Option<CompressedView>> = vec![None; n];
        for i in 0..n {
            if online[i] {
                if let Some(e) = nodes[i].as_ref() {
                    views[i] = Some(e.export_view(t));
                }
            }
        }
        let mut just = vec![false; n];
        for &i in &just_returned {
            just[i] = true;
        }
        for i in 0..n {
            if !online[i] {
                continue;
            }
            let k = if just[i] { SAMPLES * 3 } else { SAMPLES };
            let reqs = { nodes[i].as_mut().unwrap().tick(t, k) };
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

        // 7. 更新离线时长 / 上次在线轮
        for i in 0..n {
            prev_off[i] = if alive[i] { world.offline_duration(i, t) } else { 0 };
            if online[i] {
                last_on[i] = r;
            }
        }

        if r < rounds {
            world.run_steps(ROUND_LEN as usize);
        }
    }

    let alive400: Vec<usize> = (0..world.accounts.len()).filter(|&i| alive_at(&world, i, STEPS)).collect();
    let off400 = alive400.iter().filter(|&&i| !world.was_online_at(i, STEPS)).count();
    let p_meas = if alive400.is_empty() {
        0.0
    } else {
        off400 as f64 / alive400.len() as f64
    };

    RunOut {
        p_target,
        d,
        p_meas,
        main,
        k_sum,
        k_cnt,
        n_events: events.len(),
        gap_mean: if events.is_empty() {
            0.0
        } else {
            gap_sum / events.len() as f64
        },
    }
}

fn mean_sd(v: &[f64]) -> (f64, f64) {
    let n = v.len() as f64;
    if v.is_empty() {
        return (0.0, 0.0);
    }
    let m = v.iter().sum::<f64>() / n;
    if v.len() < 2 {
        return (m, 0.0);
    }
    let var = v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0);
    (m, var.sqrt())
}

fn check_equiv() {
    println!("== CHECK 前 400h 是否与 steps=400 逐位一致 ==");
    for d in DS {
        let (rate, _, _, _) = calibrate(0.50, d, &SEEDS[..1]);
        let mut a = World::new(sim_cfg(rate, d, SEEDS[0], STEPS as usize, true));
        a.run();
        let mut b = World::new(sim_cfg(rate, d, SEEDS[0], (STEPS + d + MARGIN) as usize, true));
        b.run_steps(STEPS as usize);
        let same = a.accounts.len() == b.accounts.len()
            && a.alive_count() == b.alive_count()
            && a.rating_count() == b.rating_count()
            && a.dormancy_events == b.dormancy_events
            && (0..a.accounts.len())
                .all(|i| a.was_online_at(i, STEPS) == b.was_online_at(i, STEPS));
        println!(
            "CHECK,d={},accounts={}/{},alive={}/{},ratings={}/{},dorm={}/{},same={}",
            d, a.accounts.len(), b.accounts.len(), a.alive_count(), b.alive_count(),
            a.rating_count(), b.rating_count(), a.dormancy_events, b.dormancy_events, same
        );
    }
    println!();
}


/// 收益方 / 潜伏者的存活探测（父任务要求把「存活/已离开」分开报）。
/// 只跑世界，不跑引擎；dormancy_rate 从 l4_result.txt 的 CAL 段读。
fn probe() {
    let txt = std::fs::read_to_string("../../l4_result.txt").unwrap_or_default();
    let mut rates: Vec<(f64, u64, f64)> = Vec::new();
    for ln in txt.lines() {
        if let Some(rest) = ln.strip_prefix("CAL,") {
            let f: Vec<&str> = rest.split(',').collect();
            if f.len() >= 7 {
                if let (Ok(p), Ok(d), Ok(r)) = (f[0].parse::<f64>(), f[1].parse::<u64>(), f[2].parse::<f64>()) {
                    rates.push((p, d, r));
                }
            }
        }
    }
    println!("PROBE,p,grid_d,seed,prom_alive400,prom_alive_end,prom_left_at,sleep_alive400,sleep_alive_end,accounts_end");
    for (p, d, rate) in rates {
        for &seed in &SEEDS {
            let rounds = ((STEPS + d + MARGIN + ROUND_LEN - 1) / ROUND_LEN) as usize;
            let steps_ext = rounds as u64 * ROUND_LEN;
            let mut w = World::new(sim_cfg(rate, d, seed, steps_ext as usize, true));
            let prom = w.accounts().iter().find(|a| matches!(a.kind, Kind::Promoted)).map(|a| a.idx);
            let slp = w.accounts().iter().find(|a| matches!(a.kind, Kind::Sleeper { .. })).map(|a| a.idx);
            let left_at = |w: &World, i: Option<usize>| -> i64 {
                match i {
                    Some(i) => w.life_traces[i]
                        .events()
                        .iter()
                        .find_map(|&(t, l)| if matches!(l, Life::Left { .. }) { Some(t as i64) } else { None })
                        .unwrap_or(-1),
                    None => -1,
                }
            };
            let a400 = |w: &World, i: Option<usize>| -> bool { i.map(|i| alive_at(w, i, STEPS)).unwrap_or(false) };
            let p400 = a400(&w, prom);
            let s400 = a400(&w, slp);
            w.run_steps((steps_ext - STEPS) as usize);
            let pend = prom.map(|i| w.accounts[i].is_alive()).unwrap_or(false);
            let send = slp.map(|i| w.accounts[i].is_alive()).unwrap_or(false);
            println!(
                "PROBE,{:.2},{},{},{},{},{},{},{},{}",
                p, d, seed, p400 as i32, pend as i32, left_at(&w, prom), s400 as i32, send as i32, w.accounts.len()
            );
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let smoke = args.iter().any(|a| a == "--smoke");
    let check = args.iter().any(|a| a == "--check");
    let t0 = std::time::Instant::now();

    if check {
        check_equiv();
        return;
    }
    if args.iter().any(|a| a == "--probe") {
        probe();
        return;
    }

    let cells: Vec<(f64, u64)> = if smoke {
        vec![(0.50, 216)]
    } else {
        PS.iter().flat_map(|&p| DS.iter().map(move |&d| (p, d))).collect()
    };
    let seeds: Vec<u64> = if smoke { vec![SEEDS[0]] } else { SEEDS.to_vec() };

    println!("# L4 潜伏攻击对旧用户自己的影响");
    println!("# 固定 n_initial={} steps={} join_rate=0.05 leave_rate=0.001 attack_ratio=1/{}", N_INIT, STEPS, N_INIT);
    println!("# 引擎 beta=0.3 half_life={} buckets=2048 hashers=5 samples={} 轮长={}h", HALF_LIFE, SAMPLES, ROUND_LEN);
    println!("# return_rate=1.0 sleep_hours=(D,D) enable_rhythm=false seeds={:?}", seeds);

    let mut all: Vec<RunOut> = Vec::new();

    println!("\n== CAL 标定（seed={}）==", seeds[0]);
    println!("CAL,p_target,grid_d,dormancy,p_meas_cal,off,alive");
    let mut rates: Vec<((f64, u64), f64)> = Vec::new();
    for &(p, d) in &cells {
        let (rate, f, off, alive) = calibrate(p, d, &seeds);
        rates.push(((p, d), rate));
        println!("CAL,{:.2},{},{:.6},{:.4},{},{},", p, d, rate, f, off, alive);
    }

    println!("\n== MAIN 主表（t={}）==", STEPS);
    println!("MAIN,p,grid_d,seed,p_meas,alive,online,ret_all,ret_fresh,ctrl,rep_ret_all,rep_ret_fresh,rep_ctrl,ratio_all,ratio_fresh,pct_fresh,pct_ctrl,act_ret,act_ctrl,events,gap_mean,s_mean");
    for &(p, d) in &cells {
        let rate = rates.iter().find(|((pp, dd), _)| *pp == p && *dd == d).unwrap().1;
        for &seed in &seeds {
            let o = run_case(p, d, seed, rate, true);
            let m = o.main;
            println!(
                "MAIN,{:.2},{},{},{:.4},{},{},{},{},{},{:.6},{:.6},{:.6},{:.4},{:.4},{:.4},{:.4},{:.3},{:.3},{},{:.1},{:.4}",
                p, d, seed, o.p_meas, m.alive, m.online, m.n_ret_all, m.n_ret_fresh, m.n_ctrl,
                m.rep_ret_all, m.rep_ret_fresh, m.rep_ctrl,
                if m.rep_ctrl > 0.0 { m.rep_ret_all / m.rep_ctrl } else { 0.0 },
                if m.rep_ctrl > 0.0 { m.rep_ret_fresh / m.rep_ctrl } else { 0.0 },
                m.pct_ret_fresh, m.pct_ctrl, m.act_ret, m.act_ctrl, o.n_events, o.gap_mean, m.s_mean
            );
            for k in 0..NK {
                if o.k_cnt[k] == 0 {
                    continue;
                }
                let c = o.k_cnt[k] as f64;
                println!(
                    "CURVE,{:.2},{},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.4}",
                    p, d, seed, k + 1, o.k_cnt[k],
                    o.k_sum[k][M_RATIO] / c, o.k_sum[k][M_REP] / c, o.k_sum[k][M_PRE] / c,
                    o.k_sum[k][M_PCT] / c, o.k_sum[k][M_ADOPT] / c, o.k_sum[k][M_ADOPT_C] / c,
                    o.k_sum[k][M_COVER] / c, o.k_sum[k][M_COVER_C] / c,
                    o.k_sum[k][M_SLEEP] / c, o.k_sum[k][M_SLEEP_C] / c, o.k_sum[k][M_SLEEPV] / c,
                    o.k_sum[k][M_AGE] / c, o.k_sum[k][M_AGE_C] / c, o.k_sum[k][M_ON] / c
                );
            }
            all.push(o);
        }
        eprintln!("[cell p={:.0}% D={}h] done, elapsed {:.0}s", p * 100.0, d, t0.elapsed().as_secs_f64());
    }

    println!("\n== AGG-MAIN 主表（3 seed 均值±样本标准差）==");
    println!("AGG,p,grid_d,p_meas,alive,ret_all,ret_fresh,ctrl,rep_ret_all,rep_ret_fresh,rep_ctrl,ratio_all,ratio_fresh,pct_fresh,pct_ctrl,act_ret,act_ctrl");
    for &(p, d) in &cells {
        let sub: Vec<&RunOut> = all.iter().filter(|o| o.p_target == p && o.d == d).collect();
        let g = |f: &dyn Fn(&RunOut) -> f64| mean_sd(&sub.iter().map(|o| f(o)).collect::<Vec<_>>());
        let (pm, ps) = g(&|o| o.p_meas);
        let (al, _) = g(&|o| o.main.alive as f64);
        let (ra, _) = g(&|o| o.main.n_ret_all as f64);
        let (rf, _) = g(&|o| o.main.n_ret_fresh as f64);
        let (cc, _) = g(&|o| o.main.n_ctrl as f64);
        let (r1, r1s) = g(&|o| o.main.rep_ret_all);
        let (r2, r2s) = g(&|o| o.main.rep_ret_fresh);
        let (r3, r3s) = g(&|o| o.main.rep_ctrl);
        let (x1, x1s) = g(&|o| if o.main.rep_ctrl > 0.0 { o.main.rep_ret_all / o.main.rep_ctrl } else { 0.0 });
        let (x2, x2s) = g(&|o| if o.main.rep_ctrl > 0.0 { o.main.rep_ret_fresh / o.main.rep_ctrl } else { 0.0 });
        let (q1, q1s) = g(&|o| o.main.pct_ret_fresh);
        let (q2, q2s) = g(&|o| o.main.pct_ctrl);
        let (a1, _) = g(&|o| o.main.act_ret);
        let (a2, _) = g(&|o| o.main.act_ctrl);
        println!(
            "AGG,{:.2},{},{:.4}±{:.4},{:.0},{:.1},{:.1},{:.1},{:.6}±{:.6},{:.6}±{:.6},{:.6}±{:.6},{:.3}±{:.3},{:.3}±{:.3},{:.3}±{:.3},{:.3}±{:.3},{:.2},{:.2}",
            p, d, pm, ps, al, ra, rf, cc, r1, r1s, r2, r2s, r3, r3s, x1, x1s, x2, x2s, q1, q1s, q2, q2s, a1, a2
        );
    }

    for (name, idx) in [
        ("CURVE-RATIO 声望/同期一直在线", M_RATIO),
        ("CURVE-REP 声望绝对值", M_REP),
        ("CURVE-PRE 声望/自己离线前", M_PRE),
        ("CURVE-PCT 声望在全体诚实在线用户中的百分位", M_PCT),
        ("CURVE-ADOPT 观点权重占比（回归者）", M_ADOPT),
        ("CURVE-ADOPT-CTRL 观点权重占比（一直在线）", M_ADOPT_C),
        ("CURVE-COVER 观点被缓存比例（回归者）", M_COVER),
        ("CURVE-COVER-CTRL 观点被缓存比例（一直在线）", M_COVER_C),
        ("CURVE-SLEEP 潜伏者排名百分位（回归者）", M_SLEEP),
        ("CURVE-SLEEP-CTRL 潜伏者排名百分位（一直在线）", M_SLEEP_C),
        ("CURVE-SLEEPV 潜伏者份额/分布中位数（回归者）", M_SLEEPV),
        ("CURVE-SLEEPV-CTRL 潜伏者份额/分布中位数（一直在线）", M_SLEEPV_C),
        ("CURVE-PROM 收益方排名百分位（回归者）", M_PROM),
        ("CURVE-PROM-CTRL 收益方排名百分位（一直在线）", M_PROM_C),
        ("CURVE-PROMV 收益方份额/分布中位数（回归者）", M_PROMV),
        ("CURVE-PROMV-CTRL 收益方份额/分布中位数（一直在线）", M_PROMV_C),
        ("CURVE-AGE 自己观点中位年龄h（回归者）", M_AGE),
        ("CURVE-AGE-CTRL 自己观点中位年龄h（一直在线）", M_AGE_C),
        ("CURVE-ON 回归者该轮在线比例", M_ON),
    ] {
        println!("\n== {} ==", name);
        println!("ROW,p,grid_d,n_events,k1,k2,k3,k4,k5,k6,k7,k8,k9,k10");
        for &(p, d) in &cells {
            let sub: Vec<&RunOut> = all.iter().filter(|o| o.p_target == p && o.d == d).collect();
            let ne = mean_sd(&sub.iter().map(|o| o.n_events as f64).collect::<Vec<_>>());
            let mut row = format!("ROW,{:.2},{},{:.1}", p, d, ne.0);
            for k in 0..NK {
                let vals: Vec<f64> = sub
                    .iter()
                    .map(|o| if o.k_cnt[k] > 0 { o.k_sum[k][idx] / o.k_cnt[k] as f64 } else { f64::NAN })
                    .collect();
                let fin: Vec<f64> = vals.iter().copied().filter(|x| !x.is_nan()).collect();
                let (m, _) = mean_sd(&fin);
                row.push_str(&format!(",{:.4}", m));
            }
            println!("{}", row);
        }
    }

    if !smoke {
        println!("\n== CONTROL 无攻击对照 ==");
        println!("CTRL,p,grid_d,seed,ret_all,ret_fresh,rep_ret_all,rep_ret_fresh,rep_ctrl,ratio_fresh,pct_fresh,events");
        for &(p, d) in &[(0.50f64, 216u64), (0.90, 720)] {
            let rate = rates.iter().find(|((pp, dd), _)| *pp == p && *dd == d).unwrap().1;
            for &seed in &seeds {
                let o = run_case(p, d, seed, rate, false);
                let m = o.main;
                println!(
                    "CTRL,{:.2},{},{},{},{},{:.6},{:.6},{:.6},{:.4},{:.4},{}",
                    p, d, seed, m.n_ret_all, m.n_ret_fresh, m.rep_ret_all, m.rep_ret_fresh, m.rep_ctrl,
                    if m.rep_ctrl > 0.0 { m.rep_ret_fresh / m.rep_ctrl } else { 0.0 },
                    m.pct_ret_fresh, o.n_events
                );
            }
        }
    }

    println!("\n# total {:.0}s", t0.elapsed().as_secs_f64());
}
