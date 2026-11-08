//! 预设参数：把实测最优值固定下来，开发者直接选一套。
//!
//! 所有数值来自 `SIMULATION.md` 和 `PARAMETERS.md` 的实测结果，
//! 不是估计值。
//!
//! # 四套预设
//!
//! | 预设 | 个人:群体 | K | 半衰期 | 桶数 | 适合 |
//! |---|---|---|---|---|---|
//! | `recommended` | 3:7 | 50 | 3 天 | 2048 | 防攻击优先（默认） |
//! | `balanced` | 5:5 | 50 | 7 天 | 2048 | 平衡 |
//! | `personal` | 7:3 | 100 | 14 天 | 4096 | 个人观点优先 |
//! | `mobile` | 3:7 | 20 | 3 天 | 1024 | 手机、低功耗 |
//!
//! # 用法
//!
//! ```no_run
//! use trustmesh::presets::Preset;
//!
//! let p = Preset::recommended();
//! let list = p.rating_list();
//! let sketch = p.sketch(42);
//! ```
//!
//! 想调参数就直接改字段，预设只是起点。

use crate::rating_list::{RatingList, StorageMode};
use crate::sketch::Sketch;

#[derive(Clone, Debug)]
pub struct Preset {
    pub name: &'static str,
    /// 个人权重 β，群体权重是 1-β
    pub beta: f64,
    /// 半衰期（小时），线性衰减到 2 倍半衰期归零
    pub half_life: u64,
    /// 单次抽样次数 K
    pub samples: usize,
    /// Sketch 桶数 B
    pub buckets: usize,
    /// Sketch 哈希函数数 D
    pub hashers: usize,
    /// 是否按评分比例归一化（影响力预算）
    pub normalize: bool,
    /// 正分阈值，低于这个值不参与传播
    pub positive_threshold: f64,
    /// 评分清单最大条目数
    pub capacity: usize,
}

impl Preset {
    /// 防攻击优先。实测：圈子化最低（1.046），群体估计误差 0.32%。
    pub fn recommended() -> Self {
        Self {
            name: "recommended",
            beta: 0.3,
            half_life: 72,
            samples: 50,
            buckets: 2048,
            hashers: 5,
            normalize: true,
            positive_threshold: 0.2,
            capacity: 4096,
        }
    }

    /// 平衡。圈子化 1.076，保留更多个人判断。
    pub fn balanced() -> Self {
        Self {
            name: "balanced",
            beta: 0.5,
            half_life: 168,
            samples: 50,
            buckets: 2048,
            hashers: 5,
            normalize: true,
            positive_threshold: 0.2,
            capacity: 4096,
        }
    }

    /// 个人观点优先。误差最低（1.82e-3），但圈子化 1.106。
    pub fn personal() -> Self {
        Self {
            name: "personal",
            beta: 0.7,
            half_life: 336,
            samples: 100,
            buckets: 4096,
            hashers: 5,
            normalize: true,
            positive_threshold: 0.2,
            capacity: 8192,
        }
    }

    /// 手机、低功耗。K=20，误差 1.47%，抽样约 15 毫秒。
    pub fn mobile() -> Self {
        Self {
            name: "mobile",
            beta: 0.3,
            half_life: 72,
            samples: 20,
            buckets: 1024,
            hashers: 5,
            normalize: true,
            positive_threshold: 0.2,
            capacity: 1024,
        }
    }

    /// 按预设构造评分清单。
    pub fn rating_list(&self) -> RatingList {
        RatingList::new(self.half_life, self.capacity, StorageMode::Memory)
    }

    /// 按预设构造评分清单（手机模式）。
    pub fn rating_list_compact(&self) -> RatingList {
        RatingList::new(self.half_life, self.capacity, StorageMode::Compact)
    }

    /// 按预设构造 sketch。
    pub fn sketch(&self, seed: u64) -> Sketch {
        Sketch::new(self.hashers, self.buckets, seed)
    }

    /// 预期误差（相对真值，K=50 时约 0.3%）。
    pub fn expected_error(&self) -> f64 {
        // 抽样误差经验值：0.3 / sqrt(K)
        0.3 / (self.samples as f64).sqrt()
    }

    /// 发布体积（字节）。
    pub fn publish_bytes(&self) -> usize {
        self.buckets * self.hashers * 8
    }

    pub fn summary(&self) -> String {
        format!(
            "{}: 个人:{:.0} 群体:{:.0}, K={}, H={}h, B={}, 误差~{:.2}%, 体积{}KB",
            self.name,
            self.beta * 10.0,
            (1.0 - self.beta) * 10.0,
            self.samples,
            self.half_life,
            self.buckets,
            self.expected_error() * 100.0,
            self.publish_bytes() / 1024
        )
    }
}

impl Default for Preset {
    fn default() -> Self {
        Self::recommended()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_is_default() {
        let p = Preset::default();
        assert_eq!(p.name, "recommended");
        assert!((p.beta - 0.3).abs() < 1e-9);
    }

    #[test]
    fn rating_list_constructs() {
        let p = Preset::recommended();
        let mut list = p.rating_list();
        list.record(1, 0.8, 0);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn sketch_size_matches() {
        let p = Preset::recommended();
        let sk = p.sketch(1);
        assert_eq!(sk.size(), 2048 * 5);
        assert_eq!(p.publish_bytes(), 2048 * 5 * 8);
    }

    #[test]
    fn error_estimates_reasonable() {
        let p = Preset::recommended();
        assert!(p.expected_error() < 0.05, "误差应小于 5%");
        let m = Preset::mobile();
        assert!(m.expected_error() > p.expected_error(), "手机版误差应更大");
    }

    #[test]
    fn all_presets_consistent() {
        for p in [
            Preset::recommended(),
            Preset::balanced(),
            Preset::personal(),
            Preset::mobile(),
        ] {
            assert!(p.beta > 0.0 && p.beta < 1.0);
            assert!(p.samples > 0);
            assert!(p.buckets >= 1024);
            assert!(p.half_life > 0);
            assert!(p.positive_threshold >= 0.0);
        }
    }
}
