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
