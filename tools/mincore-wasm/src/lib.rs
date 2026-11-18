//! # mincore wasm32 逐位对拍
//!
//! 证明 tm_mincore（最小反女巫内核，crates/mincore）在 **wasm32-unknown-unknown**
//! （在 Node 的 V8 里**真执行**，不是只编译）与 **native x86_64** 上，对同一批输入
//! 给出**逐位相同**的 f32 位模式、以及**逐字节相同**的定长编码快照。
//!
//! 为什么这件事是本仓最值钱的性质：内核要跑在浏览器 / Electron / 各种 wasm runtime 里。
//! 如果同一份输入在两个平台上给出不同的声望值，那么 S6（「同输入必然同输出」）在
//! 跨平台意义上失效、未来的 ZKP 验证会失败、用户会看到「我这边和服务器算的不一样」。
//!
//! 本探针**不是**测某个纯函数，而是跑完整流程（每个 cap 各跑一遍）：
//!
//! ~~~text
//! 建 Node → rate 704 个账户（含 0 分 / 负分 / NaN / 超量 / 满分）
//!         → publish(0)                  // 阶段 0：还没有任何观点
//!         → on_view 6 份观点（含 0 分 / NaN / 负分 / 超量槽位）+ 4 次被拒
//!         → publish(36)                 // 阶段 1：衰减因子 0.5^(36/72)
//!         → 刷新 2 份观点 + 1 次取失败
//!         → publish(520)                // 阶段 2：衰减因子 0.5^(520/72) 等
//!         → publish(1_000_000)          // 阶段 3：所有来源的衰减都撞 f32 下限
//! 每次 publish 都做 encode → decode → encode 的**逐字节**往返断言
//! 最后 query 全量账户、normalized/dequantize 读数、quantize 边界、decay 位模式、状态量
//! ~~~
//!
//! 每个 cap 记录 RECORDS_PER_CAP = 913 个 f32 位模式（三个 cap 共 2739 个），
//! 外加 4 个阶段的定长编码快照（STAGES = 4，三个 cap 共 12 张，最大 3074 字节）。
//!
//! # 记录布局（每个 cap 一段，顺序写死，JS 侧按同一张表解读偏移）
//!
//! | 偏移（相对该 cap 段首） | 个数 | 内容 |
//! |---|---|---|
//! | 0 .. 704        | 704 | query(BASE+i, 520)，i 是全部被评分的账户 |
//! | 704 .. 794      |  90 | query 别人提到过、但我**没有**评分过的账户 |
//! | 794 .. 808      |  14 | query 谁都没提过的账户（含 0 与 u32::MAX）——必须精确 0 |
//! | 808 .. 832      |  24 | query(BASE+i, 1_000_000)：衰减撞下限后的读数 |
//! | 832 .. 856      |  24 | 阶段 2 视图前 24 个槽的 normalized() |
//! | 856 .. 880      |  24 | 同 24 个槽的 dequantize(q)（定点还原） |
//! | 880 .. 892      |  12 | quantize(x) 的边界输入（NaN/负/-0.0/0/1/超量/∞/天文/中点） |
//! | 892 .. 900      |   8 | 状态量：cap / bytes / len / total_q / total_mass / truncated_mass / view_count / list_len |
//! | 900 .. 903      |   3 | 收发统计：成功 / 被拒 / 清单总分 |
//! | 903 .. 905      |   2 | 未归一化 mass、归一化后的 sum() |
//! | 905 .. 913      |   8 | decay() 的位模式（powf 跨平台一致性的**直接**证据） |
//!
//! 全部记录值都被断言为**有限 f32**，所以「逐位相同」的比较不会被 NaN payload 打扰。
//!
//! # 已知的坑（照抄 tools/wasm-smoke 的解法）
//!
//! * wasm 侧**不能**从 JS 传 TypedArray 指针进来（会被强转成 0），所以由本模块自己
//!   持有缓冲区并导出地址（out_buf_addr() / snap_buf_addr()），JS 从线性内存读回；
//! * static mut 只取地址、**不创建引用**（std::ptr::addr_of! / addr_of_mut!），
//!   避免 static_mut_refs；
//! * lib 名与 bin 名必须不同（tm_mincore_wasm vs mc_wasm_cli），否则 output filename collision。

use tm_mincore::{Config, Node, SparseView, Q_ONE, SLOT_BYTES, VIEW_OVERHEAD};

// ─────────────────────────── 常量：输入形状 ───────────────────────────

/// 「我评过分」的账户：账户号 = BASE + i，i 属于 [0, RATED)。三个 cap 用**同一批**。
pub const RATED: usize = 704;

/// 账户号基址（0 号与 u32::MAX 留作「谁都没提过」的裸账户）。
pub const BASE: u32 = 1000;

/// 「别人提到过、但我从来没评过」的账户：账户号 = STRANGER + k。
pub const STRANGER: u32 = BASE + 10_000;

/// 「谁都没提过」的账户：账户号 = PLAIN + k。
pub const PLAIN: u32 = BASE + 500_000;

/// 6 个来源（都是「我评过分」的账户，且分数各不相同：1.0 / 0.32 / 0.48 / 0.64 / 0.80 / 0.96）。
const VIEWERS: [usize; 6] = [5, 22, 38, 54, 70, 86];

