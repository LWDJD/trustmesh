//! 评分清单：本地维护「我评过分的账户」及其抽样时间。
//!
//! # 设计
//!
//! 清单里只有自己评过分的账户。每次抽样从清单里按「衰减后权重」抽取，
//! 抽到的结果参与信任合并。
//!
//! 每个条目记录：
//! - 账户 ID
//! - 原始评分（[-1, 1]）
//! - 抽样时间
//!
//! # 衰减
//!
//! 线性衰减，半衰期 `H`：
//!
//! ```text
//! factor(t) = max(0, 1 - t / (2H))
//! ```
//!
//! t=0 时 1.0，t=H 时 0.5，t=2H 时 0。H=3 天意味着 3 天减半，6 天归零。
//!
//! 与指数衰减的区别：指数永不归零，线性会彻底失效。适合「旧关系该忘就忘」的场景。

use std::collections::HashMap;

use crate::rng::Rng;

/// 一条评分记录。
#[derive(Clone, Debug)]
pub struct RatingEntry {
    pub account: usize,
    pub weight: f64,
    pub sampled_at: u64,
}

/// 存储模式。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StorageMode {
    /// 全量驻留内存，查询最快，适合桌面或服务器。
    Memory,
    /// 只保留最近写入的，适合手机，需要配合外部持久化。
    Compact,
}

/// 本地评分清单。
pub struct RatingList {
    entries: Vec<RatingEntry>,
    /// 半衰期（与时间单位一致，比如小时）
    half_life: u64,
    /// 最大条目数
    capacity: usize,
    /// 存储模式
    mode: StorageMode,
    /// 淘汰计数，用于统计
    pub evicted: usize,
}

