//! C2 · badlist 事件版仿真：与 C1 完全相同的场景，对照「清单 + 每 tick 重算」
//! 与「一次性事件 + min」的差异。
//!
//! 世界与 C1 相同：n_initial=420、steps=600、Infiltrator 10%、8 seed，
//! 每个存活账户一个真 Engine，t=0 交换 3 轮 × 24 采样。
//!
//! 用法：
//! ~~~text
//! cargo run --release --bin c2_report -- --seeds 8 --n 420 --steps 600 --rounds 3 --samples 24
//! ~~~
//!
//! C2 的调度：每个 **engine tick** 之后（本轮 on_view 之前）对收益方 A 调用一次
//! report_bad_account——这正好是 C1 里 tick 开头自动重算折扣的时刻，因此两者的
//! 差别只剩「清单 vs 事件」本身。收尾再报告一次，对应 C1 的 final recompute。
//!
//! 输出包含：
//! 1. 8 seed 的主对照（C1 历史 U 行 vs C2 无报告 / 每 tick 报告）；
//! 2. 闭式解单次报告抽查（用探针实际拿到的观点，重建引擎后只报告一次）；
//! 3. 惩罚持久性（1/5/20/100 tick 后折扣是否逐位不变、S3 是否停在低位）；
//! 4. 持续攻击下 S3 随轮次的曲线。

use std::collections::BTreeMap;
use std::time::Instant;

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Args {
    seeds: u64,
    n: usize,
    steps: usize,
    rounds: usize,
    samples: usize,
    curve_seeds: u64,
    curve_rounds: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seeds: 8,
            n: 420,
            steps: 600,
            rounds: 3,
            samples: 24,
            curve_seeds: 3,
            curve_rounds: 8,
        }
    }
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i + 1 < argv.len() {
        let key = argv[i].as_str();
        let val = &argv[i + 1];
        let parse = |v: &str, d: usize| v.parse::<usize>().unwrap_or(d);
        match key {
            "--seeds" => a.seeds = val.parse::<u64>().unwrap_or(a.seeds),
            "--n" => a.n = parse(val, a.n),
            "--steps" => a.steps = parse(val, a.steps),
            "--rounds" => a.rounds = parse(val, a.rounds),
            "--samples" => a.samples = parse(val, a.samples),
            "--curve-seeds" => a.curve_seeds = val.parse::<u64>().unwrap_or(a.curve_seeds),
            "--curve-rounds" => a.curve_rounds = parse(val, a.curve_rounds),
            _ => {}
        }
        i += 2;
    }
    a.seeds = a.seeds.max(1);
    a.n = a.n.max(20);
    a.steps = a.steps.max(10);
    a.rounds = a.rounds.max(1);
    a.samples = a.samples.max(1);
    a.curve_rounds = a.curve_rounds.max(1);
    a
}

// ---------------------------------------------------------------------------
// 世界（与 C1 完全一致）
// ---------------------------------------------------------------------------

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn view_params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 0xC1,
    }
}

/// C2 的引擎配置：唯一新增项是 penalty_strength，默认 1.0（完整惩罚）。
fn engine_cfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: view_params(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        penalty_strength: 1.0,
    }
}

fn make_world(seed: u64, attack: bool, a: Args) -> World {
    let mut cfg = SimConfig {
        n_initial: a.n,
        steps: a.steps,
        seed,
        join_rate: 0.10,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        enable_rhythm: false,
        follow_min: 12,
        follow_max: 20,
        max_follows: 32,
        ..Default::default()
    };
    cfg = cfg.with_attack(
        if attack {
            AttackKind::Infiltrator
        } else {
            AttackKind::None
        },
        if attack { 0.10 } else { 0.0 },
    );
    let mut w = World::new(cfg);
    w.run();
    w
}

/// 世界终态的不可变快照：账户分类、评分清单、质量。
struct Dump {
    n: usize,
    target: Option<usize>,
    attackers: Vec<usize>,
    honest: Vec<usize>,
    alive: Vec<bool>,
    is_attacker: Vec<bool>,
    is_promoted: Vec<bool>,
    quality: Vec<f32>,
    ratings: Vec<Vec<(usize, f32)>>,
}

