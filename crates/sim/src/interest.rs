//! 兴趣向量：8 维、非负、L2 归一化。
//!
//! 兴趣决定两件事：
//!
//! 1. **关注谁**：候选人的权重按余弦相似度加权（同质性越强，指数越大）；
//! 2. **评分多少**：亲密兴趣匹配度参与评分公式（`quality × match × noise`）。
//!
//! 用 L2 归一化的非负向量，是为了让余弦相似度天然落在 `[0,1]`，
//! 这样「兴趣匹配」可以直接当作乘性因子，不需要额外平移。

use crate::rng::Rng;

/// 兴趣向量。
///
/// 不变式：所有分量 ≥ 0，L2 范数 = 1（全零向量除外，见 `from_values`）。
#[derive(Clone, Debug, PartialEq)]
pub struct Interest {
    v: Vec<f32>,
}

impl Interest {
    /// 均匀兴趣：每个维度 `1/sqrt(dims)`。用于特例（没有社团中心时）。
    pub fn uniform(dims: usize) -> Self {
        let dims = dims.max(1);
        let c = 1.0 / (dims as f32).sqrt();
        Self { v: vec![c; dims] }
    }

    /// 从任意取值构造：负数截断成 0，再做 L2 归一化。
    ///
    /// 全零输入（比如所有分量都 ≤ 0）退化为「第一个维度为 1」，保证范数为 1。
    pub fn from_values(values: &[f32]) -> Self {
        let dims = values.len().max(1);
        let mut v: Vec<f32> = if values.is_empty() {
            vec![0.0; dims]
        } else {
            values.iter().map(|&x| if x > 0.0 { x } else { 0.0 }).collect()
        };
        let norm = v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>().sqrt() as f32;
        if norm > 0.0 && norm.is_finite() {
            for x in v.iter_mut() {
                *x /= norm;
            }
        } else {
            v = vec![0.0; dims];
            v[0] = 1.0;
        }
        Self { v }
    }

    /// 随机兴趣：每个维度抽 `Exp(1)`，再归一化。
    ///
    /// 等价于 Dirichlet(1,…,1) 方向的均匀分布，比「每维均匀再归一化」更均匀地覆盖单纯形。
    pub fn random(dims: usize, rng: &mut Rng) -> Self {
        let dims = dims.max(1);
        let mut v = Vec::with_capacity(dims);
        for _ in 0..dims {
            v.push(rng.exponential(1.0) as f32);
        }
        Self::from_values(&v)
    }

    /// 混合兴趣：`(1-rho) × 随机 + rho × 社团中心`，再归一化。
    ///
    /// `rho` 是「兴趣与社会圈子的相关度」：
    /// - rho = 0：兴趣与社团完全无关（社团只是社交结构）
    /// - rho = 1：同一个社团的人兴趣完全一致（社团就是同好会）
    ///
    /// 现实介于两者之间，默认 0.45。
    pub fn mixed(center: &Interest, rho: f32, dims: usize, rng: &mut Rng) -> Self {
        let rho = rho.clamp(0.0, 1.0) as f64;
        let base = Interest::random(dims, rng);
        let dims = base.v.len().min(center.v.len()).max(1);
        let mut v = Vec::with_capacity(dims);
        for d in 0..dims {
            let b = base.v.get(d).copied().unwrap_or(0.0) as f64;
            let c = center.v.get(d).copied().unwrap_or(0.0) as f64;
            v.push(((1.0 - rho) * b + rho * c) as f32);
        }
        Self::from_values(&v)
    }

    /// 维度数。
    pub fn dims(&self) -> usize {
        self.v.len()
    }

    /// 分量切片。
    pub fn values(&self) -> &[f32] {
        &self.v
    }

    /// L2 范数（正常应为 1）。
    pub fn norm(&self) -> f32 {
        self.v.iter().map(|&x| x * x).sum::<f32>().sqrt()
    }

