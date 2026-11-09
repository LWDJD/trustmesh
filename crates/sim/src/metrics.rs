//! 指标骨架：一次快照包含全部可对比的观测值。
//!
//! 设计原则：
//!
//! 1. **纯函数**：`Snapshot::compute` 只读账户表，不依赖 `World`，单独就能测；
//! 2. **无 NaN**：空世界、无评分、无社团间关系这些退化情形都返回 0，不返回 NaN；
//! 3. **可排序**：所有浮点字段都是有限值，直接可以写进 CSV 做参数扫描。
//!
//! 指标分四组：
//!
//! - **规模**：账户数、存活、在线、休眠、离开、关注边、评分边；
//! - **分类**：每类账户的数量、质量均值、收到/给出评分的数量与均分、声誉代理；
//! - **社团**：内/间关注边占比、内/间评分均分、圈子化（内均分 ÷ 间均分）；
//! - **分布与攻防**：评分分布（均值/标准差/分位数）、攻击者优势、观察者胜率、
//!   质量-评分相关、影响力基尼系数。

use std::fmt::Write as _;

use crate::account::{Account, Kind, CLASS_COUNT};

/// 各分类在快照里的固定顺序。下标与 `Kind::class_index` 一致。
/// 「收益方优势」所需的最小样本数：太少的话一条评分就能编出很大的比率。
pub const MIN_TARGET_SAMPLES: usize = 5;

pub const CLASS_LABELS: [&str; CLASS_COUNT] = [
    "honest",
    "sybil",
    "sleeper",
    "infiltrator",
    "endorser",
    "spammer",
    "promoted",
];

/// 评分分布。
///
/// 统计对象是**全世界所有已发出的评分**（每条评分算一个样本）。
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Distribution {
    /// 样本数
    pub count: usize,
    /// 均值
    pub mean: f32,
    /// 总体标准差（除以 n）
    pub std: f32,
    /// 最小值
    pub min: f32,
    /// 10 分位
    pub p10: f32,
    /// 25 分位
    pub p25: f32,
    /// 50 分位（中位数）
    pub p50: f32,
    /// 75 分位
    pub p75: f32,
    /// 90 分位
    pub p90: f32,
    /// 99 分位
    pub p99: f32,
    /// 最大值
    pub max: f32,
}