fn dump(w: &World) -> Dump {
    let accs = w.accounts();
    let n = accs.len();
    let target = w.attack.target;
    let attackers = w.attack.purchased.clone();
    let mut is_attacker = vec![false; n];
    let mut is_promoted = vec![false; n];
    for i in 0..n {
        is_attacker[i] = accs[i].kind.is_attacker();
        is_promoted[i] = matches!(accs[i].kind, tm_sim::Kind::Promoted);
    }
    // 收益方在 Infiltrator 里是 Promoted，is_attacker() 为 false，必须单独排除。
    let honest: Vec<usize> = (0..n)
        .filter(|&i| accs[i].is_alive() && !is_attacker[i] && !is_promoted[i])
        .collect();
    Dump {
        n,
        target,
        attackers,
        honest,
        alive: (0..n).map(|i| accs[i].is_alive()).collect(),
        is_attacker,
        is_promoted,
        quality: accs.iter().map(|a| a.quality).collect(),
        ratings: accs.iter().map(|a| a.ratings.clone()).collect(),
    }
}

// ---------------------------------------------------------------------------
// 引擎与仿真
// ---------------------------------------------------------------------------

fn build_engine(cfg: &EngineConfig, d: &Dump, seed: u64, idx: usize) -> Engine {
    let mix = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((idx as u64).wrapping_mul(0xD1B5_4A32_D192_ED03));
    let mut e = Engine::with_seed(cfg.clone(), mix | 1);
    for i in 0..d.n {
        e.intern(key(i));
    }
    for &(t, s) in &d.ratings[idx] {
        e.rate(t as u32, s, 0);
    }
    e
}

/// 报告次数统计（调用数 / 实际被压低的来源数）。
#[derive(Clone, Copy, Default, Debug)]
struct ReportStats {
    calls: u64,
    lowered: u64,
}

/// 报告的调度方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportMode {
    /// 从不报告（基线）。
    None,
    /// 只在收尾报告一次（等价于 C1 的 final recompute，用于隔离「事件本身」）。
    Once,
    /// 每个 tick 都报告一次，收尾再补一次（本次主实验）。
    EveryTick,
}

/// 跑一遍「无报告」或「每 tick 报告」。
///
/// 报告位置与 C1 的自动重算严格对齐：
/// 每轮先导出观点快照，然后 tick（旧实现会在 tick 开头重算折扣），
/// 紧接着对 A 报告一次，最后才把本轮观点交给 on_view。
/// 这样两者的观点集合、衰减基准与惩罚生效时点完全相同，差别只剩清单/事件本身。
fn run(
    d: &Dump,
    mode: ReportMode,
    seed: u64,
    target: usize,
    a: Args,
    probe: bool,
) -> (Vec<Option<Engine>>, Option<BTreeMap<u32, CompressedView>>, ReportStats) {
    let cfg = engine_cfg();
    let mut nodes: Vec<Option<Engine>> = (0..d.n)
        .map(|i| {
            if d.alive[i] {
                Some(build_engine(&cfg, d, seed, i))
            } else {
                None
            }
        })
        .collect();
    let mut is_observer = vec![false; d.n];
    for &o in &d.honest {
        is_observer[o] = true;
    }
    let mut stats = ReportStats::default();

    let probe_idx = if probe {
        d.honest.iter().copied().max_by_key(|&i| d.ratings[i].len())
    } else {
        None
    };
    let mut probe_views: BTreeMap<u32, CompressedView> = BTreeMap::new();

    for _ in 0..a.rounds {
        let views: Vec<Option<CompressedView>> = nodes
            .iter()
            .map(|n| n.as_ref().map(|e| e.export_view(0)))
            .collect();
        for i in 0..d.n {
            if nodes[i].is_none() {
                continue;
            }
            let reqs = nodes[i].as_mut().unwrap().tick(0, a.samples);
            if mode == ReportMode::EveryTick && is_observer[i] {
                stats.calls += 1;
                stats.lowered += nodes[i]
                    .as_mut()
                    .unwrap()
                    .report_bad_account(target as u32) as u64;
            }
            for req in reqs {
                let src = req.idx as usize;
                match views.get(src).and_then(|v| v.as_ref()) {
                    Some(v) => {
                        if Some(i) == probe_idx {
                            probe_views.insert(req.idx, v.clone());
                        }
                        nodes[i].as_mut().unwrap().on_view(req.idx, v.clone(), 0, 0);
                    }
                    None => nodes[i].as_mut().unwrap().on_view_failed(req.idx, 0),
                }
            }
        }
    }

    // 收尾：与 C1 的 final recompute 对齐，让最后一批观点也参与一次。
    if mode != ReportMode::None {
        for &o in &d.honest {
            if let Some(e) = nodes[o].as_mut() {
                stats.calls += 1;
                stats.lowered += e.report_bad_account(target as u32) as u64;
            }
        }
    }

    let pv = if probe_idx.is_some() {
        Some(probe_views)
    } else {
        None
    };
    (nodes, pv, stats)
}

