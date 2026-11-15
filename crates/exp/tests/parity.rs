//! 核心验收：**框架口径 vs 真引擎口径**的逐点等价。
//!
//! 同一份 [`SimConfig`] 跑两遍：
//!
//! 1. 真 `tm_engine` 多节点网络：每个账户一个 `Engine`，intern 全部 n 个账户
//!    （编号与仿真内部编号一一对应）→ 按自己的评分调 `rate` → 若干轮
//!    `export_view` / `tick` / `on_view` → 最后 `query`；
//! 2. 对齐后的 [`tm_exp::run_case_full`]。
//!
//! 比较口径是**绝对值差**（不是比值）：逐 `(观察者, 目标)` 断言 `|a − b| < 1%`。
//!
//! # 关键技巧：让抽样必然覆盖全员
//!
//! `samples >= 每个观察者的正分清单长度` 时，`RoundSampler` 每轮必然把整个清单抽空，
//! 两边的「谁在第几轮拿到了谁的观点」完全一致 —— RNG 流差异被彻底消除，
//! 测的就只剩公式口径本身（个人项 / 分母 / 衰减 / 读数 / 聚合范围）。
//!
//! `PropConfig` 里三个**框架独有**旋钮必须取「引擎等价」值：
//! `normalize_view = true`、`attacker_normalizes = true`（引擎 `export_view` 恒定归一化）、
//! `receiver_normalizes = true`（引擎 `on_view` 恒定估计总质量）。banlist 一族默认关闭。
//!
//! # 这个测试有多"实"（防自欺）
//!
//! 「差值全 0」也可能是「两边都是 0」。所以每条用例都额外断言三件事：
//!
//! * 引擎侧结果**非平凡**：非零声望点数 ≥ 总数的 1/4，且最大值 ≥ 0.05；
//! * 群体项**真的参与了**：与「只有个人项 β×份额」的对照组相比，
//!   至少有一个点的差 ≥ 0.01（否则这个网络上所有观点都被噪声底吃成 0，
//!   测的就不是传播口径）；
//! * 比较点数 = n²（没有漏比）。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::SimConfig;
use tm_view::{CompressedView, ViewParams};

use tm_exp::{key_of, run_case_full, PropConfig};

const BUCKETS: usize = 2048;
const HASHERS: usize = 5;
const VIEW_SEED: u64 = 12345;

fn params() -> ViewParams {
    ViewParams { buckets: BUCKETS, hashers: HASHERS, seed: VIEW_SEED }
}