impl Distribution {
    /// 从一批评分算分布。会先排序，再取分位数。
    pub fn from_unsorted(scores: &[f32]) -> Self {
        if scores.is_empty() {
            return Self::default();
        }
        let mut v: Vec<f32> = scores
            .iter()
            .map(|x| if x.is_finite() { *x } else { 0.0 })
            .collect();
        v.sort_unstable_by(f32::total_cmp);
        let n = v.len() as f64;
        let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n;
        let var = v
            .iter()
            .map(|&x| {
                let d = x as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n;
        let q = |p: f32| Self::quantile(&v, p);
        Self {
            count: v.len(),
            mean: mean as f32,
            std: var.sqrt() as f32,
            min: v[0],
            p10: q(0.10),
            p25: q(0.25),
            p50: q(0.50),
            p75: q(0.75),
            p90: q(0.90),
            p99: q(0.99),
            max: v[v.len() - 1],
        }
    }

    /// 分位数（线性插值）。`sorted` 必须已升序，`q` 会被夹到 `[0,1]`。
    pub fn quantile(sorted: &[f32], q: f32) -> f32 {
        if sorted.is_empty() {
            return 0.0;
        }
        if sorted.len() == 1 {
            return sorted[0];
        }
        let q = if q.is_finite() { q.clamp(0.0, 1.0) } else { 0.0 };
        let pos = q * (sorted.len() - 1) as f32;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        if lo == hi {
            sorted[lo.min(sorted.len() - 1)]
        } else {
            let t = pos - lo as f32;
            sorted[lo] * (1.0 - t) + sorted[hi] * t
        }
    }
}

/// 某一类账户的统计。
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClassStat {
    /// 类别标签
    pub label: &'static str,
    /// 账户数（含离开）
    pub accounts: usize,
    /// 存活数
    pub alive: usize,
    /// 在线数
    pub online: usize,
    /// 内容质量均值
    pub quality_mean: f32,
    /// 收到的评分条数
    pub ratings_in: usize,
    /// 给出的评分条数
    pub ratings_out: usize,
    /// 收到的评分均分（按条数加权）
    pub mean_received: f32,
    /// 给出的评分均分（按条数加权）
    pub mean_given: f32,
    /// 声誉代理均值
    pub reputation_mean: f32,
}

/// 社团结构统计。
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CommunityStat {
    /// 社团数量
    pub count: usize,
    /// 关注边总数
    pub follow_edges: usize,
    /// 社团内关注边
    pub follow_intra: usize,
    /// 社团间关注边
    pub follow_inter: usize,
    /// 社团内关注边占比（衡量圈子化的结构面）
    pub follow_intra_share: f32,
    /// 社团内评分条数
    pub rating_intra: usize,
    /// 社团间评分条数
    pub rating_inter: usize,
    /// 社团内平均评分
    pub intra_mean: f32,
    /// 社团间平均评分
    pub inter_mean: f32,
    /// 圈子化：社团内均分 ÷ 社团间均分；没有社团间评分时为 0
    pub cliquishness: f32,
}

/// 世界在某一时刻的完整快照。
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// 步数（小时数）
    pub step: usize,
    /// 逻辑时间（小时）
    pub time: u64,
    /// 账户总数（含离开）
    pub accounts: usize,
    /// 存活账户数
    pub alive: usize,
    /// 在线账户数
    pub online: usize,
    /// 休眠账户数
    pub dormant: usize,
    /// 已离开账户数
    pub left: usize,
    /// 桥接节点数
    pub bridges: usize,
    /// 关注边总数
    pub follow_edges: usize,
    /// 评分边总数（每条「我对某人的评分」算一条）
    pub rating_edges: usize,
    /// 分类统计，顺序见 `CLASS_LABELS`
    pub classes: Vec<ClassStat>,
    /// 社团统计
    pub community: CommunityStat,
    /// 评分分布
    pub distribution: Distribution,
    /// 攻击者收到的均分（按条数加权，不含 promoted）
    pub attacker_mean: f32,
    /// 攻击者均分 ÷ 诚实均分；诚实均分为 0 时为 0
    pub attacker_advantage: f32,
    /// 攻击收益方（被推广账户）收到的均分
    pub target_mean: f32,
    /// 收益方收到的评分条数（样本太少时优势不可信，见 `target_advantage`）
    pub target_ratings: usize,
    /// 收益方均分 ÷ 诚实均分。
    ///
    /// 只在「收益方至少收到 `MIN_TARGET_SAMPLES` 条评分」且诚实均分 > 0 时计算，
    /// 样本太少（比如垃圾账户只被别人打了一次分）时置 0，避免一条评分就得出「优势 1.6」。
    pub target_advantage: f32,
    /// 参与「观察者胜率」比较的观察者数量（0 表示没有观察者同时评过两类）
    pub observers_compared: usize,
    /// 观察者胜率：在同时评过诚实账户和攻击者的观察者里，
    /// 「给诚实账户的均分 > 给攻击者的均分」的比例
    pub observer_win_rate: f32,
    /// 诚实账户的「质量 ↔ 收到均分」皮尔逊相关（至少 3 条评分才计入）
    pub honest_quality_corr: f32,
    /// 影响力基尼系数（影响力 = 存活账户收到的评分之和）
    pub gini_received: f32,
}

/// 累加器。
#[derive(Clone, Copy, Default)]
struct Acc {
    accounts: usize,
    alive: usize,
    online: usize,
    quality_sum: f64,
    reputation_sum: f64,
    in_sum: f64,
    in_n: usize,
    out_sum: f64,
    out_n: usize,
}

impl Acc {
    fn into_stat(self, label: &'static str) -> ClassStat {
        ClassStat {
            label,
            accounts: self.accounts,
            alive: self.alive,
            online: self.online,
            quality_mean: mean_of(self.quality_sum, self.accounts),
            ratings_in: self.in_n,
            ratings_out: self.out_n,
            mean_received: mean_of(self.in_sum, self.in_n),
            mean_given: mean_of(self.out_sum, self.out_n),
            reputation_mean: mean_of(self.reputation_sum, self.accounts),
        }
    }
}

fn mean_of(sum: f64, n: usize) -> f32 {
    if n == 0 {
        0.0
    } else {
        (sum / n as f64) as f32
    }
}

impl Snapshot {
    /// 从账户表算一份快照。
    ///
    /// `rhythm` 决定「在线」是否受昼夜节律影响（与 `SimConfig::enable_rhythm` 一致）；
    /// `community_count` 是社团总数，由 `World` 传入（允许存在暂时还没有成员的社团）。
    ///
    /// `target` 是本次攻击的收益方（被推广账户），用于算「目标优势」；
    /// 没有攻击时传 `None`。
    pub fn compute(
        accounts: &[Account],
        step: usize,
        time: u64,
        rhythm: bool,
        community_count: usize,
        target: Option<usize>,
    ) -> Self {
        let mut accs = [Acc::default(); CLASS_COUNT];
        let mut alive = 0usize;
        let mut online = 0usize;
        let mut dormant = 0usize;
        let mut left = 0usize;
        let mut bridges = 0usize;

        for a in accounts {
            let ci = a.kind.class_index().min(CLASS_COUNT - 1);
            let acc = &mut accs[ci];
            acc.accounts += 1;
            acc.quality_sum += a.quality as f64;
            acc.reputation_sum += a.reputation as f64;
            acc.in_sum += a.received_sum;
            acc.in_n += a.received_count;
            acc.out_n += a.ratings.len();
            for &(_, s) in &a.ratings {
                acc.out_sum += s as f64;
            }
            if a.is_alive() {
                alive += 1;
                acc.alive += 1;
            }
            if a.is_online(time, rhythm) {
                online += 1;
                acc.online += 1;
            }
            if a.is_dormant() {
                dormant += 1;
            }
            if a.is_left() {
                left += 1;
            }
            if a.is_bridge {
                bridges += 1;
            }
        }

        let classes: Vec<ClassStat> = accs
            .iter()
            .enumerate()
            .map(|(i, a)| a.into_stat(CLASS_LABELS[i]))
            .collect();

        // —— 关系与社团 ——
        let mut follow_edges = 0usize;
        let mut follow_intra = 0usize;
        let mut follow_inter = 0usize;
        let mut rating_intra = 0usize;
        let mut rating_inter = 0usize;
        let mut intra_sum = 0.0f64;
        let mut inter_sum = 0.0f64;
        let mut scores: Vec<f32> = Vec::new();

        for a in accounts {
            for &u in &a.follows {
                follow_edges += 1;
                if accounts.get(u).map(|t| t.community) == Some(a.community) {
                    follow_intra += 1;
                } else {
                    follow_inter += 1;
                }
            }
            for &(u, s) in &a.ratings {
                scores.push(s);
                if accounts.get(u).map(|t| t.community) == Some(a.community) {
                    rating_intra += 1;
                    intra_sum += s as f64;
                } else {
                    rating_inter += 1;
                    inter_sum += s as f64;
                }
            }
        }

        let intra_mean = mean_of(intra_sum, rating_intra);
        let inter_mean = mean_of(inter_sum, rating_inter);
        let cliquishness = if inter_mean > 1e-9 {
            intra_mean / inter_mean
        } else {
            0.0
        };
        let follow_intra_share = if follow_edges == 0 {
            0.0
        } else {
            follow_intra as f32 / follow_edges as f32
        };

        // —— 攻防 ——
        let honest_mean = classes[0].mean_received;
        let mut attacker_sum = 0.0f64;
        let mut attacker_n = 0usize;
        // 除 honest(0) 和 promoted(6) 之外的类都算攻击方
        for acc in accs.iter().take(CLASS_COUNT - 1).skip(1) {
            attacker_sum += acc.in_sum;
            attacker_n += acc.in_n;
        }
        let attacker_mean = mean_of(attacker_sum, attacker_n);
        let attacker_advantage = if honest_mean > 1e-9 {
            attacker_mean / honest_mean
        } else {
            0.0
        };
        let (target_mean, target_ratings, target_advantage) = match target {
            Some(t) if t < accounts.len() => {
                let a = &accounts[t];
                let m = a.mean_received();
                let adv = if honest_mean > 1e-9 && a.received_count >= MIN_TARGET_SAMPLES {
                    m / honest_mean
                } else {
                    0.0
                };
                (m, a.received_count, adv)
            }
            _ => (0.0, 0, 0.0),
        };

        // 观察者胜率：同一个观察者眼里，诚实均分是否高于攻击者均分
        let mut wins = 0usize;
        let mut observers = 0usize;
        for a in accounts {
            let mut h_sum = 0.0f64;
            let mut h_n = 0usize;
            let mut at_sum = 0.0f64;
            let mut at_n = 0usize;
            for &(u, s) in &a.ratings {
                match accounts.get(u).map(|t| t.kind) {
                    Some(Kind::Honest) => {
                        h_sum += s as f64;
                        h_n += 1;
                    }
                    Some(k) if k.is_attacker() => {
                        at_sum += s as f64;
                        at_n += 1;
                    }
                    _ => {}
                }
            }
            if h_n > 0 && at_n > 0 {
                observers += 1;
                if h_sum / h_n as f64 > at_sum / at_n as f64 {
                    wins += 1;
                }
            }
        }
        let observer_win_rate = if observers == 0 {
            0.0
        } else {
            wins as f32 / observers as f32
        };

        // 诚实账户：质量 ↔ 收到均分
        let mut qs: Vec<f64> = Vec::new();
        let mut rs: Vec<f64> = Vec::new();
        for a in accounts {
            if a.kind == Kind::Honest && a.received_count >= 3 {
                qs.push(a.quality as f64);
                rs.push(a.mean_received() as f64);
            }
        }
        let honest_quality_corr = pearson(&qs, &rs);

        // 影响力基尼：存活账户收到的评分之和
        let influence: Vec<f64> = accounts
            .iter()
            .filter(|a| a.is_alive())
            .map(|a| a.received_sum)
            .collect();
        let gini_received = gini(&influence);

        Self {
            step,
            time,
            accounts: accounts.len(),
            alive,
            online,
            dormant,
            left,
            bridges,
            follow_edges,
            rating_edges: scores.len(),
            classes,
            community: CommunityStat {
                count: community_count,
                follow_edges,
                follow_intra,
                follow_inter,
                follow_intra_share,
                rating_intra,
                rating_inter,
                intra_mean,
                inter_mean,
                cliquishness,
            },
            distribution: Distribution::from_unsorted(&scores),
            attacker_mean,
            attacker_advantage,
            target_mean,
            target_ratings,
            target_advantage,
            observers_compared: observers,
            observer_win_rate,
            honest_quality_corr,
            gini_received,
        }
    }

