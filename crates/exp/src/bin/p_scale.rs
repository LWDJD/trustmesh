//! # 大规模仿真：这库在 1000/3000 人规模上还跑得动吗？
//!
//! B0 基线的规模是 n=200、8 个 seed。那只能回答「小社区能不能用」，
//! 回答不了「一个真实应用（几千人）能不能跑」。A11 报告给过单次操作的复杂度
//! （`trust_distribution` 是 Θ(n·m)，n=1600/m=800 时 1.43 CPU 秒），
//! 但没有把**完整传播流程 + 攻击场景**一起跑过。
//!
//! 这个 bin 补的就是那一块，输出三类数字：
//!
//! 1. **每条操作的真实耗时**（rate / tick / on_view / export_view / query / trust_distribution）
//! 2. **内存占用**（视图缓存 + 每份观点的字节数）
//! 3. **判据是否仍然成立**（S1/S2/S6 在小规模上逐位为 0，大规格上还成不成立）
//!
//! 用法（在 `crates/exp` 下）：
//!
//! ```powershell
//! $env:TMP="<仓库根>/.tmp"; $env:TEMP=$env:TMP
//! cargo run --release --bin p_scale -- 200 600 1000
//! ```
//!
//! 参数是逗号分隔的账户规模；不给则跑内置的 {200, 600, 1000}。
//!
//! ## 为什么用 n 而不是「账户总数」
//!
//! 这里 n 同时是「账户数」和「观察者数」——每个账户都是一个独立引擎，
//! 都有自己的一份正分清单和一份观点缓存。这是最坏情况（真实网络里很多账户离线）。

use std::time::Instant;

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};
use tm_sim::{SimConfig, World};

/// 引擎配置。桶数由环境变量 `P_SCALE_BUCKETS` 决定（默认 512）。
///
/// 为什么要把桶数做成参数：观点是 Count Sketch，桶越少碰撞噪声越大，
/// 而噪声会让「从没被评过分的账户」也拿到一点点声望（S4）。
/// 这个 bin 要能量化「桶数 → S1 偏差」这条因果链。
fn engine_cfg() -> EngineConfig {
    let buckets: usize = std::env::var("P_SCALE_BUCKETS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&b| b > 0 && b <= ViewParams::MAX_CELLS / 5)
        .unwrap_or(512);
    // 噪声底乘数：默认 1.0。把它调大等于「只接受明显高于噪声的读数」，
    // 可以抑制 Count Sketch 碰撞噪声给未表态账户凭空发声望（S4 机制）。
    let noise_mul: f32 = std::env::var("P_SCALE_NOISE")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|x| *x >= 0.0 && x.is_finite())
        .unwrap_or(1.0);
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams::new(buckets, 5, 0),
        noise_multiplier: noise_mul,
        ..EngineConfig::default()
    }
}

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = (i & 0xFF) as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = ((i >> 16) & 0xFF) as u8;
    k[3] = 0xC5;
    k
}

/// 某个规模的完整实测结果。
struct Row {
    n: usize,
    edges: usize,
    observers: usize,
    // 耗时（毫秒）
    ms_build: f64,
    ms_tick_total: f64,
    ms_onview_total: f64,
    ms_export_total: f64,
    ms_query_total: f64,
    ms_dist_total: f64,
    // 每次调用的平均耗时（微秒）
    us_tick: f64,
    us_onview: f64,
    us_export: f64,
    us_query: f64,
    // 规模指标
    views_cached: usize,
    view_bytes: usize,
    view_footprint_mb: f64,
    // 判据
    s1: f64,
    s2_cross: f64,
    s6: f64,
}

