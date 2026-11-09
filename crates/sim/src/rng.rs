//! 确定性伪随机数生成器（xorshift64*）与仿真需要的分布采样。
//!
//! 仿真必须可复现，所以要满足两条：
//!
//! 1. **不依赖系统熵**：种子全由 `SimConfig::seed` 派生，同一份配置跑两遍逐位相同；
//! 2. **不引入外部 crate**：离线环境拉不到依赖，只用 std。
//!
//! 分布采样全部是逆变换法或经典近似，没有查表、没有缓存，因此调用顺序固定时结果固定。

/// xorshift64* 状态。
///
/// 周期 2^64-1，低位质量足够好（乘了一个奇常数做输出混淆），
/// 对仿真这种量级的采样完全够用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// 构造。种子为 0 时换成一个固定常数，避免全零状态退化。
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    /// 由一个主种子和用途标签派生独立的随机流。
    ///
    /// 同一份配置里给不同阶段开独立流，避免「某个阶段多抽一次数」影响另一个阶段。
    pub fn derive(seed: u64, tag: u64) -> Self {
        let mut h = seed ^ 0xA076_1D64_78BD_642F;
        h = h.wrapping_add(tag.wrapping_mul(0xE703_7ED1_A0B4_28DB));
        // 一次 splitmix64 混淆
        h ^= h >> 30;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
        Self::new(h)
    }

    /// 原始 64 位输出。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, 1) 均匀分布，53 位精度。
    pub fn next_f64(&mut self) -> f64 {
        const DENOM: f64 = (1u64 << 53) as f64;
        (self.next_u64() >> 11) as f64 / DENOM
    }

    /// [0, n) 均匀整数。用「拒绝采样」消除取模偏差。
    pub fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let n = n as u64;
        // 可接受区间长度是 n 的整数倍，落在区间外的样本丢弃重抽。
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return (x % n) as usize;
            }
        }
    }

    /// 闭区间 [lo, hi] 的均匀整数。`lo > hi` 时返回 `lo`。
    pub fn range_inclusive(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }

    /// 以概率 p 返回 true。p <= 0 恒 false，p >= 1 恒 true。
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.next_f64() < p
    }

    /// 标准正态分布。Box–Muller 变换。
    pub fn normal(&mut self, mu: f64, sigma: f64) -> f64 {
        // u1 不能取 0（ln(0) = -inf）
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        mu + sigma * z
    }

    /// 对数正态分布。乘性噪声用它：中位数是 exp(mu)，不会出负数。
    pub fn lognormal(&mut self, mu: f64, sigma: f64) -> f64 {
        self.normal(mu, sigma).exp()
    }

    /// 指数分布，均值 1/lambda。
    pub fn exponential(&mut self, lambda: f64) -> f64 {
        if !(lambda > 0.0) {
            return 0.0;
        }
        let u = self.next_f64().max(1e-12);
        -u.ln() / lambda
    }

    /// 帕累托分布（幂律），`alpha` 是尾部指数，`x_min` 是下界。
    ///
    /// 用逆变换：X = x_min * U^(-1/alpha)，其中 U ~ Uniform(0,1]。
    /// 用来造「多数人一般活跃、少数人超级活跃」的长尾。
    pub fn pareto(&mut self, alpha: f64, x_min: f64) -> f64 {
        if !(alpha > 0.0) || !(x_min > 0.0) {
            return x_min.max(0.0);
        }
        // U 取 (0,1]，避免除零
        let u = (1.0 - self.next_f64()).max(1e-12);
        let x = x_min * u.powf(-1.0 / alpha);
        if x.is_finite() {
            x
        } else {
            x_min
        }
    }

    /// 泊松分布。小均值用 Knuth 连乘，大均值用正态近似。
    ///
    /// 用来描述「每小时来了几个人」「这个人这小时互动了几次」。
    pub fn poisson(&mut self, lambda: f64) -> usize {
        if !(lambda > 0.0) || !lambda.is_finite() {
            return 0;
        }
        if lambda < 30.0 {
            let limit = (-lambda).exp();
            let mut k = 0usize;
            let mut p = 1.0f64;
            loop {
                p *= self.next_f64();
                if p <= limit {
                    return k;
                }
                k += 1;
                // 理论上不会走到这里；防御性上限，避免 lambda 计算误差导致死循环
                if k > 4096 {
                    return k;
                }
            }
        } else {
            // 正态近似：加 0.5 做连续性修正
            let z = self.normal(0.0, 1.0);
            let v = lambda + lambda.sqrt() * z + 0.5;
            if v <= 0.0 {
                0
            } else {
                v as usize
            }
        }
    }

    /// 按权重抽一个下标。权重必须是有限非负数，总和为 0 时返回 `None`。
    ///
    /// 权重里出现 NaN / 负数 / inf 时当作 0 处理，保证抽样不会 panic。
    pub fn weighted_index(&mut self, weights: &[f64]) -> Option<usize> {
        let mut total = 0.0f64;
        for &w in weights {
            if w.is_finite() && w > 0.0 {
                total += w;
            }
        }
        if !(total > 0.0) {
            return None;
        }
        let mut r = self.next_f64() * total;
        for (i, &w) in weights.iter().enumerate() {
            if !(w.is_finite() && w > 0.0) {
                continue;
            }
            if r < w {
                return Some(i);
            }
            r -= w;
        }
        // 浮点累加误差兜底：返回最后一个正权重
        weights
            .iter()
            .rposition(|&w| w.is_finite() && w > 0.0)
    }

    /// 按权重不放回抽 k 个下标。
    ///
    /// 简单实现：每次抽中后把该位置权重清零。k 通常远小于权重数组长度（关注数 6~10），
    /// 所以 O(k·n) 可以接受。
    pub fn weighted_without_replacement(&mut self, weights: &[f64], k: usize) -> Vec<usize> {
        let mut w = weights.to_vec();
        let mut out = Vec::with_capacity(k.min(w.len()));
        for _ in 0..k.min(w.len()) {
            match self.weighted_index(&w) {
                Some(i) => {
                    out.push(i);
                    w[i] = 0.0;
                }
                None => break,
            }
        }
        out
    }

    /// 原地洗牌（Fisher–Yates）。
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        if items.len() < 2 {
            return;
        }
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同种子逐位相同() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn 不同种子不同序列() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let same = (0..64).filter(|_| a.next_u64() == b.next_u64()).count();
        assert!(same < 3, "两个种子不应该几乎重合，重合 {same} 次");
    }

    #[test]
    fn 零种子不退化() {
        let mut r = Rng::new(0);
        let xs: Vec<u64> = (0..8).map(|_| r.next_u64()).collect();
        assert!(xs.iter().all(|&x| x != 0), "零种子也必须产出非零序列");
        assert!(xs.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn 浮点落在单位区间() {
        let mut r = Rng::new(7);
        for _ in 0..100_000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x), "越界: {x}");
        }
    }

    #[test]
    fn 整数区间无偏且不越界() {
        let mut r = Rng::new(11);
        let mut counts = [0usize; 7];
        for _ in 0..70_000 {
            let x = r.below(7);
            assert!(x < 7);
            counts[x] += 1;
        }
        // 期望每格 10000，允许 ±20% 波动
        for &c in &counts {
            assert!((8000..12_000).contains(&c), "分布偏差过大: {counts:?}");
        }
    }

    #[test]
    fn 闭区间边界() {
        let mut r = Rng::new(3);
        for _ in 0..1000 {
            assert_eq!(r.range_inclusive(5, 5), 5);
            let x = r.range_inclusive(2, 4);
            assert!((2..=4).contains(&x));
        }
        // 反向区间退化到 lo
        assert_eq!(r.range_inclusive(9, 1), 9);
    }

    #[test]
    fn 概率边界() {
        let mut r = Rng::new(5);
        assert!(!r.chance(0.0));
        assert!(r.chance(1.0));
        assert!(!r.chance(-1.0));
        assert!(r.chance(2.0));
        let hits = (0..10_000).filter(|_| r.chance(0.3)).count();
        assert!((2600..3400).contains(&hits), "0.3 概率下命中 {hits}");
    }

    #[test]
    fn 正态与对数正态矩() {
        let mut r = Rng::new(99);
        let n = 40_000;
        let mut sum = 0.0;
        let mut sum2 = 0.0;
        for _ in 0..n {
            let x = r.normal(0.5, 0.2);
            sum += x;
            sum2 += x * x;
        }
        let mean = sum / n as f64;
        let var = sum2 / n as f64 - mean * mean;
        assert!((mean - 0.5).abs() < 0.01, "均值 {mean}");
        assert!((var - 0.04).abs() < 0.005, "方差 {var}");

        let mut lsum = 0.0f64;
        for _ in 0..20_000 {
            let x = r.lognormal(0.0, 0.25);
            assert!(x > 0.0);
            lsum += x.ln();
        }
        assert!((lsum / 20_000.0).abs() < 0.01, "对数正态中位数应接近 1");
    }

    #[test]
    fn 泊松均值() {
        let mut r = Rng::new(21);
        let n = 20_000;
        let mut sum = 0usize;
        for _ in 0..n {
            sum += r.poisson(0.6);
        }
        let mean = sum as f64 / n as f64;
        assert!((mean - 0.6).abs() < 0.05, "小均值 {mean}");

        let mut big = 0usize;
        for _ in 0..20_000 {
            big += r.poisson(120.0);
        }
        let mean_big = big as f64 / 20_000.0;
        assert!((mean_big - 120.0).abs() < 2.0, "大均值 {mean_big}");

        assert_eq!(r.poisson(0.0), 0);
        assert_eq!(r.poisson(-3.0), 0);
    }

    #[test]
    fn 帕累托长尾且不越界() {
        let mut r = Rng::new(31);
        let mut tail = 0;
        let mut total = 0.0;
        let n = 20_000;
        for _ in 0..n {
            let x = r.pareto(1.5, 1.0);
            assert!(x >= 1.0 - 1e-12, "帕累托下界被破坏: {x}");
            assert!(x.is_finite());
            if x > 10.0 {
                tail += 1;
            }
            total += x;
        }
        // P(X > 10) = 10^-1.5 = 3.16%
        let ratio = tail as f64 / n as f64;
        assert!((0.015..0.06).contains(&ratio), "长尾比例 {ratio}");
        // alpha=1.5 的均值是 3
        assert!((total / n as f64 - 3.0).abs() < 0.6);
    }

    #[test]
    fn 加权抽样尊重零权重() {
        let mut r = Rng::new(77);
        let w = [0.0, 2.0, 0.0, 1.0];
        for _ in 0..2000 {
            let i = r.weighted_index(&w).expect("有正权重");
            assert!(i == 1 || i == 3, "抽到了零权重项 {i}");
        }
        assert_eq!(r.weighted_index(&[0.0, 0.0]), None);
        assert_eq!(r.weighted_index(&[]), None);
        // 非法权重不应 panic
        assert!(r.weighted_index(&[f64::NAN, -1.0, f64::INFINITY]).is_none());
    }

    #[test]
    fn 不放回抽样无重复() {
        let mut r = Rng::new(13);
        let w = [1.0; 20];
        let picked = r.weighted_without_replacement(&w, 6);
        assert_eq!(picked.len(), 6);
        let mut seen = picked.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6, "抽到了重复项: {picked:?}");

        // k 超过样本量时只抽到全部
        let all = r.weighted_without_replacement(&[1.0, 1.0, 1.0], 10);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn 洗牌是排列() {
        let mut r = Rng::new(17);
        let mut v: Vec<usize> = (0..50).collect();
        r.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
        assert_ne!(v, (0..50).collect::<Vec<_>>(), "50 个元素洗牌后不应恰好还原");
    }
}
