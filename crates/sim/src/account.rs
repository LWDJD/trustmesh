//! 账户：身份、兴趣、活跃度、生命周期状态、关注与评分。
//!
//! 账户一旦创建就**不会从 `Vec` 里删除**（内部编号不回收），
//! 离开只是把状态改成 `Life::Left`。这样：
//!
//! - 内部编号稳定，可以直接当 `AccountIdx` 用；
//! - 离开账户的历史评分仍然参与统计（死人也会被记住）；
//! - 内存是「账户数 × 关注上限」的线性函数，可以被配置封顶。

use crate::interest::Interest;

/// 账户种类。前六种是协议里的角色，`Promoted` 是攻击的收益方。
///
/// `Promoted` 单独拿出来，是因为「被推广的账户」在行为上必须和诚实账户完全一样——
/// 否则攻击就太容易被识别了。它的可疑之处只在结果上：拿到的评分远高于内容质量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 诚实账户：按「质量 × 兴趣匹配 × 噪声」打分
    Honest,
    /// 女巫账户：第 `cluster` 号集群，只在集群内部互评满分
    Sybil { cluster: usize },
    /// 潜伏者：`normal_until` 之前完全正常，之后给被推广账户打满分
    Sleeper { normal_until: u64 },
    /// 渗透者：`bought_at` 时刻被收买的高分账户
    Infiltrator { bought_at: u64 },
    /// 背书者：公开给被推广账户打满分的高质量账户
    Endorser,
    /// 垃圾制造者：内容极差、活跃极高、不看质量乱打分
    Spammer,
    /// 被推广账户：攻击的收益方，行为与诚实账户一致
    Promoted,
}

/// 账户种类数量（快照里每类一行）。
pub const CLASS_COUNT: usize = 7;

impl Kind {
    /// 指标里用的稳定标签。
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Honest => "honest",
            Kind::Sybil { .. } => "sybil",
            Kind::Sleeper { .. } => "sleeper",
            Kind::Infiltrator { .. } => "infiltrator",
            Kind::Endorser => "endorser",
            Kind::Spammer => "spammer",
            Kind::Promoted => "promoted",
        }
    }

    /// 在快照分类数组里的下标，顺序固定为 honest, sybil, sleeper, infiltrator, endorser, spammer, promoted。
    pub fn class_index(&self) -> usize {
        match self {
            Kind::Honest => 0,
            Kind::Sybil { .. } => 1,
            Kind::Sleeper { .. } => 2,
            Kind::Infiltrator { .. } => 3,
            Kind::Endorser => 4,
            Kind::Spammer => 5,
            Kind::Promoted => 6,
        }
    }

    /// 是否属于「攻击方」。
    ///
    /// `Promoted` 不算攻击方：它只是收益方，行为上和诚实账户没有区别。
    pub fn is_attacker(&self) -> bool {
        matches!(
            self,
            Kind::Sybil { .. }
                | Kind::Sleeper { .. }
                | Kind::Infiltrator { .. }
                | Kind::Endorser
                | Kind::Spammer
        )
    }

    /// 是否按「兴趣 × 质量」的正常逻辑建立关注。
    ///
    /// 女巫只在集群内部互关，不参与正常的社交发现。
    pub fn uses_social_discovery(&self) -> bool {
        !matches!(self, Kind::Sybil { .. })
    }
}

/// 生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    /// 活跃（在线与否还要看活跃时段）
    Active,
    /// 休眠中，`until` 时刻尝试回归
    Dormant { until: u64 },
    /// 已永久离开
    Left { at: u64 },
}

/// 一个账户的全部状态。
#[derive(Clone, Debug)]
pub struct Account {
    /// 内部编号（等于在 `World::accounts` 里的下标，永不回收）
    pub idx: usize,
    /// 账户种类
    pub kind: Kind,
    /// 加入时刻（小时）
    pub joined_at: u64,
    /// 所属社团
    pub community: usize,
    /// 是否桥接节点（额外跨社团关注）
    pub is_bridge: bool,
    /// 兴趣向量
    pub interest: Interest,
    /// 内容质量 ∈ `[0,1]`，决定别人给它打多少分
    pub quality: f32,
    /// 活跃强度（幂律分布，1 起步，长尾可以到几十）
    pub activity: f64,
    /// 活跃时段起点（0..24）
    pub active_start: u8,
    /// 活跃时段长度（1..=24 小时）
    pub active_span: u8,
    /// 生命周期状态
    pub life: Life,
    /// 攻击是否已经生效（潜伏者突变、渗透者被收买）
    pub attack_active: bool,
    /// 声誉代理：收到评分的指数滑动平均（不是信任算法的输出）
    pub reputation: f32,
    /// 收到的评分之和
    pub received_sum: f64,
    /// 收到的评分条数
    pub received_count: usize,
    /// 关注列表（内部编号，插入顺序）
    pub follows: Vec<usize>,
    /// 我给出的评分：`(目标内部编号, 评分)`，只存正分，插入顺序
    pub ratings: Vec<(usize, f32)>,
}

impl Account {
    /// 是否还活着（未永久离开）。
    pub fn is_alive(&self) -> bool {
        !matches!(self.life, Life::Left { .. })
    }

