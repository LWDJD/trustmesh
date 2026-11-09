//! 端到端实验框架：把 tm_sim 生产的社交图，喂给 tm_view + tm_agg 做信任传播。
//!
//! 核心问题：原始评分里女巫可能是最高分（集群内互评 1.0），
//! 但经过「只从自己评过分的人里传播」之后，诚实观察者眼中的女巫声望还剩多少？

use std::collections::HashSet;

use tm_agg::{aggregate, combine, AggMode};
use tm_sim::{Kind, SimConfig, World};
use tm_view::{CompressedView, ViewParams};

pub const PARAMS_BUCKETS: usize = 2048;
pub const PARAMS_HASHERS: usize = 5;

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

/// 传播参数。
#[derive(Clone, Debug)]
pub struct PropConfig {
    pub beta: f32,
    pub samples: usize,
    pub rounds: usize,
    pub mode: AggMode,
    pub use_noise_floor: bool,
    /// 是否把「对方对这个账户没意见」（查询值为 0）的来源排除出分母
    pub exclude_zero: bool,
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
}

impl Default for PropConfig {
    fn default() -> Self {
        Self { beta: 0.3, samples: 8, rounds: 4, mode: AggMode::Mean, use_noise_floor: true, exclude_zero: false, banlist: false, ban_penalty: 0.5, ban_floor: 0.2, ban_exclude: 0.0, ban_lambda: 0.0, normalize_view: false }
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

/// xorshift64，保证可复现
struct R(u64);
impl R {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// 跑一次完整的「仿真 → 传播」流程。
pub fn run_case(cfg: SimConfig, pc: &PropConfig) -> Outcome {
    run_case_inject(cfg, pc, &[], &[])
}

/// 可注入额外评分与额外账户的版本。
/// inject: (评分者, 被评者, 分数)；extra_accounts 只用于抬大 n。
pub fn run_case_inject(cfg: SimConfig, pc: &PropConfig, inject: &[(usize, usize, f32)], extra_accounts: &[usize]) -> Outcome {
    let mut world = World::new(cfg);
    world.run();
    let mut ratings = world.ratings();
    ratings.extend_from_slice(inject);
    let mut n = ratings.iter().map(|(o, t, _)| (*o).max(*t)).max().unwrap_or(0) + 1;
    for e in extra_accounts { if *e + 1 > n { n = *e + 1; } }

    // 正分清单
    let mut positive: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    let mut raw_sum_h = 0.0f64;
    let mut raw_cnt_h = 0usize;
    let mut raw_sum_o = 0.0f64;
    let mut raw_cnt_o = 0usize;

    let honest: Vec<usize> = world.indices_of(Kind::Honest).into_iter().filter(|i| *i < n).collect();
    let honest_set: HashSet<usize> = honest.iter().copied().collect();
    for (o, t, s) in &ratings {
        positive[*o].push((*t, *s));
    }
    for (_, t, s) in &ratings {
        if honest_set.contains(t) { raw_sum_h += *s as f64; raw_cnt_h += 1; }
        else { raw_sum_o += *s as f64; raw_cnt_o += 1; }
    }
    let mut others: Vec<usize> = ratings.iter().map(|(_, t, _)| *t).filter(|t| !honest_set.contains(t)).collect();
    others.sort_unstable();
    others.dedup();

    // 初始观点 = 自身评分的压缩
    let keys: Vec<[u8; 32]> = (0..n).map(key_of).collect();
    let mk = |list: &[(usize, f32)]| -> CompressedView {
        let total: f32 = list.iter().map(|(_, s)| *s).sum();
        let pairs: Vec<([u8; 32], f32)> = list.iter().map(|(t, s)| {
            let v = if pc.normalize_view && total > 0.0 { *s / total } else { *s };
            (keys[*t], v)
        }).collect();
        CompressedView::from_pairs(view_params(), &pairs)
    };
    let mut views: Vec<CompressedView> = positive.iter().map(|l| mk(l)).collect();

    let mut rng = R(0x1234_5678_9ABC_DEF1);
    let mut rep: Vec<Vec<f32>> = vec![vec![0.0; n]; n];
    // mul[i][s]：i 对 s 的评分折扣，banlist 审查会调低它
    let mut mul: Vec<Vec<f32>> = vec![vec![1.0f32; n]; n];

    for _round in 0..pc.rounds {
        let mut next_rep = vec![vec![0.0f32; n]; n];
        let mut next_views: Vec<CompressedView> = Vec::with_capacity(n);

        for i in 0..n {
            // 均匀抽样 k 个（不放回）
            let pool = &positive[i];
            let mut idxs: Vec<usize> = (0..pool.len()).collect();
            let k = pc.samples.min(idxs.len());
            let mut picked: Vec<(usize, f32)> = Vec::with_capacity(k);
            for j in 0..k {
                let r = j + rng.below(idxs.len() - j);
                idxs.swap(j, r);
                picked.push(pool[idxs[j]]);
            }

            let mut row = vec![0.0f32; n];
            for u in 0..n {
                let personal = pool.iter().find(|(t, _)| *t == u).map(|(_, s)| *s).unwrap_or(0.0);
                let mut items: Vec<(f32, f32)> = Vec::with_capacity(picked.len());
                for (s, w) in &picked {
                    let mut v = views[*s].query(&keys[u]).clamp(0.0, 1.0);
                    if pc.use_noise_floor && v < 1e-6 { v = 0.0; }
                    if pc.exclude_zero && v == 0.0 { continue; }
                    items.push((*w * mul[i][*s], v));
                }
                if pc.ban_exclude > 0.0 && !others.is_empty() {
                    let mut filtered: Vec<(f32, f32)> = Vec::with_capacity(items.len());
                    for (( w, v), (s, _)) in items.iter().zip(picked.iter()) {
                        let mut hit = 0usize;
                        for &b in &others {
                            if views[*s].query(&keys[b]) > 0.05 { hit += 1; }
                        }
                        let cov = hit as f32 / others.len() as f32;
                        if cov <= pc.ban_exclude { filtered.push((*w, *v)); }
                    }
                    let group = aggregate(&filtered, pc.mode.clone());
                    row[u] = combine(pc.beta, personal, group);
                    continue;
                }
                let group = aggregate(&items, pc.mode.clone());
                row[u] = combine(pc.beta, personal, group);
            }
            let total: f32 = row.iter().sum();
            let pairs: Vec<([u8; 32], f32)> = (0..n).filter(|u| row[*u] > 0.0).map(|u| {
                let v = if pc.normalize_view && total > 0.0 { row[u] / total } else { row[u] };
                (keys[u], v)
            }).collect();
            next_views.push(CompressedView::from_pairs(view_params(), &pairs));
            next_rep[i] = row;
        }
        views = next_views;
        rep = next_rep;

        // banlist 审查（指数降权）：对每个来源，看它给攻击者的平均分，
        // 倍数 = exp(−λ × 平均分)。给得越高，降得越狠；给 0 的不受影响。
        if pc.ban_lambda > 0.0 && !others.is_empty() {
            let mut peer_mul = vec![1.0f32; n];
            for s in 0..n {
                if positive[s].is_empty() { peer_mul[s] = 0.0; continue; }
                let mut acc = 0.0f32;
                let mut cnt = 0usize;
                for &b in &others {
                    let v = views[s].query(&keys[b]).clamp(0.0, 1.0);
                    if v > 0.05 { acc += v; cnt += 1; }
                }
                // 没表过态 → 不受罚；表过态则按它给的平均分降权
                let mean = if cnt > 0 { acc / cnt as f32 } else { 0.0 };
                peer_mul[s] = (-pc.ban_lambda * mean).exp();
            }
            for i in 0..n {
                for (s, _) in positive[i].iter() {
                    mul[i][*s] = peer_mul[*s];
                }
            }
        }

        // 旧的覆盖率式软降权（保留以便对照）
        if pc.banlist && !others.is_empty() && pc.ban_lambda == 0.0 {
            for i in 0..n {
                for (s, _) in positive[i].iter() {
                    let mut hit = 0usize;
                    for &b in &others {
                        if views[*s].query(&keys[b]) > 0.05 { hit += 1; }
                    }
                    let cov = hit as f32 / others.len() as f32;
                    let target = (1.0 - pc.ban_penalty * cov).max(pc.ban_floor);
                    if target < mul[i][*s] { mul[i][*s] = target; }
                }
            }
        }
    }

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
            sum += o.rep[ob][t] as f64;
            cnt += 1;
        }
    }
    if cnt == 0 { 0.0 } else { (sum / cnt as f64) as f32 }
}

/// 观察者胜率：同一观察者眼中，诚实均值 > 攻击者均值的比例。
pub fn win_rate(o: &Outcome) -> (f32, usize) {
    let mut win = 0usize;
    let mut tot = 0usize;
    for &ob in &o.honest {
        let h: Vec<f32> = o.honest.iter().map(|&t| o.rep[ob][t]).collect();
        let a: Vec<f32> = o.others.iter().map(|&t| o.rep[ob][t]).collect();
        if h.is_empty() || a.is_empty() { continue; }
        let hm = h.iter().sum::<f32>() / h.len() as f32;
        let am = a.iter().sum::<f32>() / a.len() as f32;
        tot += 1;
        if hm > am { win += 1; }
    }
    if tot == 0 { (0.0, 0) } else { (win as f32 / tot as f32, tot) }
}