/// 跑一个规模。
fn run_one(n: usize, seed: u64) -> Row {
    // 用女巫集群做攻击，这样 S1/S2 才有东西可测（S1 应该恒为 0、S2 越线率应该恒为 0）。
    let sim_cfg = SimConfig {
        n_initial: n,
        steps: 100,
        max_accounts: n + 200,
        attack_ratio: 0.08,
        attack_kind: tm_sim::AttackKind::Sybil { cluster: 4 },
        ..SimConfig::default()
    };
    let rhythm = sim_cfg.enable_rhythm;
    let steps = sim_cfg.steps as u64;
    let mut world = World::new(sim_cfg);
    world.run();

    let _snap = world.report();
    let ace = world.accounts();
    let ratings = world.ratings();
    let now_last = steps;

    // 参与传播的账户：在线且活着（离线账户不参与，这是真实网络的样子）。
    let alive: Vec<usize> = (0..ace.len())
        .filter(|&i| ace[i].is_alive() && ace[i].is_online(now_last, rhythm))
        .collect();

    // 判据的观察者口径：**只看诚实账户**（见 docs/audit/A6-ablation.md §1.2）。
    // 把攻击者也当观察者会污染 S1：女巫会给自己人打满分，于是「女巫均值」不为 0，
    // 但那不是算法失效，是观察者选错了。这个坑必须在这里挡住。
    let honest_obs: Vec<usize> = alive
        .iter()
        .copied()
        .filter(|&i| !ace[i].kind.is_attacker() && !matches!(ace[i].kind, tm_sim::Kind::Promoted))
        .collect();
    if alive.is_empty() {
        return Row {
            n,
            edges: 0,
            observers: 0,
            ms_build: 0.0,
            ms_tick_total: 0.0,
            ms_onview_total: 0.0,
            ms_export_total: 0.0,
            ms_query_total: 0.0,
            ms_dist_total: 0.0,
            us_tick: 0.0,
            us_onview: 0.0,
            us_export: 0.0,
            us_query: 0.0,
            views_cached: 0,
            view_bytes: 0,
            view_footprint_mb: 0.0,
            s1: f64::NAN,
            s2_cross: f64::NAN,
            s6: f64::NAN,
        };
    }

    let cfg = engine_cfg();
    let total_accounts = ace.len();

    // ---- 建引擎 ----
    let t_build = Instant::now();
    let mut engines: Vec<Option<Engine>> = (0..total_accounts).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::with_seed(cfg.clone(), seed ^ (a as u64) ^ 0x5CA1E);
        for i in 0..total_accounts {
            e.intern(key(i));
        }
        engines[a] = Some(e);
    }
    let ms_build = t_build.elapsed().as_secs_f64() * 1000.0;

    // ---- 灌入评分（只看观察者对在线账户的评分，离线目标照样 intern 过）----
    let t_rate = Instant::now();
    for &(o, t, s) in &ratings {
        if let Some(e) = engines.get_mut(o).and_then(|x| x.as_mut()) {
            e.rate(t as u32, s, 0);
        }
    }
    let _ms_rate = t_rate.elapsed().as_secs_f64() * 1000.0;

    // ---- 四轮传播 ----
    let rounds = 4u64;
    let hpr = 24u64;
    let k = 16usize; // A10 推荐值
    let now0 = 100u64;
    let mut ms_tick_total = 0.0;
    let mut ms_onview_total = 0.0;
    let mut ms_export_total = 0.0;
    let mut view_bytes = 0usize;

    for r in 0..rounds {
        let now = now0 + r * hpr;
        // 导出（每份观点）
        let mut published: Vec<Option<CompressedView>> =
            (0..total_accounts).map(|_| None).collect();
        let t = Instant::now();
        for &a in &alive {
            if let Some(e) = engines[a].as_ref() {
                let v = e.export_view(now);
                if view_bytes == 0 {
                    view_bytes = v.bytes();
                }
                published[a] = Some(v);
            }
        }
        ms_export_total += t.elapsed().as_secs_f64() * 1000.0;

        // 抽样 + 收取
        let mut fetched: Vec<(usize, u32)> = Vec::new();
        let t = Instant::now();
        for &a in &alive {
            if let Some(e) = engines[a].as_mut() {
                for req in e.tick(now, k) {
                    fetched.push((a, req.idx));
                }
            }
        }
        ms_tick_total += t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        for (a, idx) in fetched {
            let src = idx as usize;
            if let Some(Some(v)) = published.get(src) {
                if let Some(e) = engines[a].as_mut() {
                    e.on_view(idx, v.clone(), now, now);
                }
            }
        }
        ms_onview_total += t.elapsed().as_secs_f64() * 1000.0;
    }

    let now = now0 + rounds * hpr;

    // ---- 查询耗时 ----
    let t = Instant::now();
    let mut count = 0u64;
    for &a in &alive {
        if let Some(e) = engines[a].as_ref() {
            for i in 0..total_accounts as u32 {
                let _ = e.query(i, now);
                count += 1;
            }
        }
    }
    let ms_query_total = t.elapsed().as_secs_f64() * 1000.0;

    // ---- 分布耗时 ----
    let t = Instant::now();
    for &a in &alive {
        if let Some(e) = engines[a].as_ref() {
            let _ = e.trust_distribution(now);
        }
    }
    let ms_dist_total = t.elapsed().as_secs_f64() * 1000.0;

    // ---- 判据 ----
    let (s1, s2_cross) = measure_s1_s2(&engines, &honest_obs, &ace, total_accounts, now);
    let s6 = measure_s6(&engines, &alive, total_accounts, now);

    let views_cached: usize = alive
        .iter()
        .filter_map(|&a| engines[a].as_ref())
        .map(|e| e.stats().2)
        .sum();

    Row {
        n,
        edges: ratings.len(),
        observers: honest_obs.len(),
        ms_build,
        ms_tick_total,
        ms_onview_total,
        ms_export_total,
        ms_query_total,
        ms_dist_total,
        us_tick: if !alive.is_empty() {
            ms_tick_total * 1000.0 / (alive.len() as f64 * rounds as f64)
        } else {
            0.0
        },
        us_onview: if !alive.is_empty() {
            ms_onview_total * 1000.0 / (alive.len() as f64 * k as f64 * rounds as f64)
        } else {
            0.0
        },
        us_export: if !alive.is_empty() {
            ms_export_total * 1000.0 / (alive.len() as f64 * rounds as f64)
        } else {
            0.0
        },
        us_query: if count > 0 {
            ms_query_total * 1000.0 / count as f64
        } else {
            0.0
        },
        views_cached,
        view_bytes,
        view_footprint_mb: views_cached as f64 * view_bytes as f64 / (1024.0 * 1024.0),
        s1,
        s2_cross,
        s6,
    }
}

