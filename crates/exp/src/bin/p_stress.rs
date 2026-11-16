//! P2 安全加固的压力测试（只依赖 std）。
//!
//! 四个场景，对应本轮修的三个缺陷：
//!
//! | 场景 | 喂什么 | 验什么 |
//! | --- | --- | --- |
//! | A | 10000 个不同 owner 的观点，max_views = 1000 | 缓存条数被钉死、rejected_count 增长、内存有界、引擎不崩 |
//! | B | 同一个 owner 更新 100 万次 | 更新路径不增长内存、不崩、连败清零 |
//! | C | NaN / +inf 的恶意视图 | 整条丢弃、引擎状态不被污染、后续 query 仍正常 |
//! | D | tick + on_view 混合 100 万次 | 无 panic、无计数器溢出、views.len() 恒 <= max_views |
//!
//! 另有一个 bench 场景测单次 on_view 的真实耗时，用来解释 B/D 的耗时来源：
//! 单次 on_view 的固定成本是 noise_floor(1000)（1000 次查询），**与 max_views 无关**，
//! 也和「容量上限挡掉多少」无关——被容量拒掉的观点在噪声底之前就 return 了，
//! 所以场景 A 里 9000 次被拒几乎是零成本。
//!
//! # 内存怎么量
//!
//! RSS 用平台原语取（Windows 走 powershell Get-Process WorkingSet64，
//! Linux 读 /proc/self/statm），都不可用时退化为「条数 × 视图字节数」估算。
//! 报告里每条内存数字都标出是「实测 RSS」还是「估算」。
//!
//! # 怎么跑
//!
//! ```text
//! cargo run --release --bin p_stress                  # 全量：A/B/C/D + bench
//! cargo run --release --bin p_stress -- --quick       # 快速冒烟（约 10 秒）
//! cargo run --release --bin p_stress -- --only=a,c --owners=20000
//! ```
//!
//! 参数：
//!
//! | 参数 | 默认 | 含义 |
//! | --- | --- | --- |
//! | --only=a,b,c,d,bench | 全部 | 只跑指定场景 |
//! | --owners=N | 10000 | 场景 A 的 owner 数（同时是「喂了多少份观点」） |
//! | --cap=N | 1000 | 场景 A/D 的 max_views |
//! | --updates=M | 1000000 | 场景 B/D 的 on_view / 混合次数 |
//! | --control=N | 3000 | 对照组（max_views=0）喂多少个 owner（用来证明「不设上限确实无界」） |
//! | --params-ac=default\|fast | default | 场景 A/C 的视图形状参数 |
//! | --params-bd=default\|fast | fast | 场景 B/D 的视图形状参数 |
//! | --quick | 关 | 所有数量缩小到 1/20 左右 |
//!
//! 断言失败会直接 panic（进程退出码非 0），这是有意的：压力测试必须真的能红。

use std::time::{Duration, Instant};

use tm_engine::{Engine, EngineConfig};
use tm_store::AccountIdx;
use tm_view::{AccountKey, CompressedView, ViewParams};

/// 观测目标账户的 key 基址（与 owner 的 key 段分开，避免撞号）。
const TARGET_BASE: u32 = 2_000_000;
/// owner 账户的 key 基址。
const OWNER_BASE: u32 = 1_000_000;

/// 每份视图除桶矩阵外的固定开销：BTreeMap 节点 + StoredView 字段 + Vec 头 +
/// 分配器记账。
///
/// 取 384 B 是按 **RSS 实测反推**的上界（见 `审计/P2-security.md` §4.2：默认 2048×5
/// 参数下每份条目的实测开销在 308~375 B 之间，取 384 保证估算不会低估）。
const ENTRY_OVERHEAD: u64 = 384;

fn key(i: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[0..4].copy_from_slice(&i.to_le_bytes());
    k[8] = 0xA5;
    k
}

fn params_preset(name: &str) -> ViewParams {
    match name {
        "fast" => ViewParams::new(64, 1, 99),
        "default" => ViewParams::new(2048, 5, 99),
        other => panic!("未知的形状参数预设 {other}（只支持 default / fast）"),
    }
}

fn ecfg(params: ViewParams, max_views: usize) -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: params,
        max_views,
        ..Default::default()
    }
}

/// 一份合规的观点：对每个 target 声明 weight（单条 <= 1）。
fn view_for(params: ViewParams, targets: &[AccountIdx], weight: f32) -> CompressedView {
    let mut pairs: Vec<(AccountKey, f32)> = Vec::with_capacity(targets.len());
    for (i, _) in targets.iter().enumerate() {
        pairs.push((key(TARGET_BASE + i as u32), weight));
    }
    CompressedView::from_pairs(params, &pairs)
}