    /// 点积。
    pub fn dot(&self, other: &Interest) -> f32 {
        let dims = self.v.len().min(other.v.len());
        let mut acc = 0.0f64;
        for d in 0..dims {
            acc += self.v[d] as f64 * other.v[d] as f64;
        }
        acc as f32
    }

    /// 余弦相似度，落在 `[0,1]`（非负向量）。任一范数为 0 时返回 0。
    pub fn cosine(&self, other: &Interest) -> f32 {
        let na = self.norm() as f64;
        let nb = other.norm() as f64;
        if !(na > 0.0) || !(nb > 0.0) {
            return 0.0;
        }
        let c = self.dot(other) as f64 / (na * nb);
        c.clamp(0.0, 1.0) as f32
    }

    /// 兴趣匹配度：`0.5 × (1 + cos)`，落在 `[0,1]`。
    ///
    /// 直接用余弦当乘性因子会太狠（随机两个人约 0.2~0.4，评分会被压到很低），
    /// 平移一半后随机交集的期望在 0.6 左右，符合「大多数内容勉强可看」的直觉。
    pub fn match_score(&self, other: &Interest) -> f32 {
        (0.5 * (1.0 + self.cosine(other))).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 归一化与边界() {
        let i = Interest::from_values(&[3.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!((i.norm() - 1.0).abs() < 1e-6);
        assert!((i.values()[0] - 0.6).abs() < 1e-6);
        assert!((i.values()[1] - 0.8).abs() < 1e-6);

        // 全零输入退化成单位向量，不能产生 NaN
        let z = Interest::from_values(&[0.0, 0.0, 0.0]);
        assert!((z.norm() - 1.0).abs() < 1e-6);
        assert_eq!(z.dims(), 3);

        // 负值被截断
        let n = Interest::from_values(&[-1.0, 2.0, 0.0]);
        assert_eq!(n.values()[0], 0.0);
        assert!((n.norm() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn 余弦取值范围与对称性() {
        let mut rng = Rng::new(8);
        for _ in 0..200 {
            let a = Interest::random(8, &mut rng);
            let b = Interest::random(8, &mut rng);
            let c = a.cosine(&b);
            assert!((0.0..=1.0).contains(&c), "余弦越界: {c}");
            assert!((c - b.cosine(&a)).abs() < 1e-6, "余弦必须对称");
            assert!((a.cosine(&a) - 1.0).abs() < 1e-5, "自相似必须是 1");
            let m = a.match_score(&b);
            assert!((0.0..=1.0).contains(&m));
            assert!((m - 0.5 * (1.0 + c)).abs() < 1e-6);
        }
    }

    #[test]
    fn 正交与零向量() {
        let a = Interest::from_values(&[1.0, 0.0]);
        let b = Interest::from_values(&[0.0, 1.0]);
        assert!(a.cosine(&b).abs() < 1e-6);
        assert!((a.match_score(&b) - 0.5).abs() < 1e-6);

        // 手工构造全零（绕过归一化）
        let zero = Interest { v: vec![0.0, 0.0] };
        assert_eq!(zero.cosine(&a), 0.0);
        assert_eq!(zero.match_score(&a), 0.5);
    }

    #[test]
    fn 混合兴趣靠近社团中心() {
        let mut rng = Rng::new(123);
        let center = Interest::random(8, &mut rng);
        let mut to_center = 0.0f32;
        let mut to_random = 0.0f32;
        for _ in 0..300 {
            let m = Interest::mixed(&center, 0.9, 8, &mut rng);
            to_center += m.cosine(&center);
            let r = Interest::random(8, &mut rng);
            to_random += m.cosine(&r);
        }
        assert!(to_center > to_random + 10.0, "rho=0.9 的向量应明显靠近中心");
    }

    #[test]
    fn 维度不匹配也不崩() {
        let a = Interest::from_values(&[1.0, 1.0, 0.0]);
        let b = Interest::from_values(&[1.0, 0.0]);
        let c = a.cosine(&b);
        assert!((0.0..=1.0).contains(&c));
        assert!(a.match_score(&b).is_finite());
    }
}
