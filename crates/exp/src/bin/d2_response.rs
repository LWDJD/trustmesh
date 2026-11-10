//! D2：网络对攻击的反应速度（上升沿 / 下降沿 × 三种衰减模式）
//!
//! 真多 runtime：每个账户一个独立的 tm_engine::Engine（同 l2.rs / l4_olduser.rs）。
//! 世界与引擎按 20h 一轮交替推进：t=0,20,...,600 共 31 个快照。
//!
//! 设计（任务给定）：
//!   steps=600，潜伏者 t=120 突变，t=300 洗白（撤销对收益方的 1.0 并取消关注），
//!   3 种衰减模式 × 3 个 seed = 9 次长仿真。
//!
//! 三种衰减模式（EngineConfig，显式填写）：
//!   std   = { decay_enabled: true,  decay_floor: 0.0 }  // 0.5^(Δt/72)
//!   floor = { decay_enabled: true,  decay_floor: 0.5 }  // max(f, 0.5)
//!   off   = { decay_enabled: false, decay_floor: 0.0 }  // factor 恒为 1
//!
//! 口径（严格三分，绝不共用分母）：
//!   诚实   = 既不是 Sleeper 也不是 Promoted
//!   潜伏者 = Kind::Sleeper
//!   收益方 = Kind::Promoted      // is_attacker() 对它返回 false，必须显式剔除
//!
//! 两个坑（已避开）：
//!   1. 账户编号一律取 world.accounts().len()，绝不用 alive_count()；
//!   2. 收益方单独报「存活 / 已离开 / 在线」。
//!
//! 实验控制 1：收益方（Promoted）全程钉住在线。原因：潜伏者只有和被推广账户同时在线时
//!   才可能给它打分（World::pick_partner 只从「关注且在线」的人里挑），而本网格的休眠
//!   时长 216h 会让收益方有 ~50% 概率在 t=120 恰好睡着，那样「攻击发生」本身就被推迟
//!   0~216h，与衰减模式完全无关。钉住只改 world.accounts[target].life（同步写
//!   life_traces），不动 sim 库任何代码。未钉住时会发生什么，PROBE 段单独报告。
//!
//! 实验控制 2：对照臂（ctl）。同一个 seed、同一个 dormancy、同一个世界构造，只是
//!   normal_until = u64::MAX（潜伏者永不突变）。于是 t≤120 两条臂逐位相同，
//!   之后「攻击臂 − 对照臂」的差（DiD）把收益方自己的世俗漂移（被越来越多诚实账户
//!   打分）与衰减模式无关的部分扣掉。
//!
//! 三条臂（wash = t=300 洗白、dark = t=300 消失、ctl = 永不突变）× 3 模式 × 3 seed
//! = 27 次长仿真。
//!
//! 用法：
//!   d2_response                     全网格（wash/dark/ctl × 3 模式 × 3 seed）+ 标定 + 探针
//!   d2_response pilot               单 seed / 单模式
//!   d2_response sleepers=5          改潜伏者数量
//!   d2_response seeds=11,23,37      改 seed
//!   d2_response modes=std,floor,off 改模式
//!   d2_response arm=wash,dark       只跑指定臂

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, Life, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

// ───────────────────────────── 固定参数 ─────────────────────────────

const N_INIT: usize = 300;
const STEPS: u64 = 600;
const ROUND_LEN: u64 = 20;
const ROUNDS: usize = (STEPS / ROUND_LEN) as usize; // 30
const SNAPS: usize = ROUNDS + 1; // 31
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const SEEDS: [u64; 3] = [11, 23, 37];
const ATTACK_T: u64 = 120;
const WASH_T: u64 = 300;
const SLEEP_H: u64 = 216; // 3 个半衰期：观点年龄最多铺到 3H
const TARGET_OFF: f64 = 0.50; // 标定目标：结束时 50% 离线
const N_SLEEPERS: usize = 10;

const RISE_FRACS: [f64; 3] = [0.25, 0.50, 0.90];
const FALL_FRACS: [f64; 2] = [0.50, 0.20];
// 绝对阈值（Δ = 收益方声望比 − 对照臂声望比）：与峰值无关，便于跨模式比较
const ABS_RISE: [f64; 3] = [0.20, 0.40, 0.60];
const ABS_FALL: [f64; 3] = [0.60, 0.40, 0.20];

// ───────────────────────────── 小工具 ─────────────────────────────

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn mean_sd(v: &[f64]) -> (f64, f64) {
    let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let n = xs.len() as f64;
    let m = xs.iter().sum::<f64>() / n;
    if xs.len() < 2 {
        return (m, 0.0);
    }
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0);
    (m, var.sqrt())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

fn f(x: f64, d: usize) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else {
        format!("{:.*}", d, x)
    }
}

#[derive(Clone, Copy)]
struct Mode {
    name: &'static str,
    decay_enabled: bool,
    decay_floor: f32,
}

const MODES: [Mode; 3] = [
    Mode { name: "std", decay_enabled: true, decay_floor: 0.0 },
    Mode { name: "floor", decay_enabled: true, decay_floor: 0.5 },
    Mode { name: "off", decay_enabled: false, decay_floor: 0.0 },
];

fn ecfg(m: Mode) -> EngineConfig {
    // 六个字段全部显式填写（含本 commit 新增的两个）
    EngineConfig {
        beta: 0.3,
        half_life: HALF_LIFE,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: m.decay_enabled,
        decay_floor: m.decay_floor,
        coverage_exponent: 1.0,
        remove_when_coverage: 0.0,
        group_half_life: 0,
        unreachable_after: 0,
        min_group_sources: 0,
    }
}

fn sim_cfg(seed: u64, n_sleep: usize, dorm: f64, mutate: bool) -> SimConfig {
    SimConfig {
        n_initial: N_INIT,
        steps: STEPS as usize,
        seed,
        join_rate: 0.05,
        leave_rate: 0.0, // 移除流失，避免「收益方中途离开」污染时间曲线
        dormancy_rate: dorm,
        sleep_hours: (SLEEP_H, SLEEP_H),
        return_rate: 1.0,
        enable_rhythm: false, // 在线只由休眠决定：离线时长 == sleep_hours
        attack_kind: AttackKind::Sleeper {
            normal_until: if mutate { ATTACK_T } else { u64::MAX },
        },
        attack_ratio: n_sleep as f64 / N_INIT as f64,
        ..Default::default()
    }
}

fn make_engine(cfg: &EngineConfig, n: usize) -> Engine {
    let mut e = Engine::new(cfg.clone());
    for i in 0..n {
        e.intern(key(i));
    }
    e
}

/// 把收益方钉在 Active（实验控制，见文件头）。
fn pin_online(world: &mut World, idx: usize, t: u64) {
    if !matches!(world.accounts[idx].life, Life::Active) {
        world.accounts[idx].life = Life::Active;
        world.life_traces[idx].record(t, Life::Active);
    }
}