/// 含 +inf 桶的视图：add(MAX) 之后 merge 2.0 溢出成 ±inf。
///
/// 关键点：这个视图的 query 返回 **±inf**，而 on_view 里如果不检查原始值、
/// 直接用 clamp(0,1)，+inf 会被静默收成 1.0（有限值），检测就漏了。
fn inf_view(params: ViewParams, poisoned: AccountKey) -> CompressedView {
    let zero = CompressedView::new(params);
    let mut v = CompressedView::new(params);
    v.add(&poisoned, f32::MAX);
    v.merge(&zero, 2.0, 2.0); // MAX*2 = +inf
    v
}

/// 含 NaN 桶的视图：在 inf_view 的基础上再做一次 0×inf。
///
/// 此时 poisoned 的 hashers 个桶是 NaN，其余桶是 0——
/// 也就是「部分行 NaN + 其余有限」的形态（审计 A8 D5 里只做过推理、没实测的那条）。
fn nan_view(params: ViewParams, poisoned: AccountKey) -> CompressedView {
    let zero = CompressedView::new(params);
    let mut v = CompressedView::new(params);
    v.add(&poisoned, f32::MAX);
    v.merge(&zero, 2.0, 2.0); // ±inf
    v.merge(&zero, 0.0, 0.0); // 0*inf = NaN
    v
}

// ---------- 内存 ----------

#[cfg(target_os = "windows")]
fn rss_bytes() -> Option<u64> {
    let pid = std::process::id();
    let script = format!("(Get-Process -Id {pid}).WorkingSet64");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn rss_bytes() -> Option<u64> {
    None
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// 按「条数 × 每份视图字节数 + 每条元数据」估算观点缓存占用的堆内存。
fn estimate_views_bytes(count: usize, params: ViewParams) -> u64 {
    let per_view = (params.buckets * params.hashers * std::mem::size_of::<f32>()) as u64;
    count as u64 * (per_view + ENTRY_OVERHEAD)
}

/// 打印一段内存：有 RSS 就报实测差值，没有就报估算值。
fn report_mem(tag: &str, rss_start: Option<u64>, rss_end: Option<u64>, estimate: u64) {
    match (rss_start, rss_end) {
        (Some(a), Some(b)) => {
            let delta = b.saturating_sub(a);
            println!(
                "  [内存] {tag}：RSS 实测 {:.2} MiB -> {:.2} MiB（Δ {:+.2} MiB）",
                mib(a),
                mib(b),
                mib(delta)
            );
        }
        _ => println!(
            "  [内存] {tag}：RSS 不可用，按估算 {:.2} MiB（条数×视图字节数，含每条 {ENTRY_OVERHEAD} B 元数据）",
            mib(estimate)
        ),
    }
}

// ---------- 参数 ----------

struct Args {
    only: Vec<String>,
    owners: u32,
    cap: usize,
    updates: u64,
    control: u32,
    params_ac: ViewParams,
    params_bd: ViewParams,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            only: vec!["a".into(), "b".into(), "c".into(), "d".into(), "bench".into()],
            owners: 10_000,
            cap: 1_000,
            updates: 1_000_000,
            control: 3_000,
            params_ac: params_preset("default"),
            params_bd: params_preset("fast"),
        };
        let mut argv = std::env::args().skip(1).peekable();
        while let Some(arg) = argv.next() {
            if arg == "--quick" {
                a.owners = 500;
                a.cap = 100;
                a.updates = 50_000;
                a.control = 300;
            } else if let Some(v) = arg.strip_prefix("--only=") {
                a.only = v.split(',').map(|s| s.trim().to_string()).collect();
            } else if let Some(v) = arg.strip_prefix("--owners=") {
                a.owners = v.parse().expect("--owners 必须是整数");
            } else if let Some(v) = arg.strip_prefix("--cap=") {
                a.cap = v.parse().expect("--cap 必须是整数");
            } else if let Some(v) = arg.strip_prefix("--updates=") {
                a.updates = v.parse().expect("--updates 必须是整数");
            } else if let Some(v) = arg.strip_prefix("--control=") {
                a.control = v.parse().expect("--control 必须是整数");
            } else if let Some(v) = arg.strip_prefix("--params-ac=") {
                a.params_ac = params_preset(v);
            } else if let Some(v) = arg.strip_prefix("--params-bd=") {
                a.params_bd = params_preset(v);
            } else {
                panic!("未知参数 {arg}");
            }
        }
        a
    }

    fn wants(&self, name: &str) -> bool {
        self.only.iter().any(|s| s == name)
    }
}

// ---------- 场景 A：10000 个不同 owner，max_views = 1000 ----------

struct Outcome {
    name: &'static str,
    ops: u64,
    elapsed: Duration,
    extra: String,
}