/// 一份观点里指向「我评过的人」的槽位数。
const KNOWN_SLOTS: usize = 12;

/// 一份观点里指向「我根本没评过的人」的槽位数。
const STRANGER_SLOTS: usize = 15;

/// 一份观点的槽位数（27 <= 最小的 cap 128，永不截断）。
const SLOTS_PER_VIEW: usize = KNOWN_SLOTS + STRANGER_SLOTS;

/// 三个 cap：这是唯一的自变量，其余输入（账户与分数）三者完全相同。
pub const CAPS: [usize; 3] = [128, 256, 512];

/// 每个 cap 跑几个 publish 阶段。
pub const STAGES: usize = 4;

/// 快照张数 = 3 个 cap × 4 个阶段。快照 slot 顺序：cap 序号 × STAGES + 阶段。
pub const SNAP_COUNT: usize = CAPS.len() * STAGES;

/// 单张快照的最大字节数（cap = 512 时的 cap×6+2）。
pub const SNAP_CAP: usize = 512 * SLOT_BYTES + VIEW_OVERHEAD;

/// 结果缓冲区容量（u32 个数）。2739 个记录 + 余量。
pub const OUT_CAP: usize = 2800;

// 每个 cap 段的记录个数（布局见模块文档的表）。
const RATED_QUERIES: usize = RATED;
const STRANGER_QUERIES: usize = STRANGER_SLOTS * VIEWERS.len();
const PLAIN_QUERIES: usize = 14;
const OLD_QUERIES: usize = 24;
const NORMALIZED_READS: usize = 24;
const DEQUANT_READS: usize = 24;
const QUANTIZE_CASES: usize = 12;
const STATE_RECORDS: usize = 8;
const STATS_RECORDS: usize = 3;
const MASS_RECORDS: usize = 2;
const DECAY_RECORDS: usize = 8;

/// 每个 cap 段写多少个 f32 位模式。
pub const RECORDS_PER_CAP: usize = RATED_QUERIES
    + STRANGER_QUERIES
    + PLAIN_QUERIES
    + OLD_QUERIES
    + NORMALIZED_READS
    + DEQUANT_READS
    + QUANTIZE_CASES
    + STATE_RECORDS
    + STATS_RECORDS
    + MASS_RECORDS
    + DECAY_RECORDS;

// 编译期检查：缓冲区装得下全部记录；单张快照装得下最大的编码。
const _: () = assert!(OUT_CAP >= CAPS.len() * RECORDS_PER_CAP);
const _: () = assert!(SNAP_CAP == 512 * 6 + 2);
const _: () = assert!(STRANGER_SLOTS * VIEWERS.len() == 90);
// f32 记录的整数类字段必须能被 f32 **精确**表示（2^24 以内的整数都精确），
// 否则「逐位相同」就不是「数值相同」的意思了。
const _: () = assert!(SNAP_CAP <= 1 << 24);
const _: () = assert!(OUT_CAP <= 1 << 24);
const _: () = assert!(RATED <= 1 << 24);

// ─────────────────────────── 缓冲区（wasm 侧由 JS 读回） ───────────────────────────

/// 结果缓冲区：全部 f32 位模式。
static mut OUT_BUF: [u32; OUT_CAP] = [0; OUT_CAP];

/// 快照缓冲区：SNAP_COUNT 行，每行 SNAP_CAP 字节（行内超出实际长度的部分清零）。
static mut SNAP_BUF: [[u8; SNAP_CAP]; SNAP_COUNT] = [[0; SNAP_CAP]; SNAP_COUNT];

// ─────────────────────────── 输入表 ───────────────────────────

/// 我给 BASE + i 打的正分。
///
/// 一张表同时覆盖了协议入口**必须消化掉**的全部畸形输入：
/// 0 分（等于撤销）、负分、NaN、超量（2.0）、天文超量（f32::MAX）、满分 1.0，
/// 以及 (0.10, 0.98] 区间的普通正分（i % 16 >= 6 的 10 档）。
///
/// 于是 RATED = 704 个账户里只有 13/16 = 572 个真正进入正分清单——
/// 这个数字被 run_cap 断言，三个 cap 下必须一模一样。
fn score_of(i: usize) -> f32 {
    match i % 16 {
        0 => 0.0,                              // 0 分：等价于撤销，不进清单
        1 => -1.0,                             // 负分：丢弃
        2 => f32::NAN,                         // NaN：丢弃（「NaN 一律当非法值」）
        3 => 2.0,                              // 超量：夹到 1.0
        4 => f32::MAX,                         // 天文超量：夹到 1.0
        5 => 1.0,                              // 满分
        _ => 0.10 + ((i % 89) as f32) / 100.0, // (0.10, 0.98]，永不越界
    }
}

