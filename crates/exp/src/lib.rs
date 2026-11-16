//! 端到端实验框架：把 tm_sim 生产的社交图，喂给 tm_view + tm_agg + tm_store 做信任传播。
//!
//! # 与库口径对齐（B4 之后）
//!
//! 本文件曾经是「集中式近似、不是真 runtime」，它自己的文件头里记着两个已知局限
//! （没有时间衰减、新账户编号按 alive_count() 可能撞号）。这两个局限都已修掉，
//! run_case_full 现在逐条对齐 tm_engine::Engine 的口径：
//!
//! | 口径 | 库（Engine::query / Engine::on_view） | 本文件 |
//! | --- | --- | --- |
//! | 个人项 | s_u ÷ Σs（份额，**不是**原始评分） | 同 |
//! | 群体项分母 | 我评过的**全部**来源；没观点的来源按 (份额, 0.0) 投弃权票 | 同 |
//! | 时间衰减 | w = 评分 × decay(now − last_check)；没观点时因子按 1.0 | 同 |
//! | 读数 | 分子先夹 [0,1] 再除 total_mass、结果再夹、低于噪声底记 0 | 同 |
//! | 视图总质量 | 接收时对 0..n 求和、max(1e-6)，并记 mass_covers | 同 |
//! | 视图覆盖上界 | target >= mass_covers 的读数一律按「这份观点没表态」 | 同 |
//! | 聚合范围 | 观点缓存**跨轮累积**，不是「本轮抽到的 k 个」 | 同 |
//! | 抽样 | tm_store::RoundSampler::draw_backoff（轮次位图 + 拒绝采样，无取模偏差） | 同 |
//!
//! 等价性由 tests/parity.rs 钉住：同一份 SimConfig 同时跑真 tm_engine 多节点网络
//! 与 run_case_full，逐 (观察者, 目标) 比较**绝对值差**（< 1%）。
//!
//! 仍有三处**框架独有**的实验旋钮（引擎里没有对应物，默认取「引擎等价」取值）：
//!
//! - normalize_view / attacker_normalizes：发布侧是否把观点归一化到总量 1。
//!   引擎的 export_view **恒定**归一化，所以引擎等价取值是 true；
//! - receiver_normalizes：接收侧是否按自估总质量 total_mass 归一化读数。
//!   引擎的 on_view **恒定**估计总质量，所以引擎等价取值是 true；
//! - banlist 一族（banlist / ban_lambda / ban_exclude / ban_penalty / ban_floor）：
//!   框架侧的「恶评者降权」实验，引擎等价取值是全部关闭。
//!
//! 三组都取引擎等价取值时，本文件与 tm_engine 的输出逐点重合（差异只剩浮点舍入）。
//!
//! 核心问题没变：原始评分里女巫可能是最高分（集群内互评 1.0），
//! 但经过「只从自己评过分的人里传播」之后，诚实观察者眼中的女巫声望还剩多少？

use std::collections::{BTreeMap, HashSet};

use tm_agg::{aggregate, combine, decay, AggMode};
use tm_sim::{Kind, SimConfig, World};
use tm_store::{AccountIdx, PositiveList, Rng, RoundSampler, MAX_SCORE};
use tm_view::{CompressedView, ViewParams};

pub const PARAMS_BUCKETS: usize = 2048;
pub const PARAMS_HASHERS: usize = 5;

/// 噪声底实测用的采样 key 数，与 tm_engine::on_view 里的 noise_floor(1000, …) 一致。
const NOISE_SAMPLES: usize = 1000;

/// 抽样随机流种子。固定，保证整份实验可复现。
const RNG_SEED: u64 = 0x1234_5678_9ABC_DEF1;

/// 「没有缓存到观点」的哨兵：来源仍按 (份额, 0.0) 进分母，读数为 0。
const NO_VIEW: usize = usize::MAX;

pub fn view_params() -> ViewParams {
    ViewParams { buckets: PARAMS_BUCKETS, hashers: PARAMS_HASHERS, seed: 12345 }
}