fn scenario_a(args: &Args) -> Outcome {
    const TARGET_COUNT: u32 = 100;

    println!("\n──────── 场景 A：{:} 个不同 owner，max_views = {} ────────", args.owners, args.cap);
    let mut e = Engine::with_seed(ecfg(args.params_ac, args.cap), 7);
    let targets: Vec<AccountIdx> = (0..TARGET_COUNT)
        .map(|i| e.intern(key(TARGET_BASE + i)))
        .collect();
    let owners: Vec<AccountIdx> = (0..args.owners)
        .map(|i| e.intern(key(OWNER_BASE + i)))
        .collect();
    // 每个 owner 都先打正分：现实里「我会去请求观点的来源」就是正分清单里的人，
    // 而且只有正分清单里的条目才记连败（on_view_failed 对未评分账户是 no-op，
    // 见 tm_store::PositiveList::record_failure 的既有语义）。这样才验得到
    // 「被拒 = 视同失败 = 下次 tick 退避它」这条链路。同时顺带压测正分清单本身。
    for &o in &owners {
        e.rate(o, 1.0, 0);
    }
    let rss0 = rss_bytes();
    let heap0 = 0;

    let t0 = Instant::now();
    let mut accepted = 0usize;
    for (i, &owner) in owners.iter().enumerate() {
        // 每份观点的内容都不同（不同 target 组合），否则的「不同 owner」太假
        let mut pairs: Vec<(AccountKey, f32)> = Vec::with_capacity(4);
        pairs.push((key(TARGET_BASE + (i as u32 % TARGET_COUNT)), 1.0));
        pairs.push((key(TARGET_BASE + ((i as u32 + 7) % TARGET_COUNT)), 0.5));
        pairs.push((key(TARGET_BASE + ((i as u32 + 13) % TARGET_COUNT)), 0.25));
        let v = CompressedView::from_pairs(args.params_ac, &pairs);
        if e.stats().2 < args.cap {
            accepted += 1;
        }
        e.on_view(owner, v, i as u64, i as u64);
    }
    let elapsed = t0.elapsed();
    let rss1 = rss_bytes();

    // —— 断言 ——
    let views = e.stats().2;
    let expected = (args.owners as usize).min(args.cap);
    println!(
        "  喂入 {} 份观点，耗时 {elapsed:?}（{:.2} µs/次）",
        args.owners,
        elapsed.as_secs_f64() * 1e6 / args.owners as f64
    );
    println!("  缓存条数 = {views}（上限 {}）", args.cap);
    println!(
        "  rejected_count = {}，细分 = {:?}",
        e.rejected_count(),
        e.rejected_breakdown()
    );
    println!("  实际被接受的条数（喂入时观察到满） = {accepted}");
    assert_eq!(views, expected, "缓存条数必须取 min(喂入数, max_views)");
    assert!(
        views <= args.cap,
        "views.len() 不得超过 max_views：{views} > {}",
        args.cap
    );
    let overflow = (args.owners as usize).saturating_sub(args.cap) as u64;
    assert_eq!(
        e.rejected_count(),
        overflow,
        "超出上限的每一个新 owner 都必须被拒"
    );
    assert_eq!(
        e.rejected_breakdown(),
        (overflow, 0),
        "场景 A 不该出现非有限丢弃"
    );
    assert_eq!(
        e.stats().1,
        expected as u64,
        "只有被接受的视图才计命中（丢弃的不得计命中）"
    );

    // 被拒的 owner：不进缓存、连败计数至少 1（只有真的溢出时才检查）
    if overflow > 0 {
        let last = *owners.last().expect("owners 非空");
        assert_eq!(
            e.view_age(last, args.owners as u64),
            None,
            "末个被拒的 owner 的观点不得进缓存"
        );
        assert!(
            e.fail_streak(last) >= 1,
            "被拒的 owner 必须按失败语义累计连败（下次 tick 会退避它）"
        );
    }
    // 被接受的 owner：都在缓存里
    let kept_last = owners[expected - 1];
    assert!(
        e.view_age(kept_last, args.owners as u64).is_some(),
        "上限内的 owner 必须进缓存"
    );

    // 容量可复用：清空之后还能再收满一批
    let pruned = e.prune_stale(args.owners as u64, 0);
    assert_eq!(pruned, args.cap);
    for &o in owners.iter().rev().take(args.cap) {
        e.on_view(o, view_for(args.params_ac, &targets, 0.5), 0, args.owners as u64);
    }
    assert_eq!(e.stats().2, expected, "清空之后必须能重新收满");
    let reused_rejected = e.rejected_count();
    println!("  prune_stale 之后重新收满：缓存 = {}，累计丢弃 = {reused_rejected}", e.stats().2);

    // 引擎能用：信任分布非空、全部有限
    let dist = e.trust_distribution(args.owners as u64);
    assert!(!dist.is_empty(), "被容量保护之后引擎仍必须产出分布");
    assert!(dist.iter().all(|(_, v)| v.is_finite()), "分布不得含非有限值");

    let est = estimate_views_bytes(args.cap, args.params_ac);
    report_mem(&format!("场景 A（{} 次喂入后）", args.owners), rss0, rss1, est);
    if let (Some(a), Some(b)) = (rss0, rss1) {
        let delta = b.saturating_sub(a);
        // 内存只随「缓存条数 × 视图大小」增长，与喂入次数无关
        let bound = est + 64 * 1024 * 1024;
        assert!(
            delta <= bound,
            "内存必须随 max_views 有界：Δ {:.2} MiB > 上界 {:.2} MiB",
            mib(delta),
            mib(bound)
        );
    }
    let _ = heap0;

    Outcome {
        name: "A 容量上限",
        ops: args.owners as u64,
        elapsed,
        extra: format!(
            "缓存 {} 条、丢弃 {} 次、内存估算 {:.1} MiB",
            e.stats().2,
            e.rejected_count(),
            mib(est)
        ),
    }
}