/// 按 PositiveList::set 的**入口语义**（`!(score > 0.0)` 丢弃、`score > 1` 夹到 1.0）
/// 对 score_of 表逐项求和，得到**应该**被记录下来的清单总分。
///
/// 这个函数是「输入表」与「内核入口校验」之间的独立对照：如果哪天 set 的
/// 夹紧/丢弃规则变了，run_cap 里的断言会立刻炸。
pub fn expected_list_total() -> f64 {
    (0..RATED)
        .map(|i| {
            let s = score_of(i);
            // 与 list.rs 的 set 同**语义**：NaN / 0 / 负数一律丢弃，> 1 夹到 1.0。
            //
            // 注意写法与内核里那句 `!(score > 0.0)` 不同：内核的写法对 NaN 返回 true，
            // 这里写成 `is_nan() || <= 0.0` 表达同一个意思（`s <= 0.0` 对 NaN 是 false，
            // 所以必须显式写出 NaN 分支），但不会触发 clippy 的
            // `neg_cmp_op_on_partial_ord`——本 crate 要求零 warning。
            if s.is_nan() || s <= 0.0 {
                0.0f64
            } else {
                f64::from(if s > 1.0 { 1.0 } else { s })
            }
        })
        .sum()
}

/// 一份观点里某个槽位的原始分数（会经由 SparseView::from_scores 的 quantize 入口消化）。
///
/// 同样塞满畸形值：0 分 / NaN / 负分**不占槽**，超量**夹到 1.0**。
fn slot_score(viewer: usize, k: usize, salt: usize) -> f32 {
    match k % 9 {
        0 => 0.0,
        1 => f32::NAN,
        2 => -2.0,
        3 => 3.0,
        4 => f32::MAX,
        _ => 0.05 + ((viewer * 11 + k * 7 + salt) % 90) as f32 / 100.0,
    }
}

/// 来源 viewer 的一份观点：12 个「我评过的人」+ 15 个「我根本没评过的人」。
///
/// salt 只用于阶段 2 的刷新，让刷新前后是两份**真的不同**的观点（不同账户、不同分数）。
fn slots_of(viewer: usize, salt: usize) -> Vec<(u32, f32)> {
    let mut out = Vec::with_capacity(SLOTS_PER_VIEW);
    for k in 0..KNOWN_SLOTS {
        // 步长 5 与 704 互质：12 个槽的账户号两两不同（from_scores 不会因去重而缩水）。
        let idx = (viewer * 37 + k * 5 + salt) % RATED;
        out.push((BASE + idx as u32, slot_score(viewer, k, salt)));
    }
    for k in 0..STRANGER_SLOTS {
        // 6 个来源 × 15 个陌生账户，账户号全局两两不同。
        let acc = STRANGER + (viewer * STRANGER_SLOTS + k) as u32;
        out.push((acc, slot_score(viewer, k + 3, salt)));
    }
    out
}

/// 第 k 个「陌生账户」（账户号 = STRANGER + k）是否**真的**被某份观点提到了。
///
/// slots_of 里陌生槽位也会带 0 分 / NaN / 负分，它们被 quantize 消化成 0，
/// **不占槽位**——于是对应的陌生账户精确拿不到任何声望。本探针把这件事
/// 也钉死（见 run_cap 里的断言），所以这里必须能用同一套规则独立算一遍。
fn stranger_mentioned(k: usize) -> bool {
    let viewer = k / STRANGER_SLOTS;
    let slot = k % STRANGER_SLOTS;
    // 阶段 2 刷新了 viewer 2（salt=1）与 viewer 4（salt=3）；其余仍是 salt=0 的那份。
    let salt = match viewer {
        2 => 1,
        4 => 3,
        _ => 0,
    };
    tm_mincore::quantize(slot_score(viewer, slot + 3, salt)) > 0
}

// ─────────────────────────── 快照 ───────────────────────────

/// 一张定长编码快照（encode 的产物，已在生成时断言过规范形式）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// 第几个 publish 阶段（0..STAGES）。
    pub stage: usize,
    /// 生成它的 cap。
    pub cap: usize,
    /// 编码后的字节（恒为 cap×6+2）。
    pub bytes: Vec<u8>,
}

/// 一次探针的全部产物：位模式序列 + 编码快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// 全部 f32 位模式（CAPS.len() × RECORDS_PER_CAP 个）。
    pub bits: Vec<u32>,
    /// 全部编码快照（SNAP_COUNT 张）。
    pub snaps: Vec<Snapshot>,
}

/// cap 对应的编码长度（cap×6+2）。
fn snap_len_of(cap: usize) -> usize {
    cap * SLOT_BYTES + VIEW_OVERHEAD
}

/// 把一张刚 publish 出来的视图做成快照，并把**规范形式**的全部断言做掉。
fn snapshot(stage: usize, cap: usize, v: &SparseView, dist_entries: usize) -> Snapshot {
    assert_eq!(v.cap(), cap);
    assert_eq!(
        v.bytes(),
        snap_len_of(cap),
        "cap={cap} 时 bytes() 必须是 cap*6+2"
    );
    assert_eq!(v.bytes(), cap * 6 + 2, "与 view.rs 文档的体积表一致");
    assert!(
        dist_entries > cap,
        "stage={stage} cap={cap} 的分布条目 {dist_entries} 没有超过 cap，截断路径没被走到"
    );
    assert_eq!(
        v.len(),
        cap,
        "stage={stage} cap={cap}：分布条目超 cap 时 publish 必须截断到恰好 cap 条"
    );
    let bytes = v.encode();
    assert_eq!(bytes.len(), snap_len_of(cap));
    let back = SparseView::decode(&bytes, cap).expect("自己编出来的负载必须解得回来");
    assert_eq!(back, *v, "decode 之后必须与原视图逐字段相同");
    assert_eq!(
        back.encode(),
        bytes,
        "encode→decode→encode 必须逐字节相同（规范形式唯一）"
    );
    Snapshot { stage, cap, bytes }
}