/// 引擎等价配置：β = 0.3、H = 72、噪声底倍数 1.0、Mean 聚合。
fn ecfg() -> EngineConfig {
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

/// 框架配置：与 [`ecfg`] 逐字段对齐，另外把三个框架独有旋钮取引擎等价取值。
fn pcfg(rounds: usize, samples: usize) -> PropConfig {
    PropConfig {
        beta: 0.3,
        samples,
        rounds,
        mode: AggMode::Mean,
        half_life: 72,
        decay_enabled: true,
        noise_multiplier: 1.0,
        use_noise_floor: true,
        // 引擎 export_view / on_view 恒定归一化，等价取值是 true。
        normalize_view: true,
        attacker_normalizes: true,
        receiver_normalizes: true,
        // 引擎没有 banlist，等价取值是全部关闭。
        banlist: false,
        ban_lambda: 0.0,
        ban_exclude: 0.0,
        ban_penalty: 0.5,
        ban_floor: 0.2,
    }
}

/// 测试用的小规模仿真配置（默认参数 + 小人口/短步数）。
fn sim_cfg(seed: u64) -> SimConfig {
    SimConfig {
        n_initial: 25,
        steps: 10,
        seed,
        follow_min: 4,
        follow_max: 7,
        max_follows: 12,
        ..Default::default()
    }
}

/// 一次性建好的测试网络：真引擎节点 + 对齐后的框架结果。
struct Network {
    n: usize,
    /// engine_rep[观察者][目标]
    engine_rep: Vec<Vec<f32>>,
    /// 同一个网络、但把所有观点都清空后的「只有个人项」对照组。
    personal_only: Vec<Vec<f32>>,
    /// framework_rep[观察者][目标]
    framework_rep: Vec<Vec<f32>>,
}

/// 建引擎节点：每个账户一个 `Engine`，intern 全部 n 个账户（编号与仿真内部编号一一对应）。
fn build_nodes(world: &tm_sim::World, n: usize) -> Vec<Engine> {
    let ratings = world.ratings();
    let mut nodes: Vec<Engine> = Vec::with_capacity(n);
    for i in 0..n {
        let mut e = Engine::new(ecfg());
        for j in 0..n {
            e.intern(key_of(j));
        }
        // 只写自己的评分。world.ratings() 里同一 (观察者, 被评者) 只留一条，
        // 所以逐条 rate 与存储层的「覆盖」语义一致。
        for &(o, t, s) in &ratings {
            if o == i {
                e.rate(t as u32, s, 0);
            }
        }
        nodes.push(e);
    }
    nodes
}

/// 跑真引擎网络，返回最终声望矩阵。
///
/// 时序与 [`tm_exp::run_case_full`] 完全一致：
///
/// ```text
/// export_view(0)
/// for r in 0..rounds:
///     tick(now = r, k = n) + 对每个请求 on_view(now = r)   // k = n 保证抽空清单
///     export_view(r + 1)
/// query(target, now = rounds)
/// ```
fn run_engine(mut nodes: Vec<Engine>, n: usize, rounds: usize) -> Vec<Vec<f32>> {
    let mut cur: Vec<CompressedView> = nodes.iter().map(|e| e.export_view(0)).collect();
    for r in 0..rounds {
        let now = r as u64;
        for i in 0..n {
            // k = n：一定 >= 每个观察者的清单长度，抽样必然覆盖全员，
            // 两边「第几轮拿到了谁的观点」逐条一致，随机性被完全消除。
            let reqs = nodes[i].tick(now, n);
            for req in &reqs {
                let src = req.idx as usize;
                nodes[i].on_view(req.idx, cur[src].clone(), now, now);
            }
        }
        cur = nodes.iter().map(|e| e.export_view(now + 1)).collect();
    }
    let now = rounds as u64;
    (0..n)
        .map(|i| (0..n).map(|t| nodes[i].query(t as u32, now)).collect())
        .collect()
}

/// 对照：完全没有观点缓存时的结果（群体项为 0，只剩 β × 个人项份额）。
///
/// 这个对照组用来证明上面对比的确实是「传播后的结果」而不是「个人项」：
/// 两者差得越多，说明群体项参与得越深。
fn run_personal_only(nodes: &[Engine], n: usize, rounds: usize) -> Vec<Vec<f32>> {
    let now = rounds as u64;
    (0..n)
        .map(|i| (0..n).map(|t| nodes[i].query(t as u32, now)).collect())
        .collect()
}

fn build(seed: u64, rounds: usize) -> Network {
    let cfg = sim_cfg(seed);
    let mut world = tm_sim::World::new(cfg.clone());
    world.run();
    let n = world.accounts().len();

    let engine_rep = run_engine(build_nodes(&world, n), n, rounds);
    let personal_only = run_personal_only(&build_nodes(&world, n), n, rounds);
    let outcome = run_case_full(cfg, &pcfg(rounds, n), &[], &[], &[]);
    assert_eq!(n, outcome.rep.len(), "账户数必须一致（编号一一对应）");

    Network { n, engine_rep, personal_only, framework_rep: outcome.rep }
}

/// 逐点比较并断言「差异有界、且这个比较不是空对空」。
fn check(seed: u64, rounds: usize) -> f32 {
    let net = build(seed, rounds);
    let n = net.n;
    assert!(n > 0, "网络为空，比较无意义");

    let mut max_diff = 0.0f32;
    let mut max_at = (0usize, 0usize);
    let mut nonzero = 0usize;
    let mut max_value = 0.0f32;
    let mut group_gain = 0.0f32;
    for ob in 0..n {
        for t in 0..n {
            let a = net.engine_rep[ob][t];
            let b = net.framework_rep[ob][t];
            let d = (a - b).abs();
            if d > max_diff {
                max_diff = d;
                max_at = (ob, t);
            }
            if a > 0.0 {
                nonzero += 1;
            }
            max_value = max_value.max(a);
            group_gain = group_gain.max((a - net.personal_only[ob][t]).abs());
        }
    }

    // —— 防「两边都是 0」的自欺：结果必须非平凡，且群体项真的起了作用。
    assert!(
        nonzero * 4 >= n * n,
        "引擎侧结果过于稀疏（{nonzero} / {} 个点非零），这个比较没有说服力",
        n * n
    );
    assert!(max_value >= 0.05, "引擎侧最大声望只有 {max_value}，比较没有说服力");
    assert!(
        group_gain >= 0.01,
        "群体项几乎没有参与（与只有个人项的对照最大差只有 {group_gain}），测不到传播口径"
    );

    println!(
        "seed {seed} / {rounds} 轮：n = {n}，比较 {} 个点；\
         最大绝对差 {max_diff:.3e} @ (观察者 {} → 目标 {})，\
         非零声望 {nonzero}，最大声望 {max_value:.4}，群体项贡献 {group_gain:.4}",
        n * n,
        max_at.0,
        max_at.1
    );
    assert!(
        max_diff < 0.01,
        "框架与引擎的声望差 {max_diff:.3e} 超过 1% @ 观察者 {} → 目标 {}（比较了 {} 个点）",
        max_at.0,
        max_at.1,
        n * n
    );
    max_diff
}

/// 核心验收：基线（无攻击）下框架与引擎逐点绝对值差 < 1%。
#[test]
fn framework_matches_engine_within_one_percent() {
    check(1, 3);
}

/// 换种子重复一次，防止结论只是某一个特定网络的巧合。
#[test]
fn framework_matches_engine_on_another_seed() {
    check(99, 3);
}

/// 跑满 6 轮：验证「观点缓存跨轮累积 + 跨轮衰减」这条口径也对齐
/// （轮次越多，早期观点被新观点覆盖、衰减越明显，最容易暴露口径漂移）。
#[test]
fn framework_matches_engine_after_six_rounds() {
    check(7, 6);
}

/// 抽样只抽 1/4 清单：两边抽到的来源不同，无法逐点比，但结果必须仍在协议内。
///
/// 这个用例的意义是钉住「抽样无偏 + 轮次位图」不会把结果推到非法区间
/// （声望越界、NaN、整行非有限）。
#[test]
fn partial_sampling_stays_within_contract() {
    let n = {
        let mut world = tm_sim::World::new(sim_cfg(3));
        world.run();
        world.accounts().len()
    };
    let outcome = run_case_full(sim_cfg(3), &pcfg(3, (n / 4).max(1)), &[], &[], &[]);
    for (ob, row) in outcome.rep.iter().enumerate() {
        let mut sum = 0.0f32;
        for (t, v) in row.iter().enumerate() {
            assert!((0.0..=1.0).contains(v), "声望越界：观察者 {ob} → 目标 {t} = {v}");
            sum += *v;
        }
        assert!(sum.is_finite(), "观察者 {ob} 的整行出现非有限值");
    }
}