/// 对照组：max_views = 0（旧行为）喂少量 owner，证明「不设上限确实无界」，
/// 并据此线性外推到 --owners 的规模。
fn scenario_a_control(args: &Args) {
    if args.control == 0 || !args.wants("a") {
        return;
    }
    println!("\n──────── 场景 A 对照：max_views = 0（旧行为），喂 {} 个 owner ────────", args.control);
    let mut e = Engine::with_seed(ecfg(args.params_ac, 0), 7);
    for i in 0..100u32 {
        e.intern(key(TARGET_BASE + i));
    }
    let rss0 = rss_bytes();
    let t0 = Instant::now();
    for i in 0..args.control {
        let owner = e.intern(key(OWNER_BASE + i));
        e.on_view(owner, view_for(args.params_ac, &[], 1.0), i as u64, i as u64);
    }
    let elapsed = t0.elapsed();
    let rss1 = rss_bytes();
    let views = e.stats().2;
    assert_eq!(views, args.control as usize, "max_views=0 时应当全部进缓存（旧行为）");
    assert_eq!(e.rejected_count(), 0, "max_views=0 时不得有任何丢弃");
    let per_owner = estimate_views_bytes(1, args.params_ac);
    let projected = estimate_views_bytes(args.owners as usize, args.params_ac);
    println!(
        "  喂入 {} 份 -> 缓存 {} 条，耗时 {elapsed:?}；线性外推到 {} 个 owner：约 {:.1} MiB",
        args.control,
        views,
        args.owners,
        mib(projected)
    );
    if let (Some(a), Some(b)) = (rss0, rss1) {
        let delta = b.saturating_sub(a);
        let per = delta / args.control.max(1) as u64;
        println!(
            "  [内存] RSS 实测 Δ {:.2} MiB（{:.1} KiB/owner，估 {} KiB/owner）",
            mib(delta),
            per as f64 / 1024.0,
            per_owner as f64 / 1024.0
        );
        // 旧行为下内存确实随喂入线性增长：外推到 --owners 应当显著超过有界场景
        let projected_rss = per * args.owners as u64;
        if projected_rss > estimate_views_bytes(args.cap, args.params_ac) {
            println!(
                "  => 同样喂 {} 个，旧行为约 {:.1} MiB，有界场景约 {:.1} MiB：差 {:.0} 倍",
                args.owners,
                mib(projected_rss),
                mib(estimate_views_bytes(args.cap, args.params_ac)),
                projected_rss as f64 / estimate_views_bytes(args.cap, args.params_ac) as f64
            );
        }
    }
}

// ---------- 场景 B：同一 owner 更新 100 万次 ----------

