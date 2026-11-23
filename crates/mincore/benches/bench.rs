//! # tm_mincore 性能基准（零外部依赖，手写 main）
//!
//! 本仓库有「不引外部 crate」的纪律，所以这里**不用** criterion / divan / iai：
//! crates/mincore/Cargo.toml 里是
//!
//!     [[bench]]
//!     name = "bench"
//!     path = "benches/bench.rs"
//!     harness = false
//!
//! ## 采样方法（不糊弄）
//!
//! * 每个用例先跑 WARM 次预热，整段丢弃；
//! * 之后取 DEFAULT_SAMPLES 个样本（--samples N 可改），报 mean ± std
//!   （样本标准差，n−1）与 median；
//! * 每个样本内部自动循环 iters 次，使单样本 ≥ 200 µs：Windows 的 Instant 走
//!   QPC（量化约 100 ns），单次 q_of（几十 ns）不放大根本测不准；
//!   循环次数由一次校准调用现算，用例之间不需要手调；
//! * 被测结果、以及输入侧的账户号，全部过 std::hint::black_box，
//!   防止常量折叠把整段删掉。
//!
//! ## 用法
//!
//!     cd crates/mincore
//!     cargo bench --bench bench -- all
//!     cargo bench --bench bench -- view qof publish onview dist query engine
//!     cargo bench --bench bench -- all --samples 50
//!
//! （`harness = false` 的 bench 目标只能用 `cargo bench` 启动；
//! `[profile.bench]` 默认继承 `[profile.release]` 的 opt-level=3 + lto，
//! 所以测出来的就是 release 数字。）
//!
//! 分组编号：view(§1) qof(§2) publish(§3) onview(§4) dist(§5) query(§6) engine(§7)。
//! §3/§5/§6 共用一次状态构造（publish 内部就是 distribution，分开建节点会白烧
//! 一遍），耗时算在一起。
//!
//! 输出是可以直接贴进 Markdown 报告的表格；配套报告见 docs/audit/mincore性能基准.md。

use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use tm_engine::view::{AccountKey, CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};
use tm_mincore::{Acc, Config as CoreConfig, Node, SparseView, SLOT_BYTES, VIEW_OVERHEAD};
use tm_sim::{AttackKind, Kind, SimConfig, World};

// ══════════════════════════════ 采样与统计 ══════════════════════════════

/// 预热次数：每个用例先跑这么多遍，整段丢弃。
const WARM: usize = 3;
/// 默认样本数（验收要求 ≥ 20）。
const DEFAULT_SAMPLES: usize = 20;
/// 单样本目标时长：低于它就自动增加迭代数，抑制 Instant 的量化噪声。
const TARGET_SAMPLE_SECS: f64 = 200e-6;
/// 迭代数上限：极快用例（几十 ns）不至于把单样本拖到秒级。
const MAX_ITERS: usize = 20_000;
/// 「陌生人」账户号：远在任何合成用例的账户号范围之外，保证 q_of 全部未命中。
const STRANGER: Acc = 0xDEAD_BEEF;

/// 一组样本的统计量（单位：秒）。
#[derive(Clone, Copy)]
struct Stats {
    n: usize,
    mean: f64,
    std: f64,
    median: f64,
    min: f64,
    max: f64,
}

impl Stats {
    fn from_samples(mut xs: Vec<f64>) -> Self {
        if xs.is_empty() {
            return Self {
                n: 0,
                mean: 0.0,
                std: 0.0,
                median: 0.0,
                min: 0.0,
                max: 0.0,
            };
        }
        xs.sort_by(|a, b| a.total_cmp(b));
        let n = xs.len();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let std = if n < 2 {
            0.0
        } else {
            let ss: f64 = xs.iter().map(|x| (x - mean) * (x - mean)).sum();
            (ss / (n - 1) as f64).sqrt()
        };
        let median = if n % 2 == 1 {
            xs[n / 2]
        } else {
            (xs[n / 2 - 1] + xs[n / 2]) / 2.0
        };
        Self {
            n,
            mean,
            std,
            median,
            min: xs[0],
            max: xs[n - 1],
        }
    }
}

/// 表格的显示单位：整张表统一单位，避免同一列混 ns / µs / ms。
#[derive(Clone, Copy)]
struct Scale {
    unit: &'static str,
    factor: f64,
    prec: usize,
}

impl Scale {
    fn auto(max_secs: f64) -> Self {
        if max_secs < 1e-6 {
            Self {
                unit: "ns",
                factor: 1e9,
                prec: 1,
            }
        } else if max_secs < 1e-3 {
            Self {
                unit: "µs",
                factor: 1e6,
                prec: 2,
            }
        } else if max_secs < 1.0 {
            Self {
                unit: "ms",
                factor: 1e3,
                prec: 2,
            }
        } else {
            Self {
                unit: "ms",
                factor: 1e3,
                prec: 0,
            }
        }
    }

    fn d(&self, secs: f64) -> String {
        format!("{:.*}", self.prec, secs * self.factor)
    }
}

/// mean ± std 单元格。
fn cell(sc: Scale, st: &Stats) -> String {
    format!("{}±{}", sc.d(st.mean), sc.d(st.std))
}

/// 单个耗时值按量级自选单位（3 位有效数字）。
///
/// 对照表的两侧常差 3~4 个数量级，共用一个单位会让小值全部塌成 0.00，
/// 所以 `print_cmp` / `print_query` 的每一格都自带单位。
fn a(secs: f64) -> String {
    let v = secs.max(0.0);
    if v < 1e-6 {
        format!("{:.1}ns", v * 1e9)
    } else if v < 1e-3 {
        format!("{:.2}µs", v * 1e6)
    } else if v < 1.0 {
        format!("{:.2}ms", v * 1e3)
    } else {
        format!("{:.2}s", v)
    }
}

/// mean ± std，每格自带单位。
fn cell_auto(st: &Stats) -> String {
    format!("{}±{}", a(st.mean), a(st.std))
}

fn max_mean(stats: &[Stats]) -> f64 {
    stats.iter().map(|s| s.mean).fold(0.0, f64::max)
}

/// 跑一个用例：预热 WARM 次 → 一次校准 → 取 samples 个样本。
///
/// 每个样本执行 iters 次 f，返回**单次调用**的耗时统计（秒）。
/// f 必须自己把结果送进 black_box，否则优化器可能把整个调用删掉。
fn bench<F: FnMut()>(samples: usize, mut f: F) -> Stats {
    for _ in 0..WARM {
        f();
    }
    let t = Instant::now();
    f();
    let single = t.elapsed().as_secs_f64().max(1e-9);
    let want = (TARGET_SAMPLE_SECS / single) as usize;
    let iters = want.clamp(1, MAX_ITERS);
    let mut xs = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        xs.push(t.elapsed().as_secs_f64() / iters as f64);
    }
    Stats::from_samples(xs)
}

/// Engine 与 mincore 对照表的一行。
struct CmpRow {
    label: String,
    eng: Stats,
    core: Stats,
}

