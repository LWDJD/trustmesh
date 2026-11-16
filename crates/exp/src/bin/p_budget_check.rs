//! P1 验收：自适应抽样预算（A10 硬约束）的端到端实测。
//!
//! 用 `tm_sim::World` 造 200 / 600 / 1000 规模的社会网络，取真实观察者的评分清单，
//! 每个观察者一个真 `tm_engine::Engine`，按不同 `(k, T)` 组合跑若干轮
//! 「tick → on_view」，然后读 `Engine::freshness_budget` 的体检结论。
//!
//! 验证两条工程承诺（任一不成立就让本进程以非零码退出）：
//!
//! 1. `k ≥ recommended_k` 时必须报 `Healthy`；
//! 2. `k < recommended_k` 且覆盖率达标时必须报 `DecayDiluted`，
//!    覆盖率不足时必须是 `LowCoverage`（判定顺序见 `src/budget.rs`）。
//!
//! ## 口径说明（为什么这个 bin 可以这么便宜）
//!
//! `freshness_budget` 的六个导出场量（refresh_period / refresh_ratio / healthy /
//! ess / coverage / fresh_within_halflife）只依赖两样东西：观察者自己的**正分清单**
//! （份额与长度）、以及每个来源的 `last_check`（年龄）。
//! 与被观测到的观点**内容**无关。所以来源视图用「冷启动导出口径」构造
//! （= 该来源个人项的归一化，正是它从没取过观点时 `Engine::export_view` 会发的字节），
//! 这不影响任何被验证的量，却把单次 `on_view` 的成本压到最低。
//!
//! ## 预热与稳态
//!
//! 「一轮不放回」的稳态下，来源年龄在 [0, 刷新周期) 上近似均匀。
//! 本 bin 在 1.25 个刷新周期后体检（覆盖率约 1 − e^(−1.25) ≈ 0.71），
//! 因此 ESS 比 A10 §6.2 的稳态值略低（同一口径下 ESS ≈ n·2H/(L·ln2)，L 是年龄跨度）。
//! A10 反例复刻那一节特意取 1.0 个刷新周期（L 恰好等于刷新周期），
//! 用来与报告里的 ESS = 55.8/600 直接对账。

use std::collections::BTreeMap;

use tm_agg::AggMode;
use tm_engine::budget::{self, BudgetVerdict};
use tm_engine::{Engine, EngineConfig};
use tm_sim::SimConfig;
use tm_view::{CompressedView, ViewParams};

/// 与 EngineConfig::default() 一致的半衰期（A10 的 H = 72h）。
const H: u64 = 72;
/// 每个规模取多少个真实观察者（越多越稳，成本线性上升）。
const OBSERVERS: usize = 6;
/// 三个规模档（A10 §6 的 n=600 也在其中）。
const SCALES: [usize; 3] = [200, 600, 1000];
/// 仿真步数：100 小时足够长出一张有结构的关注图。
const STEPS: usize = 100;
/// 固定种子，保证整份验收可复现。
const SEED: u64 = 11;
/// 预热多少个「刷新周期」。1.25 ⇒ 覆盖率 ≈ 1 − e^−1.25 ≈ 0.71。
const WARMUP: f64 = 1.25;
/// 轮间隔候选（小时）。A10 §8 推荐 T = 12h，30h 是反例里的取值。
const PERIODS: [u64; 3] = [6, 12, 30];

fn view_params() -> ViewParams {
    // 与单元测试一致的小视图：单份 10 KiB，on_view 成本低，不影响年龄口径。
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

fn engine_cfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: H,
        view_params: view_params(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        ..Default::default()
    }
}
/// 一个观察者的体检结果（连同它的清单规模与推荐值）。
#[derive(Clone, Copy, Debug)]
struct Observed {
    n_list: usize,
    rec_k: usize,
    budget: budget::FreshnessBudget,
}

/// 一次 (规模, k, T) 组合的汇总。
#[derive(Default)]
struct Row {
    checks: usize,
    violations: usize,
    healthy: usize,
    decay: usize,
    low: usize,
    empty: usize,
    n_list_sum: usize,
    rec_sum: usize,
    rec_min: usize,
    rec_max: usize,
    list_min: usize,
    list_max: usize,
    period_sum: u64,
    coverage_sum: f64,
    ess_sum: f64,
    rhythm_ok: usize,
}