fn scenario_b(args: &Args) -> Outcome {
    const TARGET_COUNT: u32 = 100;
    println!("\n──────── 场景 B：同一 owner 更新 {} 次 ────────", args.updates);
    let mut e = Engine::with_seed(ecfg(args.params_bd, 1), 11);
    let targets: Vec<AccountIdx> = (0..TARGET_COUNT)
        .map(|i| e.intern(key(TARGET_BASE + i)))
        .collect();
    let owner = e.intern(key(OWNER_BASE));
    e.rate(owner, 1.0, 0);

    // 8 份内容略有不同的观点轮流刷新（克隆一份 40 KiB 的成本远低于噪声底）
    let variants: Vec<CompressedView> = (0..8u32)
        .map(|i| {
            let mut pairs: Vec<(AccountKey, f32)> = Vec::with_capacity(4);
            pairs.push((key(TARGET_BASE + i % TARGET_COUNT), 1.0));
            pairs.push((key(TARGET_BASE + (i + 1) % TARGET_COUNT), 0.5));
            pairs.push((key(TARGET_BASE + (i + 2) % TARGET_COUNT), 0.25));
            CompressedView::from_pairs(args.params_bd, &pairs)
        })
        .collect();

    let rss0 = rss_bytes();
    let t0 = Instant::now();
    let mut failures = 0u64;
    for i in 0..args.updates {
        let now = i;
        // 每 10000 次先制造一次失败，下一次成功必须把连败清零
        if i % 10_000 == 9_999 {
            e.on_view_failed(owner, now);
            failures += 1;
        }
        let v = variants[(i % 8) as usize].clone();
        e.on_view(owner, v, i / 1000, now);
        if i % 100_000 == 0 {
            assert_eq!(e.stats().2, 1, "更新既有条目不得增长缓存（第 {i} 次）");
            assert_eq!(e.rejected_count(), 0, "更新既有条目永远不该被拒（第 {i} 次）");
        }
    }
    let elapsed = t0.elapsed();
    let rss1 = rss_bytes();

    println!(
        "  耗时 {elapsed:?}（{:.2} µs/次），省略率 = {}",
        elapsed.as_secs_f64() * 1e6 / args.updates as f64,
        failures
    );
    println!("  缓存条数 = {}，命中 = {}，丢弃 = {}", e.stats().2, e.stats().1, e.rejected_count());
    assert_eq!(e.stats().2, 1, "100 万次更新之后仍然只有 1 条缓存");
    assert_eq!(e.stats().1, args.updates, "每一次更新都必须计命中");
    assert_eq!(e.rejected_count(), 0, "更新既有条目永远不该被拒");
    assert_eq!(e.fail_streak(owner), 0, "最后一次成功刷新必须清零连败");
    assert_eq!(e.view_age(owner, args.updates - 1), Some(0), "last_check 必须是最后一次");
    let q = e.query(targets[0], args.updates - 1);
    assert!(
        q.is_finite() && (0.0..=1.0).contains(&q),
        "查询必须有限且在 [0,1]：{q}"
    );
    let dist = e.trust_distribution(args.updates - 1);
    assert!(dist.iter().all(|(_, v)| v.is_finite()));

    let est = estimate_views_bytes(1, args.params_bd);
    report_mem(&format!("场景 B（{} 次更新后）", args.updates), rss0, rss1, est);
    if let (Some(a), Some(b)) = (rss0, rss1) {
        let delta = b.saturating_sub(a);
        assert!(
            delta <= 64 * 1024 * 1024,
            "更新路径的内存必须恒定：Δ {:.2} MiB",
            mib(delta)
        );
    }

    Outcome {
        name: "B 更新路径",
        ops: args.updates,
        elapsed,
        extra: format!("缓存恒 1 条、内存估算 {:.1} KiB", mib(est) * 1024.0),
    }
}

// ---------- 场景 C：NaN / inf 恶意视图 ----------