// ---------------------------------------------------------------------------
// 指标（口径与 C1 完全一致）
// ---------------------------------------------------------------------------

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

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
    if sxx <= 0.0 || syy <= 0.0 {
        f64::NAN
    } else {
        sxy / (sxx * syy).sqrt()
    }
}

fn spearman(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 3 {
        return f64::NAN;
    }
    pearson(&ranks(&x[..n]), &ranks(&y[..n]))
}

#[derive(Clone, Default, Debug)]
struct Metrics {
    s3_prom: f64,
    s3_att: f64,
    rho: f64,
    punished_mean: f64,
    honest_fp: f64,
    honest_mean_disc: f64,
    attacker_mean_disc: f64,
    n_obs: usize,
}

fn collect_discounts(e: &Engine, d: &Dump, o: usize) -> (f64, f64, f64, f64, f64, usize, usize, usize) {
    let mut pun_sum = 0.0;
    let mut pun_n = 0usize;
    let mut hs_sum = 0.0;
    let mut hs_n = 0usize;
    let mut hs_fp = 0usize;
    let mut as_sum = 0.0;
    let mut as_n = 0usize;
    for &(src, _) in &d.ratings[o] {
        if e.view_age(src as u32, 0).is_none() {
            continue;
        }
        let disc = f64::from(e.penalty_of(src as u32));
        if disc < 0.999 {
            pun_sum += disc;
            pun_n += 1;
        }
        if d.is_attacker[src] {
            as_sum += disc;
            as_n += 1;
        } else if !d.is_promoted[src] {
            hs_sum += disc;
            hs_n += 1;
            if disc < 0.9 {
                hs_fp += 1;
            }
        }
    }
    (pun_sum, pun_n as f64, hs_sum, hs_n as f64, as_sum, as_n, hs_fp, 0)
}

