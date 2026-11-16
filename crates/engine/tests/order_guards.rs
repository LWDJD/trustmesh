//! B3 验收护栏：三条「只要有人把顺序 / 随机性带进结果就立刻变红」的回归测试。
//!
//! 这三条不是功能测试，而是把审计结论钉成可执行断言：
//!
//! | 测试 | 钉住的不变量 | 对应审计 |
//! | --- | --- | --- |
//! | [`queries_and_stats_do_not_depend_on_rng`] | S2：`query` / `trust_distribution` / `freshness_stats` / `export_view` 与 rng 状态无关 | A2-store §1.7、A10 §S6 |
//! | [`rate_and_on_view_commute_after_intern`] | S6：同一最终状态下，`rate` / `on_view` 的**操作顺序**不改变任何读数 | A1-engine §4、A3-view §9 |
//! | [`mass_covers_boundary_matches_reference_engine`] | `mass_covers == on_view 时刻的 account_count`（行为等价 + 双向边界） | A13-07、P0 附带 |
//!
//! 每条测试的文档注释都写了「它在防什么回归」与「改动前会怎样变红」。
//!
//! 全部位模式都由本工作树实测打印，没有一处是估算。公共场景固定为：
//! 引擎 `beta = 0.3 / half_life = 72`、观点参数 `512×5 / seed = 7`；
//! 6 个来源（key 200..205）各 9 条 2 位小数读数，评分与 on_view 时间
//! 分别是 [`RATINGS`] / [`ON_VIEW_AT`]。编号布局固定：
//! 目标 key 1..9 → 编号 0..8，来源 key 200..205 → 编号 9..14。

use std::collections::HashSet;

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig, FreshnessStats};
use tm_view::{CompressedView, ViewParams};

// ===========================================================================
// 公共场景
// ===========================================================================

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..4].copy_from_slice(&i.to_le_bytes());
    k
}

/// 观点参数：512 个桶 × 5 个哈希、主种子 7（与 `p0_core.rs` 的合规场景一致）。
fn params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

fn cfg() -> EngineConfig {
    EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: params(),
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        ..Default::default()
    }
}

/// 6 个来源 × 9 个目标的读数矩阵，全部是 2 位小数（协议允许的 [0,1] 内取值）。
///
/// 矩阵本身不重要 —— 它只需要是一份**合规且互不相同**的输入，让每个目标的
/// 群体项都真正依赖多个来源。列和刻意不整齐：不同目标的读数会落在不同的
/// 位模式上，任何一处累加顺序 / 随机抖动都会立刻体现在位表上。
const ROWS: [[f32; 9]; 6] = [
    [1.00, 0.90, 0.70, 0.60, 0.50, 0.40, 0.30, 0.20, 0.10],
    [0.10, 0.90, 0.80, 0.70, 0.60, 0.50, 0.40, 0.20, 0.00],
    [0.00, 0.20, 0.90, 0.80, 0.70, 0.50, 0.30, 0.10, 0.05],
    [0.30, 0.20, 0.10, 0.90, 0.80, 0.70, 0.50, 0.40, 0.00],
    [0.50, 0.40, 0.30, 0.20, 0.10, 0.90, 0.60, 0.10, 0.00],
    [0.70, 0.60, 0.50, 0.40, 0.30, 0.20, 0.90, 0.80, 0.10],
];

/// 我给 6 个来源的原始评分（0~n 的量级，由库自己归一化成份额）。
///
/// 刻意不等权（最大 6.3、最小 0.35，差 18 倍）：群体项是一次**真正加权**的
/// 平均，顺序敏感的累加会因此暴露得比等权场景快得多。
const RATINGS: [f32; 6] = [6.3, 4.1, 2.7, 1.3, 0.8, 0.35];

/// 第 i 个来源的 `on_view(now)`。刻意各不相同：来源级衰减因子互不相等，
/// `q@0` 与 `q@100` 是两份不同的权重快照。
const ON_VIEW_AT: [u64; 6] = [55, 61, 68, 74, 81, 89];