    /// 按标签取某一类的统计。
    pub fn class(&self, label: &str) -> Option<&ClassStat> {
        self.classes.iter().find(|c| c.label == label)
    }

    /// 诚实类统计（数组固定存在，不会失败）。
    pub fn honest(&self) -> &ClassStat {
        &self.classes[0]
    }

    /// 被推广账户统计。
    pub fn promoted(&self) -> &ClassStat {
        &self.classes[6]
    }

    /// 有没有产生过任何评分。
    pub fn has_ratings(&self) -> bool {
        self.rating_edges > 0
    }

    /// 所有浮点指标都是有限的（写 CSV 前自检用）。
    pub fn all_finite(&self) -> bool {
        let mut ok = self.distribution.mean.is_finite()
            && self.distribution.std.is_finite()
            && self.attacker_mean.is_finite()
            && self.attacker_advantage.is_finite()
            && self.target_mean.is_finite()
            && self.target_advantage.is_finite()
            && self.observer_win_rate.is_finite()
            && self.honest_quality_corr.is_finite()
            && self.gini_received.is_finite()
            && self.community.intra_mean.is_finite()
            && self.community.inter_mean.is_finite()
            && self.community.cliquishness.is_finite()
            && self.community.follow_intra_share.is_finite();
        for c in &self.classes {
            ok = ok
                && c.mean_received.is_finite()
                && c.mean_given.is_finite()
                && c.quality_mean.is_finite()
                && c.reputation_mean.is_finite();
        }
        ok
    }

