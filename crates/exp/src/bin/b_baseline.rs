//! B0 —— P0 三个修复之后的 S1~S6 新基线（本仓库第一份可信基线）。
//!
//! 【为什么必须重跑】P0 改的都是 Engine::query 的归一化口径：
//! * crates/engine/src/lib.rs:751-787：群体项分母从「已缓存观点的来源」改成
//!   「我评过的全部来源」（没观点的来源进分母、投弃权票）；
//! * crates/engine/src/lib.rs:690-694：读数「分子先夹到 [0,1] 再除以 total_mass」；
//! * crates/engine/src/lib.rs:767-771：mass_covers —— 观点缓存之后才登记的账户
//!   按「没表态」处理。
//!
//! 三条都是结构性改动，审计/A6-ablation.md 与 A1~A13 / B1 / C1 / C2 里全部 S1~S6
//! 数字都出自改前口径，只能读方向、不能当基线（交接文档 7.1 节）。本 bin 用同一套
//! 判据定义、同一组参数、同一组 seed 在真 Engine 上重跑，给出新基线。
//!
//! 【判据定义】唯一成文出处是审计/A6-ablation.md 第 52-64 行，逐条照抄：
//! * 观察者 = 活着的诚实账户；分布 = trust_distribution(now)，总和恒为 1；
//! * hm = 该观察者眼中「所有诚实账户」的均值（没出现在分布里的按 0 计入）；
//! * 所有比值 = 该账户在分布里的值 ÷ hm；
//! * S1 = 女巫集群成员均值 / hm；另报「出现率」（分布里非零比例）；
//! * S2 = 注入女巫均值 / hm；越线率 = 超过 hm 的（观察者,女巫）对数 ÷ 全部对数；
//! * S3 = 收益方 Promoted 的 dist 值 / hm；另报攻击者自己 / hm；
//! * S4 = 40 个「已 intern 但没人评分」的新密钥：均值 / hm、单点最高 / hm、出现率；
//! * S5 = 把已被诚实观察者评分的来源的观点换成伪造视图后，它自己的声望 ÷ 伪造前；
//! * S6 = 同输入重跑，全部（观察者 × 账户）声望矩阵的最大绝对差。
//!
//! 【与 A6 实现的两处口径差异（本 bin 刻意修正，报告里单列）】
//!
//! 1. hm 的分母。A6 的 collect_rows 实测写成 hm = Σ(分布里出现的诚实账户) / 出现个数，
//!    而它给 S1~S4 的类均值用的是「该类账户总数（缺失按 0）」—— 同一份报告里两套
//!    分母口径不一致，但成文的判据定义写的是「所有诚实账户的均值」。本文按成文定义
//!    取 hm = Σ / 活着的诚实账户总数（缺失按 0）。
//!    换算关系只有一条：hm_present = hm / present_frac。所以拿本文的比值去和 A6 旧
//!    基线比「同一口径」时，除以 present_frac 即可；present_frac 本身也逐 seed 输出，
//!    读者可以自己复算。
//! 2. S3 的 1/β 解析下限。β = 0.3 时，只要「观察者自己评过收益方、且群体项把它顶满」，
//!    它相对「一个我只评过、但没人背书的诚实账户」的比值必然 ≥ 1/β = 3.3333
//!    （推导见审计总账.md 第 70-75 行：D[T] = β/N + (1−β)/N = 1/N，D[诚实] = β/N）。
//!    所以 S3 不是「过不过」的问题，而是「离 3.33 多远」；本 bin 输出 s3_floor、
//!    s3_vs_floor（均值 ÷ 3.3333）与 s3_personal / s3_group（β·个人项 ÷ hm 与
//!    群体项 ÷ hm 的分解）。
//!
//! 【与旧基线的可比性】引擎随机流种子沿用 A6 的 0xA6A6_... 配方（同样的 seed 与账户
//! 编号 → 同样的抽样序列）。所以「本文比值 / present_frac」与 A6 第 2 节基线相减时，
//! 差异只来自 P0 代码改动。报告里必须写清楚这一点，否则会把口径差当成 P0 效应。
//!
//! 【参数】逐位照抄审计/A6-ablation.md 第 41-47 行：
//! * 人口：n_initial = 200，跑 100 步（= 100 小时），join_rate = 0.15、
//!   leave_rate = 0.00012；
//! * 关注：follow_min = 20 / follow_max = 30 / max_follows = 40；
//! * 传播：世界结束后交换 4 轮，每轮 24 小时，最终 now = 100 + 3×24 = 172；
//! * 抽样：每轮每引擎 tick(now, 12)；
//! * 视图：ViewParams { buckets: 2048, hashers: 5, seed: 99 }；
//! * 引擎：AggMode::Mean、β = 0.3、half_life = 72，其余全默认（噪声底 1.0、
//!   衰减开启、无 badlist 折扣）；
//! * 攻击：attack_ratio = 0.08，三种场景 Sybil { cluster: 3 } / Infiltrator /
//!   Endorser { ratio: 0.2 }，后两者另带 1 个 Promoted 收益方；
//! * seed = 11..18，共 8 个。
//!
//! 【用法】（在 crates/exp 目录下）
//!   cargo run --release --bin b_baseline                  # 3 场景 × 8 seed（默认）
//!   cargo run --release --bin b_baseline -- --seeds 1     # 冒烟：只跑 seed 11
//!   cargo run --release --bin b_baseline -- --scenarios sybil,infiltrator
//!   cargo run --release --bin b_baseline -- --first 11 --seeds 8
//!
//! 【输出】全部 tab 分隔：PARAM 参数块 / BASE 每个 seed 的场景规模 /
//! SEED 每 seed 每指标 / SUM 每个指标跨 seed 的 mean sd min max n / FLOOR 1/β /
//! DONE 耗时。读数方向：S1/S2/S4/S5 越小越好，S6 必须为 0，S3 的参照是 1/β 而不是 1。

use std::collections::{BTreeMap, BTreeSet};

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

// ============================== 固定参数 ==============================

/// 世界初始人口（审计/A6-ablation.md 第 41 行）。
const N_INITIAL: usize = 200;
/// 世界推进步数，一步一小时。
const STEPS: usize = 100;
/// 世界结束后的交换轮数。
const ROUNDS: u64 = 4;
/// 每轮多少小时；最终统计时刻 = STEPS + (ROUNDS − 1) × HPR。
const HPR: u64 = 24;
/// 每轮每引擎的抽样条数（审计/A6-ablation.md 第 44 行）。
const SAMPLES: usize = 12;
/// S4：已 intern、但没有被任何人评分的空账户个数。
const EMPTY_COUNT: usize = 40;
/// S2：攻击者注入的新账户个数（每个 1.0 分）。
const INJECT_COUNT: usize = 40;
/// S5「塞壳」用的已认识空壳账户个数（与 A6 一致，便于横向对照）。
const FAKE_COUNT: usize = 200;
/// 个人权重 β（EngineConfig::default 也是 0.3，这里显式写出来当参数记录）。
const BETA: f32 = 0.3;
/// 来源级半衰期（小时）。
const HALF_LIFE: u64 = 72;
/// 引擎随机流盐值。刻意与 A6 完全相同：同样的 (seed, 账户编号) 得到同样的抽样序列。
const ENGINE_SEED_SALT: u64 = 0xA6A6_0000_0000_0000;
/// 输出精度（8 位小数，固定格式，便于文本对齐与脚本解析）。
const DIGITS: usize = 8;

/// S3 的解析下限：1/β（β = 0.3 ⇒ 3.3333）。
///
/// 见审计/审计总账.md 第 70-75 行：
///   包含沉默：group[u] = 0.5 → 比值 = 1/β = 3.33
///   排除沉默：group[u] = 1.0 → 比值 = 1 + (1−β)/(β·0.5) = 5.67
/// 也就是说：只要「我把收益方当成我评过的来源之一、且群体项对它满值」，它相对
/// 「一个我只评过、但没有任何人背书的诚实账户」的比值必然 ≥ 1/β。
const ANALYTIC_FLOOR_S3: f64 = 1.0 / (BETA as f64);

