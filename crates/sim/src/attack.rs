//! 攻击编排：谁在什么时候变成攻击者、收益方是谁。
//!
//! 攻击的类型与规模由 `SimConfig` 决定，本模块只保存**计划与结果**：
//!
//! - `target`：被推广的账户（攻击的收益方）。女巫/潜伏者/垃圾用自己人，渗透/背书用一个
//!   看起来完全正常的 `Promoted` 账户；
//! - `attackers`：当前实际处于攻击状态的账户；
//! - `clusters`：女巫集群划分，集群内部互评满分、彼此不越界；
//! - `purchased` / `purchased_at`：渗透者名单与收买时刻。
//!
//! 具体的行为分支在 `World::compute_rating` 里，本模块不碰账户状态。

use crate::config::AttackKind;

/// 一次仿真里的攻击计划与执行结果。
#[derive(Clone, Debug, Default)]
pub struct AttackPlan {
    /// 攻击类型
    pub kind: Option<AttackKind>,
    /// 被推广账户（收益方）的内部编号
    pub target: Option<usize>,
    /// 处于攻击状态的账户
    pub attackers: Vec<usize>,
    /// 女巫集群：每个集群的成员内部编号
    pub clusters: Vec<Vec<usize>>,
    /// 被收买的渗透者
    pub purchased: Vec<usize>,
    /// 收买发生的时刻（小时）
    pub purchased_at: u64,
    /// 收买是否已经执行（避免重复执行）
    pub done: bool,
    /// 背书者数量
    pub endorsers: usize,
}

impl AttackPlan {
    /// 空计划（无攻击）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 是否有收益方。
    pub fn has_target(&self) -> bool {
        self.target.is_some()
    }

    /// 是否有任何攻击者。
    pub fn is_active(&self) -> bool {
        !self.attackers.is_empty()
    }

    /// 女巫集群数量。
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// 某个账户所在的集群（不是女巫返回 `None`）。
    pub fn cluster_of(&self, idx: usize) -> Option<usize> {
        self.clusters
            .iter()
            .position(|members| members.contains(&idx))
    }

    /// 攻击类型标签，用于报表。
    pub fn label(&self) -> &'static str {
        match self.kind {
            Some(k) => k.label(),
            None => "none",
        }
    }

    /// 一句话描述攻击规模，用于输出。
    pub fn describe(&self) -> String {
        match self.kind {
            None | Some(AttackKind::None) => "无攻击".to_string(),
            Some(AttackKind::Sybil { cluster }) => format!(
                "女巫攻击：{} 个集群 / {} 个账户",
                cluster,
                self.attackers.len()
            ),
            Some(AttackKind::Sleeper { normal_until }) => format!(
                "潜伏攻击：{} 个潜伏者，{} 小时后突变",
                self.attackers.len(),
                normal_until
            ),
            Some(AttackKind::Infiltrator) => format!(
                "渗透攻击：{} 个高分账户在 {} 小时被收买",
                self.purchased.len(),
                self.purchased_at
            ),
            Some(AttackKind::Endorser { ratio }) => format!(
                "背书攻击：{}% 的高质量账户（{} 个）为被推广账户背书",
                (ratio * 100.0).round() as i64,
                self.endorsers
            ),
            Some(AttackKind::Spammer) => {
                format!("垃圾攻击：{} 个垃圾制造者", self.attackers.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 空计划() {
        let p = AttackPlan::empty();
        assert!(!p.has_target());
        assert!(!p.is_active());
        assert_eq!(p.cluster_count(), 0);
        assert_eq!(p.cluster_of(3), None);
        assert_eq!(p.label(), "none");
        assert_eq!(p.describe(), "无攻击");
    }

    #[test]
    fn 集群归属查询() {
        let p = AttackPlan {
            clusters: vec![vec![3, 4], vec![9, 10, 11]],
            ..AttackPlan::empty()
        };
        assert_eq!(p.cluster_of(3), Some(0));
        assert_eq!(p.cluster_of(11), Some(1));
        assert_eq!(p.cluster_of(5), None);
        assert_eq!(p.cluster_count(), 2);
    }

    #[test]
    fn 描述文本包含规模() {
        let p = AttackPlan {
            kind: Some(AttackKind::Sybil { cluster: 2 }),
            attackers: vec![1, 2, 3],
            ..AttackPlan::empty()
        };
        let s = p.describe();
        assert!(s.contains('2') && s.contains('3'), "描述缺规模: {s}");
        assert_eq!(p.label(), "sybil");
    }

    #[test]
    fn 默认值可用() {
        let p = AttackPlan::default();
        assert!(p.kind.is_none());
        assert!(!p.done);
        assert_eq!(p.purchased_at, 0);
        assert_eq!(p.endorsers, 0);
    }
}