/// S1（女巫均值 ÷ hm）与 S2 越线率。
fn measure_s1_s2(
    engines: &[Option<Engine>],
    alive: &[usize],
    ace: &[tm_sim::Account],
    total: usize,
    now: u64,
) -> (f64, f64) {
    let mut s1_vals = Vec::new();
    let mut cross_n = 0u64;
    let mut cross_d = 0u64;

    for &a in alive {
        let e = match engines[a].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let dist: Vec<f32> = (0..total as u32).map(|i| e.query(i, now)).collect();

        // hm = 诚实账户的均值（没在分布里的按 0 计入）
        let honest: Vec<usize> = (0..total)
            .filter(|&i| ace[i].is_alive() && !ace[i].kind.is_attacker())
            .collect();
        if honest.is_empty() {
            continue;
        }
        let hm: f64 = honest.iter().map(|&i| dist[i] as f64).sum::<f64>() / honest.len() as f64;

        if hm > 0.0 {
            let sybils: Vec<usize> = (0..total)
                .filter(|&i| matches!(ace[i].kind, tm_sim::Kind::Sybil { .. }))
                .collect();
            if !sybils.is_empty() {
                let m: f64 =
                    sybils.iter().map(|&i| dist[i] as f64).sum::<f64>() / sybils.len() as f64;
                s1_vals.push(m / hm);
            }
            for &i in &sybils {
                cross_d += 1;
                if (dist[i] as f64) > hm {
                    cross_n += 1;
                }
            }
        }
    }

    let s1 = if s1_vals.is_empty() {
        f64::NAN
    } else {
        s1_vals.iter().sum::<f64>() / s1_vals.len() as f64
    };
    let s2 = if cross_d > 0 {
        cross_n as f64 / cross_d as f64
    } else {
        f64::NAN
    };
    (s1, s2)
}