/// 由内部编号合成一个 32 字节账户号（实验用）。
pub fn key_of(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

/// 与 tm_engine 内部的 noise_seed **逐位一致**（那边是私有函数，只能在这里复刻）。
///
/// 噪声底依赖「采样哪些假 key」，两边必须用同一个种子，否则读数会不会被噪声底压成
/// 0 就会出现随机分歧 —— 这正是「框架 vs 引擎 1% 对齐」测试最容易被忽略的破口。
fn noise_seed(idx: usize) -> u64 {
    (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

/// 传播参数。
///
/// 默认值**整体对齐** tm_engine::EngineConfig::default()：β = 0.3、H = 72h、衰减开、
/// 噪声底倍数 1.0、Mean 聚合，以及三个框架独有旋钮都取「引擎等价」取值
/// （发布/接收两侧都归一化，banlist 全关）。
/// 想要偏离库口径的实验，必须**显式**把这些字段改掉 —— 默认路径不再偷偷跑另一套语义。
#[derive(Clone, Debug)]
pub struct PropConfig {
    pub beta: f32,
    /// 每轮抽样数。**大于等于**每个观察者的正分清单长度时，所有人每轮都会被抽到，
    /// 抽样随机性的影响被完全消除，测的就只剩公式口径（见 tests/parity.rs）。
    pub samples: usize,
    pub rounds: usize,
    pub mode: AggMode,
    /// 来源级时间衰减的半衰期，与仿真 Time 同单位（小时）。引擎默认 72。
    pub half_life: u64,
    /// 是否启用时间衰减。false 时因子恒为 1（等价引擎 decay_enabled = false）。
    pub decay_enabled: bool,
    /// 噪声底倍数：读数低于 noise.median × 它 就按「没表态」记 0。引擎默认 1.0。
    pub noise_multiplier: f32,
    /// 是否启用噪声底（引擎恒定开启，这里留开关做消融）。
    pub use_noise_floor: bool,
    /// 是否启用 banlist 审查：来源若抬举我屏蔽的账户，就调低我对它的评分
    pub banlist: bool,
    /// 单轮最大降权幅度
    pub ban_penalty: f32,
    /// 降权下限（避免一路降到 0）
    pub ban_floor: f32,
    /// 硬排除阈值：覆盖率超过它的来源**直接剔除**（0 表示不启用）
    pub ban_exclude: f32,
    /// 指数降权强度 λ：倍数 = exp(−λ × 该来源给攻击者的平均分)。0 表示关闭
    pub ban_lambda: f32,
    /// 影响力预算：把来源发布的观点归一化到总量 1（打的人越多每份越轻）
    pub normalize_view: bool,
    /// 攻击者是否也做归一化（false = 攻击者不归一化，试图放大影响力）
    pub attacker_normalizes: bool,
    /// 接收方是否按「估计的总质量」再归一化一次（防发布方不归一化）
    pub receiver_normalizes: bool,
    /// 一轮代表多少小时（默认 1）。引擎侧等价写法是 `tick(now = r × step, …)`。
    ///
    /// 取 72（= 一个半衰期）时，相邻两轮的观点年龄相差一个半衰期，衰减因子相差一倍，
    /// 衰减口径才真正参与结果（默认 1 小时时它只造成千分之几的差异）。
    pub round_time_step: u64,
    /// **验收测试专用**：最后一轮只刷新 `来源号 % stride == 0` 的观点，其余来源保留更旧的观点。
    ///
    /// 默认 1 = 每轮刷新全部来源，与引擎「抽到就 on_view」的时序完全等价。
    /// 取 2 时，一半来源的 `last_check` 会比另一半**旧一个轮次**，
    /// 衰减因子因此不再对所有来源相同 —— 加权平均不会再把公共因子约掉，
    /// 「框架有没有实现衰减」这个口径才真正被测到。
    ///
    /// 没有这个旋钮的话，「全员每轮都被抽到」的时序会让所有来源的 `Δt` 相同，
    /// 衰减作为一个公共因子被加权平均约掉：**删掉衰减实现，逐点对比也照样是 0 差异**。
    pub last_round_refresh_stride: usize,
}

impl Default for PropConfig {
    fn default() -> Self {
        Self {
            beta: 0.3,
            samples: 8,
            rounds: 4,
            mode: AggMode::Mean,
            half_life: 72,
            decay_enabled: true,
            noise_multiplier: 1.0,
            use_noise_floor: true,
            banlist: false,
            ban_penalty: 0.5,
            ban_floor: 0.2,
            ban_exclude: 0.0,
            ban_lambda: 0.0,
            // 引擎的 export_view / on_view 恒定归一化，默认就按引擎来。
            normalize_view: true,
            attacker_normalizes: true,
            receiver_normalizes: true,
            // 1 = 每轮刷新全部来源，与引擎时序等价。
            round_time_step: 1,
            last_round_refresh_stride: 1,
        }
    }
}

/// 一次传播后的结果。
pub struct Outcome {
    /// rep[observer][target]
    pub rep: Vec<Vec<f32>>,
    pub honest: Vec<usize>,
    pub others: Vec<usize>,
    /// 原始评分里，每类账户收到的均分
    pub raw_honest: f32,
    pub raw_others: f32,
}

// ===========================================================================
// 引擎口径的镜像结构
// ===========================================================================

/// 一份已发布观点在框架内的等价物：预先算好的读数行，草图本体用完即弃。
///
/// 读数行本身就是 Engine::query 里 reading_of 的结果（含总质量归一化、二次夹紧、
/// 噪声底），所以接收方缓存到它之后，查询时不需要再碰 Count Sketch。
#[derive(Clone)]
struct Published {
    /// 该来源对 0..n 每个账户号的读数（与引擎 reading_of 同口径）。
    readings: Vec<f32>,
    /// 对「攻击方账户」的**原始读数** clamp(0,1)（只给 banlist 用，引擎里没有对应物）。
    raw_others: Vec<f32>,
    /// 覆盖率：raw_others 里 > 0.05 的比例（banlist / ban_exclude 用）。
    cov: f32,
}

/// 观察者缓存的一份观点（引擎 StoredView 的镜像，去掉草图本体）。
#[derive(Clone, Copy)]
struct StampedView {
    /// 这份观点来自哪一代发布（arena 下标 = generation × n + src）。
    generation: usize,
    /// 我上次成功取到它的时间（衰减基准）。
    last_check: u64,
    /// 这份观点的总质量覆盖到多少个账户（编号上界）。
    mass_covers: usize,
}

/// 一个观察者的本地状态：正分清单 + 观点缓存 + 轮次抽样器。
struct Node {
    /// 正分清单（引擎 PositiveList 的镜像）：抽样、份额、个人项都读它。
    pl: PositiveList,
    /// 观点缓存（引擎 views: BTreeMap 的镜像）。用定长 Vec 换 O(1) 下标，
    /// 因为查询时遍历的是正分清单、不是缓存表（引擎也是这个顺序）。
    stored: Vec<Option<StampedView>>,
    /// 轮次抽样器（引擎 RoundSampler 的镜像）。
    sampler: RoundSampler,
}

/// 一次查询预先算好的「来源权重表」。
///
/// 引擎每次 query 都要重新遍历一遍正分清单、查一次观点缓存、算一次衰减；
/// 对齐口径之后这个准备工作与 target 无关，所以按 (观察者, 时刻) 预先算一次，
/// 之后每个 target 只做「查读数 + 聚合」。这是把 O(n²·轮数) 压下来的关键。
struct Prepared {
    /// 参与群体项的来源：(来源号, 权重, 读数行起点, 该观点覆盖的账户上界)。
    /// 行起点为 NO_VIEW 表示「我没有这份观点」——权重照样进分母，读数恒 0。
    sources: Vec<(usize, f32, usize, usize)>,
    /// 个人项分母：我给出的全部评分之和。按 f32 顺序累加，与 Engine::query 同口径。
    total_rating: f32,
}

/// 一轮内不变的全局参数。
struct Net<'a> {
    n: usize,
    keys: &'a [[u8; 32]],
    pc: &'a PropConfig,
    others: &'a [usize],
    is_atk: &'a [bool],
    /// 是否需要发布 banlist 用的原始读数。
    need_ban: bool,
    /// 是否需要乘 banlist 折扣。
    need_mul: bool,
}

/// 时间衰减因子，与 Engine::decay_factor 同口径（引擎默认 decay_floor = 0）。
fn decay_factor(pc: &PropConfig, dt: u64) -> f32 {
    if pc.decay_enabled {
        decay(dt, pc.half_life)
    } else {
        1.0
    }
}

/// 时间回拨一律夹到 0，与 Engine 里的 elapsed 一致。
fn elapsed(start: u64, now: u64) -> u64 {
    now.saturating_sub(start)
}

/// 单个读数，与 Engine::reading_of **逐条同口径**：
/// 分子先夹 [0,1]，再除以总质量，结果再夹一次，最后套噪声底。
fn reading_value(raw_query: f32, total_mass: f32, floor: f32) -> f32 {
    // 分子先夹紧，与分母 Σmin(q,1) 同一口径（顺序反了会让超量声明拿走 K 倍预算）。
    let raw = raw_query.clamp(0.0, 1.0) / total_mass;
    let mut v = if raw.is_finite() { raw.clamp(0.0, 1.0) } else { 0.0 };
    if v < floor {
        v = 0.0;
    }
    v
}

impl<'a> Net<'a> {
    /// 预先算一次「来源权重表」，语义逐条对齐 Engine::query 里的那个循环。
    fn prepare(
        &self,
        node: &Node,
        arena: &[Published],
        now: u64,
        mul_row: Option<&[f32]>,
        fallback_gen: Option<usize>,
    ) -> Prepared {
        // 个人项分母：f32 顺序累加（Engine::query 就是这么写的，不能用 f64 口径）。
        let total_rating: f32 = node.pl.iter().map(|(_, s, _)| s).sum();
        let mut sources = Vec::with_capacity(node.pl.len());
        // 遍历的是**正分清单**（我评过谁），不是观点缓存（我手上有谁的观点）：
        // 没有观点的来源贡献 (份额, 0.0)，而不是从分母里消失。
        for (src_idx, w_rating, _) in node.pl.iter() {
            let src = src_idx as usize;
            let (row_start, mass_covers, decay_k) = match node.stored[src] {
                Some(st) => (
                    st.generation * self.n,
                    st.mass_covers,
                    // 衰减基准是 last_check（我自己观察到的），不是对方声称的发布时间。
                    decay_factor(self.pc, elapsed(st.last_check, now)),
                ),
                // 没有观点：没有衰减基准，因子按 1。权重照样进分母，读数按 0。
                None => (NO_VIEW, 0, 1.0),
            };
            let w_rating = w_rating * decay_k;
            let w = match mul_row {
                // banlist 折扣只压来源的话语权，不改我的原始评分（框架独有旋钮）。
                Some(m) => w_rating * m[src],
                None => w_rating,
            };
            if !w.is_finite() || w <= 0.0 {
                continue;
            }
            if self.pc.ban_exclude > 0.0 {
                // 硬排除：覆盖率超阈值的来源直接剔除（框架独有旋钮）。
                //
                // 这里**必须按来源 id 取覆盖率**，不能按「条目在聚合列表里的下标」
                // 与别的东西配对：旧实现在聚合列表上按下标 zip 回抽样结果，
                // 一旦有来源因「没表态」被跳过，列表就比抽样结果短，
                // 权重会被配到错误的来源上（旧文件头里那条 FIXME）。
                let cov = if row_start != NO_VIEW {
                    arena[row_start + src].cov
                } else {
                    match fallback_gen {
                        Some(g) => arena[g * self.n + src].cov,
                        None => 0.0,
                    }
                };
                if cov > self.pc.ban_exclude {
                    continue;
                }
            }
            sources.push((src, w, row_start, mass_covers));
        }
        Prepared { sources, total_rating }
    }

    /// 用预准备好的权重表查一个 target，语义等同 Engine::query(target, now)。
    fn query(
        &self,
        node: &Node,
        prep: &Prepared,
        target: usize,
        arena: &[Published],
        items: &mut Vec<(f32, f32)>,
    ) -> f32 {
        items.clear();
        for &(src, w, row_start, mass_covers) in &prep.sources {
            // 这份观点在接收时只统计了 0..mass_covers 号账户，之后才登记的账户
            // 没有进过分母，用偏小的分母归一化会把它们抬到 1.0 —— 一律按没表态处理。
            let v = if row_start != NO_VIEW && target < mass_covers {
                arena[row_start + src].readings[target]
            } else {
                0.0
            };
            items.push((w, v));
        }
        let raw_personal = node.pl.get(target as AccountIdx).unwrap_or(0.0);
        // 个人项 = 份额（评分 ÷ Σ评分），不是原始评分：引擎口径是 personal = s_u/Σs。
        // 曾用变异测试验证过这条断言有判别力：把这一行换成 `let personal = raw_personal;`
        // 后 `personal_term_is_share_not_raw_score` 与 `mixed_view_and_abstention_match_engine`
        // 都会立刻变红（实测），故此处必须保持除法。
        let personal = if prep.total_rating > 0.0 {
            raw_personal / prep.total_rating
        } else {
            0.0
        };
        let group = aggregate(&items[..], self.pc.mode);
        combine(self.pc.beta, personal, group)
    }
}

/// 发布一代观点：等价于每个账户各调一次 Engine::export_view(now)。
///
/// 结果按来源号升序追加进 arena，下标 = gen × n + src。
/// fallback_gen 是「这一代查询时，读的是哪一代观点」，banlist 的覆盖率取自它。
fn publish_generation(
    net: &Net,
    nodes: &[Node],
    mul: &[Vec<f32>],
    arena: &mut Vec<Published>,
    gen: usize,
    now: u64,
    fallback_gen: Option<usize>,
) {
    let n = net.n;
    debug_assert_eq!(arena.len(), gen * n, "每一代恰好 n 份观点");
    let mut items: Vec<(f32, f32)> = Vec::with_capacity(n);
    for src in 0..n {
        let mul_row = if net.need_mul {
            debug_assert!(!mul.is_empty(), "need_mul 时折扣表必须有行");
            Some(&mul[src][..])
        } else {
            None
        };
        let prep = net.prepare(&nodes[src], &arena[..], now, mul_row, fallback_gen);

        // —— 等价 Engine::export_view：对全部账户各查一次，只留 > 0，再归一化到总量 1。
        let mut vals: Vec<([u8; 32], f32)> = Vec::with_capacity(n);
        let mut sum = 0.0f32;
        for u in 0..n {
            let q = net.query(&nodes[src], &prep, u, &arena[..], &mut items);
            if q > 0.0 {
                sum += q;
                vals.push((net.keys[u], q));
            }
        }
        let do_norm = if net.is_atk[src] { net.pc.attacker_normalizes } else { net.pc.normalize_view };
        if do_norm && sum > 0.0 {
            for x in vals.iter_mut() {
                x.1 /= sum;
            }
        }
        let view = CompressedView::from_pairs(view_params(), &vals);

        // —— 等价 Engine::on_view 的三件套：自估总质量、实测噪声底、覆盖上界。
        let qs: Vec<f32> = (0..n).map(|u| view.query(&net.keys[u])).collect();
        let total_mass = if net.pc.receiver_normalizes {
            qs.iter().map(|q| q.clamp(0.0, 1.0)).sum::<f32>().max(1e-6)
        } else {
            1.0
        };
        let floor = if net.pc.use_noise_floor {
            (view.noise_floor(NOISE_SAMPLES, noise_seed(src)).median * net.pc.noise_multiplier).max(0.0)
        } else {
            0.0
        };
        let readings: Vec<f32> = qs.iter().map(|&q| reading_value(q, total_mass, floor)).collect();

        // —— banlist 需要的原始读数与覆盖率（引擎等价配置下 need_ban = false，不做功）。
        let (raw_others, cov) = if net.need_ban {
            let raw: Vec<f32> = net
                .others
                .iter()
                .map(|&b| view.query(&net.keys[b]).clamp(0.0, 1.0))
                .collect();
            let hit = raw.iter().filter(|v| **v > 0.05).count();
            let cov = if net.others.is_empty() { 0.0 } else { hit as f32 / net.others.len() as f32 };
            (raw, cov)
        } else {
            (Vec::new(), 0.0)
        };

        arena.push(Published { readings, raw_others, cov });
    }
}

/// banlist 审查（框架独有实验旋钮，引擎里没有对应物）。
///
/// 只看**刚发布**的这一代观点，更新「观察者 i 对来源 s 的折扣」表 mul[i][s]，
/// 供下一轮查询使用 —— 与改造前的时序一致。
fn update_mul(net: &Net, nodes: &[Node], mul: &mut [Vec<f32>], arena: &[Published], gen: usize) {
    if net.others.is_empty() || !net.need_mul {
        return;
    }
    let n = net.n;
    if net.pc.ban_lambda > 0.0 {
        // 指数降权：倍数 = exp(−λ × 该来源给攻击者的平均分)。给得越高降得越狠。
        let mut peer_mul = vec![1.0f32; n];
        for s in 0..n {
            // 没表过态（清单为空）→ 发布的是空观点，不受罚；这里沿用旧口径直接置 0。
            if nodes[s].pl.is_empty() {
                peer_mul[s] = 0.0;
                continue;
            }
            let raw = &arena[gen * n + s].raw_others;
            let mut acc = 0.0f32;
            let mut cnt = 0usize;
            for &v in raw {
                if v > 0.05 {
                    acc += v;
                    cnt += 1;
                }
            }
            let mean = if cnt > 0 { acc / cnt as f32 } else { 0.0 };
            peer_mul[s] = (-net.pc.ban_lambda * mean).exp();
        }
        for i in 0..n {
            for (src_idx, _, _) in nodes[i].pl.iter() {
                mul[i][src_idx as usize] = peer_mul[src_idx as usize];
            }
        }
        return;
    }
    if net.pc.banlist {
        // 旧的覆盖率式软降权：折扣 = max(1 − 惩罚 × 覆盖率, 下限)，只降不升。
        for i in 0..n {
            for (src_idx, _, _) in nodes[i].pl.iter() {
                let s = src_idx as usize;
                let cov = arena[gen * n + s].cov;
                let target = (1.0 - net.pc.ban_penalty * cov).max(net.pc.ban_floor);
                if target < mul[i][s] {
                    mul[i][s] = target;
                }
            }
        }
    }
}

/// 在某时刻算出全部 (观察者, 目标) 的声望矩阵（等价于每个账户各调一次 Engine::query）。
fn evaluate(
    net: &Net,
    nodes: &[Node],
    mul: &[Vec<f32>],
    arena: &[Published],
    now: u64,
    fallback_gen: Option<usize>,
) -> Vec<Vec<f32>> {
    let n = net.n;
    let mut rep = vec![vec![0.0f32; n]; n];
    let mut items: Vec<(f32, f32)> = Vec::with_capacity(n);
    for i in 0..n {
        let mul_row = if net.need_mul { Some(&mul[i][..]) } else { None };
        let prep = net.prepare(&nodes[i], arena, now, mul_row, fallback_gen);
        for u in 0..n {
            rep[i][u] = net.query(&nodes[i], &prep, u, arena, &mut items);
        }
    }
    rep
}

/// 跑一次完整的「仿真 → 传播」流程。
pub fn run_case(cfg: SimConfig, pc: &PropConfig) -> Outcome {
    run_case_inject(cfg, pc, &[], &[])
}

/// 可注入额外评分与额外账户的版本。
/// inject: (评分者, 被评者, 分数)；extra_accounts 只用于抬大 n。
pub fn run_case_inject(cfg: SimConfig, pc: &PropConfig, inject: &[(usize, usize, f32)], extra_accounts: &[usize]) -> Outcome {
    run_case_full(cfg, pc, inject, extra_accounts, &[])
}

/// 引擎等价的「仿真 → 传播」。
///
/// 时序与 tests/parity.rs 里的真 tm_engine 网络完全一致：
///
/// ```text
/// publish(0)                          // = 每个账户 export_view(0)
/// for r in 0..rounds:
///     receive(publish(r), now = r)    // = tick(k) + 对每个来源 on_view
///     publish(r + 1)                  // = export_view(r + 1)
/// rep = evaluate(now = rounds)        // = 最后一批观点在 rounds-1 取到，Δt = 1
/// ```
pub fn run_case_full(cfg: SimConfig, pc: &PropConfig, inject: &[(usize, usize, f32)], extra_accounts: &[usize], attacker_ids: &[usize]) -> Outcome {
    let mut world = World::new(cfg);
    world.run();
    let observed = world.ratings();

    // 账户号上界：账户表只增不删，所以基数必须取 accounts().len()。
    // 用 alive_count() 会在有人离开时与已有编号**撞号**（B4 修掉的老 bug），
    // 用 ratings 里的最大编号又会漏掉「一条正分都没收到」的账户。
    let mut n = world.accounts().len();
    for &(o, t, _) in observed.iter().chain(inject.iter()) {
        n = n.max(o + 1).max(t + 1);
    }
    for &e in extra_accounts {
        n = n.max(e + 1);
    }

    let honest: Vec<usize> = world.indices_of(Kind::Honest).into_iter().filter(|i| *i < n).collect();
    let honest_set: HashSet<usize> = honest.iter().copied().collect();

    // 每个观察者的正分清单（按目标号升序、同一目标只留一条）：
    // - 同一 (观察者, 被评者) 只保留最后一条 —— 等价于 Engine::rate 的覆盖语义；
    // - 非有限 / <= 0 的分数等价于删除该条目 —— 与 PositiveList::set 一致；
    // - 超过 tm_store::MAX_SCORE 截断，与存储层上界一致。
    let mut maps: Vec<BTreeMap<usize, f32>> = (0..n).map(|_| BTreeMap::new()).collect();
    for &(o, t, s) in observed.iter().chain(inject.iter()) {
        if o >= n || t >= n {
            continue;
        }
        if !s.is_finite() || s <= 0.0 {
            maps[o].remove(&t);
            continue;
        }
        maps[o].insert(t, s.min(MAX_SCORE));
    }

    let mut nodes: Vec<Node> = Vec::with_capacity(n);
    for m in maps.into_iter() {
        let mut pl = PositiveList::new();
        for (&t, &s) in &m {
            pl.set(t as AccountIdx, s, 0);
        }
        nodes.push(Node { pl, stored: vec![None; n], sampler: RoundSampler::new() });
    }

    // 原始评分口径的对照统计（与传播结果无关，仅供报表）。
    let mut others: Vec<usize> = Vec::new();
    let mut raw_sum_h = 0.0f64;
    let mut raw_cnt_h = 0usize;
    let mut raw_sum_o = 0.0f64;
    let mut raw_cnt_o = 0usize;
    for nd in &nodes {
        for (t, s, _) in nd.pl.iter() {
            let t = t as usize;
            if honest_set.contains(&t) {
                raw_sum_h += f64::from(s);
                raw_cnt_h += 1;
            } else {
                raw_sum_o += f64::from(s);
                raw_cnt_o += 1;
                others.push(t);
            }
        }
    }
    others.sort_unstable();
    others.dedup();

    let is_atk: Vec<bool> = (0..n).map(|i| attacker_ids.contains(&i)).collect();
    let keys: Vec<[u8; 32]> = (0..n).map(key_of).collect();

    let need_mul = !others.is_empty() && (pc.ban_lambda > 0.0 || pc.banlist);
    let need_ban = !others.is_empty() && (pc.ban_lambda > 0.0 || pc.banlist || pc.ban_exclude > 0.0);
    let net = Net { n, keys: &keys, pc, others: &others, is_atk: &is_atk, need_ban, need_mul };

    // 折扣表缺省 1.0；引擎等价配置下 need_mul = false，连分配都省掉。
    let mut mul: Vec<Vec<f32>> = if need_mul { vec![vec![1.0f32; n]; n] } else { Vec::new() };
    // 每代 n 份观点的读数行，下标 = generation × n + src。跨轮累积（读较旧的观点依然有效）。
    let mut arena: Vec<Published> = Vec::new();

    // 初始观点 = export_view(0)：此刻没有任何缓存观点，群体项为 0，结果退化为 β×个人项。
    publish_generation(&net, &nodes, &mul, &mut arena, 0, 0, None);

    let mut cur_gen = 0usize;
    let mut rng = Rng::new(RNG_SEED);
    let step = pc.round_time_step.max(1);
    for r in 0..pc.rounds {
        let now = r as u64 * step;
        // —— 接收：按真引擎的轮次抽样器抽 k 个来源，抽到的立刻算「上次检查 = now」
        // （等价 tick + on_view）。框架里没有失败路径，所以连败恒为 0，
        // draw_backoff 退化成无放回均匀抽样；抽样偏差已被拒绝采样消除。
        for i in 0..n {
            let node = &mut nodes[i];
            let ids: Vec<AccountIdx> = node.pl.indices();
            node.sampler.sync(&ids);
            let picked = node.sampler.draw_backoff(&node.pl, pc.samples, &mut rng);
            // 最后一轮可以按 stride 只刷新一部分来源（验收测试用，默认 1 = 全刷）。
            let stride = if r + 1 == pc.rounds { pc.last_round_refresh_stride.max(1) } else { 1 };
            for src in picked {
                if (src as usize) % stride != 0 {
                    continue;
                }
                node.stored[src as usize] =
                    Some(StampedView { generation: cur_gen, last_check: now, mass_covers: n });
            }
        }
        // —— 发布下一轮的观点（旧代码在最后一轮也就停在这里，之后的 mul 更新已无消费者）
        if r + 1 < pc.rounds {
            let next = cur_gen + 1;
            publish_generation(&net, &nodes, &mul, &mut arena, next, (r + 1) as u64 * step, Some(cur_gen));
            cur_gen = next;
            if need_mul {
                update_mul(&net, &nodes, &mut mul, &arena, cur_gen);
            }
        }
    }

    // —— 最终结果：与真引擎网络一样，在 now = rounds 处查一次（最后一批观点的年龄是 1）。
    let rep = evaluate(&net, &nodes, &mul, &arena, pc.rounds as u64 * step, Some(cur_gen));

    Outcome {
        rep,
        honest,
        others,
        raw_honest: if raw_cnt_h > 0 { (raw_sum_h / raw_cnt_h as f64) as f32 } else { 0.0 },
        raw_others: if raw_cnt_o > 0 { (raw_sum_o / raw_cnt_o as f64) as f32 } else { 0.0 },
    }
}

/// 从「诚实观察者」视角统计：某类账户平均拿到多少声望。
pub fn mean_rep_from_honest(o: &Outcome, targets: &[usize]) -> f32 {
    if o.honest.is_empty() || targets.is_empty() { return 0.0; }
    let mut sum = 0.0f64;
    let mut cnt = 0usize;
    for &ob in &o.honest {
        for &t in targets {
            if ob < o.rep.len() && t < o.rep[ob].len() {
                sum += o.rep[ob][t] as f64;
                cnt += 1;
            }
        }
    }
    if cnt == 0 { 0.0 } else { (sum / cnt as f64) as f32 }
}

/// 观察者胜率：同一观察者眼中，诚实均值 > 攻击者均值的比例。
pub fn win_rate(o: &Outcome) -> (f32, usize) {
    let mut win = 0usize;
    let mut tot = 0usize;
    for &ob in &o.honest {
        if ob >= o.rep.len() { continue; }
        let h: Vec<f32> = o.honest.iter().filter(|&&t| t < o.rep[ob].len()).map(|&t| o.rep[ob][t]).collect();
        let a: Vec<f32> = o.others.iter().filter(|&&t| t < o.rep[ob].len()).map(|&t| o.rep[ob][t]).collect();
        if h.is_empty() || a.is_empty() { continue; }
        let hm = h.iter().sum::<f32>() / h.len() as f32;
        let am = a.iter().sum::<f32>() / a.len() as f32;
        tot += 1;
        if hm > am { win += 1; }
    }
    if tot == 0 { (0.0, 0) } else { (win as f32 / tot as f32, tot) }
}
// ===========================================================================
// 单元测试：混合「有观点 / 没观点」时，框架的查询路径必须与引擎逐点一致
// ===========================================================================

#[cfg(test)]
mod engine_parity_unit {
    use super::*;
    use tm_engine::{Engine, EngineConfig};

    fn engine_cfg() -> EngineConfig {
        EngineConfig {
            beta: 0.3,
            half_life: 72,
            view_params: view_params(),
            agg_mode: AggMode::Mean,
            noise_multiplier: 1.0,
            decay_enabled: true,
            decay_floor: 0.0,
            ..Default::default()
        }
    }

    /// 把一个 CompressedView 变成框架侧的读数行：与 publish_generation 里的三步完全一致。
    ///
    /// known 是 on_view 那一刻的已知账户数（= 引擎的 accounts.len()），
    /// 总质量只对前 known 个账户求和。
    fn readings_of(view: &CompressedView, keys: &[[u8; 32]], known: usize, n: usize, floor: f32) -> Vec<f32> {
        let mut total_mass = 0.0f32;
        for i in 0..known {
            total_mass += view.query(&keys[i]).clamp(0.0, 1.0);
        }
        let total_mass = total_mass.max(1e-6);
        (0..n)
            .map(|u| reading_value(view.query(&keys[u]), total_mass, floor))
            .collect()
    }

    /// 构造一个很小的场景：观察者 0 评了 1 和 2，但只收到了 1 的观点。
    ///
    /// 这条用例专门钉住对齐表里最难被「全员抽样」用例覆盖的两条口径：
    /// * **弃权票**：来源 2 没有观点，仍按 (份额, 0.0) 进群体项分母；
    /// * **覆盖上界**：观点在只有 4 个账户时收到，之后才登记的第 5 个账户按没表态处理。
    #[test]
    fn mixed_view_and_abstention_match_engine() {
        const KNOWN: usize = 4;
        const N: usize = 5;
        let keys: Vec<[u8; 32]> = (0..N).map(key_of).collect();
        let view = CompressedView::from_pairs(view_params(), &[(keys[3], 0.8), (keys[1], 0.4)]);
        let floor = (view.noise_floor(NOISE_SAMPLES, noise_seed(1)).median * 1.0).max(0.0);

        // —— 框架侧：观察者 0 的正分清单 = {1: 1.0, 2: 0.5}，只缓存了来源 1 的观点。
        let mut pl = PositiveList::new();
        pl.set(1, 1.0, 0);
        pl.set(2, 0.5, 0);
        let mut stored = vec![None; N];
        stored[1] = Some(StampedView { generation: 0, last_check: 0, mass_covers: KNOWN });
        let node = Node { pl, stored, sampler: RoundSampler::new() };
        let empty = Published { readings: vec![0.0; N], raw_others: Vec::new(), cov: 0.0 };
        let arena = vec![
            Published { readings: readings_of(&view, &keys, KNOWN, N, floor), raw_others: Vec::new(), cov: 0.0 },
            Published { readings: readings_of(&view, &keys, KNOWN, N, floor), raw_others: Vec::new(), cov: 0.0 },
            empty.clone(),
            empty.clone(),
            empty,
        ];
        let pc = PropConfig::default();
        let is_atk = [false; N];
        let net = Net { n: N, keys: &keys, pc: &pc, others: &[], is_atk: &is_atk, need_ban: false, need_mul: false };
        let mut items: Vec<(f32, f32)> = Vec::new();

        // —— 引擎侧：同样的评分、同样的观点（只在 4 个账户时就收到），随后登记第 5 个账户。
        let mut e = Engine::new(engine_cfg());
        for i in 0..KNOWN {
            e.intern(keys[i]);
        }
        e.rate(1, 1.0, 0);
        e.rate(2, 0.5, 0);
        e.on_view(1, view.clone(), 0, 0);
        e.intern(keys[4]);

        for now in [0u64, 36, 72, 240] {
            let prep = net.prepare(&node, &arena, now, None, Some(0));
            for target in 0..N {
                let mine = net.query(&node, &prep, target, &arena, &mut items);
                let theirs = e.query(target as AccountIdx, now);
                assert!(
                    (mine - theirs).abs() < 1e-6,
                    "now = {now}, 目标 {target}: 框架 {mine} vs 引擎 {theirs}（差值 {}）",
                    (mine - theirs).abs()
                );
            }
        }
    }

    /// 同一份数据、同一个结论，但把衰减关掉：验证 decay_enabled 开关等价于引擎的同一开关。
    #[test]
    fn decay_switch_matches_engine() {
        let keys: Vec<[u8; 32]> = (0..3).map(key_of).collect();
        let view = CompressedView::from_pairs(view_params(), &[(keys[2], 1.0)]);
        let floor = (view.noise_floor(NOISE_SAMPLES, noise_seed(1)).median * 1.0).max(0.0);
        let row = readings_of(&view, &keys, 3, 3, floor);
        let arena = vec![
            Published { readings: row.clone(), raw_others: Vec::new(), cov: 0.0 },
            Published { readings: row.clone(), raw_others: Vec::new(), cov: 0.0 },
            Published { readings: row, raw_others: Vec::new(), cov: 0.0 },
        ];

        for decay_enabled in [true, false] {
            let mut pc = PropConfig::default();
            pc.decay_enabled = decay_enabled;
            let mut pl = PositiveList::new();
            pl.set(1, 1.0, 0);
            let mut stored = vec![None; 3];
            stored[1] = Some(StampedView { generation: 0, last_check: 0, mass_covers: 3 });
            let node = Node { pl, stored, sampler: RoundSampler::new() };
            let is_atk = [false; 3];
            let net = Net { n: 3, keys: &keys, pc: &pc, others: &[], is_atk: &is_atk, need_ban: false, need_mul: false };
            let mut items: Vec<(f32, f32)> = Vec::new();

            let mut cfg = engine_cfg();
            cfg.decay_enabled = decay_enabled;
            let mut e = Engine::new(cfg);
            for i in 0..3 {
                e.intern(keys[i]);
            }
            e.rate(1, 1.0, 0);
            e.on_view(1, view.clone(), 0, 0);

            for now in [0u64, 72, 480] {
                let prep = net.prepare(&node, &arena, now, None, Some(0));
                for target in 0..3 {
                    let mine = net.query(&node, &prep, target, &arena, &mut items);
                    let theirs = e.query(target as AccountIdx, now);
                    assert!(
                        (mine - theirs).abs() < 1e-6,
                        "decay_enabled = {decay_enabled}, now = {now}, 目标 {target}: 框架 {mine} vs 引擎 {theirs}"
                    );
                }
            }
        }
    }
}