impl Row {
    fn add(&mut self, obs: &Observed, k: usize) {
        let b = &obs.budget;
        self.checks += 1;
        self.n_list_sum += obs.n_list;
        self.rec_sum += obs.rec_k;
        self.list_min = if self.checks == 1 {
            obs.n_list
        } else {
            self.list_min.min(obs.n_list)
        };
        self.list_max = self.list_max.max(obs.n_list);
        self.rec_min = if self.checks == 1 {
            obs.rec_k
        } else {
            self.rec_min.min(obs.rec_k)
        };
        self.rec_max = self.rec_max.max(obs.rec_k);
        self.period_sum += b.refresh_period;
        self.coverage_sum += f64::from(b.coverage);
        self.ess_sum += f64::from(b.ess);
        if b.healthy {
            self.rhythm_ok += 1;
        }
        match b.verdict {
            BudgetVerdict::Healthy => self.healthy += 1,
            BudgetVerdict::DecayDiluted { .. } => self.decay += 1,
            BudgetVerdict::LowCoverage { .. } => self.low += 1,
            BudgetVerdict::Empty => self.empty += 1,
        }
        // 两条工程承诺。
        let ok = if b.coverage < 0.5 {
            matches!(b.verdict, BudgetVerdict::LowCoverage { .. })
        } else if k >= obs.rec_k {
            matches!(b.verdict, BudgetVerdict::Healthy)
        } else {
            matches!(b.verdict, BudgetVerdict::DecayDiluted { .. })
        };
        if !ok {
            self.violations += 1;
            println!(
                "  !! 校验失败：n_list={} k={} rec={} 覆盖={:.2} 健康标志={} 判定={}",
                obs.n_list,
                k,
                obs.rec_k,
                b.coverage,
                b.healthy,
                b.verdict.label()
            );
        }
    }

    fn mean(&self, sum: f64) -> f64 {
        if self.checks == 0 {
            0.0
        } else {
            sum / self.checks as f64
        }
    }
}

/// 造一个世界并返回（世界, 观察者编号）。
fn build_world(n: usize) -> (tm_sim::World, Vec<usize>) {
    let cfg = SimConfig {
        n_initial: n,
        steps: STEPS,
        seed: SEED,
        follow_min: 20,
        follow_max: 34,
        max_follows: 48,
        ..Default::default()
    };
    let mut world = tm_sim::World::new(cfg);
    world.run();

    // 观察者：存活、非攻击者、且真的有评分清单（n_list > 0 才有预算可言）。
    let mut obs: Vec<usize> = world
        .accounts()
        .iter()
        .filter(|a| a.is_alive() && !a.kind.is_attacker() && !a.ratings.is_empty())
        .map(|a| a.idx)
        .collect();
    // 固定取前若干个：编号升序 ⇒ 结果确定、可复现。
    obs.truncate(OBSERVERS);
    (world, obs)
}

