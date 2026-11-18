//! A12 对照实验：现行 `tm_engine::Engine` vs 最小内核 `tm_mincore`。
//!
//! 同一张社会图（`tm_sim::World`）、同一批 seed、同一套判据。
//! 输出是可以直接贴进报告的表。
//!
//! 运行：`cargo run --release --bin a12_compare`

use std::collections::BTreeMap;

use tm_engine::{Engine, EngineConfig};
use tm_mincore::{Config as CoreConfig, Node, SparseView};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

// ══════════════════════════ 公共小工具 ══════════════════════════

/// 账户号 -> 32 字节公钥。两种实现用同一个映射，保证比的是算法不是编码。
fn key(idx: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    k[8] = 0xA1;
    k[9] = (idx >> 8) as u8;
    k
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn std(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

fn pm(m: f64, s: f64) -> String {
    format!("{m:.6} ± {s:.6}")
}

fn pm_e(m: f64, s: f64) -> String {
    format!("{m:.3e} ± {s:.2e}")
}

// ══════════════════════════ 世界与数据准备 ══════════════════════════

/// 主图：400 人、静态人口、每人约 120~200 条正分（**清单 ≥ 100 人**，
/// 正是审计里「Count Sketch 在清单变大之后开始出问题」的量级）。
fn world_cfg(seed: u64, attack: AttackKind) -> SimConfig {
    SimConfig {
        n_initial: 400,
        steps: 120,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        dormancy_rate: 0.0,
        enable_rhythm: false,
        follow_min: 120,
        follow_max: 200,
        max_follows: 256,
        attack_kind: attack,
        attack_ratio: 0.1,
        ..SimConfig::default()
    }
}

struct Prepared {
    n: usize,
    out: Vec<Vec<(u32, f32)>>,
    sybils: Vec<usize>,
    honest: Vec<usize>,
    promoted: Option<usize>,
    engine_views: Vec<Option<CompressedView>>,
    core_views: Vec<Option<SparseView>>,
}

fn prepare(seed: u64, attack: AttackKind, cap: usize, params: ViewParams) -> Prepared {
    let mut w = World::new(world_cfg(seed, attack));
    w.run();
    let n = w.accounts.len();
    let mut out: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
    for (s, u, score) in w.ratings() {
        out[s].push((u as u32, score));
    }
    for list in out.iter_mut() {
        list.sort_by_key(|x| x.0);
        list.dedup_by_key(|x| x.0);
    }
    let kinds: Vec<Kind> = w.accounts.iter().map(|a| a.kind).collect();
    let sybils: Vec<usize> = (0..n)
        .filter(|&i| matches!(kinds[i], Kind::Sybil { .. }))
        .collect();
    let honest: Vec<usize> = (0..n)
        .filter(|&i| matches!(kinds[i], Kind::Honest))
        .collect();
    let promoted = w.attack.target;

    // 每个来源发布一份「自己的个人观点」（深度 1：观点 = 我的正分归一化）。
    let mut engine_views = vec![None; n];
    let mut core_views = vec![None; n];
    for i in 0..n {
        let total: f32 = out[i].iter().map(|x| x.1).sum();
        if !(total > 0.0) {
            continue;
        }
        let pairs: Vec<([u8; 32], f32)> = out[i]
            .iter()
            .map(|&(u, s)| (key(u as usize), s / total))
            .collect();
        engine_views[i] = Some(CompressedView::from_pairs(params, &pairs));
        let cpairs: Vec<(u32, f32)> = out[i].iter().map(|&(u, s)| (u, s / total)).collect();
        core_views[i] = Some(SparseView::from_scores(cap, &cpairs));
    }

    Prepared {
        n,
        out,
        sybils,
        honest,
        promoted,
        engine_views,
        core_views,
    }
}

/// 出度最大的 `k` 个诚实账户当观察者（出度降序、账户号升序，完全确定）。
fn observers(d: &Prepared, k: usize) -> Vec<usize> {
    let mut v: Vec<usize> = d.honest.clone();
    v.sort_by(|&a, &b| d.out[b].len().cmp(&d.out[a].len()).then(a.cmp(&b)));
    v.truncate(k);
    v
}

// ══════════════════════════ 两种实现的观察者 ══════════════════════════

fn engine_observer(d: &Prepared, obs: usize, empties: &[[u8; 32]]) -> BTreeMap<[u8; 32], f32> {
    let mut e = Engine::with_seed(EngineConfig::default(), 1);
    for i in 0..d.n {
        e.intern(key(i));
    }
    for k in empties {
        e.intern(*k);
    }
    for &(u, s) in &d.out[obs] {
        e.rate(u, s, 0);
    }
    for &(u, _) in &d.out[obs] {
        if let Some(v) = &d.engine_views[u as usize] {
            e.on_view(u, v.clone(), 0, 0);
        }
    }
    let mut out = BTreeMap::new();
    for (idx, val) in e.trust_distribution(0) {
        if let Some(k) = e.key_of(idx) {
            out.insert(k, val);
        }
    }
    out
}

fn core_observer(d: &Prepared, obs: usize, cap: usize) -> BTreeMap<u32, f32> {
    let mut n = Node::new(
        CoreConfig {
            cap,
            ..CoreConfig::default()
        },
        1,
    );
    for &(u, s) in &d.out[obs] {
        n.rate(u, s, 0);
    }
    for &(u, _) in &d.out[obs] {
        if let Some(v) = &d.core_views[u as usize] {
            n.on_view(u, v.clone(), 0, 0);
        }
    }
    n.distribution(0).entries.into_iter().collect()
}

fn at_engine(m: &BTreeMap<[u8; 32], f32>, idx: usize) -> f64 {
    f64::from(m.get(&key(idx)).copied().unwrap_or(0.0))
}

fn at_core(m: &BTreeMap<u32, f32>, idx: usize) -> f64 {
    f64::from(m.get(&(idx as u32)).copied().unwrap_or(0.0))
}
// ══════════════════════════ S1：女巫在诚实观察者眼里的声望 ══════════════════════════

fn s1(
    seeds: &[u64],
    cap: usize,
    params: ViewParams,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut e_mean = Vec::new();
    let mut e_max = Vec::new();
    let mut e_frac = Vec::new();
    let mut c_mean = Vec::new();
    let mut c_max = Vec::new();
    let mut c_frac = Vec::new();
    for &seed in seeds {
        let d = prepare(seed, AttackKind::Sybil { cluster: 2 }, cap, params);
        if d.sybils.is_empty() {
            continue;
        }
        let obs = observers(&d, 3);
        let (mut em, mut ef) = (Vec::new(), Vec::new());
        let (mut cm, mut cf) = (Vec::new(), Vec::new());
        let (mut emx, mut cmx) = (0.0f64, 0.0f64);
        for &o in &obs {
            let me = engine_observer(&d, o, &[]);
            let mc = core_observer(&d, o, cap);
            let mut ev = Vec::new();
            let mut cv = Vec::new();
            for &s in &d.sybils {
                ev.push(at_engine(&me, s));
                cv.push(at_core(&mc, s));
            }
            em.push(mean(&ev));
            cf_calc(&ev, &mut ef, &mut emx);
            cm.push(mean(&cv));
            cf_calc(&cv, &mut cf, &mut cmx);
        }
        e_mean.push(mean(&em));
        e_frac.push(mean(&ef));
        e_max.push(emx);
        c_mean.push(mean(&cm));
        c_frac.push(mean(&cf));
        c_max.push(cmx);
    }
    (e_mean, e_max, e_frac, c_mean, c_max, c_frac)
}

fn cf_calc(vals: &[f64], frac: &mut Vec<f64>, mx: &mut f64) {
    frac.push(vals.iter().filter(|&&x| x > 0.0).count() as f64 / vals.len() as f64);
    for &v in vals {
        if v > *mx {
            *mx = v;
        }
    }
}

// ══════════════════════════ S3：被抬举账户 / 诚实账户均值 ══════════════════════════

fn s3(seeds: &[u64], cap: usize, params: ViewParams) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut e_ratio = Vec::new();
    let mut c_ratio = Vec::new();
    let mut e_val = Vec::new();
    let mut c_val = Vec::new();
    for &seed in seeds {
        let d = prepare(seed, AttackKind::Infiltrator, cap, params);
        let target = match d.promoted {
            Some(t) => t,
            None => continue,
        };
        let obs = observers(&d, 3);
        let mut er = Vec::new();
        let mut cr = Vec::new();
        for &o in &obs {
            let me = engine_observer(&d, o, &[]);
            let mc = core_observer(&d, o, cap);
            let e_t = at_engine(&me, target);
            let c_t = at_core(&mc, target);
            let e_h: Vec<f64> = d.honest.iter().map(|&i| at_engine(&me, i)).collect();
            let c_h: Vec<f64> = d.honest.iter().map(|&i| at_core(&mc, i)).collect();
            let e_hm = mean(&e_h);
            let c_hm = mean(&c_h);
            if e_hm > 0.0 {
                er.push(e_t / e_hm);
            }
            if c_hm > 0.0 {
                cr.push(c_t / c_hm);
            }
            e_val.push(e_t);
            c_val.push(c_t);
        }
        e_ratio.push(mean(&er));
        c_ratio.push(mean(&cr));
    }
    (e_ratio, c_ratio, e_val, c_val)
}

// ══════════════════════════ S4：1000 个只注册不评分的账户 ══════════════════════════

fn s4(
    seeds: &[u64],
    cap: usize,
    params: ViewParams,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut e_frac = Vec::new();
    let mut e_trust = Vec::new();
    let mut e_over = Vec::new();
    let mut c_frac = Vec::new();
    let mut c_trust = Vec::new();
    let mut c_over = Vec::new();
    for &seed in seeds {
        let d = prepare(seed, AttackKind::None, cap, params);
        let obs = observers(&d, 3);
        let empties: Vec<[u8; 32]> = (0..1000).map(|i| key(1_000_000 + i)).collect();
        let (mut ef, mut et, mut eo) = (Vec::new(), Vec::new(), Vec::new());
        let (mut cf, mut ct, mut co) = (Vec::new(), Vec::new(), Vec::new());
        for &o in &obs {
            // —— 现行实现：1000 个账户被 intern 进账户表 ——
            let me = engine_observer(&d, o, &empties);
            let (mut present, mut total, mut mx) = (0usize, 0.0f64, 0.0f64);
            for k in &empties {
                let v = f64::from(me.get(k).copied().unwrap_or(0.0));
                if v > 0.0 {
                    present += 1;
                }
                total += v;
                if v > mx {
                    mx = v;
                }
            }
            let real: Vec<f64> = d.honest.iter().map(|&i| at_engine(&me, i)).collect();
            let real_mean = mean(&real);
            ef.push(present as f64 / 1000.0);
            et.push(total);
            eo.push(if real_mean > 0.0 { mx / real_mean } else { 0.0 });

            // —— 最小内核：没有账户表，只能枚举「有人提到过」的账户 ——
            // 空账户不在任何人的清单/观点里，所以分布里根本不存在它们。
            let mc = core_observer(&d, o, cap);
            let (mut present, mut total, mut mx) = (0usize, 0.0f64, 0.0f64);
            for i in 0..1000usize {
                let id = (1_000_000 + i) as u32;
                let v = f64::from(mc.get(&id).copied().unwrap_or(0.0));
                if v > 0.0 {
                    present += 1;
                }
                total += v;
                if v > mx {
                    mx = v;
                }
            }
            let real: Vec<f64> = d.honest.iter().map(|&i| at_core(&mc, i)).collect();
            let real_mean = mean(&real);
            cf.push(present as f64 / 1000.0);
            ct.push(total);
            co.push(if real_mean > 0.0 { mx / real_mean } else { 0.0 });
        }
        e_frac.push(mean(&ef));
        e_trust.push(mean(&et));
        e_over.push(mean(&eo));
        c_frac.push(mean(&cf));
        c_trust.push(mean(&ct));
        c_over.push(mean(&co));
    }
    (e_frac, e_trust, e_over, c_frac, c_trust, c_over)
}

// ══════════════════════════ S2 / S5：单来源的接收端行为 ══════════════════════════

fn engine_budget(view: &CompressedView, declared: &[usize], n: usize) -> f64 {
    let mut e = Engine::with_seed(
        EngineConfig {
            beta: 0.0,
            ..EngineConfig::default()
        },
        1,
    );
    for i in 0..n {
        e.intern(key(i));
    }
    e.rate(0, 1.0, 0);
    e.on_view(0, view.clone(), 0, 0);
    declared
        .iter()
        .map(|&i| f64::from(e.query(i as u32, 0)))
        .sum()
}

fn core_budget(pairs: &[(u32, f32)], cap: usize, declared: &[u32]) -> f64 {
    let v = SparseView::from_scores(cap, pairs);
    declared.iter().map(|&a| f64::from(v.normalized(a))).sum()
}

fn s2_s5_rows(cap: usize, params: ViewParams) -> Vec<(String, f64, f64, String)> {
    let n = 1000usize;
    let mut rows = Vec::new();
    let mk = |pairs: &[(usize, f32)]| -> CompressedView {
        CompressedView::from_pairs(
            params,
            &pairs.iter().map(|&(i, v)| (key(i), v)).collect::<Vec<_>>(),
        )
    };

    // ① 合规视图：10 条各 0.1。
    let pairs: Vec<(usize, f32)> = (0..10).map(|i| (i + 10, 0.1)).collect();
    let declared: Vec<usize> = pairs.iter().map(|x| x.0).collect();
    let e = engine_budget(&mk(&pairs), &declared, n);
    let c = core_budget(
        &pairs
            .iter()
            .map(|&(i, v)| (i as u32, v))
            .collect::<Vec<_>>(),
        cap,
        &declared.iter().map(|&x| x as u32).collect::<Vec<_>>(),
    );
    rows.push(("合规：10 条各 0.1".to_string(), e, c, "1.0".to_string()));

    // ② 超量：K 条全部声明 1000。
    for k in [2usize, 5, 10, 50, 200] {
        let declared: Vec<usize> = (10..10 + k).collect();
        let pairs: Vec<(usize, f32)> = declared.iter().map(|&i| (i, 1000.0)).collect();
        let e = engine_budget(&mk(&pairs), &declared, n);
        let c = core_budget(
            &pairs
                .iter()
                .map(|&(i, v)| (i as u32, v))
                .collect::<Vec<_>>(),
            cap,
            &declared.iter().map(|&x| x as u32).collect::<Vec<_>>(),
        );
        rows.push((format!("超量：{k} 条各 1000"), e, c, "1.0".to_string()));
    }

    // ③ 已登记 10 条 1.0 + 未登记 10 条 1.0。
    let declared: Vec<usize> = (10..20).collect();
    let mut pairs: Vec<(usize, f32)> = declared.iter().map(|&i| (i, 1.0)).collect();
    pairs.extend((5000..5010).map(|i| (i, 1.0)));
    let e = engine_budget(&mk(&pairs), &declared, n);
    let c = core_budget(
        &pairs
            .iter()
            .map(|&(i, v)| (i as u32, v))
            .collect::<Vec<_>>(),
        cap,
        &declared.iter().map(|&x| x as u32).collect::<Vec<_>>(),
    );
    rows.push((
        "伪造：10 已登记 + 10 未登记（各 1.0）".to_string(),
        e,
        c,
        "≤ 1.0".to_string(),
    ));

    // ④ 重复声明同一条 10 次。
    let pairs: Vec<(usize, f32)> = std::iter::repeat((10usize, 1.0f32)).take(10).collect();
    let e = engine_budget(&mk(&pairs), &[10], n);
    let c = core_budget(
        &pairs
            .iter()
            .map(|&(i, v)| (i as u32, v))
            .collect::<Vec<_>>(),
        cap,
        &[10],
    );
    rows.push((
        "伪造：同一账户声明 10 次（各 1.0）".to_string(),
        e,
        c,
        "≤ 1.0".to_string(),
    ));

    // ⑤ NaN。
    let e = engine_budget(
        &CompressedView::from_pairs(params, &[(key(10), f32::NAN)]),
        &[10],
        n,
    );
    let c = core_budget(&[(10, f32::NAN)], cap, &[10]);
    rows.push(("伪造：NaN".to_string(), e, c, "0".to_string()));

    // ⑥ 负分污染：10 号正 1.0，另有 200 条 -1.0。
    let mut pairs: Vec<(usize, f32)> = vec![(10, 1.0)];
    pairs.extend((200..400).map(|i| (i, -1.0)));
    let e = engine_budget(&mk(&pairs), &[10], n);
    let c = core_budget(
        &pairs
            .iter()
            .map(|&(i, v)| (i as u32, v))
            .collect::<Vec<_>>(),
        cap,
        &[10],
    );
    rows.push((
        "伪造：1 条 1.0 + 200 条 −1.0".to_string(),
        e,
        c,
        "1.0".to_string(),
    ));

    rows
}

// ══════════════════════════ S6：确定性 ══════════════════════════

fn shuffle(v: &mut [usize], seed: u64) {
    let mut s = seed | 1;
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let j = (s % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

fn engine_ordered(d: &Prepared, obs: usize, order: &[usize]) -> BTreeMap<u32, u32> {
    let mut e = Engine::with_seed(EngineConfig::default(), 1);
    for &i in order {
        e.intern(key(i));
    }
    // 注意：以 order 的顺序 intern，于是「世界编号」与「引擎内部编号」不再相等，
    // 后面每处都必须走 lookup，否则测的是错位而不是顺序无关性。
    for &i in order {
        if i == obs {
            for &(u, s) in &d.out[obs] {
                if let Some(idx) = e.lookup(&key(u as usize)) {
                    e.rate(idx, s, 0);
                }
            }
        }
    }
    for &i in order {
        if let Some(v) = &d.engine_views[i] {
            if d.out[obs].iter().any(|&(u, _)| u as usize == i) {
                if let Some(idx) = e.lookup(&key(i)) {
                    e.on_view(idx, v.clone(), 0, 0);
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    for (idx, val) in e.trust_distribution(0) {
        if let Some(k) = e.key_of(idx) {
            let acc = u64::from_le_bytes(k[..8].try_into().unwrap()) as u32;
            out.insert(acc, val.to_bits());
        }
    }
    out
}

fn core_ordered(d: &Prepared, obs: usize, order: &[usize], cap: usize) -> BTreeMap<u32, u32> {
    let mut n = Node::new(
        CoreConfig {
            cap,
            ..CoreConfig::default()
        },
        1,
    );
    for &i in order {
        if i == obs {
            for &(u, s) in &d.out[obs] {
                n.rate(u, s, 0);
            }
        }
    }
    for &i in order {
        if let Some(v) = &d.core_views[i] {
            if d.out[obs].iter().any(|&(u, _)| u as usize == i) {
                n.on_view(i as u32, v.clone(), 0, 0);
            }
        }
    }
    n.distribution(0)
        .entries
        .into_iter()
        .map(|(a, v)| (a, v.to_bits()))
        .collect()
}

fn s6(seeds: &[u64], cap: usize, params: ViewParams) -> (usize, usize, f64, f64) {
    let (mut e_ok, mut c_ok) = (0usize, 0usize);
    let (mut e_worst, mut c_worst) = (0.0f64, 0.0f64);
    let total = seeds.len();
    for &seed in seeds {
        let d = prepare(seed, AttackKind::None, cap, params);
        let obs = observers(&d, 1)[0];
        let mut fwd: Vec<usize> = (0..d.n).collect();
        let mut rev: Vec<usize> = (0..d.n).rev().collect();
        let mut rnd: Vec<usize> = (0..d.n).collect();
        shuffle(&mut rnd, seed * 7919 + 13);

        let e0 = engine_ordered(&d, obs, &fwd);
        let e1 = engine_ordered(&d, obs, &rev);
        let e2 = engine_ordered(&d, obs, &rnd);
        let c0 = core_ordered(&d, obs, &fwd, cap);
        let c1 = core_ordered(&d, obs, &rev, cap);
        let c2 = core_ordered(&d, obs, &rnd, cap);

        let same = |a: &BTreeMap<u32, u32>, b: &BTreeMap<u32, u32>| -> (bool, f64) {
            if a.len() != b.len() {
                return (false, f64::INFINITY);
            }
            let mut ok = true;
            let mut worst = 0.0f64;
            for (k, va) in a {
                match b.get(k) {
                    Some(vb) => {
                        if va != vb {
                            ok = false;
                            let fa = f32::from_bits(*va) as f64;
                            let fb = f32::from_bits(*vb) as f64;
                            let den = fa.abs().max(fb.abs()).max(1e-12);
                            worst = worst.max((fa - fb).abs() / den);
                        }
                    }
                    None => return (false, f64::INFINITY),
                }
            }
            (ok, worst)
        };

        let (a1, w1) = same(&e0, &e1);
        let (a2, w2) = same(&e0, &e2);
        if a1 && a2 {
            e_ok += 1;
        }
        e_worst = e_worst.max(w1).max(w2);
        let (b1, v1) = same(&c0, &c1);
        let (b2, v2) = same(&c0, &c2);
        if b1 && b2 {
            c_ok += 1;
        }
        c_worst = c_worst.max(v1).max(v2);
        fwd.clear();
        rev.clear();
    }
    let _ = total;
    (e_ok, c_ok, e_worst, c_worst.min(1.0))
}

// ══════════════════════════ main ══════════════════════════

// ══════════════════════════ 定点精度与截断（判决问题 2、3） ══════════════════════════

/// 精确参考实现：不量化、不截断、全 f64。只用于量化「u16 + cap」损失了多少。
fn exact_dist(d: &Prepared, obs: usize, beta: f64) -> BTreeMap<u32, f64> {
    let total: f64 = d.out[obs].iter().map(|&(_, s)| f64::from(s)).sum();
    if !(total > 0.0) {
        return BTreeMap::new();
    }
    let mut out: BTreeMap<u32, f64> = BTreeMap::new();
    for &(u, s) in &d.out[obs] {
        *out.entry(u).or_insert(0.0) += beta * (f64::from(s) / total);
    }
    let mut num: BTreeMap<u32, f64> = BTreeMap::new();
    let mut wsum = 0.0f64;
    for &(src, s) in &d.out[obs] {
        let share = f64::from(s) / total;
        let view = &d.out[src as usize];
        let vt: f64 = view.iter().map(|&(_, x)| f64::from(x)).sum();
        if !(vt > 0.0) || !(share > 0.0) {
            continue;
        }
        wsum += share;
        for &(t, x) in view {
            *num.entry(t).or_insert(0.0) += share * (f64::from(x) / vt);
        }
    }
    if wsum > 0.0 {
        let coef = (1.0 - beta) / wsum;
        for (&k, &g) in &num {
            *out.entry(k).or_insert(0.0) += coef * g;
        }
    }
    let mass: f64 = out.values().sum();
    if mass > 0.0 {
        for v in out.values_mut() {
            *v /= mass;
        }
    }
    out
}

fn prec() {
    let params = ViewParams::default();
    let beta = 0.3f64;
    let mut seeds = Vec::new();
    let mut per_cap: BTreeMap<usize, (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for cap in [128usize, 256, 512, 4096] {
        per_cap.insert(cap, (Vec::new(), Vec::new(), Vec::new(), Vec::new()));
    }
    let mut trunc: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    for cap in [128usize, 256, 512] {
        trunc.insert(cap, Vec::new());
    }
    let mut lens: Vec<usize> = Vec::new();
    let mut missing_by_cap: BTreeMap<usize, usize> = BTreeMap::new();

    for seed in 1..=20u64 {
        let d = prepare(seed, AttackKind::Infiltrator, 4096, params);
        let obs = observers(&d, 1)[0];
        let ex = exact_dist(&d, obs, beta);
        let target = d.promoted.unwrap();
        let ex_h: Vec<f64> = d
            .honest
            .iter()
            .map(|&i| *ex.get(&(i as u32)).unwrap_or(&0.0))
            .collect();
        let ex_ratio = ex.get(&(target as u32)).copied().unwrap_or(0.0) / mean(&ex_h);
        seeds.push(ex_ratio);

        for cap in [128usize, 256, 512, 4096] {
            let mc = core_observer(&d, obs, cap);
            let mut rels: Vec<f64> = Vec::new();
            for (&k, &e) in &ex {
                if e <= 0.0 {
                    continue;
                }
                match mc.get(&k) {
                    Some(&q) => rels.push((f64::from(q) - e).abs() / e),
                    None => {
                        *missing_by_cap.entry(cap).or_insert(0) += 1;
                        rels.push(1.0);
                    }
                }
            }
            rels.sort_by(|a, b| a.total_cmp(b));
            let m = mean(&rels);
            let p95 = rels[((rels.len() as f64 * 0.95) as usize).min(rels.len() - 1)];
            let mx = *rels.last().unwrap_or(&0.0);
            let c_h: Vec<f64> = d
                .honest
                .iter()
                .map(|&i| f64::from(mc.get(&(i as u32)).copied().unwrap_or(0.0)))
                .collect();
            let c_ratio = f64::from(mc.get(&(target as u32)).copied().unwrap_or(0.0)) / mean(&c_h);
            let slot = per_cap.get_mut(&cap).unwrap();
            slot.0.push(m);
            slot.1.push(p95);
            slot.2.push(mx);
            slot.3.push((c_ratio - ex_ratio).abs() / ex_ratio);
        }

        for &(src, _) in &d.out[obs] {
            let view = &d.out[src as usize];
            let total: f32 = view.iter().map(|&(_, s)| s).sum();
            let pairs: Vec<(u32, f32)> = view.iter().map(|&(t, s)| (t, s / total)).collect();
            lens.push(view.len());
            for cap in [128usize, 256, 512] {
                trunc
                    .get_mut(&cap)
                    .unwrap()
                    .push(SparseView::truncated_mass(cap, &pairs));
            }
        }
    }

    lens.sort_unstable();
    println!("## 正分清单长度（20 seed × 观察者视角下的全部来源视图）");
    println!(
        "条数：中位 {}，p95 {}，max {}",
        lens[lens.len() / 2],
        lens[(lens.len() * 95 / 100).min(lens.len() - 1)],
        lens[lens.len() - 1]
    );

    println!("\n## 截断损失（真实视图，按分数保留 top-cap）");
    println!("| cap | 平均丢掉的质量 | 被截断的视图占比 |");
    println!("|---|---|---|");
    for cap in [128usize, 256, 512] {
        let v = &trunc[&cap];
        let nz = v.iter().filter(|&&x| x > 0.0).count() as f64 / v.len() as f64;
        println!("| {cap} | {:.4}% | {:.1}% |", mean(v) * 100.0, nz * 100.0);
    }

    println!("\n## u16 定点 + cap 的误差（相对精确 f64 参考，逐账户）");
    println!("| cap | 平均相对误差 | p95 | 最大 | S3 倍数偏移 |");
    println!("|---|---|---|---|---|");
    for cap in [4096usize, 512, 256, 128] {
        let s = &per_cap[&cap];
        println!(
            "| {cap} | {:.3e} | {:.3e} | {:.3e} | {:.3e} |",
            mean(&s.0),
            mean(&s.1),
            s.2.iter().cloned().fold(0.0, f64::max),
            mean(&s.3)
        );
    }
    println!("\n被截断掉的账户数（总和）：{:?}", missing_by_cap);
    println!("S3 倍数（精确参考）：{:.6}", mean(&seeds));

    println!("\n## 截断损失随清单长度的外推（合成 Zipf(s=1.2)，**不是实测**）");
    println!("| 清单长度 | cap=128 | cap=256 | cap=512 |");
    println!("|---|---|---|---|");
    for l in [128usize, 256, 512, 1024, 2048] {
        let mut scores: Vec<f32> = (1..=l).map(|i| 1.0 / (i as f32).powf(1.2)).collect();
        let total: f32 = scores.iter().sum();
        for s in scores.iter_mut() {
            *s /= total;
        }
        let pairs: Vec<(u32, f32)> = scores
            .iter()
            .enumerate()
            .map(|(i, &s)| (i as u32, s))
            .collect();
        let mut row = format!("| {l} |");
        for cap in [128usize, 256, 512] {
            row.push_str(&format!(
                " {:.2}% |",
                SparseView::truncated_mass(cap, &pairs) * 100.0
            ));
        }
        println!("{row}");
    }
}

// ══════════════════════════ 诊断：S6 的 engine 差异从哪来 ══════════════════════════

fn diag6() {
    let cap = 512usize;
    let params = ViewParams::default();
    let d = prepare(1, AttackKind::None, cap, params);
    let obs = observers(&d, 1)[0];
    let fwd: Vec<usize> = (0..d.n).collect();
    let rev: Vec<usize> = (0..d.n).rev().collect();
    let a = engine_ordered(&d, obs, &fwd);
    let b = engine_ordered(&d, obs, &rev);
    println!("长度 {} vs {}", a.len(), b.len());
    let mut diffs: Vec<(u32, f32, f32, f64)> = Vec::new();
    for (k, va) in &a {
        if let Some(vb) = b.get(k) {
            if va != vb {
                let fa = f32::from_bits(*va);
                let fb = f32::from_bits(*vb);
                let den = fa.abs().max(fb.abs()).max(1e-12) as f64;
                diffs.push((*k, fa, fb, (fa - fb).abs() as f64 / den));
            }
        }
    }
    diffs.sort_by(|x, y| y.3.total_cmp(&x.3));
    println!("不同的键 {} 个；前 10：", diffs.len());
    for x in diffs.iter().take(10) {
        println!(
            "  账户 {:>4}: {:e} vs {:e}  相对差 {:.3e}",
            x.0, x.1, x.2, x.3
        );
    }
    // 同一 intern 顺序、只改 rate/on_view 顺序
    let c = engine_ordered(&d, obs, &fwd);
    let same = a == c;
    println!("同顺序重复：{}", same);
    {
        let sa: f64 = a.values().map(|&x| f32::from_bits(x) as f64).sum();
        let sb: f64 = b.values().map(|&x| f32::from_bits(x) as f64).sum();
        let mut va: Vec<f32> = a.values().map(|&x| f32::from_bits(x)).collect();
        let mut vb: Vec<f32> = b.values().map(|&x| f32::from_bits(x)).collect();
        va.sort_by(|x, y| x.total_cmp(y));
        vb.sort_by(|x, y| x.total_cmp(y));
        let multiset_same = va == vb;
        println!("sum a={sa:.9} b={sb:.9}；值多重集相同 = {multiset_same}");
        println!(
            "a[76]={:e} b[76]={:e}",
            f32::from_bits(*a.get(&76).unwrap()),
            f32::from_bits(*b.get(&76).unwrap())
        );
        println!("a 前 5 = {:?}", va.iter().rev().take(5).collect::<Vec<_>>());
        println!("b 前 5 = {:?}", vb.iter().rev().take(5).collect::<Vec<_>>());
    }
    // 逐项对比内部状态
    for (name, order) in [("正序", &fwd), ("逆序", &rev)] {
        let mut e = Engine::with_seed(EngineConfig::default(), 1);
        for &i in order {
            e.intern(key(i));
        }
        for &i in order {
            if i == obs {
                for &(u, s) in &d.out[obs] {
                    if let Some(idx) = e.lookup(&key(u as usize)) {
                        e.rate(idx, s, 0);
                    }
                }
            }
        }
        for &i in order {
            if let Some(v) = &d.engine_views[i] {
                if d.out[obs].iter().any(|&(u, _)| u as usize == i) {
                    if let Some(idx) = e.lookup(&key(i)) {
                        e.on_view(idx, v.clone(), 0, 0);
                    }
                }
            }
        }
        let mut qsum = 0.0f64;
        for i in 0..d.n {
            qsum += f64::from(e.query(e.lookup(&key(i)).unwrap(), 0));
        }
        let dist = e.trust_distribution(0);
        let i76 = e.lookup(&key(76)).unwrap();
        let d76 = dist
            .iter()
            .find(|x| x.0 == i76)
            .map(|x| x.1)
            .unwrap_or(-1.0);
        println!(
            "{name}: stats={:?} pos={} q76={:e} Σquery={qsum:.9} dist76={d76:e} dist_len={}",
            e.stats(),
            e.positive_count(),
            e.query(e.lookup(&key(76)).unwrap(), 0),
            dist.len()
        );
    }
    // 引擎内部编号是否改变
    let mut e1 = Engine::with_seed(EngineConfig::default(), 1);
    for &i in &fwd {
        e1.intern(key(i));
    }
    let mut e2 = Engine::with_seed(EngineConfig::default(), 1);
    for &i in &rev {
        e2.intern(key(i));
    }
    println!(
        "观察者内部编号：正序 {} / 逆序 {}",
        e1.lookup(&key(obs)).unwrap(),
        e2.lookup(&key(obs)).unwrap()
    );
}

// ══════════════════════════ 诊断：S4 的噪声到底从哪来 ══════════════════════════

fn diag() {
    let cap = 512usize;
    let params = ViewParams::default();
    let d = prepare(1, AttackKind::None, cap, params);
    let obs = observers(&d, 1)[0];
    let empties: Vec<[u8; 32]> = (0..1000).map(|i| key(1_000_000 + i)).collect();
    println!("观察者 {obs} 的正分清单 {} 条", d.out[obs].len());
    println!(
        "每个来源视图的条目数：{}",
        d.out[obs]
            .iter()
            .map(|&(u, _)| d.engine_views[u as usize]
                .as_ref()
                .map_or(0, |v| v.buckets()))
            .take(0)
            .count()
    );

    // 每份视图的噪声中位数（on_view 里做阈值的那个量）
    let mut noises: Vec<f32> = Vec::new();
    let mut masses: Vec<f32> = Vec::new();
    let mut lens: Vec<usize> = Vec::new();
    for &(u, _) in &d.out[obs] {
        let idx = u as usize;
        let v = d.engine_views[idx].as_ref().unwrap();
        lens.push(d.out[idx].len());
        let n = v.noise_floor(1000, 0xDEAD_BEEF ^ idx as u64);
        noises.push(n.median);
        // 接收端估计的总质量：遍历全部已知账户（世界 + 1000 个空账户）
        let mut tm = 0.0f32;
        for i in 0..d.n {
            tm += v.query(&key(i)).clamp(0.0, 1.0);
        }
        for k in &empties {
            tm += v.query(k).clamp(0.0, 1.0);
        }
        masses.push(tm.max(1e-6));
    }
    let mut s = noises.clone();
    s.sort_by(|a, b| a.total_cmp(b));
    println!(
        "视图噪声中位数：min {:e} 中位 {:e} max {:e}，其中 >0 的 {} 份（共 {}）",
        s[0],
        s[s.len() / 2],
        s[s.len() - 1],
        s.iter().filter(|&&x| x > 0.0).count(),
        s.len()
    );
    let mut s = masses.clone();
    s.sort_by(|a, b| a.total_cmp(b));
    println!(
        "接收端估计的总质量：min {:.4} 中位 {:.4} max {:.4}",
        s[0],
        s[s.len() / 2],
        s[s.len() - 1]
    );
    lens.sort_unstable();
    println!(
        "来源清单条目数：min {} 中位 {} max {}",
        lens[0],
        lens[lens.len() / 2],
        lens[lens.len() - 1]
    );

    // 空账户在最终分布里的取值
    let me = engine_observer(&d, obs, &empties);
    let mut vals: Vec<f64> = empties
        .iter()
        .map(|k| f64::from(me.get(k).copied().unwrap_or(0.0)))
        .collect();
    let real: Vec<f64> = d.honest.iter().map(|&i| at_engine(&me, i)).collect();
    let rm = mean(&real);
    vals.sort_by(|a, b| b.total_cmp(a));
    println!(
        "空账户：非零 {} 个；前 8 大 {}",
        vals.iter().filter(|&&v| v > 0.0).count(),
        vals.iter()
            .take(8)
            .map(|v| format!("{v:.2e}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("诚实账户均值 {rm:.2e}，中位 {:.2e}", {
        let mut r = real.clone();
        r.sort_by(|a, b| a.total_cmp(b));
        r[r.len() / 2]
    });

    // 每份视图到底命中了几个空账户
    let mut hits = vec![0usize; 1000];
    let mut per_view = Vec::new();
    for &(u, _) in &d.out[obs] {
        let v = d.engine_views[u as usize].as_ref().unwrap();
        let mut c = 0usize;
        for (i, k) in empties.iter().enumerate() {
            if v.query(k) != 0.0 {
                c += 1;
                hits[i] += 1;
            }
        }
        per_view.push(c);
    }
    let total_hits: usize = per_view.iter().sum();
    let touched = hits.iter().filter(|&&c| c > 0).count();
    let mut hist = [0usize; 6];
    for &h in &hits {
        hist[h.min(5)] += 1;
    }
    println!("198 份视图合计命中 {total_hits} 次；至少命中一次的空账户 {touched} 个");
    println!("命中次数直方图 [0,1,2,3,4,5+] = {hist:?}");

    // 对照：如果每份视图只有 20 条（稀疏清单），噪声会掉多少
    for l in [20usize, 50, 105, 200, 512] {
        let pairs: Vec<([u8; 32], f32)> = (0..l).map(|i| (key(i), 1.0 / l as f32)).collect();
        let v = CompressedView::from_pairs(params, &pairs);
        let mut cnt = 0usize;
        for i in 0..1000 {
            if v.query(&key(2_000_000 + i)) != 0.0 {
                cnt += 1;
            }
        }
        println!(
            "单份视图 {l} 条时，1000 个陌生 key 里 {} 个读数非零（{:.2}%）",
            cnt,
            cnt as f64 / 10.0
        );
    }
}

fn main() {
    if std::env::args().any(|a| a == "--diag") {
        diag();
        return;
    }
    if std::env::args().any(|a| a == "--diag6") {
        diag6();
        return;
    }
    if std::env::args().any(|a| a == "--prec") {
        prec();
        return;
    }
    let seeds: Vec<u64> = (1..=20).collect();
    let cap = 512usize;
    let params = ViewParams::default();

    // 图的基本信息
    {
        let d = prepare(1, AttackKind::None, cap, params);
        let avg = d.out.iter().map(|x| x.len()).sum::<usize>() as f64 / d.n as f64;
        println!("图：n = {}，平均正分清单 {:.1} 条", d.n, avg);
        let ds = prepare(1, AttackKind::Sybil { cluster: 2 }, cap, params);
        println!(
            "女巫场景：n = {}，女巫 {} 个，诚实 {} 个",
            ds.n,
            ds.sybils.len(),
            ds.honest.len()
        );
    }

    let (e1m, e1x, e1f, c1m, c1x, c1f) = s1(&seeds, cap, params);
    println!("\n## S1 女巫集群在诚实观察者眼里的声望");
    println!("| 指标 | Engine | mincore |");
    println!("|---|---|---|");
    println!(
        "| 平均 | {} | {} |",
        pm(mean(&e1m), std(&e1m)),
        pm(mean(&c1m), std(&c1m))
    );
    println!(
        "| 最大 | {} | {} |",
        pm(mean(&e1x), std(&e1x)),
        pm(mean(&c1x), std(&c1x))
    );
    println!(
        "| 非零占比 | {} | {} |",
        pm(mean(&e1f), std(&e1f)),
        pm(mean(&c1f), std(&c1f))
    );

    println!("\n## S2/S5 单来源接收端 Σv（beta=0，单来源权重 1）");
    println!("| 场景 | Engine | mincore | 合规 |");
    println!("|---|---|---|---|");
    for (name, e, c, want) in s2_s5_rows(cap, params) {
        println!("| {name} | {e:.4} | {c:.4} | {want} |");
    }

    let (e3r, c3r, _e3v, _c3v) = s3(&seeds, cap, params);
    println!("\n## S3 被渗透者抬举的账户 ÷ 诚实账户均值");
    println!("| 指标 | Engine | mincore |");
    println!("|---|---|---|");
    println!(
        "| 倍数 | {} | {} |",
        pm(mean(&e3r), std(&e3r)),
        pm(mean(&c3r), std(&c3r))
    );

    let (e4f, e4t, e4o, c4f, c4t, c4o) = s4(&seeds, cap, params);
    println!("\n## S4 1000 个只注册不评分的账户");
    println!("| 指标 | Engine | mincore |");
    println!("|---|---|---|");
    println!(
        "| 出现率 | {} | {} |",
        pm(mean(&e4f), std(&e4f)),
        pm(mean(&c4f), std(&c4f))
    );
    println!(
        "| 拿走的总信任 | {} | {} |",
        pm_e(mean(&e4t), std(&e4t)),
        pm_e(mean(&c4t), std(&c4t))
    );
    println!(
        "| 最大值÷真实均值 | {} | {} |",
        pm(mean(&e4o), std(&e4o)),
        pm(mean(&c4o), std(&c4o))
    );

    let (e6, c6, e6w, c6w) = s6(&seeds, cap, params);
    println!("\n## S6 确定性（正序/逆序/随机序逐位相同）");
    println!("| 实现 | 逐位相同的 seed 数 | 最大相对差 |");
    println!("|---|---|---|");
    println!("| Engine | {e6}/{} | {e6w:.3e} |", seeds.len());
    println!("| mincore | {c6}/{} | {c6w:.3e} |", seeds.len());

    // 体积
    println!("\n## 体积");
    let eng_bytes = CompressedView::new(params).bytes();
    println!("| 实现 | 每份视图字节 | 相对 |");
    println!("|---|---|---|");
    println!("| Engine（2048×5 f32） | {eng_bytes} | 100% |");
    for c in [128usize, 256, 512] {
        let b = SparseView::new(c).bytes();
        println!(
            "| mincore cap={c} | {b} | {:.1}% |",
            b as f64 / eng_bytes as f64 * 100.0
        );
    }
}