// ============================== 基础小工具 ==============================

/// 由内部编号合成 32 字节账户号（与 A6 / multinode 完全一致）。
///
/// 注意 k[8] = 0xA5：空账户与真实账户共用这个格式，S4 的碰撞率依赖它，
/// 换成别的格式就无法与旧基线横向对照了。
fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

/// 全网约定的视图参数（审计/A6-ablation.md 第 45 行）。
fn view_params() -> ViewParams {
    ViewParams {
        buckets: 2048,
        hashers: 5,
        seed: 99,
    }
}

/// 引擎配置：只写实验要钉死的字段，其余（噪声底 1.0、衰减开启、无 badlist 折扣）
/// 走 EngineConfig::default。
fn engine_config() -> EngineConfig {
    EngineConfig {
        beta: BETA,
        half_life: HALF_LIFE,
        view_params: view_params(),
        agg_mode: AggMode::Mean,
        ..Default::default()
    }
}

/// 8 位定点格式化，非有限值一律输出 NA（报告里不出现 NaN / inf）。
fn fmt8(x: f64) -> String {
    if x.is_finite() {
        format!("{:.*}", DIGITS, x)
    } else {
        "NA".to_string()
    }
}

/// 哪些指标是「个数」而不是「比值」（输出时省掉小数尾巴）。
fn is_count_metric(name: &str) -> bool {
    name.starts_with("n_")
        || name.starts_with("obs_")
        || matches!(
            name,
            "s2_cross_n" | "s2_pairs_n" | "s3_obs_rated" | "s6_len"
        )
}

/// 按指标类型格式化：计数走整数，比值走 8 位定点。
fn fmt_metric(name: &str, x: f64) -> String {
    if is_count_metric(name) && x.is_finite() {
        format!("{:.0}", x)
    } else {
        fmt8(x)
    }
}

/// 平均值：只取有限值；没有有限值时返回 NaN（而不是 0）。
fn avg(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 样本标准差（分母 n − 1，与 A6 一致）；n < 2 返回 NaN。
fn sd(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.len() < 2 {
        return f64::NAN;
    }
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() - 1) as f64;
    var.sqrt()
}

/// 最小值：只取有限值；空集返回 NaN。
fn min_of(v: &[f64]) -> f64 {
    v.iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(f64::NAN, f64::min)
}

/// 最大值：只取有限值；空集返回 NaN。
fn max_of(v: &[f64]) -> f64 {
    v.iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(f64::NAN, f64::max)
}

/// 有限值个数（报告里的 n）。
fn finite_len(v: &[f64]) -> usize {
    v.iter().filter(|x| x.is_finite()).count()
}

// ============================== 场景 ==============================

/// 一个场景 = 一个世界 + 世界结束后追加的若干「实验用账户」。
///
/// 账户编号空间（与 A6 逐位一致，S4 的碰撞率依赖这个顺序）：
///   0 .. n_world          世界里的真实账户（诚实/女巫/渗透/背书/Promoted…）
///   n_world .. +40        空账户（只 intern、没人评分）        → S4
///   +40 .. +240           伪造外壳账户（S5 塞壳用）
///   +240 .. +280          注入女巫（S2，攻击者每人 1.0 分）   → S2
struct Scenario {
    /// 场景名（sybil / infiltrator / endorser），决定输出哪些判据。
    name: &'static str,
    /// 账户总数（世界账户 + 追加账户）。
    n: usize,
    /// 世界里真实账户的个数（追加账户的编号起点）。
    n_world: usize,
    /// 世界原始评分（未注入），用于 S5 的「诚实观察者给我打过分的度数」。
    raw: Vec<(usize, usize, f32)>,
    /// 送进引擎的评分 = 世界原始评分 + S2 注入的 40 × 1.0。
    ratings: Vec<(usize, usize, f32)>,
    /// 活着的账户（引擎要建在这些账户上：攻击者也要建，否则攻击者的观点会被
    /// 系统性抹掉，S2 比值恒为 0 —— 交接文档 8.1 节）。
    alive: Vec<usize>,
    /// 活着的诚实账户（= 观察者集合）。
    honest: Vec<usize>,
    /// 女巫集群成员（S1 的分子；不要求「活着」，与 A6 一致）。
    sybil: Vec<usize>,
    /// 收益方（Promoted）；女巫场景的 target 是女巫自己，因此可能为 None。
    benef: Option<usize>,
    /// 攻击者（Kind::is_attacker，不含 Promoted）。
    attackers: Vec<usize>,
    /// S2 注入的 40 个新账户。
    injected: Vec<usize>,
    /// S4 的 40 个空账户。
    empty: Vec<usize>,
    /// S5 的 200 个伪造外壳账户。
    fake: Vec<usize>,
    /// S2 的注入者：世界结束后「评分对象最多」的攻击者。
    injector: Option<usize>,
}

impl Scenario {
    /// 诚实账户集合（判断某个编号是不是诚实观察者）。
    fn honest_set(&self) -> BTreeSet<usize> {
        self.honest.iter().copied().collect()
    }

    /// 观察者 = 活着的诚实账户。
    fn observers(&self) -> Vec<usize> {
        let hs = self.honest_set();
        self.alive
            .iter()
            .copied()
            .filter(|a| hs.contains(a))
            .collect()
    }

    /// 最终统计时刻：世界跑完 100 小时后再铺开 3 个 24 小时间隔 ⇒ 172。
    fn final_now(&self) -> u64 {
        STEPS as u64 + (ROUNDS - 1) * HPR
    }

    /// 场景规模的一行描述（报告「场景规模」表直接用）。
    fn size_line(&self) -> String {
        format!(
            "world={} total={} alive={} honest={} attackers={} sybil={} benef={} injector={} empty={} fake={} injected={}",
            self.n_world,
            self.n,
            self.alive.len(),
            self.honest.len(),
            self.attackers.len(),
            self.sybil.len(),
            self.benef.map(|b| b as i64).unwrap_or(-1),
            self.injector.map(|x| x as i64).unwrap_or(-1),
            self.empty.len(),
            self.fake.len(),
            self.injected.len()
        )
    }

    /// 女巫集群内部的原始自评分均值（S1 的「原始自评分 1.000」参照）。
    ///
    /// 只统计「女巫 → 女巫」的评分；集群内互评满分 1.0 是 S1 的注入面。
    fn sybil_raw_self(&self) -> f64 {
        let s: BTreeSet<usize> = self.sybil.iter().copied().collect();
        let mut sum = 0.0f64;
        let mut cnt = 0usize;
        for &(o, t, v) in &self.raw {
            if s.contains(&o) && s.contains(&t) {
                sum += v as f64;
                cnt += 1;
            }
        }
        if cnt == 0 {
            f64::NAN
        } else {
            sum / cnt as f64
        }
    }
}

/// 场景 → 世界配置。attack_ratio 恒为 0.08（审计/A6-ablation.md 第 46 行），
/// 三种场景的区别只在 AttackKind：女巫 16 人 3 集群 / 渗透 16 人 / 背书 40 人。
fn scenario_cfg(name: &str, seed: u64) -> SimConfig {
    let kind = match name {
        "sybil" => AttackKind::Sybil { cluster: 3 },
        "infiltrator" => AttackKind::Infiltrator,
        "endorser" => AttackKind::Endorser { ratio: 0.2 },
        _ => AttackKind::None,
    };
    SimConfig {
        n_initial: N_INITIAL,
        steps: STEPS,
        seed,
        join_rate: 0.15,
        leave_rate: 0.00012,
        follow_min: 20,
        follow_max: 30,
        max_follows: 40,
        ..Default::default()
    }
    .with_attack(kind, if kind.is_none() { 0.0 } else { 0.08 })
}

/// 造一个默认参数（200 人 / 100 步）的场景。
fn build_scenario(name: &'static str, seed: u64) -> Scenario {
    build_scenario_with_cfg(name, scenario_cfg(name, seed))
}