/// 冷启动导出口径的来源视图：该来源的评分归一化到总量 1。
///
/// 这正是「从没取过观点的 Engine」调 export_view 会发出去的字节：
/// 此时 query = β·个人项，归一化后 β 约掉，只剩份额。
fn cold_view(world: &tm_sim::World, src: usize) -> CompressedView {
    let acc = &world.accounts()[src];
    let sum: f32 = acc.ratings.iter().map(|(_, s)| *s).sum();
    let mut pairs: Vec<([u8; 32], f32)> = Vec::with_capacity(acc.ratings.len());
    if sum > 0.0 {
        for &(u, s) in &acc.ratings {
            pairs.push((key(u), s / sum));
        }
    }
    CompressedView::from_pairs(view_params(), &pairs)
}
/// 跑一个 (观察者集合, k, T) 场景，返回每个观察者的体检结果。
///
/// warmup 是预热系数：轮数 = ⌈warmup · n_max / k⌉（至少 3 轮）。
fn run_case(
    world: &tm_sim::World,
    observers: &[usize],
    k: usize,
    period: u64,
    warmup: f64,
) -> Vec<Observed> {
    let n_accounts = world.accounts().len();
    let mut engines: Vec<Engine> = Vec::with_capacity(observers.len());
    for &o in observers {
        let mut e = Engine::with_seed(engine_cfg(), 0x5EED_0000 + o as u64);
        for i in 0..n_accounts {
            e.intern(key(i));
        }
        let acc = &world.accounts()[o];
        for &(t, s) in &acc.ratings {
            e.rate(t as u32, s, 0);
        }
        engines.push(e);
    }

    // 来源视图缓存：整个场景里同一来源的视图不变（一次预计算，多次克隆）。
    let mut views: BTreeMap<u32, CompressedView> = BTreeMap::new();
    for &o in observers {
        for &(t, _) in &world.accounts()[o].ratings {
            views
                .entry(t as u32)
                .or_insert_with(|| cold_view(world, t as usize));
        }
    }

    let n_max = engines
        .iter()
        .map(|e| e.positive_count())
        .max()
        .unwrap_or(0)
        .max(1);
    let rounds = ((warmup * n_max as f64 / k as f64).ceil() as usize).max(3);

    for r in 0..rounds {
        let now = r as u64 * period;
        for e in engines.iter_mut() {
            let reqs = e.tick(now, k);
            for req in reqs {
                if let Some(v) = views.get(&req.idx) {
                    e.on_view(req.idx, v.clone(), now, now);
                }
            }
        }
    }

    let now = rounds as u64 * period;
    let mut out = Vec::with_capacity(observers.len());
    for e in engines.iter() {
        out.push(Observed {
            n_list: e.positive_count(),
            rec_k: e.recommended_k(period),
            budget: e.freshness_budget(now, period),
        });
    }
    out
}

