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
//! # 两种编码（旧格式 / 带前缀格式）
//!
//! 定长体由 [`SparseView::encode`] 输出，**不自描述 cap**（接收方必须带外知道它）：
//!
//! ~~~text
//! [0, cap×6)        槽位区（前 len 条有效，其后全零）
//! [cap×6, cap×6+2)  条数 len（小端 u16）
//! 总长               cap×6 + 2
//! ~~~
//!
//! [`SparseView::encode_prefixed`] 在它**前面**多写 2 字节 cap，于是接收方不需要
//! 预先知道 cap 就能解（[`SparseView::decode_prefixed`]），网络不会再按 cap 分裂成孤岛：
//!
//! ~~~text
//! [0, 2)             cap（小端 u16）
//! [2, cap×6+4)       定长体，与 encode() 的输出逐字节相同
//! 总长               2 + cap×6 + 2 = cap×6 + 4
//! ~~~
//!
//! 两种格式**互不误判**，但理由不是「前 2 字节的语义」，而是总长落在模 6 的
//! **不同同余类**：旧 `6·C+2 ≡ 2`、新 `6·C+4 ≡ 4`。所以旧字节喂
//! [`SparseView::decode_prefixed`]、新字节喂 [`SparseView::decode`]，
//! 都会在长度检查上必然失败。
//!
//! **这条安全性完全依赖「两个解码器都要求长度严格相等」。** 任何一处放宽成
//! 「长度在某范围内」「按 (len−2)/6 猜 cap 再四舍五入」，都会立刻重新引入
//! 「看似解码成功、语义全错」的风险。分发必须做精确长度匹配；若未来出现第三种
//! 格式，必须加带内魔数，不能靠长度同余类这种隐式契约。完整分析见
//! `crates/mincore/ENCODING.md`。
//!
//! 可诊断的失败原因见 [`DecodeError`] 与 [`SparseView::decode_prefixed_result`]；
//! 旧的 [`SparseView::decode`] 保持 `Option` 签名不变，判据与新的**逐条一致**。
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

/// 自描述前缀的字节数：`encode_prefixed` 在定长体**前面**多写的 2 字节 cap（小端 u16）。
///
/// 旧格式 [`SparseView::encode`] 不带这个头，总长 `cap × 6 + 2`；
/// 带前缀格式总长 `2 + cap × 6 + 2 = cap × 6 + 4`。
/// 两个长度落在模 6 的**不同同余类**（2 与 4），这是两种格式「靠长度就能互不误判」的全部理由——
/// 也正因为如此，分发端必须做**精确**长度匹配（见 crate 根目录的 `ENCODING.md`）。
pub const PREFIX_BYTES: usize = 2;

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

/// 解码失败的原因。由 [`SparseView::decode_prefixed_result`] 返回，
/// 让「拒绝」从 `None` 变成可诊断的具体理由。
///
/// 旧入口 [`SparseView::decode`] 仍然只返回 `Option`（签名不变），
/// 它的每一条拒绝路径本枚举都有对应变体 —— 两条路径的判据**必须**保持一致，
/// 差别只在「要不要说出理由」。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecodeError {
    /// 前缀里的 `cap` 非法：0、超过 [MAX_CAP]，或把它当 `u16` 读不了。
    BadCap,
    /// 总长度与「前缀声明的 cap」算出来的定长长度**不恰好**相等。
    ///
    /// 这是本格式唯一的格式鉴别器：旧格式 `6·C+2 ≡ 2 (mod 6)`、
    /// 前缀格式 `6·C+4 ≡ 4 (mod 6)`，两边都要求长度**精确**匹配才可能解成功。
    BadLength,
    /// 尾部条数 `count > cap`。
    BadCount,
    /// 槽位账户号不是**严格**升序（乱序或与前一个相等）。
    NotAscending,
    /// 同一个账户号出现两次（[DecodeError::NotAscending] 的特例，单独可辨）。
    DuplicateAcc,
    /// 某个有效槽位的分数 `q == 0`（规范形式要求 0 分不占槽位）。
    ZeroScore,
    /// 有效槽位之后的填充区不是全零（禁止在定长区里夹带）。
    NonZeroPadding,
}