fn view_of_source(i: usize) -> CompressedView {
    let pairs: Vec<([u8; 32], f32)> = (0..9).map(|j| (key(j as u32 + 1), ROWS[i][j])).collect();
    CompressedView::from_pairs(params(), &pairs)
}

const TARGETS: u32 = 9;
const SOURCES: u32 = 6;

/// 建场景：目标 0..8、来源 9..14，先打分再逐个收观点。
///
/// `with_tick` 为真时在收观点之前先抽一轮（抽满 6 个来源），
/// 让引擎带着「已消耗的 rng 状态」进入后续全部计算。
fn build_scene(seed: u64, with_tick: bool) -> Engine {
    let mut e = Engine::with_seed(cfg(), seed);
    for i in 1..=TARGETS {
        e.intern(key(i));
    }
    let sources: Vec<u32> = (0..SOURCES).map(|i| e.intern(key(200 + i))).collect();
    for (i, &s) in sources.iter().enumerate() {
        e.rate(s, RATINGS[i], 0);
    }
    if with_tick {
        e.tick(0, SOURCES as usize);
    }
    for (i, &s) in sources.iter().enumerate() {
        e.on_view(s, view_of_source(i), 0, ON_VIEW_AT[i]);
    }
    e
}

/// 全部 15 个账户（编号 0..14）的 query 位模式。
fn query_bits(e: &Engine, now: u64) -> Vec<u32> {
    (0..TARGETS + SOURCES).map(|i| e.query(i, now).to_bits()).collect()
}

fn dist_bits(e: &Engine, now: u64) -> Vec<(u32, u32)> {
    e.trust_distribution(now)
        .into_iter()
        .map(|(i, v)| (i, v.to_bits()))
        .collect()
}

/// 实测位表：`now = 0`。前 9 个是目标（群体项 + 个人项），后 6 个是来源
/// （只有个人项，所以和 `now = 100` 的同一组数值逐位相同）。
const BITS_AT_0: [u32; 15] = [
    1033811531, 1038234877, 1038837698, 1038386754, 1036123223, 1034803456, 1031943099,
    1024214302, 1006937445, 1039723457, 1034026755, 1029004380, 1020097889, 1014816573,
    1004356438,
];

/// 实测位表：`now = 100`。来源的 on_view 时间在 55..89，全部已经衰减过，
/// 目标读数因此与 `now = 0` 不同。
const BITS_AT_100: [u32; 15] = [
    1033518854, 1037896684, 1038724924, 1038459445, 1036176835, 1035082054, 1032204773,
    1024436963, 1006663360, 1039723457, 1034026755, 1029004380, 1020097889, 1014816573,
    1004356438,
];

/// `trust_distribution(100)` 的实测位表：15 个值全部 `query > 0`，
/// 总和恰好为 1，所以归一化没有改动任何一位（分布 = query 的逐位复制）。
const DIST_AT_100: [(u32, u32); 15] = [
    (0, 1033518854),
    (1, 1037896684),
    (2, 1038724924),
    (3, 1038459445),
    (4, 1036176835),
    (5, 1035082054),
    (6, 1032204773),
    (7, 1024436963),
    (8, 1006663360),
    (9, 1039723457),
    (10, 1034026755),
    (11, 1029004380),
    (12, 1020097889),
    (13, 1014816573),
    (14, 1004356438),
];

/// `freshness_stats(100)` 的实测读数：6 份观点，年龄 45 / 39 / 32 / 26 / 19 / 11，
/// 年龄中位数取中间两个 (26, 32) 的均值向下取整 ⇒ 29。
const FRESH_AT_100: FreshnessStats = FreshnessStats {
    count: 6,
    fresh_within_half_life: 6,
    median_age: 29,
    max_age: 45,
};

/// 全部 7 个种子。`0` / `u64::MAX` 是 rng 的边界状态（`Rng::new(0)` 会被换成
/// 内置的替代种子），`0xDEADBEEF` 是任意大数，其余是常用小种子。
const SEEDS: [u64; 7] = [0, 1, 7, 42, 99, 0xDEAD_BEEF, u64::MAX];