    /// 是否处于休眠。
    pub fn is_dormant(&self) -> bool {
        matches!(self.life, Life::Dormant { .. })
    }

    /// 是否已永久离开。
    pub fn is_left(&self) -> bool {
        matches!(self.life, Life::Left { .. })
    }

    /// 是否在线。
    ///
    /// - 离开/休眠：不在线
    /// - 关闭昼夜节律：只要活着就一直在线
    /// - 打开节律：落在 `[active_start, active_start + active_span)`（对 24 取模）里
    pub fn is_online(&self, time: u64, rhythm: bool) -> bool {
        if !matches!(self.life, Life::Active) {
            return false;
        }
        if !rhythm {
            return true;
        }
        let hour = (time % 24) as u32;
        let start = self.active_start as u32 % 24;
        let span = (self.active_span as u32).clamp(1, 24);
        if span >= 24 {
            return true;
        }
        ((hour + 24 - start) % 24) < span
    }

    /// 账户年龄（小时）。
    pub fn age(&self, time: u64) -> u64 {
        time.saturating_sub(self.joined_at)
    }

    /// 平均收到的评分（没有收到过就是 0）。
    pub fn mean_received(&self) -> f32 {
        if self.received_count == 0 {
            0.0
        } else {
            (self.received_sum / self.received_count as f64) as f32
        }
    }

    /// 我是否关注了 `target`。
    pub fn follows(&self, target: usize) -> bool {
        self.follows.contains(&target)
    }

    /// 我对 `target` 的评分（没评过返回 `None`）。
    pub fn rating_of(&self, target: usize) -> Option<f32> {
        self.ratings
            .iter()
            .find(|(t, _)| *t == target)
            .map(|(_, s)| *s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    fn 造账户(seed: u64) -> Account {
        let mut rng = Rng::new(seed);
        Account {
            idx: 3,
            kind: Kind::Honest,
            joined_at: 10,
            community: 1,
            is_bridge: false,
            interest: Interest::random(8, &mut rng),
            quality: 0.5,
            activity: 1.0,
            active_start: 8,
            active_span: 8,
            life: Life::Active,
            attack_active: false,
            reputation: 0.0,
            received_sum: 0.0,
            received_count: 0,
            follows: vec![1, 5],
            ratings: vec![(5, 0.7)],
        }
    }

    #[test]
    fn 分类下标唯一且连续() {
        let kinds = [
            Kind::Honest,
            Kind::Sybil { cluster: 0 },
            Kind::Sleeper { normal_until: 10 },
            Kind::Infiltrator { bought_at: 3 },
            Kind::Endorser,
            Kind::Spammer,
            Kind::Promoted,
        ];
        for (i, k) in kinds.iter().enumerate() {
            assert_eq!(k.class_index(), i, "{} 的下标错了", k.label());
        }
        // 标签不能重复，否则报表会混淆
        let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), CLASS_COUNT);
    }

    #[test]
    fn 攻击方判定() {
        assert!(!Kind::Honest.is_attacker());
        assert!(!Kind::Promoted.is_attacker());
        assert!(Kind::Sybil { cluster: 1 }.is_attacker());
        assert!(Kind::Sleeper { normal_until: 5 }.is_attacker());
        assert!(Kind::Infiltrator { bought_at: 5 }.is_attacker());
        assert!(Kind::Endorser.is_attacker());
        assert!(Kind::Spammer.is_attacker());
        // 只有女巫不参与正常社交发现
        assert!(!Kind::Sybil { cluster: 0 }.uses_social_discovery());
        assert!(Kind::Honest.uses_social_discovery());
    }

    #[test]
    fn 在线时段含跨零点() {
        let mut a = 造账户(1);
        a.active_start = 22;
        a.active_span = 8; // 22,23,0..5 在线；6 点下线
        assert!(a.is_online(22, true));
        assert!(a.is_online(23, true));
        assert!(a.is_online(24, true)); // 第二天 0 点
        assert!(a.is_online(29, true)); // 5 点
        assert!(!a.is_online(30, true)); // 6 点
        assert!(!a.is_online(21, true));
        // 关闭节律后恒定在线
        assert!(a.is_online(30, false));
        // 满 24 小时在线
        a.active_span = 24;
        assert!(a.is_online(7, true));
    }

    #[test]
    fn 非活跃状态一律离线() {
        let mut a = 造账户(2);
        a.life = Life::Dormant { until: 100 };
        assert!(!a.is_online(1, false));
        a.life = Life::Left { at: 5 };
        assert!(!a.is_online(1, false));
        assert!(!a.is_alive());
        assert!(a.is_left());
        a.life = Life::Dormant { until: 100 };
        assert!(a.is_alive());
        assert!(a.is_dormant());
    }

    #[test]
    fn 均分与查询() {
        let mut a = 造账户(3);
        assert_eq!(a.mean_received(), 0.0);
        a.received_sum = 0.9;
        a.received_count = 3;
        assert!((a.mean_received() - 0.3).abs() < 1e-6);
        assert_eq!(a.rating_of(5), Some(0.7));
        assert_eq!(a.rating_of(9), None);
        assert!(a.follows(1));
        assert!(!a.follows(2));
        assert_eq!(a.age(25), 15);
        assert_eq!(a.age(3), 0);
    }
}
