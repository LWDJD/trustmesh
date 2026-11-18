//! u16 定点分数。
//!
//! 为什么是 u16：一条记录 `(u32 账户号, u16 分数)` 恰好 6 字节，定长、无填充、
//! 可以按字节直接比较。为了反女巫这一点**完全够用**：
//!
//! * 一个来源的预算恒为 1，单条分数的量级是 `1/清单长度`；
//! * 要把一条分数压成 0 需要它小于 `0.5/65535 = 7.63e-6`，
//!   也就是「清单里超过 131070 个等权条目」才会开始丢尾巴；
//! * 量化误差是**绝对**的：任一条目 `|v − s| ≤ s·K/131070 + 7.63e-6`
//!   （见 `tests/anti_sybil.rs` 的 3 号用例，K = 条目数）。
//!
//! 而 u16 换来的东西是结构性的：它**表达不出** NaN、∞、负数和大于 1 的量。
//! 畸形输入在校验入口就被夹紧/丢弃，接收端没有「这条脏数据是不是该信」的判断。

use crate::list::Acc;

/// 定点的 1.0：u16 满量程。
pub const Q_ONE: u16 = u16::MAX;

/// 定点的分母（`65535.0`）。
const Q_SCALE: f32 = 65535.0;

/// 把外部分数夹到 `[0, 1]` 并量化成 u16。
///
/// * `NaN` / `-0.0` / 任何 `<= 0` 的值 → `0`（协议里 0 就是「没表态」）；
/// * `+∞` 与一切 `> 1` 的值 → `Q_ONE`（夹紧，而不是溢出）；
/// * 其余四舍五入到最近的可表示值。
///
/// 这是**唯一**的入口校验：过了这一关，分数一定落在 `[0, 1]` 内，
/// 后面所有计算都不需要再夹一次。
#[must_use]
pub fn quantize(score: f32) -> u16 {
    // 语义：NaN / -0.0 / 任何 <= 0 的值 → 0（协议里 0 表示「没表态」）。
    //
    // 这里**必须**显式判 NaN：`!(score > 0.0)` 对 NaN 为真，而朴素的 `score <= 0.0`
    // 对 NaN 为假。若写成后者，NaN 会穿过 `score > 1.0` 的夹紧（NaN > 1.0 也为假），
    // 落到 `(s * Q_SCALE + 0.5) as u32` 上，最终靠 `as` 的饱和转换「碰巧」得到 0——
    // 数值结果虽然相同，但那是实现细节的偶然，不是契约。
    // `is_nan() || <= 0.0` 与原来的 `!(score > 0.0)` 在**全部** f32 位模式上逐值等价
    // （本轮已用 rustc -O 穷举 2^32 个位模式验证，0 处不一致）。
    if score.is_nan() || score <= 0.0 {
        return 0;
    }
    let s = if score > 1.0 { 1.0 } else { score };
    // `+ 0.5` 后截断即四舍五入；`as` 转换是饱和的，1.0 * 65535 + 0.5 落在 u16 上界。
    let q = (s * Q_SCALE + 0.5) as u32;
    q.min(Q_ONE as u32) as u16
}

/// u16 定点还原成 f32：`q / 65535`。
#[must_use]
pub fn dequantize(q: u16) -> f32 {
    q as f32 / Q_SCALE
}