/// 一轮抽取得到的来源「相对编号」序列（0..6，对应 key 200..205）。
///
/// 每次都在全新引擎上重建同一场景、再抽一轮，所以序列只由种子决定。
fn tick_sequence(seed: u64) -> Vec<u32> {
    let mut e = build_scene(seed, true);
    e.tick(120, SOURCES as usize)
        .into_iter()
        .map(|r| r.idx - TARGETS)
        .collect()
}

// ===========================================================================
// ① S2 跨种子逐位不变：结果不许沾 rng
// ===========================================================================

/// **它在防什么回归**：引擎里有一支 `rng`（抽样用）。任何让**读数本身**
/// 读到 rng 的改动 —— 给读数加抽样噪声、用 rng 抽一批来源近似求群体项、
/// 用随机数决定截尾边界 —— 都会让「同一个引擎、同一份输入、只换构造种子」
/// 得到**不同**的声望。这正是设计规格 §8.1 的 S2：同样输入必然得到同样输出。
///
/// **改动前会怎样变红**（以下任一条成立，本测试立刻红）：
/// * 给读数加上 `±ε` 抽样噪声 → 位表整列漂移；
/// * 用随机数打破平局 → 前 9 个目标的位模式随种子跳变；
/// * 让群体项依赖 rng 抽样（例如按 rng 抽一部分来源近似求均值、按 rng 顺序
///   做逐步截断）→ 每次抽到的来源集合不同，读数随种子跳变，
///   [`BITS_AT_0`] / [`BITS_AT_100`] 的任一项立刻不等；
/// * `query` **消耗** rng（只读接口偷偷调 `next_u64()`）：末尾那条
///   「1000 次只读调用之后抽取序列不变」的断言会红。
///
/// 需要说清的一点：**单纯把遍历顺序换成 rng 顺序**未必会让位表变红 ——
/// 群体项的均值用 f64 累加、出口才转 f32，几个等量级数项重排通常被末位舍入
/// 吃掉（这是实测结论，不是推测）。那种「顺序回归」由下一条测试
/// [`rate_and_on_view_commute_after_intern`] 负责钉住；
/// 本条只管「数值本身沾了 rng」这一类。
///
/// 这一条同时钉住边界：`trust_distribution` / `freshness_stats` /
/// `export_view` 也必须与 rng 无关，且「无 tick / 有 tick」两种情形逐位相同。
#[test]
fn queries_and_stats_do_not_depend_on_rng() {
    let mut encodings: Vec<Vec<u8>> = Vec::new();
    let mut sequences: Vec<Vec<u32>> = Vec::new();

    for &seed in &SEEDS {
        // ---- 不带 tick 的引擎：读数必须精确等于实测位表 ----
        let plain = build_scene(seed, false);
        assert_eq!(
            query_bits(&plain, 0),
            BITS_AT_0,
            "seed {seed} 的 query@0 位表（改坏点：query 沾了 rng）"
        );
        assert_eq!(
            query_bits(&plain, 100),
            BITS_AT_100,
            "seed {seed} 的 query@100 位表"
        );
        assert_eq!(dist_bits(&plain, 100), DIST_AT_100, "seed {seed} 的分布位表");
        assert_eq!(
            plain.freshness_stats(100),
            FRESH_AT_100,
            "seed {seed} 的新鲜度"
        );
        encodings.push(plain.export_view(100).encode());

        // ---- 带 tick 的引擎：rng 状态不同，读数必须一位不差 ----
        // build_scene 内部已抽过一轮；这里再抽一轮（自动开新一轮、抽满 6 个）。
        let mut ticked = build_scene(seed, true);
        let sequence: Vec<u32> = ticked
            .tick(120, SOURCES as usize)
            .into_iter()
            .map(|r| r.idx - TARGETS)
            .collect();
        assert_eq!(
            query_bits(&ticked, 0),
            BITS_AT_0,
            "seed {seed}：有 tick 时 query@0 必须与无 tick 逐位相同"
        );
        assert_eq!(
            query_bits(&ticked, 100),
            BITS_AT_100,
            "seed {seed}：有 tick 时 query@100 必须与无 tick 逐位相同"
        );
        assert_eq!(dist_bits(&ticked, 100), DIST_AT_100);
        assert_eq!(ticked.freshness_stats(100), FRESH_AT_100);
        assert_eq!(
            ticked.export_view(100).encode(),
            *encodings.last().expect("刚刚推入过"),
            "seed {seed}：有 tick 时 export_view 必须逐位相同"
        );
        sequences.push(sequence);
    }

    // export_view 的编码（10240 字节）必须 7 个种子完全一致。
    for (i, encoded) in encodings.iter().enumerate().skip(1) {
        assert_eq!(
            encoded, &encodings[0],
            "seed {} 的 export_view 字节与 seed {} 不同：query 沾了 rng",
            SEEDS[i], SEEDS[0]
        );
    }

    // ---- rng 真的活着：抽取序列随种子不同，且每轮抽满、不放回 ----
    for (i, sequence) in sequences.iter().enumerate() {
        let seen: HashSet<u32> = sequence.iter().copied().collect();
        assert_eq!(
            seen.len(),
            SOURCES as usize,
            "seed {} 的一轮应当抽满 {} 个不同来源，实际 {sequence:?}",
            SEEDS[i],
            SOURCES
        );
    }
    let distinct: HashSet<&Vec<u32>> = sequences.iter().collect();
    assert_eq!(
        distinct.len(),
        SEEDS.len(),
        "抽取序列必须随种子变化（实测 7 个种子互不相同），否则 rng 被短路了：{sequences:?}"
    );
    for (i, &seed) in SEEDS.iter().enumerate() {
        assert_eq!(
            tick_sequence(seed),
            sequences[i],
            "同一种子必须得到同一条抽取序列（seed = {seed}）"
        );
    }

    // ---- 只读接口不得推进 rng ----
    let mut read_only = build_scene(7, false);
    let mut untouched = build_scene(7, false);
    for _ in 0..1000 {
        let _ = read_only.query(3, 17);
        let _ = read_only.trust_distribution(17);
        let _ = read_only.freshness_stats(17);
        let _ = read_only.export_view(17);
    }
    let after: Vec<u32> = read_only
        .tick(120, SOURCES as usize)
        .into_iter()
        .map(|r| r.idx - TARGETS)
        .collect();
    let expected: Vec<u32> = untouched
        .tick(120, SOURCES as usize)
        .into_iter()
        .map(|r| r.idx - TARGETS)
        .collect();
    assert_eq!(
        after, expected,
        "query / trust_distribution / freshness_stats / export_view 必须只读：\
         1000 次调用后 rng 状态变了，抽取序列跟着变"
    );
}

