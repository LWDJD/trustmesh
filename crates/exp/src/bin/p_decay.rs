//! P3 · 衰减的目的改写为「伤害时效」：E1~E4 四个实验（全部走真 tm_engine::Engine）
//!
//! # 背景（直接采信 审计/A4-decay.md 的实测结论）
//!
//! * 衰减对 S1/S2/S4/S6 的净贡献 ≤4%，95% CI 全部跨 0 —— 它不是反女巫机制；
//! * 唯一的一阶效应是「攻击残余」：关掉 0.990 / 72h 0.250（−74.7%）；
//! * 副作用是 S5 陈旧套利：攻击者保持新鲜、诚实来源陈旧 60h 时，单来源上限被放大
//!   1.60×@72h / 3.40×@24h；
//! * A4 自己承认没跑 D1~D4 全矩阵（:377-378），也没有把「伤害时效」这条主张量化成曲线。
//!
//! 作者 2026-11-16 同意把衰减的目的从「防离线账户」改写为「伤害时效」。
//! 本 bin 是这次改写的证据：四个实验各自回答一个问题，参数全部写死在下面的常量里。
//!
//! # 四个实验（参数写死在代码里，报告里逐条抄一遍）
//!
//! 实验 E1：变量 decay_enabled ∈ {on, off} × half_life ∈ {24,72,168,720}，24 个 seed，
//!          2×4×24 = 192 次仿真；输出 S1/S2/S3/S4/ρ 与攻击残余，每条都带配对差值 + 95% CI。
//! 实验 E2：攻击结束后 {6,24,72,168,600}h 的快照，5 个衰减配置 × 16 个 seed；
//!          输出「攻击者残余份额 ÷ 攻击峰值份额」的时间曲线。
//! 实验 E3：诚实来源陈旧 {0,6,12,24,60}h × H∈{24,72,168}，16 个 seed；
//!          输出单来源上限放大倍数，与解析式 7/(1+6·0.5^(age/H)) 对照。
//! 实验 E4：H∈{24,72,168,720} × 刷新周期 T∈{6,12,24}h × 批大小 k∈{6,12,24}，16 个 seed；
//!          输出 ESS / 覆盖率 / frac(fs<0.1) / 「全体变旧」漂移量，
//!          并验证 T·(n/k) ≤ H/2 时 ESS 明显更高。
//!
//! # 口径（与 A4 严格同构，便于交叉核对）
//!
//! * 只用真 runtime：每个账户一个独立 tm_engine::Engine，走
//!   rate / tick / on_view / on_view_failed / query / share_of / trust_distribution /
//!   export_view / view_age / freshness_stats / decay_factor。
//!   本文件不引用 tm_exp::run_case_full。
//! * 离线节点不 tick、不发布；观点年龄真实增长，衰减真实生效。
//! * 三类账户分开计数，绝不二分：诚实 = 既非攻击者（Sleeper/Infiltrator/Endorser/Sybil）
//!   也非收益方（Promoted）。
//! * 编号一律取 world.accounts().len()（World 只增不删）；注入账户从 n0 往后排。
//! * 配对差：同 seed 的（on_H − off），报 mean ± sd 与 95% CI（t 分布，df = n − 1）。
//!
//! # 子命令
//!
//!   p_decay                  全部四个实验（默认）
//!   p_decay stage=e1a        E1 的 S 判据矩阵（2×4×24）
//!   p_decay stage=e1b        E1 的攻击残余（2×4×24）
//!   p_decay stage=e2         E2 伤害时效曲线（5 配置 × 16 seed）
//!   p_decay stage=e3         E3 S5 套利边界（3 H × 5 age × 16 seed）
//!   p_decay stage=e4         E4 半衰期自适应（4 H × 3 T × 3 k × 16 seed）
//!   threads=N                并发线程数（默认 10）
//!   seeds=11,23,...          覆盖默认 seed 列表
//!   pilot                    每个 stage 只跑 2 个 seed 的冒烟自检（不用于报告）
//!
//! 所有随机性都来自固定 seed：世界由 SimConfig::seed 决定，引擎抽样由
//! Engine::new 的固定种子决定；遍历一律按编号升序，线程只并行不同的 (seed, 配置)，
//! 结果按 (seed, 配置) 排序输出，因此并行与否逐位相同。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, Kind, SimConfig, World};
use tm_view::{AccountKey, CompressedView, ViewParams};

// ─────────────────────────────────────────────────────────────────────────
// 固化参数（报告里必须逐条抄一遍；任何「大致」都是错的）
// ─────────────────────────────────────────────────────────────────────────

/// 初始人口。与 A4 / L3 / D1 / D4 一致。
const N_INIT: usize = 300;
/// 每轮抽样批大小。与 A4 / L3 / D1 / D4 一致（也是 E4 的 k=12 臂）。
const SAMPLES: usize = 12;
/// 个人权重 β（必须与 EngineConfig::default() 一致）。
const BETA: f64 = 0.3;
/// 观点草图形状（全网约定；A4 同值）。
const VIEW_BUCKETS: usize = 2048;
const VIEW_HASHERS: usize = 5;
const VIEW_SEED: u64 = 99;
/// E1 场景 A 的潜伏者数量（A4 中间档）。
const N_SLEEPERS: usize = 3;
/// 每个潜伏者注入的女巫数（A4 中间档）：3 × 20 = 60 个注入女巫。
const SYB_PER: usize = 20;
/// 空账户数量（S4）。
const N_EMPTY: usize = 48;
/// 场景 A 的离线标定目标（A4 主表 60%）。
const OFFLINE_TARGET_A: f64 = 0.60;
/// 场景 B（攻击残余）在 t=300（攻击结束）的离线标定目标。
const OFFLINE_TARGET_B: f64 = 0.60;

/// E1 的 24 个 seed。前 16 个与 A4 完全一致，这样 E1B 的前 8 个 seed 可以直接
/// 和 A4 的表 2.5 交叉核对（off 0.9898 / on72 0.2502 / on168 0.6142 / on720 0.9050 / on24 -0.0429）。
const SEEDS24: [u64; 24] = [
    11, 23, 37, 53, 71, 97, 113, 131, 149, 167, 181, 199, 211, 223, 239, 251, 263, 281, 293, 307,
    331, 347, 359, 373,
];
/// E2/E3/E4 的 16 个 seed（= SEEDS24 的前 16 个）。
const SEEDS16: [u64; 16] = [
    11, 23, 37, 53, 71, 97, 113, 131, 149, 167, 181, 199, 211, 223, 239, 251,
];

// ── 场景 A（S 判据）：300 步 / 10 轮 / 每轮 30h；潜伏者 60h 突变（L3 底座） ──
const A_STEPS: usize = 300;
const A_ROUNDS: u64 = 10;
const A_TICK: u64 = 30;
const A_NORMAL_UNTIL: u64 = 60;

// ── 场景 B（攻击残余，D2 口径）：600 步 / 30 轮 / 每轮 20h ──
const B_STEPS: usize = 600;
const B_ROUNDS: u64 = 30;
const B_TICK: u64 = 20;
const B_ATTACK_T: u64 = 120;
const B_DARK_T: u64 = 300;

// ── E2（伤害时效曲线）：900 步 / 150 轮 / 每轮 6h ──
// 用 6h 轮长的理由：任务要求的快照点 {6,24,72,168,600}h 必须精确落在某一轮上，
// 6h 是同时整除这四个点（除不尽的部分靠 600 除外）的最小轮长：
// 6→r51(306h)，24→r54(324h)，72→r62(372h)，168→r78(468h)，600→r150(900h)。
const E2_STEPS: usize = 900;
const E2_TICK: u64 = 6;
const E2_ROUNDS: u64 = 150;
const E2_ATTACK_T: u64 = 120;
const E2_DARK_T: u64 = 300;
/// E2 的快照点（攻击结束之后的小时数）。
const E2_OFFSETS: [u64; 5] = [6, 24, 72, 168, 600];

/// 「攻击落地」的判定门槛（见 run_residue_b 的注释）：
/// 攻击期峰值相对攻击前基线的归一化增量小于它，就认为这次载荷根本没进到观察者的
/// 群体项里（潜伏者先休眠、或观察者那时全离线），残差不参与聚合。
/// 0.05 的取值依据：落地 seed 的增量中位数在 0.15~0.75 之间（见 E1B_ROW），
/// 未落地 seed 的增量在 1e-3 量级，中间隔着两个数量级。
const MIN_ATTACK_GAIN_Q: f64 = 0.05;
const MIN_ATTACK_GAIN_G: f64 = 0.05;

// ── E4（半衰期自适应）：600 步的世界，刷新周期 T 与批大小 k 都是变量 ──
const E4_STEPS: usize = 600;
const E4_H: [u64; 4] = [24, 72, 168, 720];
const E4_T: [u64; 3] = [6, 12, 24];
const E4_K: [usize; 3] = [6, 12, 24];

// ── E3（S5 套利边界）：合成用例，与 A4 的 s5 同构 ──
const S5_H: [u64; 3] = [24, 72, 168];
const S5_AGES: [u64; 5] = [0, 6, 12, 24, 60];
const S5_QUERY_T: u64 = 600;

// ─────────────────────────────────────────────────────────────────────────
// 小工具
// ─────────────────────────────────────────────────────────────────────────

/// 内部编号 → 32 字节账户号。
///
/// 与 A4 逐位相同的构造（只有前 8 字节是编号，不含任何标记字节）：
/// E1B 的攻击残余要能直接和 A4 的表 2.5 对上，账户号换了就等于换了草图哈希布局，
/// 数字就不再可比。
fn key(i: usize) -> AccountKey {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn view_params() -> ViewParams {
    ViewParams {
        buckets: VIEW_BUCKETS,
        hashers: VIEW_HASHERS,
        seed: VIEW_SEED,
    }
}

/// 消融配置。enabled = false 时 half_life 不参与任何计算。
#[derive(Clone, Copy)]
struct Cfg {
    name: &'static str,
    enabled: bool,
    h: u64,
}

/// E1 的 2×4 全矩阵，共 8 个格子。
///
/// off24/off72/off168/off720 在数学上是同一个配置（decay_enabled=false 时
/// decay_factor 恒返回 1.0，半衰期不进任何表达式）。这里仍然跑满 8 格，
/// 是为了让「2×4×N」的格子数真的被跑完，并用 E1_DET 行验证四个 off 格子的
/// 输出 f64 位模式逐位相同 —— A4 当年用 5 格加等价性论证代替，本次改为实测。
const CFGS8: [Cfg; 8] = [
    Cfg {
        name: "on24",
        enabled: true,
        h: 24,
    },
    Cfg {
        name: "on72",
        enabled: true,
        h: 72,
    },
    Cfg {
        name: "on168",
        enabled: true,
        h: 168,
    },
    Cfg {
        name: "on720",
        enabled: true,
        h: 720,
    },
    Cfg {
        name: "off24",
        enabled: false,
        h: 24,
    },
    Cfg {
        name: "off72",
        enabled: false,
        h: 72,
    },
    Cfg {
        name: "off168",
        enabled: false,
        h: 168,
    },
    Cfg {
        name: "off720",
        enabled: false,
        h: 720,
    },
];

/// E2 使用的 5 个互异配置（off 只跑一个：四个 off 格逐位相同已由 E1_DET 证明）。
const E2_CFGS: [Cfg; 5] = [
    Cfg {
        name: "on24",
        enabled: true,
        h: 24,
    },
    Cfg {
        name: "on72",
        enabled: true,
        h: 72,
    },
    Cfg {
        name: "on168",
        enabled: true,
        h: 168,
    },
    Cfg {
        name: "on720",
        enabled: true,
        h: 720,
    },
    Cfg {
        name: "off",
        enabled: false,
        h: 72,
    },
];

/// 构造 EngineConfig：只覆盖 half_life / decay_enabled / view_params，
/// 其余 8 个字段全部走 EngineConfig::default()（与 A4 完全一致）。
fn ecfg(c: Cfg) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    cfg.half_life = c.h;
    cfg.decay_enabled = c.enabled;
    cfg.view_params = view_params();
    cfg
}

fn mean(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// 样本标准差（df = n − 1）；n < 2 返回 0。
fn sd(v: &[f64]) -> f64 {
    let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let m = xs.iter().sum::<f64>() / n as f64;
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1) as f64;
    var.sqrt()
}

fn median(v: &[f64]) -> f64 {
    let mut xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    }
}

fn midrank(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| x[a].partial_cmp(&x[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i + 1;
        while j < n && (x[order[j]] - x[order[i]]).abs() <= f64::EPSILON {
            j += 1;
        }
        let avg = (i + j + 1) as f64 / 2.0;
        for k in i..j {
            r[order[k]] = avg;
        }
        i = j;
    }
    r
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len();
    if n < 3 || n != b.len() {
        return None;
    }
    let ma = a.iter().sum::<f64>() / n as f64;
    let mb = b.iter().sum::<f64>() / n as f64;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let da = a[i] - ma;
        let db = b[i] - mb;
        sab += da * db;
        saa += da * da;
        sbb += db * db;
    }
    if saa <= 1e-15 || sbb <= 1e-15 {
        return None;
    }
    Some(sab / (saa.sqrt() * sbb.sqrt()))
}