/// 造一个场景：跑世界 → 收集各类账户 → 追加空账户 / 外壳 / 注入女巫。
///
/// 与 A6 的 build_scenario 逐行对齐（连「注入评分对三个场景都追加」这一点也一样，
/// 保证与旧基线可比）。单独暴露 cfg 参数是为了让单元测试能用小世界秒级跑完。
fn build_scenario_with_cfg(name: &'static str, cfg: SimConfig) -> Scenario {
    let mut w = World::new(cfg);
    w.run();
    let n_world = w.accounts().len();
    let raw = w.ratings();
    let accs = w.accounts();

    let alive: Vec<usize> = accs
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    let honest: Vec<usize> = accs
        .iter()
        .filter(|a| a.kind == Kind::Honest && a.is_alive())
        .map(|a| a.idx)
        .collect();
    let sybil: Vec<usize> = accs
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sybil { .. }))
        .map(|a| a.idx)
        .collect();
    let attackers: Vec<usize> = accs
        .iter()
        .filter(|a| a.kind.is_attacker())
        .map(|a| a.idx)
        .collect();
    let benef = w.attack.target.filter(|&t| {
        accs.get(t)
            .map(|a| a.kind == Kind::Promoted)
            .unwrap_or(false)
    });

    // 追加账户：编号直接接在「世界账户总数」后面（不能用 alive_count，
    // 否则会与已离开账户撞号 —— 交接文档 8.1 节）。
    let mut n = n_world;
    let empty: Vec<usize> = (n..n + EMPTY_COUNT).collect();
    n += EMPTY_COUNT;
    let fake: Vec<usize> = (n..n + FAKE_COUNT).collect();
    n += FAKE_COUNT;
    let injected: Vec<usize> = (n..n + INJECT_COUNT).collect();
    n += INJECT_COUNT;

    // S2 的注入者 = 世界结束后「评分对象最多」的攻击者（同分取小编号，保证可复现）。
    let mut by_len: Vec<(usize, usize)> = attackers
        .iter()
        .map(|&a| (a, accs[a].ratings.len()))
        .collect();
    by_len.sort_by_key(|&(a, k)| (std::cmp::Reverse(k), a));
    let injector = by_len.first().map(|&(a, _)| a);

    // 注入评分：注入者给 40 个新账户各 1.0 分。
    let mut ratings = raw.clone();
    if let Some(inf) = injector {
        for &s in &injected {
            ratings.push((inf, s, 1.0));
        }
    }

    Scenario {
        name,
        n,
        n_world,
        raw,
        ratings,
        alive,
        honest,
        sybil,
        benef,
        attackers,
        injected,
        empty,
        fake,
        injector,
    }
}

// ============================== 传播 ==============================

/// 建全套引擎并跑完 4 轮交换，返回传播结束时的引擎表。
///
/// 口径（与 A6 一致，也是交接文档 8.1 节的硬要求）：
/// * 每个活着的账户都建 Engine，包括攻击者与收益方；
/// * 每个引擎先 intern 全部 0..n 个密钥（追加账户也被 intern，这样 S2/S4 的账户
///   才可能出现在分布里），再 rate 自己的评分；
/// * 每轮先把所有观点 export_view(now) 拍成快照（同一轮里大家看到同一批字节），
///   再逐引擎 tick(now, 12) 并对抽到的来源 on_view。
///   抽样失败不模拟：每个请求都「成功取到」观点。失败只会让连败计数不清零、
///   退避变强，而本实验的网络始终全在线。
fn simulate(sc: &Scenario, seed: u64) -> Vec<Option<Engine>> {
    let cfg = engine_config();
    let mut by: Vec<Vec<(usize, f32)>> = vec![Vec::new(); sc.n];
    for &(o, t, s) in &sc.ratings {
        if o < sc.n && t < sc.n {
            by[o].push((t, s));
        }
    }

    let mut engines: Vec<Option<Engine>> = (0..sc.n).map(|_| None).collect();
    for &a in &sc.alive {
        let eseed = ENGINE_SEED_SALT ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (a as u64);
        let mut e = Engine::with_seed(cfg.clone(), eseed);
        for i in 0..sc.n {
            e.intern(key(i));
        }
        for &(t, s) in &by[a] {
            e.rate(t as u32, s, 0);
        }
        engines[a] = Some(e);
    }

    for r in 0..ROUNDS {
        let now = STEPS as u64 + r * HPR;
        let mut views: Vec<Option<CompressedView>> = (0..sc.n).map(|_| None).collect();
        for &a in &sc.alive {
            if let Some(e) = engines[a].as_ref() {
                views[a] = Some(e.export_view(now));
            }
        }
        for &a in &sc.alive {
            let reqs = match engines[a].as_mut() {
                Some(e) => e.tick(now, SAMPLES),
                None => continue,
            };
            for req in reqs {
                let idx = req.idx as usize;
                if let Some(Some(v)) = views.get(idx) {
                    if let Some(e) = engines[a].as_mut() {
                        e.on_view(req.idx, v.clone(), now, now);
                    }
                }
            }
        }
    }
    engines
}

/// 全部（观察者 × 账户）声望矩阵的扁平化：observers × n 个 query(i, now)。
///
/// 顺序固定（先观察者编号升序、再账户编号升序），两次同输入重跑必须逐位相同。
/// 这是 S6 的度量对象：它比只看分布更严格（分布经过归一化，可能掩盖小差异）。
fn digest_of(sc: &Scenario, engines: &[Option<Engine>], now: u64) -> Vec<f32> {
    let mut out = Vec::with_capacity(sc.observers().len() * sc.n);
    for &obs in &sc.observers() {
        if let Some(e) = engines[obs].as_ref() {
            for i in 0..sc.n as u32 {
                out.push(e.query(i, now));
            }
        }
    }
    out
}

/// 比较两次重跑：返回 (最大绝对差, 平均绝对差, 数值个数)。
fn digest_diff(a: &[f32], b: &[f32]) -> (f64, f64, usize) {
    if a.len() != b.len() {
        return (f64::INFINITY, f64::INFINITY, a.len().max(b.len()));
    }
    let mut maxd = 0.0f64;
    let mut sumd = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = (*x - *y).abs() as f64;
        if d > maxd {
            maxd = d;
        }
        sumd += d;
    }
    let mean = if a.is_empty() {
        0.0
    } else {
        sumd / a.len() as f64
    };
    (maxd, mean, a.len())
}

// ============================== 逐观察者读数 ==============================

/// 一个观察者眼中的全部分布读数。
///
/// 字段分三组：
/// * hm 一族：hm（成文定义，分母 = 活着的诚实账户总数）与 hm_present（A6 实现，
///   分母 = 分布里出现的诚实账户数）；
/// * 六条判据：正式列按 hm 算，_hp 后缀列按 hm_present 算（把「口径修正」与
///   「P0 代码改动」两种效应分开）；
/// * S3 分解：s3_personal / s3_group 把 d_T 拆成 β·个人项 与 (1−β)·群体项。
#[derive(Clone, Copy, Default, Debug)]
struct ObsRow {
    // —— hm 一族 ——
    hm: f64,
    hm_present: f64,
    present_frac: f64,

    // —— S1 ——
    s1: f64,
    s1_hp: f64,
    s1_present: f64,

    // —— S2 ——
    s2_ratio: f64,
    s2_ratio_hp: f64,
    s2_cross: f64,
    s2_cross_hp: f64,
    s2_present: f64,
    s2_cross_n: f64,
    s2_pairs_n: f64,

    // —— S3 ——
    s3_benef: f64,
    s3_benef_hp: f64,
    s3_att: f64,
    s3_att_hp: f64,
    s3_personal: f64,
    s3_group: f64,
    s3_benef_rated: f64,
    s3_obs_rated: f64,

    // —— S4 ——
    s4: f64,
    s4_hp: f64,
    s4_max: f64,
    s4_max_hp: f64,
    s4_present: f64,
}

