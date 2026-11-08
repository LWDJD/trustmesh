//! 确定性伪随机数生成器。
//!
//! 用 xorshift64*，周期 2^64-1，分布质量足够做模拟。
//! 零外部依赖，固定种子即可复现整个实验。

#[derive(Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // 种子不能为 0
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// 返回 [0,1) 区间的浮点数
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 返回 [0,n) 区间的整数
    pub fn next_usize(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// 以概率 p 返回 true
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn range_ok() {
        let mut r = Rng::new(7);
        for _ in 0..10000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x));
        }
    }
}
