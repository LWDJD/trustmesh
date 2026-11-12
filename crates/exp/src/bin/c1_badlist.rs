//! C1 · badlist 仿真（**已迁移到 C2 的一次性事件 API**）。
//!
//! 历史背景：C1 用「清单 + 每 tick 重算」实现，原始数字记录在
//! `crates/exp/c1_result.txt` 与 `审计/C1-badlist.md`。C2 把接口改成事件后，
//! 本文件保留为**同一场景在新接口下的复现**：
//!
//! * 清单语义由应用层模拟——维护一个 `Vec<u32>`，每个 tick 对其中每个账户
//!   调用一次 [Engine::report_bad_account]；清单为空时调用
//!   [Engine::clear_penalties]（对应旧 `clear_badlist` 在 α=1 时的即时恢复）；
//! * 因为每个 tick 都重新报告、且默认强度 λ=1，静态观点下 `min` 语义与
//!   旧实现的「覆盖写」等价，所以 U / 错罚 / 半数观察者的结果与 C1 一致；
//! * 变体 P 已从引擎删除，本文件不再输出 P。
//!
//! 用法：
//! ~~~text
//! cargo run --release --bin c1_badlist -- --seeds 8 --n 420 --steps 600 --rounds 3 --samples 24
//! ~~~
//!
//! 输出：每个 seed 的明细 + 跨 seed 均值表。C2 的正式对照见
//! `crates/exp/src/bin/c2_report.rs` 与 `审计/C2-report.md`。

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
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seeds: 8,
            n: 500,
            steps: 600,
            rounds: 3,
            samples: 24,
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
            _ => {}
        }
        i += 2;
    }
    a.seeds = a.seeds.max(1);
    a.n = a.n.max(20);
    a.steps = a.steps.max(10);
    a.rounds = a.rounds.max(1);
    a.samples = a.samples.max(1);
    a
}

// ---------------------------------------------------------------------------
// 世界
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

fn engine_cfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: view_params(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        group_half_life: 0,
        coverage_exponent: 1.0,
        unreachable_after: 0,
        remove_when_coverage: 0.0,
        min_group_sources: 0,
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
// 变体与引擎
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    None,
    Uniform,
    WrongAll,
    Partial,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::None => "无 badlist",
            Variant::Uniform => "U 统一折扣",
            Variant::WrongAll => "U 错罚购买者",
            Variant::Partial => "U 半数观察者",
        }
    }
}

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

/// C1 的「清单」语义在应用层的模拟。
///
/// 空清单 = 没有攻击者：立刻清空折扣（旧 `clear_badlist` 在 α=1 时的行为）。
/// 非空清单：对每个账户报告一次事件。重复报告不会复合（`min` 语义）。
fn sync_penalties(e: &mut Engine, ids: &[u32]) {
    if ids.is_empty() {
        e.clear_penalties();
    } else {
        for &id in ids {
            e.report_bad_account(id);
        }
    }
}

fn badlist_ids(variant: Variant, d: &Dump, rank: usize, target: usize) -> Vec<u32> {
    match variant {
        Variant::None => Vec::new(),
        Variant::Uniform => vec![target as u32],
        Variant::WrongAll => d.attackers.iter().map(|&x| x as u32).collect(),
        Variant::Partial => {
            if rank % 2 == 0 {
                vec![target as u32]
            } else {
                Vec::new()
            }
        }
    }
}