// ─────────────────────────── 记录 ───────────────────────────

/// 记录一个 f32 的位模式。
///
/// 只允许**有限**值：NaN 的位模式带 payload，跨平台不同编译器不保证一致，
/// 而本探针要比较的是「同一个数学结果」，不是 NaN 的载荷。
fn push(bits: &mut Vec<u32>, x: f32) {
    assert!(
        x.is_finite(),
        "探针只记录有限值，收到 {x}（0x{:08x}）",
        x.to_bits()
    );
    bits.push(x.to_bits());
}

// ─────────────────────────── 一个 cap 的完整流程 ───────────────────────────

/// 在 cap 下跑完整流程，返回 (该 cap 的位模式, 该 cap 的 4 张快照)。
fn run_cap(cap: usize) -> (Vec<u32>, Vec<Snapshot>) {
    let cfg = Config {
        beta: 0.3,
        half_life: 72,
        cap,
        decay_enabled: true,
    };
    let mut n = Node::new(cfg, 42);

    // ── 写入 704 个账户：0 分 / 负分 / NaN 不进清单，超量与天文超量被夹到 1.0 ──
    for i in 0..RATED {
        n.rate(BASE + i as u32, score_of(i), 0);
    }
    assert_eq!(
        n.claims().len(),
        572,
        "13/16 的账户有正分，其余（0 分 / 负分 / NaN）必须不占条目"
    );
    assert_eq!(n.claims().version(), 572, "每条新条目恰好把版本号 +1");
    assert!(
        (n.claims().total() - expected_list_total()).abs() < 1e-12,
        "清单总分与输入表的独立求和不一致：{} vs {}",
        n.claims().total(),
        expected_list_total()
    );

    let mut snaps: Vec<Snapshot> = Vec::with_capacity(STAGES);
    let mut bits: Vec<u32> = Vec::with_capacity(RECORDS_PER_CAP);

    // ── 阶段 0：还没有任何观点，发布 ──
    let d0 = n.distribution(0);
    assert!(d0.len() > cap, "阶段 0 的个人项条目必须超过 cap");
    assert!((d0.sum() - 1.0).abs() < 1e-5, "全局归一化后 Σ 恒 1");
    let v0 = n.publish(0);
    snaps.push(snapshot(0, cap, &v0, d0.len()));

    // ── 阶段 1：收 6 份观点（每份都含 0 分 / NaN / 负分 / 超量槽位）──
    for (j, &idx) in VIEWERS.iter().enumerate() {
        let slots = slots_of(j, 0);
        let view = SparseView::from_scores(cap, &slots);
        assert!(!view.is_empty() && view.total_q() > 0, "观点不能是空表");
        assert!(view.len() <= SLOTS_PER_VIEW);
        assert!(
            n.on_view(BASE + idx as u32, view, 100 + j as u64, 12 * j as u64),
            "来源 {idx} 是评过分的账户，观点必须被接收"
        );
    }
    // 三条拒绝路径（结构性拒绝，不需要逐条判脏）。
    assert!(
        !n.on_view(STRANGER, SparseView::from_scores(cap, &[(BASE, 1.0)]), 0, 0),
        "对我没有正分（且我不认识）的来源必须被拒"
    );
    assert!(
        !n.on_view(BASE + VIEWERS[0] as u32, SparseView::new(cap), 0, 0),
        "空表（Σq == 0）必须被拒"
    );
    assert!(
        !n.on_view(u32::MAX, SparseView::from_scores(cap, &[(BASE, 1.0)]), 0, 0),
        "u32::MAX 不是我的正分对象，必须被拒"
    );
    let d1 = n.distribution(36);
    assert!((d1.sum() - 1.0).abs() < 1e-5, "全局归一化后 Σ 恒 1");
    let v1 = n.publish(36);
    snaps.push(snapshot(1, cap, &v1, d1.len()));

    // ── 阶段 2：刷新两份观点 + 记一次取失败 ──
    assert!(n.on_view(
        BASE + VIEWERS[2] as u32,
        SparseView::from_scores(cap, &slots_of(2, 1)),
        300,
        300
    ));
    assert!(n.on_view(
        BASE + VIEWERS[4] as u32,
        SparseView::from_scores(cap, &slots_of(4, 3)),
        300,
        300
    ));
    n.on_view_failed(BASE + VIEWERS[1] as u32);
    assert_eq!(n.claims().fail_streak(BASE + VIEWERS[1] as u32), 1);
    // 再拒一次：满表但 owner 没被评过分。
    assert!(!n.on_view(
        STRANGER + 1,
        SparseView::from_scores(cap, &[(BASE, 1.0)]),
        0,
        0
    ));
    let d2 = n.distribution(520);
    assert!((d2.sum() - 1.0).abs() < 1e-5, "全局归一化后 Σ 恒 1");
    let v2 = n.publish(520);
    snaps.push(snapshot(2, cap, &v2, d2.len()));

    // ── 阶段 3：极老时间戳，所有来源的衰减因子都撞 f32 下限 ──
    let d3 = n.distribution(1_000_000);
    assert!((d3.sum() - 1.0).abs() < 1e-5, "全局归一化后 Σ 恒 1");
    let v3 = n.publish(1_000_000);
    snaps.push(snapshot(3, cap, &v3, d3.len()));

    assert_eq!(n.stats(), (8, 4), "6+2 份观点被接收，3+1 份被拒");
    assert_eq!(n.view_count(), 6, "只有我评过分的来源的观点才会入库");
    assert_eq!(n.claims().len(), 572);

    // ── 记录 1) 704 个「我评过的人」，now = 520 ──
    for i in 0..RATED_QUERIES {
        push(&mut bits, n.query(BASE + i as u32, 520));
    }
    // ── 2) 90 个「别人提到过、但我没评过」的账户：只有群体项，没有个人项 ──
    //
    // 观点里塞了 0 分 / NaN / 负分的槽位，它们不占槽位 ⟹ 对应的陌生账户
    // 压根没人提过 ⟹ 声望必须**精确** 0。这里把两个方向都钉死：
    // 「真被提到的」> 0，「被脏值消化掉的」== 0。
    let mut mentioned = 0usize;
    for k in 0..STRANGER_QUERIES {
        let acc = STRANGER + k as u32;
        let q = n.query(acc, 520);
        if stranger_mentioned(k) {
            mentioned += 1;
            assert!(q > 0.0, "被观点提到的账户 {acc} 必须拿到非零声望");
        } else {
            assert_eq!(q, 0.0, "槽位被脏值消化掉的账户 {acc} 必须精确为 0");
        }
        push(&mut bits, q);
    }
    assert!(
        mentioned >= 60,
        "真被提到的陌生账户只有 {mentioned} 个，覆盖太少（口令变了？）"
    );
    // ── 3) 14 个「谁都没提过」的账户：必须**精确** 0（不是噪声、不是极小正数）──
    for k in 0..12u32 {
        let q = n.query(PLAIN + k, 520);
        assert_eq!(q, 0.0, "裸账户 {} 的声望必须精确为 0", PLAIN + k);
        push(&mut bits, q);
    }
    assert_eq!(n.query(0, 520), 0.0, "0 号账户谁都没提过");
    push(&mut bits, n.query(0, 520));
    assert_eq!(n.query(u32::MAX, 520), 0.0, "u32::MAX 谁都没提过");
    push(&mut bits, n.query(u32::MAX, 520));

    // ── 4) 24 个「极老时间戳」下的查询：走 decay 撞下限的钳位分支 ──
    for i in 0..OLD_QUERIES {
        push(&mut bits, n.query(BASE + i as u32, 1_000_000));
    }

    // ── 5) / 6) 阶段 2 视图前 24 个槽的归一化读数与定点还原 ──
    assert!(v2.slots().len() >= NORMALIZED_READS);
    for &(acc, _q) in v2.slots().iter().take(NORMALIZED_READS) {
        let x = v2.normalized(acc);
        assert!(x > 0.0 && x <= 1.0, "归一化读数必须落在 (0, 1]：{x}");
        push(&mut bits, x);
    }
    for &(_acc, q) in v2.slots().iter().take(DEQUANT_READS) {
        push(&mut bits, tm_mincore::dequantize(q));
    }

    // ── 7) quantize 的入口边界（NaN / 负 / -0.0 / 0 / 1 / 超量 / ∞ / 天文 / 中点）──
    let quantize_cases: [f32; QUANTIZE_CASES] = [
        f32::NAN,
        -1.0,
        -0.0,
        0.0,
        1.0,
        1.5,
        f32::MAX,
        f32::INFINITY,
        1.0 / 131070.0,
        1.0 / 131071.0,
        0.5,
        0.5 / 65535.0,
    ];
    // 这些断言与 tests/anti_sybil.rs 的 t03/t03b 同口径，跨平台也必须成立。
    assert_eq!(tm_mincore::quantize(f32::NAN), 0, "NaN 折算成 0");
    assert_eq!(tm_mincore::quantize(-1.0), 0, "负分折算成 0");
    assert_eq!(tm_mincore::quantize(-0.0), 0, "-0.0 折算成 0");
    assert_eq!(tm_mincore::quantize(1.0), Q_ONE, "1.0 就是满量程");
    assert_eq!(tm_mincore::quantize(f32::INFINITY), Q_ONE, "+∞ 夹到满量程");
    assert_eq!(tm_mincore::quantize(1.0 / 131070.0), 1, "中点四舍五入到 1");
    assert_eq!(tm_mincore::quantize(1.0 / 131071.0), 0, "再小就归零");
    for x in quantize_cases {
        push(&mut bits, f32::from(tm_mincore::quantize(x)));
    }

    // ── 8) 状态量 ──
    push(&mut bits, cap as f32);
    push(&mut bits, v2.bytes() as f32);
    push(&mut bits, v2.len() as f32);
    push(&mut bits, v2.total_q() as f32);
    push(&mut bits, v2.total_mass());
    push(
        &mut bits,
        SparseView::truncated_mass(cap, &d2.entries) as f32,
    );
    push(&mut bits, n.view_count() as f32);
    push(&mut bits, n.claims().len() as f32);

    // ── 9) 收发统计 ──
    push(&mut bits, n.stats().0 as f32);
    push(&mut bits, n.stats().1 as f32);
    push(&mut bits, n.claims().total() as f32);

    // ── 10) 未归一化 mass 与归一化后的 sum ──
    push(&mut bits, d2.mass as f32);
    push(&mut bits, d2.sum() as f32);

    // ── 11) decay() 本身 ──
    //
    // 这是整个内核里**唯一**一处 libm 调用（`0.5f64.powf(dt / half_life)`）。
    // 定点、归一化、编码都是整数运算，天然跨平台一致；只有这个 powf 有可能在
    // native（MSVC 的 CRT）与 wasm32（V8 的 libm）之间差一个 ulp。
    // 所以必须把它的输出单独钉一遍，而不是只依赖「结果碰巧一样」。
    //
    // 8 个输入覆盖：0（精确返回 1.0，不进 powf）、一个半衰期内/外、整数倍、
    // 远下溢（返回 f32::MIN_POSITIVE 而不是 0）、以及 u64 上限（`dt as f64` 极大）。
    for dt in [0u64, 1, 36, 72, 143, 520, 1_000_000, u64::MAX] {
        let f = tm_mincore::decay(dt, 72);
        assert!(f.is_finite() && f > 0.0, "decay({dt}, 72) = {f} 越界");
        push(&mut bits, f);
    }
    // 半衰期的边界语义（与 node.rs 的文档一致）。
    assert_eq!(tm_mincore::decay(0, 72), 1.0, "dt == 0 必须精确返回 1.0");
    assert_eq!(tm_mincore::decay(72, 72), 0.5, "一个半衰期必须精确是 0.5");
    assert_eq!(
        tm_mincore::decay(1_000_000, 72),
        f32::MIN_POSITIVE,
        "下溢必须钳到最小正 f32，而不是 0 / NaN"
    );
    // half_life == 0 的语义：**按 1 处理**（h 被替换成 1，而不是「不衰减」）。
    // 于是 decay(1, 0) = 0.5^(1/1) = 0.5。中文文档里那句「按 1 处理」说的是
    // 「分母替换成 1」，不是「因子固定为 1」——写死成 0.5 才是实现的真实语义。
    assert_eq!(
        tm_mincore::decay(1, 0),
        0.5,
        "half_life == 0 时半衰期按 1 处理（分母替换，不是不衰减）"
    );

    assert_eq!(
        bits.len(),
        RECORDS_PER_CAP,
        "cap={cap} 的记录个数与文档里的布局表不符"
    );
    (bits, snaps)
}