/// 一份分布里某个集合的 (分布值之和, 最大值, 出现个数)。
///
/// 不在这里除：「某一类的平均声望」的分母是该类账户总数（缺失按 0），
/// 与「出现个数」是两件事。
fn sum_of_dist(dist: &BTreeMap<u32, f32>, set: &[usize]) -> (f64, f64, usize) {
    let mut sum = 0.0f64;
    let mut max = 0.0f64;
    let mut present = 0usize;
    for &x in set {
        if let Some(v) = dist.get(&(x as u32)) {
            let v = *v as f64;
            sum += v;
            present += 1;
            if v > max {
                max = v;
            }
        }
    }
    (sum, max, present)
}

/// 从一份归一化分布算出这一观察者的全部判据读数（纯函数，便于单测）。
///
/// 口径（照抄审计/A6-ablation.md 第 52-64 行）：
/// * 某类的「平均声望」= 该类所有账户的分布值之和 ÷ 该类账户总数
///   （没出现在分布里的按 0 计入）。这样「注入 40 个女巫只有 1 个露头」
///   不会被写成「女巫平均分 = 那个露头的值」；
/// * S1/S2 的「出现率」分母同样是该类账户总数；
/// * S2 的越线数是（观察者, 女巫）对数，由调用方在观察者之间累加；
/// * hm 的两个版本见 ObsRow 的说明；S3 的分解需要引擎的 share_of，
///   由 collect_rows 补。
fn row_from_dist(
    sc: &Scenario,
    dist: &BTreeMap<u32, f32>,
    hm: f64,
    hm_present: f64,
    honest_present: usize,
) -> ObsRow {
    let set_mean = |set: &[usize]| -> (f64, f64, usize) {
        let (sum, max, present) = sum_of_dist(dist, set);
        if set.is_empty() || !hm.is_finite() || hm <= 0.0 {
            (f64::NAN, f64::NAN, present)
        } else {
            (sum / set.len() as f64 / hm, max / hm, present)
        }
    };
    let set_mean_hp = |set: &[usize]| -> (f64, f64) {
        let (sum, max, _) = sum_of_dist(dist, set);
        if set.is_empty() || !hm_present.is_finite() || hm_present <= 0.0 {
            (f64::NAN, f64::NAN)
        } else {
            (sum / set.len() as f64 / hm_present, max / hm_present)
        }
    };

    // S1：女巫集群成员均值 / hm，另外给出现率。
    let (s1, _, s1_present_n) = set_mean(&sc.sybil);
    let (s1_hp, _) = set_mean_hp(&sc.sybil);

    // S2：注入女巫均值 / hm，以及「超过 hm 的（观察者,女巫）对数」。
    let (s2_ratio, _, s2_present_n) = set_mean(&sc.injected);
    let (s2_ratio_hp, _) = set_mean_hp(&sc.injected);
    let mut cross = 0usize;
    let mut cross_hp = 0usize;
    for &x in &sc.injected {
        if let Some(v) = dist.get(&(x as u32)) {
            let v = *v as f64;
            if v > hm {
                cross += 1;
            }
            if v > hm_present {
                cross_hp += 1;
            }
        }
    }

    // S3：收益方 / hm；攻击者自己 / hm。
    let benef = sc
        .benef
        .and_then(|b| dist.get(&(b as u32)).copied())
        .unwrap_or(0.0) as f64;
    let (s3_benef, s3_benef_hp) = if sc.benef.is_some() {
        if hm.is_finite() && hm > 0.0 && hm_present.is_finite() && hm_present > 0.0 {
            (benef / hm, benef / hm_present)
        } else {
            (f64::NAN, f64::NAN)
        }
    } else {
        (f64::NAN, f64::NAN)
    };
    let (s3_att, _, _) = set_mean(&sc.attackers);
    let (s3_att_hp, _) = set_mean_hp(&sc.attackers);

    // S4：40 个已 intern 但没人评分的空账户。
    let (s4, s4_max, s4_present_n) = set_mean(&sc.empty);
    let (s4_hp, s4_max_hp) = set_mean_hp(&sc.empty);

    ObsRow {
        hm,
        hm_present,
        present_frac: if sc.honest.is_empty() {
            f64::NAN
        } else {
            honest_present as f64 / sc.honest.len() as f64
        },
        s1,
        s1_hp,
        s1_present: if sc.sybil.is_empty() {
            f64::NAN
        } else {
            s1_present_n as f64 / sc.sybil.len() as f64
        },
        s2_ratio,
        s2_ratio_hp,
        s2_cross: if sc.injected.is_empty() {
            f64::NAN
        } else {
            cross as f64 / sc.injected.len() as f64
        },
        s2_cross_hp: if sc.injected.is_empty() {
            f64::NAN
        } else {
            cross_hp as f64 / sc.injected.len() as f64
        },
        s2_present: if sc.injected.is_empty() {
            f64::NAN
        } else {
            s2_present_n as f64 / sc.injected.len() as f64
        },
        s2_cross_n: cross as f64,
        s2_pairs_n: sc.injected.len() as f64,
        s3_benef,
        s3_benef_hp,
        s3_att,
        s3_att_hp,
        s3_personal: f64::NAN,
        s3_group: f64::NAN,
        s3_benef_rated: f64::NAN,
        s3_obs_rated: 0.0,
        s4,
        s4_hp,
        s4_max,
        s4_max_hp,
        s4_present: if sc.empty.is_empty() {
            f64::NAN
        } else {
            s4_present_n as f64 / sc.empty.len() as f64
        },
    }
}

/// 收集每个观察者的判据读数，并补上 S3 分解。
///
/// 返回 (逐观察者行, 被跳过的观察者数, 分布总和最大偏差)。跳过条件与 A6 一致：
/// 分布为空（这个引擎还没给任何人打正分）或诚实账户读数为 0 —— 两者都无法
/// 定义 hm，强行计算会得到 0 分母。
///
/// 分布总和最大偏差是附加诊断：|Σdist − 1| 在全部观察者上的最大值。
/// 设计规格第 8 节要求分布总和恒为 1，这一列当场验证。
fn collect_rows(sc: &Scenario, engines: &[Option<Engine>], now: u64) -> (Vec<ObsRow>, usize, f64) {
    let hs = sc.honest_set();
    let mut rows = Vec::with_capacity(sc.honest.len());
    let mut skipped = 0usize;
    let mut dist_sum_err = 0.0f64;

    for &obs in &sc.alive {
        if !hs.contains(&obs) {
            continue;
        }
        let e = match engines[obs].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let dist_vec = e.trust_distribution(now);
        let total: f64 = dist_vec.iter().map(|(_, v)| *v as f64).sum();
        if total > 0.0 {
            let d = (total - 1.0).abs();
            if d > dist_sum_err {
                dist_sum_err = d;
            }
        }
        let dist_map: BTreeMap<u32, f32> = dist_vec.into_iter().collect();
        if dist_map.is_empty() {
            skipped += 1;
            continue;
        }

        // 成文定义：hm = Σ(分布里的诚实账户) ÷ 活着的诚实账户总数（缺失按 0）。
        let (hsum, _, hp) = sum_of_dist(&dist_map, &sc.honest);
        if hp == 0 || hsum <= 0.0 {
            skipped += 1;
            continue;
        }
        let hm = hsum / sc.honest.len() as f64;
        let hm_present = hsum / hp as f64;
        let mut row = row_from_dist(sc, &dist_map, hm, hm_present, hp);

        // —— S3 分解：d_T = β·个人项 + (1−β)·群体项 ——
        //
        // 个人项 = share_of(T)（份额；没给 T 打正分时为 0）；群体项由 d_T 反解。
        // 反解用的是已经过 combine 夹到 [0,1] 的 d_T，所以两项相加与 s3_benef
        // 可能有 ≤ 1e-7 的误差（报告里按「组成项」读，不按「精确分解」读）。
        if let Some(t) = sc.benef {
            let beta = BETA as f64;
            let d_t = e.query(t as u32, now) as f64;
            let p_t = e.share_of(t as u32).map(|s| s as f64).unwrap_or(0.0);
            let personal = beta * p_t;
            let group = d_t - personal;
            row.s3_personal = personal / hm;
            row.s3_group = group / hm;
            if p_t > 0.0 {
                row.s3_benef_rated = d_t / hm;
                row.s3_obs_rated = 1.0;
            }
        }
        rows.push(row);
    }
    (rows, skipped, dist_sum_err)
}

