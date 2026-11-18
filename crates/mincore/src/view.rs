//! 定长稀疏表：观点的最小可用表示。
//!
//! # 形状
//!
//! `cap` 个槽位，每槽 `(u32 账户号, u16 定点分数)` = **6 字节**，
//! 加上尾部 2 字节条数，一份视图恒为 `cap × 6 + 2` 字节：
//!
//! | cap | 字节数 |
//! |---|---|
//! | 128 | 770 |
//! | 256 | 1538 |
//! | 512 | 3074 |
//!
//! 现行实现的 Count Sketch 是 `2048 × 5 × 4 = 40960` 字节，与账户数无关；
//! 稀疏表则是**与 cap 有关、与实际条目数无关**。cap = 512 时体积是它的 7.5%，
//! 换来的是：误差 0、总质量可精确计算、查询无碰撞。
//!
//! # 编码是规范形式
//!
//! * 槽位按账户号**严格升序**排列（同一账户出现两次 = 非法负载，直接拒绝）；
//! * 分数为 0 的条目不占槽位（0 就是「没表态」）；
//! * 条数之后的填充区必须是全零（禁止夹带）。
//!
//! 三条合起来让「同一份观点」只有**唯一**的字节串，于是
//! `encode(语义相同的视图) == encode(...)` 恒成立——这是确定性判据的基础。
//!
//! # 总质量
//!
//! `total_q = Σ 槽位分数`（u32 累加，**精确**，没有任何估计）。
//! 归一化时 `v = q / total_q`，于是 `Σv ≡ 1`（在声明集合上），
//! 且 `v ≤ 1` 由 `q ≤ 65535` 自动成立——**没有「除以总质量后再夹紧」这一步**，
//! 也就没有「一个来源的预算从 1 变 K」的饱和路径。

use crate::fixed::{quantize, Q_ONE};
use crate::list::Acc;

/// 一个槽位的字节数：u32 账户号 + u16 定点分数。
pub const SLOT_BYTES: usize = 6;

/// 定长区之外的额外字节：2 字节条数。
pub const VIEW_OVERHEAD: usize = 2;

/// 允许的最大 `cap`。再大就没有理由用稀疏表了（该分片了）。
pub const MAX_CAP: usize = 4096;

/// 定长稀疏表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SparseView {
    cap: usize,
    /// 按账户号**严格升序**，分数 `q > 0`。
    slots: Vec<(Acc, u16)>,
    /// Σ 分数（u32 精确累加）。
    total_q: u32,
}

impl SparseView {
    /// 建一张空的定长表。
    ///
    /// # Panics
    ///
    /// `cap` 为 0 或超过 [MAX_CAP] 时 panic：这是配置错误，不是运行时输入。
    #[must_use]
    pub fn new(cap: usize) -> Self {
        assert!(
            cap > 0 && cap <= MAX_CAP,
            "cap 必须在 [1, {MAX_CAP}] 内，收到 {cap}"
        );
        Self {
            cap,
            slots: Vec::new(),
            total_q: 0,
        }
    }

    /// 从一批 `(账户, 分数)` 构造。
    ///
    /// * 分数先经 [quantize] 夹到 `[0, 1]` 并四舍五入——NaN / ∞ / 负数 /
    ///   超大值**在这一步就被消化**，不可能进入表里；
    /// * 量化为 0 的条目丢弃（没表态）；
    /// * 同一账户出现多次时**取最大值**（可交换、可结合，因此与输入顺序无关，
    ///   且不会像「求和」那样把一条重复声明变成双倍预算）；
    /// * 条目数超过 `cap` 时按 `(分数降序, 账户号升序)` 保留前 `cap` 条，
    ///   再按账户号升序落表。截断策略与信息损失见 [Self::truncated_mass]。
    #[must_use]
    pub fn from_scores(cap: usize, pairs: &[(Acc, f32)]) -> Self {
        let mut view = Self::new(cap);
        let mut merged: std::collections::BTreeMap<Acc, u16> = std::collections::BTreeMap::new();
        for &(acc, score) in pairs {
            let q = quantize(score);
            if q == 0 {
                continue;
            }
            merged
                .entry(acc)
                .and_modify(|cur| {
                    if q > *cur {
                        *cur = q;
                    }
                })
                .or_insert(q);
        }
        let mut items: Vec<(Acc, u16)> = merged.into_iter().collect();
        if items.len() > cap {
            // 先按「分数高的优先」挑，分数相同按账户号小的优先——全序，无歧义。
            items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            items.truncate(cap);
            items.sort_unstable_by_key(|x| x.0);
        }
        view.slots = items;
        view.total_q = view.slots.iter().map(|&(_, q)| u32::from(q)).sum();
        view
    }

    /// 直接由有序槽位构造（内部用；会校验升序与 `q > 0`）。
    fn from_slots_unchecked(cap: usize, slots: Vec<(Acc, u16)>) -> Self {
        let total_q = slots.iter().map(|&(_, q)| u32::from(q)).sum();
        Self {
            cap,
            slots,
            total_q,
        }
    }

    /// 容量。
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// 实际条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 是否为空表。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Σ 分数（精确整数，u32）。
    #[must_use]
    pub fn total_q(&self) -> u32 {
        self.total_q
    }

    /// 总质量 `Σq / 65535`（f32，仅用于展示；归一化一律走整数）。
    #[must_use]
    pub fn total_mass(&self) -> f32 {
        self.total_q as f32 / Q_ONE as f32
    }

    /// 槽位视图（按账户号升序）。
    #[must_use]
    pub fn slots(&self) -> &[(Acc, u16)] {
        &self.slots
    }

    /// 某个账户的定点分数；不在表里返回 0。
    #[must_use]
    pub fn q_of(&self, acc: Acc) -> u16 {
        match self.slots.binary_search_by_key(&acc, |x| x.0) {
            Ok(i) => self.slots[i].1,
            Err(_) => 0,
        }
    }

    /// 某个账户的**归一化**观点 `q / Σq`；不在表里或空表返回 0。
    ///
    /// 这是接收端唯一使用的读数：每条先由 u16 保证合法，再除以声明集合自己的
    /// 总质量，所以 `Σ ≡ 1`、单条 `≤ 1`。
    #[must_use]
    pub fn normalized(&self, acc: Acc) -> f32 {
        if self.total_q == 0 {
            return 0.0;
        }
        (f64::from(self.q_of(acc)) / f64::from(self.total_q)) as f32
    }

    /// 归一化后的全部槽位 `(账户, v)`，`Σv ≡ 1`（空表返回空）。
    #[must_use]
    pub fn normalized_slots(&self) -> Vec<(Acc, f32)> {
        if self.total_q == 0 {
            return Vec::new();
        }
        let t = self.total_q as f32;
        self.slots.iter().map(|&(a, q)| (a, q as f32 / t)).collect()
    }

