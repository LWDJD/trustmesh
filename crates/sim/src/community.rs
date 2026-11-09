//! 社团结构：每个社团有一根「主题中心」，以及成员名单。
//!
//! 社团影响两处：
//!
//! 1. **关注对象**：跨社团关注的比例由「同质性」和「桥接节点」共同决定；
//! 2. **兴趣分布**：成员兴趣 = 随机兴趣与社团中心按 `topic_correlation` 混合。
//!
//! 社团成员名单按内部编号升序维护，遍历顺序固定，保证复现性。

use crate::interest::Interest;
use crate::rng::Rng;

/// 全部社团。
#[derive(Clone, Debug)]
pub struct Communities {
    /// 每个社团的主题中心（单位向量）
    centers: Vec<Interest>,
    /// 每个社团的成员内部编号
    members: Vec<Vec<usize>>,
}

impl Communities {
    /// 建 `count` 个社团，每个社团抽一根主题中心。
    ///
    /// `count` 至少为 1。
    pub fn new(count: usize, dims: usize, rng: &mut Rng) -> Self {
        let count = count.max(1);
        let centers = (0..count)
            .map(|_| Interest::random(dims.max(1), rng))
            .collect();
        Self {
            centers,
            members: vec![Vec::new(); count],
        }
    }

    /// 社团数量。
    pub fn len(&self) -> usize {
        self.centers.len()
    }

    /// 是否为空（恒为 false，保留惯例方法）。
    pub fn is_empty(&self) -> bool {
        self.centers.is_empty()
    }

    /// 第 `c` 个社团的主题中心。
    pub fn center(&self, c: usize) -> &Interest {
        &self.centers[c.min(self.centers.len() - 1)]
    }

    /// 第 `c` 个社团的成员（升序）。
    pub fn members(&self, c: usize) -> &[usize] {
        &self.members[c.min(self.members.len() - 1)]
    }

    /// 第 `c` 个社团的成员数。
    pub fn member_count(&self, c: usize) -> usize {
        self.members(c).len()
    }

    /// 把一个账户登记进社团。重复登记会被忽略。
    pub fn add_member(&mut self, c: usize, idx: usize) {
        let c = c.min(self.members.len() - 1);
        if !self.members[c].contains(&idx) {
            self.members[c].push(idx);
        }
    }

    /// 按成员数选一个社团：`w = 成员数 + 1`。
    ///
    /// +1 是为了让空社团/小社团也有机会，避免新账户全挤进最大的社团。
    pub fn choose(&self, rng: &mut Rng) -> usize {
        let weights: Vec<f64> = self
            .members
            .iter()
            .map(|m| (m.len() as f64) + 1.0)
            .collect();
        rng.weighted_index(&weights).unwrap_or(0)
    }

    /// 按成员数选一个「不是 `c`」的社团，用于跨社团抽样。
    ///
    /// 只剩一个社团时返回 `None`。
    pub fn other_than(&self, c: usize, rng: &mut Rng) -> Option<usize> {
        if self.len() <= 1 {
            return None;
        }
        let mut weights: Vec<f64> = self
            .members
            .iter()
            .map(|m| (m.len() as f64) + 1.0)
            .collect();
        let self_idx = c.min(weights.len() - 1);
        weights[self_idx] = 0.0;
        rng.weighted_index(&weights)
    }
}

/// 一个账户的「跨社团关注比例」。
///
/// 组成：
///
/// - `random_mix × (1 − homophily)`：完全随机时的跨社团比例是 `1 − 1/K`，
///   同质性把它线性压到 0。所以 homophily = 0 → 接近随机混合，homophily = 1 → 只在社团内。
/// - 桥接节点额外跨社团：`bridge_gain` 把剩余部分再拉高一半左右。
///
/// 结果夹在 [0, 0.9]，保证任何账户都至少保留少量社团内关注（否则社团结构会退化）。
pub fn cross_community_share(
    homophily: f64,
    is_bridge: bool,
    bridge_gain: f64,
    community_count: usize,
) -> f64 {
    if community_count <= 1 {
        // 只有一个社团时不存在「跨社团」，桥接节点也没地方去
        return 0.0;
    }
    let k = community_count as f64;
    let random_mix = 1.0 - 1.0 / k;
    let h = homophily.clamp(0.0, 1.0);
    let mut share = random_mix * (1.0 - h);
    if is_bridge {
        let gain = bridge_gain.clamp(0.0, 1.0);
        share += (1.0 - share) * gain;
    }
    share.clamp(0.0, 0.9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 社团中心是单位向量() {
        let mut rng = Rng::new(4);
        let c = Communities::new(5, 8, &mut rng);
        assert_eq!(c.len(), 5);
        for k in 0..5 {
            assert!((c.center(k).norm() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn 成员登记与去重() {
        let mut rng = Rng::new(4);
        let mut c = Communities::new(2, 8, &mut rng);
        c.add_member(0, 7);
        c.add_member(0, 3);
        c.add_member(0, 7); // 重复
        assert_eq!(c.member_count(0), 2);
        assert_eq!(c.members(0), &[7, 3]); // 插入顺序
        assert_eq!(c.members(1), &[] as &[usize]);
    }

    #[test]
    fn 社团选择按规模加权() {
        let mut rng = Rng::new(6);
        let mut c = Communities::new(3, 8, &mut rng);
        for i in 0..40 {
            c.add_member(1, i);
        }
        let mut counts = [0usize; 3];
        for _ in 0..10_000 {
            counts[c.choose(&mut rng)] += 1;
        }
        assert!(counts[1] > counts[0] + 3000, "大社团应被明显偏好: {counts:?}");
        assert!(counts[0] > 100 && counts[2] > 100, "小社团不能被饿死: {counts:?}");
    }

    #[test]
    fn 跨社团比例随同质性下降() {
        let mut prev = 2.0;
        for step in 0..=10 {
            let h = step as f64 / 10.0;
            let s = cross_community_share(h, false, 0.5, 4);
            assert!(s <= prev + 1e-12, "同质性上升时跨社团比例必须单调下降");
            assert!((0.0..=0.9).contains(&s));
            prev = s;
        }
        assert!(cross_community_share(0.0, false, 0.5, 4) > 0.7);
        assert!(cross_community_share(1.0, false, 0.5, 4) < 1e-9);
    }

    #[test]
    fn 桥接节点跨社团更多() {
        for h in [0.0, 0.3, 0.6, 0.9, 1.0] {
            let normal = cross_community_share(h, false, 0.5, 4);
            let bridge = cross_community_share(h, true, 0.5, 4);
            assert!(bridge >= normal, "桥接节点跨社团比例不能更低");
            assert!(bridge <= 0.9);
        }
        // 同质性拉满时，唯一的外部连接来自桥接节点
        assert!(cross_community_share(1.0, true, 0.5, 4) > 0.4);
    }

    #[test]
    fn 单社团时没有跨社团关注() {
        assert_eq!(cross_community_share(0.0, false, 0.5, 1), 0.0);
        assert_eq!(cross_community_share(1.0, true, 0.5, 1), 0.0);
    }
}