/// min~max 与样本数：只在有波动时列出来，用得上 Stats 的全部字段。
fn fmt_span(st: &Stats) -> String {
    format!("{}~{}（n={}）", a(st.min), a(st.max), st.n)
}

fn print_cmp(rows: &[CmpRow]) {
    // 每格自带单位（Engine 与 mincore 常差 3~4 个数量级，共用单位会把小值全塌成 0.00）。
    println!(
        "| 用例 | Engine mean±std | Engine median | mincore mean±std | mincore median | 对比 |"
    );
    println!("|---|---|---|---|---|---|");
    for r in rows {
        let ratio = r.eng.mean / r.core.mean.max(1e-12);
        let verdict = if ratio >= 1.0 {
            format!("mincore 快 {ratio:.2}×")
        } else {
            format!("mincore 慢 {:.2}×", 1.0 / ratio)
        };
        println!(
            "| {} | {} | {} | {} | {} | {verdict} |",
            r.label,
            cell_auto(&r.eng),
            a(r.eng.median),
            cell_auto(&r.core),
            a(r.core.median),
        );
    }
    // 波动超过 10% 的行单独列 min~max：表格里只有 mean±std 时容易看漏长尾。
    let noisy: Vec<String> = rows
        .iter()
        .filter(|r| r.eng.std > r.eng.mean * 0.1 || r.core.std > r.core.mean * 0.1)
        .map(|r| {
            format!(
                "{}：Engine {}，mincore {}",
                r.label,
                fmt_span(&r.eng),
                fmt_span(&r.core),
            )
        })
        .collect();
    if !noisy.is_empty() {
        println!("\n标准差超过均值 10% 的行（列出 min~max 与样本数，供判断是不是噪声）：\n");
        for s in noisy {
            println!("- {s}");
        }
    }
}

// ══════════════════════════════ 造表 / 造节点 ══════════════════════════════

/// 造一份含 len 条目的稀疏表：第 j 条的账户号 = first + j*stride，
/// 分数 = 0.1 + (j%7)*0.05（全部 > 0，保证 quantize 后不丢条）。
///
/// 只用 from_scores 造表，所以这里**不计时**。
fn make_view(cap: usize, len: usize, first: u32, stride: u32) -> SparseView {
    let pairs: Vec<(Acc, f32)> = (0..len)
        .map(|j| (first + j as u32 * stride, 0.1 + (j % 7) as f32 * 0.05))
        .collect();
    SparseView::from_scores(cap, &pairs)
}

/// 一个「观察者节点」的合成状态。
///
/// * list：观察者自己的正分清单长度（账户 0..list，全部 0.5）；
/// * views：收到的来源观点数（**必须 ≤ list**，on_view 只接受评过的来源）；
/// * cap：观点容量；
/// * slots：每份来源观点的条目数（≤ cap）；
/// * disjoint：true = 各来源覆盖**互不重叠**的账户号（最坏情形，条目并集
///   = views×slots）；false = 全体覆盖**同一批账户**（同一人口，条目并集
///   = slots），这是真实网络的形态。
#[derive(Clone, Copy)]
struct Case {
    list: usize,
    views: usize,
    cap: usize,
    slots: usize,
    disjoint: bool,
}

impl Case {
    const fn new(list: usize, views: usize, cap: usize, slots: usize, disjoint: bool) -> Self {
        Self {
            list,
            views,
            cap,
            slots,
            disjoint,
        }
    }

    /// 条目并集的上界：分配函数的真正驱动项。
    fn union_slots(&self) -> usize {
        if self.disjoint {
            self.views * self.slots
        } else {
            self.slots
        }
    }
}

fn build_node(case: &Case) -> Node {
    let mut node = Node::new(
        CoreConfig {
            cap: case.cap,
            ..CoreConfig::default()
        },
        0x5EED,
    );
    for i in 0..case.list {
        node.rate(i as Acc, 0.5, 0);
    }
    for v in 0..case.views {
        let (first, stride) = if case.disjoint {
            ((v * case.slots) as u32, 1u32)
        } else {
            (0u32, 7u32)
        };
        let view = make_view(case.cap, case.slots.min(case.cap), first, stride);
        node.on_view(v as Acc, view, 0, 0);
    }
    node
}

// ══════════════════════════════ §1 编解码 ══════════════════════════════

fn group_view(samples: usize) {
    println!("\n## 1. SparseView::encode / decode（cap × 填充度）\n");
    println!("定长布局：SLOT_BYTES=6、VIEW_OVERHEAD=2 ⇒ 字节数恒为 cap*6+2（与条目数无关）。");
    println!(
        "decode 的输入是 encode() 的产物（合法规范形式），因此计量的正是一个「解析 + 逐槽位校验 + 填充区扫描」的正向路径。\n"
    );

    struct Row {
        cap: usize,
        fill: &'static str,
        len: usize,
        bytes: usize,
        enc: Stats,
        dec: Stats,
    }

    let mut rows: Vec<Row> = Vec::new();
    for cap in [128usize, 256, 512, 4096] {
        for (fill, len) in [("满表", cap), ("半满", cap / 2), ("仅 1 条", 1usize)] {
            let view = make_view(cap, len, 0, 7);
            let bytes = view.encode();
            let nbytes = bytes.len();
            let enc = bench(samples, || {
                let v = black_box(&view).encode();
                black_box(&v);
            });
            let dec = bench(samples, || {
                let v = SparseView::decode(black_box(&bytes), cap);
                black_box(&v);
            });
            rows.push(Row {
                cap,
                fill,
                len,
                bytes: nbytes,
                enc,
                dec,
            });
        }
    }

    let all: Vec<Stats> = rows.iter().flat_map(|r| [r.enc, r.dec]).collect();
    let sc = Scale::auto(max_mean(&all));
    let unit = sc.unit;
    println!(
        "| cap | 填充 | 条目 | 字节 | encode mean±std [{unit}] | encode median | decode mean±std [{unit}] | decode median | encode MB/s | decode MB/s |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        let cap = r.cap;
        let fill = r.fill;
        let len = r.len;
        let bytes = r.bytes;
        let enc_mbs = bytes as f64 / (1e6 * r.enc.mean);
        let dec_mbs = bytes as f64 / (1e6 * r.dec.mean);
        println!(
            "| {cap} | {fill} | {len} | {bytes} | {} | {} | {} | {} | {enc_mbs:.1} | {dec_mbs:.1} |",
            cell(sc, &r.enc),
            sc.d(r.enc.median),
            cell(sc, &r.dec),
            sc.d(r.dec.median),
        );
    }
    println!("\n注：encode 每次都要新分配 cap*6+2 字节的 Vec，decode 每次都要新分配槽位区；");
    println!(
        "「仅 1 条」与「满表」的 decode 差异很小，因为 decode 除逐条解析外，还要把 cap*6 的填充区整段扫一遍查非零。"
    );
}

