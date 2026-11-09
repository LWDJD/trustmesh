//! 仿真配置。
//!
//! 所有参数都可以从外部改，但 `World::new` 会先调 `SimConfig::validated` 做一次夹紧：
//! 非法值（NaN、负数、越界比例）会被替换成默认值或截断到边界，而不是 panic。
//! 这样实验脚本批量扫参数时不会因为一个笔误整轮崩掉。

/// 攻击类型。
///
/// 每个场景只启用一种攻击（便于归因），但都包含「被推广账户」这一收益方：
///
/// - `Sybil`：女巫自己就是收益方，集群内部互评满分；
/// - `Sleeper`：潜伏者自己抱团，突变后互相打满分；
/// - `Infiltrator` / `Endorser`：收益方是一个看起来完全正常的 `Promoted` 账户；
/// - `Spammer`：垃圾账户自己就是收益方（测「垃圾账户能不能靠刷量混到高分」）。
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum AttackKind {
    /// 不发动任何攻击（对照组）
    #[default]
    None,
    /// 女巫集群：`cluster` 个集群，集群内部互评满分
    Sybil { cluster: usize },
    /// 潜伏者：`normal_until` 小时之前行为正常，之后给目标打满分
    Sleeper { normal_until: u64 },
    /// 渗透者：按声誉从高到低收买高分账户（时间点见 `SimConfig::infiltrate_at`）
    Infiltrator,
    /// 背书攻击：`ratio` 比例的高质量账户公开背书被推广账户
    Endorser { ratio: f32 },
    /// 垃圾制造者：内容极差、活跃极高、不看质量乱打分
    Spammer,
}

impl AttackKind {
    /// 报表里用的稳定标签。
    pub fn label(&self) -> &'static str {
        match self {
            AttackKind::None => "none",
            AttackKind::Sybil { .. } => "sybil",
            AttackKind::Sleeper { .. } => "sleeper",
            AttackKind::Infiltrator => "infiltrator",
            AttackKind::Endorser { .. } => "endorser",
            AttackKind::Spammer => "spammer",
        }
    }

    /// 是否没有攻击。
    pub fn is_none(&self) -> bool {
        matches!(self, AttackKind::None)
    }
}

/// 仿真配置。
///
/// 参数分五组：人口、兴趣与结构、关注与活跃、评分行为、攻击。
/// 默认值对应「200 人的小社区跑 200 小时」，这是最常用的对比规模。
#[derive(Clone, Debug)]
pub struct SimConfig {
    // ———————————————————— 人口动态 ————————————————————
    /// 初始账户数
    pub n_initial: usize,
    /// 默认运行步数（`World::run` 用），一步 = 一小时
    pub steps: usize,
    /// 每小时期望加入人数（泊松过程）
    pub join_rate: f64,
    /// 活跃账户每小时的基础离开概率
    pub leave_rate: f64,
    /// 活跃账户每小时进入休眠的概率
    pub dormancy_rate: f64,
    /// 休眠时长区间（小时，闭区间）
    pub sleep_hours: (u64, u64),
    /// 休眠结束时回归的概率；失败则永久离开（模拟流失）
    pub return_rate: f64,
    /// 账户总数上限（防止长时间仿真把内存吃光）
    pub max_accounts: usize,

    // ———————————————————— 兴趣与结构 ————————————————————
    /// 兴趣维度（默认 8）
    pub interest_dims: usize,
    /// 同质性 ∈ `[0,1]`：0 = 兴趣随便、社团随便；1 = 只看同类
    pub homophily: f64,
    /// 兴趣与社团中心的相关度 ∈ `[0,1]`
    pub topic_correlation: f64,
    /// 社团数量
    pub community_count: usize,
    /// 桥接节点比例 ∈ `[0,1]`
    pub bridge_ratio: f64,
    /// 桥接节点额外跨社团的强度 ∈ `[0,1]`
    pub bridge_gain: f64,