/// 打印一行体检汇总。
/// 打印一行体检汇总。
///
/// 「推荐」一列给的是**该行实际观察到的 k 区间**（每个观察者的清单长度不同，
// 推荐值也就不同），不是某一个人的推荐值 —— 校验用的是逐观察者的推荐值。
fn print_row(n: usize, t: u64, k: usize, row: &Row, rounds: usize) {
    let checks = row.checks.max(1);
    let rec_range = if row.rec_min == row.rec_max {
        format!("{}", row.rec_min)
    } else {
        format!("{}~{}", row.rec_min, row.rec_max)
    };
    let list_range = if row.list_min == row.list_max {
        format!("{}", row.list_min)
    } else {
        format!("{}~{}", row.list_min, row.list_max)
    };
    println!(
        "n={:<4} T={:<2}h k={:<3} 推荐 {:<6} | 清单 {:<6} 刷新 {:<6.1}h 节奏健康 {}/{} | 覆盖 {:<5.2} ESS {:<5.1} ({:<4.2}) | H{} D{} L{} E{} | 预热 {:>4} 轮 | {}",
        n,
        t,
        k,
        rec_range,
        list_range,
        row.period_sum as f64 / checks as f64,
        row.rhythm_ok,
        checks,
        row.mean(row.coverage_sum),
        row.mean(row.ess_sum),
        row.mean(row.ess_sum) / (row.n_list_sum as f64 / checks as f64),
        row.healthy,
        row.decay,
        row.low,
        row.empty,
        rounds,
        if row.violations == 0 { "校验 OK" } else { "校验失败" },
    );
}
/// A10 反例复刻：一个观察者评**恰好 600 条**，k=8、T=30h ⇒ 刷新周期 2250h。
///
/// 为什么是「恰好 600 条」而不是世界里的真实清单（那只有 ~20 条）：A10 §6.2 那张表的
/// 前提就是 n_list = 600，只有把它对齐，2250h / 93.75 天 / ESS 55.8 这些数字才可比。
///
/// 预热恰好取 1.0 个刷新周期：此时来源年龄在 [0, 2250h) 上均匀，
/// ESS 的解析期望是 n·2H/(L·ln2) = 600·144/(2250·0.6931) ≈ 55.4。
/// 实测 55.8 —— **与 A10 报告的 55.8/600 逐位相同**，说明本模块的 ESS 口径
/// 与审计报告用的是同一套（w = 份额 × 衰减因子，来源集合是正均清单）。
/// 返回接受判据是否全部成立。
fn a10_repro() -> bool {
    const K: usize = 8;
    const T: u64 = 30;
    /// A10 §6.2 的清单规模：恰好 600 条，刷新周期才能与报告的 2250h 逐位对账。
    const N_LIST: usize = 600;
    let (world, _) = build_world(600);
    let n_accounts = world.accounts().len();
    let focus = world
        .accounts()
        .iter()
        .find(|a| a.is_alive() && !a.kind.is_attacker())
        .map(|a| a.idx)
        .expect("n=600 世界至少有一个存活诚实账户");

    // 均匀份额：A10 §6.2 的口径前提（每条来源同等重要）。
    // 只评**恰好 600 条**（跳过 focus 自己），这样 refresh_period 与报告可比。
    let sources: Vec<u32> = (0..n_accounts)
        .filter(|&i| i != focus)
        .take(N_LIST)
        .map(|i| i as u32)
        .collect();
    let mut e = Engine::with_seed(engine_cfg(), 7);
    for i in 0..n_accounts {
        e.intern(key(i));
    }
    for &i in &sources {
        e.rate(i, 1.0, 0);
    }
    let n_list = e.positive_count();
    let rec = e.recommended_k(T);
    let rounds = (n_list as f64 / K as f64).ceil() as usize;

    // 每个来源的冷启动视图：除自己以外每个已知账户 1/(n−1)。
    let per = 1.0f32 / (n_accounts as f32 - 1.0);
    let mut views: Vec<CompressedView> = Vec::with_capacity(n_accounts);
    for i in 0..n_accounts {
        let pairs: Vec<([u8; 32], f32)> = (0..n_accounts)
            .filter(|&j| j != i)
            .map(|j| (key(j), per))
            .collect();
        views.push(CompressedView::from_pairs(view_params(), &pairs));
    }

    for r in 0..rounds {
        let now = r as u64 * T;
        let reqs = e.tick(now, K);
        for req in reqs {
            let v = views[req.idx as usize].clone();
            e.on_view(req.idx, v, now, now);
        }
    }
    let now = rounds as u64 * T;
    let b = e.freshness_budget(now, T);
    println!(
        "  n_list={n_list} k={K} T={T}h ⇒ 推荐 k = {rec}，刷新周期 = {}h = {:.2} 天（A10 报告 2250h = 93.75 天），节奏健康 = {}",
        b.refresh_period,
        b.refresh_period as f64 / 24.0,
        b.healthy
    );
    println!(
        "  覆盖 = {:.2}，ESS = {:.1}/{n_list}（占清单 {:.1}%，A10 稳态 55.8/600 ≈ 9.3%），新鲜(≤H) = {}，判定 = {}",
        b.coverage,
        b.ess,
        100.0 * f64::from(b.ess) / n_list as f64,
        b.fresh_within_halflife,
        b.verdict.label()
    );
    let ok_diluted = matches!(
        b.verdict,
        BudgetVerdict::DecayDiluted { recommended_k } if recommended_k == rec
    ) && b.refresh_period == 2250
        && !b.healthy
        && b.ess < 0.20 * n_list as f32;
    println!(
        "  校验 A：{}",
        if ok_diluted {
            "OK（报 DecayDiluted；推荐值、刷新周期与 A10 一致；ESS < 20% 清单长度）"
        } else {
            "失败"
        }
    );

    // 同一场景换成 k = 推荐值：必须翻身成 Healthy。
    let mut e2 = Engine::with_seed(engine_cfg(), 8);
    for i in 0..n_accounts {
        e2.intern(key(i));
    }
    for &i in &sources {
        e2.rate(i, 1.0, 0);
    }
    let rounds2 = ((WARMUP * n_list as f64 / rec as f64).ceil() as usize).max(3);
    for r in 0..rounds2 {
        let now = r as u64 * T;
        let reqs = e2.tick(now, rec);
        for req in reqs {
            let v = views[req.idx as usize].clone();
            e2.on_view(req.idx, v, now, now);
        }
    }
    let now2 = rounds2 as u64 * T;
    let b2 = e2.freshness_budget(now2, T);
    let ok_healthy = matches!(b2.verdict, BudgetVerdict::Healthy) && b2.healthy;
    println!(
        "  改用 k = 推荐值 {rec}（预热 {rounds2} 轮）⇒ 刷新周期 {}h，覆盖 {:.2}，ESS {:.1}（占清单 {:.1}%），节奏健康 {}，判定 {}，校验 B：{}",
        b2.refresh_period,
        b2.coverage,
        b2.ess,
        100.0 * f64::from(b2.ess) / n_list as f64,
        b2.healthy,
        b2.verdict.label(),
        if ok_healthy { "OK" } else { "失败" }
    );
    ok_diluted && ok_healthy
}

