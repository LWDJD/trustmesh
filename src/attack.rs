//! 攻击模拟：女巫攻击与信任潜伏攻击。
//!
//! **已废弃**：被 `fullsim` 的场景配置取代。
//! 本模块只服务于 `attack_sim`，属于历史实验。
//!
//! 两种攻击的结构不同：
//!
//! - 女巫攻击：注入一批新节点，它们互相连接形成密集子图，再少量渗透到诚实图。
//!   特征是「内部密度高、对外割边少」。
//!
//! - 信任潜伏：攻击者原本是图中的正常节点，长期表现正常，积累信任后突然改变行为。
//!   特征是「历史清白，突变难以从单次快照识别」。
//!
//! 本模块只测量「效果」，不测量「检测延迟」（当前模型是静态的）。

use crate::graph::Graph;
use crate::local_push::{solve_local, LocalPushParams};
use crate::rng::Rng;

pub struct SybilResult {
    /// 攻击前，诚实观察者眼中目标的分数
    pub target_before: f64,
    /// 攻击后，诚实观察者眼中目标的分数
    pub target_after: f64,
    /// 诚实观察者眼中，女巫节点的平均分数
    pub sybil_mean: f64,
    /// 诚实观察者眼中，诚实节点的平均分数
    pub honest_mean: f64,
    /// 目标分数的相对提升
    pub lift: f64,
    /// 受影响的诚实观察者比例（分数变化超过 1%）
    pub affected_ratio: f64,
}

/// 女巫攻击。
///
/// 参数：
/// - `honest_n`：诚实节点数
/// - `sybil_n`：女巫节点数
/// - `infiltration`：每个女巫渗透到诚实图的边数
/// - `target`：被抬高的目标节点（诚实节点之一）
pub fn sybil_attack(
    honest_n: usize,
    sybil_n: usize,
    infiltration: usize,
    compromised_ratio: f64,
    target: usize,
    beta: f64,
    seed: u64,
) -> SybilResult {
    let params = LocalPushParams {
        beta,
        eps: 1e-4,
        max_iter: 400_000,
    };

    // ---- 基线图：只有诚实节点 ----
    let mut rng = Rng::new(seed);
    let mut base = crate::sim::gen_ba(honest_n, 3, &mut rng);
    crate::sim::random_ratings(&mut base, 8, &mut rng);

    let baseline: Vec<f64> = (0..honest_n)
        .map(|s| solve_local(&base, s, &params)[target])
        .collect();
    let target_before = mean(&baseline);

    // ---- 攻击图：诚实节点 + 女巫 ----
    let total = honest_n + sybil_n;
    let mut g = Graph::new(total);

    // 复制诚实图
    for u in 0..honest_n {
        for &(v, w) in base.neighbors(u) {
            g.add_edge(u, v, w);
        }
        for t in 0..honest_n {
            g.set_rating(u, t, base.rating(u, t));
        }
    }

    // 女巫内部：完全图，高权重
    for i in 0..sybil_n {
        for j in 0..sybil_n {
            if i != j {
                g.add_edge(honest_n + i, honest_n + j, 10.0);
            }
        }
    }

    // 女巫渗透：连到随机诚实节点（女巫关注别人）
    let mut rng = Rng::new(seed ^ 0xABCD);
    for i in 0..sybil_n {
        for _ in 0..infiltration {
            let v = rng.next_usize(honest_n);
            g.add_edge(honest_n + i, v, 1.0);
        }
    }

    // 关键：一部分诚实节点「被骗关注」女巫（诚实 -> 女巫）
    // 只有这些人会受女巫评分影响，这是以自己为中心的模型的特性
    for v in 0..honest_n {
        if rng.chance(compromised_ratio) {
            let s = honest_n + rng.next_usize(sybil_n);
            g.add_edge(v, s, 1.0);
        }
    }

    g.finalize();

    // 女巫对所有诚实目标打满分，尤其对 target
    for i in 0..sybil_n {
        for t in 0..honest_n {
            g.set_rating(honest_n + i, t, 1.0);
        }
    }
    // 女巫之间互相打满分
    for i in 0..sybil_n {
        for j in 0..sybil_n {
            g.set_rating(honest_n + i, honest_n + j, 1.0);
        }
    }

    // ---- 攻击后，诚实观察者眼中的分数 ----
    let mut after = Vec::with_capacity(honest_n);
    let mut sybil_scores = Vec::new();
    let mut honest_scores = Vec::new();
    let mut affected = 0usize;

    for s in 0..honest_n {
        let row = solve_local(&g, s, &params);
        after.push(row[target]);
        if (row[target] - baseline[s]).abs() > 0.01 {
            affected += 1;
        }
        for i in 0..sybil_n {
            sybil_scores.push(row[honest_n + i]);
        }
        for t in 0..honest_n {
            if t != target {
                honest_scores.push(row[t]);
            }
        }
    }

    let target_after = mean(&after);
    let lift = if target_before > 1e-9 {
        (target_after - target_before) / target_before
    } else {
        0.0
    };

    SybilResult {
        target_before,
        target_after,
        sybil_mean: mean(&sybil_scores),
        honest_mean: mean(&honest_scores),
        lift,
        affected_ratio: affected as f64 / honest_n as f64,
    }
}