    // ———————————————————— 关注与活跃 ————————————————————
    /// 新账户初始关注数下限
    pub follow_min: usize,
    /// 新账户初始关注数上限
    pub follow_max: usize,
    /// 单个账户关注数上限（内存上界的来源）
    pub max_follows: usize,
    /// 每小时新增关注的期望次数（乘以活跃强度）
    pub follow_rate: f64,
    /// 活跃强度的幂律指数（越小尾巴越重）
    pub activity_alpha: f64,
    /// 活跃强度上限（防止极端值把单步耗时打爆）
    pub activity_cap: f64,
    /// 每小时互动次数的基准值（乘以活跃强度）
    pub interactions_per_step: f64,
    /// 是否启用昼夜节律（关闭后活着就在线，便于做消融）
    pub enable_rhythm: bool,

    // ———————————————————— 评分行为 ————————————————————
    /// 内容质量的逐次抖动（每次发帖质量围绕账户质量上下波动）
    pub content_noise: f64,
    /// 评分的乘性噪声（对数正态的标准差）
    pub rating_noise: f64,
    /// 表态阈值：算出来的分低于它就不打分（0 = 没表态）
    pub min_stance: f32,
    /// 声誉反馈强度 ∈ `[0,1]`：大于 0 时高声誉者更容易被关注/互动（马太效应）
    pub reputation_feedback: f32,

    // ———————————————————— 攻击 ————————————————————
    /// 攻击者占初始人口的比例 ∈ `[0,1]`
    pub attack_ratio: f64,
    /// 攻击类型
    pub attack_kind: AttackKind,
    /// 渗透者被收买的时刻；`None` 表示默认在总步数的 1/3 处
    pub infiltrate_at: Option<u64>,
    /// 攻击者对被推广账户的额外关注倍率
    pub attack_focus: f64,

    // ———————————————————— 记录 ————————————————————
    /// 每隔多少步自动存一份快照；0 表示不自动存（只留 `report()`）
    pub snapshot_every: usize,
    /// 历史快照上限
    pub max_history: usize,
    /// 随机种子
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            n_initial: 200,
            steps: 200,
            join_rate: 0.15,
            leave_rate: 0.00012,
            dormancy_rate: 0.0005,
            sleep_hours: (48, 240),
            return_rate: 0.9,
            max_accounts: 20_000,

            interest_dims: crate::INTEREST_DIMS,
            homophily: 0.6,
            topic_correlation: 0.45,
            community_count: 4,
            bridge_ratio: 0.12,
            bridge_gain: 0.5,

            follow_min: 6,
            follow_max: 10,
            max_follows: 48,
            follow_rate: 0.008,
            activity_alpha: 1.7,
            activity_cap: 12.0,
            interactions_per_step: 0.9,
            enable_rhythm: true,

            content_noise: 0.15,
            rating_noise: 0.25,
            min_stance: 0.03,
            reputation_feedback: 0.0,

            attack_ratio: 0.1,
            attack_kind: AttackKind::None,
            infiltrate_at: None,
            attack_focus: 4.0,

            snapshot_every: 0,
            max_history: 4096,
            seed: 1,
        }
    }
}

impl SimConfig {
    /// 小规模配置，测试与快速试跑用。
    pub fn small(n_initial: usize, steps: usize) -> Self {
        Self {
            n_initial,
            steps,
            ..Self::default()
        }
    }

    /// 挂上一种攻击，其余用默认值。
    pub fn with_attack(mut self, kind: AttackKind, ratio: f64) -> Self {
        self.attack_kind = kind;
        self.attack_ratio = ratio;
        self
    }