fn measure(nodes: &[Option<Engine>], d: &Dump) -> Metrics {
    let mut m = Metrics::default();
    let mut s3p = Vec::new();
    let mut s3a = Vec::new();
    let mut rho_v = Vec::new();
    let mut pun = Vec::new();
    let mut fp = Vec::new();
    let mut hmd = Vec::new();
    let mut amd = Vec::new();

    let target = match d.target {
        Some(t) => t,
        None => return m,
    };
    for &o in &d.honest {
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        let dist = e.trust_distribution(0);
        if dist.is_empty() {
            continue;
        }
        let mut v = vec![0.0f64; d.n];
        let mut tot = 0.0f64;
        for (i, x) in dist {
            v[i as usize] = f64::from(x);
            tot += f64::from(x);
        }
        if tot <= 1e-9 {
            continue;
        }
        let hm = mean(&d.honest.iter().map(|&i| v[i]).collect::<Vec<_>>());
        if !(hm > 1e-9) {
            continue;
        }
        s3p.push(v[target] / hm);
        let am = mean(&d.attackers.iter().map(|&i| v[i]).collect::<Vec<_>>());
        s3a.push(am / hm);

        let xs: Vec<f64> = d.honest.iter().map(|&i| v[i]).collect();
        let ys: Vec<f64> = d.honest.iter().map(|&i| f64::from(d.quality[i])).collect();
        let r = spearman(&xs, &ys);
        if r.is_finite() {
            rho_v.push(r);
        }

        let (ps, pn, hs, hn, asum, an, hfp, _) = collect_discounts(e, d, o);
        if pn > 0.0 {
            pun.push(ps / pn);
        }
        if hn > 0.0 {
            fp.push(hfp as f64 / hn);
            hmd.push(hs / hn);
        }
        if an > 0 {
            amd.push(asum / an as f64);
        }
        m.n_obs += 1;
    }

    m.s3_prom = mean(&s3p);
    m.s3_att = mean(&s3a);
    m.rho = mean(&rho_v);
    m.punished_mean = mean(&pun);
    m.honest_fp = mean(&fp);
    m.honest_mean_disc = mean(&hmd);
    m.attacker_mean_disc = mean(&amd);
    m
}

// ---------------------------------------------------------------------------
// 闭式解抽查：用探针实际拿到的观点重建引擎，只报告一次
// ---------------------------------------------------------------------------