// ══════════════════════════════ §2 q_of ══════════════════════════════

fn group_qof(samples: usize) {
    println!("\n## 2. SparseView::q_of / normalized（二分查找）\n");
    println!(
        "满表（条目数 = cap），账户号 = j*7（j 从 0 起）。命中查询按 j 轮转（每次查不同槽位）；"
    );
    println!(
        "未命中查询查 STRANGER（0xDEADBEEF，必然不在表里）。normalized = q_of + 一次 f64 除法。\n"
    );

    struct Row {
        cap: usize,
        hit: Stats,
        miss: Stats,
        norm: Stats,
    }

    let mut rows: Vec<Row> = Vec::new();
    for cap in [128usize, 256, 512, 4096] {
        let view = make_view(cap, cap, 0, 7);
        let mut cursor = 0usize;
        let hit = bench(samples, || {
            cursor = (cursor + 1) % cap;
            let acc = (cursor * 7) as u32;
            black_box(view.q_of(black_box(acc)));
        });
        let miss = bench(samples, || {
            black_box(view.q_of(black_box(STRANGER)));
        });
        let mut cursor2 = 0usize;
        let norm = bench(samples, || {
            cursor2 = (cursor2 + 1) % cap;
            let acc = (cursor2 * 7) as u32;
            black_box(view.normalized(black_box(acc)));
        });
        rows.push(Row {
            cap,
            hit,
            miss,
            norm,
        });
    }

    let all: Vec<Stats> = rows.iter().flat_map(|r| [r.hit, r.miss, r.norm]).collect();
    let sc = Scale::auto(max_mean(&all));
    let unit = sc.unit;
    println!(
        "| cap | q_of 命中 mean±std [{unit}] | median | q_of 未命中 mean±std | median | normalized(命中) mean±std | median |"
    );
    println!("|---|---|---|---|---|---|---|");
    for r in &rows {
        let cap = r.cap;
        println!(
            "| {cap} | {} | {} | {} | {} | {} | {} |",
            cell(sc, &r.hit),
            sc.d(r.hit.median),
            cell(sc, &r.miss),
            sc.d(r.miss.median),
            cell(sc, &r.norm),
            sc.d(r.norm.median),
        );
    }
    println!(
        "\nq_of 是 O(log cap) 次比较，cap 从 128 到 4096 只多约 5 次比较；未命中与命中同价（都要二分到底）。"
    );
}

// ══════════════════════════════ §3/§5/§6 共用状态扫描 ══════════════════════════════

struct CaseStats {
    case: Case,
    dist: Stats,
    publish: Stats,
    q_hit: Stats,
    q_miss: Stats,
}

/// 一次性把 §3/§5/§6 要用的状态准备好并逐项计时：
/// distribution（§5）、publish（§3，内部含 distribution）、query（§6）。
fn sweep_compute(samples: usize) -> Vec<CaseStats> {
    let mut out: Vec<CaseStats> = Vec::new();
    for &case in CASES {
        let node = build_node(&case);
        let dist = bench(samples, || {
            let d = black_box(&node).distribution(0);
            black_box(&d);
        });
        let publish = bench(samples, || {
            let v = black_box(&node).publish(0);
            black_box(&v);
        });
        let q_hit = bench(samples, || {
            black_box(node.query(black_box(0 as Acc), 0));
        });
        let q_miss = bench(samples, || {
            black_box(node.query(black_box(STRANGER), 0));
        });
        out.push(CaseStats {
            case,
            dist,
            publish,
            q_hit,
            q_miss,
        });
    }
    out
}

/// §3/§5/§6 的状态网格。
///
/// 每组都写明「清单长度 / 视图数 / cap / 每视图条目 / 是否互不重叠」。
/// on_view 只接受「我给过正分」的来源，所以 视图数 ≤ 清单长度 是硬约束；
/// 要测 1000 份视图，观察者清单也必须是 1000 长。
/// 视图数上限是 Config::max_views（默认 4096），1000 在默认值之内。
const CASES: &[Case] = &[
    // —— 视图数 × cap 轴：每份视图塞满 cap 条，覆盖互不重叠的账户（最坏情形）——
    Case::new(10, 10, 128, 128, true),
    Case::new(100, 100, 128, 128, true),
    Case::new(1000, 1000, 128, 128, true),
    Case::new(10, 10, 512, 512, true),
    Case::new(100, 100, 512, 512, true),
    Case::new(1000, 1000, 512, 512, true),
    // —— Σ条目 才是 distribution 的真正驱动项：稀疏视图（每份 16 条）对照 ——
    Case::new(1000, 1000, 512, 16, true),
    // —— 同一人口（各视图覆盖同一批账户，条目并集 = slots）对照 ——
    Case::new(1000, 1000, 512, 512, false),
    // —— 清单长度轴：|清单| 项只出现在 publish/distribution 的个人项与权重里 ——
    Case::new(50, 10, 512, 50, true),
    Case::new(200, 100, 512, 200, true),
    Case::new(400, 400, 512, 400, true),
    Case::new(1600, 1000, 512, 512, true),
    // —— 大 cap：publish 的排序 + 二次量化成本（小清单 + 长视图）——
    Case::new(100, 100, 4096, 100, true),
    Case::new(400, 400, 4096, 400, true),
];

fn print_publish(rows: &[CaseStats]) {
    println!("\n## 3. Node::publish（= distribution → 排序截断 → from_scores 二次量化）\n");
    println!(
        "口径：每份来源观点 = 该来源「自己的完整正分清单」min(cap, slots) 条（真实部署里来源就是这样发布的）。"
    );
    println!(
        "两个副列：其一是内部的 distribution 本身，其二是 publish 相对它的放大倍数（截断/量化/排序的额外开销）。\n"
    );

    let all: Vec<Stats> = rows.iter().flat_map(|r| [r.dist, r.publish]).collect();
    let sc = Scale::auto(max_mean(&all));
    let unit = sc.unit;
    println!(
        "| 清单长度 | 视图数 | cap | 每视图条目 | Σ条目 | 条目并集 | distribution mean±std [{unit}] | publish mean±std [{unit}] | publish median | publish/distribution |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let c = r.case;
        let list = c.list;
        let views = c.views;
        let cap = c.cap;
        let slots = c.slots;
        let sum = c.views * c.slots;
        let union = c.union_slots();
        let ratio = r.publish.mean / r.dist.mean.max(1e-12);
        println!(
            "| {list} | {views} | {cap} | {slots} | {sum} | {union} | {} | {} | {} | {ratio:.2}× |",
            cell(sc, &r.dist),
            cell(sc, &r.publish),
            sc.d(r.publish.median),
        );
    }
    println!(
        "\n注意 publish 不是白送的：它先跑一遍完整分布，再把全部条目排序取 top-cap，最后 from_scores 重新量化一遍。"
    );
    println!(
        "cap 越大、条目并集越大，这一段越贵 —— 这是 mincore 唯一可能被 Engine::export_view 反超的区间（见 §7C）。"
    );
}