pub struct LatentResult {
    /// 潜伏者诚实时的目标分数
    pub target_honest: f64,
    /// 潜伏者突变后的目标分数
    pub target_attack: f64,
    /// 潜伏者自己的分数（在诚实观察者眼中）
    pub agent_score: f64,
    /// 目标分数的相对提升
    pub lift: f64,
    /// 需要多少潜伏者才能把目标抬高 10%
    pub agents_for_10pct: Option<usize>,
}

/// 信任潜伏攻击。
///
/// 选 `agent_n` 个已有节点充当潜伏者，它们本来在图中正常存在（有正常的边和评分），
/// 只是评分突然从 `honest` 变成 `attack`。
///
/// 这模拟的是「长期表现正常，突然作恶」。静态模型测不出检测延迟，只能测收益。
pub fn latent_attack(
    n: usize,
    agent_n: usize,
    target: usize,
    _honest_rating: f64,
    attack_rating: f64,
    beta: f64,
    seed: u64,
) -> LatentResult {
    let params = LocalPushParams {
        beta,
        eps: 1e-4,
        max_iter: 400_000,
    };

    let mut rng = Rng::new(seed);
    let mut g = crate::sim::gen_ba(n, 3, &mut rng);
    crate::sim::random_ratings(&mut g, 8, &mut rng);

    // 选择潜伏者：编号靠后的一批节点（模拟「老用户」）
    let agents: Vec<usize> = (n - agent_n..n).collect();

    // 记录潜伏者原本对 target 的评分
    let originals: Vec<f64> = agents.iter().map(|&a| g.rating(a, target)).collect();

    // 诚实阶段
    let base: Vec<f64> = (0..n)
        .filter(|s| !agents.contains(s))
        .map(|s| solve_local(&g, s, &params)[target])
        .collect();
    let target_honest = mean(&base);

    // 突变：所有潜伏者把评分改成 attack_rating
    for &a in &agents {
        g.set_rating(a, target, attack_rating);
    }

    let after: Vec<f64> = (0..n)
        .filter(|s| !agents.contains(s))
        .map(|s| solve_local(&g, s, &params)[target])
        .collect();
    let target_attack = mean(&after);

    let lift = if target_honest > 1e-9 {
        (target_attack - target_honest) / target_honest
    } else {
        0.0
    };

    // 潜伏者在诚实观察者眼中的平均分数
    let mut agent_scores = Vec::new();
    for s in 0..n {
        if agents.contains(&s) {
            continue;
        }
        let row = solve_local(&g, s, &params);
        for &a in &agents {
            agent_scores.push(row[a]);
        }
    }

    // 二分查找改为由调用方扫描，避免递归过深
    let agents_for_10pct = None;

    // 恢复原值，避免调用方误用
    for (idx, &a) in agents.iter().enumerate() {
        g.set_rating(a, target, originals[idx]);
    }

    LatentResult {
        target_honest,
        target_attack,
        agent_score: mean(&agent_scores),
        lift,
        agents_for_10pct,
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}