// ===========================================================================
// ② S6 换序不变：只打乱 rate / on_view 的操作顺序
// ===========================================================================

/// 一步操作：给某个来源打分（`kind = 0`）或收下它的观点（`kind = 1`）。
/// 每个操作自带自己的时间参数（[`RATINGS`] / [`ON_VIEW_AT`]），
/// 换序只改**先后**，不改参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Op {
    src: u32,
    kind: u8,
}

/// 一次完整运行得到的全部读数（逐位比较用）。
#[derive(Debug, PartialEq, Eq)]
struct Signature {
    q0: Vec<u32>,
    q100: Vec<u32>,
    dist0: Vec<(u32, u32)>,
    dist100: Vec<(u32, u32)>,
    encoded: Vec<u8>,
}

/// **所有账户先 intern 完**，再按 `order` 重放 rate / on_view。
fn build_with_ops(order: &[Op]) -> Engine {
    let mut e = Engine::with_seed(cfg(), 7);
    // 编号先固定死：目标 0..8、来源 9..14。换序测试**不动**这一步，
    // 原因见下面 `rate_and_on_view_commute_after_intern` 的限制说明。
    for i in 1..=TARGETS {
        e.intern(key(i));
    }
    let sources: Vec<u32> = (0..SOURCES).map(|i| e.intern(key(200 + i))).collect();
    for op in order {
        let owner = sources[op.src as usize];
        if op.kind == 0 {
            e.rate(owner, RATINGS[op.src as usize], 0);
        } else {
            e.on_view(owner, view_of_source(op.src as usize), 0, ON_VIEW_AT[op.src as usize]);
        }
    }
    e
}

