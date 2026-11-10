//! A5 · §4.9「两级衰减」判决实验（全部跑在真 Engine 上）
//!
//! 三个子命令：
//! * diag   主实验：长期离线者回归后，逐轮刷新观点时 live 集合的规模与组成。
//! * effect 效果实验：同一世界配对跑 Off（全部关闭，= 当前生产语义）与 G
//!          （T_remove=168、H_group=120、α=1、θ_remove=0.5、N_min=3），
//!          测回归者判断偏差（规格 4.9.7 的验收线 +10 百分位）、S3 比值、覆盖率分桶的排序质量。
//! * s3     S3 复核：渗透 / 背书 / 潜伏三种攻击在真 Engine 上的「收益方比值」。
//!
//! 口径（与 L4 对齐，便于对表）：
//! 轮长 20h、每轮抽样 12（回归当轮 36）、β=0.3、H_source=72h、Count Sketch 2048×5。
//! 忠实度：每个账户一个独立 Engine，离线者不 tick、不取观点；观点年龄按轮真实增长。
//!
//! 输出全部是「前缀,列,列,...」的 UTF-8 行，方便脚本汇总；不写文件，不依赖外部 crate。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

const ROUND_LEN: u64 = 20;
const SAMPLES: usize = 12;
const HALF_LIFE: u64 = 72;
const T_REMOVE: u64 = 168;
const H_GROUP: u64 = 120;
const MARGIN: u64 = 240; // 回归后至少跟踪 12 轮
const NK: usize = 10; // 第 0 轮 .. 第 9 轮（k=0 是回归当轮、刷新前）
const CTRL_CAP: usize = 48;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// 生产语义：只有来源级衰减（§4.9 已删除，判决见 审计/A5-group.md）
    Off,
}

impl Arm {
    fn tag(self) -> &'static str {
        match self {
            Arm::Off => "off",
        }
    }
}

fn ecfg(_arm: Arm) -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: HALF_LIFE,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
    }
}

fn arms_of(_more: bool) -> Vec<Arm> {
    vec![Arm::Off]
}

// ─────────────────────────── 世界与轮 ───────────────────────────

#[derive(Clone, Copy, Debug)]
struct WorldCfg {
    p: f64,
    dormancy: f64,
    d: u64,
    seed: u64,
    steps: u64,
    n_initial: usize,
    attack_ratio: f64,
    attack: AttackKind,
    /// 把攻击者视为「永远在线」（可达性对抗变体；false = 用仿真原始在线状态）
    force_att_online: bool,
}

fn sim_cfg(wc: &WorldCfg, steps_ext: u64) -> SimConfig {
    SimConfig {
        n_initial: wc.n_initial,
        steps: steps_ext as usize,
        seed: wc.seed,
        join_rate: 0.05,
        leave_rate: 0.001,
        dormancy_rate: wc.dormancy,
        sleep_hours: (wc.d, wc.d),
        return_rate: 1.0,
        enable_rhythm: false,
        attack_kind: wc.attack,
        attack_ratio: wc.attack_ratio,
        ..Default::default()
    }
}

// ─────────────────────────── 记录结构 ───────────────────────────