/// 所有观察者眼中 target 的平均声望（缺失按 0）。
fn mean_rep_of(sc: &Scenario, engines: &[Option<Engine>], now: u64, target: usize) -> f64 {
    let mut sum = 0.0f64;
    let mut cnt = 0usize;
    for &obs in &sc.observers() {
        if let Some(e) = engines[obs].as_ref() {
            for (i, v) in e.trust_distribution(now) {
                if i as usize == target {
                    sum += v as f64;
                    break;
                }
            }
            cnt += 1;
        }
    }
    if cnt == 0 {
        0.0
    } else {
        sum / cnt as f64
    }
}

/// S5：把一个已被诚实观察者评分的来源的观点换成伪造视图，看它自己的声望变化。
///
/// 测法（与 A6 一致）：
/// 1. 选「被诚实观察者评分最多」的攻击者 / 收益方作伪造者；
/// 2. 先把它真实的导出观点推给所有诚实观察者（时间戳取 now，排除衰减混淆），
///    得到 R0；
/// 3. 换成「自吹」视图（只写一条：自己 = 1.0），得到 R1，s5_self = R1 / R0；
/// 4. 再换成「自吹 + 塞 200 个观察者已认识的空壳账户各 1.0」（试图把 total_mass
///    撑大），得到 R2，s5_mass = R2 / R0。
///
/// 返回 (s5_self, s5_mass)；伪造者不存在时返回 (NaN, NaN)。
///
/// 注意：本函数会改写 engines（往每个观察者塞入伪造观点）。调用方必须在采集完
/// 读数与矩阵之后才调用它，否则会污染 S1~S6。
fn measure_s5(sc: &Scenario, engines: &mut [Option<Engine>], now: u64) -> (f64, f64) {
    let hs = sc.honest_set();
    let mut cand: Vec<(usize, usize)> = Vec::new();
    for &a in &sc.attackers {
        let deg = sc
            .raw
            .iter()
            .filter(|(o, t, _)| *t == a && hs.contains(o))
            .count();
        cand.push((a, deg));
    }
    if let Some(b) = sc.benef {
        let deg = sc
            .raw
            .iter()
            .filter(|(o, t, _)| *t == b && hs.contains(o))
            .count();
        cand.push((b, deg));
    }
    cand.sort_by_key(|&(a, d)| (std::cmp::Reverse(d), a));
    let forger = match cand.first() {
        Some(&(a, d)) if d > 0 => a,
        _ => return (f64::NAN, f64::NAN),
    };

    let obs_list = sc.observers();
    let honest_view = match engines[forger].as_ref() {
        Some(e) => e.export_view(now),
        None => return (f64::NAN, f64::NAN),
    };
    for &obs in &obs_list {
        if let Some(e) = engines[obs].as_mut() {
            e.on_view(forger as u32, honest_view.clone(), now, now);
        }
    }
    let r0 = mean_rep_of(sc, engines, now, forger);

    let forged_self = CompressedView::from_pairs(view_params(), &[(key(forger), 1.0)]);
    for &obs in &obs_list {
        if let Some(e) = engines[obs].as_mut() {
            e.on_view(forger as u32, forged_self.clone(), now, now);
        }
    }
    let r1 = mean_rep_of(sc, engines, now, forger);

    let mut pairs: Vec<([u8; 32], f32)> = vec![(key(forger), 1.0)];
    for &f in &sc.fake {
        pairs.push((key(f), 1.0));
    }
    let forged_mass = CompressedView::from_pairs(view_params(), &pairs);
    for &obs in &obs_list {
        if let Some(e) = engines[obs].as_mut() {
            e.on_view(forger as u32, forged_mass.clone(), now, now);
        }
    }
    let r2 = mean_rep_of(sc, engines, now, forger);

    (
        if r0 > 0.0 { r1 / r0 } else { f64::NAN },
        if r0 > 0.0 { r2 / r0 } else { f64::NAN },
    )
}

// ============================== 汇总行 ==============================

/// 一个（场景, seed）的汇总读数。字段 = 场景规模 + 观察者统计 + 六条判据 + S6。
///
/// 判据读数本身是「逐观察者的比值」，汇总时对观察者取平均（与 A6 一致）。
#[derive(Clone, Copy, Debug, Default)]
struct Row {
    // —— 场景规模（同一场景内各 seed 不同，报告要逐 seed 给出）——
    n_accounts: f64,
    n_world: f64,
    n_alive: f64,
    n_honest: f64,
    n_attackers: f64,
    n_benef: f64,
    n_injected: f64,
    n_empty: f64,
    n_fake: f64,

    // —— 观察者与分布诊断 ——
    obs_used: f64,
    obs_skipped: f64,
    dist_sum_err: f64,

    // —— hm 一族 ——
    hm: f64,
    hm_present: f64,
    present_frac: f64,

    // —— 引擎 / 世界诊断 ——
    avg_views: f64,
    avg_degree: f64,
    avg_rating_sum: f64,

    // —— S1 ——
    s1_raw_self: f64,
    s1: f64,
    s1_hp: f64,
    s1_present: f64,

    // —— S2 ——
    s2_ratio: f64,
    s2_ratio_hp: f64,
    s2_cross: f64,
    s2_cross_hp: f64,
    s2_present: f64,
    s2_cross_n: f64,
    s2_pairs_n: f64,

    // —— S3 ——
    s3_benef: f64,
    s3_benef_hp: f64,
    s3_benef_max: f64,
    s3_att: f64,
    s3_att_hp: f64,
    s3_personal: f64,
    s3_group: f64,
    s3_benef_rated: f64,
    s3_obs_rated: f64,
    s3_floor: f64,
    s3_vs_floor: f64,
    s3_vs_floor_max: f64,

    // —— S4 ——
    s4: f64,
    s4_hp: f64,
    s4_max: f64,
    s4_max_hp: f64,
    s4_present: f64,
    s4_worst_obs_present: f64,

    // —— S5 ——
    s5_self: f64,
    s5_mass: f64,

    // —— S6 ——
    s6_max: f64,
    s6_mean: f64,
    s6_len: f64,
}

/// 三个场景共用的指标（顺序 = 报告表格的列序）。
const COMMON_METRICS: [&str; 21] = [
    "n_accounts",
    "n_world",
    "n_alive",
    "n_honest",
    "n_attackers",
    "n_benef",
    "n_injected",
    "n_empty",
    "n_fake",
    "obs_used",
    "obs_skipped",
    "dist_sum_err",
    "hm",
    "hm_present",
    "present_frac",
    "avg_views",
    "avg_degree",
    "avg_rating_sum",
    "s6_max",
    "s6_mean",
    "s6_len",
];

/// 女巫场景独有的指标（S1）。
const SYBIL_METRICS: [&str; 4] = ["s1_raw_self", "s1", "s1_hp", "s1_present"];

/// 渗透场景独有的指标（S2 / S3 / S4 / S5）。
const INFILTRATOR_METRICS: [&str; 27] = [
    "s2_ratio",
    "s2_ratio_hp",
    "s2_cross",
    "s2_cross_hp",
    "s2_present",
    "s2_cross_n",
    "s2_pairs_n",
    "s3_benef",
    "s3_benef_hp",
    "s3_benef_max",
    "s3_att",
    "s3_att_hp",
    "s3_personal",
    "s3_group",
    "s3_benef_rated",
    "s3_obs_rated",
    "s3_floor",
    "s3_vs_floor",
    "s3_vs_floor_max",
    "s4",
    "s4_hp",
    "s4_max",
    "s4_max_hp",
    "s4_present",
    "s4_worst_obs_present",
    "s5_self",
    "s5_mass",
];