fn print_dist(rows: &[CaseStats]) {
    println!("\n## 5. Node::distribution（视图数 × cap）\n");
    println!(
        "复杂度读源码：O(Σ 来源视图条目数 + |清单| + 条目并集 log) —— **与「网络里有多少账户」无关**，只由我手上有多少份观点决定。\n"
    );

    let all: Vec<Stats> = rows.iter().map(|r| r.dist).collect();
    let sc = Scale::auto(max_mean(&all));
    let unit = sc.unit;
    println!(
        "| 视图数 | cap | 每视图条目 | Σ条目 | 条目并集 | 清单长度 | distribution mean±std [{unit}] | median | 相对同 cap 同覆盖口径下最小视图数的倍率 |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let c = r.case;
        let base = rows
            .iter()
            .filter(|b| b.case.cap == c.cap && b.case.disjoint == c.disjoint)
            .map(|b| b.dist.mean)
            .fold(f64::INFINITY, f64::min);
        let rel = r.dist.mean / base.max(1e-12);
        let views = c.views;
        let cap = c.cap;
        let slots = c.slots;
        let sum = c.views * c.slots;
        let union = c.union_slots();
        let list = c.list;
        println!(
            "| {views} | {cap} | {slots} | {sum} | {union} | {list} | {} | {} | {rel:.2}× |",
            cell(sc, &r.dist),
            sc.d(r.dist.median),
        );
    }
    println!(
        "\n读法要点：同一 cap 下视图数 ×10 → 耗时约 ×10（Σ条目线性）；同一视图数下 cap 128→512 → 约 ×4。"
    );
    println!(
        "「同一人口」那一行（条目并集只有 slots）说明并集大小影响的是 BTreeMap 插入，不是主项。"
    );
}

fn print_query(rows: &[CaseStats]) {
    println!("\n## 6. Node::query 与 distribution 同规模对比\n");
    println!(
        "同一份节点状态上：query(某账户) = 只对该账户做 |views| 次 q_of，distribution = 遍历所有视图的所有槽位。"
    );
    println!("这证明 query 不为「我不关心的账户」付钱。\n");

    let picks: Vec<&CaseStats> = rows.iter().filter(|r| r.case.cap == 512).collect();
    println!(
        "| 视图数 | cap | 清单长度 | Σ条目 | distribution mean±std | query(命中) mean±std | query median | query(陌生人) mean±std | distribution/query |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for r in &picks {
        let c = r.case;
        let views = c.views;
        let cap = c.cap;
        let list = c.list;
        let sum = c.views * c.slots;
        let ratio = r.dist.mean / r.q_hit.mean.max(1e-12);
        println!(
            "| {views} | {cap} | {list} | {sum} | {} | {} | {} | {} | {ratio:.1}× |",
            cell_auto(&r.dist),
            cell_auto(&r.q_hit),
            a(r.q_hit.median),
            cell_auto(&r.q_miss),
        );
    }
    println!(
        "\nquery 与「被查账户是谁」几乎无关：命中/未命中同样要遍历全部来源的权重表（weights()）。"
    );
    println!(
        "它的优势在 |views| 数，不在目标账户 —— 与 Engine::query 遍历 |清单| 的结构一样，常数不同。"
    );
}

// ══════════════════════════════ §4 on_view ══════════════════════════════

fn group_onview(samples: usize) {
    println!("\n## 4. Node::on_view（decode + 入库，端到端）\n");
    println!("四列把成本拆开：clone（只复制视图，无 decode）、decode（只反序列化）、");
    println!(
        "decode+on_view（端到端）、clone+on_view（纯入库）。两个「减出来」的本体估计应当一致 —— 这是一致性自检。\n"
    );

    struct Row {
        cap: usize,
        existing: usize,
        slots: usize,
        clone: Stats,
        decode: Stats,
        de_ins: Stats,
        cl_ins: Stats,
    }
    struct Spec {
        cap: usize,
        existing: usize,
        slots: usize,
    }

    let specs: &[Spec] = &[
        Spec {
            cap: 128,
            existing: 10,
            slots: 128,
        },
        Spec {
            cap: 128,
            existing: 1000,
            slots: 128,
        },
        Spec {
            cap: 512,
            existing: 10,
            slots: 512,
        },
        Spec {
            cap: 512,
            existing: 1000,
            slots: 512,
        },
        Spec {
            cap: 4096,
            existing: 10,
            slots: 4096,
        },
        Spec {
            cap: 4096,
            existing: 1000,
            slots: 4096,
        },
        Spec {
            cap: 4096,
            existing: 10,
            slots: 1,
        },
    ];

    let mut rows: Vec<Row> = Vec::new();
    for s in specs {
        let cap = s.cap;
        let existing = s.existing;
        let slots = s.slots.min(cap);
        let case = Case::new(existing.max(1), existing, cap, slots, true);
        let mut node = build_node(&case);
        let view = make_view(cap, slots, 0, 7);
        let bytes = view.encode();
        let owner: Acc = 0;
        let clone = bench(samples, || {
            let v = black_box(&view).clone();
            black_box(&v);
        });
        let decode = bench(samples, || {
            let v = SparseView::decode(black_box(&bytes), cap);
            black_box(&v);
        });
        let cl_ins = bench(samples, || {
            node.on_view(owner, black_box(&view).clone(), 0, 0);
        });
        let de_ins = bench(samples, || {
            let v = SparseView::decode(black_box(&bytes), cap).expect("自造负载必须可解码");
            node.on_view(owner, v, 0, 0);
        });
        rows.push(Row {
            cap,
            existing,
            slots,
            clone,
            decode,
            de_ins,
            cl_ins,
        });
    }

    let all: Vec<Stats> = rows
        .iter()
        .flat_map(|r| [r.clone, r.decode, r.de_ins, r.cl_ins])
        .collect();
    let sc = Scale::auto(max_mean(&all));
    let unit = sc.unit;
    println!(
        "| cap | 已有视图数 | 本视图条目 | clone [{unit}] | decode [{unit}] | decode+on_view [{unit}] | clone+on_view [{unit}] | 本体估计(decode 路径) | 本体估计(clone 路径) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        let cap = r.cap;
        let existing = r.existing;
        let slots = r.slots;
        let body_dec = r.de_ins.mean - r.decode.mean;
        let body_cl = r.cl_ins.mean - r.clone.mean;
        println!(
            "| {cap} | {existing} | {slots} | {} | {} | {} | {} | {} | {} |",
            cell(sc, &r.clone),
            cell(sc, &r.decode),
            cell(sc, &r.de_ins),
            cell(sc, &r.cl_ins),
            sc.d(body_dec.max(0.0)),
            sc.d(body_cl.max(0.0)),
        );
    }
    println!(
        "\n结论方向：on_view 的端到端成本几乎全在 decode 的 O(cap) 上，入库本体只有几十 ns（BTreeMap 插入 + 连败清零）。"
    );
    println!("「已有视图数」只影响一次 BTreeMap 插入的 log，1000 份与 10 份看不出差别。");
    println!(
        "cap=4096 且只有 1 条数据时 decode 依然要扫整段填充区，所以它并不因为「数据少」而变便宜。"
    );
}