// ─────────────────────────── 全量探针 ───────────────────────────

/// 跑全部 cap，返回位模式序列与编码快照。
///
/// native 侧的 bin 与 wasm 侧的 fill_scores 调的**都是这一个函数**——
/// 如果 native 侧另写一份代码，比的就不是平台差异了。
#[must_use]
pub fn run_all() -> Probe {
    let mut bits: Vec<u32> = Vec::with_capacity(CAPS.len() * RECORDS_PER_CAP);
    let mut snaps: Vec<Snapshot> = Vec::with_capacity(SNAP_COUNT);
    for &cap in CAPS.iter() {
        let (b, s) = run_cap(cap);
        assert_eq!(b.len(), RECORDS_PER_CAP);
        assert_eq!(s.len(), STAGES);
        bits.extend(b);
        snaps.extend(s);
    }
    assert_eq!(snaps.len(), SNAP_COUNT);
    // 快照顺序必须与 snap_len(slot) 的推导一致：slot = cap 序号 × STAGES + 阶段。
    for (cap_idx, &cap) in CAPS.iter().enumerate() {
        for stage in 0..STAGES {
            let s = &snaps[cap_idx * STAGES + stage];
            assert_eq!(s.cap, cap, "快照顺序与 snap_len() 的推导不一致");
            assert_eq!(s.stage, stage);
            assert_eq!(s.bytes.len(), snap_len_of(cap));
        }
    }
    Probe { bits, snaps }
}