/// 把账户钉成永久休眠（「攻击者消失」臂用：从此不再发布、不可刷新）。
fn pin_offline(world: &mut World, idx: usize, t: u64) {
    if !matches!(world.accounts[idx].life, Life::Dormant { .. }) {
        world.accounts[idx].life = Life::Dormant { until: u64::MAX };
        world.life_traces[idx].record(t, Life::Dormant { until: u64::MAX });
    }
}

/// 三条臂：
///   Wash = 攻击臂，t=300 洗白（换回诚实行为，继续在线发布）
///   Dark = 攻击臂，t=300 消失（永久离线，观点不可再刷新）
///   Ctl  = 对照臂，潜伏者永不突变
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Wash,
    Dark,
    Ctl,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::Wash => "wash",
            Variant::Dark => "dark",
            Variant::Ctl => "ctl",
        }
    }
    fn attacks(self) -> bool {
        !matches!(self, Variant::Ctl)
    }
}

// ───────────────────────────── 世界探针（不带引擎） ─────────────────────────────

#[derive(Default, Clone)]
struct WorldProbe {
    accounts: usize,
    alive: usize,
    online_end: usize,
    off_end: f64,
    off_mean: f64,
    prom_alive: bool,
    prom_online_end: bool,
    prom_first_dorm: i64,
    prom_off: [u64; 3],
    prom_alive_at: [bool; 3],
    sleep_alive: usize,
    sleep_online_end: usize,
    sleep_off_mean: f64,
    sleep_ever_dorm: usize,
}

fn probe_world(seed: u64, n_sleep: usize, dorm: f64, pin: bool) -> WorldProbe {
    let mut w = World::new(sim_cfg(seed, n_sleep, dorm, true));
    let prom = w.attack.target;
    let sleepers: Vec<usize> = w
        .accounts()
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    let mut offs: Vec<f64> = Vec::new();
    let mut sleep_offs: Vec<f64> = Vec::new();
    let mut prom_off = [0u64; 3];
    let mut prom_alive_at = [false; 3];
    let mut first_dorm = -1i64;
    let marks = [ATTACK_T, WASH_T, STEPS];

    for r in 0..=ROUNDS {
        let t = r as u64 * ROUND_LEN;
        if t >= ATTACK_T {
            let alive: Vec<usize> = w.accounts().iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
            let online = alive.iter().filter(|&&i| w.was_online_at(i, t)).count();
            if !alive.is_empty() {
                offs.push((alive.len() - online) as f64 / alive.len() as f64);
            }
            let mut so = 0.0;
            let mut sc = 0usize;
            for &s in &sleepers {
                if w.accounts[s].is_alive() {
                    so += w.offline_duration(s, t) as f64;
                    sc += 1;
                }
            }
            if sc > 0 {
                sleep_offs.push(so / sc as f64);
            }
            for (k, &m) in marks.iter().enumerate() {
                if t == m {
                    if let Some(p) = prom {
                        prom_off[k] = w.offline_duration(p, t);
                        prom_alive_at[k] = w.accounts[p].is_alive();
                    }
                }
            }
        }
        if r < ROUNDS {
            for _ in 0..ROUND_LEN {
                w.step();
                if let Some(p) = prom {
                    if first_dorm < 0 && matches!(w.accounts[p].life, Life::Dormant { .. }) {
                        first_dorm = w.time as i64;
                    }
                    if pin {
                        let now = w.time;
                        pin_online(&mut w, p, now);
                    }
                }
            }
        }
    }

    let alive: Vec<usize> = w.accounts().iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let online_end = alive.iter().filter(|&&i| w.was_online_at(i, STEPS)).count();
    let sleep_alive: Vec<usize> = sleepers.iter().copied().filter(|&s| w.accounts[s].is_alive()).collect();
    let sleep_online_end = sleep_alive.iter().filter(|&&s| w.was_online_at(s, STEPS)).count();
    let sleep_ever_dorm = sleepers
        .iter()
        .filter(|&&s| {
            w.life_traces
                .get(s)
                .map(|tr| tr.events().iter().any(|(_, l)| matches!(l, Life::Dormant { .. })))
                .unwrap_or(false)
        })
        .count();

    WorldProbe {
        accounts: w.accounts.len(),
        alive: alive.len(),
        online_end,
        off_end: if alive.is_empty() { 0.0 } else { (alive.len() - online_end) as f64 / alive.len() as f64 },
        off_mean: mean(&offs),
        prom_alive: prom.map(|p| w.accounts[p].is_alive()).unwrap_or(false),
        prom_online_end: prom.map(|p| w.was_online_at(p, STEPS)).unwrap_or(false),
        prom_first_dorm: first_dorm,
        prom_off,
        prom_alive_at,
        sleep_alive: sleep_alive.len(),
        sleep_online_end,
        sleep_off_mean: mean(&sleep_offs),
        sleep_ever_dorm,
    }
}