// ══════════════════════════════ §1b Engine 侧编解码（口径对齐用） ══════════════════════════════

/// §1b：`CompressedView`（20480 个 f32 = 40960 B）的 encode / decode 吞吐。
///
/// 存在的理由：`a12_perf` 量 engine 的 `on_view` 用的是 `view.clone()`（**不含**解码），
/// 而量 mincore 的 `on_view` 用的是 `SparseView::decode()`（**含**解码）。
/// 要判断这个不对称对结论有多大影响，就得先知道 engine 侧解码一份 40 KB 草图要多久。
/// 这一组就是那个数。
fn group_codec(samples: usize) {
    println!("\n## 1b. Engine 侧 CompressedView::encode / decode（审计口径对齐用）\n");
    println!("默认参数 2048×5 ⇒ 每份 40960 B（是 mincore cap=512 的 13.3 倍）。本组用来量化");
    println!(
        "「a12_perf 量 engine 用 clone（不付解码）、量 mincore 用 decode（付解码）」这个口径不对称有多大。\n"
    );

    let params = ViewParams::default();
    let pairs: Vec<(AccountKey, f32)> = (0..600)
        .map(|j| (key(j), 0.1 + (j % 7) as f32 * 0.05))
        .collect();
    let view = CompressedView::from_pairs(params, &pairs);
    let bytes = view.encode();
    let nbytes = bytes.len();

    let enc = bench(samples, || {
        let v = black_box(&view).encode();
        black_box(&v);
    });
    let dec = bench(samples, || {
        let v = CompressedView::decode(black_box(&bytes), params);
        black_box(&v);
    });
    let clone = bench(samples, || {
        let v = black_box(&view).clone();
        black_box(&v);
    });
    // 顺带量一次 mincore cap=512 的 decode，让「不对称有多大」在同一张表里可读。
    let core_cap = 512usize;
    let core_pairs: Vec<(Acc, f32)> = (0..core_cap)
        .map(|j| (j as Acc * 7, 0.1 + (j % 7) as f32 * 0.05))
        .collect();
    let core_view = SparseView::from_scores(core_cap, &core_pairs);
    let core_bytes = core_view.encode();
    let core_dec = bench(samples, || {
        let v = SparseView::decode(black_box(&core_bytes), core_cap);
        black_box(&v);
    });

    println!("| 形状 | 字节 | clone | encode | decode | encode MB/s | decode MB/s |");
    println!("|---|---|---|---|---|---|---|");
    println!(
        "| 2048×5 f32（默认） | {nbytes} | {} | {} | {} | {:.0} | {:.0} |",
        a(clone.mean),
        a(enc.mean),
        a(dec.mean),
        nbytes as f64 / (1e6 * enc.mean),
        nbytes as f64 / (1e6 * dec.mean),
    );
    println!(
        "\n对照（同一次运行里现量的，见 §1 同口径）：mincore cap=512、3074 B 的 `decode` 是 {}。",
        a(core_dec.mean)
    );
    println!(
        "代价倍数：Engine 解一份观点 {} / mincore 解一份观点 {} ≈ {:.1}×。",
        a(dec.mean),
        a(core_dec.mean),
        dec.mean / core_dec.mean.max(1e-12)
    );
}

// ══════════════════════════════ §7 与 Engine 对比 ══════════════════════════════

/// 账户号 -> 32 字节公钥（与 a12_compare 完全同一映射）。
fn key(idx: usize) -> AccountKey {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    k[8] = 0xA1;
    k[9] = (idx >> 8) as u8;
    k
}

/// 来源把自己的正分清单归一化后发布的观点（engine 侧）。
fn engine_view(list: &[(Acc, f32)], params: ViewParams) -> Option<CompressedView> {
    let total: f32 = list.iter().map(|&(_, s)| s).sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let pairs: Vec<(AccountKey, f32)> = list
        .iter()
        .map(|&(u, s)| (key(u as usize), s / total))
        .collect();
    Some(CompressedView::from_pairs(params, &pairs))
}

/// 同上，mincore 侧。
fn core_view(list: &[(Acc, f32)], cap: usize) -> Option<SparseView> {
    let total: f32 = list.iter().map(|&(_, s)| s).sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let pairs: Vec<(Acc, f32)> = list.iter().map(|&(u, s)| (u, s / total)).collect();
    Some(SparseView::from_scores(cap, &pairs))
}