    /// 被 [Self::from_scores] 截断掉的质量占比（`1 − 保留质量 / 全部质量`）。
    ///
    /// 需要「全部质量」是构造时就该知道的，所以这里只提供一个静态估算口径：
    /// 给定未截断的完整 `(账户, 分数)` 列表，返回 `1 − 保留的定点质量 / 全部定点质量`。
    #[must_use]
    pub fn truncated_mass(cap: usize, pairs: &[(Acc, f32)]) -> f64 {
        let mut merged: std::collections::BTreeMap<Acc, u16> = std::collections::BTreeMap::new();
        for &(acc, score) in pairs {
            let q = quantize(score);
            if q == 0 {
                continue;
            }
            merged
                .entry(acc)
                .and_modify(|cur| {
                    if q > *cur {
                        *cur = q;
                    }
                })
                .or_insert(q);
        }
        let all: u64 = merged.values().map(|&q| u64::from(q)).sum();
        if all == 0 {
            return 0.0;
        }
        let mut items: Vec<(Acc, u16)> = merged.into_iter().collect();
        if items.len() > cap {
            items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            items.truncate(cap);
        }
        let kept: u64 = items.iter().map(|&(_, q)| u64::from(q)).sum();
        1.0 - kept as f64 / all as f64
    }

    /// 序列化成**恰好** `cap × 6 + 2` 字节。
    ///
    /// 布局：`[0, cap*6)` 是槽位区（前 `len` 条有效，其后补零），
    /// 最后 2 字节是条数（小端 u16）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let fixed = self.cap * SLOT_BYTES;
        let mut out = vec![0u8; fixed + VIEW_OVERHEAD];
        for (i, &(acc, q)) in self.slots.iter().enumerate() {
            let off = i * SLOT_BYTES;
            out[off..off + 4].copy_from_slice(&acc.to_le_bytes());
            out[off + 4..off + 6].copy_from_slice(&q.to_le_bytes());
        }
        let n = self.slots.len() as u16;
        out[fixed..fixed + 2].copy_from_slice(&n.to_le_bytes());
        out
    }

    /// 反序列化。**任何**不合规范形式或不合容量的负载都返回 `None`。
    #[must_use]
    pub fn decode(bytes: &[u8], cap: usize) -> Option<Self> {
        if cap == 0 || cap > MAX_CAP {
            return None;
        }
        let fixed = cap.checked_mul(SLOT_BYTES)?;
        if bytes.len() != fixed.checked_add(VIEW_OVERHEAD)? {
            return None;
        }
        let count = u16::from_le_bytes([bytes[fixed], bytes[fixed + 1]]) as usize;
        if count > cap {
            return None;
        }
        let mut slots = Vec::with_capacity(count);
        let mut prev: Option<Acc> = None;
        for i in 0..count {
            let off = i * SLOT_BYTES;
            let acc =
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            let q = u16::from_le_bytes([bytes[off + 4], bytes[off + 5]]);
            if q == 0 {
                return None; // 规范形式：0 分不占槽位
            }
            if let Some(p) = prev {
                if acc <= p {
                    return None; // 重复或乱序
                }
            }
            prev = Some(acc);
            slots.push((acc, q));
        }
        // 填充区必须全零：禁止在定长区里夹带任何东西。
        if bytes[count * SLOT_BYTES..fixed].iter().any(|&b| b != 0) {
            return None;
        }
        Some(Self::from_slots_unchecked(cap, slots))
    }

    /// 编码后的字节数（恒定）。
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.cap * SLOT_BYTES + VIEW_OVERHEAD
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::{dequantize, quantize, Q_ONE};

    /// 归一化求和的容差。正是 lib.rs 与 tests/anti_sybil.rs 里用的 1e-6。
    const SUM_TOL: f64 = 1e-6;

    /// 确定性伪随机（xorshift64）：本仓零外部依赖，不用 rand crate。
    /// 同一个种子给同一条序列，测试不会 flaky。
    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_u32(&mut self) -> u32 {
            (self.next_u64() >> 32) as u32
        }

        /// 落在 [lo, hi] 的闭区间随机数。
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            debug_assert!(lo <= hi);
            lo + self.next_u32() % (hi - lo + 1)
        }
    }

    /// 用位模式表达「恰好这个 f32」，避免十进制字面量与编译器舍入互相扯皮。
    fn fb(bits: u32) -> f32 {
        f32::from_bits(bits)
    }

    // ═══════════════════ A. 构造与形状 ═══════════════════

    /// 空表形态：len == 0、is_empty、total_q == 0、任何查询都返回精确 0。
    ///
    /// 守的是「不编造」：空表既不是 NaN、也不是 0/0，而是一组精确的 0 读数。
    #[test]
    fn new_空表形态() {
        let v = SparseView::new(128);
        assert_eq!(v.cap(), 128);
        assert_eq!(v.len(), 0);
        assert!(v.is_empty());
        assert_eq!(v.total_q(), 0);
        assert_eq!(v.total_mass().to_bits(), 0.0f32.to_bits());
        assert!(v.slots().is_empty());
        assert!(v.normalized_slots().is_empty());
        // 任何账户号都读不到东西，两个入口都是**精确**的 0.0。
        for acc in [0u32, 1, 128, 4096, u32::MAX] {
            assert_eq!(v.q_of(acc), 0, "空表不该给账户 {acc} 任何分数");
            assert_eq!(
                v.normalized(acc).to_bits(),
                0.0f32.to_bits(),
                "空表的 normalized({acc}) 应为精确 0.0"
            );
        }
    }

    /// 定长：bytes() == cap×6+2，与条目数无关；encode 恰好这么长。
    ///
    /// 守的是视图体积公式（cap=128/256/512 分别 770/1538/3074），
    /// 这是「定长稀疏表 vs Count Sketch 40960 字节」那条对比的分母。
    #[test]
    fn new_字节数随cap线性() {
        assert_eq!(SLOT_BYTES, 6);
        assert_eq!(VIEW_OVERHEAD, 2);
        assert_eq!(MAX_CAP, 4096);
        for &(cap, want) in &[
            (1usize, 8usize),
            (2, 14),
            (128, 770),
            (256, 1538),
            (512, 3074),
            (4095, 24572),
            (4096, 24578),
        ] {
            let v = SparseView::new(cap);
            assert_eq!(v.bytes(), want, "cap={cap} 的定长字节数不对");
            assert_eq!(
                v.encode().len(),
                want,
                "cap={cap} 的编码长度与 bytes() 不一致"
            );
            assert_eq!(v.bytes(), cap * SLOT_BYTES + VIEW_OVERHEAD);
        }
    }

    /// 非法配置必须 panic：cap == 0。
    ///
    /// 0 槽位的表没有意义（编码后只剩 2 字节条数），也违背「cap 是配置」的前提。
    #[test]
    #[should_panic(expected = "cap 必须在 [1, 4096] 内，收到 0")]
    fn new_cap为零必须panic() {
        let _ = SparseView::new(0);
    }

    /// 非法配置必须 panic：cap > MAX_CAP。
    ///
    /// 再大就没有理由用稀疏表了（该分片了），上限是显式的配置约束。
    #[test]
    #[should_panic(expected = "cap 必须在 [1, 4096] 内，收到 4097")]
    fn new_cap超上限必须panic() {
        let _ = SparseView::new(MAX_CAP + 1);
    }

    /// cap == 1 是最小的合法表：只能留一条，且处处自洽。
    #[test]
    fn cap边界_cap为一() {
        let empty = SparseView::new(1);
        assert_eq!(empty.bytes(), 8);
        assert_eq!(empty.encode(), vec![0u8; 8]);
        assert_eq!(
            SparseView::decode(&empty.encode(), 1).map(|v| (v.cap(), v.len())),
            Some((1, 0))
        );

        // 两个条目争一个槽：分数高的（9 号，0.9）胜出，3 号被截掉。
        let v = SparseView::from_scores(1, &[(3, 0.5), (9, 0.9)]);
        assert_eq!(v.len(), 1, "cap=1 只能有 1 条");
        assert_eq!(v.slots(), [(9, 58982u16)]);
        assert_eq!(v.total_q(), 58982);
        // 单条视图归一化后必然是 1.0。
        assert_eq!(v.normalized(9).to_bits(), 1.0f32.to_bits());
        assert_eq!(v.normalized(3).to_bits(), 0.0f32.to_bits());
        // 编码逐字节钉死：账户号小端 u32 + 分数小端 u16 + 条数小端 u16。
        assert_eq!(v.encode(), vec![9, 0, 0, 0, 102, 230, 1, 0]);
        assert_eq!(
            SparseView::decode(&v.encode(), 1).map(|x| (x.cap(), x.total_q(), x.slots().to_vec())),
            Some((1, 58982, vec![(9, 58982)]))
        );
    }

    /// cap == MAX_CAP == 4096 且被塞满：条数、总质量、编码长度、往返全部自洽。
    ///
    /// 守的是上界一侧：4096 条满额时 total_q = 268431360 < u32::MAX，
    /// Σq 的 u32 累加不会溢出；total_mass 恰好是 4096.0（2^24 以内的整数 f32 精确）。
    #[test]
    fn cap边界_塞满上限容量() {
        let pairs: Vec<(Acc, f32)> = (0..MAX_CAP as u32).map(|i| (i, 1.0)).collect();
        let v = SparseView::from_scores(MAX_CAP, &pairs);
        assert_eq!(v.cap(), MAX_CAP);
        assert_eq!(v.len(), MAX_CAP);
        assert_eq!(v.total_q(), 268_431_360);
        assert_eq!(v.total_mass().to_bits(), 4096.0f32.to_bits());
        assert_eq!(v.bytes(), 24578);
        assert_eq!(v.encode().len(), 24578);

        // 4096 个等权条目：每条恰好 1/4096，方差以外的误差为 0（2 的幂）。
        assert_eq!(v.normalized(0).to_bits(), 0x3980_0000);
        assert_eq!(v.normalized_slots()[0].1.to_bits(), 0x3980_0000);

        // 全集合求和逐位等于 1.0（Σ 的 f64 累加在这里恰好回到 1）。
        let sum: f64 = v
            .slots()
            .iter()
            .map(|&(a, _)| f64::from(v.normalized(a)))
            .sum();
        assert_eq!(sum, 1.0, "4096 等权条目的 Σv 应逐位等于 1.0");

        let bytes = v.encode();
        let back = SparseView::decode(&bytes, MAX_CAP).expect("满表负载应能解码");
        assert_eq!(back, v);
        assert_eq!(back.encode(), bytes);
    }

    // ═══════════════════ B. from_scores ═══════════════════

    /// 非正 / NaN / 极小分数不占槽位；恰好半个定点格的值才留得下。
    ///
    /// 守的是「0 就是没表态」与量化阈值：定点能分辨的最小正分量是 0.5/65535。
    #[test]
    fn from_scores_非正与极小不占槽() {
        let dirty = SparseView::from_scores(
            16,
            &[
                (1, 0.0),
                (2, fb(0x8000_0000)), // -0.0
                (3, f32::NAN),
                (4, -5.0),
                (5, 1e-9),
                (6, f32::MIN_POSITIVE),
                (7, fb(0x3700_007F)), // 比半个格小 1 ulp
            ],
        );
        assert_eq!(dirty.len(), 0, "这些分数没有一个该占槽");
        assert_eq!(dirty.total_q(), 0);
        assert!(dirty.is_empty());
        assert_eq!(dirty.bytes(), 16 * SLOT_BYTES + VIEW_OVERHEAD);

        // 正好等于半个定点格的值：留得下，但只有一个格点，还原后是 1/65535。
        let tiny = SparseView::from_scores(16, &[(7, 0.5 / 65535.0)]);
        assert_eq!(tiny.len(), 1);
        assert_eq!(tiny.slots(), [(7, 1u16)]);
        assert_eq!(tiny.total_q(), 1);
        assert_eq!(tiny.normalized(7).to_bits(), 1.0f32.to_bits());
        assert_eq!(tiny.total_mass().to_bits(), dequantize(1).to_bits());

        // NaN / 负数 / 超大值在构造入口就被消化：只剩被夹到 1.0 的那条。
        let clamp = SparseView::from_scores(16, &[(1, f32::NAN), (2, -5.0), (3, 1e9)]);
        assert_eq!(clamp.len(), 1);
        assert_eq!(clamp.slots(), [(3, Q_ONE)]);
        assert_eq!(clamp.total_q(), u32::from(Q_ONE));
    }

    /// 同一账户重复声明取最大值：不放大预算，且与声明顺序无关。
    ///
    /// 守的是「可交换、可结合」：如果改成求和，一条重复声明就变成双倍预算。
    #[test]
    fn from_scores_重复账户取最大值() {
        let a = SparseView::from_scores(64, &[(7, 0.5), (7, 0.9), (7, 0.1)]);
        let b = SparseView::from_scores(64, &[(7, 0.9), (7, 0.5), (7, 0.1)]);
        let c = SparseView::from_scores(64, &[(7, 0.1), (7, 0.9), (7, 0.5)]);
        assert_eq!(a.len(), 1);
        assert_eq!(a.q_of(7), 58982, "应取 0.9 对应的定点值");
        assert_eq!(a.total_q(), 58982, "重复声明不能累加");
        assert_eq!(a.encode(), b.encode());
        assert_eq!(a.encode(), c.encode());

        // 夹杂 NaN（量化成 0）时，0 分那条被直接跳过，不参与 max。
        let d = SparseView::from_scores(64, &[(7, 0.25), (7, f32::NAN), (7, 0.0)]);
        assert_eq!(d.slots(), [(7, 16384u16)]);
        assert_eq!(d.total_q(), 16384);
    }

    /// 超 cap 时按**分数降序**截断，保留分数最高的 cap 条。
    ///
    /// 守的是截断策略：丢掉的是尾巴，不是头；截断后仍按账户号升序落表。
    #[test]
    fn from_scores_超cap按分数降序截断() {
        // 三条争两个槽：0.9 与 0.5 进，0.1 出。
        let v = SparseView::from_scores(2, &[(1, 0.1), (2, 0.9), (3, 0.5)]);
        assert_eq!(v.len(), 2);
        assert_eq!(v.slots(), [(2, 58982u16), (3, 32768u16)]);
        assert_eq!(v.total_q(), 91750);

        // 大批量：300 条 → cap 128，保住分数最高的 128 条（账户 172..=299）。
        let p300: Vec<(Acc, f32)> = (0..300u32).map(|i| (i, (i + 1) as f32 / 300.0)).collect();
        let v300 = SparseView::from_scores(128, &p300);
        assert_eq!(v300.len(), 128);
        assert_eq!(v300.slots()[0], (172, 37792));
        assert_eq!(v300.slots()[1], (173, 38010));
        assert_eq!(v300.slots()[127], (299, 65535));
        assert_eq!(v300.total_q(), 6_612_922);
        assert_eq!(v300.bytes(), 770);

        // 更大批量：5000 条 → cap 4096（同时钉住 MAX_CAP 一侧的截断）。
        let p5000: Vec<(Acc, f32)> = (0..5000u32).map(|i| (i, (i + 1) as f32 / 5000.0)).collect();
        let v5000 = SparseView::from_scores(MAX_CAP, &p5000);
        assert_eq!(v5000.len(), MAX_CAP);
        assert_eq!(v5000.slots()[0], (904, 11862));
        assert_eq!(v5000.slots()[MAX_CAP - 1], (4999, 65535));
        assert_eq!(v5000.total_q(), 158_508_726);
        assert_eq!(v5000.bytes(), 24578);
    }

    /// 截断的并列打破规则：分数相同时**账户号小的优先**（全序，无歧义）。
    ///
    /// 守的是「同一份观点只有唯一字节串」：并列若不定序，截断结果就会随
    /// BTreeMap/哈希实现而变，S6 逐位确定性立刻失守。
    #[test]
    fn from_scores_并列按账户号升序() {
        // 四个同分争两个槽：留 3、5（账户号最小），丢 7、9。
        let tie2 = SparseView::from_scores(2, &[(3, 0.5), (9, 0.5), (7, 0.5), (5, 0.5)]);
        assert_eq!(tie2.slots(), [(3, 32768u16), (5, 32768u16)]);
        assert_eq!(tie2.total_q(), 65536);

        // 四个同分争三个槽。
        let tie3 = SparseView::from_scores(3, &[(13, 0.5), (10, 0.5), (12, 0.5), (11, 0.5)]);
        assert_eq!(
            tie3.slots(),
            [(10, 32768u16), (11, 32768u16), (12, 32768u16)]
        );

        // 混合：0.9 的 9/3/7 全部保留（账户号升序落表），0.5 的 1/2 争最后一个槽 → 1 号进。
        let mixed = SparseView::from_scores(4, &[(9, 0.9), (3, 0.9), (7, 0.9), (1, 0.5), (2, 0.5)]);
        assert_eq!(
            mixed.slots(),
            [(1, 32768u16), (3, 58982u16), (7, 58982u16), (9, 58982u16)]
        );
        assert_eq!(mixed.total_q(), 209_714);

        // 陷阱：0.5 与 0.50001 量化后是**同一个** q（相差不到半个格），
        // 所以截断并列只看 q，不看原始 f32。3 号因此被 1、2 号挤掉。
        let near_tie = SparseView::from_scores(2, &[(1, 0.5), (3, 0.5), (2, 0.50001)]);
        assert_eq!(near_tie.slots(), [(1, 32768u16), (2, 32768u16)]);
    }

    /// 截断结果与输入顺序无关（同一集合 ⟹ 逐字节相同的表）。
    #[test]
    fn from_scores_截断与输入顺序无关() {
        let pairs: Vec<(Acc, f32)> = (0..300u32).map(|i| (i, (i + 1) as f32 / 300.0)).collect();
        let a = SparseView::from_scores(128, &pairs);
        let mut rev = pairs.clone();
        rev.reverse();
        let b = SparseView::from_scores(128, &rev);
        assert_eq!(a.encode(), b.encode(), "逆序输入应得到逐字节相同的表");

        // 同一账户在多处出现 + 截断：仍然与顺序无关。
        let mut shuffled = pairs.clone();
        let mut rng = XorShift::new(0xC0FF_EE00);
        for i in (1..shuffled.len()).rev() {
            let j = rng.range(0, i as u32) as usize;
            shuffled.swap(i, j);
        }
        let c = SparseView::from_scores(128, &shuffled);
        assert_eq!(a.encode(), c.encode(), "打乱输入顺序不该改变编码");
    }

    // ═══════════════════ C. 编码规范形式 ═══════════════════

    /// 编码布局逐字节钉死：账户号小端 u32、分数小端 u16、尾部条数小端 u16、其余补零。
    ///
    /// 守的是「格式即协议」：字节布局一旦漂移，跨端（native/wasm/别的实现）
    /// 的逐位对拍就会静默失败。
    #[test]
    fn encode_布局逐字节() {
        // cap=2，[(1, 0.5), (2, 1.0)]：共 14 字节。
        let v = SparseView::from_scores(2, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(
            v.encode(),
            vec![1, 0, 0, 0, 0, 128, 2, 0, 0, 0, 255, 255, 2, 0]
        );
        assert_eq!(v.total_q(), 98_303);

        // 同一个视图放进 cap=4：只多了 12 字节的零填充区。
        let wide = SparseView::from_scores(4, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(
            wide.encode(),
            vec![
                1, 0, 0, 0, 0, 128, // 槽 0：账户 1，q=32768
                2, 0, 0, 0, 255, 255, // 槽 1：账户 2，q=65535
                0, 0, 0, 0, 0, 0, // 槽 2：空
                0, 0, 0, 0, 0, 0, // 槽 3：空
                2, 0, // 条数
            ]
        );
        assert_eq!(&wide.encode()[12..24], [0u8; 12], "填充区必须全零");

        // 空表：除了尾部条数（0）之外全是零。
        assert_eq!(SparseView::new(1).encode(), vec![0u8; 8]);
    }

    /// 规范形式的三条不变式：槽位账户号严格升序、q > 0、填充区全零、条数 == len。
    ///
    /// 三条合起来让「同一份观点」只有唯一字节串，encode(语义相同) 恒相等。
    #[test]
    fn encode_规范形式() {
        let pairs: Vec<(Acc, f32)> = (0..200u32).map(|i| (i, (i % 11) as f32 / 10.0)).collect();
        let v = SparseView::from_scores(64, &pairs);
        assert_eq!(v.len(), 64);

        // 槽位严格升序（不是「非降」，是「严格」——重复账户是非法负载）。
        let slots = v.slots();
        assert!(slots.len() >= 2);
        for w in slots.windows(2) {
            assert!(w[0].0 < w[1].0, "槽位账户号必须严格升序：{:?}", w);
        }
        // 每个槽的 q 都大于 0（0 分不占槽位）。
        assert!(slots.iter().all(|&(_, q)| q > 0));

        let bytes = v.encode();
        let fixed = v.cap() * SLOT_BYTES;
        // 槽位区前 len 条逐字节对应 slots，其后全零。
        for (i, &(acc, q)) in slots.iter().enumerate() {
            let off = i * SLOT_BYTES;
            assert_eq!(&bytes[off..off + 4], acc.to_le_bytes());
            assert_eq!(&bytes[off + 4..off + 6], q.to_le_bytes());
        }
        assert_eq!(bytes.len(), fixed + VIEW_OVERHEAD);
        assert!(bytes[slots.len() * SLOT_BYTES..fixed]
            .iter()
            .all(|&b| b == 0));
        assert_eq!(
            u16::from_le_bytes([bytes[fixed], bytes[fixed + 1]]) as usize,
            v.len(),
            "尾部条数必须等于实际条目数"
        );
    }

    // ═══════════════════ D. 解码与拒绝 ═══════════════════

    /// 合法负载往返：decode(encode(v)) == v，且再编码逐字节相同。
    ///
    /// 守的是「编码是规范形式」的可逆性：decode 不做二次量化、不重排槽位。
    #[test]
    fn decode_合法负载逐字节往返() {
        for cap in [1usize, 2, 128, 256, 512, MAX_CAP] {
            let n = if cap >= 512 { 512 } else { cap };
            let pairs: Vec<(Acc, f32)> = (0..n as u32)
                .map(|i| (i, (i + 1) as f32 / n.max(1) as f32))
                .collect();
            let v = SparseView::from_scores(cap, &pairs);
            let bytes = v.encode();
            let back = SparseView::decode(&bytes, cap).expect("合法负载必须能解码");
            assert_eq!(back, v, "cap={cap} 往返后视图不相等");
            assert_eq!(back.cap(), cap, "cap 必须原样保留");
            assert_eq!(back.total_q(), v.total_q(), "total_q 必须由槽位重算并一致");
            assert_eq!(back.encode(), bytes, "往返之后必须逐字节相同");
            // 空表往返。
            let empty = SparseView::new(cap);
            let eb = empty.encode();
            assert_eq!(SparseView::decode(&eb, cap).map(|x| x.len()), Some(0));
            assert_eq!(SparseView::decode(&eb, cap).expect("空表可解码"), empty);
        }
    }

    /// 长度与 cap 对不上，一律拒绝（不是「尽力解析」）。
    ///
    /// 守的是容量校验：cap 是协议参数，长度不符说明对面用了别的 cap 或负载被截断。
    #[test]
    fn decode_拒绝长度与cap不符() {
        let cap = 2usize;
        let good = SparseView::from_scores(cap, &[(1, 0.5), (3, 0.25)]).encode();
        let fixed = cap * SLOT_BYTES;

        assert!(SparseView::decode(&good[..good.len() - 1], cap).is_none());
        let mut longer = good.clone();
        longer.push(0);
        assert!(
            SparseView::decode(&longer, cap).is_none(),
            "多 1 字节也要拒绝"
        );
        assert!(
            SparseView::decode(&good, cap + 1).is_none(),
            "cap 与负载不符必须拒绝"
        );
        assert!(SparseView::decode(&good, cap - 1).is_none());

        // 正好是 0 条但填充区有垃圾。
        let mut garbage = vec![0u8; fixed + VIEW_OVERHEAD];
        garbage[0] = 1;
        assert!(
            SparseView::decode(&garbage, cap).is_none(),
            "count=0 时填充区必须全零"
        );

        // 条数取自尾部两字节；这里手工构造一条合法的 count=1 负载。
        let mut one = vec![0u8; fixed + VIEW_OVERHEAD];
        one[0..4].copy_from_slice(&7u32.to_le_bytes());
        one[4..6].copy_from_slice(&65535u16.to_le_bytes());
        one[fixed..fixed + 2].copy_from_slice(&1u16.to_le_bytes());
        let v = SparseView::decode(&one, cap).expect("手工构造的合法负载应能解码");
        assert_eq!(v.len(), 1);
        assert_eq!(v.slots(), [(7, 65535u16)]);
        assert_eq!(v.total_q(), 65_535);
        assert_eq!(v.encode(), one, "重新编码必须与输入逐字节相同");
    }

    /// 槽位区不规范：条数超容量、账户号非严格升序、重复账户、q == 0 —— 全部拒绝。
    ///
    /// 守的是三条攻击面：条数超 cap 会越界读；重复账户 = 双倍预算；
    /// 乱序会破坏 q_of 的二分查找前提。
    #[test]
    fn decode_拒绝不规范槽位区() {
        let cap = 2usize;
        let fixed = cap * SLOT_BYTES;
        let good = SparseView::from_scores(cap, &[(1, 0.5), (3, 0.25)]).encode();
        let mut bad = good.clone();
        bad[fixed..fixed + 2].copy_from_slice(&3u16.to_le_bytes());
        assert!(
            SparseView::decode(&bad, cap).is_none(),
            "条数 3 > cap 2 必须拒绝"
        );

        let mut bad = good.clone();
        bad[fixed..fixed + 2].copy_from_slice(&65535u16.to_le_bytes());
        assert!(
            SparseView::decode(&bad, cap).is_none(),
            "条数 65535 必须拒绝"
        );

        // 账户号降序（把第 0 槽改成 9，比第 1 槽的 3 大）。
        let mut bad = good.clone();
        bad[0..4].copy_from_slice(&9u32.to_le_bytes());
        assert!(SparseView::decode(&bad, cap).is_none(), "乱序必须拒绝");

        // 重复账户（把第 1 槽改成和第 0 槽一样）。
        let mut bad = good.clone();
        bad[6..10].copy_from_slice(&1u32.to_le_bytes());
        assert!(SparseView::decode(&bad, cap).is_none(), "重复账户必须拒绝");

        // 分数为 0（规范形式要求 0 分不占槽位）。
        let mut bad = good.clone();
        bad[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert!(SparseView::decode(&bad, cap).is_none(), "q=0 的槽必须拒绝");

        // 相邻槽位相等（acc <= prev 判定的是 <=，所以相等也拒绝）。
        let mut bad = good.clone();
        bad[6..10].copy_from_slice(&1u32.to_le_bytes());
        bad[fixed..fixed + 2].copy_from_slice(&2u16.to_le_bytes());
        assert!(SparseView::decode(&bad, cap).is_none());
    }

    /// 填充区（第一条空槽到定长区末尾）必须全零：禁止在定长区里夹带。
    ///
    /// 守的是「同一份观点唯一字节串」：不留夹带空间，就没有「语义相同、
    /// 字节不同」的歧义，也堵掉了把数据藏在空槽里的侧信道。
    #[test]
    fn decode_拒绝填充区夹带() {
        let cap = 2usize;
        let fixed = cap * SLOT_BYTES;
        let good = SparseView::from_scores(cap, &[(1, 0.5), (3, 0.25)]).encode();

        // 只声明 1 条，填充区（偏移 6..12）任何非零都拒绝。
        for pos in (SLOT_BYTES)..fixed {
            let mut bad = good.clone();
            bad[fixed..fixed + 2].copy_from_slice(&1u16.to_le_bytes());
            bad[pos] = 1;
            assert!(
                SparseView::decode(&bad, cap).is_none(),
                "填充区偏移 {pos} 夹带非零必须拒绝"
            );
        }

        // 条数 == cap 时没有填充区，任何字节都必须落在槽里，正常解码。
        let full = SparseView::from_scores(2, &[(1, 1.0), (2, 0.5)]);
        assert_eq!(full.len(), 2);
        assert_eq!(
            SparseView::decode(&full.encode(), 2).map(|x| x.len()),
            Some(2)
        );
    }

    /// 非法 cap 直接拒绝：0、超过 MAX_CAP、usize::MAX 都不能引起越界或 panic。
    #[test]
    fn decode_非法容量一律拒绝() {
        let good = SparseView::new(2).encode();
        assert!(SparseView::decode(&good, 0).is_none());
        assert!(SparseView::decode(&good, MAX_CAP + 1).is_none());
        assert!(SparseView::decode(&good, usize::MAX).is_none());
        // 空输入。
        assert!(SparseView::decode(&[], 2).is_none());
        // 合法 cap 但空表。
        assert_eq!(SparseView::decode(&good, 2).map(|x| x.len()), Some(0));
    }

    // ═══════════════════ E. 读数 ═══════════════════

    /// q_of 是二分查找：表内账户返回定点值，不在表里返回 0。
    #[test]
    fn q_of_不在表里返回零() {
        let v = SparseView::from_scores(8, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(v.q_of(1), 32768);
        assert_eq!(v.q_of(2), 65535);
        for acc in [0u32, 3, 4, 100, u32::MAX] {
            assert_eq!(v.q_of(acc), 0, "账户 {acc} 不在表里，q_of 应为 0");
            assert_eq!(v.normalized(acc).to_bits(), 0.0f32.to_bits());
        }
        // 边界账户号（0 与 u32::MAX）也能正确落表并读出。
        let edge = SparseView::from_scores(8, &[(0, 0.5), (u32::MAX, 0.25)]);
        assert_eq!(edge.q_of(0), 32768);
        assert_eq!(edge.q_of(u32::MAX), 16384);
        assert_eq!(edge.len(), 2);
    }

    /// 单条目视图：normalized 精确等于 1.0（q/Σq == 1 在 f32 里逐位成立）。
    #[test]
    fn normalized_单条目满额() {
        let v = SparseView::from_scores(4, &[(5, 1.0)]);
        assert_eq!(v.len(), 1);
        assert_eq!(v.total_q(), 65535);
        assert_eq!(v.normalized(5).to_bits(), 1.0f32.to_bits());
        assert_eq!(v.normalized_slots(), [(5, 1.0f32)]);

        // 哪怕这条只有 1 个定点格，归一化后也是精确的 1.0。
        let tiny = SparseView::from_scores(4, &[(7, 0.5 / 65535.0)]);
        assert_eq!(tiny.total_q(), 1);
        assert_eq!(tiny.normalized(7).to_bits(), 1.0f32.to_bits());
        assert_eq!(tiny.normalized_slots(), [(7, 1.0f32)]);
    }

    /// 归一化在声明集合上 Σ ≡ 1（容差 1e-6）；单条 ≤ 1、非负。
    ///
    /// 这是 S2 判据（超量声明只有一份预算）的单元级落点：
    /// 分子的 q 被 u16 钉在 [0,1]，分母是 Σq 的精确整数，所以 Σ(q/Σq) ≡ 1。
    #[test]
    fn normalized_声明集合上求和恒一() {
        // 两条：0.5 与 1.0 → v = 32768/98303 与 65535/98303。
        let v = SparseView::from_scores(8, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(v.total_q(), 98_303);
        assert_eq!(v.normalized(1).to_bits(), 0x3EAA_AB1C); // 0.3333367
        assert_eq!(v.normalized(2).to_bits(), 0x3F2A_AA72); // 0.6666633
        let sum: f64 = v
            .slots()
            .iter()
            .map(|&(a, _)| f64::from(v.normalized(a)))
            .sum();
        assert_eq!(sum, 1.0, "两条时求和恰好回到 1.0");

        // 50 条等间距：误差 3.0850060284137726e-9，逐位钉死。
        let p50: Vec<(Acc, f32)> = (0..50u32).map(|i| (i, (i + 1) as f32 / 50.0)).collect();
        let v50 = SparseView::from_scores(512, &p50);
        assert_eq!(v50.total_q(), 1_671_145);
        let sum50: f64 = v50
            .slots()
            .iter()
            .map(|&(a, _)| f64::from(v50.normalized(a)))
            .sum();
        assert_eq!(sum50.to_bits(), 0x3FF0_0000_00D4_0000);
        let dev = (sum50 - 1.0).abs();
        assert_eq!(dev.to_bits(), 0x3E2A_8000_0000_0000);
        assert!(dev < SUM_TOL, "50 条时 Σv 偏差 {dev:?} 超出 1e-6");

        // normalized_slots 走的是同一条 f64 除法，求和一致。
        let sum_slots: f64 = v50.normalized_slots().iter().map(|x| f64::from(x.1)).sum();
        assert_eq!(sum_slots, sum50);

        // 每条 v 都在 [0,1] 内。
        for &(_, x) in v50.normalized_slots().iter() {
            assert!((0.0..=1.0).contains(&x), "单条 v 越界：{x}");
        }

        // 空表：没有声明集合，normalized_slots 返回空，求和是 0（不是 NaN）。
        // 注意 f64 的 sum() 对空迭代器给出的是 **-0.0**（本工具链实测），
        // 这里比数值而不是位模式：-0.0 == 0.0，且必须有限。
        let empty = SparseView::new(8);
        assert!(empty.normalized_slots().is_empty());
        let s: f64 = empty
            .normalized_slots()
            .iter()
            .map(|x| f64::from(x.1))
            .sum();
        assert_eq!(s, 0.0, "空表求和应为 0，实际 {s:?}");
        assert!(s.is_finite(), "空表求和不能是 NaN");
    }

    /// ⚠️ 可疑，建议作者确认：normalized 与 normalized_slots 在总质量 > 2^24 时
    /// 给出**相差 1 ulp** 的两个结果。
    ///
    /// normalized 用 f64 除法（分子分母都是精确整数）再截成 f32——这是正确舍入的；
    /// normalized_slots 先把 total_q 转成 f32（在 2^24 以上会丢低位）再除，
    /// 于是偏一格。构型：256 条 1.0 + 1 条 q=257 → total_q = 16777217，
    /// 而 (16777217 as f32) = 16777216，差 1。
    /// 影响面：Σv 仍是 1（偏差 6e-8 < 1e-6），但「同一份视图、两条读数 API 位模式不同」
    /// 足以让逐位对拍在 Σq > 2^24 的极端视图上失守。文档说「归一化一律走整数」，
    /// normalized_slots 却是 f32 路径——建议确认是否统一到 f64 版本。
    #[test]
    fn normalized_与normalized_slots在大总质量下差一个ulp() {
        // 256 条满额 + 1 条 q=257：总和 16777217 = 2^24 + 1。
        let mut pairs: Vec<(Acc, f32)> = (0..256u32).map(|i| (i, 1.0)).collect();
        pairs.push((256, dequantize(257)));
        let v = SparseView::from_scores(512, &pairs);
        assert_eq!(v.len(), 257);
        assert_eq!(v.total_q(), 16_777_217);
        assert_eq!(v.total_q() as f32, 16_777_216.0, "2^24+1 转 f32 会丢一位");

        // 两条读数 API 逐槽位全部差一个 ulp（257 条全中）。
        let ns = v.normalized_slots();
        let mut mismatched = 0usize;
        for &(acc, x) in &ns {
            if x.to_bits() != v.normalized(acc).to_bits() {
                mismatched += 1;
            }
        }
        assert_eq!(
            mismatched,
            ns.len(),
            "极端视图上两个 API 应处处不同（当前实现如此）"
        );
        assert_eq!(v.normalized(0).to_bits(), 0x3B7F_FEFF);
        assert_eq!(ns[0].1.to_bits(), 0x3B7F_FF00);
        assert_eq!(v.normalized(256).to_bits(), 0x3780_7FFF);
        assert_eq!(ns[256].1.to_bits(), 0x3780_8000);

        // 两者都还在 1e-6 容差内——所以 S2 判据本身不受影响，差的只是位模式。
        let sum_n: f64 = v
            .slots()
            .iter()
            .map(|&(a, _)| f64::from(v.normalized(a)))
            .sum();
        let sum_ns: f64 = ns.iter().map(|x| f64::from(x.1)).sum();
        assert!((sum_n - 1.0).abs() < SUM_TOL, "normalized 求和 {sum_n}");
        assert!(
            (sum_ns - 1.0).abs() < SUM_TOL,
            "normalized_slots 求和 {sum_ns}"
        );
        assert_ne!(
            (sum_n - 1.0).abs().to_bits(),
            (sum_ns - 1.0).abs().to_bits(),
            "两条路径的误差不同（一个 1.8e-12、一个 5.96e-8）"
        );
    }

    // ═══════════════════ F. 总质量 ═══════════════════

    /// total_q 是 u32 精确累加；total_mass 是它除以 65535 的展示值。
    #[test]
    fn total_q与total_mass() {
        let v = SparseView::from_scores(8, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(v.total_q(), 32768 + 65535);
        assert_eq!(v.total_mass().to_bits(), 0x3FC0_0040); // 1.5000076
        assert_eq!(v.total_mass(), v.total_q() as f32 / Q_ONE as f32);

        assert_eq!(
            SparseView::from_scores(8, &[(1, 1.0)])
                .total_mass()
                .to_bits(),
            1.0f32.to_bits()
        );
        assert_eq!(
            SparseView::from_scores(8, &[(1, 0.5)])
                .total_mass()
                .to_bits(),
            0x3F00_0080 // 0.5000076
        );

        // ⚠️ 可疑（低危），建议作者确认：total_mass 先把 u32 转 f32，
        // 257 × 65535 = 16842495 > 2^24，转 f32 变成 16842496，
        // 于是展示值 257.00003 而不是精确的 257。这是一条「仅用于展示」的路径，
        // 文档已标注归一化不走它，故不判为缺陷，但值得钉住以免被误用。
        let p257: Vec<(Acc, f32)> = (0..257u32).map(|i| (i, 1.0)).collect();
        let v257 = SparseView::from_scores(512, &p257);
        assert_eq!(v257.total_q(), 16_842_495);
        assert_eq!(v257.total_mass().to_bits(), 0x4380_8001);
        assert!(v257.total_mass() > 257.0, "展示值略大于精确的 257");
    }

    // ═══════════════════ G. 截断质量 ═══════════════════

    /// truncated_mass 的口径：1 − 保留的定点质量 / 全部定点质量（f64）。
    ///
    /// 守的是「截断损失可被精确算出来」：不是估计，是同一套量化后的整数之比。
    #[test]
    fn truncated_mass_口径与具体值() {
        // 两条争一个槽：留 65535，丢 32768。
        let t1 = SparseView::truncated_mass(1, &[(1, 1.0), (2, 0.5)]);
        assert_eq!(t1.to_bits(), 0x3FD5_5563_8E42_5ED6);
        assert_eq!(t1, 1.0 - 65535.0f64 / 98303.0f64);

        // 300 → 128：保留 172..=299，与视图的 total_q 必须对得上。
        let p300: Vec<(Acc, f32)> = (0..300u32).map(|i| (i, (i + 1) as f32 / 300.0)).collect();
        let t2 = SparseView::truncated_mass(128, &p300);
        assert_eq!(t2.to_bits(), 0x3FD5_16EB_A3CF_BCD2);
        let all: f64 = (0..300)
            .map(|i| f64::from(quantize((i + 1) as f32 / 300.0)))
            .sum();
        assert_eq!(t2, 1.0 - 6_612_922.0f64 / all);

        // 5000 → 4096。
        let p5000: Vec<(Acc, f32)> = (0..5000u32).map(|i| (i, (i + 1) as f32 / 5000.0)).collect();
        let t3 = SparseView::truncated_mass(MAX_CAP, &p5000);
        assert_eq!(t3.to_bits(), 0x3FA0_C072_5F1E_F290);
        let all3: f64 = (0..5000)
            .map(|i| f64::from(quantize((i + 1) as f32 / 5000.0)))
            .sum();
        assert_eq!(t3, 1.0 - 158_508_726.0f64 / all3);

        // 没超容量：精确的 0.0。
        assert_eq!(
            SparseView::truncated_mass(512, &[(1, 1.0), (2, 0.5)]).to_bits(),
            0.0f64.to_bits()
        );
        // 全是 0 分/脏数据：没有被量化进来的条目，口径定义成 0 而不是 NaN。
        assert_eq!(
            SparseView::truncated_mass(4, &[(1, 0.0), (2, -1.0), (3, f32::NAN)]).to_bits(),
            0.0f64.to_bits()
        );
        // 重复账户先合并（取最大）再算，不会把重复声明算成两份质量。
        assert_eq!(
            SparseView::truncated_mass(1, &[(7, 0.5), (7, 0.9)]).to_bits(),
            0.0f64.to_bits()
        );
        assert_eq!(SparseView::truncated_mass(1, &[(7, 0.5), (7, 0.9)]), 0.0);
    }

    /// ⚠️ 可疑，建议作者确认：truncated_mass 不校验 cap，cap=0 返回 1.0、cap>MAX_CAP 返回 0.0。
    ///
    /// 同一个参数在 new() 里是 panic 的配置错误，在这里却静默给出数值。
    /// 现场：cap=0 时「全部质量都被截掉」→ 1.0；cap=4097（超过上限）时
    /// 「没截断」→ 0.0。后果是运维/报告侧可能把这两种非法配置当成合法读数。
    #[test]
    fn truncated_mass_非法cap不校验() {
        assert_eq!(SparseView::truncated_mass(0, &[(1, 1.0)]), 1.0);
        assert_eq!(SparseView::truncated_mass(0, &[(1, 0.0)]), 0.0);
        assert_eq!(SparseView::truncated_mass(0, &[]), 0.0);
        assert_eq!(
            SparseView::truncated_mass(MAX_CAP + 1, &[(1, 1.0), (2, 1.0)]),
            0.0
        );
    }

    // ═══════════════════ H. 内部构造入口 ═══════════════════

    /// ⚠️ 可疑，建议作者确认：from_slots_unchecked 的文档说「会校验升序与 q > 0」，
    /// 但函数体**没有任何校验**，直接采信调用方。
    ///
    /// 守的是「私有入口的契约」：目前唯一的调用方 decode 已在外面校验过，
    /// 所以现状不产生漏洞；但文档与实现不符，以后新增调用方就等于埋了一个
    /// 「乱序/零分视图」的口子——编码出去会被自己的 decode 拒绝。
    #[test]
    fn from_slots_unchecked_实际不校验() {
        // 乱序 + 分数为 0，函数照单全收。
        let broken = SparseView::from_slots_unchecked(4, vec![(5, 1), (3, 0)]);
        assert_eq!(broken.len(), 2);
        assert_eq!(broken.total_q(), 1, "total_q 由槽位求和得到，含 0 分槽");
        assert_eq!(broken.slots(), [(5, 1u16), (3, 0u16)]);
        // 槽位乱序，二分查找的前提被破坏：5 号明明在表里，q_of 却读不到。
        assert_eq!(
            broken.q_of(5),
            0,
            "乱序让 q_of 读不到 5 号（这正是必须校验的原因）"
        );
        assert_eq!(broken.q_of(3), 0, "0 分的 3 号在本机读作没表态");
        assert_eq!(
            broken.normalized(3).to_bits(),
            0.0f32.to_bits(),
            "0 分槽归一化后是 0"
        );
        // 它编码出来的字节，连自己的 decode 都不认（乱序 + 0 分槽双重违规）。
        assert!(
            SparseView::decode(&broken.encode(), 4).is_none(),
            "不校验的入口能造出自家 decode 拒绝的负载"
        );
    }

    /// 相等性包含 cap：内容相同但 cap 不同 ⟹ 不相等（字节串也不同）。
    ///
    /// 守的是「视图 == 语义 + 容量」：跨 cap 的两个视图编码长度不同，
    /// 不能互相替换，否则 decode 端的长度校验会失败。
    #[test]
    fn partial_eq_跨cap不相等() {
        let a = SparseView::from_scores(4, &[(1, 0.5), (2, 1.0)]);
        let b = SparseView::from_scores(8, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(a.cap(), 4);
        assert_eq!(b.cap(), 8);
        assert_eq!(a.slots(), b.slots(), "槽位内容相同");
        assert_ne!(a, b, "cap 不同就不能相等");
        assert_ne!(a.encode(), b.encode(), "编码长度不同");
        assert_eq!(a.bytes(), 26);
        assert_eq!(b.bytes(), 50);
    }

    // ═══════════════════ I. 确定性伪随机负载 ═══════════════════

    /// 随机负载恒满足规范形式与往返闭合：这条守住所有构造路径的共同不变式。
    ///
    /// 用 xorshift 生成确定性负载（含重复账户、0 分、NaN、负数、>1 的超量值、
    /// 账户号上界），对 cap ∈ {1, 2, 7, 128, 512} 逐一验证：
    /// 槽位严格升序、q > 0、len ≤ cap、total_q == Σq、编码规范、往返逐字节相等、
    /// 归一化求和落在 1e-6 内。
    #[test]
    fn random_负载恒满足规范形式() {
        let mut rng = XorShift::new(0x5EED_1234_ABCD_0001);
        for &cap in &[1usize, 2, 7, 128, 512] {
            let n = rng.range(0, cap as u32 * 3) as usize;
            let mut pairs: Vec<(Acc, f32)> = Vec::with_capacity(n);
            for _ in 0..n {
                let acc = rng.range(0, 300);
                let score = match rng.next_u32() % 10 {
                    0 => 0.0,
                    1 => -1.0,
                    2 => f32::NAN,
                    3 => 1e9,
                    4 => 0.5 / 65535.0 * 0.5, // 量化后归零
                    _ => rng.range(1, 65535) as f32 / 65535.0,
                };
                pairs.push((acc, score));
            }

            let v = SparseView::from_scores(cap, &pairs);
            assert!(v.len() <= cap);
            assert_eq!(v.cap(), cap);
            // 槽位严格升序。
            for w in v.slots().windows(2) {
                assert!(w[0].0 < w[1].0, "cap={cap} 槽位乱序：{:?}", w);
            }
            // q > 0，且 total_q 就是各槽之和。
            assert!(v.slots().iter().all(|&(_, q)| q > 0));
            let sum: u32 = v.slots().iter().map(|&(_, q)| u32::from(q)).sum();
            assert_eq!(v.total_q(), sum);
            // 编码规范 + 往返逐字节闭合。
            let bytes = v.encode();
            assert_eq!(bytes.len(), cap * SLOT_BYTES + VIEW_OVERHEAD);
            assert_eq!(bytes.len(), v.bytes());
            let back = SparseView::decode(&bytes, cap).expect("自产的负载必须能解码");
            assert_eq!(back, v);
            assert_eq!(back.encode(), bytes);
            // 归一化：非空时 Σ ≡ 1（1e-6 内）。
            if !v.is_empty() {
                let s: f64 = v
                    .slots()
                    .iter()
                    .map(|&(a, _)| f64::from(v.normalized(a)))
                    .sum();
                assert!((s - 1.0).abs() < SUM_TOL, "cap={cap} 时 Σv = {s}");
            }
        }
    }

    /// 同一集合、不同到达顺序 ⟹ 逐字节相同的视图（逐位确定性 S6 的单元级落点）。
    ///
    /// 含重复账户与并列分数：这正是「取最大值 + (分数降序, 账户号升序) 全序」
    /// 两条规则被同时考到的地方。
    #[test]
    fn random_同一集合不同顺序逐字节相同() {
        for seed in [1u64, 0xDEAD_BEEF, 0x2545_F491_4F6C_DD1D] {
            let mut rng = XorShift::new(seed);
            let n = rng.range(0, 200) as usize;
            let mut pairs: Vec<(Acc, f32)> = Vec::with_capacity(n);
            for _ in 0..n {
                let acc = rng.range(0, 40); // 故意让账户重复
                let q = rng.range(1, 65535) as u16;
                pairs.push((acc, dequantize(q)));
            }
            let cap = 13usize; // 故意小于不同账户数，强制触发截断
            let base = SparseView::from_scores(cap, &pairs);

            let mut rev = pairs.clone();
            rev.reverse();
            let reversed = SparseView::from_scores(cap, &rev);
            assert_eq!(base.encode(), reversed.encode(), "逆序改变了编码");

            // 另一种确定性打乱。
            let mut shuffled = pairs.clone();
            let mut r2 = XorShift::new(seed ^ 0x9E37_79B9_7F4A_7C15);
            for i in (1..shuffled.len()).rev() {
                let j = r2.range(0, i as u32) as usize;
                shuffled.swap(i, j);
            }
            assert_eq!(
                SparseView::from_scores(cap, &shuffled).encode(),
                base.encode(),
                "打乱顺序改变了编码"
            );

            // 同一个种子跑两遍：结果逐位相同（没有隐藏的全局状态）。
            let again = SparseView::from_scores(cap, &pairs);
            assert_eq!(again.encode(), base.encode());
            assert_eq!(again, base);
        }
    }
}