/// 引擎私有的 noise_seed 口径（只用于把 a_v 重算到与 query 同口径）。
fn noise_seed(idx: u32) -> u64 {
    (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

/// 用探针最终拿到的观点重建一个干净引擎，调用 **一次** report_bad_account，
/// 检查压完后的群体项是否等于未加权平均 ā。返回 (|G_after−ā|, 实际压低数, 来源数)。
fn verify_single_report(
    d: &Dump,
    seed: u64,
    probe: usize,
    views: &BTreeMap<u32, CompressedView>,
    target: usize,
) -> Option<(f64, usize, usize)> {
    let cfg = engine_cfg();
    let mut e = build_engine(&cfg, d, seed, probe);
    for (src, v) in views {
        e.on_view(*src, v.clone(), 0, 0);
    }

    let target_key = key(target);
    let total: f64 = d.ratings[probe].iter().map(|&(_, s)| f64::from(s)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut a = Vec::new();
    for &(src, _) in &d.ratings[probe] {
        let view = match views.get(&(src as u32)) {
            Some(v) => v,
            None => continue,
        };
        // 与 Engine::on_view 完全相同的 total_mass 估计：对所有已 intern 账户求和。
        let mut mass = 0.0f64;
        for i in 0..d.n {
            mass += f64::from(view.query(&key(i)).clamp(0.0, 1.0));
        }
        let mass = mass.max(1e-6);
        let raw = f64::from(view.query(&target_key)) / mass;
        let mut av = if raw.is_finite() {
            raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let floor = f64::from(view.noise_floor(1000, noise_seed(src as u32)).median).max(0.0);
        if av < floor {
            av = 0.0;
        }
        a.push(av);
    }
    if a.len() < 2 {
        return None;
    }
    let abar = a.iter().sum::<f64>() / a.len() as f64;

    let lowered = e.report_bad_account(target as u32);
    let personal = f64::from(e.share_of(target as u32).unwrap_or(0.0));
    let g_engine = (f64::from(e.query(target as u32, 0)) - 0.3 * personal) / 0.7;
    Some(((g_engine - abar).abs(), lowered, a.len()))
}

// ---------------------------------------------------------------------------
// 持久性 / 持续攻击曲线
// ---------------------------------------------------------------------------

/// 报告之后经过 N 个 tick，折扣是否逐位不变、S3 是否停在被压低的水平。
struct PersistRow {
    ticks: usize,
    s3_prom: f64,
    s3_att: f64,
    mean_disc: f64,
    fp: f64,
    bit_same: bool,
}

fn persistence(nodes: &mut [Option<Engine>], d: &Dump, probe: usize, m0: &Metrics) -> Vec<PersistRow> {
    let source_bits = |nodes: &[Option<Engine>]| -> Vec<u32> {
        let e = match nodes.get(probe).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => return Vec::new(),
        };
        d.ratings[probe]
            .iter()
            .filter(|(src, _)| e.view_age(*src as u32, 0).is_some())
            .map(|&(src, _)| e.penalty_of(src as u32).to_bits())
            .collect()
    };
    let before = source_bits(nodes);
    let mut out = Vec::new();
    let mut done = 0usize;
    for ticks in [1usize, 5, 20, 100] {
        while done < ticks {
            for e in nodes.iter_mut().flatten() {
                let _ = e.tick(0, 1);
            }
            done += 1;
        }
        let m = measure(nodes, d);
        let bit_same = before == source_bits(nodes);
        out.push(PersistRow {
            ticks,
            s3_prom: if m.s3_prom.is_finite() { m.s3_prom } else { m0.s3_prom },
            s3_att: if m.s3_att.is_finite() { m.s3_att } else { m0.s3_att },
            mean_disc: m.honest_mean_disc,
            fp: m.honest_fp,
            bit_same,
        });
    }
    out
}

/// 持续攻击：每轮结束后记录指标；mode 决定这一轮是否报告。
///
/// 返回值第 i 项对应第 i+1 轮结束后的状态（交换前不做测量：那时还没有任何观点，
/// S3 没有意义）。
fn curve(d: &Dump, seed: u64, target: usize, a: Args, mode: ReportMode) -> Vec<Metrics> {
    let cfg = engine_cfg();
    let mut nodes: Vec<Option<Engine>> = (0..d.n)
        .map(|i| {
            if d.alive[i] {
                Some(build_engine(&cfg, d, seed, i))
            } else {
                None
            }
        })
        .collect();
    let mut is_observer = vec![false; d.n];
    for &o in &d.honest {
        is_observer[o] = true;
    }

    let mut rows = Vec::with_capacity(a.curve_rounds);
    for _ in 0..a.curve_rounds {
        let views: Vec<Option<CompressedView>> = nodes
            .iter()
            .map(|n| n.as_ref().map(|e| e.export_view(0)))
            .collect();
        for i in 0..d.n {
            if nodes[i].is_none() {
                continue;
            }
            let reqs = nodes[i].as_mut().unwrap().tick(0, a.samples);
            if mode == ReportMode::EveryTick && is_observer[i] {
                nodes[i].as_mut().unwrap().report_bad_account(target as u32);
            }
            for req in reqs {
                let src = req.idx as usize;
                match views.get(src).and_then(|v| v.as_ref()) {
                    Some(v) => {
                        nodes[i].as_mut().unwrap().on_view(req.idx, v.clone(), 0, 0);
                    }
                    None => nodes[i].as_mut().unwrap().on_view_failed(req.idx, 0),
                }
            }
        }
        rows.push(measure(&nodes, d));
    }
    rows
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

fn stat(xs: &[f64]) -> (f64, f64) {
    let xs: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let m = mean(&xs);
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64;
    (m, var.sqrt())
}

fn f3(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.3}")
    } else {
        "—".to_string()
    }
}

fn f4(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.4}")
    } else {
        "—".to_string()
    }
}