/// §7A：合成最坏图（复现 src/bin/a12_perf.rs 的构造，但采样换成 3 预热 + ≥20 样本）。
fn group_synth(samples: usize, seed: u64) {
    println!("\n## 7A. 与 tm_engine 正面对比 —— 合成最坏图（复现 a12_perf，图密度写死）\n");
    println!("图密度（**固定，报告必须写明**）：n 个账户；每个账户的正分清单恰好 10 条");
    println!(
        "（账户 i 给 (i+1)..=(i+10) 各 0.1）；观察者给**全部 n 个**账户打 0.5（清单长度 = n、来源视图数 = n）。"
    );
    println!("这正是 trust_distribution 的 O(n·m) 最坏情形，也是 A12 表格用的同一套构造。");
    println!(
        "mincore cap = 512（Config::default()）；Engine ViewParams::default()（2048×5 ⇒ 40960 B/份）。\n"
    );

    let cap = 512usize;
    let params = ViewParams::default();

    println!("### 7A-1 on_view / query\n");
    let mut onview_rows: Vec<CmpRow> = Vec::new();
    let mut full_rows: Vec<CmpRow> = Vec::new();
    let mut n_axis: Vec<(usize, Stats, Stats)> = Vec::new();

    for n in [100usize, 200, 400, 800, 1600] {
        // ---- 造图 ----
        let mut eng_views: Vec<CompressedView> = Vec::with_capacity(n);
        let mut core_views: Vec<SparseView> = Vec::with_capacity(n);
        for i in 0..n {
            let raw: Vec<(Acc, f32)> = (1..=10).map(|j| (((i + j) % n) as Acc, 0.1f32)).collect();
            let ep: Vec<(AccountKey, f32)> =
                raw.iter().map(|&(a, s)| (key(a as usize), s)).collect();
            eng_views.push(CompressedView::from_pairs(params, &ep));
            core_views.push(SparseView::from_scores(cap, &raw));
        }
        let eng_bytes = eng_views[0].encode();
        let core_bytes = core_views[0].encode();
        let eng0 = eng_views[0].clone();

        // ---- Engine 状态：先 intern 全部账户（mass_covers 陷阱），再 rate，再 on_view ----
        let mut e = Engine::with_seed(EngineConfig::default(), seed);
        for i in 0..n {
            e.intern(key(i));
        }
        for i in 0..n {
            e.rate(i as Acc, 0.5, 0);
        }
        for (i, v) in eng_views.into_iter().enumerate().skip(1) {
            e.on_view(i as Acc, v, 0, 0);
        }
        // ---- mincore 状态 ----
        let mut node = Node::new(
            CoreConfig {
                cap,
                ..CoreConfig::default()
            },
            seed,
        );
        for i in 0..n {
            node.rate(i as Acc, 0.5, 0);
        }
        for (i, v) in core_views.iter().enumerate().skip(1) {
            node.on_view(i as Acc, v.clone(), 0, 0);
        }

        // ---- on_view：decode 路径（A12 的 mincore 口径）对 decode 路径（对称化）----
        let eng_dec = bench(samples, || {
            let v = CompressedView::decode(black_box(&eng_bytes), params)
                .expect("自造 engine 负载必须可解码");
            e.on_view(0, v, 0, 0);
        });
        let core_dec = bench(samples, || {
            let v = SparseView::decode(black_box(&core_bytes), cap)
                .expect("自造 mincore 负载必须可解码");
            node.on_view(0, v, 0, 0);
        });
        // ---- on_view：clone 路径（A12 的 engine 口径）对 clone 路径（对称化）----
        let eng_clone = bench(samples, || {
            e.on_view(0, black_box(&eng0).clone(), 0, 0);
        });
        let core_clone = bench(samples, || {
            node.on_view(0, black_box(&core_views[0]).clone(), 0, 0);
        });

        onview_rows.push(CmpRow {
            label: format!("n={n} on_view（decode 端到端）"),
            eng: eng_dec,
            core: core_dec,
        });
        onview_rows.push(CmpRow {
            label: format!("n={n} on_view（仅 clone+入库）"),
            eng: eng_clone,
            core: core_clone,
        });

        // ---- 单账户 query（轮转目标，避免总是查同一个）----
        let mut r1 = 0usize;
        let eng_q = bench(samples, || {
            r1 = (r1 + 1) % n;
            black_box(e.query(black_box(r1 as Acc), 0));
        });
        let mut r2 = 0usize;
        let core_q = bench(samples, || {
            r2 = (r2 + 1) % n;
            black_box(node.query(black_box(r2 as Acc), 0));
        });
        onview_rows.push(CmpRow {
            label: format!("n={n} 单账户 query"),
            eng: eng_q,
            core: core_q,
        });

        // ---- 全量分布：engine trust_distribution vs mincore distribution ----
        let eng_d = bench(samples, || {
            let v = e.trust_distribution(0);
            black_box(&v);
        });
        let core_d = bench(samples, || {
            let v = node.distribution(0);
            black_box(&v);
        });
        full_rows.push(CmpRow {
            label: format!("n={n} 全量分布"),
            eng: eng_d,
            core: core_d,
        });
        n_axis.push((n, eng_d, core_d));

        // ---- 发布：engine export_view vs mincore publish ----
        let eng_x = bench(samples, || {
            let v = e.export_view(0);
            black_box(&v);
        });
        let core_x = bench(samples, || {
            let v = node.publish(0);
            black_box(&v);
        });
        full_rows.push(CmpRow {
            label: format!("n={n} 发布（publish / export_view）"),
            eng: eng_x,
            core: core_x,
        });
    }

    print_cmp(&onview_rows);
    println!(
        "\n注意口径对齐：a12_perf 的原测量是**不对称**的 —— engine 侧用 view.clone()（不付 40 KB 解码），"
    );
    println!(
        "mincore 侧用 SparseView::decode()（付 24 KB 解码）。本表两侧各给两条，同一条路径才能正面比。\n"
    );
    print_cmp(&full_rows);

    // ---- 「在什么规模开始撑不住」：只报实测点 + 明确标注的外推 ----
    println!("\n### 7A-2 规模曲线（trust_distribution 的 O(n²) 是最陡的一条）\n");
    println!(
        "| n | Engine 全量分布 mean [ms] | Engine 每秒可做次数 | mincore 全量分布 mean [ms] | mincore 每秒可做次数 | Engine 相对 n=100 的实测倍率 / n² 应有倍率 |"
    );
    println!("|---|---|---|---|---|---|");
    let eng100 = n_axis[0].1.mean;
    for (n, eng, core) in &n_axis {
        let per_eng = 1.0 / eng.mean.max(1e-12);
        let per_core = 1.0 / core.mean.max(1e-12);
        let gained = eng.mean / eng100.max(1e-12);
        let expect = ((*n as f64) / 100.0).powi(2);
        println!(
            "| {n} | {:.2} | {per_eng:.4} | {:.3} | {per_core:.0} | {gained:.1}× / {expect:.1}× |",
            eng.mean * 1e3,
            core.mean * 1e3,
        );
    }
    // 外推：按 n² 从最大的实测点推「达到 100 ms / 1 s / 10 s / 1000 s」的 n。
    let (n_max, eng_max, _) = n_axis[n_axis.len() - 1];
    println!("\n外推（只由上面的实测点按 n² 拟合，**不是实测**）：");
    for target_ms in [100.0f64, 1000.0, 10_000.0, 1e6] {
        let n_est = (n_max as f64) * (target_ms / 1e3 / eng_max.mean.max(1e-12)).sqrt();
        println!("- Engine trust_distribution 单次达到 {target_ms:.0} ms 的规模 ≈ n = {n_est:.0}");
    }
    println!(
        "- 同样的 n 上 mincore 的 distribution 只随「我手上多少份观点、每份多长」涨，不随 n 涨。\n"
    );
}

/// §7B 的构造结果：一张真实的 tm_sim::World 图 + 两侧预热的引擎/节点状态。
struct WorldInc {
    n: usize,
    obs_list: usize,
    views: usize,
    avg_slots: f64,
    union: usize,
    engine: Engine,
    node: Node,
    reserved: Acc,
    eng_bytes: Vec<u8>,
    core_bytes: Vec<u8>,
    eng0: CompressedView,
    core0: SparseView,
}