/// Spearman 秩相关（并列取平均秩）。
fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 3 || x.len() != y.len() {
        return None;
    }
    pearson(&midrank(x), &midrank(y))
}

fn f(x: f64, d: usize) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else {
        format!("{:.*}", d, x)
    }
}

/// f64 位模式哈希，用于「四个 off 格子是否逐位相同」的确定性检查。
fn bit_hash(xs: &[f64]) -> u64 {
    let mut h: u64 = 0x243F_6A88_85A3_08D3;
    for &x in xs {
        h ^= x.to_bits();
        h = h.wrapping_mul(0x1000_0000_01B3);
        h ^= h >> 29;
    }
    h
}

// ─────────────────────────────────────────────────────────────────────────
// t 分布（95% 双侧 CI 需要 t_{0.975,df}，不许用 1.96 冒充）
// ─────────────────────────────────────────────────────────────────────────

/// ln Γ(x)（Lanczos 近似，g = 5，6 个系数）。
fn gammln(x: f64) -> f64 {
    const COF: [f64; 6] = [
        76.18009172947146,
        -86.50532032941677,
        24.01409824083091,
        -1.231739572450155,
        0.1208650973866179e-2,
        -0.5395239384953e-5,
    ];
    let y = x;
    let mut xx = x;
    let mut ser = 1.000000000190015;
    for c in COF.iter() {
        xx += 1.0;
        ser += c / xx;
    }
    let t = y + 5.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (y + 0.5) * t.ln() - t + (ser / y).ln()
}

/// 不完全 beta 的连分式（Numerical Recipes betacf）。
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAXIT: usize = 300;
    const EPS: f64 = 3.0e-14;
    const FPMIN: f64 = 1.0e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAXIT {
        let mf = m as f64;
        let m2 = 2.0 * mf;
        let aa = mf * (b - mf) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + mf) * (qab + mf) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// 正则化不完全 beta I_x(a,b)。
fn betai(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let bt = (gammln(a + b) - gammln(a) - gammln(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * betacf(a, b, x) / a
    } else {
        1.0 - bt * betacf(b, a, 1.0 - x) / b
    }
}

/// Student-t 的 CDF：P(T ≤ t)，df 为自由度（f64 允许非整数，这里永远是整数）。
fn t_cdf(t: f64, df: f64) -> f64 {
    let x = df / (df + t * t);
    let half = 0.5 * betai(df / 2.0, 0.5, x);
    if t >= 0.0 {
        1.0 - half
    } else {
        half
    }
}

/// 95% 双侧临界值 t_{0.975, df}（二分求解，精度优于 1e-9）。
fn t_quantile_975(df: f64) -> f64 {
    let mut lo = 0.0;
    let mut hi = 400.0;
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if t_cdf(mid, df) < 0.975 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// 同 seed 配对差的汇总：mean ± sd、95% CI、相对基准的百分比。
#[derive(Clone)]
struct Paired {
    metric: &'static str,
    cfg: &'static str,
    n: usize,
    mean: f64,
    sd: f64,
    lo: f64,
    hi: f64,
    /// diff_mean ÷ 基准均值（%）
    rel_pct: f64,
    /// 基准（off）那一侧的均值
    base: f64,
}

/// 由「基准组逐 seed 值」和「对照组逐 seed 值」算配对差。
///
/// 两个切片必须按同一个 seed 列表顺序给出（本文件所有 stage 都是这样）。
fn paired(
    metric: &'static str,
    cfg: &'static str,
    base: &[f64],
    other: &[f64],
    n: usize,
) -> Paired {
    let d: Vec<f64> = (0..n).map(|i| other[i] - base[i]).collect();
    let m = mean(&d);
    let s = sd(&d);
    let tc = t_quantile_975((n - 1) as f64);
    let se = if n > 1 { s / (n as f64).sqrt() } else { 0.0 };
    let b = mean(base);
    Paired {
        metric,
        cfg,
        n,
        mean: m,
        sd: s,
        lo: m - tc * se,
        hi: m + tc * se,
        rel_pct: if b.abs() > 1e-15 {
            100.0 * m / b
        } else {
            f64::NAN
        },
        base: b,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 并行工具（结果按下标回填，因此并行与否逐位相同）
// ─────────────────────────────────────────────────────────────────────────

fn par_map<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let slots: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let th = threads.max(1).min(n.max(1));
    std::thread::scope(|s| {
        for _ in 0..th {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let v = f(i);
                slots.lock().unwrap()[i] = Some(v);
            });
        }
    });
    slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|x| x.expect("每个下标都必须被填上"))
        .collect()
}

fn nthreads() -> usize {
    std::env::args()
        .find_map(|a| {
            a.strip_prefix("threads=")
                .and_then(|x| x.parse::<usize>().ok())
        })
        .unwrap_or(10)
        .clamp(1, 32)
}

fn pilot() -> bool {
    std::env::args().any(|a| a == "pilot")
}

fn arg_seeds(default: &[u64]) -> Vec<u64> {
    let v: Vec<u64> = std::env::args()
        .find_map(|a| {
            a.strip_prefix("seeds=").map(|s| {
                s.split(',')
                    .filter_map(|x| x.trim().parse::<u64>().ok())
                    .collect::<Vec<u64>>()
            })
        })
        .filter(|v: &Vec<u64>| !v.is_empty())
        .unwrap_or_else(|| default.to_vec());
    if pilot() && v.len() > 2 {
        v[..2].to_vec()
    } else {
        v
    }
}

fn stage() -> String {
    std::env::args()
        .find_map(|a| a.strip_prefix("stage=").map(|s| s.to_string()))
        .unwrap_or_else(|| "all".to_string())
}

fn want(s: &str) -> bool {
    let st = stage();
    st == "all" || st == s
}

// ─────────────────────────────────────────────────────────────────────────
// 世界构造：场景 A（S 判据，L3 底座）
// ─────────────────────────────────────────────────────────────────────────

/// 场景 A 的底座配置（与 A4 逐字段同值）。
fn cfg_a(seed: u64, dorm: f64) -> SimConfig {
    let lo = (A_STEPS as u64 * 168 / 300).max(24);
    let hi = (A_STEPS as u64 * 336 / 300).max(lo + 24);
    SimConfig {
        n_initial: N_INIT,
        steps: A_STEPS,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        enable_rhythm: false,
        dormancy_rate: dorm,
        sleep_hours: (lo, hi),
        return_rate: 0.10,
        attack_kind: AttackKind::Sleeper {
            normal_until: A_NORMAL_UNTIL,
        },
        attack_ratio: N_SLEEPERS as f64 / N_INIT as f64,
        ..Default::default()
    }
}

/// 场景 A 的世界数据（同一 seed 的所有配置共享同一个世界）。
#[allow(dead_code)]
struct WorldA {
    seed: u64,
    dorm: f64,
    n0: usize,
    n_total: usize,
    n_syb: usize,
    ratings: Vec<Vec<(usize, f32)>>,
    alive: Vec<usize>,
    alive_flag: Vec<bool>,
    online: Vec<Vec<bool>>,
    is_hon: Vec<bool>,
    is_prom: Vec<bool>,
    quality: Vec<f64>,
    sybils: Vec<usize>,
    empty: Vec<usize>,
    promoted: Option<usize>,
    obs: Vec<usize>,
    off_end: f64,
    n_syb_ratings: usize,
}

fn build_world_a(seed: u64, dorm: f64) -> WorldA {
    let mut world = World::new(cfg_a(seed, dorm));
    world.run();
    let accs = world.accounts();
    let n0 = accs.len();
    let sleepers: Vec<usize> = accs
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    let is_slp: Vec<bool> = accs
        .iter()
        .map(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .collect();
    let is_prom: Vec<bool> = accs.iter().map(|a| a.kind == Kind::Promoted).collect();
    let is_hon: Vec<bool> = (0..n0).map(|i| !is_slp[i] && !is_prom[i]).collect();
    let quality: Vec<f64> = accs.iter().map(|a| a.quality as f64).collect();
    let alive: Vec<usize> = accs
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    let mut alive_flag = vec![false; n0];
    for &i in &alive {
        alive_flag[i] = true;
    }
    let promoted = world.attack.target;

    let mut ratings: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n0];
    for (o, t, s) in world.ratings() {
        if o < n0 && t < n0 {
            ratings[o].push((t, s));
        }
    }
    // 攻击落地：潜伏者若没给收益方打过分，补一条 1.0。
    // 与 L3 对女巫的注入口径一致；A4 已论证这不是伪造优势，只是让所有配置
    // 测同一个「攻击已落地」的世界（否则潜伏者先休眠，S3 的分子在配置间不可比）。
    if let Some(p) = promoted {
        for &s in &sleepers {
            if !ratings[s].iter().any(|(t, _)| *t == p) {
                ratings[s].push((p, 1.0));
            }
        }
    }

    // 女巫：注入的账户（编号 n0 起，紧跟世界账户）
    let n_syb = sleepers.len() * SYB_PER;
    let sybils: Vec<usize> = (0..n_syb).map(|x| n0 + x).collect();
    let empty_start = n0 + n_syb;
    let empty: Vec<usize> = (0..N_EMPTY).map(|x| empty_start + x).collect();
    for (k, &s) in sleepers.iter().enumerate() {
        for j in 0..SYB_PER {
            ratings[s].push((n0 + k * SYB_PER + j, 1.0));
        }
    }
    let n_total = empty_start + N_EMPTY;
    let n_syb_ratings = ratings
        .iter()
        .flat_map(|rs| rs.iter())
        .filter(|(t, _)| *t >= n0 && *t < empty_start)
        .count();

    let online: Vec<Vec<bool>> = (0..n0)
        .map(|i| {
            (0..=A_ROUNDS)
                .map(|r| world.was_online_at(i, r * A_TICK))
                .collect()
        })
        .collect();
    let off_end = {
        let off = alive
            .iter()
            .filter(|&&i| !online[i][A_ROUNDS as usize])
            .count();
        if alive.is_empty() {
            0.0
        } else {
            off as f64 / alive.len() as f64
        }
    };
    let obs: Vec<usize> = (0..n0)
        .filter(|&i| is_hon[i] && alive_flag[i] && online[i][A_ROUNDS as usize])
        .collect();

    WorldA {
        seed,
        dorm,
        n0,
        n_total,
        n_syb,
        ratings,
        alive,
        alive_flag,
        online,
        is_hon,
        is_prom,
        quality,
        sybils,
        empty,
        promoted,
        obs,
        off_end,
        n_syb_ratings,
    }
}

/// 纯世界离线探针（标定用）。
fn offline_frac_a(seed: u64, dorm: f64) -> f64 {
    let mut w = World::new(cfg_a(seed, dorm));
    w.run();
    let alive: Vec<usize> = w
        .accounts()
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    if alive.is_empty() {
        return 0.0;
    }
    let off = alive
        .iter()
        .filter(|&&i| !w.was_online_at(i, A_STEPS as u64))
        .count();
    off as f64 / alive.len() as f64
}