/// 背书场景独有的指标（S3）。
const ENDORSER_METRICS: [&str; 12] = [
    "s3_benef",
    "s3_benef_hp",
    "s3_benef_max",
    "s3_att",
    "s3_att_hp",
    "s3_personal",
    "s3_group",
    "s3_benef_rated",
    "s3_obs_rated",
    "s3_floor",
    "s3_vs_floor",
    "s3_vs_floor_max",
];

impl Row {
    /// 按名字取指标；未知名字返回 NaN（汇总会把它过滤掉）。
    fn metric(&self, name: &str) -> f64 {
        match name {
            "n_accounts" => self.n_accounts,
            "n_world" => self.n_world,
            "n_alive" => self.n_alive,
            "n_honest" => self.n_honest,
            "n_attackers" => self.n_attackers,
            "n_benef" => self.n_benef,
            "n_injected" => self.n_injected,
            "n_empty" => self.n_empty,
            "n_fake" => self.n_fake,
            "obs_used" => self.obs_used,
            "obs_skipped" => self.obs_skipped,
            "dist_sum_err" => self.dist_sum_err,
            "hm" => self.hm,
            "hm_present" => self.hm_present,
            "present_frac" => self.present_frac,
            "avg_views" => self.avg_views,
            "avg_degree" => self.avg_degree,
            "avg_rating_sum" => self.avg_rating_sum,
            "s1_raw_self" => self.s1_raw_self,
            "s1" => self.s1,
            "s1_hp" => self.s1_hp,
            "s1_present" => self.s1_present,
            "s2_ratio" => self.s2_ratio,
            "s2_ratio_hp" => self.s2_ratio_hp,
            "s2_cross" => self.s2_cross,
            "s2_cross_hp" => self.s2_cross_hp,
            "s2_present" => self.s2_present,
            "s2_cross_n" => self.s2_cross_n,
            "s2_pairs_n" => self.s2_pairs_n,
            "s3_benef" => self.s3_benef,
            "s3_benef_hp" => self.s3_benef_hp,
            "s3_benef_max" => self.s3_benef_max,
            "s3_att" => self.s3_att,
            "s3_att_hp" => self.s3_att_hp,
            "s3_personal" => self.s3_personal,
            "s3_group" => self.s3_group,
            "s3_benef_rated" => self.s3_benef_rated,
            "s3_obs_rated" => self.s3_obs_rated,
            "s3_floor" => self.s3_floor,
            "s3_vs_floor" => self.s3_vs_floor,
            "s3_vs_floor_max" => self.s3_vs_floor_max,
            "s4" => self.s4,
            "s4_hp" => self.s4_hp,
            "s4_max" => self.s4_max,
            "s4_max_hp" => self.s4_max_hp,
            "s4_present" => self.s4_present,
            "s4_worst_obs_present" => self.s4_worst_obs_present,
            "s5_self" => self.s5_self,
            "s5_mass" => self.s5_mass,
            "s6_max" => self.s6_max,
            "s6_mean" => self.s6_mean,
            "s6_len" => self.s6_len,
            _ => f64::NAN,
        }
    }
}

/// 场景 → 指标列清单（接口集中在这里，免得主流程里散落 match）。
fn metric_names(scenario: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = COMMON_METRICS.to_vec();
    match scenario {
        "sybil" => out.extend(SYBIL_METRICS),
        "infiltrator" => out.extend(INFILTRATOR_METRICS),
        "endorser" => out.extend(ENDORSER_METRICS),
        _ => {}
    }
    out
}

/// 按选择器把一个观察者行映射成数值。
type ObsField = fn(&ObsRow) -> f64;

/// 取一列（保留 NA，交给 avg/min/max 过滤）。
fn col_of(rows: &[ObsRow], f: ObsField) -> Vec<f64> {
    rows.iter().map(f).collect()
}

/// 一列的平均（跳过 NA）。
fn mean_of(rows: &[ObsRow], f: ObsField) -> f64 {
    avg(&col_of(rows, f))
}

/// 一列的和（跳过 NA）。
fn sum_of(rows: &[ObsRow], f: ObsField) -> f64 {
    rows.iter().map(f).filter(|x| x.is_finite()).sum()
}

/// 一列的最大值（跳过 NA）。
fn max_col(rows: &[ObsRow], f: ObsField) -> f64 {
    max_of(&col_of(rows, f))
}

/// 把逐观察者行 + 引擎诊断汇总成一个 Row。
///
/// 报告里同时给两个口径：mean（逐观察者比值再对观察者取平均，正式口径）与
/// max（最坏的那个观察者）。S3 的 s3_benef_max 就是后者：它回答「最离谱的
/// 那个观察者把收益方抬到了几倍」，与「平均抬到几倍」是两件事。
fn aggregate(
    sc: &Scenario,
    engines: &[Option<Engine>],
    rows: &[ObsRow],
    skipped: usize,
    dist_sum_err: f64,
) -> Row {
    // 引擎诊断：只统计观察者（诚实、活着、有引擎）。
    let mut views = 0.0f64;
    let mut degree = 0.0f64;
    let mut cnt = 0usize;
    for &obs in &sc.alive {
        if let Some(e) = engines[obs].as_ref() {
            let (_, _, vn, rn) = e.stats();
            views += vn as f64;
            degree += rn as f64;
            cnt += 1;
        }
    }
    let (avg_views, avg_degree) = if cnt > 0 {
        (views / cnt as f64, degree / cnt as f64)
    } else {
        (f64::NAN, f64::NAN)
    };

    // 世界侧诊断：观察者给出的原始评分总和（与 A6 报的「平均给 18.7 人评分、
    // Σ评分 = 9.93」对照）。
    let hs = sc.honest_set();
    let mut rsum = 0.0f64;
    for &(o, _, v) in &sc.raw {
        if hs.contains(&o) {
            rsum += v as f64;
        }
    }
    let avg_rating_sum = if sc.honest.is_empty() {
        f64::NAN
    } else {
        rsum / sc.honest.len() as f64
    };

    let s3_benef = mean_of(rows, |r| r.s3_benef);
    let s3_benef_max = max_col(rows, |r| r.s3_benef);
    // 只对「自己给收益方打过正分」的观察者求均值：其余观察者的 d_T 里
    // 个人项为 0，混进来会把均值往下拉。
    let s3_benef_rated = {
        let vals: Vec<f64> = rows
            .iter()
            .filter(|r| r.s3_obs_rated > 0.0)
            .map(|r| r.s3_benef_rated)
            .collect();
        avg(&vals)
    };

    Row {
        n_accounts: sc.n as f64,
        n_world: sc.n_world as f64,
        n_alive: sc.alive.len() as f64,
        n_honest: sc.honest.len() as f64,
        n_attackers: sc.attackers.len() as f64,
        n_benef: if sc.benef.is_some() { 1.0 } else { 0.0 },
        n_injected: sc.injected.len() as f64,
        n_empty: sc.empty.len() as f64,
        n_fake: sc.fake.len() as f64,
        obs_used: rows.len() as f64,
        obs_skipped: skipped as f64,
        dist_sum_err,
        hm: mean_of(rows, |r| r.hm),
        hm_present: mean_of(rows, |r| r.hm_present),
        present_frac: mean_of(rows, |r| r.present_frac),
        avg_views,
        avg_degree,
        avg_rating_sum,
        s1_raw_self: sc.sybil_raw_self(),
        s1: mean_of(rows, |r| r.s1),
        s1_hp: mean_of(rows, |r| r.s1_hp),
        s1_present: mean_of(rows, |r| r.s1_present),
        s2_ratio: mean_of(rows, |r| r.s2_ratio),
        s2_ratio_hp: mean_of(rows, |r| r.s2_ratio_hp),
        s2_cross: mean_of(rows, |r| r.s2_cross),
        s2_cross_hp: mean_of(rows, |r| r.s2_cross_hp),
        s2_present: mean_of(rows, |r| r.s2_present),
        s2_cross_n: sum_of(rows, |r| r.s2_cross_n),
        // 全部对数 = Σ_观察者 (注入女巫数)：每个观察者一视同仁，分布为空而被跳过的
        // 观察者自然不计入。
        s2_pairs_n: sum_of(rows, |r| r.s2_pairs_n),
        s3_benef,
        s3_benef_hp: mean_of(rows, |r| r.s3_benef_hp),
        s3_benef_max,
        s3_att: mean_of(rows, |r| r.s3_att),
        s3_att_hp: mean_of(rows, |r| r.s3_att_hp),
        s3_personal: mean_of(rows, |r| r.s3_personal),
        s3_group: mean_of(rows, |r| r.s3_group),
        s3_benef_rated,
        s3_obs_rated: sum_of(rows, |r| r.s3_obs_rated),
        s3_floor: if sc.benef.is_some() {
            ANALYTIC_FLOOR_S3
        } else {
            f64::NAN
        },
        s3_vs_floor: if s3_benef.is_finite() {
            s3_benef / ANALYTIC_FLOOR_S3
        } else {
            f64::NAN
        },
        s3_vs_floor_max: if s3_benef_max.is_finite() {
            s3_benef_max / ANALYTIC_FLOOR_S3
        } else {
            f64::NAN
        },
        s4: mean_of(rows, |r| r.s4),
        s4_hp: mean_of(rows, |r| r.s4_hp),
        s4_max: mean_of(rows, |r| r.s4_max),
        s4_max_hp: mean_of(rows, |r| r.s4_max_hp),
        s4_present: mean_of(rows, |r| r.s4_present),
        s4_worst_obs_present: max_col(rows, |r| r.s4_present),
        s5_self: f64::NAN,
        s5_mass: f64::NAN,
        s6_max: f64::NAN,
        s6_mean: f64::NAN,
        s6_len: f64::NAN,
    }
}