#[derive(Clone, Copy, Debug, Default)]
struct LiveStat {
    n_pos: usize,
    live: usize,
    live_slp: usize,
    live_att: usize,
    pos_slp: usize,
    pos_att: usize,
    avg_age: f64,
    cov: f64,
    s: f64,
    age_med: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct EvRec {
    obs: usize,
    r0: usize,
    k: usize,
    pct_s: f64,
    pct_m: f64,
    share_prom: f64,
    hon_mean: f64,
    rho: f64,
    support: usize,
    ls: LiveStat,
    online: bool,
    /// 该观察者眼里「女巫账户」的平均份额（S1 用；只在 k=999 行填）
    syb: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct RoundRec {
    r: usize,
    n_h: usize,
    ctrl_n: usize,
    ctrl_pct_s: f64,
    ctrl_pct_m: f64,
    ctrl_ratio: f64,
    s_p50: f64,
    cov_p50: f64,
    pop_slp_alive: f64,
    pop_att_alive: f64,
    pop_slp_on: f64,
    pop_att_on: f64,
    n_alive: usize,
    n_online: usize,
}

#[derive(Debug, Default)]
struct SimOut {
    ev: Vec<EvRec>,
    rounds: Vec<RoundRec>,
    n_events: usize,
    secs: f64,
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

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

fn row_of(e: &Engine, n: usize, t: u64) -> Vec<f32> {
    let mut row = vec![0.0f32; n];
    let mut sum = 0.0f32;
    for u in 0..n {
        let v = e.query(u as u32, t);
        row[u] = v;
        sum += v;
    }
    if sum > 0.0 {
        let inv = 1.0 / sum;
        for v in row.iter_mut() {
            *v *= inv;
        }
    }
    row
}

/// 百分位：strict = L4 口径（并列按「严格大于」计数，全部并列时返回 1.0）
/// mid = 平均秩口径（并列记 0.5，全部并列时返回 0.5）
fn pct_of(row: &[f32], hset: &[usize], x: usize) -> (f64, f64) {
    if hset.len() < 2 {
        return (0.0, 0.0);
    }
    let v = row[x];
    let (mut better, mut below, mut ties) = (0usize, 0usize, 0usize);
    for &u in hset {
        if u == x {
            continue;
        }
        if row[u] > v {
            better += 1;
        } else if row[u] < v {
            below += 1;
        } else {
            ties += 1;
        }
    }
    let den = (hset.len() - 1) as f64;
    let strict = 1.0 - better as f64 / den;
    let mid = (below as f64 + 0.5 * ties as f64 + 0.5) / hset.len() as f64;
    (strict, mid)
}

/// 平均秩
fn ranks(vals: &[f64]) -> Vec<f64> {
    let n = vals.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| vals[a].partial_cmp(&vals[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i + 1;
        while j < n && (vals[idx[j]] - vals[idx[i]]).abs() < 1e-12 {
            j += 1;
        }
        let r = (i + j - 1) as f64 / 2.0 + 1.0;
        for t in i..j {
            out[idx[t]] = r;
        }
        i = j;
    }
    out
}

fn spearman(row: &[f32], quality: &[f32], n: usize) -> (f64, usize) {
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for u in 0..n {
        let v = row[u];
        if v > 0.0 {
            xs.push(v as f64);
            ys.push(quality[u] as f64);
        }
    }
    let m = xs.len();
    if m < 3 {
        return (0.0, m);
    }
    let rx = ranks(&xs);
    let ry = ranks(&ys);
    let mx = rx.iter().sum::<f64>() / m as f64;
    let my = ry.iter().sum::<f64>() / m as f64;
    let mut num = 0.0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    for i in 0..m {
        num += (rx[i] - mx) * (ry[i] - my);
        dx += (rx[i] - mx) * (rx[i] - mx);
        dy += (ry[i] - my) * (ry[i] - my);
    }
    if dx <= 0.0 || dy <= 0.0 {
        return (0.0, m);
    }
    (num / (dx.sqrt() * dy.sqrt()), m)
}

fn live_stat(e: &Engine, t: u64, is_att: &[bool], is_slp: &[bool]) -> LiveStat {
    let shares = e.shares();
    let mut ls = LiveStat { n_pos: shares.len(), ..LiveStat::default() };
    let mut w_sum = 0.0f64;
    let mut age_sum = 0.0f64;
    let mut ages: Vec<f64> = Vec::new();
    for (idx, sh) in &shares {
        let i = *idx as usize;
        if is_att[i] {
            ls.pos_att += 1;
        }
        if is_slp[i] {
            ls.pos_slp += 1;
        }
        if let Some(age) = e.view_age(*idx, t) {
            ages.push(age as f64);
            if age < T_REMOVE {
                ls.live += 1;
                if is_slp[i] {
                    ls.live_slp += 1;
                }
                if is_att[i] {
                    ls.live_att += 1;
                }
                w_sum += *sh as f64;
                age_sum += *sh as f64 * age as f64;
            }
        }
    }
    ls.avg_age = if w_sum > 0.0 { age_sum / w_sum } else { 0.0 };
    ls.cov = if ls.n_pos > 0 { ls.live as f64 / ls.n_pos as f64 } else { 0.0 };
    ls.s = 1.0; // §4.9 删除后不存在群体级系数（列位保留，便于与历史数据对齐）
    ls.age_med = median(&mut ages);
    ls
}

fn stride_sample_idx(v: &[(usize, usize)], cap: usize) -> Vec<(usize, usize)> {
    if v.len() <= cap {
        return v.to_vec();
    }
    let step = (v.len() + cap - 1) / cap;
    v.iter().copied().step_by(step).take(cap).collect()
}

fn stride_sample(v: &[usize], cap: usize) -> Vec<usize> {
    if v.len() <= cap {
        return v.to_vec();
    }
    let step = (v.len() + cap - 1) / cap;
    v.iter().copied().step_by(step).take(cap).collect()
}

// ─────────────────────────── 主循环 ───────────────────────────

fn simulate(wc: &WorldCfg, arm: Arm) -> SimOut {
    let t0 = std::time::Instant::now();
    let rounds = ((wc.steps + wc.d + MARGIN + ROUND_LEN - 1) / ROUND_LEN) as usize;
    let steps_ext = rounds as u64 * ROUND_LEN;
    let mut world = World::new(sim_cfg(wc, steps_ext));
    let mut out = SimOut::default();
    let mut nodes: Vec<Option<Engine>> = Vec::new();
    let mut interned = 0usize;
    let mut prev_off: Vec<u64> = Vec::new();
    let mut ever_gap: Vec<bool> = Vec::new();
    let mut events: Vec<(usize, usize, u64)> = Vec::new();
    let g_min = (wc.d * 8 / 10).max(1);

    for r in 0..=rounds {
        let t = r as u64 * ROUND_LEN;
        let n = world.accounts().len();
        if nodes.len() < n {
            nodes.resize_with(n, || None);
            prev_off.resize(n, 0);
            ever_gap.resize(n, false);
        }
        let alive: Vec<bool> = world.accounts().iter().map(|a| a.is_alive()).collect();
        let kind: Vec<Kind> = world.accounts().iter().map(|a| a.kind).collect();
        let is_att: Vec<bool> = kind.iter().map(|k| k.is_attacker()).collect();
        let is_slp: Vec<bool> = kind.iter().map(|k| matches!(k, Kind::Sleeper { .. })).collect();
        let is_prom: Vec<bool> = kind.iter().map(|k| matches!(k, Kind::Promoted)).collect();
        let is_syb: Vec<bool> = kind.iter().map(|k| matches!(k, Kind::Sybil { .. })).collect();
        let joined: Vec<u64> = world.accounts().iter().map(|a| a.joined_at).collect();
        let quality: Vec<f32> = world.accounts().iter().map(|a| a.quality).collect();
        let online: Vec<bool> = (0..n)
            .map(|i| alive[i] && (world.was_online_at(i, t) || (wc.force_att_online && is_att[i])))
            .collect();

        // 1. 引擎：新建 + intern 新编号
        for i in 0..n {
            if alive[i] && nodes[i].is_none() {
                let mut e = Engine::new(ecfg(arm));
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

        // 2. 回归检测（在线，间隔 ≥ 0.8D）
        let mut just_returned: Vec<usize> = Vec::new();
        for i in 0..n {
            if !alive[i] || !online[i] {
                continue;
            }
            if !is_att[i] && !is_prom[i] && prev_off[i] > 0 && prev_off[i] + ROUND_LEN >= g_min {
                events.push((i, r, prev_off[i] + ROUND_LEN));
                ever_gap[i] = true;
                just_returned.push(i);
            }
        }

        // 3. 同步正分清单（本轮 t 时刻的世界状态）
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

        // 4. 面板
        let honest_online: Vec<usize> = (0..n)
            .filter(|&i| online[i] && !is_att[i] && !is_prom[i])
            .collect();
        // 收益方按「种类」定位，不要求当前存活：它离开网络后仍然是被抬举的那个账户，
        // 若这里要求 alive，收益方一离开，后面所有事件的 pct 都会静默变成 0（污染 A/Dp）。
        let prom_idx: Option<usize> = (0..n).find(|&i| is_prom[i]);
        let alive_n = (0..n).filter(|&i| alive[i]).count();
        let online_n = (0..n).filter(|&i| online[i]).count();
        let n_alive_att = (0..n).filter(|&i| alive[i] && is_att[i]).count();
        let n_alive_slp = (0..n).filter(|&i| alive[i] && is_slp[i]).count();
        let n_on_att = (0..n).filter(|&i| online[i] && is_att[i]).count();
        let n_on_slp = (0..n).filter(|&i| online[i] && is_slp[i]).count();

        // 5. 本轮活跃的回归事件（先算，决定要不要做昂贵的面板行）
        let active_all: Vec<(usize, usize)> = events
            .iter()
            .filter(|&&(_, r0, _)| r >= r0 && r - r0 < NK)
            .map(|&(obs, r0, _)| (obs, r0))
            .collect();
        // 每轮最多 8 个事件做面板行（按步长抽，跨臂一致，可配对）
        let active = stride_sample_idx(&active_all, 16);
        let mut rr = RoundRec {
            r,
            n_h: honest_online.len(),
            n_alive: alive_n,
            n_online: online_n,
            pop_slp_alive: if alive_n > 0 { n_alive_slp as f64 / alive_n as f64 } else { 0.0 },
            pop_att_alive: if alive_n > 0 { n_alive_att as f64 / alive_n as f64 } else { 0.0 },
            pop_slp_on: if online_n > 0 { n_on_slp as f64 / online_n as f64 } else { 0.0 },
            pop_att_on: if online_n > 0 { n_on_att as f64 / online_n as f64 } else { 0.0 },
            ..RoundRec::default()
        };

        let last_round = r == rounds;
        let probe_round = r == (wc.steps / ROUND_LEN) as usize;
        if !active.is_empty() || last_round || probe_round {
            // 6a. 对照组（joined_at=0、从未离线 >=0.8D、当轮在线）
            let ctrl_all: Vec<usize> = honest_online
                .iter()
                .copied()
                .filter(|&i| joined[i] == 0 && !ever_gap[i])
                .collect();
            let ctrl = stride_sample(&ctrl_all, CTRL_CAP);
            if let Some(prom) = prom_idx {
                let mut cs = Vec::new();
                let mut cm = Vec::new();
                let mut cr = Vec::new();
                for &c in &ctrl {
                    if let Some(e) = nodes[c].as_ref() {
                        let row = row_of(e, n, t);
                        let (s, m) = pct_of(&row, &honest_online, prom);
                        cs.push(s);
                        cm.push(m);
                        let mut hs = 0.0f64;
                        for &u in &honest_online {
                            hs += row[u] as f64;
                        }
                        let hm = hs / honest_online.len().max(1) as f64;
                        cr.push(if hm > 0.0 { row[prom] as f64 / hm } else { 0.0 });
                    }
                }
                rr.ctrl_n = cs.len();
                rr.ctrl_pct_s = if cs.is_empty() { 0.0 } else { cs.iter().sum::<f64>() / cs.len() as f64 };
                rr.ctrl_pct_m = if cm.is_empty() { 0.0 } else { cm.iter().sum::<f64>() / cm.len() as f64 };
                rr.ctrl_ratio = if cr.is_empty() { 0.0 } else { cr.iter().sum::<f64>() / cr.len() as f64 };
            }
            let sample_obs = stride_sample(&honest_online, 8);
            {
                // 对照组每轮快照（live 规模在 §4.9 删除后不再可读，故不再输出）
                let mut live_v: Vec<f64> = Vec::new();
                println!(
                    "CTRL,{:.2},{},{},{},{},{},{:.4},{:.4},{:.4},{:.3},{:.3},{:.4},{:.4},{:.4}",
                    wc.p, wc.d, wc.seed, arm.tag(), r, rr.ctrl_n, rr.ctrl_pct_s, rr.ctrl_pct_m,
                    rr.ctrl_ratio, rr.s_p50, rr.cov_p50, rr.pop_slp_alive, rr.pop_att_alive,
                    median(&mut live_v)
                );
            }

            // 6b. 事件行
            for &(obs, r0) in &active {
                let k = r - r0;
                let e = match nodes[obs].as_ref() {
                    Some(e) => e,
                    None => continue,
                };
                let ls = live_stat(e, t, &is_att, &is_slp);
                let mut rec = EvRec { obs, r0, k, ls, online: online[obs], ..EvRec::default() };
                if let Some(prom) = prom_idx {
                    let row = row_of(e, n, t);
                    let (s, m) = pct_of(&row, &honest_online, prom);
                    rec.pct_s = s;
                    rec.pct_m = m;
                    rec.share_prom = row[prom] as f64;
                    let mut hs = 0.0f64;
                    for &u in &honest_online {
                        hs += row[u] as f64;
                    }
                    rec.hon_mean = hs / honest_online.len().max(1) as f64;
                    let (rho, sup) = spearman(&row, &quality, n);
                    rec.rho = rho;
                    rec.support = sup;
                }
                out.ev.push(rec);
            }
            // 6c. 覆盖率分桶用的抽样行（k=999 表示非事件）
            let sample_obs = stride_sample(&honest_online, 8);
            for &o in &sample_obs {
                if let Some(e) = nodes[o].as_ref() {
                    let row = row_of(e, n, t);
                    let (rho, sup) = spearman(&row, &quality, n);
                    let ls = live_stat(e, t, &is_att, &is_slp);
                    let mut rec = EvRec { obs: o, r0: r, k: 999, rho, support: sup, ls, online: true, ..EvRec::default() };
                    let syb_n = is_syb.iter().filter(|x| **x).count();
                    if syb_n > 0 {
                        let mut s = 0.0f64;
                        for u in 0..n {
                            if is_syb[u] {
                                s += row[u] as f64;
                            }
                        }
                        rec.syb = s / syb_n as f64;
                    }
                    if let Some(prom) = prom_idx {
                        let (s, m) = pct_of(&row, &honest_online, prom);
                        rec.pct_s = s;
                        rec.pct_m = m;
                        rec.share_prom = row[prom] as f64;
                        let mut hs = 0.0f64;
                        for &u in &honest_online {
                            hs += row[u] as f64;
                        }
                        rec.hon_mean = hs / honest_online.len().max(1) as f64;
                    }
                    out.ev.push(rec);
                }
            }
        }

        // 7. 观点交换

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
            let reqs = match nodes[i].as_mut() {
                Some(e) => e.tick(t, k),
                None => continue,
            };
            for req in reqs {
                let src = req.idx as usize;
                match views.get(src) {
                    Some(Some(v)) => {
                        let v = v.clone();
                        if let Some(e) = nodes[i].as_mut() {
                            e.on_view(req.idx, v, t, t);
                        }
                    }
                    _ => {
                        if let Some(e) = nodes[i].as_mut() {
                            e.on_view_failed(req.idx, t);
                        }
                    }
                }
            }
        }

        // 8. 更新离线时长
        for i in 0..n {
            prev_off[i] = if alive[i] { world.offline_duration(i, t) } else { 0 };
        }

        out.rounds.push(rr);
        if r < rounds {
            world.run_steps(ROUND_LEN as usize);
        }
    }
    out.n_events = events.len();
    out.secs = t0.elapsed().as_secs_f64();
    out
}

// ─────────────────────────── 诊断主实验 ───────────────────────────

fn diag(args: &[String]) {
    let smoke = args.iter().any(|a| a == "--smoke");
    let force_on = args.iter().any(|a| a == "--att-online");
    let ratio: f64 = get_f64(args, "--ratio", 0.10);
    let ps: Vec<f64> = if smoke {
        vec![0.70]
    } else {
        let p0 = get_f64(args, "--p", -1.0);
        if p0 >= 0.0 { vec![p0] } else { vec![0.50, 0.70, 0.90] }
    };
    let d: u64 = get_u64(args, "--d", 216);
    let seeds: Vec<u64> = if smoke { vec![11] } else { vec![11, 23, 37, 53, 71, 89, 97, 113] };
    let steps: u64 = 400;
    let n_init: usize = get_u64(args, "--n", 300) as usize;

    // dormancy_rate 取 L4 的 CAL 标定值（attack_ratio=1/300）
    let calib: [(f64, u64, f64); 12] = [
        (0.30, 72, 0.002779), (0.30, 216, 0.000905), (0.30, 720, 0.000292),
        (0.50, 72, 0.006416), (0.50, 216, 0.002171), (0.50, 720, 0.000655),
        (0.70, 72, 0.015679), (0.70, 216, 0.004917), (0.70, 720, 0.001159),
        (0.90, 72, 0.069347), (0.90, 216, 0.009935), (0.90, 720, 0.002999),
    ];
    println!("# A5 diag：回归者 live 集合组成");
    println!("# 攻击=Sleeper ratio={:.3} normal_until={} 强制攻击者在线={} 轮长={}h samples={} (回归当轮 x3)",
        ratio, steps / 5, force_on as i32, ROUND_LEN, SAMPLES);
    println!("# live = rating>0 且 view_age<{}h；本臂 = G（T_remove={}, H_group={}, θ_remove=0.5, N_min=3）", T_REMOVE, T_REMOVE, H_GROUP);
    println!("DIAGHDR,p,D,seed,k,n_events,live_mean,n_pos_mean,cov_mean,s_mean,live_slp_share,live_att_share,pop_slp_share,pop_att_share,pos_slp_share,pos_att_share,ratio_slp,ratio_att,ratio_slp_on,ratio_att_on,age_med");

    for &p in &ps {
        let dormancy = calib.iter().find(|c| (c.0 - p).abs() < 1e-9 && c.1 == d).map(|c| c.2).unwrap_or(0.005);
        for &seed in &seeds {
            let wc = WorldCfg {
                p, dormancy, d, seed, steps, n_initial: n_init,
                attack_ratio: ratio,
                attack: AttackKind::Sleeper { normal_until: steps / 5 },
                force_att_online: force_on,
            };
            let o = simulate(&wc, Arm::Off);
            for k in 0..NK {
                let rows: Vec<&EvRec> = o.ev.iter().filter(|e| e.k == k && e.online).collect();
                if rows.is_empty() {
                    continue;
                }
                let mut live = Vec::new();
                let mut npos = Vec::new();
                let mut cov = Vec::new();
                let mut s = Vec::new();
                let mut lsh = Vec::new();
                let mut lsh_att = Vec::new();
                let mut psh = Vec::new();
                let mut psh_att = Vec::new();
                let mut ratio_slp = Vec::new();
                let mut ratio_att = Vec::new();
                let mut ratio_slp_on = Vec::new();
                let mut ratio_att_on = Vec::new();
                let mut ages = Vec::new();
                for e in &rows {
                    let ls = e.ls;
                    live.push(ls.live as f64);
                    npos.push(ls.n_pos as f64);
                    cov.push(ls.cov);
                    s.push(ls.s);
                    ages.push(ls.age_med);
                    let lsv = if ls.live > 0 { ls.live_slp as f64 / ls.live as f64 } else { 0.0 };
                    let lav = if ls.live > 0 { ls.live_att as f64 / ls.live as f64 } else { 0.0 };
                    let psv = if ls.n_pos > 0 { ls.pos_slp as f64 / ls.n_pos as f64 } else { 0.0 };
                    let pav = if ls.n_pos > 0 { ls.pos_att as f64 / ls.n_pos as f64 } else { 0.0 };
                    lsh.push(lsv);
                    lsh_att.push(lav);
                    psh.push(psv);
                    psh_att.push(pav);
                    let rr = o.rounds.iter().find(|x| x.r == e.r0 + e.k).copied().unwrap_or_default();
                    ratio_slp.push(if rr.pop_slp_alive > 0.0 { lsv / rr.pop_slp_alive } else { 0.0 });
                    ratio_att.push(if rr.pop_att_alive > 0.0 { lav / rr.pop_att_alive } else { 0.0 });
                    ratio_slp_on.push(if rr.pop_slp_on > 0.0 { lsv / rr.pop_slp_on } else { 0.0 });
                    ratio_att_on.push(if rr.pop_att_on > 0.0 { lav / rr.pop_att_on } else { 0.0 });
                }
                let mut pop_s = Vec::new();
                let mut pop_a = Vec::new();
                for e in &rows {
                    if let Some(rr) = o.rounds.iter().find(|x| x.r == e.r0 + e.k) {
                        pop_s.push(rr.pop_slp_alive);
                        pop_a.push(rr.pop_att_alive);
                    }
                }
                println!(
                    "DIAG,{:.2},{},{},{},{},{:.2},{:.2},{:.3},{:.3},{:.3},{:.3},{:.4},{:.4},{:.4},{:.4},{:.2},{:.2},{:.2},{:.2},{:.1}",
                    p, d, seed, k, rows.len(),
                    mean_sd(&live).0, mean_sd(&npos).0, mean_sd(&cov).0, mean_sd(&s).0,
                    mean_sd(&lsh).0, mean_sd(&lsh_att).0, mean_sd(&pop_s).0, mean_sd(&pop_a).0,
                    mean_sd(&psh).0, mean_sd(&psh_att).0,
                    mean_sd(&ratio_slp).0, mean_sd(&ratio_att).0,
                    mean_sd(&ratio_slp_on).0, mean_sd(&ratio_att_on).0,
                    mean_sd(&ages).0
                );
            }
            eprintln!("[diag] p={p} d={d} seed={seed} events={} {:.1}s", o.n_events, o.secs);
        }
    }
}

// ─────────────────────────── 效果实验 ───────────────────────────

fn effect(args: &[String]) {
    let smoke = args.iter().any(|a| a == "--smoke");
    let more = args.iter().any(|a| a == "--more");
    let arms = arms_of(more);
    let ratio: f64 = get_f64(args, "--ratio", 1.0 / 300.0);
    let ps: Vec<f64> = if smoke {
        vec![0.50]
    } else {
        let p0 = get_f64(args, "--p", -1.0);
        if p0 >= 0.0 { vec![p0] } else { vec![0.30, 0.50, 0.70, 0.90] }
    };
    let d: u64 = get_u64(args, "--d", 216);
    let seeds: Vec<u64> = if smoke {
        vec![11, 23]
    } else {
        vec![11, 23, 37, 53, 71, 89, 97, 113]
    };
    let steps: u64 = 400;
    let n_init: usize = 300;
    let calib: [(f64, u64, f64); 12] = [
        (0.30, 72, 0.002779), (0.30, 216, 0.000905), (0.30, 720, 0.000292),
        (0.50, 72, 0.006416), (0.50, 216, 0.002171), (0.50, 720, 0.000655),
        (0.70, 72, 0.015679), (0.70, 216, 0.004917), (0.70, 720, 0.001159),
        (0.90, 72, 0.069347), (0.90, 216, 0.009935), (0.90, 720, 0.002999),
    ];
    println!("# A5 effect：Off（全关）/ Cov（只开 live 判定）/ G（4.9 推荐参数）三臂配对");
    println!("# 同 seed 的世界逐位一致，配对按 (观察者, 回归轮, k) 对齐");
    println!("EVHDR,p,D,seed,arm,k,obs,r0,pct_strict,pct_mid,share_prom,hon_mean,rho,support,live,cov,s,avg_age,n_pos");
    println!("CTRLHDR,p,D,seed,arm,r,ctrl_n,ctrl_pct_s,ctrl_pct_m,ctrl_ratio,s_p50,cov_p50,pop_slp_alive,pop_att_alive,live_p50");
    let mut ev_all: Vec<(f64, u64, u64, String, Vec<EvRec>)> = Vec::new();
    let mut round_all: Vec<(f64, u64, u64, String, Vec<RoundRec>)> = Vec::new();

    for &p in &ps {
        let dormancy = calib.iter().find(|c| (c.0 - p).abs() < 1e-9 && c.1 == d).map(|c| c.2).unwrap_or(0.005);
        for &seed in &seeds {
            for &arm in &arms {
                let wc = WorldCfg {
                    p, dormancy, d, seed, steps, n_initial: n_init,
                    attack_ratio: ratio,
                    attack: AttackKind::Sleeper { normal_until: steps / 5 },
                    force_att_online: false,
                };
                let o = simulate(&wc, arm);
                for e in &o.ev {
                    if (e.k < NK && e.online) || e.k == 999 {
                        println!(
                            "EV,{:.2},{},{},{},{},{},{},{:.4},{:.4},{:.5},{:.5},{:.4},{},{},{:.3},{:.3},{:.1},{},{:.6}",
                            p, d, seed, arm.tag(), e.k, e.obs, e.r0, e.pct_s, e.pct_m,
                            e.share_prom, e.hon_mean, e.rho, e.support, e.ls.live, e.ls.cov, e.ls.s,
                            e.ls.avg_age, e.ls.n_pos, e.syb
                        );
                    }
                }
                eprintln!("[effect] p={p} d={d} seed={seed} arm={} events={} {:.1}s",
                    arm.tag(), o.n_events, o.secs);
                ev_all.push((p, d, seed, arm.tag().to_string(), o.ev));
                round_all.push((p, d, seed, arm.tag().to_string(), o.rounds));
            }
        }
    }

    println!("\n# ---- 回归者判断偏差 A = 事件观察者均值 − 同轮对照均值（百分位）----");
    println!("AGG,p,arm,k,n_ev,ev_pct_strict,ctrl_pct_strict,A_strict,ev_pct_mid,ctrl_pct_mid,A_mid,ratio_ev,ratio_ctrl,live_med,cov_med,s_med");
    for &p in &ps {
        for k in 0..NK {
            for &arm in &arms {
                let tag = arm.tag().to_string();
                let mut evs = Vec::new();
                let mut ev_m = Vec::new();
                let mut ctrl_s = Vec::new();
                let mut ctrl_m = Vec::new();
                let mut ratio_ev = Vec::new();
                let mut ratio_ctrl = Vec::new();
                let mut live_med = Vec::new();
                let mut cov_med = Vec::new();
                let mut s_med = Vec::new();
                for (pp, _dd, seed, t2, ev) in &ev_all {
                    if (*pp - p).abs() > 1e-9 || *t2 != tag {
                        continue;
                    }
                    let _ = seed;
                    for e in ev.iter().filter(|e| e.k == k && e.online) {
                        evs.push(e.pct_s);
                        ev_m.push(e.pct_m);
                        live_med.push(e.ls.live as f64);
                        cov_med.push(e.ls.cov);
                        s_med.push(e.ls.s);
                        ratio_ev.push(if e.hon_mean > 0.0 { e.share_prom / e.hon_mean } else { 0.0 });
                    }
                }
                // 对照：与该事件同轮（r = r0 + k）的对照均值
                for e in ev_all.iter().filter(|x| (x.0 - p).abs() < 1e-9).flat_map(|x| x.4.iter()) {
                    if e.k != k || !e.online {
                        continue;
                    }
                    let r = e.r0 + e.k;
                    for (pp2, _d2, _s2, t2, rr) in &round_all {
                        if (*pp2 - p).abs() > 1e-9 || *t2 != tag {
                            continue;
                        }
                        if let Some(x) = rr.iter().find(|x| x.r == r && x.ctrl_n > 0) {
                            ctrl_s.push(x.ctrl_pct_s);
                            ctrl_m.push(x.ctrl_pct_m);
                            ratio_ctrl.push(x.ctrl_ratio);
                        }
                    }
                }
                if evs.is_empty() {
                    continue;
                }
                let em = mean_sd(&evs).0;
                let cm = mean_sd(&ctrl_s).0;
                let emm = mean_sd(&ev_m).0;
                let cmm = mean_sd(&ctrl_m).0;
                println!(
                    "AGG,{:.2},{},{},{},{:.2},{:.2},{:+.2},{:.2},{:.2},{:+.2},{:.2},{:.2},{:.0},{:.3},{:.3}",
                    p, tag, k, evs.len(), em * 100.0, cm * 100.0, (em - cm) * 100.0,
                    emm * 100.0, cmm * 100.0, (emm - cmm) * 100.0,
                    mean_sd(&ratio_ev).0, mean_sd(&ratio_ctrl).0,
                    median(&mut live_med), median(&mut cov_med), median(&mut s_med)
                );
            }
        }
    }

    println!("\n# ---- 配对差 Dp = pct(G 或 Cov) − pct(Off)，同 seed 同观察者同回归轮 ----");
    println!("DP,p,k,arm,n,dp_strict_mean,dp_strict_sd,dp_mid_mean,dp_mid_sd,win_rate");
    for &p in &ps {
        for k in 0..NK {
            for &arm in &arms {
                if arm == Arm::Off {
                    continue;
                }
                let tag = arm.tag().to_string();
                let mut ds = Vec::new();
                let mut dm = Vec::new();
                for (pp, _dd, seed, t2, ev) in &ev_all {
                    if (*pp - p).abs() > 1e-9 || *t2 != tag {
                        continue;
                    }
                    let off = match ev_all.iter().find(|(p2, _d2, s2, t3, _)| {
                        (*p2 - p).abs() < 1e-9 && *s2 == *seed && *t3 == "off"
                    }) {
                        Some(x) => &x.4,
                        None => continue,
                    };
                    for e in ev.iter().filter(|e| e.k == k && e.online) {
                        if let Some(b) = off.iter().find(|b| b.obs == e.obs && b.r0 == e.r0 && b.k == k) {
                            ds.push(e.pct_s - b.pct_s);
                            dm.push(e.pct_m - b.pct_m);
                        }
                    }
                }
                if ds.is_empty() {
                    continue;
                }
                let (ms, ss) = mean_sd(&ds);
                let (mm, sm) = mean_sd(&dm);
                let win = dm.iter().filter(|x| **x > 0.0).count() as f64 / dm.len() as f64;
                println!(
                    "DP,{:.2},{},{},{},{:+.4},{:.4},{:+.4},{:.4},{:.3}",
                    p, k, tag, ds.len(), ms, ss, mm, sm, win
                );
            }
        }
    }

    println!("\n# ---- 覆盖率分桶的排序质量（Spearman ρ vs 真实 quality）----");
    println!("BUCKET,p,arm,bucket,n,rho_mean,rho_sd,support_mean,s_mean");
    let buckets = [(0.0, 0.0), (0.01, 0.25), (0.25, 0.50), (0.50, 0.75), (0.75, 1.0001)];
    for &p in &ps {
        for &arm in &arms {
            let tag = arm.tag().to_string();
            for &(lo, hi) in &buckets {
                let mut rhos = Vec::new();
                let mut sups = Vec::new();
                let mut sval = Vec::new();
                for (pp, _dd, _seed, t2, ev) in &ev_all {
                    if (*pp - p).abs() > 1e-9 || *t2 != tag {
                        continue;
                    }
                    for e in ev.iter().filter(|e| e.k == 999) {
                        if e.ls.cov >= lo && e.ls.cov < hi {
                            rhos.push(e.rho);
                            sups.push(e.support as f64);
                            sval.push(e.ls.s);
                        }
                    }
                }
                if rhos.is_empty() {
                    continue;
                }
                let bname = if hi <= 0.001 {
                    "cov=0".to_string()
                } else {
                    format!("({:.2},{:.2}]", lo, hi.min(1.0))
                };
                println!(
                    "BUCKET,{:.2},{},{},{},{:.4},{:.4},{:.1},{:.3}",
                    p, tag, bname, rhos.len(), mean_sd(&rhos).0, mean_sd(&rhos).1,
                    mean_sd(&sups).0, mean_sd(&sval).0
                );
            }
        }
    }
}

// ─────────────────────────── S3 复核 ───────────────────────────

fn s3(args: &[String]) {
    let smoke = args.iter().any(|a| a == "--smoke");
    let seeds: Vec<u64> = if smoke { vec![11] } else { vec![11, 23, 37, 53, 71] };
    let n_init = 300usize;
    let steps = 400u64;
    let want = args
        .iter()
        .position(|a| a == "--kind")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "all".to_string());
    println!("# A5 s3：收益方比值（真 Engine，每账户一个 Engine）");
    println!("# ratio = 诚实观察者眼中 share(收益方) ÷ 同期诚实账户平均 share");
    println!("S3HDR,kind,seed,arm,n_obs,ratio_mean,ratio_sd,share_prom,hon_mean,ctrl_n,ctrl_ratio");
    for &(kind, ratio) in &[
        (AttackKind::Sleeper { normal_until: steps / 5 }, 1.0 / 300.0),
        (AttackKind::Infiltrator, 0.08),
        (AttackKind::Endorser { ratio: 0.20 }, 0.20),
    ] {
        let tag = match kind {
            AttackKind::Sleeper { .. } => "sleeper",
            AttackKind::Infiltrator => "infiltrator",
            _ => "endorser",
        };
        if want != "all" && want != tag {
            continue;
        }
        for &seed in &seeds {
            for arm in arms_of(false) {
                let wc = WorldCfg {
                    p: 0.30, dormancy: 0.000905, d: 216, seed, steps, n_initial: n_init,
                    attack_ratio: ratio, attack: kind, force_att_online: false,
                };
                let o = simulate(&wc, arm);
                let last_r = (steps / ROUND_LEN) as usize;
                let mut ratios = Vec::new();
                let mut sp = Vec::new();
                let mut hm = Vec::new();
                for e in o.ev.iter().filter(|e| e.k == 999 && e.r0 == last_r) {
                    if e.hon_mean > 0.0 {
                        ratios.push(e.share_prom / e.hon_mean);
                        sp.push(e.share_prom);
                        hm.push(e.hon_mean);
                    }
                }
                let ctrl = o.rounds.last().copied().unwrap_or_default();
                println!(
                    "S3,{},{},{},{},{:.4},{:.4},{:.5},{:.5},{},{:.4}",
                    tag, seed, arm.tag(), ratios.len(),
                    mean_sd(&ratios).0, mean_sd(&ratios).1, mean_sd(&sp).0, mean_sd(&hm).0,
                    ctrl.ctrl_n, ctrl.ctrl_ratio
                );
            }
        }
    }
}

// ─────────────────────────── S1 复核 ───────────────────────────

fn s1(args: &[String]) {
    let smoke = args.iter().any(|a| a == "--smoke");
    let seeds: Vec<u64> = if smoke { vec![11] } else { vec![11, 23, 37, 53, 71] };
    println!("# A5 s1：女巫账户在诚实观察者眼里的平均份额（真 Engine）");
    println!("# S1HDR,seed,arm,n_obs,syb_share,hon_mean,ratio");
    for &seed in &seeds {
        for arm in arms_of(false) {
            let wc = WorldCfg {
                p: 0.30, dormancy: 0.000905, d: 216, seed, steps: 400, n_initial: 300,
                attack_ratio: 0.08,
                attack: AttackKind::Sybil { cluster: 4 },
                force_att_online: false,
            };
            let o = simulate(&wc, arm);
            let last_r = (400u64 / ROUND_LEN) as usize;
            let rows: Vec<&EvRec> = o.ev.iter().filter(|e| e.k == 999 && e.r0 == last_r).collect();
            let syb: Vec<f64> = rows.iter().map(|e| e.syb).collect();
            let hon: Vec<f64> = rows.iter().map(|e| e.hon_mean).collect();
            let r: Vec<f64> = rows
                .iter()
                .map(|e| if e.hon_mean > 0.0 { e.syb / e.hon_mean } else { 0.0 })
                .collect();
            println!(
                "S1,{},{},{},{:.6},{:.5},{:.3}",
                seed, arm.tag(), rows.len(), mean_sd(&syb).0, mean_sd(&hon).0, mean_sd(&r).0
            );
        }
    }
}

// ─────────────────────────── 参数 ───────────────────────────

fn get_f64(args: &[String], name: &str, default: f64) -> f64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

fn get_u64(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("diag");
    match mode {
        "diag" => diag(&args),
        "effect" => effect(&args),
        "s3" => s3(&args),
        "s1" => s1(&args),
        _ => println!("usage: a5_group diag|effect|s3 [--smoke] [--p 0.5] [--d 216] [--ratio 0.1] [--n 300] [--att-online]"),
    }
}