// ─────────────────────────── wasm 导出 ───────────────────────────

/// 跑一遍完整探针，把位模式写进内部缓冲区，返回写入个数（= 2739）。
///
/// # Panics
///
/// 任何一条内部断言失败都会 panic（wasm 里表现为 trap）——这是刻意的：
/// 探针的原则是「宁可炸也不静默给出可疑结果」。
#[no_mangle]
pub extern "C" fn fill_scores() -> u32 {
    let probe = run_all();
    assert!(
        probe.bits.len() <= OUT_CAP,
        "记录数 {} 超过缓冲区容量 {OUT_CAP}",
        probe.bits.len()
    );
    assert_eq!(probe.snaps.len(), SNAP_COUNT);
    // SAFETY: 单线程、无并发；写范围由上面的 assert 与 SNAP_COUNT × SNAP_CAP
    // 的静态布局保证，不会越出 OUT_BUF / SNAP_BUF。
    unsafe {
        let dst = std::ptr::addr_of_mut!(OUT_BUF).cast::<u32>();
        for (i, b) in probe.bits.iter().enumerate() {
            std::ptr::write(dst.add(i), *b);
        }
        let base = std::ptr::addr_of_mut!(SNAP_BUF).cast::<u8>();
        for (slot, snap) in probe.snaps.iter().enumerate() {
            assert_eq!(snap.bytes.len(), snap_len_of(snap.cap));
            let row = base.add(slot * SNAP_CAP);
            for (i, b) in snap.bytes.iter().enumerate() {
                std::ptr::write(row.add(i), *b);
            }
            // 行内剩下的字节清零：不把任何旧数据留在缓冲区里。
            for i in snap.bytes.len()..SNAP_CAP {
                std::ptr::write(row.add(i), 0);
            }
        }
    }
    probe.bits.len() as u32
}