// ============================== 一次运行 ==============================

/// 跑一个（场景, seed）：第一次建引擎 → 取矩阵 → 收集读数 → 测 S5；
/// 再建一次 → 取矩阵 → 比 S6。
///
/// 两次运行的世界不重建：世界本身也是确定性的（同 seed 必然逐位相同），
/// 重建一次没有意义；引擎的随机流种子只依赖 (seed, 账户编号)，
/// 所以两次必须逐位相同。S6 因此是在完全相同输入下重跑得到的。
fn run_one(sc: &Scenario, seed: u64) -> Row {
    let now = sc.final_now();

    let mut engines_a = simulate(sc, seed);
    let digest_a = digest_of(sc, &engines_a, now);

    let (obs_rows, skipped, dist_sum_err) = collect_rows(sc, &engines_a, now);
    let mut row = aggregate(sc, &engines_a, &obs_rows, skipped, dist_sum_err);
    // S5 会改写引擎里的观点缓存，必须放在读数与矩阵都取完之后。
    let (s5_self, s5_mass) = measure_s5(sc, &mut engines_a, now);
    row.s5_self = s5_self;
    row.s5_mass = s5_mass;

    let engines_b = simulate(sc, seed);
    let digest_b = digest_of(sc, &engines_b, now);
    let (mx, mean, len) = digest_diff(&digest_a, &digest_b);
    row.s6_max = mx;
    row.s6_mean = mean;
    row.s6_len = len as f64;
    row
}

// ============================== 主程序 ==============================