fn main() {
    let args = parse_args();
    let t0 = Instant::now();
    println!("# C2 · badlist 事件版仿真（与 C1 同场景）");
    println!(
        "# 世界：n_initial={} steps={}，Infiltrator 渗透率 10%，每节点一个真 Engine，{} 轮 × {} 采样，t=0 交换",
        args.n, args.steps, args.rounds, args.samples
    );
    println!("# C1 历史行来自 crates/exp/c1_result.txt（清单版，每 tick 重算，U 统一折扣）");
    println!("# C2 每 tick 在 tick 之后、on_view 之前报告一次；收尾再报告一次（对齐 C1 final recompute）\n");

    let mut rows_none: Vec<Metrics> = Vec::new();
    let mut rows_once: Vec<Metrics> = Vec::new();
    let mut rows_rep: Vec<Metrics> = Vec::new();
    let mut verify_err: Vec<f64> = Vec::new();
    let mut persist_rows: Vec<PersistRow> = Vec::new();
    let mut curve_none: Vec<Vec<Metrics>> = Vec::new();
    let mut curve_rep: Vec<Vec<Metrics>> = Vec::new();

    for s in 0..args.seeds {
        let seed = 1 + s;
        let w = make_world(seed, true, args);
        let d = dump(&w);
        let target = match d.target {
            Some(t) => t,
            None => {
                println!("seed {seed}: 没有收益方，跳过");
                continue;
            }
        };
        println!(
            "## seed {seed}：账户 {}，诚实 {}，购买者 {}，收益方 {}（质量 {:.2}）",
            d.n,
            d.honest.len(),
            d.attackers.len(),
            target,
            d.quality[target]
        );

        let probe = d.honest.iter().copied().max_by_key(|&i| d.ratings[i].len());

        // 无报告：应与 C1 的「无 badlist」行完全一致。
        let (nodes_none, _, _) = run(&d, ReportMode::None, seed, target, args, false);
        let m_none = measure(&nodes_none, &d);
        println!(
            "  无报告        S3收益方/诚实={:>7} S3攻击者/诚实={:>7} ρ={:>7} 诚实<0.9={:>7} n={}",
            f3(m_none.s3_prom),
            f3(m_none.s3_att),
            f4(m_none.rho),
            f3(m_none.honest_fp),
            m_none.n_obs
        );

        // 只在收尾报告一次：等价于 C1 的最后一次 recompute，用来隔离「事件本身」。
        let (nodes_once, _, _) = run(&d, ReportMode::Once, seed, target, args, false);
        let m_once = measure(&nodes_once, &d);
        println!(
            "  单次报告      S3收益方/诚实={:>7} S3攻击者/诚实={:>7} ρ={:>7} 被罚折扣={:>7} 诚实<0.9={:>7} 攻击者折扣={:>7}",
            f3(m_once.s3_prom),
            f3(m_once.s3_att),
            f4(m_once.rho),
            f3(m_once.punished_mean),
            f3(m_once.honest_fp),
            f3(m_once.attacker_mean_disc)
        );

        // 每 tick 报告一次。
        let (mut nodes_rep, pv, stats) = run(&d, ReportMode::EveryTick, seed, target, args, true);
        let m_rep = measure(&nodes_rep, &d);
        println!(
            "  每tick报告    S3收益方/诚实={:>7} S3攻击者/诚实={:>7} ρ={:>7} 被罚折扣={:>7} 诚实<0.9={:>7} 攻击者折扣={:>7} 报告={} 压低={}",
            f3(m_rep.s3_prom),
            f3(m_rep.s3_att),
            f4(m_rep.rho),
            f3(m_rep.punished_mean),
            f3(m_rep.honest_fp),
            f3(m_rep.attacker_mean_disc),
            stats.calls,
            stats.lowered
        );

        // 闭式解：用探针观点重建引擎，只报告一次。
        if let (Some(probe), Some(pv)) = (probe, pv.as_ref()) {
            if let Some((err, lowered, k)) = verify_single_report(&d, seed, probe, pv, target) {
                verify_err.push(err);
                println!(
                    "  闭式解单次报告：{} 个来源，压低 {} 个，|G_after−ā|={:.2e}",
                    k, lowered, err
                );
            }
        }

        // 持久性只看 seed 1，避免重复的测量开销。
        if s == 0 {
            if let Some(probe) = probe {
                let rows = persistence(&mut nodes_rep, &d, probe, &m_rep);
                persist_rows = rows;
            }
        }

        // 持续攻击曲线：前 curve_seeds 个 seed，报告 / 不报告各跑一次。
        if s < args.curve_seeds {
            let rows = curve(&d, seed, target, args, ReportMode::None);
            if curve_none.is_empty() {
                curve_none = vec![Vec::new(); rows.len()];
            }
            for (r, m) in rows.into_iter().enumerate() {
                if r < curve_none.len() {
                    curve_none[r].push(m);
                }
            }
            let rows = curve(&d, seed, target, args, ReportMode::EveryTick);
            if curve_rep.is_empty() {
                curve_rep = vec![Vec::new(); rows.len()];
            }
            for (r, m) in rows.into_iter().enumerate() {
                if r < curve_rep.len() {
                    curve_rep[r].push(m);
                }
            }
        }

        rows_none.push(m_none);
        rows_once.push(m_once);
        rows_rep.push(m_rep);
    }

    // 主表：C1 历史 + C2 无报告 / 单次 / 每 tick 报告。
    println!("\n## 主表（{} seed，均值）\n", args.seeds);
    println!("| 版本 | S3 收益方 ÷ 诚实均值 | S3 攻击者 ÷ 诚实均值 | 排序质量 ρ | 被罚来源平均折扣 | 诚实来源误伤率 (<0.9) |");
    println!("|---|---:|---:|---:|---:|---:|");
    println!("| C1 清单版（历史 U） | 3.524 | 2.069 | 0.3382 | 0.720 | 11.1% |");
    for (label, ms) in [
        ("C2 无报告（本次）", &rows_none),
        ("C2 单次报告（收尾，本次）", &rows_once),
        ("C2 每 tick 报告（本次）", &rows_rep),
    ] {
        let (p, _) = stat(&ms.iter().map(|m| m.s3_prom).collect::<Vec<_>>());
        let (a, _) = stat(&ms.iter().map(|m| m.s3_att).collect::<Vec<_>>());
        let (r, _) = stat(&ms.iter().map(|m| m.rho).collect::<Vec<_>>());
        let (d, _) = stat(&ms.iter().map(|m| m.punished_mean).collect::<Vec<_>>());
        let (fp, _) = stat(&ms.iter().map(|m| m.honest_fp).collect::<Vec<_>>());
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            label,
            f3(p),
            f3(a),
            f4(r),
            f3(d),
            f3(fp)
        );
    }

    // 持久性。
    println!("\n## 惩罚的持久性（seed 1，报告后仅 tick、不再报告）\n");
    println!("| 经过 tick | 平均折扣 | 诚实<0.9 | S3 收益方 | S3 攻击者 | 折扣逐位不变 |");
    println!("|---:|---:|---:|---:|---:|---|");
    for r in &persist_rows {
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            r.ticks,
            f3(r.mean_disc),
            f3(r.fp),
            f3(r.s3_prom),
            f3(r.s3_att),
            if r.bit_same { "是" } else { "否" }
        );
    }

    // 持续攻击曲线：同一批 seed 上对照报告 / 不报告。
    println!("\n## 持续攻击：S3 随轮次（{} seed 均值）\n", args.curve_seeds);
    println!("| 轮次 | S3 收益方（无报告） | S3 收益方（每tick报告） | 平均折扣（报告） | 诚实<0.9（报告） |");
    println!("|---:|---:|---:|---:|---:|");
    for r in 0..curve_rep.len() {
        let ms = &curve_none[r];
        let (pn, _) = stat(&ms.iter().map(|m| m.s3_prom).collect::<Vec<_>>());
        let ms = &curve_rep[r];
        let (pr, _) = stat(&ms.iter().map(|m| m.s3_prom).collect::<Vec<_>>());
        let (d, _) = stat(&ms.iter().map(|m| m.punished_mean).collect::<Vec<_>>());
        let (fp, _) = stat(&ms.iter().map(|m| m.honest_fp).collect::<Vec<_>>());
        println!(
            "| {} | {} | {} | {} | {} |",
            r + 1,
            f3(pn),
            f3(pr),
            f3(d),
            f3(fp)
        );
    }

    // 闭式解汇总。
    let mx = verify_err.iter().copied().fold(0.0f64, f64::max);
    println!(
        "\n## 闭式解抽查\n\n单次报告（用探针实际观点重建引擎）：{} 个 seed，max |G_after−ā| = {:.2e}\n",
        verify_err.len(),
        mx
    );

    println!("总耗时 {:.1}s", t0.elapsed().as_secs_f64());
}