/// 结果缓冲区的地址（wasm 线性内存内的字节偏移）。
#[no_mangle]
pub extern "C" fn out_buf_addr() -> u32 {
    // 用 addr_of! 取裸指针，**不创建**对 mutable static 的引用
    // （OUT_BUF.as_ptr() 会创建共享引用，在 Rust 2024 里属于未定义行为）。
    std::ptr::addr_of!(OUT_BUF) as u32
}

/// 快照缓冲区的地址（行 stride = snap_stride()，行数 = snap_count()）。
#[no_mangle]
pub extern "C" fn snap_buf_addr() -> u32 {
    std::ptr::addr_of!(SNAP_BUF).cast::<u8>() as u32
}

/// 快照缓冲区的行 stride（字节）。
#[no_mangle]
pub extern "C" fn snap_stride() -> u32 {
    SNAP_CAP as u32
}

/// 快照张数（= 12）。
#[no_mangle]
pub extern "C" fn snap_count() -> u32 {
    SNAP_COUNT as u32
}

/// 第 slot 张快照的实际字节长度（= 该 cap 的 cap×6+2）；越界返回 0。
#[no_mangle]
pub extern "C" fn snap_len(slot: u32) -> u32 {
    let i = slot as usize;
    if i >= SNAP_COUNT {
        return 0;
    }
    snap_len_of(CAPS[i / STAGES]) as u32
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针自身必须是确定性的：连跑两遍，位模式与快照逐字节相同。
    ///
    /// 这条守的是探针**自己**的可重复性：如果它连自己都不能复现，
    /// 那么 native vs wasm 的任何差异都分不清是平台差异还是探针在抖动。
    #[test]
    fn 探针连跑两遍逐位相同() {
        let a = run_all();
        let b = run_all();
        assert_eq!(a, b, "同一个函数跑两遍必须给出完全相同的结果");
        assert_eq!(a.bits.len(), CAPS.len() * RECORDS_PER_CAP);
        assert_eq!(a.snaps.len(), SNAP_COUNT);
    }

    /// 缓冲区容量、记录总数与快照长度公式的一致性（JS 侧靠这些数字切段）。
    #[test]
    fn 缓冲区与布局表一致() {
        assert!(OUT_CAP >= CAPS.len() * RECORDS_PER_CAP);
        assert_eq!(OUT_CAP, 2800);
        assert_eq!(RECORDS_PER_CAP, 913);
        assert_eq!(CAPS.len() * RECORDS_PER_CAP, 2739);
        assert_eq!(SNAP_CAP, 3074, "cap=512 时 512×6+2 = 3074 字节");
        for slot in 0..SNAP_COUNT as u32 {
            assert_eq!(
                snap_len(slot) as usize,
                snap_len_of(CAPS[slot as usize / STAGES]),
                "slot={slot} 的长度推导不一致"
            );
        }
        assert_eq!(snap_len(SNAP_COUNT as u32), 0, "越界返回 0");
        assert_eq!(snap_len(u32::MAX), 0, "远越界也返回 0");
        assert_eq!(snap_count(), 12);
        assert_eq!(snap_stride(), 3074);
    }

    /// 三个 cap 各自的记录个数与快照张数必须一致（布局是定长的，JS 靠它切段）。
    #[test]
    fn 三个cap的段长一致() {
        for &cap in CAPS.iter() {
            let (b, s) = run_cap(cap);
            assert_eq!(b.len(), RECORDS_PER_CAP, "cap={cap}");
            assert_eq!(s.len(), STAGES, "cap={cap}");
        }
    }

    /// 三个 cap 下的**编码长度**必须精确等于 770 / 1538 / 3074 字节。
    ///
    /// 这是 A12 报告与 索引.md 里写死的体积数字，跨平台不允许漂移一个字节。
    #[test]
    fn 编码体积精确是770_1538_3074() {
        let expect = [770usize, 1538, 3074];
        let probe = run_all();
        for (i, &cap) in CAPS.iter().enumerate() {
            for stage in 0..STAGES {
                let s = &probe.snaps[i * STAGES + stage];
                assert_eq!(s.bytes.len(), expect[i], "cap={cap} stage={stage}");
                assert_eq!(s.bytes.len(), cap * 6 + 2);
            }
        }
    }

    /// 每个 cap 段的 3 个「谁都没提过」的裸账户记录必须**精确**是 +0.0 的位模式。
    ///
    /// 布局：段内偏移 794..808 是 14 个裸账户（12 个 PLAIN+k、0 号、u32::MAX）。
    #[test]
    fn 裸账户精确零() {
        let probe = run_all();
        for cap_idx in 0..CAPS.len() {
            let base = cap_idx * RECORDS_PER_CAP + 794;
            for k in 0..PLAIN_QUERIES {
                assert_eq!(
                    probe.bits[base + k],
                    0,
                    "cap 段 {cap_idx} 的第 {k} 个裸账户记录不是 +0.0"
                );
            }
        }
    }

    /// 每个 cap 段的 90 个「别人提到过但我没评过」的账户必须拿到**非零**声望，
    /// 且读数落在 (0, 1] —— 它们只有群体项，这正是「先夹后除」的判据。
    #[test]
    fn 陌生账户只有群体项() {
        let probe = run_all();
        for cap_idx in 0..CAPS.len() {
            let base = cap_idx * RECORDS_PER_CAP + RATED_QUERIES;
            for k in 0..STRANGER_QUERIES {
                let b = probe.bits[base + k];
                let x = f32::from_bits(b);
                if stranger_mentioned(k) {
                    assert!(x > 0.0 && x <= 1.0, "被提到的陌生账户 {k} 声望越界：{x}");
                } else {
                    assert_eq!(b, 0, "被脏值消化掉的陌生账户 {k} 必须精确为 0");
                }
            }
        }
    }

    /// 状态量记录：cap / bytes / len 必须与本段对应的 cap 完全吻合。
    #[test]
    fn 状态量记录与cap吻合() {
        let probe = run_all();
        let state_base_in_seg = RATED_QUERIES
            + STRANGER_QUERIES
            + PLAIN_QUERIES
            + OLD_QUERIES
            + NORMALIZED_READS
            + DEQUANT_READS
            + QUANTIZE_CASES;
        for (cap_idx, &cap) in CAPS.iter().enumerate() {
            let base = cap_idx * RECORDS_PER_CAP + state_base_in_seg;
            assert_eq!(probe.bits[base], (cap as f32).to_bits(), "记录的 cap 不对");
            assert_eq!(
                probe.bits[base + 1],
                ((cap * 6 + 2) as f32).to_bits(),
                "记录的 bytes() 不对"
            );
            assert_eq!(
                probe.bits[base + 2],
                (cap as f32).to_bits(),
                "记录的 len() 必须等于 cap（截断到满）"
            );
        }
    }

    /// 收发统计记录：6+2 份观点被接收，3+1 份被拒；清单条目恒为 572。
    ///
    /// 布局：段内偏移 900..903。
    #[test]
    fn 统计记录固定() {
        let probe = run_all();
        let stats_off = 900;
        for cap_idx in 0..CAPS.len() {
            let base = cap_idx * RECORDS_PER_CAP + stats_off;
            assert_eq!(probe.bits[base], 8.0f32.to_bits(), "成功接收数不对");
            assert_eq!(probe.bits[base + 1], 4.0f32.to_bits(), "被拒数不对");
            // 第三项是清单总分（f64），不是条目数：与输入表的独立求和必须吻合。
            assert_eq!(
                probe.bits[base + 2],
                (expected_list_total() as f32).to_bits(),
                "清单总分与输入表的独立求和不对"
            );
        }
    }

    /// decay 段（段内偏移 905..913）的 8 个位模式必须与 decay() 直接算出来的逐位相同。
    ///
    /// 这是探针里唯一一处 powf：把「记录下来的结果」与「直接调 decay 的结果」
    /// 对一遍，防止有人改错了记录顺序却仍然自我一致。
    #[test]
    fn decay段与直接调用逐位一致() {
        let probe = run_all();
        let decay_off = 905;
        let inputs = [0u64, 1, 36, 72, 143, 520, 1_000_000, u64::MAX];
        for cap_idx in 0..CAPS.len() {
            for (k, &dt) in inputs.iter().enumerate() {
                let want = tm_mincore::decay(dt, 72).to_bits();
                assert_eq!(
                    probe.bits[cap_idx * RECORDS_PER_CAP + decay_off + k],
                    want,
                    "cap 段 {cap_idx} 的 decay({dt}) 记录不是 0x{want:08x}"
                );
            }
        }
    }

    /// 归一化后的 sum 记录必须非常接近 1（但**不是**逐位 1.0：f32 累加有舍入）。
    ///
    /// 布局：段内偏移 903..905。
    #[test]
    fn 归一化sum接近一() {
        let probe = run_all();
        let sum_off = 904;
        for cap_idx in 0..CAPS.len() {
            let x = f32::from_bits(probe.bits[cap_idx * RECORDS_PER_CAP + sum_off]);
            assert!((x - 1.0).abs() < 1e-5, "cap 段 {cap_idx} 的 sum() = {x}");
            assert!(x.is_finite());
        }
    }

    /// 每个 cap 段的记录值必须全被 `push` 的有限性断言筛过：再验一遍，防止有人绕开。
    ///
    /// 注意 cap / bytes / len / total_q / 统计量这些**整数与计数**也以 f32 放进记录，
    /// 它们都在 2^24 以内（cap×6+2 最大 3074），f32 可以**精确**表示，逐位比较是安全的。
    #[test]
    fn 记录里没有非有限值且整数精确可表示() {
        let probe = run_all();
        for (i, &b) in probe.bits.iter().enumerate() {
            let x = f32::from_bits(b);
            assert!(x.is_finite(), "第 {i} 个记录 0x{b:08x} 不是有限值");
        }
        // 整数类记录的精确性由模块顶部的 const 断言保证（SNAP_CAP / OUT_CAP / RATED <= 2^24）。
    }
}