fn scenario_c(args: &Args) -> Outcome {
    const TARGET_COUNT: u32 = 32;
    const OWNER_COUNT: u32 = 64;
    println!("\n──────── 场景 C：NaN / +inf 恶意视图 ────────");
    let mut dirty = Engine::with_seed(ecfg(args.params_ac, OWNER_COUNT as usize), 13);
    let mut clean = Engine::with_seed(ecfg(args.params_ac, OWNER_COUNT as usize), 13);

    let mut targets = Vec::new();
    for i in 0..TARGET_COUNT {
        let a = dirty.intern(key(TARGET_BASE + i));
        let b = clean.intern(key(TARGET_BASE + i));
        assert_eq!(a, b, "两台引擎的编号必须一一对应");
        targets.push(a);
    }
    let owners: Vec<AccountIdx> = (0..OWNER_COUNT)
        .map(|i| {
            let a = dirty.intern(key(OWNER_BASE + i));
            let b = clean.intern(key(OWNER_BASE + i));
            assert_eq!(a, b);
            a
        })
        .collect();
    // 额外登记 64 个 target key：让质量扫描扫到「无碰撞」的账户（读数恰好是 0，
    // 而不是别的 target 的碰撞泄漏）。正常路径上这类账户每天都会大量出现，
    // 这里只是把「脏值必须被逐点检测、而不是被碰撞掩盖」这件事钉死。
    for i in 0..32u32 {
        let a = dirty.intern(key(TARGET_BASE + 100 + i));
        let b = clean.intern(key(TARGET_BASE + 100 + i));
        assert_eq!(a, b);
    }
    for &o in &owners {
        dirty.rate(o, 1.0, 0);
        clean.rate(o, 1.0, 0);
    }

    // 先给两边喂完全一样的一批干净观点，得到可以逐位比较的基准。
    let t0 = Instant::now();
    for (i, &o) in owners.iter().enumerate() {
        let v = view_for(args.params_ac, &targets[..8], 0.5);
        dirty.on_view(o, v.clone(), 0, i as u64);
        clean.on_view(o, v, 0, i as u64);
    }
    let total_accounts = TARGET_COUNT + 32 + OWNER_COUNT;
    let baseline: Vec<(u32, u32)> = (0..total_accounts)
        .map(|i| (i, clean.query(i, 0).to_bits()))
        .collect();
    let baseline_ages: Vec<Option<u64>> = owners.iter().map(|&o| clean.view_age(o, 0)).collect();

    // 脏视图：一半 NaN、一半 +inf，既有「已缓存 owner 的更新」也有「新 owner」。
    let mut dirty_feeds = 0u64;
    // 被投毒的 key 放在 target 段的最后一个：质量扫描是升序的，会扫到它。
    // 它自身在视图里没有任何声明，所以脏桶会不会泄漏到它只取决于碰撞——
    // 若被漏检，读数就会从 0 变成 1.0，逐位比较立刻抓住。
    let poisoned = key(TARGET_BASE + TARGET_COUNT - 1);
    for i in 0..200u32 {
        let now = 1000 + i as u64;
        let view = if i % 2 == 0 {
            nan_view(args.params_ac, poisoned)
        } else {
            inf_view(args.params_ac, poisoned)
        };
        if i % 4 == 0 {
            // 新 owner（第 64.. 号）：本应进缓存，必须被拒。
            // **刻意不 rate**：给一个没有观点的来源打分会改变群体项的分母（
            // 弃权票），那是 query 的正常语义，会干扰本场景「脏视图不得留下
            // 任何痕迹」的逐位比较。这里只登记编号、不评分。
            let new_owner = dirty.intern(key(OWNER_BASE + OWNER_COUNT + i / 4));
            dirty.on_view(new_owner, view, 0, now);
        } else {
            // 已缓存 owner 的刷新：必须被拒且不得刷新 last_check
            let o = owners[(i as usize) % owners.len()];
            dirty.on_view(o, view, 0, now);
        }
        dirty_feeds += 1;
    }
    let elapsed = t0.elapsed();

    let (cap_rej, nonfinite_rej) = dirty.rejected_breakdown();
    // 第 0..64 号 owner 已经在缓存里（缓存恰好满），后面的新 owner 会被**容量**
    // 先挡下（那条判定在质量扫描之前），所以它们计入 capacity 而不是 nonfinite。
    // 这正是设计意图：脏视图不会给缓存制造新条目，也就谈不上「被它的读数污染」。
    let expected_capacity = dirty_feeds / 4; // i % 4 == 0 的那 1/4 是新 owner
    println!(
        "  脏视图喂入 {dirty_feeds} 次：容量丢弃 {cap_rej} 次（其中 {:.0} 为新增 owner 撞上限）、非有限丢弃 {nonfinite_rej} 次",
        expected_capacity
    );
    println!("  缓存条数 = {}（与干净引擎相同）", dirty.stats().2);
    assert_eq!(
        cap_rej,
        expected_capacity,
        "新增 owner 的脏视图必须先撞容量上限（那条判定在噪声底与质量扫描之前）"
    );
    assert_eq!(
        nonfinite_rej,
        dirty_feeds - expected_capacity,
        "已缓存 owner 的每一次 NaN/inf 刷新都必须按非有限丢弃"
    );
    assert_eq!(dirty.rejected_count(), dirty_feeds);
    assert_eq!(dirty.stats().2, clean.stats().2, "脏视图不得改变缓存条数");
    assert_eq!(dirty.stats().1, clean.stats().1, "脏视图不得计命中");

    // 脏视图不得刷新 last_check：与干净引擎逐条比较
    for (i, &o) in owners.iter().enumerate() {
        assert_eq!(
            dirty.view_age(o, 0),
            baseline_ages[i],
            "第 {i} 条既有观点的 last_check 被脏视图改动了"
        );
    }
    // 引擎状态一点没被污染：逐位比较所有查询结果
    let after: Vec<(u32, u32)> = (0..total_accounts)
        .map(|i| (i, dirty.query(i, 0).to_bits()))
        .collect();
    assert_eq!(after, baseline, "脏视图除了丢弃计数之外不得留下任何痕迹");

    // 丢弃之后继续喂干净观点：两台引擎仍然逐位一致
    for (i, &o) in owners.iter().enumerate() {
        let v = view_for(args.params_ac, &targets[..4], 0.25);
        dirty.on_view(o, v.clone(), 0, 5000 + i as u64);
        clean.on_view(o, v, 0, 5000 + i as u64);
    }
    let final_dirty: Vec<(u32, u32)> = (0..total_accounts)
        .map(|i| (i, dirty.query(i, 5000).to_bits()))
        .collect();
    let final_clean: Vec<(u32, u32)> = (0..total_accounts)
        .map(|i| (i, clean.query(i, 5000).to_bits()))
        .collect();
    assert_eq!(final_dirty, final_clean, "脏视图之后引擎必须与从未见过脏视图时逐位相同");
    assert!(
        dirty.rejected_count() == dirty_feeds,
        "后续的正常观点不得被误杀（丢弃计数只能停在 {dirty_feeds}）"
    );
    let dist = dirty.trust_distribution(5000);
    assert!(!dist.is_empty() && dist.iter().all(|(_, v)| v.is_finite()));

    // 顺带钉住「+inf 会被 clamp 吞掉」这件事：不查原始值就检测不出来
    let probe = inf_view(args.params_ac, poisoned);
    assert!(
        !probe.query(&poisoned).is_finite(),
        "inf 视图的 query 必须是非有限值"
    );

    let est = estimate_views_bytes(OWNER_COUNT as usize, args.params_ac);
    report_mem("场景 C", None, None, est);

    Outcome {
        name: "C 恶意视图",
        ops: dirty_feeds,
        elapsed,
        extra: format!("丢弃 {dirty_feeds} 次、缓存未变、查询逐位一致"),
    }
}