    /// 渲染成人类可读的多行文本。二进制程序与测试共用。
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "=== 快照 · 第 {} 步 / t = {}h ===",
            self.step, self.time
        );
        let _ = writeln!(
            s,
            "账户 {} ｜ 存活 {} ｜ 在线 {} ｜ 休眠 {} ｜ 离开 {} ｜ 桥接 {}",
            self.accounts, self.alive, self.online, self.dormant, self.left, self.bridges
        );
        let _ = writeln!(
            s,
            "关注边 {} ｜ 评分边 {} ｜ 人均关注 {:.2}",
            self.follow_edges,
            self.rating_edges,
            if self.alive == 0 {
                0.0
            } else {
                self.follow_edges as f32 / self.alive as f32
            }
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "[分类账户]");
        for c in &self.classes {
            let _ = writeln!(
                s,
                "{:<11} 账户 {:>4} ｜ 存活 {:>4} ｜ 在线 {:>4} ｜ 质量 {:.3} ｜ 收评分 {:>5}（均分 {:.3}）｜ 给评分 {:>5}（均分 {:.3}）｜ 声誉 {:.3}",
                c.label,
                c.accounts,
                c.alive,
                c.online,
                c.quality_mean,
                c.ratings_in,
                c.mean_received,
                c.ratings_out,
                c.mean_given,
                c.reputation_mean
            );
        }
        let _ = writeln!(s);
        let _ = writeln!(s, "[社团结构]");
        let _ = writeln!(
            s,
            "社团 {} ｜ 关注边：内 {} / 间 {}（内占比 {:.3}）",
            self.community.count,
            self.community.follow_intra,
            self.community.follow_inter,
            self.community.follow_intra_share
        );
        let _ = writeln!(
            s,
            "评分：内 {} / 间 {} ｜ 社内均分 {:.3} ｜ 社间均分 {:.3} ｜ 圈子化 {:.3}",
            self.community.rating_intra,
            self.community.rating_inter,
            self.community.intra_mean,
            self.community.inter_mean,
            self.community.cliquishness
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "[评分分布]");
        let d = &self.distribution;
        let _ = writeln!(
            s,
            "样本 {} ｜ 均值 {:.3} ｜ 标准差 {:.3}",
            d.count, d.mean, d.std
        );
        let _ = writeln!(
            s,
            "最小 {:.3} ｜ p10 {:.3} ｜ p25 {:.3} ｜ 中位 {:.3} ｜ p75 {:.3} ｜ p90 {:.3} ｜ p99 {:.3} ｜ 最大 {:.3}",
            d.min, d.p10, d.p25, d.p50, d.p75, d.p90, d.p99, d.max
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "[攻防指标]");
        let _ = writeln!(
            s,
            "攻击者均分 {:.3} ｜ 攻击者/诚实 {:.3} ｜ 收益方均分 {:.3}（{} 条）｜ 收益方优势 {:.3}",
            self.attacker_mean,
            self.attacker_advantage,
            self.target_mean,
            self.target_ratings,
            self.target_advantage
        );
        let _ = writeln!(
            s,
            "观察者胜率 {:.3}（{} 位观察者）｜ 质量-评分相关 {:.3} ｜ 影响力基尼 {:.3}",
            self.observer_win_rate, self.observers_compared, self.honest_quality_corr, self.gini_received
        );
        s
    }
}