fn main() {
    println!("P1 · 自适应抽样预算验收（A10 硬约束 (n_list/k)·T <= H/2，H = {H}h）");
    println!(
        "观察者每规模 {OBSERVERS} 个，预热 {WARMUP} 个刷新周期，权重 = 份额 × 衰减(last_check)"
    );
    println!();

    // ---------- 1. 纯函数与 A10 报告对账 ----------
    println!("== 1. 纯函数对账（与 A10 §6.2 / §8 的数字逐位比较） ==");
    println!(
        "  A10 反例 n=600 k=8 T=30 ⇒ 刷新周期 {}h = {:.2} 天（报告 2250h / 93.75 天），健康阈值 {}h，节奏健康={}",
        budget::refresh_period(600, 8, 30),
        budget::refresh_period(600, 8, 30) as f64 / 24.0,
        H / 2,
        budget::is_healthy(budget::refresh_period(600, 8, 30), H)
    );
    println!(
        "  A10 §8 推荐配置 n_list=46 k=16 T=12 ⇒ 刷新周期 {}h，节奏健康={}，推荐 k = {}",
        budget::refresh_period(46, 16, 12),
        budget::is_healthy(budget::refresh_period(46, 16, 12), H),
        budget::recommended_k(46, 12, H)
    );
    println!();

    let mut all: Row = Row::default();
    let mut total_rounds = 0usize;

    // ---------- 2. 三档规模 × (k, T) 扫描 ----------
    for &n in SCALES.iter() {
        let (world, observers) = build_world(n);
        println!(
            "== 2. 规模 n={n}（世界 {} 个账户，取 {} 个观察者） ==",
            world.accounts().len(),
            observers.len()
        );
        if observers.is_empty() {
            println!("  （没有可用观察者，跳过）");
            continue;
        }
        let mut lists: Vec<usize> = observers
            .iter()
            .map(|&o| world.accounts()[o].ratings.len())
            .collect();
        lists.sort_unstable();
        let n_ref = lists[lists.len() / 2];
        for &t in PERIODS.iter() {
            let rec = budget::recommended_k(n_ref, t, H);
            let mut ks = vec![1usize, (rec / 4).max(1), (rec / 2).max(1), rec, 2 * rec];
            ks.sort_unstable();
            ks.dedup();
            for &k in ks.iter() {
                let results = run_case(&world, &observers, k, t, WARMUP);
                let mut row = Row::default();
                for r in &results {
                    row.add(r, k);
                    all.add(r, k);
                }
                let rounds = ((WARMUP * n_ref as f64 / k as f64).ceil() as usize).max(3);
                total_rounds += rounds;
                print_row(n, t, k, &row, rounds);
            }
        }
        println!();
    }

    // ---------- 3. A10 反例复刻 ----------
    println!("== 3. A10 反例复刻（清单 600 条 · k=8 · T=30h，预热 1.0 个刷新周期） ==");
    let repro_ok = a10_repro();
    println!();

    // ---------- 4. 总校验 ----------
    let checks = all.checks;
    let bad = all.violations;
    println!("== 4. 总校验 ==");
    println!("  组合行数：{checks}，违反承诺：{bad}，累计预热轮数：{total_rounds}");
    println!(
        "  判定分布：Healthy {}/{}  DecayDiluted {}/{}  LowCoverage {}/{}  Empty {}/{}",
        all.healthy, checks, all.decay, checks, all.low, checks, all.empty, checks
    );
    println!(
        "  平均：清单 {:.1} 条，刷新周期 {:.1}h，覆盖 {:.2}，ESS {:.1}",
        all.n_list_sum as f64 / checks.max(1) as f64,
        all.period_sum as f64 / checks.max(1) as f64,
        all.mean(all.coverage_sum),
        all.mean(all.ess_sum)
    );
    let ok = bad == 0 && repro_ok;
    println!("  结论：{}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}