// ---------- 场景 D：tick + on_view 混合 100 万次 ----------

fn scenario_d(args: &Args) -> Outcome {
    // 账户数取「上限的两倍」，保证容量上限**真的会被顶满**：否则 300 个账户配
    // max_views=1000 的缓存永远装不满，这个场景就只测了热路径、没测上限本身。
    let account_count = ((args.cap.saturating_mul(2)).max(300)) as u32;
    println!(
        "\n──────── 场景 D：tick + on_view 混合 {} 次（{} 个账户，max_views = {}） ────────",
        args.updates, account_count, args.cap
    );
    let mut e = Engine::with_seed(ecfg(args.params_bd, args.cap), 17);
    let accounts: Vec<AccountIdx> = (0..account_count)
        .map(|i| e.intern(key(OWNER_BASE + i)))
        .collect();
    for (i, &a) in accounts.iter().enumerate() {
        e.rate(a, 1.0 + (i % 5) as f32, 0);
    }
    let variants: Vec<CompressedView> = (0..4u32)
        .map(|i| {
            let mut pairs: Vec<(AccountKey, f32)> = Vec::with_capacity(8);
            for j in 0..8u32 {
                pairs.push((key(OWNER_BASE + (i * 7 + j) % account_count), 0.5));
            }
            CompressedView::from_pairs(args.params_bd, &pairs)
        })
        .collect();

    let rss0 = rss_bytes();
    let t0 = Instant::now();
    let mut seed = 0x1234_5678_9ABC_DEF1u64;
    let mut rejected_before = 0u64;
    let mut max_views_seen = 0usize;
    let mut ticks = 0u64;
    let mut probes = 0u64;
    let mut views_fed = 0u64;
    let mut fails = 0u64;
    let mut prunes = 0u64;
    for i in 0..args.updates {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let now = i / 100; // 每 100 次推进一个时间刻度
        match (seed >> 33) % 10 {
            0..=2 => {
                let _ = e.tick(now, 8);
                ticks += 1;
            }
            3..=7 => {
                let owner = accounts[((seed >> 20) as usize) % accounts.len()];
                let v = variants[((seed >> 8) as usize) % variants.len()].clone();
                e.on_view(owner, v, now, now);
                views_fed += 1;
            }
            8 => {
                let owner = accounts[((seed >> 20) as usize) % accounts.len()];
                e.on_view_failed(owner, now);
                fails += 1;
            }
            _ => {
                if i % 16 == 0 {
                    let _ = e.probe(now);
                    probes += 1;
                } else {
                    let _ = e.prune_stale(now, 200);
                    prunes += 1;
                }
            }
        }
        let views_now = e.stats().2;
        max_views_seen = max_views_seen.max(views_now);
        assert!(
            views_now <= args.cap,
            "第 {i} 次：views.len() = {views_now} 超过上限 {}",
            args.cap
        );
        let rejected_now = e.rejected_count();
        assert!(rejected_now >= rejected_before, "第 {i} 次：丢弃计数不得回退");
        rejected_before = rejected_now;
        if i % 10_000 == 0 {
            let q = e.query(accounts[(i as usize / 10_000) % accounts.len()], now);
            assert!(q.is_finite() && (0.0..=1.0).contains(&q), "第 {i} 次：查询异常 {q}");
            let (req, hits, _, rated) = e.stats();
            // 每次 tick 最多讨 k=8 个，每次 probe 最多讨一整轮（<= 账户数），
            // 两者都是**累计**计数，所以上界是两类调用各自次数乘上自己的单次上限。
            assert!(
                req <= ticks * 8 + probes * account_count as u64,
                "第 {i} 次：请求数异常 {req}（tick {ticks}、probe {probes}）"
            );
            assert!(hits <= views_fed, "第 {i} 次：命中数超过喂入数");
            assert!(rated <= account_count as usize);
        }
    }
    let elapsed = t0.elapsed();
    let rss1 = rss_bytes();

    println!(
        "  耗时 {elapsed:?}（{:.2} µs/次），tick {ticks} 次、probe {probes} 次、on_view {views_fed} 次、on_view_failed {fails} 次、prune {prunes} 次",
        elapsed.as_secs_f64() * 1e6 / args.updates as f64
    );
    println!(
        "  缓存条数 = {}（峰值 {max_views_seen}，上限 {}），丢弃 = {:?}，命中 = {}",
        e.stats().2,
        args.cap,
        e.rejected_breakdown(),
        e.stats().1
    );
    assert!(max_views_seen <= args.cap);
    assert!(
        max_views_seen >= args.cap,
        "本场景必须真的把容量顶满（峰值 {max_views_seen} < 上限 {}），否则上限本身没被测到",
        args.cap
    );
    assert!(
        e.rejected_count() > 0,
        "容量顶满之后必然有观点被拒，丢弃计数应当大于 0"
    );
    let dist = e.trust_distribution(args.updates / 100);
    assert!(dist.iter().all(|(_, v)| v.is_finite()), "混合压测之后分布必须有限");
    let est = estimate_views_bytes(max_views_seen, args.params_bd);
    report_mem("场景 D", rss0, rss1, est);
    if let (Some(a), Some(b)) = (rss0, rss1) {
        let delta = b.saturating_sub(a);
        assert!(
            delta <= est + 64 * 1024 * 1024,
            "混合压测的内存必须随上限有界：Δ {:.2} MiB",
            mib(delta)
        );
    }

    Outcome {
        name: "D 混合压测",
        ops: args.updates,
        elapsed,
        extra: format!(
            "峰值缓存 {max_views_seen}、丢弃 {:?}",
            e.rejected_breakdown()
        ),
    }
}