impl RatingList {
    pub fn new(half_life: u64, capacity: usize, mode: StorageMode) -> Self {
        Self {
            entries: Vec::new(),
            half_life: half_life.max(1),
            capacity: capacity.max(1),
            mode,
            evicted: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 记录一次评分。同一账户重复评分时更新权重和时间。
    pub fn record(&mut self, account: usize, weight: f64, now: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.account == account) {
            e.weight = weight;
            e.sampled_at = now;
            return;
        }
        self.entries.push(RatingEntry {
            account,
            weight,
            sampled_at: now,
        });
        self.enforce_capacity(now);
    }

    /// 容量控制：先清掉衰减到零的，再按时间淘汰最旧的。
    fn enforce_capacity(&mut self, now: u64) {
        if self.entries.len() <= self.capacity {
            return;
        }

        // 清掉已经衰减到零的
        let h = self.half_life;
        self.entries
            .retain(|e| now.saturating_sub(e.sampled_at) < 2 * h);

        // 还超就按时间淘汰最旧的
        while self.entries.len() > self.capacity {
            if let Some((idx, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.sampled_at)
            {
                self.entries.swap_remove(idx);
                self.evicted += 1;
            } else {
                break;
            }
        }
    }

    /// 某条目在 `now` 时刻的衰减后权重。
    pub fn weight_at(&self, e: &RatingEntry, now: u64) -> f64 {
        let age = now.saturating_sub(e.sampled_at) as f64;
        let h = self.half_life as f64;
        let factor = 1.0 - age / (2.0 * h);
        factor.max(0.0) * e.weight
    }

    /// 当前所有条目的衰减后权重。
    pub fn decayed_weights(&self, now: u64) -> Vec<(usize, f64)> {
        self.entries
            .iter()
            .map(|e| (e.account, self.weight_at(e, now)))
            .filter(|(_, w)| *w > 0.0)
            .collect()
    }

    /// 按衰减后权重抽样 k 次（有放回），返回 (账户, 命中比例)。
    ///
    /// 命中比例 = 次数/k，作为该样本的合并权重。
    pub fn sample(&self, k: usize, now: u64, rng: &mut Rng) -> Vec<(usize, f64)> {
        let pool = self.decayed_weights(now);
        if pool.is_empty() || k == 0 {
            return Vec::new();
        }

        let total: f64 = pool.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            return Vec::new();
        }

        // 累积分布
        let mut cum: Vec<(usize, f64)> = Vec::with_capacity(pool.len());
        let mut acc = 0.0;
        for (a, w) in &pool {
            acc += w / total;
            cum.push((*a, acc));
        }

        let mut hits: HashMap<usize, usize> = HashMap::new();
        for _ in 0..k {
            let r = rng.next_f64();
            let mut lo = 0;
            let mut hi = cum.len() - 1;
            while lo < hi {
                let mid = (lo + hi) / 2;
                if cum[mid].1 < r {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            *hits.entry(cum[lo].0).or_insert(0) += 1;
        }

        hits.into_iter()
            .map(|(a, c)| (a, c as f64 / k as f64))
            .collect()
    }

    /// 合并权重：返回每个账户的归一化权重，用于信任合并。
    pub fn merge_weights(&self, now: u64) -> HashMap<usize, f64> {
        let dw = self.decayed_weights(now);
        let total: f64 = dw.iter().map(|(_, w)| w).sum();
        let mut out = HashMap::new();
        if total <= 0.0 {
            return out;
        }
        for (a, w) in dw {
            out.insert(a, w / total);
        }
        out
    }

    /// 存盘模式下的压缩：只保留最近的，返回被淘汰的条目。
    pub fn compact(&mut self, keep: usize, now: u64) -> Vec<RatingEntry> {
        if self.mode != StorageMode::Compact {
            return Vec::new();
        }
        if self.entries.len() <= keep {
            return Vec::new();
        }
        self.entries.sort_by_key(|e| e.sampled_at);
        let drop_n = self.entries.len() - keep;
        let dropped: Vec<RatingEntry> = self.entries.drain(0..drop_n).collect();
        self.evicted += drop_n;
        let _ = now;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_decay() {
        let list = RatingList::new(72, 100, StorageMode::Memory); // 3 天 = 72 小时
        let mut l = list;
        l.record(1, 1.0, 0);

        // t=0: 1.0
        assert!((l.decayed_weights(0)[0].1 - 1.0).abs() < 1e-9);
        // t=H: 0.5
        assert!((l.decayed_weights(72)[0].1 - 0.5).abs() < 1e-9);
        // t=1.5H: 0.25
        assert!((l.decayed_weights(108)[0].1 - 0.25).abs() < 1e-9);
        // t=2H: 0
        assert!(l.decayed_weights(144).is_empty());
    }

    #[test]
    fn record_updates_existing() {
        let mut l = RatingList::new(72, 100, StorageMode::Memory);
        l.record(1, 0.5, 0);
        l.record(1, 0.9, 10);
        assert_eq!(l.len(), 1);
        let w = l.decayed_weights(10);
        assert!((w[0].1 - 0.9).abs() < 1e-9);
    }

    #[test]
    fn sample_favors_high_weight() {
        let mut l = RatingList::new(72, 100, StorageMode::Memory);
        l.record(1, 1.0, 0);
        l.record(2, 0.1, 0);
        l.record(3, 0.1, 0);

        let mut rng = Rng::new(42);
        let s = l.sample(10000, 0, &mut rng);
        let w1 = s.iter().find(|(a, _)| *a == 1).map(|(_, w)| *w).unwrap_or(0.0);
        // 账户 1 的权重约 1/1.2 ≈ 0.83
        assert!(w1 > 0.7, "高权重账户应被抽中更多，实际 {}", w1);
    }

    #[test]
    fn merge_weights_normalize() {
        let mut l = RatingList::new(72, 100, StorageMode::Memory);
        l.record(1, 1.0, 0);
        l.record(2, 1.0, 0);
        let m = l.merge_weights(0);
        assert!((m[&1] - 0.5).abs() < 1e-9);
        assert!((m[&2] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn capacity_eviction() {
        let mut l = RatingList::new(72, 3, StorageMode::Memory);
        for i in 0..5 {
            l.record(i, 1.0, i as u64);
        }
        assert_eq!(l.len(), 3);
        assert!(l.evicted > 0);
        // 保留的应该是最新的三个
        let accounts: Vec<usize> = l.decayed_weights(4).iter().map(|(a, _)| *a).collect();
        assert!(accounts.contains(&4));
    }

    #[test]
    fn expired_entries_removed() {
        let mut l = RatingList::new(10, 100, StorageMode::Memory);
        l.record(1, 1.0, 0);
        // 25 小时后（2.5 个半衰期），已过期
        assert!(l.decayed_weights(25).is_empty());
    }
}
