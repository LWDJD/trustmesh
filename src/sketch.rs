//! 恒定大小的可合并摘要（Count Sketch）。
//!
//! # 为什么需要
//!
//! 发布「对所有人的评分向量」长度是 n，随网络增长。
//! 要恒定长度，必须压缩。压缩后还要能查询任意账户，还要能合并。
//!
//! 满足这三个条件的结构是 Count Sketch：
//! - 大小 d×k，与 n 无关
//! - 合并是逐元素相加，线性
//! - 查询是取 d 个桶的中位数，误差约 1/√k
//!
//! # 原理
//!
//! 每个账户 u 通过 d 个哈希函数映射到 d 个桶，每个桶带一个 ±1 的符号。
//! 加入值 x 时，第 j 个桶加 `s_j(u)·x`。
//! 查询时，取 `s_j(u)·C[j][h_j(u)]` 的中位数。
//!
//! 碰撞产生的是加性噪声，中位数能抗住少数几个异常桶。
//!
//! # 模糊性
//!
//! 模糊来自碰撞：多个账户落在同一个桶，值相加，无法分离。
//! 这是信息损失，但保持线性，所以可以合并。

/// splitmix64，确定性哈希，用于生成桶索引和符号。
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

#[derive(Clone, Debug)]
pub struct Sketch {
    d: usize,
    k: usize,
    /// d 个哈希种子，所有参与者必须用同一套，否则查询对不上
    seeds: Vec<u64>,
    /// d × k 的桶
    data: Vec<f64>,
}

impl Sketch {
    /// d 个哈希函数，k 个桶。
    ///
    /// 固定 d=5, k=1024 时，大小 5120 个 f64 = 40KB。
    pub fn new(d: usize, k: usize, seed: u64) -> Self {
        assert!(d > 0 && d <= 16);
        assert!(k > 0);
        let seeds: Vec<u64> = (0..d)
            .map(|j| splitmix64(seed.wrapping_add(j as u64)))
            .collect();
        Self {
            d,
            k,
            seeds,
            data: vec![0.0; d * k],
        }
    }

    pub fn size(&self) -> usize {
        self.d * self.k
    }

    pub fn bytes(&self) -> usize {
        self.size() * std::mem::size_of::<f64>()
    }

    /// 桶索引。所有参与者用同样的 (seeds, k)，结果一致。
    fn bucket(&self, j: usize, u: usize) -> usize {
        (splitmix64((u as u64) ^ self.seeds[j]) % self.k as u64) as usize
    }

    /// 符号，±1。
    fn sign(&self, j: usize, u: usize) -> f64 {
        if splitmix64((u as u64) ^ self.seeds[j] ^ 0xABCDEF) & 1 == 0 {
            1.0
        } else {
            -1.0
        }
    }

    /// 加入单点。
    pub fn add(&mut self, u: usize, value: f64) {
        for j in 0..self.d {
            let b = self.bucket(j, u);
            self.data[j * self.k + b] += self.sign(j, u) * value;
        }
    }

    /// 加入整行评分，带权重。
    ///
    /// 这是「个人评分归一化后压缩」的入口。
    pub fn add_row(&mut self, row: &[f64], weight: f64) {
        for (u, &v) in row.iter().enumerate() {
            if v != 0.0 {
                self.add(u, v * weight);
            }
        }
    }

    /// 加入整行，只存「偏离中性值的部分」。
    ///
    /// 这是稀疏场景的正确用法。中性值不占桶，查询时再加回去。
    /// 如果不这样做，一千万个 0.5 会把总质量抬高几个数量级，误差随之爆炸。
    pub fn add_row_sparse(&mut self, row: &[f64], neutral: f64, weight: f64) {
        for (u, &v) in row.iter().enumerate() {
            let delta = v - neutral;
            if delta.abs() > 1e-9 {
                self.add(u, delta * weight);
            }
        }
    }