// ---------- bench：单次 on_view 的耗时 ----------

fn scenario_bench() -> Outcome {
    println!("\n──────── bench：单次 on_view 的真实耗时（决定 B/D 用哪种形状参数） ────────");
    let mut results = Vec::new();
    for (tag, params) in [
        ("default 2048x5（40 KiB/视图）", params_preset("default")),
        ("fast 64x1（256 B/视图）", params_preset("fast")),
    ] {
        let mut e = Engine::with_seed(ecfg(params, 0), 5);
        let targets: Vec<AccountIdx> = (0..100u32)
            .map(|i| e.intern(key(TARGET_BASE + i)))
            .collect();
        let owner = e.intern(key(OWNER_BASE));
        e.rate(owner, 1.0, 0);
        let v = view_for(params, &targets, 0.5);
        for _ in 0..10 {
            e.on_view(owner, v.clone(), 0, 0);
        }
        let t0 = Instant::now();
        const REPS: u32 = 200;
        for _ in 0..REPS {
            e.on_view(owner, v.clone(), 0, 0);
        }
        let dt = t0.elapsed();
        let us = dt.as_secs_f64() * 1e6 / REPS as f64;
        println!("  {tag}: {us:.2} µs/次（{REPS} 次平均）");
        results.push((tag, us));
    }
    let slow = results[0].1;
    println!(
        "  => 100 万次 on_view 在默认参数下约 {:.0} 秒，fast 参数下约 {:.0} 秒；",
        slow * 1_000_000.0 / 1e6,
        results[1].1 * 1_000_000.0 / 1e6
    );
    println!("     这就是场景 B/D 用 --params-bd=fast 的原因：省下的是噪声底，不是行为。");
    Outcome {
        name: "bench",
        ops: 200,
        elapsed: Duration::ZERO,
        extra: format!("default {:.1} µs、fast {:.1} µs", results[0].1, results[1].1),
    }
}

// ---------- main ----------

fn main() {
    let args = Args::parse();
    println!("═══ P2 安全加固压力测试 ═══");
    println!(
        "参数：owners={} cap={} updates={} control={} params_ac={:?} params_bd={:?}",
        args.owners, args.cap, args.updates, args.control, args.params_ac, args.params_bd
    );
    match rss_bytes() {
        Some(b) => println!("RSS 可读：当前 {:.2} MiB", mib(b)),
        None => println!("RSS 不可读：内存数字将退化为估算"),
    }

    let mut outcomes = Vec::new();
    if args.wants("bench") {
        outcomes.push(scenario_bench());
    }
    if args.wants("a") {
        outcomes.push(scenario_a(&args));
        scenario_a_control(&args);
    }
    if args.wants("b") {
        outcomes.push(scenario_b(&args));
    }
    if args.wants("c") {
        outcomes.push(scenario_c(&args));
    }
    if args.wants("d") {
        outcomes.push(scenario_d(&args));
    }

    println!("\n═══ 汇总 ═══");
    println!(
        "{:<16} {:>12} {:>14} {:>16} {}",
        "场景", "操作数", "耗时", "吞吐", "备注"
    );
    for o in &outcomes {
        let throughput = if o.elapsed.as_secs_f64() > 0.0 {
            o.ops as f64 / o.elapsed.as_secs_f64()
        } else {
            0.0
        };
        println!(
            "{:<16} {:>12} {:>14} {:>16} {}",
            o.name,
            o.ops,
            format!(
                "{:.3} s",
                o.elapsed.as_secs_f64()
            ),
            format!("{throughput:.0} ops/s"),
            o.extra
        );
    }
    println!("\n全部断言通过：A/B/C/D 四个场景在真实运行中均未 panic、未超界。");
}