/// 二分标定 dormancy_rate，使末轮离线比例 ≈ OFFLINE_TARGET_A。
fn calibrate_a(seed: u64) -> f64 {
    let (mut lo, mut hi) = (1e-6f64, 0.5f64);
    for _ in 0..14 {
        let mid = 0.5 * (lo + hi);
        if offline_frac_a(seed, mid) < OFFLINE_TARGET_A {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// 建节点：每个存活账户一个 Engine（intern 全部 n_total 个账户，再录入自己的评分）。
fn build_nodes_a(wd: &WorldA, c: Cfg) -> Vec<Option<Engine>> {
    let cfg = ecfg(c);
    let mut nodes: Vec<Option<Engine>> = (0..wd.n0).map(|_| None).collect();
    for &a in &wd.alive {
        let mut e = Engine::new(cfg.clone());
        for i in 0..wd.n_total {
            e.intern(key(i));
        }
        for &(t, s) in &wd.ratings[a] {
            e.rate(t as u32, s, 0);
        }
        nodes[a] = Some(e);
    }
    nodes
}

/// 观点交换（与 D1/D4/A4 完全同构）：在线者发布 export_view；在线者 tick 抽样并
/// on_view / on_view_failed；离线者不发布也不抽样；「回归者」本轮 3 倍抽样快速刷新。
fn exchange_a(nodes: &mut [Option<Engine>], wd: &WorldA, r: u64) {
    let t = r * A_TICK;
    let mut views: Vec<Option<CompressedView>> = vec![None; wd.n0];
    for &a in &wd.alive {
        if wd.online[a][r as usize] {
            if let Some(e) = nodes[a].as_ref() {
                views[a] = Some(e.export_view(t));
            }
        }
    }
    for &a in &wd.alive {
        if !wd.online[a][r as usize] {
            continue;
        }
        let returning = r > 0 && !wd.online[a][(r - 1) as usize];
        let k = if returning { SAMPLES * 3 } else { SAMPLES };
        let reqs = match nodes[a].as_mut() {
            Some(e) => e.tick(t, k),
            None => Vec::new(),
        };
        for req in reqs {
            let v = views.get(req.idx as usize).and_then(|x| x.clone());
            if let Some(e) = nodes[a].as_mut() {
                match v {
                    Some(view) => e.on_view(req.idx, view, t, t),
                    None => e.on_view_failed(req.idx, t),
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// E1A：S1 / S2 / S3 / S4 / Spearman 的消融矩阵（2×4×24 = 192 次运行）
// ─────────────────────────────────────────────────────────────────────────

/// 一次静态运行的观测汇总（与 A4 的 Cell 同口径，另加 S5 用的权重比与确定性哈希）。
#[derive(Clone, Default)]
struct CellA {
    cfg: &'static str,
    seed: u64,
    n_obs: usize,
    n_hon_alive: usize,
    honest_mean: f64,
    s1_abs: f64,
    s1_ratio: f64,
    s1_max: f64,
    s2_eff: usize,
    s2_eff_p50: usize,
    s2_total: usize,
    s3_abs: f64,
    s3_ratio: f64,
    s4_abs: f64,
    s4_ratio: f64,
    sp: f64,
    sp_n: usize,
    sum_err: f64,
    mean_views: f64,
    /// 观察者缓存里的来源级衰减因子分布：均值 / 最小值
    mean_factor: f64,
    min_factor: f64,
    /// 全部观察者 trust_distribution 缓冲的 f64 位模式哈希（确定性检查用）
    det_hash: u64,
    /// 女巫收到的原始评分条数（应等于 60，每条 1.0）
    n_syb_ratings: usize,
    off_end: f64,
}

fn measure_a(wd: &WorldA, nodes: &[Option<Engine>], c: Cfg) -> CellA {
    let now = A_ROUNDS * A_TICK;
    let hon_alive: Vec<usize> = (0..wd.n0)
        .filter(|&i| wd.is_hon[i] && wd.alive_flag[i])
        .collect();
    let mut cell = CellA {
        cfg: c.name,
        seed: wd.seed,
        off_end: wd.off_end,
        n_syb_ratings: wd.n_syb_ratings,
        n_hon_alive: hon_alive.len(),
        ..CellA::default()
    };
    let mut per_syb = vec![0.0f64; wd.n_syb];
    let mut honest_vals: Vec<f64> = Vec::new();
    let (mut hm_sum, mut s3_sum, mut s4_sum, mut sp_sum) = (0.0, 0.0, 0.0, 0.0);
    let (mut sp_n, mut views_sum) = (0usize, 0usize);
    let mut factors: Vec<f64> = Vec::new();
    let mut hash_buf: Vec<f64> = Vec::new();

    for &o in &wd.obs {
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        cell.n_obs += 1;
        views_sum += e.freshness_stats(now).count;

        // 来源级衰减因子（只统计有缓存观点的来源）
        for v in 0..wd.n0 {
            if let Some(age) = e.view_age(v as u32, now) {
                factors.push(e.decay_factor(age) as f64);
            }
        }

        // 整份分布（观察者编号升序 → 缓冲追加顺序确定）
        let mut buf = vec![0.0f64; wd.n_total];
        let mut sum = 0.0f64;
        for (i, v) in e.trust_distribution(now) {
            let x = v as f64;
            if (i as usize) < wd.n_total {
                buf[i as usize] = x;
            }
            sum += x;
        }
        cell.sum_err = cell.sum_err.max((sum - 1.0).abs());
        hash_buf.extend_from_slice(&buf);

        let (mut hs, mut hc) = (0.0f64, 0usize);
        for &t in &hon_alive {
            if t != o {
                hs += buf[t];
                hc += 1;
                honest_vals.push(buf[t]);
            }
        }
        if hc == 0 {
            continue;
        }
        let hm = hs / hc as f64;
        hm_sum += hm;
        for (k, &sy) in wd.sybils.iter().enumerate() {
            per_syb[k] += buf[sy];
        }
        if let Some(p) = wd.promoted {
            s3_sum += buf[p];
        }
        if !wd.empty.is_empty() {
            s4_sum += wd.empty.iter().map(|&t| buf[t]).sum::<f64>() / wd.empty.len() as f64;
        }
        // Spearman：观察者支持集（dist > 0）里的真实账户 vs 内容质量
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        for i in 0..wd.n0 {
            if buf[i] > 0.0 {
                xs.push(buf[i]);
                ys.push(wd.quality[i]);
            }
        }
        if xs.len() >= 3 {
            if let Some(r) = spearman(&xs, &ys) {
                sp_sum += r;
                sp_n += 1;
            }
        }
    }

    cell.det_hash = bit_hash(&hash_buf);
    if cell.n_obs == 0 {
        return cell;
    }
    cell.honest_mean = hm_sum / cell.n_obs as f64;
    cell.mean_views = views_sum as f64 / cell.n_obs as f64;
    cell.mean_factor = mean(&factors);
    cell.min_factor = factors.iter().copied().fold(f64::INFINITY, f64::min);
    cell.s3_abs = s3_sum / cell.n_obs as f64;
    cell.s4_abs = s4_sum / cell.n_obs as f64;
    cell.sp = if sp_n > 0 {
        sp_sum / sp_n as f64
    } else {
        f64::NAN
    };
    cell.sp_n = sp_n;
    if wd.n_syb > 0 {
        let inv = 1.0 / cell.n_obs as f64;
        for k in 0..wd.n_syb {
            per_syb[k] *= inv;
        }
        cell.s1_abs = mean(&per_syb);
        cell.s1_max = per_syb.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        cell.s2_total = wd.n_syb;
        cell.s2_eff = per_syb.iter().filter(|v| **v > cell.honest_mean).count();
        let p50 = median(&honest_vals);
        cell.s2_eff_p50 = per_syb.iter().filter(|v| **v > p50).count();
    }
    if cell.honest_mean > 0.0 {
        cell.s1_ratio = cell.s1_abs / cell.honest_mean;
        cell.s3_ratio = cell.s3_abs / cell.honest_mean;
        cell.s4_ratio = cell.s4_abs / cell.honest_mean;
    } else {
        cell.s1_ratio = f64::NAN;
        cell.s3_ratio = f64::NAN;
        cell.s4_ratio = f64::NAN;
    }
    cell
}

fn run_a(wd: &WorldA, c: Cfg) -> CellA {
    let mut nodes = build_nodes_a(wd, c);
    for r in 0..=A_ROUNDS {
        exchange_a(&mut nodes, wd, r);
    }
    measure_a(wd, &nodes, c)
}

// ─────────────────────────────────────────────────────────────────────────
// 场景 B（攻击残余，D2 口径）
// ─────────────────────────────────────────────────────────────────────────

/// 场景 B/E2 的底座配置。休眠时长固定 (168, 336)：两个场景的世界都在 600~900 小时之间，
/// 用 (336,672) 会让大部分账户在攻击结束时还没回来，造成大量零基线观察者。
fn cfg_b(seed: u64, dorm: f64, steps: usize, attack_t: u64) -> SimConfig {
    // 休眠时长与 A4 的 base_cfg 逐字段对齐：按 steps/300 等比缩放 (168,336)。
    // 换掉这个值会让离线标定结果与世界结构同时变化，E1B 就再也对不上 A4 的表 2.5。
    let lo = (steps as u64 * 168 / 300).max(24);
    let hi = (steps as u64 * 336 / 300).max(lo + 24);
    SimConfig {
        n_initial: N_INIT,
        steps,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        enable_rhythm: false,
        dormancy_rate: dorm,
        sleep_hours: (lo, hi),
        return_rate: 0.10,
        attack_kind: AttackKind::Sleeper {
            normal_until: attack_t,
        },
        attack_ratio: N_SLEEPERS as f64 / N_INIT as f64,
        ..Default::default()
    }
}

/// 场景 B 世界。tick / rounds 由调用方给定（E1B 用 20h×30 轮，E2 用 6h×150 轮）。
#[allow(dead_code)]
struct WorldB {
    seed: u64,
    dorm: f64,
    tick: u64,
    rounds: u64,
    n0: usize,
    ratings: Vec<Vec<(usize, f32)>>,
    alive: Vec<usize>,
    alive_flag: Vec<bool>,
    online: Vec<Vec<bool>>,
    is_hon: Vec<bool>,
    quality: Vec<f64>,
    sleepers: Vec<usize>,
    promoted: Option<usize>,
}

fn build_world_b(
    seed: u64,
    dorm: f64,
    steps: usize,
    tick: u64,
    rounds: u64,
    attack_t: u64,
) -> WorldB {
    let mut world = World::new(cfg_b(seed, dorm, steps, attack_t));
    world.run();
    let accs = world.accounts();
    let n0 = accs.len();
    let sleepers: Vec<usize> = accs
        .iter()
        .filter(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .map(|a| a.idx)
        .collect();
    let is_slp: Vec<bool> = accs
        .iter()
        .map(|a| matches!(a.kind, Kind::Sleeper { .. }))
        .collect();
    let is_prom: Vec<bool> = accs.iter().map(|a| a.kind == Kind::Promoted).collect();
    let is_hon: Vec<bool> = (0..n0).map(|i| !is_slp[i] && !is_prom[i]).collect();
    let quality: Vec<f64> = accs.iter().map(|a| a.quality as f64).collect();
    let alive: Vec<usize> = accs
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    let mut alive_flag = vec![false; n0];
    for &i in &alive {
        alive_flag[i] = true;
    }
    let promoted = world.attack.target;

    let mut ratings: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n0];
    for (o, t, s) in world.ratings() {
        if o < n0 && t < n0 {
            ratings[o].push((t, s));
        }
    }
    // D2 口径：潜伏者对收益方的载荷先摘掉，由运行器在 t=ATTACK_T 动态注入。
    // 这样才能在 t=DARK_T 之后观察「攻击者转离线、缓存副本只能靠衰减变旧」的残余。
    if let Some(p) = promoted {
        for &s in &sleepers {
            ratings[s].retain(|(t, _)| *t != p);
        }
    }

    let online: Vec<Vec<bool>> = (0..n0)
        .map(|i| {
            (0..=rounds)
                .map(|r| world.was_online_at(i, r * tick))
                .collect()
        })
        .collect();

    WorldB {
        seed,
        dorm,
        tick,
        rounds,
        n0,
        ratings,
        alive,
        alive_flag,
        online,
        is_hon,
        quality,
        sleepers,
        promoted,
    }
}

/// 纯世界离线探针：推进到 mark 时刻统计离线比例（标定用）。
fn offline_frac_b(seed: u64, dorm: f64, steps: usize, attack_t: u64, mark: u64) -> f64 {
    let mut w = World::new(cfg_b(seed, dorm, steps, attack_t));
    while (w.time as usize) < mark as usize && w.step < steps {
        w.step();
    }
    let alive: Vec<usize> = w
        .accounts()
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    if alive.is_empty() {
        return 0.0;
    }
    let off = alive.iter().filter(|&&i| !w.was_online_at(i, mark)).count();
    off as f64 / alive.len() as f64
}

/// 二分标定场景 B 的 dormancy_rate（目标 = mark 时刻的 OFFLINE_TARGET_B）。
fn calibrate_b(seed: u64, steps: usize, attack_t: u64, mark: u64) -> f64 {
    let (mut lo, mut hi) = (1e-6f64, 0.5f64);
    for _ in 0..14 {
        let mid = 0.5 * (lo + hi);
        if offline_frac_b(seed, mid, steps, attack_t, mark) < OFFLINE_TARGET_B {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

fn build_nodes_b(wd: &WorldB, c: Cfg) -> Vec<Option<Engine>> {
    let cfg = ecfg(c);
    let mut nodes: Vec<Option<Engine>> = (0..wd.n0).map(|_| None).collect();
    for &a in &wd.alive {
        let mut e = Engine::new(cfg.clone());
        // intern 到 n0 + N_EMPTY：A4 的 build_nodes 对所有场景都 intern 了「世界账户 +
        // 60 个注入女巫 + 48 个空账户」；residue 场景里前两者为 0，但 48 个空账户
        // 仍然在里面，它们会参与 on_view 的 total_mass 估计、并占掉草图里的桶。
        // 只 intern n0 个会让归一化分母系统性偏小，读数与 A4 不再可比
        // （本文件第一版就是这样，residue 的 pre 基线偏高一倍，残余失去意义）。
        for i in 0..wd.n0 + N_EMPTY {
            e.intern(key(i));
        }
        for &(t, s) in &wd.ratings[a] {
            e.rate(t as u32, s, 0);
        }
        nodes[a] = Some(e);
    }
    nodes
}

/// 一轮观点交换（攻击者转离线后不再发布、也不再被抽到）。
fn exchange_b(nodes: &mut [Option<Engine>], wd: &WorldB, r: u64, dark: bool) {
    let t = r * wd.tick;
    let online: Vec<bool> = (0..wd.n0)
        .map(|a| wd.online[a][r as usize] && !(dark && wd.sleepers.contains(&a)))
        .collect();
    let mut views: Vec<Option<CompressedView>> = vec![None; wd.n0];
    for &a in &wd.alive {
        if online[a] {
            if let Some(e) = nodes[a].as_ref() {
                views[a] = Some(e.export_view(t));
            }
        }
    }
    for &a in &wd.alive {
        if !online[a] {
            continue;
        }
        let returning = r > 0 && !online[(r - 1) as usize];
        let k = if returning { SAMPLES * 3 } else { SAMPLES };
        let reqs = match nodes[a].as_mut() {
            Some(e) => e.tick(t, k),
            None => Vec::new(),
        };
        for req in reqs {
            let v = views.get(req.idx as usize).and_then(|x| x.clone());
            if let Some(e) = nodes[a].as_mut() {
                match v {
                    Some(view) => e.on_view(req.idx, view, t, t),
                    None => e.on_view_failed(req.idx, t),
                }
            }
        }
    }
}

/// 某轮「收益方 ÷ 诚实均值」的两个版本：
/// * q = Σ_obs(收益方份额 ÷ 诚实均值) / 观察者数 —— 与 A4 表 2.5 同口径；
/// * g = 同一比值，但先减掉 β·个人项（纯群体项）—— 衰减只作用在群体项上，
///   这个版本才是「伤害时效」的机制直读。
/// 另返回观察者数。
fn ratio_at_b(wd: &WorldB, nodes: &[Option<Engine>], r: u64) -> (f64, f64, usize) {
    let t = r * wd.tick;
    let hon_alive: Vec<usize> = (0..wd.n0)
        .filter(|&i| wd.is_hon[i] && wd.alive_flag[i])
        .collect();
    let p = match wd.promoted {
        Some(p) => p,
        None => return (f64::NAN, f64::NAN, 0),
    };
    let (mut q_sum, mut g_sum, mut cnt) = (0.0f64, 0.0f64, 0usize);
    for o in 0..wd.n0 {
        if !wd.is_hon[o] || !wd.alive_flag[o] || !wd.online[o][r as usize] {
            continue;
        }
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        let (mut hs, mut hc, mut gs, mut gc) = (0.0f64, 0usize, 0.0f64, 0usize);
        for &h in &hon_alive {
            if h == o {
                continue;
            }
            let q = e.query(h as u32, t) as f64;
            let personal = e.share_of(h as u32).unwrap_or(0.0) as f64;
            hs += q;
            hc += 1;
            gs += ((q - BETA * personal) / (1.0 - BETA)).max(0.0);
            gc += 1;
        }
        if hc == 0 {
            continue;
        }
        let base = hs / hc as f64;
        if base <= 1e-12 {
            continue;
        }
        let g_base = gs / gc as f64;
        let qp = e.query(p as u32, t) as f64;
        let pp = e.share_of(p as u32).unwrap_or(0.0) as f64;
        q_sum += qp / base;
        if g_base > 1e-12 {
            g_sum += ((qp - BETA * pp) / (1.0 - BETA)).max(0.0) / g_base;
        }
        cnt += 1;
    }
    let q = if cnt > 0 {
        q_sum / cnt as f64
    } else {
        f64::NAN
    };
    let g = if cnt > 0 {
        g_sum / cnt as f64
    } else {
        f64::NAN
    };
    (q, g, cnt)
}

/// E1B 的一次攻击残余运行。
#[derive(Clone, Default)]
struct ResidB {
    cfg: &'static str,
    seed: u64,
    /// 攻击前基线（t ∈ [60,100]）
    q_pre: f64,
    g_pre: f64,
    /// 攻击期的峰值（t ∈ [120,300]）
    q_peak: f64,
    g_peak: f64,
    t_peak: u64,
    /// 末段（t ≥ 560）
    q_end: f64,
    g_end: f64,
    /// 残余 = (末段 − 攻击前) / (峰值 − 攻击前)
    q_residue: f64,
    g_residue: f64,
    /// 末轮观察者数
    n_obs_end: usize,
    /// 转离线后观察者缓存里攻击者副本的平均年龄（末轮）
    mean_age_end: f64,
    /// 转离线后攻击者在观察者群体权重中的份额（末轮）
    wshare_end: f64,
}

/// 计算观察者缓存里攻击者副本的平均年龄，以及攻击者在群体项权重中的份额。
fn attacker_weight_stats(wd: &WorldB, nodes: &[Option<Engine>], r: u64) -> (f64, f64) {
    let t = r * wd.tick;
    let (mut age_sum, mut age_n, mut atk_w, mut all_w) = (0.0f64, 0usize, 0.0f64, 0.0f64);
    for o in 0..wd.n0 {
        if !wd.is_hon[o] || !wd.alive_flag[o] || !wd.online[o][r as usize] {
            continue;
        }
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        for &s in &wd.sleepers {
            if let Some(a) = e.view_age(s as u32, t) {
                age_sum += a as f64;
                age_n += 1;
            }
        }
        // 群体项权重 = 份额 × 衰减因子 × penalty（本实验从不写入 penalty，恒 1）
        for v in 0..wd.n0 {
            let Some(share) = e.share_of(v as u32) else {
                continue;
            };
            let fs = match e.view_age(v as u32, t) {
                Some(a) => e.decay_factor(a),
                None => 1.0,
            };
            let w = share as f64 * fs as f64;
            all_w += w;
            if wd.sleepers.contains(&v) {
                atk_w += w;
            }
        }
    }
    (
        if age_n > 0 {
            age_sum / age_n as f64
        } else {
            f64::NAN
        },
        if all_w > 1e-12 {
            atk_w / all_w
        } else {
            f64::NAN
        },
    )
}

fn run_residue_b(wd: &WorldB, c: Cfg) -> ResidB {
    let mut nodes = build_nodes_b(wd, c);
    let mut dark = false;
    let mut curve: Vec<(u64, f64, f64, usize)> = Vec::new();
    let (mut age_end, mut wshare_end) = (f64::NAN, f64::NAN);
    for r in 0..=wd.rounds {
        let t = r * wd.tick;
        // 注入载荷：攻击者给收益方 1.0（只注入一次）
        if t == B_ATTACK_T {
            if let Some(p) = wd.promoted {
                for &s in &wd.sleepers {
                    if let Some(e) = nodes[s].as_mut() {
                        e.rate(p as u32, 1.0, t);
                    }
                }
            }
        }
        let (q, g, cnt) = ratio_at_b(wd, &nodes, r);
        curve.push((t, q, g, cnt));
        if t == B_DARK_T {
            dark = true;
        }
        if r == wd.rounds {
            let (a, w) = attacker_weight_stats(wd, &nodes, r);
            age_end = a;
            wshare_end = w;
        }
        exchange_b(&mut nodes, wd, r, dark);
    }

    let pre_v: Vec<f64> = curve
        .iter()
        .filter(|(t, _, _, _)| (60..=100).contains(t))
        .map(|(_, q, _, _)| *q)
        .collect();
    let pre_g: Vec<f64> = curve
        .iter()
        .filter(|(t, _, _, _)| (60..=100).contains(t))
        .map(|(_, _, g, _)| *g)
        .collect();
    let mut q_peak = f64::NEG_INFINITY;
    let mut g_peak = f64::NEG_INFINITY;
    let mut t_peak = 0u64;
    for &(t, q, g, _) in &curve {
        if (B_ATTACK_T..=B_DARK_T).contains(&t) && q.is_finite() && q > q_peak {
            q_peak = q;
            t_peak = t;
        }
        if (B_ATTACK_T..=B_DARK_T).contains(&t) && g.is_finite() && g > g_peak {
            g_peak = g;
        }
    }
    let q_end = mean(
        &curve
            .iter()
            .filter(|(t, _, _, _)| *t >= 560)
            .map(|(_, q, _, _)| *q)
            .collect::<Vec<_>>(),
    );
    let g_end = mean(
        &curve
            .iter()
            .filter(|(t, _, _, _)| *t >= 560)
            .map(|(_, _, g, _)| *g)
            .collect::<Vec<_>>(),
    );
    let q_pre = mean(&pre_v);
    let g_pre = mean(&pre_g);
    // 「攻击是否真的落地」判定。
    //
    // 原始公式 (末段 − 攻击前)/(峰值 − 攻击前) 在「攻击根本没落地」的 seed 上会
    // 除以一个接近 0 的分母（潜伏者先休眠 → 永远不会给收益方打分 → 峰值 = 基线），
    // 结果变成 ±几十的假残余。A4 用「补一条 1.0」保证载荷落地；本实验按 D2 口径
    // 动态注入，所以必须显式判定：归一化增量 peak−pre 小于 MIN_ATTACK_GAIN 的 seed
    // 记为未落地，残差不参与聚合（但仍在 E1B_ROW 里输出 landed=0）。
    let landed = (q_peak - q_pre) >= MIN_ATTACK_GAIN_Q;
    let q_residue = if landed {
        (q_end - q_pre) / (q_peak - q_pre)
    } else {
        f64::NAN
    };
    let g_landed = (g_peak - g_pre) >= MIN_ATTACK_GAIN_G;
    let g_residue = if g_landed {
        (g_end - g_pre) / (g_peak - g_pre)
    } else {
        f64::NAN
    };
    ResidB {
        cfg: c.name,
        seed: wd.seed,
        q_pre,
        g_pre,
        q_peak,
        g_peak,
        t_peak,
        q_end,
        g_end,
        q_residue,
        g_residue,
        n_obs_end: curve.last().map(|x| x.3).unwrap_or(0),
        mean_age_end: age_end,
        wshare_end,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// E2：伤害时效曲线（攻击结束后 {6,24,72,168,600}h 的快照）
// ─────────────────────────────────────────────────────────────────────────

/// E2 一个快照的全部数字。
#[derive(Clone, Default)]
struct E2Snap {
    cfg: &'static str,
    seed: u64,
    /// 攻击结束后的偏移小时
    off: u64,
    /// 实际快照时刻
    t: u64,
    /// 攻击者在观察者信任分布里的份额（观测者均值）
    mass: f64,
    /// 攻击者份额 ÷ 攻击期峰值份额
    mass_ratio: f64,
    /// 收益方 ÷ 诚实均值（A4 口径）
    q: f64,
    /// 收益方 ÷ 诚实均值，攻击结束后的「相对峰值残留」= (q_t − q_pre)/(q_peak − q_pre)
    q_resid: f64,
    /// 攻击者在群体项权重里的份额
    wshare: f64,
    /// 攻击者在群体项权重里的份额 ÷ 攻击期峰值
    wshare_ratio: f64,
    /// 观察者缓存里攻击者副本的平均年龄
    mean_age: f64,
    /// 该年龄对应的衰减因子均值（= 0.5^(age/H)，off 时恒 1）
    mean_factor: f64,
    n_obs: usize,
}

/// E2 的一次完整运行（900h / 150 轮 / 每轮 6h）。
struct E2Run {
    cfg: &'static str,
    seed: u64,
    q_pre: f64,
    q_peak: f64,
    mass_peak: f64,
    wshare_peak: f64,
    t_peak: u64,
    snaps: Vec<E2Snap>,
}

/// 某轮：攻击者份额（信任分布里的总份额）、收益方/诚实比、攻击者权重份额、平均副本年龄。
fn e2_measure(
    wd: &WorldB,
    nodes: &[Option<Engine>],
    r: u64,
) -> (f64, f64, f64, f64, f64, f64, usize) {
    let t = r * wd.tick;
    let hon_alive: Vec<usize> = (0..wd.n0)
        .filter(|&i| wd.is_hon[i] && wd.alive_flag[i])
        .collect();
    let p = match wd.promoted {
        Some(p) => p,
        None => {
            return (
                f64::NAN,
                f64::NAN,
                f64::NAN,
                f64::NAN,
                f64::NAN,
                f64::NAN,
                0,
            )
        }
    };
    let (mut mass_sum, mut q_sum, mut w_atk, mut w_all) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut age_sum, mut age_n, mut f_sum, mut f_n, mut cnt) =
        (0.0f64, 0usize, 0.0f64, 0usize, 0usize);
    for o in 0..wd.n0 {
        if !wd.is_hon[o] || !wd.alive_flag[o] || !wd.online[o][r as usize] {
            continue;
        }
        let e = match nodes.get(o).and_then(|x| x.as_ref()) {
            Some(e) => e,
            None => continue,
        };
        // 攻击者在信任分布里的份额
        let mut atk_mass = 0.0f64;
        for &s in &wd.sleepers {
            atk_mass += e.query(s as u32, t) as f64;
        }
        // 群体项权重份额（份额 × 衰减因子）
        for v in 0..wd.n0 {
            let Some(share) = e.share_of(v as u32) else {
                continue;
            };
            let fs = match e.view_age(v as u32, t) {
                Some(a) => e.decay_factor(a),
                None => 1.0,
            };
            let w = share as f64 * fs as f64;
            w_all += w;
            if wd.sleepers.contains(&v) {
                w_atk += w;
            }
        }
        // 攻击者副本年龄
        let mut youngest: Option<u64> = None;
        for &s in &wd.sleepers {
            if let Some(a) = e.view_age(s as u32, t) {
                youngest = Some(youngest.map_or(a, |x| x.min(a)));
            }
        }
        if let Some(a) = youngest {
            age_sum += a as f64;
            f_sum += e.decay_factor(a) as f64;
            f_n += 1;
            age_n += 1;
        }
        // 收益方 ÷ 诚实均值
        let (mut hs, mut hc) = (0.0f64, 0usize);
        for &h in &hon_alive {
            if h != o {
                hs += e.query(h as u32, t) as f64;
                hc += 1;
            }
        }
        if hc == 0 {
            continue;
        }
        let base = hs / hc as f64;
        if base <= 1e-12 {
            continue;
        }
        mass_sum += atk_mass;
        q_sum += e.query(p as u32, t) as f64 / base;
        cnt += 1;
    }
    (
        if cnt > 0 {
            mass_sum / cnt as f64
        } else {
            f64::NAN
        },
        if cnt > 0 {
            q_sum / cnt as f64
        } else {
            f64::NAN
        },
        if w_all > 1e-12 {
            w_atk / w_all
        } else {
            f64::NAN
        },
        if age_n > 0 {
            age_sum / age_n as f64
        } else {
            f64::NAN
        },
        if f_n > 0 {
            f_sum / f_n as f64
        } else {
            f64::NAN
        },
        f64::NAN,
        cnt,
    )
}

fn run_e2(wd: &WorldB, c: Cfg) -> E2Run {
    let mut nodes = build_nodes_b(wd, c);
    let mut dark = false;
    // (t, mass, q, wshare, age, factor, cnt)
    let mut per_round: Vec<(u64, f64, f64, f64, f64, f64, f64, usize)> =
        Vec::with_capacity(wd.rounds as usize + 1);
    for r in 0..=wd.rounds {
        let t = r * wd.tick;
        if t == E2_ATTACK_T {
            if let Some(p) = wd.promoted {
                for &s in &wd.sleepers {
                    if let Some(e) = nodes[s].as_mut() {
                        e.rate(p as u32, 1.0, t);
                    }
                }
            }
        }
        let (mass, q, wshare, age, factor, _atk_landed_probe, cnt) = e2_measure(wd, &nodes, r);
        per_round.push((t, mass, q, wshare, age, factor, 0.0, cnt));
        if t == E2_DARK_T {
            dark = true;
        }
        exchange_b(&mut nodes, wd, r, dark);
    }

    let q_pre = mean(
        &per_round
            .iter()
            .filter(|x| (60..=100).contains(&x.0))
            .map(|x| x.2)
            .collect::<Vec<_>>(),
    );
    let mut q_peak = f64::NEG_INFINITY;
    let mut mass_peak = f64::NEG_INFINITY;
    let mut wshare_peak = f64::NEG_INFINITY;
    let mut t_peak = 0u64;
    for x in &per_round {
        if (E2_ATTACK_T..=E2_DARK_T).contains(&x.0) {
            if x.2.is_finite() && x.2 > q_peak {
                q_peak = x.2;
                t_peak = x.0;
            }
            if x.1.is_finite() && x.1 > mass_peak {
                mass_peak = x.1;
            }
            if x.3.is_finite() && x.3 > wshare_peak {
                wshare_peak = x.3;
            }
        }
    }

    let mut snaps: Vec<E2Snap> = Vec::new();
    for off in E2_OFFSETS {
        let t = E2_DARK_T + off;
        if let Some(x) = per_round.iter().find(|x| x.0 == t) {
            let q_resid = if (q_peak - q_pre).abs() > 1e-12 {
                (x.2 - q_pre) / (q_peak - q_pre)
            } else {
                f64::NAN
            };
            snaps.push(E2Snap {
                cfg: c.name,
                seed: wd.seed,
                off,
                t,
                mass: x.1,
                mass_ratio: if mass_peak.abs() > 1e-12 {
                    x.1 / mass_peak
                } else {
                    f64::NAN
                },
                q: x.2,
                q_resid,
                wshare: x.3,
                wshare_ratio: if wshare_peak.abs() > 1e-12 {
                    x.3 / wshare_peak
                } else {
                    f64::NAN
                },
                mean_age: x.4,
                mean_factor: x.5,
                n_obs: x.7,
            });
        }
    }

    E2Run {
        cfg: c.name,
        seed: wd.seed,
        q_pre,
        q_peak,
        mass_peak,
        wshare_peak,
        t_peak,
        snaps,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// E3：S5 套利边界（合成用例，与 A4 的 s5 同构）
// ─────────────────────────────────────────────────────────────────────────

/// 单个 seed 的合成用例：观察者给 6 个诚实来源 + 1 个攻击者各 1.0（份额各 1/7）。
///
/// 攻击者的观点给目标 1.0；6 个诚实来源发布空观点（对目标没表态）。
/// 诚实来源的观点年龄 = honest_age，攻击者年龄 = 0（保持新鲜）。
///
/// 返回 (实测 ÷ 等新鲜度上限, 解析式 7/(1+6·0.5^(age/H)), 全新鲜时的实测 ÷ 上限)。
fn s5_case(c: Cfg, honest_age: u64, seed: u64) -> (f64, f64, f64) {
    // seed 只影响账户号与草图种子（噪音底/碰撞布局），不影响份额结构：
    // 每个 seed 换一套编号与草图种子，16 个 seed 就真的是 16 次独立测量。
    // 账户号按 seed 变化（16 个 seed 是 16 次独立测量），但**草图参数必须与
    // ecfg() 里的 view_params 逐字段相同**，否则 Engine::on_view 会在
    // 「形状或种子不一致」的分支上直接 return（本文件第一版就踩过这个坑，
    // 结果 measured 恒为 0：参数不匹配时观点根本没进缓存）。
    let atk_key = key(1000 + (seed as usize % 97));
    let target_key = key(2000 + (seed as usize % 89));
    let honest_keys: Vec<AccountKey> = (0..6)
        .map(|i| key(3000 + i * 17 + (seed as usize % 71)))
        .collect();
    let vp = view_params();
    let cap = (1.0 - BETA) / 7.0;

    let run = |honest_age: u64| -> f64 {
        let query_t = S5_QUERY_T;
        let mut e = Engine::new(ecfg(c));
        // intern 顺序：0=target, 1..=6=诚实来源, 7=攻击者
        e.intern(target_key);
        for k in &honest_keys {
            e.intern(*k);
        }
        e.intern(atk_key);
        for i in 1..=7u32 {
            e.rate(i, 1.0, 0);
        }
        // 诚实来源：空观点（对目标没表态），年龄 = honest_age
        for i in 1..=6u32 {
            e.on_view(i, CompressedView::new(vp), 0, query_t - honest_age);
        }
        // 攻击者：给目标 1.0，年龄 = 0（保持新鲜）
        let atk_view = CompressedView::from_pairs(vp, &[(target_key, 1.0)]);
        e.on_view(7, atk_view, 0, query_t);
        e.query(0, query_t) as f64 / cap
    };

    let measured = run(honest_age);
    let hom = run(0);
    let analytic = if c.enabled {
        7.0 / (1.0 + 6.0 * 0.5f64.powf(honest_age as f64 / c.h as f64))
    } else {
        1.0
    };
    (measured, analytic, hom)
}

// ─────────────────────────────────────────────────────────────────────────
// E4：半衰期自适应（H × 刷新周期 T × 批大小 k）
// ─────────────────────────────────────────────────────────────────────────

/// E4 的底座配置：150 人、480 步、**每人关注 60 个来源**（这样 n/k 才有意义）。
///
/// 关键点：引擎的 tick(k) 是从「我评过的来源」（正分清单）里抽的，不是从全网账户里抽。
/// 想让 T·(n/k) 这条判据真的可测，就必须让 n（正分清单长度）显著大于 k。
/// 默认仿真里每人只评 6~10 个来源、而 SAMPLES=12，一轮就能刷完整个清单，
/// 那样 n/k ≡ 1，判据退化成 T ≤ H/2，测不出任何东西。
const E4_N: usize = 150;
const E4_FOLLOWS: usize = 60;

fn cfg_e4(seed: u64, dorm: f64) -> SimConfig {
    SimConfig {
        n_initial: E4_N,
        steps: E4_STEPS,
        seed,
        join_rate: 0.0,
        leave_rate: 0.0,
        enable_rhythm: false,
        dormancy_rate: dorm,
        sleep_hours: (96, 192),
        return_rate: 0.80,
        follow_min: E4_FOLLOWS,
        follow_max: E4_FOLLOWS,
        max_follows: E4_FOLLOWS + 8,
        follow_rate: 0.0,
        interactions_per_step: 0.6,
        attack_kind: AttackKind::None,
        attack_ratio: 0.0,
        ..Default::default()
    }
}

#[allow(dead_code)]
struct WorldE4 {
    seed: u64,
    dorm: f64,
    n0: usize,
    ratings: Vec<Vec<(usize, f32)>>,
    alive: Vec<usize>,
    alive_flag: Vec<bool>,
    /// online[r]：第 r 个刷新轮次在线的账户集合（按 tick 参数 T 计算）
    online: Vec<Vec<bool>>,
    is_hon: Vec<bool>,
    mean_ratings: f64,
}

fn build_world_e4(seed: u64, dorm: f64, tick: u64, rounds: u64) -> WorldE4 {
    let mut world = World::new(cfg_e4(seed, dorm));
    world.run();
    let accs = world.accounts();
    let n0 = accs.len();
    let is_hon: Vec<bool> = (0..n0).map(|i| accs[i].kind == Kind::Honest).collect();
    let alive: Vec<usize> = accs
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    let mut alive_flag = vec![false; n0];
    for &i in &alive {
        alive_flag[i] = true;
    }
    let mut ratings: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n0];
    for (o, t, s) in world.ratings() {
        if o < n0 && t < n0 {
            ratings[o].push((t, s));
        }
    }
    let online: Vec<Vec<bool>> = (0..=rounds)
        .map(|r| {
            (0..n0)
                .map(|i| world.was_online_at(i, r * tick))
                .collect::<Vec<bool>>()
        })
        .collect();
    let hon_alive: Vec<usize> = (0..n0).filter(|&i| is_hon[i] && alive_flag[i]).collect();
    let mean_ratings = if hon_alive.is_empty() {
        0.0
    } else {
        hon_alive
            .iter()
            .map(|&i| ratings[i].len() as f64)
            .sum::<f64>()
            / hon_alive.len() as f64
    };
    WorldE4 {
        seed,
        dorm,
        n0,
        ratings,
        alive,
        alive_flag,
        online,
        is_hon,
        mean_ratings,
    }
}

fn offline_frac_e4(seed: u64, dorm: f64, steps: usize) -> f64 {
    let mut w = World::new(cfg_e4(seed, dorm));
    w.run();
    let _ = steps;
    let alive: Vec<usize> = w
        .accounts()
        .iter()
        .filter(|a| a.is_alive())
        .map(|a| a.idx)
        .collect();
    if alive.is_empty() {
        return 0.0;
    }
    let off = alive
        .iter()
        .filter(|&&i| !w.was_online_at(i, E4_STEPS as u64))
        .count();
    off as f64 / alive.len() as f64
}

/// 二分标定 E4 的 dormancy，目标 50% 末轮离线。
fn calibrate_e4(seed: u64) -> f64 {
    let (mut lo, mut hi) = (1e-6f64, 0.5f64);
    for _ in 0..12 {
        let mid = 0.5 * (lo + hi);
        if offline_frac_e4(seed, mid, E4_STEPS) < 0.50 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// E4 一个格子的观测。
#[derive(Clone, Default)]
struct CellE4 {
    h: u64,
    t_card: u64,
    k: usize,
    seed: u64,
    n_sources: f64,
    /// T·(n/k)：一次完整刷新需要多少小时
    sweep: f64,
    /// 判据 T·(n/k) ≤ H/2
    satisfied: bool,
    /// 群体项权重的 Kish 有效来源数 (Σw)²/Σw²
    ess: f64,
    /// 有缓存观点的来源数 ÷ 正分清单长度
    coverage: f64,
    /// 缓存观点里衰减因子 < 0.1 的比例
    frac_stale: f64,
    /// 有缓存观点的平均年龄（「全体变旧」漂移量）
    mean_age: f64,
    /// 有缓存观点的平均衰减因子
    mean_factor: f64,
    /// 无缓存观点的来源数（弃权票）÷ 清单长度
    frac_noview: f64,
}

fn run_e4(wd: &WorldE4, h: u64, t_card: u64, k: usize) -> CellE4 {
    let half_life = h;
    let cfg = EngineConfig {
        half_life,
        ..EngineConfig::default()
    };
    let rounds = E4_STEPS as u64 / t_card;
    let now = E4_STEPS as u64;
    let mut nodes: Vec<Option<Engine>> = (0..wd.n0).map(|_| None).collect();
    for &a in &wd.alive {
        let mut e = Engine::new(cfg.clone());
        for i in 0..wd.n0 {
            e.intern(key(i));
        }
        for &(t, s) in &wd.ratings[a] {
            e.rate(t as u32, s, 0);
        }
        nodes[a] = Some(e);
    }
    for r in 0..=rounds {
        let t = r * t_card;
        let mut views: Vec<Option<CompressedView>> = vec![None; wd.n0];
        for &a in &wd.alive {
            if wd.online[r as usize][a] {
                if let Some(e) = nodes[a].as_ref() {
                    views[a] = Some(e.export_view(t));
                }
            }
        }
        for &a in &wd.alive {
            if !wd.online[r as usize][a] {
                continue;
            }
            let reqs = match nodes[a].as_mut() {
                Some(e) => e.tick(t, k),
                None => Vec::new(),
            };
            for req in reqs {
                let v = views.get(req.idx as usize).and_then(|x| x.clone());
                if let Some(e) = nodes[a].as_mut() {
                    match v {
                        Some(view) => e.on_view(req.idx, view, t, t),
                        None => e.on_view_failed(req.idx, t),
                    }
                }
            }
        }
    }

    let mut ess: Vec<f64> = Vec::new();
    let mut cov: Vec<f64> = Vec::new();
    let mut fstale: Vec<f64> = Vec::new();
    let mut mage: Vec<f64> = Vec::new();
    let mut mfac: Vec<f64> = Vec::new();
    let mut fnone: Vec<f64> = Vec::new();
    let mut ns: Vec<f64> = Vec::new();
    for o in 0..wd.n0 {
        if !wd.is_hon[o] || !wd.alive_flag[o] {
            continue;
        }
        let e = match nodes[o].as_ref() {
            Some(e) => e,
            None => continue,
        };
        let m = wd.ratings[o].len();
        if m == 0 {
            continue;
        }
        ns.push(m as f64);
        let (mut w_sum, mut w2_sum) = (0.0f64, 0.0f64);
        let (mut cached, mut stale, mut age_sum, mut fac_sum) = (0usize, 0usize, 0.0f64, 0.0f64);
        for &(v, _score) in &wd.ratings[o] {
            let Some(share) = e.share_of(v as u32) else {
                continue;
            };
            let fsv = match e.view_age(v as u32, now) {
                Some(a) => {
                    cached += 1;
                    age_sum += a as f64;
                    let ff = e.decay_factor(a) as f64;
                    fac_sum += ff;
                    if ff < 0.1 {
                        stale += 1;
                    }
                    ff
                }
                // 没有观点的来源：衰减基准按 1（与 Engine::query 一致），投弃权票
                None => 1.0,
            };
            let w = share as f64 * fsv;
            w_sum += w;
            w2_sum += w * w;
        }
        if w2_sum > 1e-12 {
            ess.push(w_sum * w_sum / w2_sum);
        }
        cov.push(cached as f64 / m as f64);
        fnone.push((m - cached) as f64 / m as f64);
        if cached > 0 {
            fstale.push(stale as f64 / cached as f64);
            mage.push(age_sum / cached as f64);
            mfac.push(fac_sum / cached as f64);
        }
    }
    let n_sources = mean(&ns);
    let sweep = t_card as f64 * (n_sources / k as f64);
    CellE4 {
        h,
        t_card,
        k,
        seed: wd.seed,
        n_sources: mean(&ns),
        sweep,
        satisfied: sweep <= (h as f64) / 2.0,
        ess: mean(&ess),
        coverage: mean(&cov),
        frac_stale: mean(&fstale),
        mean_age: mean(&mage),
        mean_factor: mean(&mfac),
        frac_noview: mean(&fnone),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// E1A 的输出：逐格 + 逐指标配对差
// ─────────────────────────────────────────────────────────────────────────

fn cmd_e1a() {
    let seeds = arg_seeds(&SEEDS24);
    let t0 = Instant::now();
    let worlds: Vec<WorldA> = par_map(seeds.len(), nthreads(), |i| {
        let s = seeds[i];
        let dorm = calibrate_a(s);
        build_world_a(s, dorm)
    });
    eprintln!(
        "[E1A] 世界就绪 {} 个（{:.1}s）",
        worlds.len(),
        t0.elapsed().as_secs_f64()
    );

    // jobs：先按 (seed, 配置) 展开，结果按同一下标回填 → 输出顺序确定
    let jobs: Vec<(usize, usize)> = (0..worlds.len())
        .flat_map(|wi| (0..CFGS8.len()).map(move |ci| (wi, ci)))
        .collect();
    let cells: Vec<(usize, usize, CellA)> = par_map(jobs.len(), nthreads(), |j| {
        let (wi, ci) = jobs[j];
        let c = run_a(&worlds[wi], CFGS8[ci]);
        (wi, ci, c)
    });
    eprintln!(
        "[E1A] {} 次仿真完成（{:.1}s）",
        cells.len(),
        t0.elapsed().as_secs_f64()
    );

    println!(
        "\n== E1A 逐格原始数字（cfg = decay_enabled × half_life；2×4×{} = {} 次仿真） ==",
        seeds.len(),
        cells.len()
    );
    println!("E1A_ROW,cfg,enabled,H,seed,dorm,off_end,n0,n_total,n_obs,n_hon_alive,n_hon_targets,honest_mean,s1_abs,s1_ratio,s1_max,s2_eff,s2_eff_p50,s2_total,s3_abs,s3_ratio,s4_abs,s4_ratio,spearman,sp_n,sum_err,mean_views,mean_factor,min_factor,n_syb_ratings,det_hash");
    for (wi, ci, c) in &cells {
        let wd = &worlds[*wi];
        let cfg = CFGS8[*ci];
        println!(
            "E1A_ROW,{},{},{},{},{:.6},{:.4},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            c.cfg,
            if cfg.enabled { 1 } else { 0 },
            cfg.h,
            c.seed,
            wd.dorm,
            f(c.off_end, 4),
            wd.n0,
            wd.n_total,
            c.n_obs,
            c.n_hon_alive,
            c.n_obs.saturating_sub(1),
            f(c.honest_mean, 8),
            f(c.s1_abs, 8),
            f(c.s1_ratio, 8),
            f(c.s1_max, 8),
            c.s2_eff,
            c.s2_eff_p50,
            c.s2_total,
            f(c.s3_abs, 8),
            f(c.s3_ratio, 8),
            f(c.s4_abs, 8),
            f(c.s4_ratio, 8),
            f(c.sp, 6),
            c.sp_n,
            f(c.sum_err, 8),
            f(c.mean_views, 3),
            f(c.mean_factor, 5),
            f(c.min_factor, 5),
            c.n_syb_ratings,
            c.det_hash,
        );
    }

    // ── 逐指标：均值±sd（每个配置跨 seed） ──
    let metrics: [(&'static str, fn(&CellA) -> f64); 11] = [
        ("s1_ratio", |c| c.s1_ratio),
        ("s1_abs", |c| c.s1_abs),
        ("s2_eff", |c| c.s2_eff as f64),
        ("s3_ratio", |c| c.s3_ratio),
        ("s4_ratio", |c| c.s4_ratio),
        ("spearman", |c| c.sp),
        ("honest_mean", |c| c.honest_mean),
        ("off_end", |c| c.off_end),
        ("mean_views", |c| c.mean_views),
        ("mean_factor", |c| c.mean_factor),
        ("min_factor", |c| c.min_factor),
    ];
    println!(
        "\n== E1A 均值±sd（每个配置跨 {} seed；sd = 样本标准差 df=n-1） ==",
        seeds.len()
    );
    println!("E1A_CELL,cfg,metric,n,mean,sd,median,min,max");
    for ci in 0..CFGS8.len() {
        let g: Vec<&CellA> = cells
            .iter()
            .filter(|(_, cj, _)| *cj == ci)
            .map(|(_, _, c)| c)
            .collect();
        for (name, getv) in metrics {
            let v: Vec<f64> = g.iter().map(|c| getv(c)).collect();
            let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
            println!(
                "E1A_CELL,{},{},{},{},{},{},{},{}",
                CFGS8[ci].name,
                name,
                xs.len(),
                f(mean(&v), 8),
                f(sd(&v), 8),
                f(median(&v), 8),
                f(xs.iter().copied().fold(f64::INFINITY, f64::min), 8),
                f(xs.iter().copied().fold(f64::NEG_INFINITY, f64::max), 8),
            );
        }
    }

    // ── 配对差（同 seed 的 on_H − off72） + 95% CI ──
    // 基准用 off72（四个 off 格逐位相同，已由 det 行验证；用哪个 off 不影响结果）。
    println!("\n== E1A 配对差（同 seed 的 on_H − off72）+ 95% CI（t 分布，df = n−1） ==");
    println!("E1A_PAIRED,metric,cfg,n,diff_mean,diff_sd,ci95_lo,ci95_hi,base,rel_pct,significant");
    let base_ci = CFGS8.iter().position(|c| c.name == "off72").unwrap();
    for ci in 0..4usize {
        let a: Vec<&CellA> = cells
            .iter()
            .filter(|(_, cj, _)| *cj == ci)
            .map(|(_, _, c)| c)
            .collect();
        let b: Vec<&CellA> = cells
            .iter()
            .filter(|(_, cj, _)| *cj == base_ci)
            .map(|(_, _, c)| c)
            .collect();
        for (name, getv) in metrics {
            let va: Vec<f64> = a.iter().map(|c| getv(c)).collect();
            let vb: Vec<f64> = b.iter().map(|c| getv(c)).collect();
            let p = paired(name, CFGS8[ci].name, &vb, &va, seeds.len());
            let sig = if p.lo.is_finite() && p.hi.is_finite() && !(p.lo <= 0.0 && p.hi >= 0.0) {
                1
            } else {
                0
            };
            println!(
                "E1A_PAIRED,{},{},n={},diff_mean={},diff_sd={},ci95_lo={},ci95_hi={},base={},rel_pct={},significant={}",
                p.metric,
                p.cfg,
                p.n,
                f(p.mean, 8),
                f(p.sd, 8),
                f(p.lo, 8),
                f(p.hi, 8),
                f(p.base, 8),
                f(p.rel_pct, 2),
                sig
            );
        }
    }

    // ── 确定性：四个 off 格子的分布哈希必须逐位相同 ──
    println!("\n== E1A_DET 四个 off 格子的 trust_distribution 位模式哈希（应逐 seed 全部相同） ==");
    println!("E1A_DET,seed,off24,off72,off168,off720,identical");
    let off_idxs: Vec<usize> = (4..8).collect();
    let mut all_identical = true;
    for (si, &seed) in seeds.iter().enumerate() {
        let hs: Vec<u64> = off_idxs
            .iter()
            .map(|&ci| {
                cells
                    .iter()
                    .find(|(wi, cj, _)| *wi == si && *cj == ci)
                    .map(|(_, _, c)| c.det_hash)
                    .unwrap_or(0)
            })
            .collect();
        let ident = hs.iter().all(|x| *x == hs[0]);
        all_identical &= ident;
        println!(
            "E1A_DET,{},{},{},{},{},{}",
            seed,
            hs[0],
            hs[1],
            hs[2],
            hs[3],
            if ident { 1 } else { 0 }
        );
    }
    println!(
        "E1A_DET_ALL_IDENTICAL,{}",
        if all_identical { 1 } else { 0 }
    );
    eprintln!("[E1A] 结束 {:.1}s", t0.elapsed().as_secs_f64());
}

// ─────────────────────────────────────────────────────────────────────────
// E1B：攻击残余（2×4×24 = 192 次运行，600h / 30 轮 / 每轮 20h）
// ─────────────────────────────────────────────────────────────────────────

fn cmd_e1b() {
    let seeds = arg_seeds(&SEEDS24);
    let t0 = Instant::now();
    let worlds: Vec<WorldB> = par_map(seeds.len(), nthreads(), |i| {
        let s = seeds[i];
        let dorm = calibrate_b(s, B_STEPS, B_ATTACK_T, B_STEPS as u64);
        build_world_b(s, dorm, B_STEPS, B_TICK, B_ROUNDS, B_ATTACK_T)
    });
    eprintln!(
        "[E1B] 世界就绪 {} 个（{:.1}s）",
        worlds.len(),
        t0.elapsed().as_secs_f64()
    );

    let jobs: Vec<(usize, usize)> = (0..worlds.len())
        .flat_map(|wi| (0..CFGS8.len()).map(move |ci| (wi, ci)))
        .collect();
    let rows: Vec<ResidB> = par_map(jobs.len(), nthreads(), |j| {
        let (wi, ci) = jobs[j];
        run_residue_b(&worlds[wi], CFGS8[ci])
    });
    eprintln!(
        "[E1B] {} 次仿真完成（{:.1}s）",
        rows.len(),
        t0.elapsed().as_secs_f64()
    );

    println!("\n== E1B 攻击残余逐格（D2 口径：600h/30 轮/每轮 20h；t=120 注入载荷、t=300 攻击者转离线） ==");
    println!("E1B_ROW,cfg,enabled,H,seed,dorm,q_pre,q_peak,q_end,q_residue,q_landed,g_pre,g_peak,g_end,g_residue,g_landed,t_peak,n_obs_end,mean_age_end,wshare_end");
    for (j, r) in rows.iter().enumerate() {
        let (wi, ci) = jobs[j];
        let cfg = CFGS8[ci];
        let q_landed = (r.q_peak - r.q_pre) >= MIN_ATTACK_GAIN_Q;
        let g_landed = (r.g_peak - r.g_pre) >= MIN_ATTACK_GAIN_G;
        println!(
            "E1B_ROW,{},{},{},{},{:.6},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.cfg,
            if cfg.enabled { 1 } else { 0 },
            cfg.h,
            r.seed,
            worlds[wi].dorm,
            f(r.q_pre, 6),
            f(r.q_peak, 6),
            f(r.q_end, 6),
            f(r.q_residue, 6),
            if q_landed { 1 } else { 0 },
            f(r.g_pre, 6),
            f(r.g_peak, 6),
            f(r.g_end, 6),
            f(r.g_residue, 6),
            if g_landed { 1 } else { 0 },
            r.t_peak,
            r.n_obs_end,
            f(r.mean_age_end, 2),
            f(r.wshare_end, 5)
        );
    }

    let metrics: [(&'static str, fn(&ResidB) -> f64); 10] = [
        ("q_residue", |r| r.q_residue),
        ("g_residue", |r| r.g_residue),
        ("q_peak", |r| r.q_peak),
        ("g_peak", |r| r.g_peak),
        ("q_end", |r| r.q_end),
        ("g_end", |r| r.g_end),
        ("q_pre", |r| r.q_pre),
        ("mean_age_end", |r| r.mean_age_end),
        ("wshare_end", |r| r.wshare_end),
        ("n_obs_end", |r| r.n_obs_end as f64),
    ];
    println!("\n== E1B 攻击落地计数（峰值−基线 >= {} 视为落地；未落地的 seed 残差记为 nan 且不参与聚合） ==", MIN_ATTACK_GAIN_Q);
    println!("E1B_LANDED,cfg,n_seeds,q_landed,g_landed");
    for ci in 0..CFGS8.len() {
        let g: Vec<&ResidB> = rows.iter().filter(|r| r.cfg == CFGS8[ci].name).collect();
        let ql = g
            .iter()
            .filter(|r| (r.q_peak - r.q_pre) >= MIN_ATTACK_GAIN_Q)
            .count();
        let gl = g
            .iter()
            .filter(|r| (r.g_peak - r.g_pre) >= MIN_ATTACK_GAIN_G)
            .count();
        println!("E1B_LANDED,{},{},{},{}", CFGS8[ci].name, g.len(), ql, gl);
    }

    println!("\n== E1B 均值±sd（每个配置跨 {} seed） ==", seeds.len());
    println!("E1B_CELL,cfg,metric,n,mean,sd,median,min,max");
    for ci in 0..CFGS8.len() {
        let g: Vec<&ResidB> = rows.iter().filter(|r| r.cfg == CFGS8[ci].name).collect();
        for (name, getv) in metrics {
            let v: Vec<f64> = g.iter().map(|r| getv(r)).collect();
            let xs: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
            println!(
                "E1B_CELL,{},{},{},{},{},{},{},{}",
                CFGS8[ci].name,
                name,
                xs.len(),
                f(mean(&v), 6),
                f(sd(&v), 6),
                f(median(&v), 6),
                f(xs.iter().copied().fold(f64::INFINITY, f64::min), 6),
                f(xs.iter().copied().fold(f64::NEG_INFINITY, f64::max), 6),
            );
        }
    }

    println!("\n== E1B 配对差（同 seed 的 on_H − off72）+ 95% CI ==");
    println!("E1B_PAIRED,metric,cfg,n,diff_mean,diff_sd,ci95_lo,ci95_hi,base,rel_pct,significant");
    let pick = |ci: usize| -> Vec<&ResidB> {
        rows.iter()
            .enumerate()
            .filter(|(j, _)| jobs[*j].1 == ci)
            .map(|(_, r)| r)
            .collect()
    };
    let base_ci = CFGS8.iter().position(|c| c.name == "off72").unwrap();
    for ci in 0..4usize {
        let a = pick(ci);
        let b = pick(base_ci);
        for (name, getv) in metrics {
            let va: Vec<f64> = a.iter().map(|r| getv(r)).collect();
            let vb: Vec<f64> = b.iter().map(|r| getv(r)).collect();
            let p = paired(name, CFGS8[ci].name, &vb, &va, seeds.len());
            let sig = if p.lo.is_finite() && p.hi.is_finite() && !(p.lo <= 0.0 && p.hi >= 0.0) {
                1
            } else {
                0
            };
            println!(
                "E1B_PAIRED,{},{},n={},diff_mean={},diff_sd={},ci95_lo={},ci95_hi={},base={},rel_pct={},significant={}",
                p.metric, p.cfg, p.n, f(p.mean, 6), f(p.sd, 6), f(p.lo, 6), f(p.hi, 6),
                f(p.base, 6), f(p.rel_pct, 2), sig
            );
        }
    }
    println!("\n== E1B 前 8 seed（= A4 的 seed 集）与后 16 seed 的稳定性 ==");
    println!("E1B_SPLIT,cfg,metric,first8_mean,last16_mean,first8_sd,last16_sd,sign_flip");
    for ci in 0..CFGS8.len() {
        let g = pick(ci);
        for (name, getv) in metrics {
            let (f8, l16) = if seeds.len() >= 24 {
                (
                    mean(&g[..8].iter().map(|r| getv(r)).collect::<Vec<_>>()),
                    mean(&g[8..].iter().map(|r| getv(r)).collect::<Vec<_>>()),
                )
            } else {
                (f64::NAN, f64::NAN)
            };
            let (s8, s16) = if seeds.len() >= 24 {
                (
                    sd(&g[..8].iter().map(|r| getv(r)).collect::<Vec<_>>()),
                    sd(&g[8..].iter().map(|r| getv(r)).collect::<Vec<_>>()),
                )
            } else {
                (f64::NAN, f64::NAN)
            };
            let flip = if f8.is_finite() && l16.is_finite() {
                if (f8 > 0.0 && l16 < 0.0) || (f8 < 0.0 && l16 > 0.0) {
                    1
                } else {
                    0
                }
            } else {
                0
            };
            println!(
                "E1B_SPLIT,{},{},{},{},{},{},{},{}",
                CFGS8[ci].name,
                name,
                f(f8, 6),
                f(l16, 6),
                f(s8, 6),
                f(s16, 6),
                flip,
                ""
            );
        }
    }
    eprintln!("[E1B] 结束 {:.1}s", t0.elapsed().as_secs_f64());
}

// ─────────────────────────────────────────────────────────────────────────
// E2：伤害时效曲线
// ─────────────────────────────────────────────────────────────────────────

fn cmd_e2() {
    let seeds = arg_seeds(&SEEDS16);
    let t0 = Instant::now();
    let worlds: Vec<WorldB> = par_map(seeds.len(), nthreads(), |i| {
        let s = seeds[i];
        let dorm = calibrate_b(s, E2_STEPS, E2_ATTACK_T, E2_STEPS as u64);
        build_world_b(s, dorm, E2_STEPS, E2_TICK, E2_ROUNDS, E2_ATTACK_T)
    });
    eprintln!(
        "[E2] 世界就绪 {} 个（{:.1}s）",
        worlds.len(),
        t0.elapsed().as_secs_f64()
    );
    let jobs: Vec<(usize, usize)> = (0..worlds.len())
        .flat_map(|wi| (0..E2_CFGS.len()).map(move |ci| (wi, ci)))
        .collect();
    let runs: Vec<E2Run> = par_map(jobs.len(), nthreads(), |j| {
        let (wi, ci) = jobs[j];
        run_e2(&worlds[wi], E2_CFGS[ci])
    });
    eprintln!(
        "[E2] {} 次仿真完成（{:.1}s）",
        runs.len(),
        t0.elapsed().as_secs_f64()
    );

    println!(
        "\n== E2 攻击结束前的基线/峰值（每配置跨 {} seed） ==",
        seeds.len()
    );
    println!("E2_BASE,cfg,seed,dorm,q_pre,q_peak,q_gain,mass_peak,wshare_peak,t_peak");
    for (j, r) in runs.iter().enumerate() {
        let (wi, _ci) = jobs[j];
        println!(
            "E2_BASE,{},{},{:.6},{},{},{},{},{},{}",
            r.cfg,
            r.seed,
            worlds[wi].dorm,
            f(r.q_pre, 6),
            f(r.q_peak, 6),
            f(r.q_peak - r.q_pre, 6),
            f(r.mass_peak, 6),
            f(r.wshare_peak, 6),
            r.t_peak
        );
    }

    println!("\n== E2 快照（攻击结束后 off 小时；mass_ratio = 攻击者份额 ÷ 攻击期峰值） ==");
    println!("E2_SNAP,cfg,seed,off_h,t,mass,mass_ratio,q,q_resid,wshare,wshare_ratio,mean_age,mean_factor,n_obs");
    for r in &runs {
        for s in &r.snaps {
            println!(
                "E2_SNAP,{},{},{},{},{},{},{},{},{},{},{},{},{}",
                s.cfg,
                s.seed,
                s.off,
                s.t,
                f(s.mass, 8),
                f(s.mass_ratio, 6),
                f(s.q, 6),
                f(s.q_resid, 6),
                f(s.wshare, 6),
                f(s.wshare_ratio, 6),
                f(s.mean_age, 1),
                f(s.mean_factor, 5),
                s.n_obs
            );
        }
    }

    println!(
        "\n== E2 聚合（每配置 × 每偏移点，跨 {} seed 的 mean±sd） ==",
        seeds.len()
    );
    println!("E2_AGG,cfg,off_h,metric,n,mean,sd,median");
    let metrics: [(&'static str, fn(&E2Snap) -> f64); 8] = [
        ("mass_ratio", |s| s.mass_ratio),
        ("q_resid", |s| s.q_resid),
        ("wshare_ratio", |s| s.wshare_ratio),
        ("mass", |s| s.mass),
        ("q", |s| s.q),
        ("mean_age", |s| s.mean_age),
        ("mean_factor", |s| s.mean_factor),
        ("n_obs", |s| s.n_obs as f64),
    ];
    for c in E2_CFGS {
        for off in E2_OFFSETS {
            let g: Vec<&E2Snap> = runs
                .iter()
                .filter(|r| r.cfg == c.name)
                .flat_map(|r| r.snaps.iter())
                .filter(|s| s.off == off)
                .collect();
            for (name, getv) in metrics {
                let v: Vec<f64> = g.iter().map(|s| getv(s)).collect();
                println!(
                    "E2_AGG,{},{},{},{},{},{},{}",
                    c.name,
                    off,
                    name,
                    v.iter().filter(|x| x.is_finite()).count(),
                    f(mean(&v), 6),
                    f(sd(&v), 6),
                    f(median(&v), 6)
                );
            }
        }
    }

    // 只统计「攻击真的落地」的 seed（q_peak − q_pre >= MIN_ATTACK_GAIN_Q）。
    // 未落地的 seed 的 q 曲线只是收益方的世俗漂移，会把残差分母抬成噪声。
    println!(
        "\n== E2 聚合（仅落地的 seed；q_gain >= {}） ==",
        MIN_ATTACK_GAIN_Q
    );
    println!("E2_AGG_LANDED,cfg,off_h,metric,n,mean,sd,median");
    for c in E2_CFGS {
        let landed_runs: Vec<&E2Run> = runs
            .iter()
            .filter(|r| r.cfg == c.name && (r.q_peak - r.q_pre) >= MIN_ATTACK_GAIN_Q)
            .collect();
        for off in E2_OFFSETS {
            let g: Vec<&E2Snap> = landed_runs
                .iter()
                .flat_map(|r| r.snaps.iter())
                .filter(|s| s.off == off)
                .collect();
            for (name, getv) in metrics {
                let v: Vec<f64> = g.iter().map(|s| getv(s)).collect();
                println!(
                    "E2_AGG_LANDED,{},{},{},{},{},{},{}",
                    c.name,
                    off,
                    name,
                    v.iter().filter(|x| x.is_finite()).count(),
                    f(mean(&v), 6),
                    f(sd(&v), 6),
                    f(median(&v), 6)
                );
            }
        }
    }

    // 逐 seed 的「衰减到 X% 需要多久」：用 off 点拟合「mass_ratio 衰减到 50% 的时刻」
    println!("\n== E2 衰减半程（mass_ratio 从峰值跌到 50% 的偏移小时；逐 seed） ==");
    println!("E2_HALFLIFE,cfg,seed,t_half_mass,t_half_wshare");
    for r in &runs {
        let cross = |getv: fn(&E2Snap) -> f64| -> f64 {
            let mut prev: Option<(u64, f64)> = None;
            for s in &r.snaps {
                let v = getv(s);
                if !v.is_finite() {
                    continue;
                }
                if v <= 0.5 {
                    if let Some((pt, pv)) = prev {
                        // 线性插值：mass_ratio(t) 在 [pt, s.off] 上近似
                        let span = (s.off - pt) as f64;
                        let denom = pv - v;
                        if denom.abs() > 1e-12 {
                            return pt as f64 + span * (pv - 0.5) / denom;
                        }
                        return s.off as f64;
                    }
                    return s.off as f64;
                }
                prev = Some((s.off, v));
            }
            f64::NAN
        };
        println!(
            "E2_HALFLIFE,{},{},{},{}",
            r.cfg,
            r.seed,
            f(cross(|s| s.mass_ratio), 1),
            f(cross(|s| s.wshare_ratio), 1)
        );
    }
    eprintln!("[E2] 结束 {:.1}s", t0.elapsed().as_secs_f64());
}

// ─────────────────────────────────────────────────────────────────────────
// E3：S5 套利边界
// ─────────────────────────────────────────────────────────────────────────

fn cmd_e3() {
    let seeds = arg_seeds(&SEEDS16);
    let t0 = Instant::now();
    println!("\n== E3 S5 套利边界（合成用例：7 个来源各 1.0，6 个诚实来源陈旧 age 小时、攻击者保持新鲜） ==");
    println!("E3_ROW,H,age_h,seed,measured_over_cap,analytic_ratio,allfresh_over_cap,cap_abs,engine_factor");
    let n_seeds = seeds.len();
    let jobs: Vec<(usize, usize, usize)> = (0..S5_H.len())
        .flat_map(|hi| {
            (0..S5_AGES.len()).flat_map(move |ai| (0..n_seeds).map(move |si| (hi, ai, si)))
        })
        .collect();
    let rows: Vec<(f64, f64, f64)> = par_map(jobs.len(), nthreads(), |j| {
        let (hi, ai, si) = jobs[j];
        let c = Cfg {
            name: "s5",
            enabled: true,
            h: S5_H[hi],
        };
        s5_case(c, S5_AGES[ai], seeds[si])
    });
    for (j, &(m, a, hom)) in rows.iter().enumerate() {
        let (hi, ai, si) = jobs[j];
        let c = Cfg {
            name: "s5",
            enabled: true,
            h: S5_H[hi],
        };
        let e = Engine::new(ecfg(c));
        println!(
            "E3_ROW,{},{},{},{},{},{},{},{},{}",
            c.h,
            S5_AGES[ai],
            seeds[si],
            f(m, 8),
            f(a, 8),
            f(hom, 8),
            f((1.0 - BETA) / 7.0, 8),
            f(e.decay_factor(S5_AGES[ai]) as f64, 8),
            f(m - a, 8)
        );
    }

    println!(
        "\n== E3 聚合（H × age，跨 {} seed 的 mean±sd；并与解析式对照） ==",
        seeds.len()
    );
    println!(
        "E3_AGG,H,age_h,n,measured_mean,measured_sd,analytic,abs_err_mean,abs_err_max,rel_err_pct"
    );
    for hi in 0..S5_H.len() {
        for ai in 0..S5_AGES.len() {
            let v: Vec<(f64, f64, f64)> = (0..jobs.len())
                .filter(|&j| jobs[j].0 == hi && jobs[j].1 == ai)
                .map(|j| rows[j])
                .collect();
            let meas: Vec<f64> = v.iter().map(|x| x.0).collect();
            let an: Vec<f64> = v.iter().map(|x| x.1).collect();
            let err: Vec<f64> = v.iter().map(|x| x.0 - x.1).collect();
            let am = mean(&an);
            let em = mean(&err);
            println!(
                "E3_AGG,{},{},{},{},{},{},{},{},{}",
                S5_H[hi],
                S5_AGES[ai],
                v.len(),
                f(mean(&meas), 8),
                f(sd(&meas), 8),
                f(am, 8),
                f(em, 8),
                f(
                    err.iter()
                        .map(|x| x.abs())
                        .fold(f64::NEG_INFINITY, f64::max),
                    8
                ),
                f(
                    if am.abs() > 1e-12 {
                        100.0 * em / am
                    } else {
                        f64::NAN
                    },
                    3
                )
            );
        }
    }

    // off 对照组（decay_enabled = false 时放大倍数应恒为 1）
    println!("\n== E3 对照：decay_enabled=false 时放大倍数必须恒为 1 ==");
    println!("E3_OFF,age_h,n,measured_mean,measured_sd");
    for &age in &S5_AGES {
        let c = Cfg {
            name: "off",
            enabled: false,
            h: 72,
        };
        let v: Vec<f64> = seeds.iter().map(|&s| s5_case(c, age, s).0).collect();
        println!(
            "E3_OFF,{},{},{},{}",
            age,
            v.len(),
            f(mean(&v), 8),
            f(sd(&v), 8)
        );
    }
    eprintln!("[E3] 结束 {:.1}s", t0.elapsed().as_secs_f64());
}

// ─────────────────────────────────────────────────────────────────────────
// E4：半衰期自适应
// ─────────────────────────────────────────────────────────────────────────

fn cmd_e4() {
    let seeds = arg_seeds(&SEEDS16);
    let t0 = Instant::now();
    // 世界与 T 有关（在线节奏按 tick 采样），所以每个 (seed, T) 一个世界
    let worlds: Vec<WorldE4> = par_map(seeds.len() * E4_T.len(), nthreads(), |i| {
        let si = i / E4_T.len();
        let ti = i % E4_T.len();
        let s = seeds[si];
        let dorm = calibrate_e4(s);
        build_world_e4(s, dorm, E4_T[ti], E4_STEPS as u64 / E4_T[ti])
    });
    eprintln!(
        "[E4] 世界就绪 {} 个（{:.1}s）",
        worlds.len(),
        t0.elapsed().as_secs_f64()
    );

    let jobs: Vec<(usize, usize, usize, usize)> = (0..seeds.len())
        .flat_map(|si| {
            (0..E4_H.len()).flat_map(move |hi| {
                (0..E4_T.len()).flat_map(move |ti| (0..E4_K.len()).map(move |ki| (si, hi, ti, ki)))
            })
        })
        .collect();
    let cells: Vec<CellE4> = par_map(jobs.len(), nthreads(), |j| {
        let (si, hi, ti, ki) = jobs[j];
        let wd = &worlds[si * E4_T.len() + ti];
        run_e4(wd, E4_H[hi], E4_T[ti], E4_K[ki])
    });
    eprintln!(
        "[E4] {} 次仿真完成（{:.1}s）",
        cells.len(),
        t0.elapsed().as_secs_f64()
    );

    println!("\n== E4 逐格（H × T × k；每格 {} seed） ==", seeds.len());
    println!("E4_ROW,H,T,k,seed,n_sources,sweep_h,satisfied,ess,coverage,frac_stale,mean_age,mean_factor,frac_noview");
    for (j, c) in cells.iter().enumerate() {
        let (si, _, _, _) = jobs[j];
        let _ = si;
        println!(
            "E4_ROW,{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            c.h,
            c.t_card,
            c.k,
            c.seed,
            f(c.n_sources, 2),
            f(c.sweep, 1),
            if c.satisfied { 1 } else { 0 },
            f(c.ess, 4),
            f(c.coverage, 4),
            f(c.frac_stale, 4),
            f(c.mean_age, 1),
            f(c.mean_factor, 4),
            f(c.frac_noview, 4),
            ""
        );
    }

    println!(
        "\n== E4 聚合（H × T × k，跨 {} seed 的 mean±sd） ==",
        seeds.len()
    );
    println!("E4_AGG,H,T,k,satisfied,n,ess_mean,ess_sd,coverage_mean,coverage_sd,frac_stale_mean,frac_stale_sd,mean_age_mean,mean_factor_mean,frac_noview_mean");
    for hi in 0..E4_H.len() {
        for ti in 0..E4_T.len() {
            for ki in 0..E4_K.len() {
                let g: Vec<&CellE4> = (0..jobs.len())
                    .filter(|&j| jobs[j].1 == hi && jobs[j].2 == ti && jobs[j].3 == ki)
                    .map(|j| &cells[j])
                    .collect();
                let ess: Vec<f64> = g.iter().map(|c| c.ess).collect();
                let cov: Vec<f64> = g.iter().map(|c| c.coverage).collect();
                let fs: Vec<f64> = g.iter().map(|c| c.frac_stale).collect();
                let ag: Vec<f64> = g.iter().map(|c| c.mean_age).collect();
                let fa: Vec<f64> = g.iter().map(|c| c.mean_factor).collect();
                let nv: Vec<f64> = g.iter().map(|c| c.frac_noview).collect();
                println!(
                    "E4_AGG,{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    E4_H[hi],
                    E4_T[ti],
                    E4_K[ki],
                    if g.first().map(|c| c.satisfied).unwrap_or(false) {
                        1
                    } else {
                        0
                    },
                    g.len(),
                    f(mean(&ess), 4),
                    f(sd(&ess), 4),
                    f(mean(&cov), 4),
                    f(sd(&cov), 4),
                    f(mean(&fs), 4),
                    f(sd(&fs), 4),
                    f(mean(&ag), 1),
                    f(mean(&fa), 4),
                    f(mean(&nv), 4),
                    ""
                );
            }
        }
    }

    // 判据 T·(n/k) ≤ H/2 的验证：满足组 vs 违反组的 ESS
    println!("\n== E4 判据验证：T·(n/k) ≤ H/2 满足组 vs 违反组的 ESS ==");
    println!("E4_RULE,group,n_cells,ess_mean,ess_sd,ess_min,ess_max,coverage_mean,frac_stale_mean");
    for (grp, sel) in [("satisfied", true), ("violated", false)] {
        let g: Vec<&CellE4> = cells.iter().filter(|c| c.satisfied == sel).collect();
        let ess: Vec<f64> = g.iter().map(|c| c.ess).collect();
        let cov: Vec<f64> = g.iter().map(|c| c.coverage).collect();
        let fs: Vec<f64> = g.iter().map(|c| c.frac_stale).collect();
        let xs: Vec<f64> = ess.iter().copied().filter(|x| x.is_finite()).collect();
        println!(
            "E4_RULE,{},n={},ess_mean={},ess_sd={},ess_min={},ess_max={},coverage_mean={},frac_stale_mean={}",
            grp,
            xs.len(),
            f(mean(&ess), 4),
            f(sd(&ess), 4),
            f(xs.iter().copied().fold(f64::INFINITY, f64::min), 4),
            f(xs.iter().copied().fold(f64::NEG_INFINITY, f64::max), 4),
            f(mean(&cov), 4),
            f(mean(&fs), 4),
        );
    }
    // 分组更细：T·(n/k)/H 的比值分箱
    println!("\n== E4 判据细化：(T·(n/k))/H 比值分箱 ==");
    println!(
        "E4_BIN,ratio_lo,ratio_hi,n_cells,ess_mean,coverage_mean,frac_stale_mean,mean_age_mean"
    );
    for (lo, hi) in [
        (0.0, 0.25),
        (0.25, 0.5),
        (0.5, 1.0),
        (1.0, 2.0),
        (2.0, 100.0),
    ] {
        let g: Vec<&CellE4> = cells
            .iter()
            .filter(|c| {
                let ratio = c.sweep / c.h as f64;
                ratio >= lo && ratio < hi
            })
            .collect();
        let ess: Vec<f64> = g.iter().map(|c| c.ess).collect();
        let cov: Vec<f64> = g.iter().map(|c| c.coverage).collect();
        let fs: Vec<f64> = g.iter().map(|c| c.frac_stale).collect();
        let ag: Vec<f64> = g.iter().map(|c| c.mean_age).collect();
        println!(
            "E4_BIN,{},{},n={},ess_mean={},coverage_mean={},frac_stale_mean={},mean_age_mean={}",
            f(lo, 2),
            f(hi, 2),
            g.len(),
            f(mean(&ess), 4),
            f(mean(&cov), 4),
            f(mean(&fs), 4),
            f(mean(&ag), 1),
        );
    }
    eprintln!("[E4] 结束 {:.1}s", t0.elapsed().as_secs_f64());
}

// ─────────────────────────────────────────────────────────────────────────
// main
// ─────────────────────────────────────────────────────────────────────────

fn main() {
    let t0 = Instant::now();
    println!("# P3 · 衰减目的改写为「伤害时效」：E1~E4（真 tm_engine::Engine，无集中式近似）");
    println!(
        "# 参数：N_INIT={} SAMPLES={} BETA={} view=b{}/h{}/seed{} N_SLEEPERS={} SYB_PER={} N_EMPTY={}",
        N_INIT, SAMPLES, BETA, VIEW_BUCKETS, VIEW_HASHERS, VIEW_SEED, N_SLEEPERS, SYB_PER, N_EMPTY
    );
    println!(
        "# 场景 A（S 判据）：steps={} rounds={} tick={}h 潜伏者 t={}h 突变；离线标定目标 {:.0}%",
        A_STEPS,
        A_ROUNDS,
        A_TICK,
        A_NORMAL_UNTIL,
        OFFLINE_TARGET_A * 100.0
    );
    println!(
        "# 场景 B/E1B：steps={} rounds={} tick={}h t={}h 注入 t={}h 转离线；离线标定目标(在 t={}) {:.0}%",
        B_STEPS, B_ROUNDS, B_TICK, B_ATTACK_T, B_DARK_T, B_DARK_T, OFFLINE_TARGET_B * 100.0
    );
    println!(
        "# E2：steps={} rounds={} tick={}h 快照偏移={:?}h",
        E2_STEPS, E2_ROUNDS, E2_TICK, E2_OFFSETS
    );
    println!(
        "# E3：H={:?} ages={:?} 合成用例 7 来源各 1.0",
        S5_H, S5_AGES
    );
    println!(
        "# E4：H={:?} T={:?} k={:?} N={} FOLLOWS={} steps={}",
        E4_H, E4_T, E4_K, E4_N, E4_FOLLOWS, E4_STEPS
    );
    println!("# seed24={:?}", SEEDS24);
    println!("# seed16={:?}", SEEDS16);
    println!("# threads={} stage={}", nthreads(), stage());

    if want("e1a") {
        cmd_e1a();
    }
    if want("e1b") {
        cmd_e1b();
    }
    if want("e2") {
        cmd_e2();
    }
    if want("e3") {
        cmd_e3();
    }
    if want("e4") {
        cmd_e4();
    }
    println!("\n# total {:.0}s", t0.elapsed().as_secs_f64());
}