    /// 「离线-回归」场景：大批账户前期活跃、随后长期休眠，只剩少数节点一直在网。
    ///
    /// 这是潜伏（sleeper）实验的底座：大部分账户在仿真前段就断线，之后长期不回来，
    /// 于是「一直在线」的少数节点承担了几乎全部的信息流转。
    ///
    /// 参数取法（全部落在已有开关上，不引入新概念）：
    /// - `dormancy_rate` 与步数成反比：整段仿真里平均每个账户遇到约 1 次休眠判定
    ///   （实际概率还要乘以「不活跃」倍率 1~2.5），于是不论跑 100 步还是 1000 步，
    ///   都有八成以上的账户在仿真里睡过，且多数在前 1/3 的时长内睡去；
    /// - `sleep_hours` 下限取总步数的一半、上限取 1.5 倍步数：睡着之后基本覆盖
    ///   剩余的全部仿真时长，模拟「长期不回来」；
    /// - `return_rate = 0.03`（≤ 0.05）：休眠结束时几乎都判定为流失；
    /// - `join_rate = 0`、`leave_rate = 0`：人口固定，离线只来自休眠，便于归因；
    /// - `enable_rhythm = false`：在线与否只由休眠决定，不被昼夜节律混淆；
    /// - 攻击类型是 `Sleeper { normal_until: steps / 5 }`：潜伏者在前 1/5 步完全正常。
    pub fn sleeper_scenario(n: usize, steps: usize, seed: u64) -> Self {
        let steps = steps.max(1);
        let mut cfg = SimConfig::small(n, steps);
        cfg.seed = seed;
        cfg.enable_rhythm = false;
        cfg.join_rate = 0.0;
        cfg.leave_rate = 0.0;
        // 1.0 / steps：平均每个账户在整段仿真里遇到约 1 次休眠判定
        cfg.dormancy_rate = (1.0 / steps as f64).clamp(1e-6, 0.2);
        // 睡眠时长覆盖「大半仿真时长」：最短也有总步数的一半
        let lo = (steps as u64 / 2).max(24);
        let hi = ((steps as u64) * 3 / 2).max(lo + 24);
        cfg.sleep_hours = (lo, hi);
        cfg.return_rate = 0.03;
        cfg.attack_ratio = 0.1;
        cfg.attack_kind = AttackKind::Sleeper {
            normal_until: (steps as u64 / 5).max(1),
        };
        cfg
    }

    /// 夹紧所有越界参数，返回可安全运行的配置。
    ///
    /// 规则：
    /// - 比例类参数统一夹到 `[0,1]`；
    /// - 计数类参数至少为 1（社团数、兴趣维度、关注上限）；
    /// - 区间类参数（休眠时长）保证 `lo <= hi`；
    /// - NaN / inf 一律回退到默认值。
    pub fn validated(mut self) -> Self {
        let d = SimConfig::default();
        self.join_rate = non_neg(self.join_rate, d.join_rate);
        self.leave_rate = unit(self.leave_rate, d.leave_rate);
        self.dormancy_rate = unit(self.dormancy_rate, d.dormancy_rate);
        let (lo, hi) = self.sleep_hours;
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        self.sleep_hours = (lo.max(1), hi.clamp(1, 24 * 365));
        self.return_rate = unit(self.return_rate, d.return_rate);
        self.max_accounts = self.max_accounts.max(self.n_initial.max(1)).min(10_000_000);

        self.interest_dims = self.interest_dims.clamp(2, 64);
        self.homophily = unit(self.homophily, d.homophily);
        self.topic_correlation = unit(self.topic_correlation, d.topic_correlation);
        self.community_count = self.community_count.clamp(1, 4096);
        self.bridge_ratio = unit(self.bridge_ratio, d.bridge_ratio);
        self.bridge_gain = unit(self.bridge_gain, d.bridge_gain);

        self.max_follows = self.max_follows.clamp(1, 4096);
        self.follow_min = self.follow_min.max(1).min(self.max_follows);
        self.follow_max = self.follow_max.clamp(self.follow_min, self.max_follows);
        self.follow_rate = non_neg(self.follow_rate, d.follow_rate);
        self.activity_alpha = if self.activity_alpha.is_finite() && self.activity_alpha > 0.1 {
            self.activity_alpha.min(16.0)
        } else {
            d.activity_alpha
        };
        self.activity_cap = if self.activity_cap.is_finite() {
            self.activity_cap.clamp(1.0, 4096.0)
        } else {
            d.activity_cap
        };
        self.interactions_per_step = non_neg(self.interactions_per_step, d.interactions_per_step);

        self.content_noise = unit(self.content_noise, d.content_noise);
        self.rating_noise = non_neg(self.rating_noise, d.rating_noise).min(2.0);
        self.min_stance = if self.min_stance.is_finite() {
            self.min_stance.clamp(0.0, 1.0)
        } else {
            d.min_stance
        };
        self.reputation_feedback = if self.reputation_feedback.is_finite() {
            self.reputation_feedback.clamp(0.0, 1.0)
        } else {
            d.reputation_feedback
        };

        self.attack_ratio = unit(self.attack_ratio, d.attack_ratio);
        if let AttackKind::Endorser { ratio } = self.attack_kind {
            let r = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.0 };
            self.attack_kind = AttackKind::Endorser { ratio: r };
        }
        if let AttackKind::Sybil { cluster } = self.attack_kind {
            self.attack_kind = AttackKind::Sybil {
                cluster: cluster.clamp(1, 1024),
            };
        }
        self.attack_focus = if self.attack_focus.is_finite() {
            self.attack_focus.clamp(1.0, 32.0)
        } else {
            d.attack_focus
        };