/// 打印参数块：报告的「参数」一节直接抄这块，避免手抄失真。
fn print_params() {
    println!("PARAM n_initial {}", N_INITIAL);
    println!("PARAM steps {}", STEPS);
    println!("PARAM join_rate 0.15");
    println!("PARAM leave_rate 0.00012");
    println!("PARAM follow_min 20");
    println!("PARAM follow_max 30");
    println!("PARAM max_follows 40");
    println!("PARAM rounds {}", ROUNDS);
    println!("PARAM hours_per_round {}", HPR);
    println!("PARAM samples_per_tick {}", SAMPLES);
    println!("PARAM final_now {}", STEPS as u64 + (ROUNDS - 1) * HPR);
    println!("PARAM beta {}", BETA);
    println!("PARAM half_life {}", HALF_LIFE);
    println!("PARAM agg_mode Mean");
    println!("PARAM view_buckets {}", view_params().buckets);
    println!("PARAM view_hashers {}", view_params().hashers);
    println!("PARAM view_seed {}", view_params().seed);
    println!("PARAM attack_ratio 0.08");
    println!("PARAM attack_kinds sybil:cluster=3 infiltrator endorser:ratio=0.2");
    println!("PARAM empty_count {}", EMPTY_COUNT);
    println!("PARAM inject_count {}", INJECT_COUNT);
    println!("PARAM fake_count {}", FAKE_COUNT);
    println!("PARAM analytic_floor_s3 {}", fmt8(ANALYTIC_FLOOR_S3));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n_seeds: usize = args
        .iter()
        .position(|s| s == "--seeds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let first_seed: u64 = args
        .iter()
        .position(|s| s == "--first")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(11);
    let scenarios: Vec<&'static str> = args
        .iter()
        .position(|s| s == "--scenarios")
        .and_then(|i| args.get(i + 1))
        .map(|s| {
            s.split(',')
                .map(|x| x.trim())
                .filter_map(|x| match x {
                    "sybil" => Some("sybil"),
                    "infiltrator" => Some("infiltrator"),
                    "endorser" => Some("endorser"),
                    _ => None,
                })
                .collect::<Vec<&'static str>>()
        })
        .unwrap_or_else(|| vec!["sybil", "infiltrator", "endorser"]);
    if scenarios.is_empty() {
        eprintln!("没有合法场景；可用：sybil,infiltrator,endorser");
        return;
    }

    let seeds: Vec<u64> = (0..n_seeds as u64).map(|i| first_seed + i).collect();
    let t0 = std::time::Instant::now();

    println!(
        "# B0 baseline（P0 修复后）scenarios={:?} seeds={:?}",
        scenarios, seeds
    );
    print_params();
    println!("# 行格式：BASE <场景> <seed> <规模>；SEED <场景> <seed> <指标> <值>；SUM <场景> <指标> <mean> <sd> <min> <max> <n>");
    println!("FLOOR\t{}\t{}", "s3", fmt8(ANALYTIC_FLOOR_S3));

    for &scen in &scenarios {
        let names = metric_names(scen);
        let mut rows: Vec<Row> = Vec::with_capacity(seeds.len());
        for &seed in &seeds {
            let sc = build_scenario(scen, seed);
            println!("BASE\t{}\t{}\t{}", sc.name, seed, sc.size_line());
            let row = run_one(&sc, seed);
            for name in &names {
                println!(
                    "SEED\t{}\t{}\t{}\t{}",
                    scen,
                    seed,
                    name,
                    fmt_metric(name, row.metric(name))
                );
            }
            rows.push(row);
            eprintln!(
                "[{:>7.1}s] {} seed={} 完成（obs={}，hm={}，S6max={}）",
                t0.elapsed().as_secs_f64(),
                scen,
                seed,
                fmt_metric("obs_used", row.obs_used),
                fmt8(row.hm),
                fmt8(row.s6_max)
            );
        }
        for name in &names {
            let vals: Vec<f64> = rows.iter().map(|r| r.metric(name)).collect();
            println!(
                "SUM\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                scen,
                name,
                fmt_metric(name, avg(&vals)),
                fmt8(sd(&vals)),
                fmt_metric(name, min_of(&vals)),
                fmt_metric(name, max_of(&vals)),
                finite_len(&vals)
            );
        }
    }

    println!("# DONE elapsed {:.1}s", t0.elapsed().as_secs_f64());
}

// ============================== 单元测试 ==============================

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小的假场景（不跑世界），只用来单测判据口径。
    fn tiny() -> Scenario {
        Scenario {
            name: "infiltrator",
            n: 8,
            n_world: 5,
            raw: vec![(0, 4, 1.0), (0, 1, 1.0)],
            ratings: vec![(0, 4, 1.0), (0, 1, 1.0)],
            alive: vec![0, 1, 2, 3],
            honest: vec![0, 1, 2, 3],
            sybil: vec![],
            benef: Some(4),
            attackers: vec![5],
            injected: vec![4, 5],
            empty: vec![6],
            fake: vec![7],
            injector: Some(5),
        }
    }

    /// 一份手写分布：诚实 4 个里出现 3 个、收益方、注入女巫、空账户。
    fn dist() -> BTreeMap<u32, f32> {
        let mut d = BTreeMap::new();
        d.insert(0u32, 0.20f32);
        d.insert(1u32, 0.10f32);
        d.insert(2u32, 0.05f32);
        d.insert(4u32, 0.30f32); // 收益方
        d.insert(5u32, 0.02f32); // 注入女巫里露头的那一个
        d.insert(6u32, 0.01f32); // 空账户
        d
    }

    /// hm 的分母必须是「活着的诚实账户总数」，缺失按 0。
    #[test]
    fn hm_denominator_counts_all_honest_accounts() {
        let sc = tiny();
        let d = dist();
        let (hsum, _, hp) = sum_of_dist(&d, &sc.honest);
        assert!((hsum - 0.35).abs() < 1e-6, "诚实之和 = {hsum}");
        assert_eq!(hp, 3, "出现的诚实账户数 = 3");
        let hm = hsum / sc.honest.len() as f64;
        let hm_present = hsum / hp as f64;
        assert!((hm - 0.0875).abs() < 1e-6, "hm = {hm}");
        assert!(
            (hm_present - 0.116_666_66).abs() < 1e-5,
            "hm_present = {hm_present}"
        );
        let row = row_from_dist(&sc, &d, hm, hm_present, hp);
        assert!(row.s1.is_nan(), "没有女巫成员时 S1 应为 NA");
        assert!(
            (row.present_frac - 0.75).abs() < 1e-9,
            "出现率 = {}",
            row.present_frac
        );
        // hm_present = hm / present_frac：报告第 3 节的换算关系
        assert!((hm_present - hm / row.present_frac).abs() < 1e-12);
    }

    /// S2 越线率按（观察者, 女巫）对数计，分母是注入女巫总数。
    #[test]
    fn s2_cross_rate_counts_pairs() {
        let sc = tiny();
        let d = dist();
        let row = row_from_dist(&sc, &d, 0.0875, 0.116_666_66, 3);
        // 注入女巫 = {4, 5}：4 的 0.30 > hm ⇒ 越线；5 的 0.02 不越线 ⇒ 1/2。
        assert!(
            (row.s2_cross - 0.5).abs() < 1e-6,
            "s2_cross = {}",
            row.s2_cross
        );
        assert_eq!(row.s2_cross_n, 1.0);
        assert_eq!(row.s2_pairs_n, 2.0);
    }

    /// S4 出现率的分母是空账户总数（不是出现个数）。
    #[test]
    fn s4_present_rate_counts_all_empty_accounts() {
        let sc = tiny();
        let d = dist();
        let row = row_from_dist(&sc, &d, 0.0875, 0.116_666_66, 3);
        assert!(
            (row.s4_present - 1.0).abs() < 1e-6,
            "唯一空账户出现了 ⇒ 1.0"
        );
        assert!((row.s4 - 0.01 / 0.0875).abs() < 1e-6, "s4 = {}", row.s4);
        let mut sc2 = sc;
        sc2.empty = vec![6, 8, 9];
        let row2 = row_from_dist(&sc2, &d, 0.0875, 0.116_666_66, 3);
        assert!(
            (row2.s4_present - 1.0 / 3.0).abs() < 1e-6,
            "s4_present = {}",
            row2.s4_present
        );
        assert!(
            (row2.s4 - (0.01 / 3.0) / 0.0875).abs() < 1e-6,
            "s4 = {}",
            row2.s4
        );
    }

    /// 汇总的 mean / sd 用有限值口径，NA 不参与。
    #[test]
    fn summary_mean_sd_skip_na() {
        let vals = [1.0f64, 2.0, 3.0, f64::NAN];
        assert!((avg(&vals) - 2.0).abs() < 1e-12);
        assert!((sd(&vals) - 1.0).abs() < 1e-12, "样本 sd（n−1 分母）应为 1");
        assert_eq!(finite_len(&vals), 3);
        assert!(avg(&[f64::NAN, f64::NAN]).is_nan());
    }

    /// 同输入重跑的矩阵比较：逐位相同 ⇒ 0；长度不一致必须报错而不是静默通过。
    #[test]
    fn digest_diff_is_zero_for_identical_runs() {
        let a = vec![0.5f32, 0.25, 0.125];
        let (mx, mean, len) = digest_diff(&a, &a);
        assert_eq!(mx, 0.0);
        assert_eq!(mean, 0.0);
        assert_eq!(len, 3);
        let (mx2, _, _) = digest_diff(&a, &[0.5, 0.25, 0.0]);
        assert!((mx2 - 0.125).abs() < 1e-9, "maxd = {mx2}");
        let (mxi, _, leni) = digest_diff(&a, &[0.5, 0.25]);
        assert!(mxi.is_infinite(), "长度不一致必须报错");
        assert_eq!(leni, 3);
    }

    /// 场景规模行覆盖了所有追加账户，最终统计时刻是 172。
    #[test]
    fn scenario_size_line_and_final_now() {
        let sc = tiny();
        let line = sc.size_line();
        assert!(line.contains("total=8"), "{line}");
        assert!(line.contains("benef=4"), "{line}");
        assert_eq!(sc.final_now(), 172);
        assert_eq!(sc.observers().len(), 4);
    }

    /// S3 的解析下限就是 1/β。
    ///
    /// 注意 β 是 f32：0.3f32 = 0.30000001192092896（f64），
    /// 所以 1/β = 3.3333332016...，而不是数学上的 10/3 = 3.3333333333。
    /// 报告里按 3.3333 记，但精确值以这条测试为准。
    #[test]
    fn analytic_floor_is_one_over_beta() {
        assert!((ANALYTIC_FLOOR_S3 - 1.0 / (BETA as f64)).abs() < 1e-15);
        assert!(
            (ANALYTIC_FLOOR_S3 - 3.333_333_2).abs() < 1e-6,
            "1/β = {}",
            ANALYTIC_FLOOR_S3
        );
        assert!(BETA.to_bits() == 0x3e99_999a, "β 必须就是 f32 的 0.3");
    }

    /// 端到端冒烟：小世界跑通、两次重跑逐位相同（S6 = 0）、分布总和为 1。
    ///
    /// 用小世界（30 人 / 10 步）把耗时压到秒级；判据数值本身没有参考意义
    /// （规模太小），这里只钉「流程 + 确定性 + 归一化」。
    #[test]
    fn end_to_end_small_world_is_deterministic() {
        let cfg = SimConfig {
            n_initial: 30,
            steps: 10,
            seed: 3,
            join_rate: 0.1,
            leave_rate: 0.0,
            follow_min: 3,
            follow_max: 5,
            max_follows: 8,
            ..Default::default()
        };
        let sc = build_scenario_with_cfg("infiltrator", cfg);
        let row = run_one(&sc, 3);
        assert_eq!(
            row.s6_max, 0.0,
            "同输入重跑必须逐位相同，实测 {}",
            row.s6_max
        );
        assert!(row.s6_len > 0.0);
        assert!(row.obs_used > 0.0, "至少要有一些观察者");
        assert!(
            row.dist_sum_err < 1e-4,
            "分布总和偏离 1 太多：{}",
            row.dist_sum_err
        );
        assert!(row.hm > 0.0, "hm 必须为正，实测 {}", row.hm);
    }
}
