//! 哈希工具：FNV-1a 64 位、splitmix64 终混合、每个 hasher 的独立种子。
//!
//! 为什么不用 `std::collections::hash_map::DefaultHasher`：
//!
//! 1. 它的算法没有稳定性承诺，换一个 Rust 版本结果可能就变；而本 crate 要求
//!    两个节点对同一个 key 算出完全相同的桶，否则查询对不上。
//! 2. 这里需要「每个 hasher 一个独立种子」，并且要对 32 字节定长 key
//!    做两次廉价哈希（一条定桶，一条定符号）。

/// FNV-1a 64 位的初始偏移量（offset basis）。
pub(crate) const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64 位的质数。
pub(crate) const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 符号哈希的盐。和第 j 个 hasher 的种子异或，得到与定桶哈希独立的一条哈希。
pub(crate) const SIGN_SALT: u64 = 0xd6e8_feb8_6659_fd93;

/// splitmix64 使用的黄金比例常数。
pub(crate) const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// 带种子的 FNV-1a。
///
/// 种子直接异或进初始偏移量，等价于换一个起点；同一个 `(key, seed)`
/// 在任何机器、任何 Rust 版本上都得到同一个 u64（纯整数运算，无平台差异）。
#[inline]
pub(crate) fn fnv1a64_seeded(key: &[u8], seed: u64) -> u64 {
    let mut hash = FNV_OFFSET_BASIS ^ seed;
    for &byte in key {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// splitmix64 的终混合函数。
///
/// FNV-1a 的乘法只把进位往高位传播，低位雪崩较弱；而默认桶数 2048 是 2 的幂，
/// `hash % 2048` 只看低 11 位。先过一遍终混合，输出的每一位都与输入充分相关，
/// 取模后的桶分布才均匀（见本模块的分布测试）。
#[inline]
pub(crate) fn fmix64(value: u64) -> u64 {
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// 由主种子派生第 `index` 个 hasher 的独立 64 位种子。
///
/// 先乘黄金比例常数再混合，保证相邻 index 的种子毫不相关；
/// 同一份 `(master, index)` 在所有节点上结果一致。
#[inline]
pub(crate) fn derive_seed(master: u64, index: usize) -> u64 {
    fmix64(master ^ (index as u64).wrapping_add(1).wrapping_mul(GOLDEN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 从整数生成 32 字节 key，避免测试 key 之间只差一两个 bit。
    fn key_from_u64(value: u64) -> [u8; 32] {
        let mut key = [0u8; 32];
        let mut state = value.wrapping_mul(GOLDEN) ^ 0xdead_beef_cafe_f00d;
        for chunk in key.chunks_exact_mut(8) {
            state = fmix64(state);
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        key
    }

    fn bucket_of(key: &[u8; 32], seed: u64, buckets: u64) -> u64 {
        fmix64(fnv1a64_seeded(key, seed)) % buckets
    }

    #[test]
    fn fnv_is_deterministic_and_seed_sensitive() {
        let key = [7u8; 32];
        assert_eq!(fnv1a64_seeded(&key, 1), fnv1a64_seeded(&key, 1));
        assert_ne!(fnv1a64_seeded(&key, 1), fnv1a64_seeded(&key, 2));
        assert_ne!(fnv1a64_seeded(&key, 1), fnv1a64_seeded(&[8u8; 32], 1));
        // 空 key 也要有确定的非零输出。
        assert_ne!(fnv1a64_seeded(&[], 0), 0);
    }

    #[test]
    fn derived_seeds_are_distinct_and_stable() {
        let seeds: HashSet<u64> = (0..64).map(|j| derive_seed(0, j)).collect();
        assert_eq!(seeds.len(), 64, "同一主种子派生的种子出现重复");
        let other: HashSet<u64> = (0..64).map(|j| derive_seed(42, j)).collect();
        assert_eq!(other.len(), 64);
        assert!(seeds.is_disjoint(&other), "不同主种子派生出相同种子");
        // 派生是纯函数：重复调用结果一致。
        assert_eq!(derive_seed(12345, 3), derive_seed(12345, 3));
    }

    #[test]
    fn bucket_distribution_is_uniform() {
        let buckets = 2048u64;
        let samples = 8192u64;
        let seed = derive_seed(0, 0);
        let mut counts = vec![0u32; buckets as usize];
        for i in 0..samples {
            let key = key_from_u64(i);
            let bucket = bucket_of(&key, seed, buckets);
            counts[bucket as usize] += 1;
        }

        let occupied = counts.iter().filter(|&&count| count > 0).count();
        // 8192 个球进 2048 个桶，期望占用 2048*(1-e^-4) ≈ 2005 个桶。
        assert!(occupied > 1900, "占用桶数太少: {occupied}");
        let max = *counts.iter().max().expect("非空");
        assert!(max <= 16, "某个桶过度拥挤: {max}");

        // 卡方检验（自由度 2047，期望每桶 4）。固定输入是确定性的，
        // 阈值 2600 约为均值 2047 的 8.6 个标准差之外，只防灾难性退化。
        let chi2: f64 = counts
            .iter()
            .map(|&count| {
                let diff = f64::from(count) - 4.0;
                diff * diff / 4.0
            })
            .sum();
        assert!(chi2 < 2600.0, "卡方值过大: {chi2}");
    }

    #[test]
    fn sign_is_balanced_and_seed_sensitive() {
        let mut plus = 0usize;
        let mut total = 0usize;
        for hasher in 0..5 {
            let seed = derive_seed(0, hasher);
            for i in 0..2048u64 {
                let key = key_from_u64(i * 5 + hasher as u64);
                let hash = fnv1a64_seeded(&key, seed ^ SIGN_SALT);
                if (hash >> 63) & 1 == 0 {
                    plus += 1;
                }
                total += 1;
            }
        }
        let fraction = plus as f64 / total as f64;
        assert!((0.47..=0.53).contains(&fraction), "符号不平衡: {fraction}");

        // 换一个种子，符号序列必须变；否则各 hasher 的噪声会同向叠加。
        // 两个独立符号位在约一半的 key 上相同，明显偏离 50% 就说明相关。
        let seed_a = derive_seed(0, 0) ^ SIGN_SALT;
        let seed_b = derive_seed(0, 1) ^ SIGN_SALT;
        let samples = 2000u64;
        let mut agree = 0usize;
        for i in 0..samples {
            let key = key_from_u64(i);
            let a = fnv1a64_seeded(&key, seed_a) >> 63;
            let b = fnv1a64_seeded(&key, seed_b) >> 63;
            if a == b {
                agree += 1;
            }
        }
        let agreement = agree as f64 / samples as f64;
        assert!(
            (0.4..=0.6).contains(&agreement),
            "两个 hasher 的符号相关性过高: {agreement}"
        );
    }
}