fn signature(e: &Engine) -> Signature {
    Signature {
        q0: query_bits(e, 0),
        q100: query_bits(e, 100),
        dist0: dist_bits(e, 0),
        dist100: dist_bits(e, 100),
        encoded: e.export_view(100).encode(),
    }
}

/// 确定性的 xorshift64：换序表本身也必须可复现，不能借用系统随机源。
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn fisher_yates(order: &mut [Op], state: &mut u64) {
    for i in (1..order.len()).rev() {
        let j = (xorshift(state) % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
}

/// 全部 rate 在前、全部 on_view 在后：这是「同一个最终状态」的基准顺序。
fn canonical_order() -> Vec<Op> {
    let mut order: Vec<Op> = (0..SOURCES).map(|src| Op { src, kind: 0 }).collect();
    order.extend((0..SOURCES).map(|src| Op { src, kind: 1 }));
    order
}

/// **它在防什么回归**：`Engine` 的很多状态是「集合」，但集合是**逐个插入**的：
/// 观点缓存是 `BTreeMap`（按编号升序）、正分清单也是 `BTreeMap`。只要有人让
/// 某个量的结果取决于「谁先插入」（按插入顺序累加 f32、按到达顺序截断、
/// 用「最后一条观点」覆盖），换操作顺序就会改结果。
///
/// **改动前会怎样变红**：把 [`RATINGS`] / [`ON_VIEW_AT`] 的施加顺序打乱后，
/// [`Signature`] 的任一字段变化即红 —— 本场景下实测正序 / 逆序 / 交错 /
/// 8 组确定性随机（共 11 种顺序）全部**逐位相同（0 ULP）**，包括
/// `export_view().encode()` 的 10240 字节。
///
/// # 本测试**不覆盖 intern 换序**（刻意如此，不是遗漏）
///
/// 审计 A13-07：`Engine::on_view` 求 `total_mass` 时按**编号升序**遍历
/// `0..accounts.len()` 累加 f32，而编号由 intern 顺序决定；f32 加法不满足
/// 结合律，所以「重排 intern 顺序」会让 `total_mass` 末位漂移，进而让 query
/// 差 1~3 ULP —— 本场景实测 64 种 intern 排列里 61 种有差异、最大 2 ULP，
/// 审计 A1-engine D11 在 2000 个场景上测得 43% 不同（最大 1.19e-7）。
/// 这是**已知且未修复**的缺陷，不属于本护栏要钉的 S6 范围。
///
/// 想覆盖它，必须先修 `total_mass` 的累加口径（f64 累加 + 按 key 排序），
/// 而不是把这条测试改成「接受 ULP 级差异」—— 否则真正的顺序回归会被噪声掩盖。
#[test]
fn rate_and_on_view_commute_after_intern() {
    // 基准顺序：先全部 rate、再全部 on_view。它必须复现 ① 的实测位表，
    // 否则「换序基准」本身就跑偏了。
    let base = signature(&build_with_ops(&canonical_order()));
    assert_eq!(base.q0, BITS_AT_0, "基准顺序必须复现 ① 的 query@0 位表");
    assert_eq!(
        base.q100, BITS_AT_100,
        "基准顺序必须复现 ① 的 query@100 位表"
    );
    assert_eq!(base.dist100, DIST_AT_100, "基准顺序必须复现 ① 的分布位表");

    // 逆序：on_view 逆序在前，rate 逆序在后（连「同一个来源的两步」都反了）。
    let mut reversed: Vec<Op> = (0..SOURCES)
        .rev()
        .map(|src| Op { src, kind: 1 })
        .collect();
    reversed.extend((0..SOURCES).rev().map(|src| Op { src, kind: 0 }));
    assert_eq!(
        signature(&build_with_ops(&reversed)),
        base,
        "逆序（先 on_view 后 rate）必须逐位相同"
    );

    // 交错：rate(s_i) 紧跟 on_view(s_i)。
    let mut interleaved: Vec<Op> = Vec::new();
    for src in 0..SOURCES {
        interleaved.push(Op { src, kind: 0 });
        interleaved.push(Op { src, kind: 1 });
    }
    assert_eq!(
        signature(&build_with_ops(&interleaved)),
        base,
        "交错（打分与收观点交替）必须逐位相同"
    );

    // 8 组确定性随机顺序。
    for trial in 0..8u64 {
        let mut order = canonical_order();
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (trial + 1).wrapping_mul(0xDEAD_BEEF);
        fisher_yates(&mut order, &mut state);
        assert_eq!(
            signature(&build_with_ops(&order)),
            base,
            "第 {trial} 组随机顺序（{order:?}）必须逐位相同"
        );
    }
}

// ===========================================================================
// ③ mass_covers 双向边界
// ===========================================================================

/// 建一个两来源引擎（两个来源各 1.0 分），只登记前 `known` 个目标，
/// 投递一份**只声明前 `declared` 个目标**的观点，然后再登记剩下的目标。
///
/// `declared` 与 `known` 的不同组合就是 mass_covers 的两侧：
/// 声明了但当时没登记（会被边界挡住）／声明了且当时已登记（正常计入分母）。
fn build_mass_scene(declared: usize, known: usize) -> Engine {
    let mut e = Engine::with_seed(cfg(), 7);
    let s0 = e.intern(key(200));
    let s1 = e.intern(key(201));
    e.rate(s0, 1.0, 0);
    e.rate(s1, 1.0, 0);
    for i in 1..=known as u32 {
        e.intern(key(i));
    }
    let pairs: Vec<([u8; 32], f32)> = (1..=declared as u32).map(|i| (key(i), 1.0)).collect();
    let view = CompressedView::from_pairs(params(), &pairs);
    e.on_view(s0, view.clone(), 0, 0);
    e.on_view(s1, view, 0, 0);
    for i in (known as u32 + 1)..=6u32 {
        e.intern(key(i));
    }
    e
}

/// 按**编号**查询。本场景里目标 key i 的编号恒为 i + 1（两个来源占了 0、1）。
fn target_bits(e: &Engine, first: u32, last: u32) -> Vec<u32> {
    (first..=last).map(|i| e.query(i, 0).to_bits()).collect()
}

/// **它在防什么回归**：`on_view` 会把「此刻已知账户数」记进
/// `StoredView::mass_covers`，`query` 对 `target >= mass_covers` 的账户一律按
/// 「这份观点没表态」处理。少了这道守卫，`total_mass` 只统计 on_view 时刻的
/// 少量账户（分母偏小），后登记的账户读数会被抬到 1.0 —— 实测 50 条声明
/// ⇒ Σv = 50（合规应 ≤ 1，见 `p0_core.rs::late_interned_accounts_are_not_endorsed`）。
/// 反过来，若把守卫改成「永远覆盖」或「按当前 account_count 重算」，同一份
/// 输入在不同登记顺序下就会得到不同结果（审计 A13-07 的 0.7 vs 0.35）。
///
/// **改动前会怎样变红**：
/// * 去掉边界（后登记账户照样参与归一化）：本测试 A 段里「视图声明了、但在
///   on_view 之后才登记」的 key2 / key3 会读到 0.35，而不是恒定的 `0x0`；
///   B 段变体 2 里那 6-K 个被声明却后登记的目标同样会变成非零
///   （它们每人拿 0.7/K，Σv 从 1 份预算涨到 6/K 份）；
/// * mass_covers 记错（比如记成 on_view **之后**的 account_count，或改成
///   「永远覆盖」）：表现同上 —— 变体 2 的逐位等价断言立刻不等；
/// * 「精确等于」这个断言本身**读不到字段**：`StoredView.mass_covers` 虽是 pub
///   字段，但 `Engine::views` 私有、没有公开访问器（本测试不去改
///   `crates/engine/src/`）。所以这里用两条**行为**断言钉住它：
///   ① 与「参考引擎」逐位等价；② 编号 >= on_view 时刻 account_count ⇒ bits `0x0`。
#[test]
fn mass_covers_boundary_matches_reference_engine() {
    // ---- A. 两次 on_view 的边界：声明了但当时未登记 vs 已登记 ----
    let mut e = Engine::with_seed(cfg(), 7);
    let s0 = e.intern(key(200));
    let s1 = e.intern(key(201));
    e.rate(s0, 1.0, 0);
    e.rate(s1, 1.0, 0);
    let k1 = e.intern(key(1));
    assert_eq!(k1, 2);
    assert_eq!(e.account_count(), 3, "on_view 前恰好已知 3 个账户");

    // 观点 A 同时声明 key1（已登记）与 key2（尚未登记）。
    // total_mass 只累加**已知**账户 ⇒ = 1.0；两个来源里只有 s0 投了票
    // ⇒ group = 1.0/2 = 0.5，query = 0.7 × 0.5 = 0.35（bits 0x3eb33333）。
    let view_a = CompressedView::from_pairs(params(), &[(key(1), 1.0), (key(2), 1.0)]);
    e.on_view(s0, view_a, 0, 0);
    assert_eq!(
        e.query(k1, 0).to_bits(),
        0x3EB3_3333,
        "account_count = 3 时被覆盖的目标应读到 0.35"
    );

    // key2 现在才登记：观点 A 明明声明过它，但它在 on_view 之后，
    // 没有进过 total_mass ⇒ 必须按「这份观点没表态」处理。
    let t1 = e.intern(key(2));
    assert_eq!(t1, 3);
    assert_eq!(e.account_count(), 4);
    assert_eq!(
        e.query(t1, 0).to_bits(),
        0x0,
        "视图中声明过、但 on_view 之后才登记的账户必须恒为 bits 0x0"
    );

    // 观点 B（另一个来源）声明 key2：这次它已在 on_view 前登记，正常计入。
    let view_b = CompressedView::from_pairs(params(), &[(key(2), 1.0)]);
    e.on_view(s1, view_b, 0, 0);
    assert_eq!(
        e.query(k1, 0).to_bits(),
        0x3EB3_3333,
        "第一次视图下 key1 的 0.35 必须保持不变"
    );
    assert_eq!(
        e.query(t1, 0).to_bits(),
        0x3EB3_3333,
        "第二次 on_view 之后 key2 才拿到 0.35"
    );

    // 实测分布：两个来源各 0.15（只有个人项），两个被覆盖目标各 0.35。
    assert_eq!(
        dist_bits(&e, 0),
        vec![
            (0, 1_041_865_114),
            (1, 1_041_865_114),
            (2, 0x3EB3_3333),
            (3, 0x3EB3_3333)
        ],
        "两次 on_view + 交错登记之后的分布位表"
    );
    assert_eq!(e.stats(), (0, 2, 2, 2), "两份观点都进缓存、两条评分都在");

    // 第二次 on_view 之后才 intern 的目标：两份观点都没覆盖它，恒为 0。
    let t2 = e.intern(key(3));
    assert_eq!(t2, 4);
    assert_eq!(
        e.query(t2, 0).to_bits(),
        0x0,
        "第二次 on_view 之后才登记的账户必须恒为 bits 0x0"
    );

    // ---- B. K = 1..6：与「参考引擎」逐位等价 ----
    //
    // 参考引擎 = 「同样 K 个目标在 on_view 时已存在」：视图只声明 K 个目标，
    // 而 6 个目标在 on_view 前全部登记完毕。它的实测位表见 EXPECTED：
    // 前 K 个各拿 0.7/K（total_mass 恰为 K），其余 0x0。
    //
    // 两个待测变体都必须与它逐位相同：
    //
    // * 变体 1「先 on_view 后 intern」：视图只声明 K 个目标，on_view 时只登记了
    //   这 K 个，其余 6-K 个在 on_view 之后才 intern。它验证的是「当时还不认识
    //   的账户对 total_mass 贡献 0」—— 注意这个变体**踩不到** mass_covers 守卫：
    //   所有被声明目标的编号都 < mass_covers，守卫对它是空操作。
    //
    // * 变体 2「声明了但当时未登记」才是真正踩到守卫的那一侧：视图**声明
    //   全部 6 个**目标，on_view 时只登记了前 K 个，其余在 on_view 之后才 intern。
    //   total_mass 只累加当时已知的 K 个（每个 1.0）⇒ 前 K 个各拿 0.7/K；
    //   后登记的 6-K 个虽然被观点声明过，但编号 >= mass_covers ⇒ 必须恒为 0x0。
    //   去掉守卫时这 6-K 个会按 `1.0 / total_mass` 被抬到 0.7/K ——
    //   Σv 从 1 份预算涨到 6/K 份（K < 6 时 > 1），这正是要防的回归。
    const EXPECTED: [[u32; 6]; 6] = [
        [1060320051, 0, 0, 0, 0, 0],
        [1051931443, 1051931443, 0, 0, 0, 0],
        [1047457519, 1047457519, 1047457519, 0, 0, 0],
        [1043542835, 1043542835, 1043542835, 1043542835, 0, 0],
        [
            1041194025, 1041194025, 1041194025, 1041194025, 1041194025, 0,
        ],
        [
            1039068911, 1039068911, 1039068911, 1039068911, 1039068911, 1039068911,
        ],
    ];

    for k in 1..=6usize {
        let reference = build_mass_scene(k, 6);
        let reference_bits = target_bits(&reference, 2, 7);
        assert_eq!(
            reference_bits,
            EXPECTED[k - 1],
            "K = {k}：参考引擎的位表不匹配（被覆盖目标应各拿 0.7/K）"
        );
        assert_eq!(
            reference.account_count(),
            8,
            "参考引擎在 on_view 时确实认识全部 6 个目标和 2 个来源"
        );

        // 变体 1：视图只声明 K 个，其余目标在 on_view 之后才登记。
        let early_view = build_mass_scene(k, k);
        assert_eq!(
            target_bits(&early_view, 2, 7),
            reference_bits,
            "K = {k} 变体 1：先 on_view 后 intern，必须与参考引擎逐位相同"
        );

        // 变体 2：视图声明全部 6 个，但只有前 K 个在 on_view 时已登记。
        let late_view = build_mass_scene(6, k);
        assert_eq!(
            target_bits(&late_view, 2, 7),
            reference_bits,
            "K = {k} 变体 2：声明了但当时未登记的目标，必须与参考引擎逐位相同"
        );
        // 边界：编号 >= on_view 时刻的 account_count（= 2 + K）一律 bits 0x0。
        // 两个变体各验一遍（变体 2 里这些编号的视图读数明明非零）。
        for idx in (k as u32 + 2)..8 {
            assert_eq!(
                early_view.query(idx, 0).to_bits(),
                0x0,
                "K = {k} 变体 1：编号 {idx} >= account_count_at_view 必须恒为 bits 0x0"
            );
            assert_eq!(
                late_view.query(idx, 0).to_bits(),
                0x0,
                "K = {k} 变体 2：观点声明过、但编号 {idx} >= account_count_at_view 必须恒为 0"
            );
        }

        // 预算：K 个被覆盖目标的群体项总量恒为「一份观点、一份预算」= 1。
        for (name, e) in [("变体 1", &early_view), ("变体 2", &late_view)] {
            let covered: f32 = (2..2 + k as u32).map(|idx| e.query(idx, 0) / 0.7).sum();
            assert!(
                (covered - 1.0).abs() <= 1e-5,
                "K = {k} {name}：K 个被覆盖目标的群体项总量应等于 1，实际 {covered}"
            );
        }
    }
}