/// 检查一个账户号是否在「可发布」范围内（u32 全量程都合法，这里只保留一个显式入口，
/// 便于以后收紧到「账户号必须连续分配」）。
#[inline]
#[must_use]
pub fn valid_acc(acc: Acc) -> bool {
    // u32 是账户号的定义域，没有非法值；保留这个函数是为了让「校验」显式化。
    let _ = acc;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用位模式表达「恰好这个 f32」，避免十进制字面量与编译器舍入互相扯皮。
    fn fb(bits: u32) -> f32 {
        f32::from_bits(bits)
    }

    /// 入口校验：一切非正输入（含 NaN 与 -0.0）必须落在 0。
    ///
    /// 守的是模块头部「u16 表达不出 NaN/inf/负数」那句话的实现面：
    /// 注意 `!(score > 0.0)` 对 NaN 也为真——这是刻意的「NaN 一律当非法值」语义。
    /// 协议里 0 就是「没表态」，任何 NaN/负数漏进来都会让下游
    ///「分数落在 [0,1]」的推理失效。
    #[test]
    fn quantize_非正输入一律归零() {
        assert_eq!(quantize(0.0), 0);
        assert_eq!(quantize(fb(0x8000_0000)), 0, "-0.0 归零");
        assert_eq!(quantize(-1.0), 0);
        assert_eq!(quantize(-1e-30), 0);
        assert_eq!(quantize(f32::NEG_INFINITY), 0);
        assert_eq!(quantize(f32::NAN), 0);
        assert_eq!(quantize(-f32::NAN), 0, "负 NaN 同样归零");
        assert_eq!(quantize(fb(0x7FC0_0001)), 0, "带载荷的 NaN 归零");
        assert_eq!(quantize(-f32::MIN_POSITIVE), 0);
        assert_eq!(quantize(fb(0x0000_0001)), 0, "最小正次正规仍归零");
    }

    /// 满量程夹紧：一切 >= 1 与 +inf 都落到 Q_ONE，既不溢出也不回绕。
    #[test]
    fn quantize_大于等于一夹到满量程() {
        assert_eq!(Q_ONE, u16::MAX);
        assert_eq!(Q_ONE, 65535);
        for &x in &[
            1.0f32,
            fb(0x3F80_0001),
            1.5,
            2.0,
            1e3,
            1e30,
            f32::MAX,
            f32::INFINITY,
        ] {
            assert_eq!(quantize(x), Q_ONE, "quantize({x:?}) 应夹到满量程");
        }
        // 实现是「先把 s 夹到 1.0 再乘 65535」：乘积上界 65535.5，
        // `as u32` 截断成 65535，再有一层 min 兜底，所以 f32::MAX 也不会溢出。
        assert_eq!(dequantize(quantize(f32::MAX)).to_bits(), 1.0f32.to_bits());
    }

    /// 满量程的下界落在 1.0 的下方：0x3F7FFF80（约 0.9999924）已经是 65535。
    ///
    /// 这是「四舍五入而不是向下取整」的直接后果：0.99999... 只要离 1.0
    /// 不到半格就会被吸到满量程。
    #[test]
    fn quantize_满量程下界落在一下方() {
        let first_max = fb(0x3F7F_FF80);
        assert!(first_max < 1.0);
        assert_eq!(quantize(first_max), Q_ONE);
        assert_eq!(quantize(fb(0x3F7F_FF7F)), 65534);
    }

    /// 丢尾阈值恰好是「半个定点格」：x >= 0.5/65535 才留得下。
    ///
    /// 守的是 fixed.rs 头部那句「要把一条分数压成 0 需要它小于
    /// 0.5/65535 = 7.63e-6」，也就是「清单最多支持 131070 个等权条目」。
    #[test]
    fn quantize_半个定点格是保留阈值() {
        let half_grid = 0.5f32 / 65535.0;
        assert_eq!(half_grid.to_bits(), 0x3700_0080);
        assert_eq!(quantize(half_grid), 1);
        assert_eq!(quantize(fb(0x3700_007F)), 0, "比阈值小 1 个 ulp 就归零");
        // 131070 = 2 x 65535：每条恰好压到半个格，向上取整后仍保留。
        assert_eq!(quantize(1.0 / 131070.0), 1);
        assert_eq!(quantize(1.0 / 131071.0), 0);
    }

    /// 常见分数的定点值：这些就是编码后真正落盘的 u16。
    #[test]
    fn quantize_具体定点值与舍入() {
        assert_eq!(quantize(0.5), 32768);
        assert_eq!(quantize(0.25), 16384);
        assert_eq!(quantize(0.75), 49151);
        assert_eq!(quantize(0.05), 3277);
        assert_eq!(quantize(0.02), 1311);
        assert_eq!(quantize(0.1), 6554);
        assert_eq!(quantize(0.2), 13107);
        assert_eq!(quantize(0.4), 26214);
        assert_eq!(quantize(0.6), 39321);
        assert_eq!(quantize(0.8), 52428);
        assert_eq!(quantize(0.9), 58982);
        assert_eq!(quantize(1.0 / 3.0), 21845);
        assert_eq!(quantize(0.499999), 32767);
        // 0.50001 与 0.5 相差 1e-5 < 半个格，会落在同一个定点上：
        // 「谁进谁出」的截断只认量化后的 q（见 view.rs 的并列测试）。
        assert_eq!(quantize(0.50001), 32768);
    }

    /// 舍入方向：**中点向上**（half-up），既不是银行家舍入也不是朝偶取整。
    ///
    /// 守的是 `(s * Q_SCALE + 0.5) as u32` 这个写法：对每个 k，
    /// `(k + 0.5) / 65535` 正好是 k 与 k+1 的中间点，必须落到 k+1。
    #[test]
    fn quantize_中点一律向上() {
        for k in 0..=65534u32 {
            let mid = (k as f32 + 0.5) / Q_SCALE;
            assert_eq!(quantize(mid), (k + 1) as u16, "k={k} 的中点应向上取整");
        }
    }

    /// 单调非降：x1 <= x2 推出 quantize(x1) <= quantize(x2)。
    ///
    /// 守的是「量化不改变序」：view.rs 的截断排序、q_of 的二分查找、
    /// 以及任何逐位比较都建立在这个全序上。
    /// 三个独立扫描面：定点网格逐格、正 f32 位模式稠密采样、越界区间。
    #[test]
    fn quantize_单调非降() {
        // (a) 定点网格：65536 个格点逐一（这同时覆盖了 dequantize 的全部输出）。
        let mut prev = 0u16;
        for q in 0..=Q_ONE {
            let cur = quantize(dequantize(q));
            assert!(cur >= prev, "q={q} 处出现下降：{prev} -> {cur}");
            prev = cur;
        }
        // (b) 正 f32 位模式稠密采样：每 4093 个位模式取一个（含 0 与 1.0）。
        let mut prev = 0u16;
        let mut bits = 0u32;
        while bits <= 0x3F80_0000 {
            let cur = quantize(fb(bits));
            assert!(cur >= prev, "bits={bits:08x} 处出现下降：{prev} -> {cur}");
            prev = cur;
            bits += 4093;
        }
        // (c) 越过 1.0 之后全是满量程，不会回绕。
        assert_eq!(quantize(1.0), Q_ONE);
        assert_eq!(quantize(fb(0x7F7F_FFFF)), Q_ONE);
    }

    /// dequantize 是 q / 65535：端点与若干位模式逐一钉死。
    #[test]
    fn dequantize_端点与位模式() {
        assert_eq!(dequantize(0).to_bits(), 0x0000_0000);
        assert_eq!(dequantize(1).to_bits(), 0x3780_0080);
        assert_eq!(dequantize(2).to_bits(), 0x3800_0080);
        assert_eq!(dequantize(3).to_bits(), 0x3840_00C0);
        assert_eq!(dequantize(4).to_bits(), 0x3880_0080);
        assert_eq!(dequantize(100).to_bits(), 0x3AC8_00C8);
        assert_eq!(dequantize(1000).to_bits(), 0x3C7A_00FA);
        assert_eq!(dequantize(32767).to_bits(), 0x3EFF_FF00);
        assert_eq!(dequantize(32768).to_bits(), 0x3F00_0080);
        assert_eq!(dequantize(65534).to_bits(), 0x3F7F_FF00);
        assert_eq!(dequantize(Q_ONE).to_bits(), 1.0f32.to_bits());
        assert_eq!(dequantize(Q_ONE), 1.0);

        // 0.5 落在 32767 与 32768 的正中间，向上取整到 32768，
        // 于是还原回来是 0.5000076 而不是 0.5——这是**刻意**的 half-up 副作用。
        assert!(dequantize(32768) > 0.5);

        // 四个「好数」：0.2/0.4/0.6/0.8 量化再还原与原 f32 逐位相同。
        assert_eq!(dequantize(13107).to_bits(), 0.2f32.to_bits());
        assert_eq!(dequantize(26214).to_bits(), 0.4f32.to_bits());
        assert_eq!(dequantize(39321).to_bits(), 0.6f32.to_bits());
        assert_eq!(dequantize(52428).to_bits(), 0.8f32.to_bits());
    }

    /// 严格递增：相邻定点还原后严格变大，绝不出现两个 q 挤到同一个 f32。
    ///
    /// 守的是「分辨率 1/65535」：任意两条不同记录一定能分出大小，
    /// 逐位比较（S6 判据）才有意义。
    #[test]
    fn dequantize_严格递增() {
        let mut prev = dequantize(0);
        for q in 1..=Q_ONE {
            let cur = dequantize(q);
            assert!(cur > prev, "q={q}: {cur:?} 不大于 {prev:?}");
            prev = cur;
        }
    }

    /// 往返恒等：quantize(dequantize(q)) == q，65536 个定点全覆盖。
    ///
    /// 这是 view.rs「编码是规范形式」的根基：decode 不做二次量化，
    /// 「解出来的字节再编回去必须逐字节相同」全靠这条。
    #[test]
    fn quantize_dequantize_往返恒等() {
        for q in 0..=Q_ONE {
            assert_eq!(quantize(dequantize(q)), q, "定点 {q} 往返后变了");
        }
    }

    /// 往返误差上界：|dequantize(quantize(x)) - x| <= 0.5/65535 + 2^-24
    /// = 7.689115593123602e-6（x 落在 (0,1]）。
    ///
    /// 两项来源不同：0.5/65535 是半个定点格（四舍五入的最大偏差），
    /// 2^-24 是还原时再做一次 f32 除法引入的半个 ulp（|v| <= 1）。
    /// 全量程穷举（release 版扫过 (0,1] 的 10.65 亿个 f32）实测最大偏差是
    /// 7.68899917602539e-6，出现在 0x3F008000（约 0.5019531）——本测试把它钉死；
    /// 单元测试里只做稠密采样以控制耗时。
    #[test]
    fn 往返误差不超过半个格加半个ulp() {
        let bound: f64 = 0.5 / 65535.0 + 1.0 / 16_777_216.0;
        assert_eq!(bound.to_bits(), 0x3EE0_2010_0010_0010);
        assert!(bound < 7.69e-6, "上界应紧于 7.69e-6");

        // 精确的最坏点：偏差超过半个格，说明 +2^-24 这一项不能省。
        let worst_x = fb(0x3F00_8000);
        let diff = (f64::from(dequantize(quantize(worst_x))) - f64::from(worst_x)).abs();
        assert_eq!(
            diff.to_bits(),
            0x3EE0_2000_0000_0000,
            "最坏点偏差应为 7.68899917602539e-6"
        );
        assert!(diff > 0.5 / 65535.0);
        assert!(diff <= bound);

        // (a) 十进制细扫 0..=1，步长 1e-6。
        let mut worst = diff;
        for i in 0..=1_000_000u32 {
            let x = i as f32 / 1_000_000.0;
            let d = (f64::from(dequantize(quantize(x))) - f64::from(x)).abs();
            if d > worst {
                worst = d;
            }
        }
        assert!(
            worst <= bound,
            "十进制细扫最大偏差 {worst:?} 超过上界 {bound:?}"
        );

        // (b) 正 f32 位模式稠密采样：每 4093 个位模式取一个。
        let mut worst_bits = diff;
        let mut bits = 1u32;
        while bits <= 0x3F80_0000 {
            let x = fb(bits);
            let d = (f64::from(dequantize(quantize(x))) - f64::from(x)).abs();
            if d > worst_bits {
                worst_bits = d;
            }
            bits += 4093;
        }
        assert!(
            worst_bits <= bound,
            "位模式采样最大偏差 {worst_bits:?} 超过上界"
        );
    }

    /// dequantize 相对精确有理值 q/65535 的误差不超过半个 f32 ulp（2^-24）。
    ///
    /// 守的是「还原只多一次舍入」这句话：q/65535 的实数除法在 f32 里
    /// 被正确舍入，最大偏差就是 2^-24 = 5.9604645e-8。
    /// 实测最坏点 q=65407（偏差 2.9801867640344426e-8），钉死。
    #[test]
    fn dequantize_误差不超过半个ulp() {
        let mut worst = 0.0f64;
        let mut arg = 0u16;
        for q in 0..=Q_ONE {
            let err = (f64::from(dequantize(q)) - f64::from(q) / 65535.0).abs();
            if err > worst {
                worst = err;
                arg = q;
            }
        }
        assert_eq!(arg, 65407, "最坏点变了");
        assert_eq!(worst.to_bits(), 0x3E5F_FFE0_0000_0000);
        assert!(worst <= 1.0 / 16_777_216.0);
    }

    /// valid_acc 对 u32 全量程恒真：账户号的定义域就是 u32，没有非法值。
    ///
    /// 这个函数当前只是把「校验」显式化，没有任何收紧；测试守的是
    /// 「别哪天偷偷拒绝某个账户号」。
    #[test]
    fn valid_acc_全量程合法() {
        for &acc in &[0u32, 1, 2, 42, 0x00FF_FFFF, u32::MAX - 1, u32::MAX] {
            assert!(valid_acc(acc), "账户号 {acc} 应被判为合法");
        }
    }
}