/// S6：同一个引擎跑两次 query，差必须为 0（这里只做自洽检查，跨引擎对比需要重建）。
fn measure_s6(engines: &[Option<Engine>], alive: &[usize], total: usize, now: u64) -> f64 {
    let mut max_diff = 0.0f64;
    for &a in alive {
        let e = match engines[a].as_ref() {
            Some(e) => e,
            None => continue,
        };
        for i in 0..total as u32 {
            let x = e.query(i, now);
            let y = e.query(i, now);
            let d = (x as f64 - y as f64).abs();
            if d > max_diff {
                max_diff = d;
            }
        }
    }
    max_diff
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sizes: Vec<usize> = if args.is_empty() {
        vec![200, 600, 1000]
    } else {
        // 参数既可以用逗号也可以用空格分隔（"200,600" 和 "200 600" 都行）。
        args.join(",")
            .split(',')
            .flat_map(|s| s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    };

    println!("# p_scale —— 大规模仿真（每规模 1 个 seed，这是规模问题不是统计问题）");
    let buckets = {
        let cfg = engine_cfg();
        let p = cfg.view_params;
        // ViewParams 没有公开的 buckets getter（保持私有不变量），
        // 这里从环境变量回读同一个值，保证打印的和实际用的一致。
        let _ = p;
        std::env::var("P_SCALE_BUCKETS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&b| b > 0 && b <= ViewParams::MAX_CELLS / 5)
            .unwrap_or(512)
    };
    let noise_mul = engine_cfg().noise_multiplier;
    println!("# k = 16（A10 推荐值），4 轮传播，每轮 24h，半衰期 72h，view {buckets}×5，噪声底乘数 {noise_mul}");
    println!("# 每行：n accounts edges observers | build_ms tick_us onview_us export_us query_us dist_ms | views KB footprint_MB | S1 S2越线率 S6");

    for &n in &sizes {
        let seed = 0x5CA1E_0000 + n as u64;
        let row = run_one(n, seed);
        println!(
            "n={:<5} edges={:<6} observers={:<5} | build={:<7.1} tick={:<8.1} onview={:<8.1} export={:<8.1} query={:<7.1} dist={:<8.1} (ms)",
            row.n,
            row.edges,
            row.observers,
            row.ms_build,
            row.ms_tick_total,
            row.ms_onview_total,
            row.ms_export_total,
            row.ms_query_total,
            row.ms_dist_total,
        );
        println!(
            "      per-call: tick={:<6.2}us onview={:<6.2}us export={:<7.2}us query={:<5.2}us | views={:<6} {}B {:>6.1}MB | S1={:<14} S2={:<14} S6={:.0}",
            row.us_tick,
            row.us_onview,
            row.us_export,
            row.us_query,
            row.views_cached,
            row.view_bytes,
            row.view_footprint_mb,
            format!("{:.8}", row.s1),
            format!("{:.8}", row.s2_cross),
            row.s6,
        );
    }

    // ---- 外推：n 翻倍时耗时怎么涨 ----
    println!();
    println!("# 说明：trust_distribution 是 Θ(n·m)，所以 dist 列应随 n 平方增长；");
    println!("# tick/on_view/export/query 是 Θ(n) 或 Θ(k)，应近线性。");
}