/// 与 `a12_compare::world_cfg` 逐字段相同的世界配置。
///
/// `attack` 显式作为参数暴露：§7B 默认测 `AttackKind::None`（干净社会），
/// 但 `--samples` 复跑并做「同机对比」时必须能复现**同一个世界**，
/// 所以 seed 与攻击类型都是参数而不是常量。
fn world_cfg(seed: u64, n_initial: usize, attack: AttackKind) -> SimConfig {
    SimConfig {
        n_initial,
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

fn world_inc(
    n_initial: usize,
    seed: u64,
    attack: AttackKind,
    cap: usize,
    params: ViewParams,
) -> WorldInc {
    let mut w = World::new(world_cfg(seed, n_initial, attack));
    w.run();
    let n = w.accounts.len();
    let mut out: Vec<Vec<(Acc, f32)>> = vec![Vec::new(); n];
    for (s, u, score) in w.ratings() {
        out[s].push((u as Acc, score));
    }
    for list in &mut out {
        list.sort_by_key(|x| x.0);
        list.dedup_by_key(|x| x.0);
    }
    let kinds: Vec<Kind> = w.accounts.iter().map(|a| a.kind).collect();
    let mut honest: Vec<usize> = (0..n)
        .filter(|&i| matches!(kinds[i], Kind::Honest))
        .collect();
    // 出度最大的诚实账户当观察者（与 a12_compare::observers 同规则）。
    honest.sort_by(|&a, &b| out[b].len().cmp(&out[a].len()).then(a.cmp(&b)));
    let observer = honest[0];
    let obs_list = out[observer].len();
    let reserved = out[observer][0].0;

    // 条目并集（各来源视图覆盖到多少不同账户）。
    let union = {
        let mut seen: std::collections::BTreeSet<Acc> = std::collections::BTreeSet::new();
        for &(u, _) in &out[observer] {
            for &(a, _) in &out[u as usize] {
                seen.insert(a);
            }
        }
        seen.len()
    };
    let mut slotsum = 0usize;
    let mut views = 0usize;
    for &(u, _) in &out[observer] {
        let len = out[u as usize].len();
        if len > 0 {
            slotsum += len.min(cap);
            views += 1;
        }
    }
    let avg_slots = slotsum as f64 / views.max(1) as f64;

    // Engine：先把**全部** n 个账户 intern（StoredView::mass_covers 陷阱），再 rate、再 on_view。
    let mut engine = Engine::with_seed(EngineConfig::default(), 1);
    for i in 0..n {
        engine.intern(key(i));
    }
    for &(u, s) in &out[observer] {
        engine.rate(u, s, 0);
    }
    let eng0 = engine_view(&out[reserved as usize], params).expect("保留来源必须有正分");
    let eng_bytes = eng0.encode();
    for &(u, _) in &out[observer] {
        if u == reserved {
            continue;
        }
        if let Some(v) = engine_view(&out[u as usize], params) {
            engine.on_view(u, v, 0, 0);
        }
    }

    // mincore：同一份观察者清单、同一批来源观点。
    let mut node = Node::new(
        CoreConfig {
            cap,
            ..CoreConfig::default()
        },
        1,
    );
    for &(u, s) in &out[observer] {
        node.rate(u, s, 0);
    }
    let core0 = core_view(&out[reserved as usize], cap).expect("保留来源必须有正分");
    let core_bytes = core0.encode();
    for &(u, _) in &out[observer] {
        if u == reserved {
            continue;
        }
        if let Some(v) = core_view(&out[u as usize], cap) {
            node.on_view(u, v, 0, 0);
        }
    }

    WorldInc {
        n,
        obs_list,
        views,
        avg_slots,
        union,
        engine,
        node,
        reserved,
        eng_bytes,
        core_bytes,
        eng0,
        core0,
    }
}

/// §7B：同一张 tm_sim::World 真实社会图上的正面对比。
fn group_world(samples: usize) {
    println!("\n## 7B. 与 tm_engine 正面对比 —— 同一张 tm_sim::World 真实社会图\n");
    println!(
        "图参数与 a12_compare::world_cfg **逐字段相同**：n_initial ∈ {{400,800,1600}}、steps = 120、"
    );
    println!("join/leave/dormancy = 0、enable_rhythm = false、follow 120~200、max_follows = 256、");
    println!(
        "attack_kind = None、seed = 7；观察者 = 出度最大的诚实账户；每个人发布自己的归一化正分清单。"
    );
    println!(
        "mincore cap = 512；Engine ViewParams::default()。Engine 侧**先 intern 全部账户再 on_view**。\n"
    );

    let params = ViewParams::default();
    let cap = 512usize;
    let mut rows: Vec<CmpRow> = Vec::new();
    for n_initial in [400usize, 800, 1600] {
        let mut d = world_inc(n_initial, 7, AttackKind::None, cap, params);
        println!(
            "图 n={}：观察者清单 {} 条、来源视图 {} 份、每份平均 {:.1} 条、条目并集 {}。",
            d.n, d.obs_list, d.views, d.avg_slots, d.union
        );

        let eng_dec = bench(samples, || {
            let v = CompressedView::decode(black_box(&d.eng_bytes), params)
                .expect("engine 负载必须可解码");
            d.engine.on_view(d.reserved, v, 0, 0);
        });
        let core_dec = bench(samples, || {
            let v =
                SparseView::decode(black_box(&d.core_bytes), cap).expect("mincore 负载必须可解码");
            d.node.on_view(d.reserved, v, 0, 0);
        });
        rows.push(CmpRow {
            label: format!("n={} on_view（decode 端到端）", d.n),
            eng: eng_dec,
            core: core_dec,
        });

        let eng_cl = bench(samples, || {
            d.engine
                .on_view(d.reserved, black_box(&d.eng0).clone(), 0, 0);
        });
        let core_cl = bench(samples, || {
            d.node
                .on_view(d.reserved, black_box(&d.core0).clone(), 0, 0);
        });
        rows.push(CmpRow {
            label: format!("n={} on_view（仅 clone+入库）", d.n),
            eng: eng_cl,
            core: core_cl,
        });

        let eng_q = bench(samples, || {
            black_box(d.engine.query(black_box(d.reserved), 0));
        });
        let core_q = bench(samples, || {
            black_box(d.node.query(black_box(d.reserved), 0));
        });
        rows.push(CmpRow {
            label: format!("n={} 单账户 query", d.n),
            eng: eng_q,
            core: core_q,
        });

        let eng_dist = bench(samples, || {
            let v = d.engine.trust_distribution(0);
            black_box(&v);
        });
        let core_dist = bench(samples, || {
            let v = d.node.distribution(0);
            black_box(&v);
        });
        rows.push(CmpRow {
            label: format!("n={} 全量分布", d.n),
            eng: eng_dist,
            core: core_dist,
        });

        let eng_x = bench(samples, || {
            let v = d.engine.export_view(0);
            black_box(&v);
        });
        let core_x = bench(samples, || {
            let v = d.node.publish(0);
            black_box(&v);
        });
        rows.push(CmpRow {
            label: format!("n={} 发布（publish / export_view）", d.n),
            eng: eng_x,
            core: core_x,
        });
    }
    println!();
    print_cmp(&rows);
    println!("\n真实图上 Engine 的 on_view 是 O(已知账户数) + 1000 次噪声采样（与清单长度无关）；");
    println!("mincore 的 on_view 是 O(cap) 解码（与人数无关）。两者的斜率方向完全不同。");
}

/// §7C：mincore 唯一可能被反超的区间 —— 小 n、长视图、大 cap。
fn group_publish_vs_export(samples: usize) {
    println!("\n## 7C. 反超区间：publish 对 export_view（小 n + 长视图 + 大 cap）\n");
    println!(
        "Engine export_view 的成本 ≈ n × |清单| × hashers 次哈希（闭式，与「来源视图多长」无关）；"
    );
    println!("mincore publish 的成本 ≈ Σ 视图条目 + 条目并集 log + 截断重排。");
    println!(
        "因此当「观察者清单短、但来源视图很长」时，mincore 会反被 Engine 超车。这正是 A12 预言但没实测到的那一段。\n"
    );

    #[derive(Clone, Copy)]
    struct Spec {
        population: usize,
        list: usize,
        view_len: usize,
        cap: usize,
    }

    let specs: &[Spec] = &[
        Spec {
            population: 100,
            list: 100,
            view_len: 512,
            cap: 4096,
        },
        Spec {
            population: 100,
            list: 100,
            view_len: 4096,
            cap: 4096,
        },
        Spec {
            population: 400,
            list: 400,
            view_len: 512,
            cap: 4096,
        },
        Spec {
            population: 400,
            list: 400,
            view_len: 4096,
            cap: 4096,
        },
        Spec {
            population: 1600,
            list: 400,
            view_len: 2048,
            cap: 4096,
        },
    ];

    let params = ViewParams::default();
    let mut rows: Vec<CmpRow> = Vec::new();
    for s in specs {
        let n = s.population;
        let list = s.list.min(n);
        let slots = s.view_len;
        // 图：n 个账户；观察者评 list 个来源；每个来源发布 slots 条
        // （账户号取 0..slots，允许超出 n 的「外部账户」）。
        let mut eng_views: Vec<CompressedView> = Vec::with_capacity(list);
        let mut core_views: Vec<SparseView> = Vec::with_capacity(list);
        for v in 0..list {
            let raw: Vec<(Acc, f32)> = (0..slots)
                .map(|j| (((v * slots + j) % 200_000) as Acc, 0.1f32))
                .collect();
            let ep: Vec<(AccountKey, f32)> =
                raw.iter().map(|&(a, sc)| (key(a as usize), sc)).collect();
            eng_views.push(CompressedView::from_pairs(params, &ep));
            core_views.push(SparseView::from_scores(s.cap, &raw));
        }
        let mut e = Engine::with_seed(EngineConfig::default(), 1);
        for i in 0..n {
            e.intern(key(i));
        }
        for i in 0..list {
            e.rate(i as Acc, 0.5, 0);
        }
        for (i, v) in eng_views.into_iter().enumerate() {
            e.on_view(i as Acc, v, 0, 0);
        }
        let mut node = Node::new(
            CoreConfig {
                cap: s.cap,
                ..CoreConfig::default()
            },
            1,
        );
        for i in 0..list {
            node.rate(i as Acc, 0.5, 0);
        }
        for (i, v) in core_views.into_iter().enumerate() {
            node.on_view(i as Acc, v, 0, 0);
        }
        let eng_x = bench(samples, || {
            let v = e.export_view(0);
            black_box(&v);
        });
        let core_x = bench(samples, || {
            let v = node.publish(0);
            black_box(&v);
        });
        rows.push(CmpRow {
            label: format!(
                "n={} 清单={} 视图={} 条 cap={}",
                s.population, list, slots, s.cap
            ),
            eng: eng_x,
            core: core_x,
        });
    }
    print_cmp(&rows);
    println!(
        "\n这张表就是「mincore 不一定永远更快」的证据：Σ 视图条目 一旦超过 Engine 的 n×|清单|×hashers，"
    );
    println!("publish 的「全量遍历 + 排序截断 + 二次量化」就要付账。");
}

// ══════════════════════════════ 入口 ══════════════════════════════

fn rustc_version() -> String {
    match Command::new("rustc").arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "（rustc 不在 PATH：请自行记录 rustc --version）".to_string(),
    }
}

fn print_env(samples: usize) {
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "（未知）".to_string());
    let cpus = std::env::var("NUMBER_OF_PROCESSORS").unwrap_or_else(|_| "（未知）".to_string());
    let profile = if cfg!(debug_assertions) {
        "debug（不是 release，数字不可用）"
    } else {
        "release（opt-level=3 + lto=true，见 crates/mincore/Cargo.toml）"
    };
    let rustc = rustc_version();
    println!("# tm_mincore 性能基准");
    println!("\n## 0. 环境\n");
    println!("| 项 | 值 |");
    println!("|---|---|");
    println!("| CPU（PROCESSOR_IDENTIFIER） | {cpu} |");
    println!("| 逻辑核 | {cpus} |");
    println!("| rustc | {rustc} |");
    println!("| 构建 profile | {profile} |");
    println!("| 计时 | std::time::Instant（Windows QPC，量化约 100 ns） |");
    println!(
        "| 采样 | 每用例 {WARM} 次预热丢弃 + {samples} 个样本；单样本自适应迭代，目标 ≥200 µs |"
    );
    println!("| 统计 | mean ± std（样本标准差，n−1）、median |");
    println!("| 消优化 | std::hint::black_box（结果与输入两侧） |");
}