        self.max_history = self.max_history.min(1_000_000);
        if self.snapshot_every > 0 {
            self.snapshot_every = self.snapshot_every.min(100_000);
        }
        self
    }

    /// 解析渗透者被收买的具体时刻。
    ///
    /// 未显式指定时，默认在总步数的 1/3 处（至少 1），这样前面有足够时间积累声誉。
    pub fn resolved_infiltrate_at(&self) -> u64 {
        match self.infiltrate_at {
            Some(t) => t,
            None => ((self.steps.max(1) as f64 / 3.0).round() as u64).max(1),
        }
    }

    /// 本次仿真期望创建的攻击者数量（按初始人口比例）。
    pub fn attack_population(&self) -> usize {
        ((self.n_initial as f64) * self.attack_ratio).round() as usize
    }
}

fn unit(x: f64, fallback: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

fn non_neg(x: f64, fallback: f64) -> f64 {
    if x.is_finite() {
        // 负数说明写错了，夹到 0 比悄悄用默认值更容易被发现
        x.clamp(0.0, 1e6)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认配置合法() {
        let c = SimConfig::default().validated();
        assert!(c.follow_min >= 1);
        assert!(c.follow_min <= c.follow_max);
        assert!(c.follow_max <= c.max_follows);
        assert!(c.community_count >= 1);
        assert!(c.interest_dims >= 2);
        assert_eq!(c.attack_kind, AttackKind::None);
        assert!(c.max_accounts >= c.n_initial);
    }

    #[test]
    fn 非法值被夹紧而不是崩溃() {
        let c = SimConfig {
            n_initial: 10,
            homophily: 9.0,
            bridge_ratio: -3.0,
            topic_correlation: f64::NAN,
            community_count: 0,
            interest_dims: 1,
            follow_min: 0,
            follow_max: 999,
            max_follows: 5,
            sleep_hours: (200, 10),
            join_rate: -1.0,
            leave_rate: f64::INFINITY,
            return_rate: 2.0,
            activity_alpha: 0.0,
            activity_cap: f64::NAN,
            min_stance: 5.0,
            attack_ratio: 3.0,
            attack_focus: f64::NAN,
            reputation_feedback: -1.0,
            ..SimConfig::default()
        }
        .validated();
        assert_eq!(c.homophily, 1.0);
        assert_eq!(c.bridge_ratio, 0.0);
        assert!(c.topic_correlation.is_finite());
        assert!(c.community_count >= 1);
        assert!(c.interest_dims >= 2);
        assert!(c.follow_min >= 1);
        assert!(c.follow_max <= 5);
        assert!(c.sleep_hours.0 <= c.sleep_hours.1);
        assert_eq!(c.join_rate, 0.0);
        assert!(c.leave_rate <= 1.0);
        assert_eq!(c.return_rate, 1.0);
        assert!(c.activity_alpha > 0.0);
        assert!(c.activity_cap.is_finite());
        assert_eq!(c.min_stance, 1.0);
        assert_eq!(c.attack_ratio, 1.0);
        assert!(c.attack_focus.is_finite());
        assert_eq!(c.reputation_feedback, 0.0);
    }

    #[test]
    fn 攻击人口按比例计算() {
        let c = SimConfig {
            n_initial: 200,
            attack_ratio: 0.1,
            ..SimConfig::default()
        };
        assert_eq!(c.attack_population(), 20);
        let c = SimConfig {
            n_initial: 0,
            ..SimConfig::default()
        };
        assert_eq!(c.attack_population(), 0);
        let c = SimConfig {
            n_initial: 3,
            attack_ratio: 1.0,
            ..SimConfig::default()
        };
        assert_eq!(c.attack_population(), 3);
    }

    #[test]
    fn 收买时刻有默认值() {
        let c = SimConfig::default();
        assert_eq!(c.resolved_infiltrate_at(), 67); // 200 / 3 四舍五入
        let c = SimConfig {
            steps: 0,
            ..SimConfig::default()
        };
        assert_eq!(c.resolved_infiltrate_at(), 1); // 至少 1
        let c = SimConfig {
            infiltrate_at: Some(7),
            ..SimConfig::default()
        };
        assert_eq!(c.resolved_infiltrate_at(), 7);
    }

    #[test]
    fn 女巫集群数与背书比例被限制() {
        let c = SimConfig {
            attack_kind: AttackKind::Sybil { cluster: 0 },
            ..SimConfig::default()
        }
        .validated();
        assert_eq!(c.attack_kind, AttackKind::Sybil { cluster: 1 });

        let c = SimConfig {
            attack_kind: AttackKind::Endorser { ratio: f32::NAN },
            ..SimConfig::default()
        }
        .validated();
        assert_eq!(c.attack_kind, AttackKind::Endorser { ratio: 0.0 });

        let c = SimConfig {
            attack_kind: AttackKind::Endorser { ratio: 2.5 },
            ..SimConfig::default()
        }
        .validated();
        assert_eq!(c.attack_kind, AttackKind::Endorser { ratio: 1.0 });
    }

    #[test]
    fn sleeper_scenario_参数符合语义() {
        let c = SimConfig::sleeper_scenario(200, 200, 9).validated();
        assert_eq!(c.n_initial, 200);
        assert_eq!(c.steps, 200);
        assert_eq!(c.seed, 9);
        assert_eq!(c.attack_kind, AttackKind::Sleeper { normal_until: 40 });
        assert!(c.return_rate <= 0.05, "回归概率必须足够低");
        assert!(c.sleep_hours.0 >= 100, "睡眠时长应覆盖大半仿真时长");
        assert!(c.sleep_hours.0 <= c.sleep_hours.1);
        assert!(c.dormancy_rate > 0.0, "没有休眠就没有离线轨迹");
        assert_eq!(c.join_rate, 0.0);
        assert_eq!(c.leave_rate, 0.0);
        assert!(!c.enable_rhythm, "在线与否只由休眠决定");
        // 步数退化到 0 时也不能造出非法配置
        let c = SimConfig::sleeper_scenario(10, 0, 1).validated();
        assert!(c.sleep_hours.0 >= 1);
        assert!(c.sleep_hours.0 <= c.sleep_hours.1);
        assert_eq!(c.attack_kind, AttackKind::Sleeper { normal_until: 1 });
    }

    #[test]
    fn 攻击标签稳定() {
        assert_eq!(AttackKind::None.label(), "none");
        assert_eq!(AttackKind::Sybil { cluster: 3 }.label(), "sybil");
        assert_eq!(AttackKind::Sleeper { normal_until: 9 }.label(), "sleeper");
        assert_eq!(AttackKind::Infiltrator.label(), "infiltrator");
        assert_eq!(AttackKind::Endorser { ratio: 0.1 }.label(), "endorser");
        assert_eq!(AttackKind::Spammer.label(), "spammer");
    }
}