/// 标定 dormancy_rate：对数网格粗扫 + 对数二分细化，目标「结束时离线比例」。
fn calibrate(seed: u64, n_sleep: usize) -> (f64, WorldProbe) {
    let l0 = 1e-5f64.ln();
    let l1 = 0.05f64.ln();
    let n = 12usize;
    let mut best: (f64, f64, WorldProbe) = (0.0, f64::INFINITY, WorldProbe::default());
    let mut xs: Vec<(f64, f64)> = Vec::new();
    for i in 0..=n {
        let x = (l0 + (l1 - l0) * i as f64 / n as f64).exp();
        let p = probe_world(seed, n_sleep, x, true);
        let err = (p.off_end - TARGET_OFF).abs();
        if err < best.1 {
            best = (x, err, p.clone());
        }
        xs.push((x, p.off_end));
    }
    let idx = xs.iter().position(|&(x, _)| x == best.0).unwrap_or(0);
    let mut lo = if idx > 0 { xs[idx - 1].0 } else { xs[0].0 * 0.5 };
    let mut hi = if idx + 1 < xs.len() { xs[idx + 1].0 } else { xs[xs.len() - 1].0 * 2.0 };
    for _ in 0..8 {
        let mid = (lo * hi).sqrt();
        let p = probe_world(seed, n_sleep, mid, true);
        let err = (p.off_end - TARGET_OFF).abs();
        if err < best.1 {
            best = (mid, err, p.clone());
        }
        if p.off_end < TARGET_OFF {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (best.0, best.2)
}

// ───────────────────────────── 快照结构 ─────────────────────────────

#[derive(Clone, Default)]
struct Snap {
    t: u64,
    accts: usize,
    alive: usize,
    online: usize,
    obs: usize,
    zero_base: usize,
    base: f64,
    p_share: f64,
    p_ratio: f64,
    p_ratio_med: f64,
    p_group: f64,
    p_group_med: f64,
    p_pct: f64,
    p_top10: f64,
    p_ratio_exp: f64,
    p_ratio_unexp: f64,
    exposed: f64,
    s_ratio: f64,
    s_share: f64,
    s_view: f64,
    s_view_ratio: f64,
    s_alive: usize,
    s_online: usize,
    prom_alive: usize,
    prom_online: usize,
    n_deliv: usize,
    n_raters_prom: usize,
    s_age: f64,
    cache_age: f64,
    cache_n: f64,
    cache_stale: f64,
}

struct RunOut {
    variant: Variant,
    mode: &'static str,
    seed: u64,
    snaps: Vec<Snap>,
    t_deliver: i64,
    n_deliv_max: usize,
    n_deliv_at_wash: usize,
}

// ───────────────────────────── 一次完整运行 ─────────────────────────────

fn run_case(m: Mode, seed: u64, n_sleep: usize, dorm: f64, variant: Variant) -> RunOut {
    let mutate = variant.attacks();
    let mut world = World::new(sim_cfg(seed, n_sleep, dorm, mutate));
    let target = world.attack.target.expect("Sleeper 场景必有 Promoted 收益方");
    let sleepers: Vec<usize> = world
        .accounts()
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();

    let cfg = ecfg(m);
    let mut nodes: Vec<Option<Engine>> = Vec::new();
    let mut interned = 0usize;
    let mut prev_online: Vec<bool> = Vec::new();
    let mut snaps: Vec<Snap> = Vec::with_capacity(SNAPS);
    let mut t_deliver: i64 = -1;
    let mut n_deliv_max = 0usize;
    let mut n_deliv_at_wash = 0usize;

    for r in 0..=ROUNDS {
        let t = r as u64 * ROUND_LEN;
        let n = world.accounts().len();
        nodes.resize_with(n, || None);
        prev_online.resize(n, false);
        let alive: Vec<bool> = world.accounts().iter().map(|a| a.is_alive()).collect();
        let mut online: Vec<bool> = (0..n)
            .map(|i| alive[i] && world.was_online_at(i, t))
            .collect();

        // 1. 引擎：新建 + intern 新编号（编号只增不减，绝不用 alive_count）
        for i in 0..n {
            if alive[i] && nodes[i].is_none() {
                nodes[i] = Some(make_engine(&cfg, n));
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

        // 2. 同步正分清单（本轮 t 时刻世界状态）
        let ratings: Vec<Vec<(usize, f32)>> = world.accounts().iter().map(|a| a.ratings.clone()).collect();
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

        // 3. 快照（观点交换之前）
        let sn = measure(&world, &nodes, t, target, &sleepers, n);
        n_deliv_max = n_deliv_max.max(sn.n_deliv);
        if t == WASH_T {
            n_deliv_at_wash = sn.n_deliv;
        }
        snaps.push(sn);

        // 4. 洗白 / 消失：t=300 快照之后、该轮观点交换之前（对照臂什么都不做）
        if mutate && t == WASH_T {
            for &s in &sleepers {
                // 关键：只把 attack_active 置 false 会被下一小时的 phase_attack 立刻重新拉起
                // （Sleeper{normal_until} 到点后只要 !attack_active 就会再次突变）。
                // 所以连 kind 里的 normal_until 一起推到 u64::MAX：永久洗白。
                world.accounts[s].kind = Kind::Sleeper { normal_until: u64::MAX };
                world.accounts[s].attack_active = false;
                world.accounts[s].ratings.retain(|&(x, _)| x != target);
                world.accounts[s].follows.retain(|&x| x != target);
                if let Some(e) = nodes[s].as_mut() {
                    e.rate(target as u32, 0.0, t);
                }
                if variant == Variant::Dark {
                    pin_offline(&mut world, s, t);
                    online[s] = false;
                    prev_online[s] = false;
                }
            }
        }

        // 5. 观点交换：在线者发布，再按轮抽样拉取；离线不 tick、不发布
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
                prev_online[i] = false;
                continue;
            }
            let returning = !prev_online[i] && r > 0;
            let k = if returning { SAMPLES * 3 } else { SAMPLES };
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
            prev_online[i] = true;
        }

        // 6. 推进世界（逐小时；钉住收益方；记录载荷首次写入时刻）
        if r < ROUNDS {
            for _ in 0..ROUND_LEN {
                world.step();
                let now = world.time;
                pin_online(&mut world, target, now);
                if variant == Variant::Dark && now >= WASH_T {
                    for &s in &sleepers {
                        pin_offline(&mut world, s, now);
                    }
                }
                if mutate && t_deliver < 0 && now >= ATTACK_T && now < WASH_T {
                    if sleepers
                        .iter()
                        .any(|&s| world.accounts[s].rating_of(target) == Some(1.0))
                    {
                        t_deliver = now as i64;
                    }
                }
            }
        }
    }

    RunOut {
        variant,
        mode: m.name,
        seed,
        snaps,
        t_deliver,
        n_deliv_max,
        n_deliv_at_wash,
    }
}

// ───────────────────────────── 快照测量 ─────────────────────────────

fn measure(
    world: &World,
    nodes: &[Option<Engine>],
    t: u64,
    target: usize,
    sleepers: &[usize],
    n: usize,
) -> Snap {
    let alive: Vec<bool> = world.accounts().iter().map(|a| a.is_alive()).collect();
    let online: Vec<bool> = (0..n).map(|i| alive[i] && world.was_online_at(i, t)).collect();
    let is_slp: Vec<bool> = world.accounts().iter().map(|a| matches!(a.kind, Kind::Sleeper { .. })).collect();
    let is_prom: Vec<bool> = world.accounts().iter().map(|a| matches!(a.kind, Kind::Promoted)).collect();
    // 诚实 = 既非潜伏者也非收益方
    let is_hon: Vec<bool> = (0..n).map(|i| !is_slp[i] && !is_prom[i]).collect();

    let hon_alive: Vec<usize> = (0..n).filter(|&i| is_hon[i] && alive[i]).collect();
    let obs: Vec<usize> = hon_alive.iter().copied().filter(|&i| online[i]).collect();
    let s_alive: Vec<usize> = sleepers.iter().copied().filter(|&s| alive[s]).collect();
    let s_online = s_alive.iter().filter(|&&s| online[s]).count();

    let mut p_shares: Vec<f64> = Vec::new();
    let mut p_ratios: Vec<f64> = Vec::new();
    let mut p_pcts: Vec<f64> = Vec::new();
    let mut p_groups: Vec<f64> = Vec::new();
    let mut top10 = 0usize;
    let mut ratios_exp: Vec<f64> = Vec::new();
    let mut ratios_unexp: Vec<f64> = Vec::new();
    let mut s_ratios: Vec<f64> = Vec::new();
    let mut s_shares: Vec<f64> = Vec::new();
    let mut bases: Vec<f64> = Vec::new();
    let mut zero_base = 0usize;
    let mut n_exp = 0usize;
    let mut ages_exp: Vec<f64> = Vec::new();
    let mut cache_age: Vec<f64> = Vec::new();
    let mut cache_n: Vec<f64> = Vec::new();
    let mut cache_stale = 0usize;
    let mut cache_cnt = 0usize;

    let mut v_of = vec![0.0f64; n];
    for &o in &obs {
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        for x in v_of.iter_mut() {
            *x = 0.0;
        }
        let dist = e.trust_distribution(t);
        let mut sum = 0.0f64;
        for &(i, v) in &dist {
            let i = i as usize;
            if i < n {
                v_of[i] = v as f64;
                sum += v as f64;
            }
        }
        if sum <= 1e-12 {
            continue;
        }
        // 诚实基线：全部存活诚实账户（含离线者）的均值
        let mut hs = 0.0f64;
        for &h in &hon_alive {
            hs += v_of[h];
        }
        let base = if hon_alive.is_empty() { 0.0 } else { hs / hon_alive.len() as f64 };
        if base <= 1e-12 {
            zero_base += 1;
            continue;
        }
        bases.push(base);

        // 收益方
        let pv = v_of[target];
        let ratio = pv / base;
        p_shares.push(pv);
        p_ratios.push(ratio);
        // 拆出群体项：query = β·personal + (1−β)·group
        let personal = e.share_of(target as u32).unwrap_or(0.0) as f64;
        let group = ((pv - 0.3 * personal) / 0.7).max(0.0);
        p_groups.push(group / base);
        let better = (0..n).filter(|&i| i != target && v_of[i] > pv).count();
        let pct = if n > 1 { 1.0 - better as f64 / (n - 1) as f64 } else { 0.0 };
        p_pcts.push(pct);
        if pct >= 0.90 {
            top10 += 1;
        }

        // 潜伏者（存活者集合，独立累加器）
        if !s_alive.is_empty() {
            let mut ss = 0.0f64;
            for &s in &s_alive {
                ss += v_of[s];
            }
            let sm = ss / s_alive.len() as f64;
            s_shares.push(sm);
            s_ratios.push(sm / base);
        }

        // 暴露组：持有至少一个潜伏者观点副本
        let mut young: Option<u64> = None;
        for &s in &s_alive {
            if let Some(a) = e.view_age(s as u32, t) {
                young = Some(young.map_or(a, |x| x.min(a)));
            }
        }
        match young {
            Some(a) => {
                n_exp += 1;
                ages_exp.push(a as f64);
                ratios_exp.push(ratio);
            }
            None => ratios_unexp.push(ratio),
        }

        // 观察者自己的缓存新鲜度
        let fs = e.freshness_stats(t);
        if fs.count > 0 {
            cache_age.push(fs.median_age as f64);
            cache_n.push(fs.count as f64);
            cache_cnt += 1;
            if fs.median_age > HALF_LIFE {
                cache_stale += 1;
            }
        }
    }

    let n_deliv = s_alive
        .iter()
        .filter(|&&s| world.accounts[s].rating_of(target) == Some(1.0))
        .count();
    let n_raters_prom = world
        .accounts()
        .iter()
        .filter(|a| a.is_alive() && a.rating_of(target).is_some())
        .count();
    // 潜伏者自己发布的观点里，收益方值多少（洗白后应掉回基线）
    let mut sv = 0.0f64;
    let mut svc = 0usize;
    for &s in &s_alive {
        if let Some(e) = nodes.get(s).and_then(|x| x.as_ref()) {
            sv += e.query(target as u32, t) as f64;
            svc += 1;
        }
    }
    let s_view_mean = if svc > 0 { sv / svc as f64 } else { f64::NAN };

    Snap {
        t,
        accts: n,
        alive: alive.iter().filter(|&&a| a).count(),
        online: online.iter().filter(|&&a| a).count(),
        obs: obs.len(),
        zero_base,
        base: mean(&bases),
        p_share: mean(&p_shares),
        p_ratio: mean(&p_ratios),
        p_ratio_med: median(p_ratios.clone()),
        p_group: mean(&p_groups),
        p_group_med: median(p_groups.clone()),
        p_pct: mean(&p_pcts),
        p_top10: if p_pcts.is_empty() { f64::NAN } else { top10 as f64 / p_pcts.len() as f64 },
        p_ratio_exp: mean(&ratios_exp),
        p_ratio_unexp: mean(&ratios_unexp),
        exposed: if n_exp + ratios_unexp.len() == 0 {
            f64::NAN
        } else {
            n_exp as f64 / (n_exp + ratios_unexp.len()) as f64
        },
        s_ratio: mean(&s_ratios),
        s_share: mean(&s_shares),
        s_view: s_view_mean,
        s_view_ratio: if mean(&bases) > 0.0 { s_view_mean / mean(&bases) } else { f64::NAN },
        s_alive: s_alive.len(),
        s_online,
        prom_alive: if alive[target] { 1 } else { 0 },
        prom_online: if online[target] { 1 } else { 0 },
        n_deliv,
        n_raters_prom,
        s_age: mean(&ages_exp),
        cache_age: mean(&cache_age),
        cache_n: mean(&cache_n),
        cache_stale: if cache_cnt == 0 { f64::NAN } else { cache_stale as f64 / cache_cnt as f64 },
    }
}

// ───────────────────────────── 差中差（DiD） ─────────────────────────────

#[derive(Clone, Default)]
struct Did {
    d_pre: f64,
    d_peak: f64,
    t_peak: u64,
    rise_f: [i64; RISE_FRACS.len()], // 峰值分数的 25/50/90%
    rise_a: [i64; ABS_RISE.len()],    // 绝对 Δ = 0.20/0.40/0.60
    fall_r: [i64; FALL_FRACS.len()],  // 回落到峰值的 50/20%
    fall_a: [i64; ABS_FALL.len()],    // 绝对 Δ = 0.60/0.40/0.20
    d: [f64; 5],                      // t=300/320/400/500/600
    rel: [f64; 5],                    // d[k] / d_peak
    plateau: f64,                     // t=520..600 的 Δ 均值
    resid: f64,                       // plateau / d_peak
    top10: [f64; 4],                  // pre / peak / 500 / 600
    s: [f64; 4],
    exp: [f64; 4],
}

fn did_at(snaps: &[Snap], t: u64, get: &dyn Fn(&Snap) -> f64) -> f64 {
    match snaps.iter().find(|s| s.t == t) {
        Some(s) => get(s),
        None => f64::NAN,
    }
}

/// 持续穿越：x(t) 与 x(t+20) 同时越过阈值。
fn first_cross_did(d: &[(u64, f64)], lo: u64, hi: u64, target: f64, up: bool) -> i64 {
    for i in 0..d.len() {
        let (t, v) = d[i];
        if t < lo || t > hi || !v.is_finite() || !target.is_finite() {
            continue;
        }
        let ok = if up { v >= target } else { v <= target };
        if !ok {
            continue;
        }
        let next = d.iter().find(|(tt, _)| *tt == t + ROUND_LEN).map(|(_, x)| *x);
        let ok2 = match next {
            Some(nv) if nv.is_finite() => {
                if up {
                    nv >= target
                } else {
                    nv <= target
                }
            }
            _ => true,
        };
        if ok2 {
            return t as i64;
        }
    }
    -1
}

fn did(atk: &[Snap], ctl: &[Snap]) -> Did {
    let mut o = Did::default();
    let curve: Vec<(u64, f64)> = atk
        .iter()
        .map(|s| (s.t, s.p_ratio - did_at(ctl, s.t, &|c| c.p_ratio)))
        .collect();
    let pre_ts = [60u64, 80, 100, 120];
    o.d_pre = mean(&pre_ts.iter().map(|&t| did_at(atk, t, &|s| s.p_ratio) - did_at(ctl, t, &|s| s.p_ratio)).collect::<Vec<_>>());
    let mut peak = f64::NEG_INFINITY;
    let mut t_peak = 0u64;
    for &(t, v) in &curve {
        if t > ATTACK_T && t <= WASH_T && v.is_finite() && v > peak {
            peak = v;
            t_peak = t;
        }
    }
    o.d_peak = peak;
    o.t_peak = t_peak;
    for (k, fr) in RISE_FRACS.iter().enumerate() {
        o.rise_f[k] = first_cross_did(&curve, ATTACK_T + ROUND_LEN, WASH_T, o.d_pre + fr * (o.d_peak - o.d_pre), true);
    }
    for (k, th) in ABS_RISE.iter().enumerate() {
        o.rise_a[k] = if *th < o.d_peak {
            first_cross_did(&curve, ATTACK_T + ROUND_LEN, WASH_T, *th, true)
        } else {
            -1
        };
    }
    for (k, fr) in FALL_FRACS.iter().enumerate() {
        o.fall_r[k] = first_cross_did(&curve, WASH_T + ROUND_LEN, STEPS, o.d_pre + fr * (o.d_peak - o.d_pre), false);
    }
    for (k, th) in ABS_FALL.iter().enumerate() {
        o.fall_a[k] = if *th < o.d_peak {
            first_cross_did(&curve, WASH_T + ROUND_LEN, STEPS, *th, false)
        } else {
            -1
        };
    }
    for (k, t) in [300u64, 320, 400, 500, 600].iter().enumerate() {
        o.d[k] = did_at(atk, *t, &|s| s.p_ratio) - did_at(ctl, *t, &|s| s.p_ratio);
        o.rel[k] = if o.d_peak.abs() > 1e-12 { o.d[k] / o.d_peak } else { f64::NAN };
    }
    o.plateau = mean(
        &[520u64, 540, 560, 580, 600]
            .iter()
            .map(|&t| did_at(atk, t, &|s| s.p_ratio) - did_at(ctl, t, &|s| s.p_ratio))
            .collect::<Vec<_>>(),
    );
    o.resid = if o.d_peak.abs() > 1e-12 { o.plateau / o.d_peak } else { f64::NAN };
    for (k, t) in [0u64, 0, 500, 600].iter().enumerate() {
        let tt = if k == 0 { 120 } else if k == 1 { t_peak } else { *t };
        o.top10[k] = did_at(atk, tt, &|s| s.p_top10) - did_at(ctl, tt, &|s| s.p_top10);
        o.s[k] = did_at(atk, tt, &|s| s.s_ratio) - did_at(ctl, tt, &|s| s.s_ratio);
        o.exp[k] = did_at(atk, tt, &|s| s.p_ratio_exp) - did_at(ctl, tt, &|s| s.p_ratio_exp);
    }
    o
}

// ───────────────────────────── 参数解析 ─────────────────────────────

fn parse_list<T: std::str::FromStr>(s: &str) -> Option<Vec<T>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        out.push(part.trim().parse::<T>().ok()?);
    }
    Some(out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pilot = args.iter().any(|a| a == "pilot");
    let n_sleep: usize = args
        .iter()
        .find_map(|a| a.strip_prefix("sleepers=").and_then(|x| x.parse().ok()))
        .unwrap_or(N_SLEEPERS);
    let mut seeds: Vec<u64> = args
        .iter()
        .find_map(|a| a.strip_prefix("seeds=").and_then(parse_list::<u64>))
        .unwrap_or_else(|| SEEDS.to_vec());
    let mut mode_names: Vec<String> = args
        .iter()
        .find_map(|a| a.strip_prefix("modes="))
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_else(|| MODES.iter().map(|m| m.name.to_string()).collect());
    if pilot && !args.iter().any(|a| a.starts_with("seeds=")) {
        seeds = vec![SEEDS[0]];
    }
    if pilot && !args.iter().any(|a| a.starts_with("modes=")) {
        mode_names = vec!["std".to_string()];
    }
    let modes: Vec<Mode> = MODES.iter().copied().filter(|m| mode_names.iter().any(|x| x == m.name)).collect();
    let dorm_override: Option<f64> = args
        .iter()
        .find_map(|a| a.strip_prefix("dorm=").and_then(|x| x.parse().ok()));
    let arm_filter: Vec<Variant> = args
        .iter()
        .find_map(|a| a.strip_prefix("arm="))
        .map(|s| {
            s.split(',')
                .filter_map(|x| match x.trim() {
                    "wash" => Some(Variant::Wash),
                    "dark" => Some(Variant::Dark),
                    "ctl" => Some(Variant::Ctl),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_else(|| vec![Variant::Wash, Variant::Dark, Variant::Ctl]);

    let t0 = std::time::Instant::now();

    println!("# D2 网络对攻击的反应速度（上升沿 / 下降沿 × 三种衰减模式）");
    println!(
        "# n_initial={} steps={} 轮长={}h 轮数={} samples={} half_life={}h sleep_hours=({},{}) return_rate=1.0",
        N_INIT, STEPS, ROUND_LEN, ROUNDS, SAMPLES, HALF_LIFE, SLEEP_H, SLEEP_H
    );
    println!(
        "# join_rate=0.05 leave_rate=0 attack_kind=Sleeper{{normal_until:{}}} attack_ratio={}/{} 潜伏者={}",
        ATTACK_T, n_sleep, N_INIT, n_sleep
    );
    println!("# 时间线：t=120 突变 → t=300 洗白（撤销 1.0 并取消关注）→ t=600 结束；快照每 20h 一次");
    println!("# 观察者 = 存活 ∧ 诚实 ∧ 该轮在线；诚实 = 非 Sleeper 且非 Promoted");
    println!("# 实验控制：收益方全程钉住在线（见文件头）；对照臂 normal_until=u64::MAX（潜伏者永不突变）");
    println!("# modes={:?} seeds={:?}", modes.iter().map(|m| m.name).collect::<Vec<_>>(), seeds);

    // 1. 衰减因子自检
    println!("\n== DECAY 三种模式的因子自检（Engine::decay_factor）==");
    println!("DECAY,mode,enabled,floor,dt0,dt24,dt72,dt144,dt216,dt432");
    for m in MODES {
        let e = Engine::new(ecfg(m));
        let v: Vec<String> = [0u64, 24, 72, 144, 216, 432]
            .iter()
            .map(|&dt| format!("{:.6}", e.decay_factor(dt)))
            .collect();
        println!(
            "DECAY,{},{},{},{}",
            m.name,
            if m.decay_enabled { 1 } else { 0 },
            f(m.decay_floor as f64, 2),
            v.join(",")
        );
    }

    // 2. 标定 dormancy（每 seed 一次，三种模式共用同一个世界构造）
    println!("\n== CAL 标定（目标：结束时离线比例 {:.2}）==", TARGET_OFF);
    println!("CAL,seed,n_sleepers,dormancy,off_end,off_mean,accounts,alive,online_end,prom_alive,prom_online_end");
    let mut dorms: Vec<(u64, f64)> = Vec::new();
    for &seed in &seeds {
        let (dorm, p) = calibrate(seed, n_sleep);
        let d = dorm_override.unwrap_or(dorm);
        let p = if dorm_override.is_some() { probe_world(seed, n_sleep, d, true) } else { p };
        dorms.push((seed, d));
        println!(
            "CAL,{},{},{:.6},{:.4},{:.4},{},{},{},{},{}",
            seed, n_sleep, d, p.off_end, p.off_mean, p.accounts, p.alive, p.online_end,
            p.prom_alive as i32, p.prom_online_end as i32
        );
    }

    // 3. 探针：未钉住时收益方会怎样（说明为什么要做这个控制）
    println!("\n== PROBE 未钉住收益方时的世界（同 seed / 同 dormancy，纯世界，不带引擎）==");
    println!("PROBE,seed,accounts,alive,off_end,prom_first_dorm,prom_off120,prom_off300,prom_off600,prom_alive120,prom_alive300,prom_alive600,sleep_alive,sleep_online_end,sleep_off_mean,sleep_ever_dorm");
    for &(seed, dorm) in &dorms {
        let p = probe_world(seed, n_sleep, dorm, false);
        println!(
            "PROBE,{},{},{},{:.4},{},{},{},{},{},{},{},{},{},{:.1},{}",
            seed, p.accounts, p.alive, p.off_end, p.prom_first_dorm,
            p.prom_off[0], p.prom_off[1], p.prom_off[2],
            p.prom_alive_at[0] as i32, p.prom_alive_at[1] as i32, p.prom_alive_at[2] as i32,
            p.sleep_alive, p.sleep_online_end, p.sleep_off_mean, p.sleep_ever_dorm
        );
    }

    // 4. 主网格：wash / dark / ctl（每 seed 三种模式共用同一个世界构造）
    println!("\n== SNAP 逐 seed 逐快照（原始数字；arm=wash/dark/ctl）==");
    println!("#HEAD,arm,mode,seed,t,accts,alive,online,obs,zero_base,base,p_share,p_ratio,p_ratio_med,p_group,p_group_med,p_pct,p_top10,p_ratio_exp,p_ratio_unexp,exposed,s_ratio,s_share,s_view,s_view_ratio,s_alive,s_online,prom_alive,prom_online,n_deliv,n_raters_prom,s_age,cache_age,cache_n,cache_stale");
    let mut runs: Vec<RunOut> = Vec::new();
    for &seed in &seeds {
        let dorm = dorms.iter().find(|(s, _)| *s == seed).unwrap().1;
        for &m in &modes {
            for &v in &arm_filter {
                let o = run_case(m, seed, n_sleep, dorm, v);
                eprintln!(
                    "[{} seed={} arm={}] t_deliver={} n_deliv@300={} elapsed {:.0}s",
                    m.name,
                    seed,
                    o.variant.name(),
                    o.t_deliver,
                    o.n_deliv_at_wash,
                    t0.elapsed().as_secs_f64()
                );
                for s in &o.snaps {
                    let fields: Vec<String> = vec![
                        "SNAP".into(),
                        o.variant.name().into(),
                        m.name.into(),
                        seed.to_string(),
                        s.t.to_string(),
                        s.accts.to_string(),
                        s.alive.to_string(),
                        s.online.to_string(),
                        s.obs.to_string(),
                        s.zero_base.to_string(),
                        f(s.base, 8),
                        f(s.p_share, 8),
                        f(s.p_ratio, 6),
                        f(s.p_ratio_med, 6),
                        f(s.p_group, 6),
                        f(s.p_group_med, 6),
                        f(s.p_pct, 6),
                        f(s.p_top10, 6),
                        f(s.p_ratio_exp, 6),
                        f(s.p_ratio_unexp, 6),
                        f(s.exposed, 4),
                        f(s.s_ratio, 6),
                        f(s.s_share, 8),
                        f(s.s_view, 8),
                        f(s.s_view_ratio, 6),
                        s.s_alive.to_string(),
                        s.s_online.to_string(),
                        s.prom_alive.to_string(),
                        s.prom_online.to_string(),
                        s.n_deliv.to_string(),
                        s.n_raters_prom.to_string(),
                        f(s.s_age, 1),
                        f(s.cache_age, 2),
                        f(s.cache_n, 2),
                        f(s.cache_stale, 4),
                    ];
                    println!("{}", fields.join(","));
                }
                runs.push(o);
            }
        }
    }

    let get = |mode: &str, seed: u64, v: Variant| -> Option<&RunOut> {
        runs.iter().find(|r| r.mode == mode && r.seed == seed && r.variant == v)
    };

    // 5. 攻击臂原始曲线（不做差中差）
    println!("\n== EDGE 攻击臂原始曲线边沿（t=小时；-1 表示窗口内未穿越）==");
    println!("#HEAD,arm,mode,seed,t_deliver,n_deliv_max,n_deliv_at_wash,pre,peak,t_peak,rise50,rise90,fall50,fall90,r300,r320,r400,r500,r600,pre_top10,peak_top10,post_top10,pre_s,peak_s,post_s");
    for o in runs.iter().filter(|r| r.variant != Variant::Ctl) {
        let snaps = &o.snaps;
        let pre = mean(&[60u64, 80, 100, 120].iter().map(|&t| did_at(snaps, t, &|s| s.p_ratio)).collect::<Vec<_>>());
        let mut peak = f64::NEG_INFINITY;
        let mut t_peak = 0u64;
        for s in snaps.iter().filter(|s| s.t > ATTACK_T && s.t <= WASH_T) {
            if s.p_ratio.is_finite() && s.p_ratio > peak {
                peak = s.p_ratio;
                t_peak = s.t;
            }
        }
        let curve: Vec<(u64, f64)> = snaps.iter().map(|s| (s.t, s.p_ratio)).collect();
        let rise50 = first_cross_did(&curve, ATTACK_T + ROUND_LEN, WASH_T, pre + 0.5 * (peak - pre), true);
        let rise90 = first_cross_did(&curve, ATTACK_T + ROUND_LEN, WASH_T, pre + 0.9 * (peak - pre), true);
        let fall50 = first_cross_did(&curve, WASH_T + ROUND_LEN, STEPS, pre + 0.5 * (peak - pre), false);
        let fall90 = first_cross_did(&curve, WASH_T + ROUND_LEN, STEPS, pre + 0.1 * (peak - pre), false);
        let g = |t: u64, ff: &dyn Fn(&Snap) -> f64| did_at(snaps, t, ff);
        let fields: Vec<String> = vec![
            "EDGE".into(),
            o.variant.name().into(),
            o.mode.into(),
            o.seed.to_string(),
            o.t_deliver.to_string(),
            o.n_deliv_max.to_string(),
            o.n_deliv_at_wash.to_string(),
            f(pre, 6),
            f(peak, 6),
            t_peak.to_string(),
            rise50.to_string(),
            rise90.to_string(),
            fall50.to_string(),
            fall90.to_string(),
            f(g(300, &|s| s.p_ratio), 6),
            f(g(320, &|s| s.p_ratio), 6),
            f(g(400, &|s| s.p_ratio), 6),
            f(g(500, &|s| s.p_ratio), 6),
            f(g(600, &|s| s.p_ratio), 6),
            f(mean(&[60u64, 80, 100, 120].iter().map(|&t| g(t, &|s| s.p_top10)).collect::<Vec<_>>()), 6),
            f(g(t_peak, &|s| s.p_top10), 6),
            f(mean(&[500u64, 600].iter().map(|&t| g(t, &|s| s.p_top10)).collect::<Vec<_>>()), 6),
            f(mean(&[60u64, 80, 100, 120].iter().map(|&t| g(t, &|s| s.s_ratio)).collect::<Vec<_>>()), 6),
            f(g(t_peak, &|s| s.s_ratio), 6),
            f(mean(&[500u64, 600].iter().map(|&t| g(t, &|s| s.s_ratio)).collect::<Vec<_>>()), 6),
        ];
        println!("{}", fields.join(","));
    }

    // 6. DiD 逐 seed
    println!("\n== DID 差中差（攻击臂 − 对照臂；t≤120 应恒为 0）==");
    println!("#HEAD,arm,mode,seed,d_pre,d_peak,t_peak,rise_f25,rise_f50,rise_f90,rise_a20,rise_a40,rise_a60,fall_r50,fall_r20,fall_a60,fall_a40,fall_a20,d300,d320,d400,d500,d600,rel300,rel320,rel400,rel500,rel600,plateau,resid,dtop10_pre,dtop10_peak,dtop10_500,dtop10_600,ds_pre,ds_peak,ds_500,ds_600,dexp_pre,dexp_peak,dexp_500,dexp_600");
    let mut dids: Vec<(Variant, &'static str, u64, Did, f64)> = Vec::new();
    for o in runs.iter().filter(|r| r.variant != Variant::Ctl) {
        let c = match get(o.mode, o.seed, Variant::Ctl) {
            Some(c) => c,
            None => continue,
        };
        let d = did(&o.snaps, &c.snaps);
        let max_diff = o
            .snaps
            .iter()
            .filter(|s| s.t <= ATTACK_T)
            .map(|s| (s.p_ratio - did_at(&c.snaps, s.t, &|x| x.p_ratio)).abs())
            .fold(0.0f64, f64::max);
        dids.push((o.variant, o.mode, o.seed, d.clone(), max_diff));
        let fields: Vec<String> = vec![
            "DID".into(),
            o.variant.name().into(),
            o.mode.into(),
            o.seed.to_string(),
            f(d.d_pre, 6),
            f(d.d_peak, 6),
            d.t_peak.to_string(),
            d.rise_f[0].to_string(),
            d.rise_f[1].to_string(),
            d.rise_f[2].to_string(),
            d.rise_a[0].to_string(),
            d.rise_a[1].to_string(),
            d.rise_a[2].to_string(),
            d.fall_r[0].to_string(),
            d.fall_r[1].to_string(),
            d.fall_a[0].to_string(),
            d.fall_a[1].to_string(),
            d.fall_a[2].to_string(),
            f(d.d[0], 6),
            f(d.d[1], 6),
            f(d.d[2], 6),
            f(d.d[3], 6),
            f(d.d[4], 6),
            f(d.rel[0], 4),
            f(d.rel[1], 4),
            f(d.rel[2], 4),
            f(d.rel[3], 4),
            f(d.rel[4], 4),
            f(d.plateau, 6),
            f(d.resid, 4),
            f(d.top10[0], 6),
            f(d.top10[1], 6),
            f(d.top10[2], 6),
            f(d.top10[3], 6),
            f(d.s[0], 6),
            f(d.s[1], 6),
            f(d.s[2], 6),
            f(d.s[3], 6),
            f(d.exp[0], 6),
            f(d.exp[1], 6),
            f(d.exp[2], 6),
            f(d.exp[3], 6),
        ];
        println!("{}", fields.join(","));
    }

    println!("\n== VERIFY 攻击臂与对照臂在 t≤120 的最大 |Δratio|（应为 0）==");
    for (v, mode, seed, _, md) in &dids {
        println!("VERIFY,{},{},{},{:.12}", v.name(), mode, seed, md);
    }

    // 7. DiD 曲线聚合
    println!("\n== DIDAGG 差中差的曲线（3 seed 均值±sd）==");
    println!("DIDAGG,arm,mode,t,n_deliv,d_ratio,d_group,d_ratio_exp,d_ratio_unexp,d_top10,d_pct,d_s_ratio,d_s_view_ratio,d_share");
    for v in [Variant::Wash, Variant::Dark] {
        for &m in &modes {
            for si in 0..SNAPS {
                let mut vals: Vec<Vec<f64>> = vec![Vec::new(); 10];
                for &seed in &seeds {
                    let (a, c) = match (get(m.name, seed, v), get(m.name, seed, Variant::Ctl)) {
                        (Some(a), Some(c)) => (a, c),
                        _ => continue,
                    };
                    let sa = &a.snaps[si];
                    let sc = &c.snaps[si];
                    vals[0].push(sa.p_ratio - sc.p_ratio);
                    vals[1].push(sa.p_group - sc.p_group);
                    vals[2].push(sa.p_ratio_exp - sc.p_ratio_exp);
                    vals[3].push(sa.p_ratio_unexp - sc.p_ratio_unexp);
                    vals[4].push(sa.p_top10 - sc.p_top10);
                    vals[5].push(sa.p_pct - sc.p_pct);
                    vals[6].push(sa.s_ratio - sc.s_ratio);
                    vals[7].push(sa.s_view_ratio - sc.s_view_ratio);
                    vals[8].push(sa.p_share - sc.p_share);
                    vals[9].push(sa.n_deliv as f64);
                }
                let (md, mds) = mean_sd(&vals[9]);
                let mut row = format!("DIDAGG,{},{},{}", v.name(), m.name, si as u64 * ROUND_LEN);
                row.push_str(&format!(",{}±{}", f(md, 1), f(mds, 1)));
                for k in 0..9 {
                    let (mu, sd) = mean_sd(&vals[k]);
                    row.push_str(&format!(",{}±{}", f(mu, 4), f(sd, 4)));
                }
                println!("{}", row);
            }
        }
    }

    // 8. 快照聚合（三条臂分开）
    println!("\n== AGG 快照表（3 seed 均值±sd）==");
    let cols: [(&str, fn(&Snap) -> f64); 14] = [
        ("base", |s| s.base),
        ("p_share", |s| s.p_share),
        ("p_ratio", |s| s.p_ratio),
        ("p_ratio_med", |s| s.p_ratio_med),
        ("p_group", |s| s.p_group),
        ("p_pct", |s| s.p_pct),
        ("p_top10", |s| s.p_top10),
        ("p_ratio_exp", |s| s.p_ratio_exp),
        ("p_ratio_unexp", |s| s.p_ratio_unexp),
        ("exposed", |s| s.exposed),
        ("s_ratio", |s| s.s_ratio),
        ("s_view_ratio", |s| s.s_view_ratio),
        ("cache_age", |s| s.cache_age),
        ("cache_stale", |s| s.cache_stale),
    ];
    {
        let mut head = String::from("AGG,arm,mode,t,obs");
        for (name, _) in cols {
            head.push_str(&format!(",{}", name));
        }
        println!("{}", head);
    }
    for v in [Variant::Wash, Variant::Dark, Variant::Ctl] {
        for &m in &modes {
            let sub: Vec<&RunOut> = runs.iter().filter(|o| o.mode == m.name && o.variant == v).collect();
            if sub.is_empty() {
                continue;
            }
            for si in 0..SNAPS {
                let t = si as u64 * ROUND_LEN;
                let obs_vals: Vec<f64> = sub.iter().map(|o| o.snaps[si].obs as f64).collect();
                let mut row = format!("AGG,{},{},{},{}", v.name(), m.name, t, f(mean(&obs_vals), 1));
                for (_, getv) in cols {
                    let vals: Vec<f64> = sub.iter().map(|o| getv(&o.snaps[si])).collect();
                    let (mu, sd) = mean_sd(&vals);
                    row.push_str(&format!(",{}±{}", f(mu, 4), f(sd, 4)));
                }
                println!("{}", row);
            }
        }
    }

    // 9. DiD 边沿聚合
    println!("\n== DIDEDGE 边沿聚合（3 seed 均值±sd；小时，-1=未穿越）==");
    println!("DIDEDGE,arm,mode,metric,mean,sd");
    let metrics: [(&str, fn(&Did) -> f64); 22] = [
        ("d_peak", |d| d.d_peak),
        ("t_peak", |d| d.t_peak as f64),
        ("rise_f25", |d| d.rise_f[0] as f64),
        ("rise_f50", |d| d.rise_f[1] as f64),
        ("rise_f90", |d| d.rise_f[2] as f64),
        ("rise_a20", |d| d.rise_a[0] as f64),
        ("rise_a40", |d| d.rise_a[1] as f64),
        ("rise_a60", |d| d.rise_a[2] as f64),
        ("fall_r50", |d| d.fall_r[0] as f64),
        ("fall_r20", |d| d.fall_r[1] as f64),
        ("fall_a60", |d| d.fall_a[0] as f64),
        ("fall_a40", |d| d.fall_a[1] as f64),
        ("fall_a20", |d| d.fall_a[2] as f64),
        ("d320", |d| d.d[1]),
        ("d400", |d| d.d[2]),
        ("d500", |d| d.d[3]),
        ("d600", |d| d.d[4]),
        ("rel320", |d| d.rel[1]),
        ("rel400", |d| d.rel[2]),
        ("rel600", |d| d.rel[4]),
        ("resid", |d| d.resid),
        ("dexp_peak", |d| d.exp[1]),
    ];
    for v in [Variant::Wash, Variant::Dark] {
        for &m in &modes {
            let sub: Vec<&Did> = dids
                .iter()
                .filter(|(vv, mm, _, _, _)| *vv == v && *mm == m.name)
                .map(|(_, _, _, d, _)| d)
                .collect();
            for (name, getv) in metrics {
                let vals: Vec<f64> = sub.iter().map(|d| getv(d)).collect();
                let (mu, sd) = mean_sd(&vals);
                println!("DIDEDGE,{},{},{},{:.4},{:.4}", v.name(), m.name, name, mu, sd);
            }
        }
    }

    // 10. 配对差（同 seed 下 floor/off 相对 std）
    println!("\n== PAIRED 同 seed 配对差（模式 − std；小时；正数=更慢/更高）==");
    println!("PAIRED,arm,seed,metric,floor_minus_std,off_minus_std");
    let pairs: [(&str, fn(&Did) -> f64); 11] = [
        ("rise_f50", |d| d.rise_f[1] as f64),
        ("rise_f90", |d| d.rise_f[2] as f64),
        ("fall_r50", |d| d.fall_r[0] as f64),
        ("d_peak", |d| d.d_peak),
        ("d320", |d| d.d[1]),
        ("d400", |d| d.d[2]),
        ("d600", |d| d.d[4]),
        ("rel320", |d| d.rel[1]),
        ("rel600", |d| d.rel[4]),
        ("resid", |d| d.resid),
        ("dexp_peak", |d| d.exp[1]),
    ];
    for v in [Variant::Wash, Variant::Dark] {
        for &seed in &seeds {
            let pick = |m: &str| -> Option<Did> {
                dids.iter()
                    .find(|(vv, mm, ss, _, _)| *vv == v && *mm == m && *ss == seed)
                    .map(|(_, _, _, d, _)| d.clone())
            };
            let (std, flo, off) = (pick("std"), pick("floor"), pick("off"));
            for (name, getv) in pairs {
                let d = |a: &Option<Did>, b: &Option<Did>| -> f64 {
                    match (a, b) {
                        (Some(x), Some(y)) => getv(x) - getv(y),
                        _ => f64::NAN,
                    }
                };
                println!("PAIRED,{},{},{},{:.2},{:.2}", v.name(), seed, name, d(&flo, &std), d(&off, &std));
            }
        }
    }

    println!("\n# total {:.0}s", t0.elapsed().as_secs_f64());
}