fn want(groups: &[String], all: bool, name: &str) -> bool {
    all || groups.iter().any(|g| g == name)
}

fn main() {
    let mut samples = DEFAULT_SAMPLES;
    let mut seed: u64 = 1;
    let mut groups: Vec<String> = Vec::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--samples" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    if let Ok(k) = v.parse::<usize>() {
                        samples = k.max(2);
                    }
                }
            }
            "--seed" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    if let Ok(k) = v.parse::<u64>() {
                        seed = k;
                    }
                }
            }
            _ => groups.push(args[i].clone()),
        }
        i += 1;
    }
    let all = groups.is_empty() || groups.iter().any(|g| g == "all");

    // 定长布局自检：报告里引用的 770/1538/3074/24578 字节就是这一式算出来的。
    for cap in [128usize, 256, 512, 4096] {
        assert_eq!(
            SparseView::new(cap).bytes(),
            cap * SLOT_BYTES + VIEW_OVERHEAD,
            "定长布局与 SLOT_BYTES/VIEW_OVERHEAD 不符"
        );
    }
    assert_eq!(CompressedView::new(ViewParams::default()).bytes(), 40_960);

    print_env(samples);

    if want(&groups, all, "view") {
        group_view(samples);
    }
    if want(&groups, all, "codec") {
        group_codec(samples);
    }
    if want(&groups, all, "qof") {
        group_qof(samples);
    }
    if want(&groups, all, "onview") {
        group_onview(samples);
    }
    if want(&groups, all, "publish") || want(&groups, all, "dist") || want(&groups, all, "query") {
        println!("\n> §3 / §5 / §6 共用一次状态构造（publish 内部就是 distribution）。");
        let rows = sweep_compute(samples);
        if want(&groups, all, "publish") {
            print_publish(&rows);
        }
        if want(&groups, all, "dist") {
            print_dist(&rows);
        }
        if want(&groups, all, "query") {
            print_query(&rows);
        }
    }
    if want(&groups, all, "engine") {
        group_synth(samples, seed);
        group_world(samples);
        group_publish_vs_export(samples);
    }
    println!("\n（完）");
}