impl DecodeError {
    /// 一个稳定的、人类可读的短标签（排查日志用；不承诺跨版本不变）。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BadCap => "bad-cap",
            Self::BadLength => "bad-length",
            Self::BadCount => "bad-count",
            Self::NotAscending => "not-ascending",
            Self::DuplicateAcc => "duplicate-account",
            Self::ZeroScore => "zero-score",
            Self::NonZeroPadding => "nonzero-padding",
        }
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for DecodeError {}

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

    /// 直接由槽位构造，**不做任何校验**（内部用）。
    ///
    /// # 调用方契约
    ///
    /// 必须保证：`slots.len() <= cap`、按账户号**严格升序**、**每个 `q > 0`**。
    /// 违反任一条会让 `q_of` 的二分查找失效或让 `Σv ≡ 1` 失真。
    ///
    /// 当前有两个调用方：`Self::decode` 与 `Self::decode_body_result` 的末尾 —— 两者
    /// 都在外层把长度、条数、升序、重复、零分、填充区校验过一遍，所以这里省掉重复检查。
    /// **新增调用方时必须自己校验**，或者改用 `Self::from_scores`。
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
    ///
    /// 与 [Self::normalized] **走同一条 f64 除法路径**，所以同一个账户在这两个
    /// 接口上读出**逐位相同**的 f32。
    ///
    /// 为什么必须是 f64：`total_q` 是 u32，可以超过 2^24（此时 f32 无法精确表示），
    /// 若先把 `total_q` 转 f32 再除，低位会被舍入掉，导致 `normalized_slots` 与
    /// `normalized` 差 1 ulp —— 那会让「逐位对拍」在极端视图上失守（S6 的前提）。
    #[must_use]
    pub fn normalized_slots(&self) -> Vec<(Acc, f32)> {
        if self.total_q == 0 {
            return Vec::new();
        }
        self.slots
            .iter()
            .map(|&(a, q)| (a, (f64::from(q) / f64::from(self.total_q)) as f32))
            .collect()
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

    /// 序列化成**恰好** `cap × 6 + 2` 字节（旧格式，不带 cap 头）。
    ///
    /// 布局：`[0, cap*6)` 是槽位区（前 `len` 条有效，其后补零），
    /// 最后 2 字节是条数（小端 u16）。
    ///
    /// 这条格式**不自描述 cap**：接收方必须带外知道 cap 才能解。要自描述
    /// （解掉「网络按 cap 分裂成孤岛」）用 [`Self::encode_prefixed`]。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.cap * SLOT_BYTES + VIEW_OVERHEAD];
        self.write_body(&mut out);
        out
    }

    /// 序列化成**恰好** `2 + cap × 6 + 2` 字节：先 2 字节自带 cap，再是旧格式的定长体。
    ///
    /// 布局：
    /// ~~~text
    /// offset 0          : cap（小端 u16，2 B）
    /// offset 2          : 旧格式的定长体（见 [Self::encode]），长度 cap × 6 + 2
    /// 总长度            : 2 + cap × 6 + 2 = cap × 6 + 4
    /// ~~~
    ///
    /// **[2..] 与 [`Self::encode`] 的输出逐字节相同**——新格式只是在旧负载前面多了个
    /// 自描述头，没有改动定长体一个字节，所以 `encode_prefixed()[2..] == encode()`。
    ///
    /// # 与旧格式的互不误判
    ///
    /// 两格式总长落在模 6 的**不同同余类**：旧 `6·C+2 ≡ 2`、新 `6·C+4 ≡ 4`。
    /// 因此旧字节喂 [`Self::decode_prefixed`] 必然在长度检查上被拒，反之亦然。
    /// 但这条安全性**完全依赖两个解码器都要求长度严格相等**：任何一处放宽成
    /// 「长度在某范围内」「按 (len−2)/6 猜 cap 再四舍五入」都会立刻重新引入
    /// 「看似解码成功、语义全错」的风险。分发必须精确长度匹配；
    /// 若未来出现第三种格式，必须加带内魔数，不能靠长度同余类这种隐式契约。
    /// 详见 `crates/mincore/ENCODING.md`。
    ///
    /// # Panics
    ///
    /// `cap` 落在 `[1, 65535]` 之外时 panic（配置错误，不是运行时输入）。
    #[must_use]
    pub fn encode_prefixed(&self) -> Vec<u8> {
        let cap16 = self.encoded_cap();
        let mut out = vec![0u8; PREFIX_BYTES + self.cap * SLOT_BYTES + VIEW_OVERHEAD];
        out[..PREFIX_BYTES].copy_from_slice(&cap16.to_le_bytes());
        self.write_body(&mut out[PREFIX_BYTES..]);
        out
    }

    /// 序列化进一个已经有正确长度的缓冲区：`[0, cap*6+2)` 是定长体。
    ///
    /// 越界写会 panic，所以两个公开编码入口都先分配好`bytes()`长度的缓冲区。
    fn write_body(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), self.bytes());
        let fixed = self.cap * SLOT_BYTES;
        for (i, &(acc, q)) in self.slots.iter().enumerate() {
            let off = i * SLOT_BYTES;
            out[off..off + 4].copy_from_slice(&acc.to_le_bytes());
            out[off + 4..off + 6].copy_from_slice(&q.to_le_bytes());
        }
        let n = self.slots.len() as u16;
        out[fixed..fixed + 2].copy_from_slice(&n.to_le_bytes());
    }

    /// 把 `cap` 转成前缀里的 u16。
    ///
    /// `new()` 已把合法 cap 夹在 `[1, MAX_CAP]` 内，而 `MAX_CAP == 4096 < 65535`，
    /// 所以这里的断言在**所有**构造路径上都成立（`from_slots_unchecked` 是私有的，
    /// 它的两个调用方都已校验过 cap）。
    fn encoded_cap(&self) -> u16 {
        let cap16 = u16::try_from(self.cap).expect("cap 在 [1, MAX_CAP] 内，必然装得进 u16");
        debug_assert!(self.cap >= 1 && self.cap <= MAX_CAP);
        cap16
    }

    /// 反序列化（旧格式，`cap` 由调用方给出）。**任何**不合规范形式或不合容量的
    /// 负载都返回 `None`。
    ///
    /// 判据与 [`Self::decode_prefixed_result`] 完全一致，只是这里不说明理由。
    #[must_use]
    pub fn decode(bytes: &[u8], cap: usize) -> Option<Self> {
        if cap == 0 || cap > MAX_CAP {
            return None;
        }
        let fixed = cap.checked_mul(SLOT_BYTES)?;
        if bytes.len() != fixed.checked_add(VIEW_OVERHEAD)? {
            return None;
        }
        Self::decode_body_result(bytes).ok()
    }

    /// 反序列化**带前缀**的负载：cap 从头部读，调用方不需要预先知道它。
    ///
    /// 等价于 `Self::decode_prefixed_result(bytes).ok()`。
    #[must_use]
    pub fn decode_prefixed(bytes: &[u8]) -> Option<Self> {
        Self::decode_prefixed_result(bytes).ok()
    }

    /// 反序列化带前缀的负载，失败时给出**为什么**（[DecodeError]）。
    ///
    /// # 检查顺序（任何一条不过就立刻返回，不在错误输入上继续走）
    ///
    /// 1. 连前缀都不够 2 字节 ⟹ [DecodeError::BadLength]
    /// 2. 前缀里的 cap 为 0 或超过 [MAX_CAP] ⟹ [DecodeError::BadCap]
    /// 3. 总长不恰好等于 `2 + cap × 6 + 2` ⟹ [DecodeError::BadLength]
    /// 4. 之后与旧格式完全相同的规范性检查（条数 / 升序 / 重复 / 零分 / 填充区）
    ///
    /// 注意顺序：cap 先于长度。这样「用别的 cap 编出来的负载」会诚实地报
    /// [DecodeError::BadLength]，而不是被某个越界读取悄悄吞掉。
    ///
    /// # 防溢出
    ///
    /// 长度计算全程走 `checked_mul` / `checked_add`：前缀是攻击者可控的，
    /// `cap × 6` 在 32 位目标上也不能回绕。
    pub fn decode_prefixed_result(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < PREFIX_BYTES {
            return Err(DecodeError::BadLength);
        }
        let cap = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        if cap == 0 || cap > MAX_CAP {
            return Err(DecodeError::BadCap);
        }
        let fixed = cap.checked_mul(SLOT_BYTES).ok_or(DecodeError::BadLength)?;
        let want = PREFIX_BYTES
            .checked_add(fixed)
            .and_then(|n| n.checked_add(VIEW_OVERHEAD))
            .ok_or(DecodeError::BadLength)?;
        if bytes.len() != want {
            return Err(DecodeError::BadLength);
        }
        Self::decode_body_result(&bytes[PREFIX_BYTES..])
    }

    /// 定长体（不含前缀）的规范性检查。旧格式与新格式共用这一份实现。
    ///
    /// `body` 的长度必须已经由调用方校验过（恰为 `cap × 6 + 2`）；
    /// 这里的 cap 由上层的 cap 校验保证落在 `[1, MAX_CAP]` 内。
    fn decode_body_result(body: &[u8]) -> Result<Self, DecodeError> {
        // 反算 cap：长度恰为 cap*6+2 是上层已经钉死的，所以这里必然整除。
        // 用 checked 运算而不是 unwrap，是为了让「函数被单独调用」时也不 panic。
        let fixed_len = body
            .len()
            .checked_sub(VIEW_OVERHEAD)
            .ok_or(DecodeError::BadLength)?;
        let cap = fixed_len / SLOT_BYTES;
        // 用乘法复核整除（而不是 `fixed_len % SLOT_BYTES`）：溢出安全，也顺手挡掉余数。
        if cap.checked_mul(SLOT_BYTES) != Some(fixed_len) {
            return Err(DecodeError::BadLength);
        }
        // 上层已校验，但本函数可以是唯一防线（未来若新增调用方），所以再验一次。
        if cap == 0 || cap > MAX_CAP {
            return Err(DecodeError::BadCap);
        }
        let count = u16::from_le_bytes([body[fixed_len], body[fixed_len + 1]]) as usize;
        if count > cap {
            return Err(DecodeError::BadCount);
        }
        let mut slots = Vec::with_capacity(count);
        let mut prev: Option<Acc> = None;
        for i in 0..count {
            let off = i * SLOT_BYTES;
            let acc = u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
            let q = u16::from_le_bytes([body[off + 4], body[off + 5]]);
            if q == 0 {
                return Err(DecodeError::ZeroScore); // 规范形式：0 分不占槽位
            }
            if let Some(p) = prev {
                if acc == p {
                    return Err(DecodeError::DuplicateAcc);
                }
                if acc < p {
                    return Err(DecodeError::NotAscending);
                }
            }
            prev = Some(acc);
            slots.push((acc, q));
        }
        // 填充区必须全零：禁止在定长区里夹带任何东西。
        if body[count * SLOT_BYTES..fixed_len].iter().any(|&b| b != 0) {
            return Err(DecodeError::NonZeroPadding);
        }
        Ok(Self::from_slots_unchecked(cap, slots))
    }

    /// 编码后的字节数（恒定，旧格式）。
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.cap * SLOT_BYTES + VIEW_OVERHEAD
    }

    /// 带前缀编码后的字节数（恒定）= `2 + bytes()`。
    #[must_use]
    pub fn prefixed_bytes(&self) -> usize {
        PREFIX_BYTES + self.cap * SLOT_BYTES + VIEW_OVERHEAD
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::{dequantize, quantize, Q_ONE};

    /// 归一化求和的容差。正是 lib.rs 与 tests/anti_sybil.rs 里用的 1e-6。
    const SUM_TOL: f64 = 1e-6;

    /// 旧格式 / 前缀格式的总长（`[0, 2)` 是前缀）。
    ///
    /// 两个长度落在模 6 的**不同同余类**（2 与 4），这是两格式互不误判的全部理由，
    /// 所以测试里把它们写成显式的同余断言，而不是只比较大小。
    fn prefixed_len_of(cap: usize) -> usize {
        2 + cap * 6 + 2
    }

    /// 断言「带前缀解码失败」且给出**具体**理由。
    fn expect_prefix_err(bytes: &[u8], want: DecodeError, what: &str) {
        assert_eq!(
            SparseView::decode_prefixed_result(bytes).err(),
            Some(want),
            "{what}：期望 {want:?}"
        );
        assert!(
            SparseView::decode_prefixed(bytes).is_none(),
            "{what}：decode_prefixed 也必须拒绝"
        );
    }

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

    /// 守：`normalized` 与 `normalized_slots` 在总质量 > 2^24 的极端视图上
    /// 给出**逐位相同**的 f32。
    ///
    /// 历史：commit `03f75eb` 之前，`normalized` 走 f64 除法而 `normalized_slots`
    /// 先把 `total_q` 转 f32 再除（在 2^24 以上会丢低位），两者差 1 ulp。
    /// 那会让「同一份视图、两条读数 API 位模式不同」，足以让逐位对拍在极端视图上失守。
    /// 已统一到 f64 路径（见 `normalized_slots` 的文档）。
    ///
    /// 构型：256 条满额 + 1 条 q=257 → total_q = 16777217 = 2^24 + 1，
    /// 而 `(16777217 as f32) = 16777216`（差 1）—— 这正是 f32 路径会出错的地方。
    #[test]
    fn normalized_与normalized_slots在极端总质量下逐位相同() {
        let mut pairs: Vec<(Acc, f32)> = (0..256u32).map(|i| (i, 1.0)).collect();
        pairs.push((256, dequantize(257)));
        let v = SparseView::from_scores(512, &pairs);
        assert_eq!(v.len(), 257);
        assert_eq!(v.total_q(), 16_777_217);
        assert_eq!(v.total_q() as f32, 16_777_216.0, "2^24+1 转 f32 会丢一位");

        // 两条读数 API 逐槽位必须位模式相同（257 条全部）。
        let ns = v.normalized_slots();
        let mut mismatched = 0usize;
        for &(acc, x) in &ns {
            if x.to_bits() != v.normalized(acc).to_bits() {
                mismatched += 1;
            }
        }
        assert_eq!(mismatched, 0, "两条读数 API 必须逐位相同");

        // 钉死具体位模式：走 f64 路径的正确舍入值。
        assert_eq!(v.normalized(0).to_bits(), 0x3B7F_FEFF);
        assert_eq!(ns[0].1.to_bits(), 0x3B7F_FEFF);
        assert_eq!(v.normalized(256).to_bits(), 0x3780_7FFF);
        assert_eq!(ns[256].1.to_bits(), 0x3780_7FFF);

        // Σv ≡ 1（两条路径都要满足）。
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
        // 两条路径现在误差也相同（都走 f64）。
        assert_eq!(
            (sum_n - 1.0).abs().to_bits(),
            (sum_ns - 1.0).abs().to_bits(),
            "两条路径误差必须一致"
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

    /// 守：`from_slots_unchecked` **不做任何校验**，直接采信调用方。
    ///
    /// 这个测试钉住的是「入口的契约边界」，不是缺陷：函数是私有的，唯一调用方
    /// `decode` 已在外层校验过长度/条数/升序/重复/零分/填充区，所以现状不产生漏洞。
    /// 但任何新增调用方都必须自己校验 —— 否则能造出「自家 decode 都拒绝」的负载，
    /// 也就是下面断言的那样。
    ///
    /// 文档曾经写成「会校验升序与 q > 0」，与实现不符；2026-11-18 已修正为
    /// 「不做校验 + 调用方契约」。本测试保留了当时构造的那个反例。
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

    // ═══════════════════ J. 带前缀编码（自描述 cap，解「网络分裂成孤岛」） ═══════════════════

    /// 前缀格式的字节布局逐字节钉死。
    ///
    /// 守的是「格式即协议」：`[0, 2)` 是 cap（小端 u16），`[2, 2+cap×6+2)` 是
    /// 旧格式的定长体，中间没有第二个头、没有填充、没有魔数。
    #[test]
    fn prefixed_encode_布局逐字节() {
        assert_eq!(PREFIX_BYTES, 2);

        // cap=1 + 一条 (9, 58982)：前缀 [1, 0] + 旧格式那 8 字节。
        let one = SparseView::from_scores(1, &[(9, 0.9)]);
        assert_eq!(
            one.encode_prefixed(),
            vec![1, 0, 9, 0, 0, 0, 102, 230, 1, 0]
        );
        assert_eq!(one.encode_prefixed().len(), prefixed_len_of(1));
        assert_eq!(one.prefixed_bytes(), prefixed_len_of(1));

        // cap=2 + [(1, 0.5), (2, 1.0)]：前缀 [2, 0] + 旧格式的 14 字节。
        let two = SparseView::from_scores(2, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(
            two.encode_prefixed(),
            vec![2, 0, 1, 0, 0, 0, 0, 128, 2, 0, 0, 0, 255, 255, 2, 0]
        );
        assert_eq!(two.encode_prefixed().len(), 16);

        // 空表：前缀之后全是零（尾部条数也是 0），长度仍是 cap×6+4。
        assert_eq!(
            SparseView::new(1).encode_prefixed(),
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(SparseView::new(1).encode_prefixed().len(), 10);

        // 前缀永远写 cap，与槽位内容无关（哪怕首账户号的低 16 位恰好也是 1）。
        let tricky = SparseView::from_scores(3, &[(1, 1.0)]);
        assert_eq!(&tricky.encode_prefixed()[..2], [3u8, 0]);
    }

    /// 往返：`encode_prefixed → decode_prefixed → encode_prefixed` 逐字节相同。
    ///
    /// 覆盖验收要求的 cap = 1 / 2 / 128 / 256 / 512 / 4096，每档都跑「空表 + 有内容」。
    #[test]
    fn prefixed_往返逐字节相同() {
        for &cap in &[1usize, 2, 128, 256, 512, MAX_CAP] {
            let n = if cap >= 256 { 200 } else { cap };
            let pairs: Vec<(Acc, f32)> = (0..n as u32)
                .map(|i| (i, (i + 1) as f32 / n as f32))
                .collect();
            let v = SparseView::from_scores(cap, &pairs);

            let bytes = v.encode_prefixed();
            assert_eq!(bytes.len(), prefixed_len_of(cap));
            assert_eq!(bytes.len(), v.prefixed_bytes());
            let back = SparseView::decode_prefixed(&bytes).expect("自产的前缀负载必须能解码");
            assert_eq!(back, v, "cap={cap} 往返后视图不相等");
            assert_eq!(back.cap(), cap, "cap={cap} 必须原样保留");
            assert_eq!(back.total_q(), v.total_q());
            assert_eq!(
                back.encode_prefixed(),
                bytes,
                "cap={cap} 往返后必须逐字节相同"
            );

            // 空表往返。
            let empty = SparseView::new(cap);
            let eb = empty.encode_prefixed();
            let eback = SparseView::decode_prefixed(&eb).expect("空前缀负载能解码");
            assert_eq!(eback, empty);
            assert_eq!(eback.encode_prefixed(), eb);
        }
    }

    /// 与旧版一致：同一份 `SparseView`，`encode()` 与 `encode_prefixed()[2..]` 逐字节相同。
    ///
    /// 守的是「新格式只是加了个头」：定长体一个字节都没动，所以 128 个现有测试、
    /// 12 张 wasm 金标准快照全部不受影响。
    #[test]
    fn prefixed_与旧编码只差前缀() {
        for &cap in &[1usize, 2, 7, 128, 256, 512, MAX_CAP] {
            let n = cap.min(64) as u32;
            let pairs: Vec<(Acc, f32)> = (0..n).map(|i| (i, 0.5)).collect();
            let views = [
                SparseView::new(cap),
                SparseView::from_scores(cap, &[(0, 1.0)]),
                SparseView::from_scores(cap, &[(1, 1.0), (65_536, 0.5), (u32::MAX, 0.25)]),
                SparseView::from_scores(cap, &pairs),
            ];
            for v in views {
                let old = v.encode();
                let new = v.encode_prefixed();
                assert_eq!(new.len(), old.len() + PREFIX_BYTES);
                assert_eq!(&new[PREFIX_BYTES..], &old[..], "定长体必须逐字节相同");
                assert_eq!(&new[..PREFIX_BYTES], (v.cap() as u16).to_le_bytes());
            }
        }
    }

    /// 前缀是**唯一**的 cap 来源：解码器必须真的采用它，而不是沿用某个默认值。
    #[test]
    fn prefixed_前缀的cap被真正采用() {
        for &cap in &[1usize, 2, 7, 128, 256, 512, MAX_CAP] {
            let view = SparseView::new(cap);
            let bytes = view.encode_prefixed();
            assert_eq!(
                SparseView::decode_prefixed(&bytes).map(|x| x.cap()),
                Some(cap),
                "cap={cap} 没有从前缀读回来"
            );
        }
        // 有内容的表也一样。
        let v = SparseView::from_scores(128, &[(1, 0.5), (2, 1.0)]);
        assert_eq!(
            SparseView::decode_prefixed(&v.encode_prefixed())
                .map(|x| (x.cap(), x.len()))
                .unwrap(),
            (128, 2)
        );
    }

    /// 跨 cap 必须**明确**拒绝，不是静默错解（验收第 2 条）。
    ///
    /// 旧格式需要带外 cap，错配只能靠长度不符拒掉；新格式把 cap 写进字节里，
    /// 「期望 cap」这个概念本身就不存在了——解码器只信前缀。两个方向都钉死。
    #[test]
    fn prefixed_跨cap明确拒绝() {
        let v256 = SparseView::from_scores(256, &[(1, 0.5), (2, 0.25)]);
        let bytes = v256.encode_prefixed();
        assert_eq!(bytes.len(), prefixed_len_of(256));

        // 1) 前缀解码只信前缀：cap 必须是 256，与「我以为的 512」无关。
        let back = SparseView::decode_prefixed(&bytes).expect("合法负载");
        assert_eq!(back.cap(), 256);
        assert_eq!(back, v256, "跨 cap 不该改变语义");
        assert_eq!(back.encode_prefixed(), bytes);

        // 2) 旧入口按 512 解：长度 1540 ≠ 512×6+2 ⟹ None（不是「解出一半」）。
        assert!(SparseView::decode(&bytes, 512).is_none(), "旧入口必须拒绝");
        assert!(
            SparseView::decode(&bytes, 256).is_none(),
            "前缀格式对旧入口一律不合法（长度多 2）"
        );

        // 3) 手工把前缀撒谎成 512（负载本体还是 256 的）⟹ 长度不符，明确 BadLength。
        let mut lie = bytes.clone();
        lie[..2].copy_from_slice(&512u16.to_le_bytes());
        expect_prefix_err(&lie, DecodeError::BadLength, "前缀撒谎成 512");

        // 4) 反向：512 的字节，前缀撒谎成 256。
        let v512 = SparseView::from_scores(512, &[(1, 0.5)]);
        let mut lie2 = v512.encode_prefixed();
        lie2[..2].copy_from_slice(&256u16.to_le_bytes());
        expect_prefix_err(&lie2, DecodeError::BadLength, "前缀撒谎成 256");
    }

    /// 截断：从 0 字节到「差 1 字节」的每一个前缀都必须被拒，且绝不 panic。
    #[test]
    fn prefixed_防御_任何截断都拒绝() {
        let v = SparseView::from_scores(4, &[(1, 0.5), (2, 0.25), (7, 1.0)]);
        let bytes = v.encode_prefixed();
        assert_eq!(bytes.len(), prefixed_len_of(4));
        for cut in 0..bytes.len() {
            let part = bytes.get(..cut).expect("cut 在范围内");
            assert_eq!(
                SparseView::decode_prefixed_result(part).err(),
                Some(DecodeError::BadLength),
                "截断到 {cut} 字节的理由不对"
            );
            assert!(
                SparseView::decode_prefixed(part).is_none(),
                "截断到 {cut} 字节被接受了"
            );
        }
        // 恰好少 1 字节：前缀完整、cap 合法，仍然必须 BadLength（不是 BadCap）。
        let almost = bytes.get(..bytes.len() - 1).unwrap();
        assert_eq!(
            SparseView::decode_prefixed_result(almost).err(),
            Some(DecodeError::BadLength)
        );
    }

    /// 尾部多余字节（0 或非零）一律拒绝：长度必须**恰好**相等，不是「前缀匹配就行」。
    #[test]
    fn prefixed_防御_尾部多余字节一律拒绝() {
        let v = SparseView::from_scores(2, &[(1, 0.5), (3, 0.25)]);
        let base = v.encode_prefixed();
        for extra in 1..=8usize {
            let mut longer = base.clone();
            longer.extend_from_slice(&[0u8; 8][..extra]);
            expect_prefix_err(
                &longer,
                DecodeError::BadLength,
                &format!("尾部多 {extra} 个零字节"),
            );

            let mut longer_nz = base.clone();
            longer_nz.extend_from_slice(&[0xFFu8; 8][..extra]);
            expect_prefix_err(
                &longer_nz,
                DecodeError::BadLength,
                &format!("尾部多 {extra} 个非零字节"),
            );
        }
        // 只多 1 个字节也必须拒绝（不是「多出来的忽略」）。
        let mut one_extra = base.clone();
        one_extra.push(0);
        assert_eq!(
            SparseView::decode_prefixed_result(&one_extra).err(),
            Some(DecodeError::BadLength)
        );
    }

    /// 撒谎的 cap：0、超出 MAX_CAP、超大 u16 —— 一律 BadCap，且绝不 panic / 越界读。
    #[test]
    fn prefixed_防御_撒谎的cap报bad_cap() {
        let v = SparseView::from_scores(4, &[(1, 0.5)]);
        let bytes = v.encode_prefixed();
        for bad_cap in [0u16, (MAX_CAP as u16) + 1, 4098, 65_534, u16::MAX] {
            let mut lie = bytes.clone();
            lie[..2].copy_from_slice(&bad_cap.to_le_bytes());
            expect_prefix_err(
                &lie,
                DecodeError::BadCap,
                &format!("前缀撒谎成 cap={bad_cap}"),
            );
        }
        // 边界：cap 恰好是 MAX_CAP 是**合法**前缀（只是长度对不上 → BadLength）。
        let mut edge = bytes.clone();
        edge[..2].copy_from_slice(&(MAX_CAP as u16).to_le_bytes());
        expect_prefix_err(&edge, DecodeError::BadLength, "cap=MAX_CAP 长度不符");
    }

    /// 随机字节：绝不 panic；一旦解成功，就必须能再编码回**同一串**（规范形式唯一）。
    #[test]
    fn prefixed_防御_随机字节绝不panic() {
        let mut rng = XorShift::new(0x1234_5678_9ABC_DEF0);
        for _ in 0..2000 {
            let len = rng.range(0, 300) as usize;
            let mut bytes = vec![0u8; len];
            for b in bytes.iter_mut() {
                *b = rng.next_u32() as u8;
            }
            match SparseView::decode_prefixed_result(&bytes) {
                Err(_) => {}
                Ok(v) => {
                    assert_eq!(
                        v.encode_prefixed(),
                        bytes,
                        "解出来的东西再编码必须逐字节相同"
                    );
                    assert!(v.cap() >= 1 && v.cap() <= MAX_CAP);
                    assert!(v.len() <= v.cap());
                    assert!(v.slots().iter().all(|&(_, q)| q > 0));
                }
            }
        }
    }

    /// 逐位置、逐掩码篡改：任何一位变化都只能是「拒绝」或「另一个合法负载」，绝不 panic。
    #[test]
    fn prefixed_防御_逐位置篡改不panic() {
        let v = SparseView::from_scores(4, &[(1, 0.5), (2, 0.25)]);
        let good = v.encode_prefixed();
        for (pos, &orig) in good.iter().enumerate() {
            for xor in [0x01u8, 0x80, 0xFF] {
                let mut bad = good.clone();
                bad[pos] = orig ^ xor;
                match SparseView::decode_prefixed_result(&bad) {
                    Err(e) => {
                        // 七个变体都在覆盖范围内，且必须是其中之一。
                        let known = matches!(
                            e,
                            DecodeError::BadCap
                                | DecodeError::BadLength
                                | DecodeError::BadCount
                                | DecodeError::NotAscending
                                | DecodeError::DuplicateAcc
                                | DecodeError::ZeroScore
                                | DecodeError::NonZeroPadding
                        );
                        assert!(known, "未知错误变体：{e:?}");
                    }
                    Ok(v2) => assert_eq!(v2.encode_prefixed(), bad),
                }
            }
        }
    }

    /// 同余类边角（前半）：旧**空表** `[0; 8]` 的首 2 字节是 `0x0000`，
    /// 前缀解码器把它读成 cap=0 ⟹ **BadCap**（而不是 BadLength）。
    ///
    /// 这条钉的是「拒绝理由的顺序」：先看 cap，再看长度。
    #[test]
    fn 同余类_旧空表被前缀解码器读成cap零() {
        for &cap in &[1usize, 2, 128, 256, 512, MAX_CAP] {
            let old = SparseView::new(cap).encode();
            assert_eq!(&old[..2], [0u8, 0], "cap={cap} 旧空表首两字节必须是 0");
            expect_prefix_err(&old, DecodeError::BadCap, "旧空表");
            // 旧格式总长 ≡ 2 (mod 6)，前缀格式 ≡ 4 (mod 6)。
            assert_eq!(old.len() % 6, 2);
            assert_eq!(prefixed_len_of(1) % 6, 4);
        }
    }

    /// 同余类边角（后半）：首账户号的**低 16 位恰好为 0**（如 acc=65536）的旧负载，
    /// 前缀解码器把 `[0, 2)` 读成 cap=0 ⟹ 同样落进 **BadCap**。
    ///
    /// 注意这不是「前 2 字节被赋予了语义」——两种格式真正的鉴别器是长度同余类，
    /// 这里只是恰好先撞上 cap 检查那一条。
    #[test]
    fn 同余类_首账户号低16位为零落进bad_cap() {
        for &acc in &[65_536u32, 131_072, 0x0001_0000, 0xFFFF_0000] {
            assert_eq!(acc & 0xFFFF, 0, "构造前提：低 16 位为 0");
            let cap = 2usize;
            let fixed = cap * SLOT_BYTES;
            // 手工造一条 cap=2、恰好一条槽 (acc, q=1) 的**合法**旧负载。
            let mut old = vec![0u8; fixed + VIEW_OVERHEAD];
            old[0..4].copy_from_slice(&acc.to_le_bytes());
            old[4..6].copy_from_slice(&1u16.to_le_bytes());
            old[fixed..fixed + 2].copy_from_slice(&1u16.to_le_bytes());
            // 旧解码器认这条（这是合法的规范形式）。
            let back = SparseView::decode(&old, cap).expect("手工构造的合法旧负载");
            assert_eq!(back.len(), 1);
            assert_eq!(back.slots(), [(acc, 1u16)]);
            assert_eq!(back.encode(), old);
            // 前缀解码器读 [0,2) = acc & 0xFFFF = 0 ⟹ cap=0 ⟹ BadCap。
            expect_prefix_err(&old, DecodeError::BadCap, "首账户号低 16 位为 0 的旧负载");
        }
    }

    /// 同余类边角（补）：**任何**旧格式负载喂给前缀解码器都必须被拒。
    ///
    /// 理由不是「前 2 字节的语义」，而是总长：旧 `6·C+2 ≡ 2 (mod 6)`、
    /// 新 `6·P+4 ≡ 4 (mod 6)`，模 6 下无解 ⟹ 长度检查必然拦下。
    #[test]
    fn 同余类_旧字节喂前缀解码器恒被拒() {
        for &cap in &[1usize, 2, 128, 256, 512, MAX_CAP] {
            // 用首账户号低 16 位非零的负载，好让 cap 检查先通过、走到长度检查。
            let views = [
                SparseView::from_scores(cap, &[(1, 0.5), (2, 1.0)]),
                SparseView::from_scores(cap, &[(1, 1.0), (u32::MAX, 0.25)]),
            ];
            for v in views {
                let old = v.encode();
                assert_eq!(old.len() % 6, 2, "旧格式总长必须 ≡ 2 (mod 6)");
                let p = u16::from_le_bytes([old[0], old[1]]) as usize;
                if p == 0 || p > MAX_CAP {
                    expect_prefix_err(&old, DecodeError::BadCap, "首账户号低 16 位非法");
                    continue;
                }
                // 前缀解码器读到的 cap 是 p；若长度真的对上，就说明同余类论证错了。
                assert_ne!(
                    prefixed_len_of(p),
                    old.len(),
                    "长度落进了两种格式的交集——同余类论证失效"
                );
                expect_prefix_err(&old, DecodeError::BadLength, "旧格式长度落不进新格式");
                assert!(SparseView::decode_prefixed(&old).is_none());
            }
        }
    }

    /// 同余类边角（补）：**任何**前缀格式负载喂给旧解码器都必须返回 None。
    #[test]
    fn 同余类_前缀字节喂旧解码器恒被拒() {
        for &cap in &[1usize, 2, 128, 256, 512, MAX_CAP] {
            let views = [
                SparseView::new(cap),
                SparseView::from_scores(cap, &[(1, 0.5), (2, 1.0)]),
            ];
            for v in views {
                let pre = v.encode_prefixed();
                assert_eq!(pre.len() % 6, 4, "前缀格式总长必须 ≡ 4 (mod 6)");
                // 没有任何合法 cap 能让 `cap×6+2` 等于这个长度。
                for try_cap in 1..=MAX_CAP {
                    assert_ne!(try_cap * SLOT_BYTES + VIEW_OVERHEAD, pre.len());
                }
                for &try_cap in &[1usize, 2, 128, 256, 512, MAX_CAP] {
                    assert!(
                        SparseView::decode(&pre, try_cap).is_none(),
                        "cap={cap} 的前缀负载被旧解码器以 try_cap={try_cap} 接受了"
                    );
                }
                // 去掉前缀之后**才是**旧格式该解的东西——这正说明前缀是唯一的区别。
                assert!(
                    SparseView::decode(&pre[PREFIX_BYTES..], cap).is_some(),
                    "剥掉前缀后必须能被旧解码器解出来"
                );
            }
        }
    }

    /// 两格式的总长同余类逐 cap 钉死：旧 ≡ 2、新 ≡ 4（mod 6），永不相交。
    #[test]
    fn 同余类_两格式总长模六分别为二与四() {
        for cap in 1..=MAX_CAP {
            let l_old = cap * SLOT_BYTES + VIEW_OVERHEAD;
            assert_eq!(l_old % 6, 2, "cap={cap} 旧格式总长不是 6n+2");
            assert_eq!(
                prefixed_len_of(cap) % 6,
                4,
                "cap={cap} 前缀格式总长不是 6n+4"
            );
            assert_eq!(prefixed_len_of(cap), l_old + PREFIX_BYTES);
        }
        // 6·C_old + 2 = 6·C_new + 4 ⟹ 6(C_old − C_new) = 2，无整数解。
        for &c_old in &[1usize, 2, 128, 256, 512, MAX_CAP] {
            for &c_new in &[1usize, 2, 128, 256, 512, MAX_CAP] {
                assert_ne!(
                    6 * c_old + 2,
                    6 * c_new + 4,
                    "c_old={c_old} c_new={c_new}：两种格式的总长撞上了"
                );
            }
        }
    }

    /// 可诊断入口与 `Option` 入口的判据必须逐条一致：`Err` ⟺ `None`。
    #[test]
    fn prefixed_result与option判据一致() {
        let mut cases: Vec<Vec<u8>> = Vec::new();
        let good = SparseView::from_scores(4, &[(1, 0.5), (2, 0.25)]).encode_prefixed();
        cases.push(good.clone());
        for cut in 0..good.len() {
            cases.push(good.get(..cut).unwrap().to_vec());
        }
        let mut longer = good.clone();
        longer.push(0);
        cases.push(longer);
        for bad_cap in [0u16, (MAX_CAP as u16) + 1, u16::MAX] {
            let mut b = good.clone();
            b[..2].copy_from_slice(&bad_cap.to_le_bytes());
            cases.push(b);
        }
        let mut rng = XorShift::new(0xABCD_1234);
        for _ in 0..500 {
            let len = rng.range(0, 60) as usize;
            let mut b = vec![0u8; len];
            for x in b.iter_mut() {
                *x = rng.next_u32() as u8;
            }
            cases.push(b);
        }
        for (i, bytes) in cases.iter().enumerate() {
            let res = SparseView::decode_prefixed_result(bytes);
            let opt = SparseView::decode_prefixed(bytes);
            assert_eq!(
                res.is_err(),
                opt.is_none(),
                "第 {i} 个用例：可诊断入口与 Option 入口判据不一致"
            );
            if let (Ok(a), Some(b)) = (&res, &opt) {
                assert_eq!(a, b);
            }
        }
    }

    /// `DecodeError` 的标签：七个变体都在、互不相同、`Display` 与 `as_str` 一致，
    /// 且实现了 `std::error::Error`。
    #[test]
    fn decode_error_七个变体标签互不相同() {
        let all = [
            DecodeError::BadCap,
            DecodeError::BadLength,
            DecodeError::BadCount,
            DecodeError::NotAscending,
            DecodeError::DuplicateAcc,
            DecodeError::ZeroScore,
            DecodeError::NonZeroPadding,
        ];
        assert_eq!(all.len(), 7);
        let mut seen: Vec<&str> = Vec::new();
        for e in all {
            let s = e.as_str();
            assert!(!s.is_empty(), "{e:?} 的标签不能为空");
            assert!(!s.contains(' '), "{e:?} 的标签不能含空格");
            assert_eq!(format!("{e}"), s, "Display 必须与 as_str 一致");
            assert!(!seen.contains(&s), "标签 {s} 重复了");
            seen.push(s);
            // 实现 std::error::Error（可被 ? / Box<dyn Error> 消费）。
            let _: &dyn std::error::Error = &e;
        }
        assert_eq!(seen.len(), 7);
    }

    /// 五条规范性检查（条数 / 升序 / 重复 / 零分 / 填充区）在前缀格式下各有**独立**错误码。
    ///
    /// 守的是「可诊断」：不是笼统的「解不出来」，而是能说清哪一条被违反。
    #[test]
    fn prefixed_每条规范检查都有独立错误码() {
        let cap = 2usize;
        let fixed = cap * SLOT_BYTES;
        let good = SparseView::from_scores(cap, &[(1, 0.5), (3, 0.25)]).encode_prefixed();
        assert_eq!(good.len(), prefixed_len_of(cap));
        let off = PREFIX_BYTES;

        // 1) 条数 > cap。
        let mut b = good.clone();
        b[off + fixed..off + fixed + 2].copy_from_slice(&3u16.to_le_bytes());
        expect_prefix_err(&b, DecodeError::BadCount, "条数超容量");

        // 2) 账户号降序：第 0 槽（1）改成比第 1 槽（3）大的 9。
        let mut b = good.clone();
        b[off..off + 4].copy_from_slice(&9u32.to_le_bytes());
        expect_prefix_err(&b, DecodeError::NotAscending, "乱序");

        // 3) 重复账户：第 1 槽改成和第 0 槽一样的 1。
        let mut b = good.clone();
        b[off + 6..off + 10].copy_from_slice(&1u32.to_le_bytes());
        expect_prefix_err(&b, DecodeError::DuplicateAcc, "重复账户");

        // 4) 零分槽。
        let mut b = good.clone();
        b[off + 4..off + 6].copy_from_slice(&0u16.to_le_bytes());
        expect_prefix_err(&b, DecodeError::ZeroScore, "q=0 的槽");

        // 5) 填充区夹带：条数改成 1，第 1 槽区留非零。
        let mut b = good.clone();
        b[off + fixed..off + fixed + 2].copy_from_slice(&1u16.to_le_bytes());
        b[off + 6] = 1;
        expect_prefix_err(&b, DecodeError::NonZeroPadding, "填充区夹带");
        // 同一份字节交给旧入口也拒绝（两条路径判据一致）。
        assert!(SparseView::decode(&b[off..], cap).is_none());
    }

    /// 满表 + 边界账户号：前缀里永远是 cap，不是账户号；4096 条的往返仍然逐字节闭合。
    #[test]
    fn prefixed_满表与边界账户号() {
        let pairs: Vec<(Acc, f32)> = (0..MAX_CAP as u32).map(|i| (i, 1.0)).collect();
        let v = SparseView::from_scores(MAX_CAP, &pairs);
        let bytes = v.encode_prefixed();
        assert_eq!(bytes.len(), 24_580, "4096×6+4 = 24580");
        let back = SparseView::decode_prefixed(&bytes).expect("满表必须能解");
        assert_eq!(back, v);
        assert_eq!(back.encode_prefixed(), bytes);

        // 首尾边界账户号同时出现：前缀写的是 cap（2），不是任何一种账户号。
        let e = SparseView::from_scores(2, &[(0, 1.0), (u32::MAX, 0.5)]);
        let eb = e.encode_prefixed();
        assert_eq!(&eb[..2], [2u8, 0], "前缀永远是 cap，与槽位内容无关");
        let eback = SparseView::decode_prefixed(&eb).expect("边界账户号负载能解");
        assert_eq!(eback, e);
        assert_eq!(eback.q_of(u32::MAX), 32_768);
        assert_eq!(eback.q_of(0), 65_535);
    }

    /// `prefixed_bytes` 恒为 `bytes + 2`，并钉死体积表（旧 770/1538/3074 → 新 772/1540/3076）。
    #[test]
    fn prefixed_bytes_恒为bytes加二() {
        for &(cap, old, new) in &[
            (1usize, 8usize, 10usize),
            (2, 14, 16),
            (128, 770, 772),
            (256, 1538, 1540),
            (512, 3074, 3076),
            (4096, 24578, 24580),
        ] {
            let v = SparseView::new(cap);
            assert_eq!(v.bytes(), old, "cap={cap} 旧格式体积");
            assert_eq!(v.prefixed_bytes(), new, "cap={cap} 前缀格式体积");
            assert_eq!(v.prefixed_bytes(), v.bytes() + PREFIX_BYTES);
            assert_eq!(v.encode().len(), old);
            assert_eq!(v.encode_prefixed().len(), new);
        }
    }

    /// 前缀编码同样是**逐位确定**的：同一集合不同到达顺序 ⟹ 逐字节相同。
    #[test]
    fn prefixed_逐位确定() {
        let pairs: Vec<(Acc, f32)> = (0..300u32).map(|i| (i, (i + 1) as f32 / 300.0)).collect();
        let a = SparseView::from_scores(128, &pairs);
        let mut rev = pairs.clone();
        rev.reverse();
        let b = SparseView::from_scores(128, &rev);
        assert_eq!(
            a.encode_prefixed(),
            b.encode_prefixed(),
            "逆序改变了前缀编码"
        );
        assert_eq!(a.encode_prefixed(), a.encode_prefixed(), "连跑两遍不相同");
        let back = SparseView::decode_prefixed(&a.encode_prefixed()).expect("合法");
        assert_eq!(back.encode_prefixed(), a.encode_prefixed());
        // 定长体也必须逐比特相同（前缀格式没有偷偷改本体）。
        assert_eq!(a.encode_prefixed()[PREFIX_BYTES..], a.encode()[..]);
    }

    /// 定长体解码器（私有）单独被调用时也不 panic，且给出准确的错误码。
    ///
    /// 它现在是旧格式与前缀格式**共用**的那一份实现，所以它的边界行为值得单独钉。
    #[test]
    fn decode_body_result_长度与整除检查() {
        // 连定长体的最小长度都不够。
        assert_eq!(
            SparseView::decode_body_result(&[]).err(),
            Some(DecodeError::BadLength)
        );
        // 恰好 2 字节 ⟹ fixed_len = 0 ⟹ cap = 0 ⟹ BadCap。
        assert_eq!(
            SparseView::decode_body_result(&[0, 0]).err(),
            Some(DecodeError::BadCap)
        );
        // 3 字节 ⟹ fixed_len = 1，除不尽 6 ⟹ BadLength。
        assert_eq!(
            SparseView::decode_body_result(&[0, 0, 0]).err(),
            Some(DecodeError::BadLength)
        );
        // 8 字节 = cap 1 的空表定长体 ⟹ 合法。
        assert_eq!(
            SparseView::decode_body_result(&[0u8; 8]).map(|v| (v.cap(), v.len())),
            Ok((1, 0))
        );
        // 10 字节 ⟹ fixed_len = 8，8/6 余 2 ⟹ BadLength（不是「猜个 cap 凑合解」）。
        assert_eq!(
            SparseView::decode_body_result(&[0u8; 10]).err(),
            Some(DecodeError::BadLength)
        );
        // 14 字节 = cap 2 的空表定长体 ⟹ 合法。
        assert_eq!(
            SparseView::decode_body_result(&[0u8; 14]).map(|v| v.cap()),
            Ok(2)
        );
        // 超过 MAX_CAP 的定长体（4097 槽）⟹ BadCap，不是越界分配。
        let too_big = vec![0u8; (MAX_CAP + 1) * SLOT_BYTES + VIEW_OVERHEAD];
        assert_eq!(
            SparseView::decode_body_result(&too_big).err(),
            Some(DecodeError::BadCap)
        );
    }

    /// 最小合法前缀负载 = 10 字节（cap=1 的空表）；只有前缀（2 字节）必被拒。
    #[test]
    fn prefixed_最小负载与空前缀() {
        let min = SparseView::new(1).encode_prefixed();
        assert_eq!(min.len(), 10);
        let back = SparseView::decode_prefixed(&min).expect("最小的前缀负载必须能解");
        assert_eq!(back, SparseView::new(1));
        assert_eq!(back.encode_prefixed(), min);

        // 只有前缀（2 字节）：cap 合法但长度不符 ⟹ BadLength。
        assert_eq!(
            SparseView::decode_prefixed_result(&min[..2]).err(),
            Some(DecodeError::BadLength)
        );
        // 只有 1 字节 ⟹ 连前缀都不完整 ⟹ BadLength。
        assert_eq!(
            SparseView::decode_prefixed_result(&min[..1]).err(),
            Some(DecodeError::BadLength)
        );
        // 空输入 ⟹ BadLength。
        assert_eq!(
            SparseView::decode_prefixed_result(&[]).err(),
            Some(DecodeError::BadLength)
        );
        // 9 字节（少 1）⟹ BadLength。
        assert_eq!(
            SparseView::decode_prefixed_result(&min[..9]).err(),
            Some(DecodeError::BadLength)
        );
    }

    /// 前缀不变式：`bytes[0..2] == cap.to_le_bytes()`，且与本体内容完全解耦。
    ///
    /// 守的是「头里只有 cap，没有第二个字段」——任何把别的元数据塞进前 2 字节的想法
    /// 都会立刻破坏这个等式。
    #[test]
    fn prefixed_前缀只装cap() {
        for &cap in &[1usize, 2, 3, 128, 255, 256, 257, 4096] {
            for n in [0usize, 1, 2] {
                let pairs: Vec<(Acc, f32)> = (0..n as u32).map(|i| (i, 0.5)).collect();
                let v = SparseView::from_scores(cap, &pairs);
                let bytes = v.encode_prefixed();
                assert_eq!(
                    &bytes[..PREFIX_BYTES],
                    &(cap as u16).to_le_bytes(),
                    "cap={cap} n={n} 的前缀不是 cap 的小端 u16"
                );
                // 换内容不改前缀，改 cap 才改前缀。
                let other = SparseView::new(cap);
                assert_eq!(
                    &other.encode_prefixed()[..PREFIX_BYTES],
                    &bytes[..PREFIX_BYTES]
                );
                // 前缀之后的第一个字节是定长体的槽位区（本测试里大部分是 0）。
                assert_eq!(bytes.len(), prefixed_len_of(cap));
            }
        }
    }

    /// 靠长度**猜** cap 必然猜错：两种格式的长度公式互为对方的「非整数解」。
    ///
    /// 这条把「为什么必须精确长度匹配」的两个方向都钉住：
    /// * 用旧公式 `(len−2)/6` 去套**前缀**负载：不能整除 ⟹ 猜出来的 cap 会让
    ///   长度检查把**合法**负载拒掉（可用性事故，不是安全漏洞，但同样致命）；
    /// * 用新公式 `(len−4)/6` 去套**旧**负载：同样不能整除。
    ///
    /// 换句话说：唯一安全的做法是让发送方/接收方**显式**声明格式并精确匹配长度，
    /// 而不是从长度反推格式。第三种格式出现时必须加带内魔数（见 ENCODING.md）。
    #[test]
    fn prefixed_靠长度猜cap必然猜错() {
        for cap in 1..=MAX_CAP {
            let l_old = cap * SLOT_BYTES + VIEW_OVERHEAD;
            let l_pre = prefixed_len_of(cap);
            // 前缀负载里没有一处能用旧公式整除。
            assert_ne!((l_pre - VIEW_OVERHEAD) % SLOT_BYTES, 0, "cap={cap}");
            // 旧负载里也没有一处能用新公式整除（「新公式」= (len − 2 − 2) / 6，
            // 减的是前缀 2 字节 + 尾部条数 2 字节）。
            assert_ne!(
                (l_old as isize - (PREFIX_BYTES + VIEW_OVERHEAD) as isize) % 6,
                0,
                "cap={cap} 旧长度能用 (len−4)/6 整除"
            );
            assert_ne!(l_old, l_pre);
        }
        // 具体数字：cap=256 的旧 1538 字节——(1538−2)/6 = 256 整数；
        // 但前缀负载 1540 字节：(1540−2)/6 = 256.33 ⟹ 猜出来只能凑合，长度对不上。
        assert_eq!((1538 - 2) / 6, 256);
        assert_eq!((1540 - 2) % 6, 2, "1540−2 除以 6 余 2，不能整除");
        assert_eq!((1540 - 4) / 6, 256, "前缀格式必须用 (len−4)/6");
        assert_ne!(prefixed_len_of(256), 1538);
    }

    /// 重复前缀、超容量条数、超长条数：前缀格式下的这几条常见误用都要被拒。
    #[test]
    fn prefixed_重复前缀与超容量条数都被拒绝() {
        let v = SparseView::from_scores(2, &[(1, 0.5), (3, 0.25)]);
        let base = v.encode_prefixed();

        // 1) 前缀被写了两遍：长度 6C+6 ⟹ 既不是 6C'+2 也不是 6C'+4 ⟹ BadLength。
        let doubled: Vec<u8> = {
            let mut out = Vec::with_capacity(base.len() + PREFIX_BYTES);
            out.extend_from_slice(&base[..PREFIX_BYTES]); // 前缀再抄一遍
            out.extend_from_slice(&base);
            out
        };
        // 注意：抄在前面的第二份前缀会覆盖 cap 语义之外的东西，长度才是判据。
        let mut fixed_doubled = base.clone();
        fixed_doubled.insert(0, base[0]);
        fixed_doubled.insert(1, base[1]);
        assert_eq!(fixed_doubled.len(), base.len() + 2);
        expect_prefix_err(&fixed_doubled, DecodeError::BadLength, "前缀写了两遍");
        assert_eq!(doubled.len(), base.len() + 2, "两份构造长度一致");

        // 2) 条数字段被改成 65535（远超 cap）⟹ BadCount。
        let fixed = 2 * SLOT_BYTES;
        let mut b = base.clone();
        b[PREFIX_BYTES + fixed..PREFIX_BYTES + fixed + 2].copy_from_slice(&65535u16.to_le_bytes());
        expect_prefix_err(&b, DecodeError::BadCount, "条数 65535");

        // 3) 条数 = cap+1 ⟹ BadCount（边界）。
        let mut b = base.clone();
        b[PREFIX_BYTES + fixed..PREFIX_BYTES + fixed + 2].copy_from_slice(&3u16.to_le_bytes());
        expect_prefix_err(&b, DecodeError::BadCount, "条数 cap+1");

        // 4) 条数恰好 = cap：没有填充区，正常解。
        let full = SparseView::from_scores(2, &[(1, 1.0), (2, 0.5)]);
        let fb = full.encode_prefixed();
        assert_eq!(SparseView::decode_prefixed(&fb).map(|x| x.len()), Some(2));
        // 旧入口对同一份前缀负载仍然 None（长度多 2）。
        assert!(SparseView::decode(&fb, 2).is_none());
    }
}