fn run(
    d: &Dump,
    variant: Variant,
    seed: u64,
    target: usize,
    a: Args,
    probe: bool,
) -> (Vec<Option<Engine>>, Option<BTreeMap<u32, CompressedView>>) {
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

    // 每个观察者的「应用层清单」只在开始时确定；之后每轮按它同步事件。
    let mut lists: Vec<Option<Vec<u32>>> = vec![None; d.n];
    for (rank, &o) in d.honest.iter().enumerate() {
        lists[o] = Some(badlist_ids(variant, d, rank, target));
    }

    // 探针：选正分清单最长的诚实观察者，记录它实际拿到的观点，用于闭式解抽查。
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
            // 旧实现是在 tick 开头按清单重算折扣；事件版在 tick 之后同步，
            // 两者看到的观点集合相同（都在本轮 on_view 之前）。
            if let Some(ids) = lists[i].as_deref() {
                sync_penalties(nodes[i].as_mut().unwrap(), ids);
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

    // 收尾：让最后一批观点也进入折扣（旧实现同样在最后手动重算一次）。
    for i in 0..d.n {
        if let (Some(e), Some(ids)) = (nodes[i].as_mut(), lists[i].as_deref()) {
            sync_penalties(e, ids);
        }
    }

    let pv = if probe_idx.is_some() {
        Some(probe_views)
    } else {
        None
    };
    (nodes, pv)
}

// ---------------------------------------------------------------------------
// 指标
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

fn collect_discounts(
    e: &Engine,
    d: &Dump,
    o: usize,
) -> (f64, f64, f64, f64, f64, usize, usize, usize, usize) {
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
    (
        pun_sum,
        pun_n as f64,
        hs_sum,
        hs_n as f64,
        as_sum,
        as_n,
        hs_fp,
        0,
        0,
    )
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

        let (ps, pn, hs, hn, asum, an, hfp, _, _) = collect_discounts(e, d, o);
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
// 中心对照：A 是社交圈中心时机制应自动退出
// ---------------------------------------------------------------------------

/// 精确一致：所有来源对 A 的读数完全相同 → S+ 为空 → 折扣全 1。
///
/// 构造：40 个来源都按同样的权重评价 A 和 9 个填充账户（各 1.0），
/// 于是每份观点里 A 的份额都精确等于 0.1，a_v 全相同。
/// 这不是「A 独占一切」的退化情形，而是「全圈对 A 的态度一模一样」。
fn central_exact() -> (f64, f64, f64, usize) {
    let mut e = Engine::with_seed(engine_cfg(), 7);
    let a = e.intern(key(1));
    let mut fillers = Vec::new();
    for i in 0..9usize {
        fillers.push(e.intern(key(10 + i)));
    }
    let pairs: Vec<([u8; 32], f32)> = std::iter::once((key(1), 1.0))
        .chain((0..9usize).map(|i| (key(10 + i), 1.0)))
        .collect();
    let mut srcs = Vec::new();
    for i in 0..40usize {
        let s = e.intern(key(100 + i));
        e.rate(s, 1.0, 0);
        let v = CompressedView::from_pairs(view_params(), &pairs);
        e.on_view(s, v, 0, 0);
        srcs.push(s);
    }
    let _ = fillers;
    let before = f64::from(e.query(a, 0));
    e.report_bad_account(a);
    let after = f64::from(e.query(a, 0));
    let max_pen = srcs
        .iter()
        .map(|&s| 1.0 - f64::from(e.penalty_of(s)))
        .fold(0.0f64, f64::max);
    (before, after, max_pen, srcs.len())
}

/// 真实世界的「中心」：选被评分最多（in-degree 最高）的诚实账户 A，
/// 所有诚实观察者都错误地把它放进 badlist。这是**误报**压力测试。
struct CentralRow {
    seed: u64,
    a: usize,
    in_deg: usize,
    mean_disc: f64,
    fp_rate: f64,
    share_none: f64,
    share_u: f64,
    max_penalty: f64,
}

fn central_world(seed: u64, args: Args) -> Option<CentralRow> {
    let w = make_world(seed, false, args);
    let d = dump(&w);
    // 社交中心：被最多人评分；平手时质量高者优先。
    let in_degree: Vec<usize> = (0..d.n)
        .map(|x| {
            d.ratings
                .iter()
                .filter(|r| r.iter().any(|&(t, _)| t == x))
                .count()
        })
        .collect();
    let mut cand: Vec<usize> = d.honest.clone();
    cand.sort_by(|&a, &b| {
        in_degree[b]
            .cmp(&in_degree[a])
            .then_with(|| {
                d.quality[b]
                    .partial_cmp(&d.quality[a])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.cmp(&b))
    });
    let a = *cand.first()?;
    let in_deg = in_degree[a];

    let measure_one = |nodes: &[Option<Engine>]| -> (f64, f64, f64, f64) {
        let mut discs = Vec::new();
        let mut fps = Vec::new();
        let mut shares = Vec::new();
        let mut max_pen: f64 = 0.0;
        for &o in &d.honest {
            let e = match nodes.get(o).and_then(|x| x.as_ref()) {
                Some(e) => e,
                None => continue,
            };
            shares.push(f64::from(e.query(a as u32, 0)));
            let mut n = 0usize;
            let mut fp = 0usize;
            let mut sum = 0.0;
            for &(src, _) in &d.ratings[o] {
                if e.view_age(src as u32, 0).is_none() || d.is_attacker[src] || d.is_promoted[src] {
                    continue;
                }
                let dd = f64::from(e.penalty_of(src as u32));
                n += 1;
                sum += dd;
                max_pen = max_pen.max(1.0 - dd);
                if dd < 0.9 {
                    fp += 1;
                }
            }
            if n > 0 {
                discs.push(sum / n as f64);
                fps.push(fp as f64 / n as f64);
            }
        }
        (mean(&discs), mean(&fps), mean(&shares), max_pen)
    };

    let (nodes_none, _) = run(&d, Variant::None, seed, a, args, false);
    let (nodes_u, _) = run(&d, Variant::Uniform, seed, a, args, false);
    let (_, _, share_none, _) = measure_one(&nodes_none);
    let (mean_disc, fp_rate, share_u, max_penalty) = measure_one(&nodes_u);
    Some(CentralRow {
        seed,
        a,
        in_deg,
        mean_disc,
        fp_rate,
        share_none,
        share_u,
        max_penalty,
    })
}

// ---------------------------------------------------------------------------
// 网络内闭式解抽查
// ---------------------------------------------------------------------------

/// 引擎私有的 noise_seed 口径（只用于把 a_v 重算到与 query 同口径）。
fn noise_seed(idx: u32) -> u64 {
    (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

/// 从探针实际拿到的观点重算 a_v / ā / f / G_after，与引擎的群体项对照。
/// 返回 (|G_after − ā|, |G_engine − G_after|, 来源数)。
fn verify_probe(
    nodes: &[Option<Engine>],
    d: &Dump,
    probe: usize,
    views: &BTreeMap<u32, CompressedView>,
    target: usize,
) -> Option<(f64, f64, usize)> {
    let e = nodes.get(probe).and_then(|x| x.as_ref())?;
    let target_key = key(target);
    let total: f64 = d.ratings[probe].iter().map(|&(_, s)| f64::from(s)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut w = Vec::new();
    let mut a = Vec::new();
    for &(src, score) in &d.ratings[probe] {
        let view = views.get(&(src as u32))?;
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
        w.push(f64::from(score) / total);
        a.push(av);
    }
    if w.len() < 2 {
        return None;
    }
    let abar = a.iter().sum::<f64>() / a.len() as f64;
    let mut p0 = 0.0;
    let mut p2 = 0.0;
    for i in 0..w.len() {
        p0 += w[i];
        p2 += w[i] * a[i];
    }
    let g_before = p2 / p0;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..w.len() {
        if a[i] > abar {
            den += w[i] * (a[i] - abar);
        } else {
            num += w[i] * (abar - a[i]);
        }
    }
    let f = if den > 1e-12 {
        (num / den).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let mut n1 = 0.0;
    let mut d1 = 0.0;
    for i in 0..w.len() {
        let f_i = if a[i] > abar { f } else { 1.0 };
        n1 += w[i] * f_i * a[i];
        d1 += w[i] * f_i;
    }
    let g_after = n1 / d1;
    let personal = f64::from(e.share_of(target as u32).unwrap_or(0.0));
    let g_engine = (f64::from(e.query(target as u32, 0)) - 0.3 * personal) / 0.7;
    let _ = g_before;
    Some(((g_after - abar).abs(), (g_engine - g_after).abs(), w.len()))
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
    println!("# C1 · badlist 仿真");
    println!(
        "# 世界：n_initial={} steps={}，Infiltrator 渗透率 10%，每节点一个真 Engine，{} 轮 × {} 采样，t=0 交换",
        args.n, args.steps, args.rounds, args.samples
    );
    println!("# badlist = [收益方 A]（单条）；攻击者也建 Engine，但只有诚实观察者会设 badlist\n");

    let variants = [Variant::None, Variant::Uniform];
    let mut rows: Vec<(u64, Variant, Metrics)> = Vec::new();
    let mut extra: Vec<(u64, Variant, Metrics)> = Vec::new();
    let mut verify_err: Vec<f64> = Vec::new();
    let mut verify_engine_err: Vec<f64> = Vec::new();

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

        // 探针：正分清单最长的诚实观察者。
        let probe = d.honest.iter().copied().max_by_key(|&i| d.ratings[i].len());

        for v in variants {
            let (nodes, pv) = run(&d, v, seed, target, args, v == Variant::Uniform);
            let m = measure(&nodes, &d);
            println!(
                "  {:<14} S3收益方/诚实={:>7} S3攻击者/诚实={:>7} ρ={:>7} 被罚折扣={:>7} 诚实<0.9={:>7} 攻击者折扣={:>7} n={}",
                v.label(),
                f3(m.s3_prom),
                f3(m.s3_att),
                f4(m.rho),
                f3(m.punished_mean),
                f3(m.honest_fp),
                f3(m.attacker_mean_disc),
                m.n_obs
            );
            if v == Variant::Uniform {
                if let (Some(probe), Some(pv)) = (probe, pv.as_ref()) {
                    if let Some((err, eerr, k)) = verify_probe(&nodes, &d, probe, pv, target) {
                        verify_err.push(err);
                        verify_engine_err.push(eerr);
                        println!("    闭式解抽查：{} 个来源，|G_after−ā|={:.2e}，|G_engine−G_after|={:.2e}", k, err, eerr);
                    }
                }
            }
            if matches!(v, Variant::None | Variant::Uniform) {
                rows.push((seed, v, m));
            } else {
                extra.push((seed, v, m));
            }
        }

        // 额外的 badlist 用法对照：错罚全部购买者 / 只让一半观察者启用。
        for v in [Variant::WrongAll, Variant::Partial] {
            let (nodes, _) = run(&d, v, seed, target, args, false);
            let m = measure(&nodes, &d);
            println!(
                "  {:<14} S3收益方/诚实={:>7} S3攻击者/诚实={:>7} ρ={:>7} 被罚折扣={:>7} 诚实<0.9={:>7} 攻击者折扣={:>7}",
                v.label(),
                f3(m.s3_prom),
                f3(m.s3_att),
                f4(m.rho),
                f3(m.punished_mean),
                f3(m.honest_fp),
                f3(m.attacker_mean_disc)
            );
            extra.push((seed, v, m));
        }
    }

    println!("\n## 主表（{} seed，均值 ± 标准差）\n", args.seeds);
    println!("| 变体 | S3 收益方 ÷ 诚实均值 | S3 攻击者 ÷ 诚实均值 | 排序质量 ρ | 被罚来源平均折扣 | 诚实来源误伤率 (<0.9) | 攻击者来源平均折扣 |");
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for v in variants {
        let ms: Vec<&Metrics> = rows
            .iter()
            .filter(|(_, vv, _)| *vv == v)
            .map(|(_, _, m)| m)
            .collect();
        let (p, _) = stat(&ms.iter().map(|m| m.s3_prom).collect::<Vec<_>>());
        let (a, _) = stat(&ms.iter().map(|m| m.s3_att).collect::<Vec<_>>());
        let (r, _) = stat(&ms.iter().map(|m| m.rho).collect::<Vec<_>>());
        let (d, _) = stat(&ms.iter().map(|m| m.punished_mean).collect::<Vec<_>>());
        let (fp, _) = stat(&ms.iter().map(|m| m.honest_fp).collect::<Vec<_>>());
        let (ad, _) = stat(&ms.iter().map(|m| m.attacker_mean_disc).collect::<Vec<_>>());
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            v.label(),
            f3(p),
            f3(a),
            f4(r),
            f3(d),
            f3(fp),
            f3(ad)
        );
    }

    println!("\n## 额外用法对照（% seed 均值）\n");
    println!("| 用法 | S3 收益方 ÷ 诚实 | S3 攻击者 ÷ 诚实 | ρ | 被罚折扣 | 诚实<0.9 |");
    println!("|---|---:|---:|---:|---:|---:|");
    for v in [
        Variant::None,
        Variant::Uniform,
        Variant::WrongAll,
        Variant::Partial,
    ] {
        let ms: Vec<&Metrics> = rows
            .iter()
            .chain(extra.iter())
            .filter(|(_, vv, _)| *vv == v)
            .map(|(_, _, m)| m)
            .collect();
        let (p, _) = stat(&ms.iter().map(|m| m.s3_prom).collect::<Vec<_>>());
        let (a, _) = stat(&ms.iter().map(|m| m.s3_att).collect::<Vec<_>>());
        let (r, _) = stat(&ms.iter().map(|m| m.rho).collect::<Vec<_>>());
        let (d, _) = stat(&ms.iter().map(|m| m.punished_mean).collect::<Vec<_>>());
        let (fp, _) = stat(&ms.iter().map(|m| m.honest_fp).collect::<Vec<_>>());
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            v.label(),
            f3(p),
            f3(a),
            f4(r),
            f3(d),
            f3(fp)
        );
    }

    // 中心对照
    println!("\n## 关键对照：A 是社交圈中心 / 全圈态度一致\n");
    let (cb, ca, cpen, cn) = central_exact();
    println!(
        "精确一致（{} 个来源全部以相同权重评价 A 和 9 个填充账户，读数完全相同）：S+ 为空，最大惩罚 {:.1e}，A 的声望 {:.6} → {:.6}",
        cn, cpen, cb, ca
    );
    println!("\n| seed | A | 被多少人评分 | 来源平均折扣 | 诚实<0.9 | A 份额（无 badlist） | A 份额（U） | 最大惩罚 |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    let mut c_disc = Vec::new();
    let mut c_fp = Vec::new();
    let mut c_share = Vec::new();
    let mut c_drop = Vec::new();
    for s in 0..args.seeds {
        if let Some(r) = central_world(1 + s, args) {
            c_disc.push(r.mean_disc);
            c_fp.push(r.fp_rate);
            c_share.push(r.share_none);
            c_drop.push(if r.share_none > 1e-12 {
                (r.share_none - r.share_u) / r.share_none
            } else {
                0.0
            });
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.1e} |",
                r.seed,
                r.a,
                r.in_deg,
                f3(r.mean_disc),
                f3(r.fp_rate),
                f4(r.share_none),
                f4(r.share_u),
                r.max_penalty
            );
        }
    }
    println!(
        "\n中心对照跨 seed：来源平均折扣 {}，诚实<0.9 {}，A 份额相对下降 {}",
        f3(stat(&c_disc).0),
        f3(stat(&c_fp).0),
        f3(stat(&c_drop).0)
    );

    println!("\n## 闭式解网络抽查\n");
    println!(
        "|G_after − ā| 最大 {:.2e}，均值 {:.2e}；|G_engine − G_after| 最大 {:.2e}（{} 个 seed×探针）",
        verify_err.iter().copied().fold(0.0f64, f64::max),
        mean(&verify_err),
        verify_engine_err.iter().copied().fold(0.0f64, f64::max),
        verify_err.len()
    );
    println!("\n总耗时 {:.1}s", t0.elapsed().as_secs_f64());
}