    /// 查询并加回中性值。
    pub fn query_with_baseline(&self, u: usize, neutral: f64) -> f64 {
        neutral + self.query(u)
    }

    /// 查询账户 u 的估计值。
    pub fn query(&self, u: usize) -> f64 {
        let mut vals = Vec::with_capacity(self.d);
        for j in 0..self.d {
            let b = self.bucket(j, u);
            vals.push(self.sign(j, u) * self.data[j * self.k + b]);
        }
        median(&mut vals)
    }

    /// 合并两份 sketch，加权。
    ///
    /// 这是「个人 + 群体」的加权入口。
    pub fn merge(&self, other: &Sketch, wa: f64, wb: f64) -> Sketch {
        assert_eq!(self.d, other.d);
        assert_eq!(self.k, other.k);
        assert_eq!(self.seeds, other.seeds, "合并要求哈希种子一致");
        let mut out = self.clone();
        for (i, x) in out.data.iter_mut().enumerate() {
            *x = self.data[i] * wa + other.data[i] * wb;
        }
        out
    }

    /// 复制并缩放。
    pub fn scaled(&self, factor: f64) -> Sketch {
        let mut out = self.clone();
        for x in out.data.iter_mut() {
            *x *= factor;
        }
        out
    }

    /// 导出原始桶数据（用于测试和序列化）。
    pub fn raw(&self) -> &[f64] {
        &self.data
    }

    pub fn params(&self) -> (usize, usize) {
        (self.d, self.k)
    }
}

fn median(vals: &mut [f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        (vals[n / 2 - 1] + vals[n / 2]) / 2.0
    }
}

// ============================================================
// 发布：个人评分 + 群体抽样 的加权
// ============================================================

/// 个人评分归一化。
///
/// 输入是各类行为的计数，输出 0 到 1 的分数。
/// 权重由应用层定，这里只用一套默认值。
pub struct BehaviorCounts {
    pub follow: f64,
    pub like: f64,
    pub reply: f64,
    pub repost: f64,
    pub block: f64,
}

impl BehaviorCounts {
    /// 默认权重：转发 > 回复 > 点赞 > 关注，屏蔽为负。
    pub fn normalize(&self) -> f64 {
        let positive =
            self.follow * 0.2 + self.like * 0.1 + self.reply * 0.3 + self.repost * 0.4;
        let negative = self.block * 1.0;
        let raw = positive - negative;
        // 用 tanh 压到 [0,1]，避免长尾
        (0.5 + 0.5 * (raw / 3.0).tanh()).clamp(0.0, 1.0)
    }
}

/// 一次发布的构造器。
///
/// 公式：
///
/// ```text
/// published = α · personal + (1-α) · group
/// ```
///
/// 其中 `personal` 是「个人评分归一化后的压缩」，
/// `group` 是「从别人那里收集的抽样结果压缩」。
pub fn blend(personal: &Sketch, group: &Sketch, alpha: f64) -> Sketch {
    assert!((0.0..=1.0).contains(&alpha));
    personal.merge(group, alpha, 1.0 - alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_size() {
        let a = Sketch::new(5, 1024, 1);
        let b = Sketch::new(5, 1024, 1);
        assert_eq!(a.size(), 5120);
        assert_eq!(a.size(), b.size());
        // 与数据规模无关
        let mut c = a.clone();
        c.add_row(&vec![0.5; 1_000_000], 1.0);
        assert_eq!(c.size(), 5120);
    }

    #[test]
    fn exact_when_k_large() {
        let mut sk = Sketch::new(7, 100_000, 2);
        sk.add(42, 0.8);
        assert!((sk.query(42) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn merge_is_linear() {
        let mut a = Sketch::new(5, 1024, 3);
        let mut b = Sketch::new(5, 1024, 3);
        a.add(1, 0.6);
        b.add(1, 0.4);
        let m = a.merge(&b, 0.5, 0.5);
        assert!((m.query(1) - 0.5).abs() < 1e-9);
    }
}
