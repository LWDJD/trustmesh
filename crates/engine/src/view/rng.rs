//! 极简 xorshift64 伪随机数发生器。
//!
//! 只用于两件事：`noise_floor` 生成全新的 32 字节 key；测试里生成确定性的输入。
//! 不追求密码学强度，也不用于任何安全决策。
//!
//! 选 xorshift64 是因为它只要三行、状态只有 8 字节、在 debug 构建下也足够快，
//! 并且跨平台结果完全一致（纯整数运算）。

/// xorshift64 发生器。
///
/// 序列由种子完全决定：同一种子在同一平台、不同平台上都产生同一串数。
#[derive(Clone, Debug)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// 用给定种子初始化。
    ///
    /// xorshift 的状态不能为 0，否则会永远输出 0；这里把 0 换成
    /// splitmix64 的黄金比例常数，保证任意种子都能正常工作。
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    /// 生成下一个 64 位随机数。
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// 生成一个全新的 32 字节 key：4 个 u64 按小端拼起来。
    #[inline]
    pub fn next_key(&mut self) -> [u8; 32] {
        let mut key = [0u8; 32];
        for chunk in key.chunks_exact_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        key
    }

    /// 当前内部状态（调试用）。
    pub const fn state(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = XorShift64::new(12345);
        let mut b = XorShift64::new(12345);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = XorShift64::new(1);
        let mut b = XorShift64::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn zero_seed_still_produces_noise() {
        let mut rng = XorShift64::new(0);
        let mut zeros = 0;
        for _ in 0..1000 {
            if rng.next_u64() == 0 {
                zeros += 1;
            }
        }
        assert_eq!(zeros, 0, "状态为 0 时发生器退化");
        assert_ne!(rng.state(), 0);
    }

    #[test]
    fn keys_are_not_trivially_repeated() {
        let mut rng = XorShift64::new(7);
        let first = rng.next_key();
        let second = rng.next_key();
        assert_ne!(first, second);
        // 32 字节必须全部填满，而不是只写前 8 字节。
        assert!(first.chunks_exact(8).any(|chunk| chunk != &first[0..8]));
    }
}