/// 基尼系数。全 0 或样本不足 2 个时返回 0。
pub fn gini(values: &[f64]) -> f32 {
    let n = values.len();
    if n < 2 {
        return 0.0;
    }
    let mut v: Vec<f64> = values
        .iter()
        .map(|&x| if x.is_finite() && x > 0.0 { x } else { 0.0 })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let sum: f64 = v.iter().sum();
    if !(sum > 0.0) {
        return 0.0;
    }
    let mut weighted = 0.0f64;
    for (i, &x) in v.iter().enumerate() {
        weighted += (i as f64 + 1.0) * x;
    }
    let g = 2.0 * weighted / (n as f64 * sum) - (n as f64 + 1.0) / n as f64;
    g.clamp(0.0, 1.0) as f32
}

/// 皮尔逊相关系数。样本不足 2 个或方差为 0 时返回 0。
pub fn pearson(xs: &[f64], ys: &[f64]) -> f32 {
    let n = xs.len().min(ys.len());
    if n < 2 {
        return 0.0;
    }
    let mx = xs[..n].iter().sum::<f64>() / n as f64;
    let my = ys[..n].iter().sum::<f64>() / n as f64;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for i in 0..n {
        let dx = xs[i] - mx;
        let dy = ys[i] - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 1e-18 || syy <= 1e-18 {
        return 0.0;
    }
    (sxy / (sxx.sqrt() * syy.sqrt())).clamp(-1.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Life;
    use crate::interest::Interest;
    use crate::rng::Rng;

    fn 账户(idx: usize, kind: Kind, community: usize, quality: f32) -> Account {
        let mut rng = Rng::new(idx as u64 + 1);
        Account {
            idx,
            kind,
            joined_at: 0,
            community,
            is_bridge: false,
            interest: Interest::random(8, &mut rng),
            quality,
            activity: 1.0,
            active_start: 0,
            active_span: 24,
            life: Life::Active,
            attack_active: false,
            reputation: 0.0,
            received_sum: 0.0,
            received_count: 0,
            follows: Vec::new(),
            ratings: Vec::new(),
        }
    }

    #[test]
    fn 分布计算() {
        let d = Distribution::from_unsorted(&[0.1, 0.2, 0.3, 0.4, 0.5]);
        assert_eq!(d.count, 5);
        assert!((d.mean - 0.3).abs() < 1e-6);
        assert!((d.min - 0.1).abs() < 1e-6);
        assert!((d.max - 0.5).abs() < 1e-6);
        assert!((d.p50 - 0.3).abs() < 1e-6);
        assert!((d.p25 - 0.2).abs() < 1e-6);
        // 总体标准差：sqrt(0.02) ≈ 0.1414
        assert!((d.std - 0.141_421).abs() < 1e-4, "std = {}", d.std);
        // 乱序输入也要给出同样的结果
        let e = Distribution::from_unsorted(&[0.5, 0.1, 0.3, 0.2, 0.4]);
        assert_eq!(d, e);
    }

    #[test]
    fn 空分布全零() {
        let d = Distribution::from_unsorted(&[]);
        assert_eq!(d, Distribution::default());
        assert_eq!(d.mean, 0.0);
        assert_eq!(d.p99, 0.0);
        assert_eq!(Distribution::quantile(&[], 0.5), 0.0);
    }

    #[test]
    fn 分位数插值() {
        let v = [0.0, 1.0];
        assert!((Distribution::quantile(&v, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(Distribution::quantile(&v, 0.0), 0.0);
        assert_eq!(Distribution::quantile(&v, 1.0), 1.0);
        // 越界分位被夹紧
        assert_eq!(Distribution::quantile(&v, -1.0), 0.0);
        assert_eq!(Distribution::quantile(&v, 9.0), 1.0);
        // 单元素
        assert_eq!(Distribution::quantile(&[0.7], 0.3), 0.7);
    }

    #[test]
    fn 基尼系数() {
        assert_eq!(gini(&[]), 0.0);
        assert_eq!(gini(&[1.0]), 0.0);
        // 完全均等
        assert!(gini(&[1.0; 20]).abs() < 1e-6);
        // 全 0
        assert_eq!(gini(&[0.0, 0.0, 0.0]), 0.0);
        // 极端不均
        let mut v = vec![0.0; 99];
        v.push(1.0);
        let g = gini(&v);
        assert!((0.9..=1.0).contains(&g), "极不均的基尼 {g}");
        // 非法值按 0
        assert!(gini(&[f64::NAN, 1.0]).is_finite());
    }

    #[test]
    fn 皮尔逊相关() {
        let x = [1.0, 2.0, 3.0, 4.0];
        let y = [2.0, 4.0, 6.0, 8.0];
        assert!((pearson(&x, &y) - 1.0).abs() < 1e-6);
        let z = [8.0, 6.0, 4.0, 2.0];
        assert!((pearson(&x, &z) + 1.0).abs() < 1e-6);
        // 常数序列没有相关系数
        assert_eq!(pearson(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(pearson(&[1.0], &[1.0]), 0.0);
        assert_eq!(pearson(&[], &[]), 0.0);
    }

    #[test]
    fn 空世界快照安全() {
        let s = Snapshot::compute(&[], 0, 0, true, 0, None);
        assert_eq!(s.accounts, 0);
        assert_eq!(s.alive, 0);
        assert_eq!(s.online, 0);
        assert_eq!(s.rating_edges, 0);
        assert_eq!(s.follow_edges, 0);
        assert_eq!(s.classes.len(), CLASS_COUNT);
        assert_eq!(s.attacker_advantage, 0.0);
        assert_eq!(s.observer_win_rate, 0.0);
        assert_eq!(s.honest_quality_corr, 0.0);
        assert_eq!(s.gini_received, 0.0);
        assert!(s.all_finite());
        // 渲染不能 panic，且包含关键字段
        let text = s.render();
        assert!(text.contains("快照"));
        assert!(text.contains("honest"));
    }

    #[test]
    fn 分类标签顺序固定() {
        for (i, label) in CLASS_LABELS.iter().enumerate() {
            let kinds = [
                Kind::Honest,
                Kind::Sybil { cluster: 0 },
                Kind::Sleeper { normal_until: 0 },
                Kind::Infiltrator { bought_at: 0 },
                Kind::Endorser,
                Kind::Spammer,
                Kind::Promoted,
            ];
            assert_eq!(kinds[i].label(), *label);
            assert_eq!(kinds[i].class_index(), i);
        }
    }

    #[test]
    fn 小世界统计正确() {
        // 4 个账户：0/1 在社团 0，2/3 在社团 1
        let mut acc = vec![
            账户(0, Kind::Honest, 0, 0.9),
            账户(1, Kind::Honest, 0, 0.5),
            账户(2, Kind::Sybil { cluster: 0 }, 1, 1.0),
            账户(3, Kind::Sybil { cluster: 0 }, 1, 1.0),
        ];
        // 关注：0->1（内），0->2（间），2->3（内）
        acc[0].follows = vec![1, 2];
        acc[2].follows = vec![3];
        // 评分：0 给 1 打 0.4，0 给 2 打 0.8，2 给 3 打 1.0
        acc[0].ratings = vec![(1, 0.4), (2, 0.8)];
        acc[2].ratings = vec![(3, 1.0)];
        // 手工补上收到评分的累计值
        acc[1].received_sum = 0.4;
        acc[1].received_count = 1;
        acc[2].received_sum = 0.8;
        acc[2].received_count = 1;
        acc[3].received_sum = 1.0;
        acc[3].received_count = 1;

        let s = Snapshot::compute(&acc, 5, 5, true, 2, Some(3));
        assert_eq!(s.accounts, 4);
        assert_eq!(s.alive, 4);
        assert_eq!(s.online, 4);
        assert_eq!(s.follow_edges, 3);
        assert_eq!(s.rating_edges, 3);
        assert_eq!(s.community.follow_intra, 2);
        assert_eq!(s.community.follow_inter, 1);
        assert!((s.community.follow_intra_share - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(s.community.rating_intra, 2);
        assert_eq!(s.community.rating_inter, 1);
        assert!((s.community.intra_mean - 0.7).abs() < 1e-6);
        assert!((s.community.inter_mean - 0.8).abs() < 1e-6);
        assert!((s.community.cliquishness - 0.875).abs() < 1e-6);

        assert_eq!(s.class("honest").unwrap().accounts, 2);
        assert_eq!(s.class("sybil").unwrap().accounts, 2);
        assert_eq!(s.class("spammer").unwrap().accounts, 0);
        assert_eq!(s.class("promoted").unwrap().accounts, 0);
        assert!((s.class("honest").unwrap().mean_received - 0.4).abs() < 1e-6);
        assert!((s.class("sybil").unwrap().mean_received - 0.9).abs() < 1e-6);

        // 攻击者均分 0.9，诚实均分 0.4 → 优势 2.25
        assert!((s.attacker_mean - 0.9).abs() < 1e-6);
        assert!((s.attacker_advantage - 2.25).abs() < 1e-6);
        // 收益方是账户 3（女巫），只收到 1 条评分 → 样本不足，优势记 0
        assert!((s.target_mean - 1.0).abs() < 1e-6);
        assert_eq!(s.target_ratings, 1);
        assert_eq!(s.target_advantage, 0.0);
        assert_eq!(s.observers_compared, 1);

        // 观察者 0：给诚实 0.4、给攻击者 0.8 → 输；观察者 2：只给攻击者 → 不计入
        assert!((s.observer_win_rate - 0.0).abs() < 1e-6);
        assert!(s.all_finite());

        // 收益方样本补到阈值以上，优势才会被算出来
        let mut acc2 = acc.clone();
        acc2[3].received_sum = 5.0;
        acc2[3].received_count = 5;
        let s2 = Snapshot::compute(&acc2, 5, 5, true, 2, Some(3));
        assert!((s2.target_mean - 1.0).abs() < 1e-6);
        assert!((s2.target_advantage - 2.5).abs() < 1e-6);
    }

    #[test]
    fn 离线与离开被正确计数() {
        let mut acc = vec![
            账户(0, Kind::Honest, 0, 0.5),
            账户(1, Kind::Honest, 0, 0.5),
            账户(2, Kind::Honest, 0, 0.5),
        ];
        acc[0].active_span = 4;
        acc[0].active_start = 0;
        acc[1].life = Life::Dormant { until: 100 };
        acc[2].life = Life::Left { at: 1 };
        acc[2].follows = vec![0];
        let s = Snapshot::compute(&acc, 1, 1, true, 1, None);
        assert_eq!(s.alive, 2);
        assert_eq!(s.left, 1);
        assert_eq!(s.dormant, 1);
        // t=1 时账户 0 在活跃时段内，账户 1 休眠，账户 2 离开
        assert_eq!(s.online, 1);
        // 关闭节律：只有账户 0 在线（另两个不是 Active）
        let s = Snapshot::compute(&acc, 1, 1, false, 1, None);
        assert_eq!(s.online, 1);
    }

    #[test]
    fn 渲染结果可读且无非法数值() {
        let mut acc = vec![账户(0, Kind::Honest, 0, 0.6)];
        acc[0].received_sum = 0.5;
        acc[0].received_count = 1;
        acc[0].reputation = 0.5;
        let s = Snapshot::compute(&acc, 3, 3, true, 1, None);
        let text = s.render();
        assert!(text.contains("[分类账户]"));
        assert!(text.contains("[社团结构]"));
        assert!(text.contains("[评分分布]"));
        assert!(text.contains("[攻防指标]"));
        assert!(!text.contains("NaN"), "报表里出现 NaN:\n{text}");
        // 注意：类别名 "infiltrator" 里也有 "inf"，所以只能检查独立记号
        assert!(!text.contains(" inf"), "报表里出现 inf:\n{text}");
        assert!(s.all_finite(), "报表里出现非有限数值:\n{text}");
    }
}
