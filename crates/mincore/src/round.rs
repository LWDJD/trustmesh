//! 轮次位图均匀抽样：一轮不放回、抽完重置、**没有退避**。
//!
//! # 为什么砍掉退避
//!
//! 退避（连败 n 次权重降为 `1/2^n`）看起来是「省带宽」，但它按定义**不是均匀抽样**：
//! 它把「谁在线」这个与信任无关的属性，变成了「谁的观点更容易被听见」。
//! 审计结论是净贡献为 0、相位偏置为负（24/7 在线者拿到 +14pp 的轮首曝光）。
//! 这里把它整条拿掉：连败计数照记（运维要知道谁失联了），但**抽样不看它**。
//!
//! # 位图怎么做均匀抽样
//!
//! 设本轮还没抽过的位置集合为 `U`（`|U| = remaining`）。每次取 `j ∈ [0, remaining)`
//! 均匀随机，再取 `U` 里第 `j` 个元素——这是**精确**的均匀分布（不是拒绝采样，
//! 也没有「抽到重复就重抽」），且一轮内绝不重复。
//!
//! 位图的第 `i` 位为 1 表示「位置 `i` 本轮已抽过」。最后一个 `u64` 的越界位
//! 预置为 1，于是 `!word` 的 `count_ones` 恰好就是该字的空闲数，
//! 「第 j 个 0 位」可以整字跳过，不用逐位扫。
//!
//! 单次抽取是 `O(n/64)`；对 `n = 1600` 就是 25 个字 + 最多 64 位的内部扫描。
//! 整轮全取（`draw_all`）是一次线性扫描，不消耗随机数。

use crate::list::{Acc, PositiveList};

/// 确定性伪随机源（xorshift64*）。零依赖，只用于抽样。
///
/// 同一种子必然给出同一序列；与 `std` 的 `HashMap` 随机种子无关。
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// 用种子构造。种子为 0 也安全（内部强制非零）。
    #[must_use]
    pub fn new(seed: u64) -> Self {
        // 任何种子都映到非零奇数：xorshift 的不动点只有 0。
        Self {
            state: (seed ^ 0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// 下一个 u64。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, n)` 上的均匀整数；`n == 0` 返回 0。
    ///
    /// 用取模而不是拒绝采样：`n ≤ 4096`、模数 `2^64`，偏差量级 `n/2^64 < 2^-52`，
    /// 比任何可观测效应都小；而拒绝采样会让「抽了多少个随机数」依赖数据，
    /// 反而更难对照。
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

/// 轮次位图抽样器。
#[derive(Clone, Debug, Default)]
pub struct RoundBitmap {
    /// 快照：本轮开始时正分清单的账户号（升序）。
    order: Vec<Acc>,
    /// 已抽位图；长度 = ceil(order.len() / 64)。
    drawn: Vec<u64>,
    /// 本轮还剩几个没抽。
    remaining: usize,
    /// 快照对应的清单版本号。
    version: u64,
    /// 是否已经建过快照（区分「空清单」和「还没同步」）。
    ready: bool,
}

impl RoundBitmap {
    /// 空抽样器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 本轮还剩几个没抽。
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining
    }

    /// 本轮一共有多少个候选。
    #[must_use]
    pub fn round_len(&self) -> usize {
        self.order.len()
    }

    /// 清单变了就重建快照并开新一轮；没变就什么也不做。
    fn sync(&mut self, list: &PositiveList) {
        if self.ready && self.version == list.version() && self.order.len() == list.len() {
            return;
        }
        self.order = list.indices();
        self.version = list.version();
        self.ready = true;
        self.reset();
    }

    /// 开新一轮：清空位图，全部候选重新可用。
    ///
    /// 空清单时 `remaining = 0`，`draw` 会安全地返回空向量。
    pub fn reset(&mut self) {
        let n = self.order.len();
        let words = n.div_ceil(64);
        self.drawn = vec![0u64; words];
        // 越界位预置为 1：这样 `!word` 里只有合法位为 1。
        if let Some(last) = self.drawn.last_mut() {
            let used = n % 64;
            if used != 0 {
                *last = u64::MAX << used;
            }
        }
        self.remaining = n;
    }

    /// 取第 `j` 个空闲位（0 起算）。调用前必须保证 `j < remaining`。
    fn select_free(&self, mut j: usize) -> usize {
        for (wi, &word) in self.drawn.iter().enumerate() {
            let free = (!word).count_ones() as usize;
            if j >= free {
                j -= free;
                continue;
            }
            let mut bits = !word;
            loop {
                let b = bits.trailing_zeros() as usize;
                if j == 0 {
                    return wi * 64 + b;
                }
                j -= 1;
                bits &= bits - 1; // 清掉最低位的 1
            }
        }
        // 走到这里说明位图与 remaining 不一致，属于内部不变量被破坏。
        // 不 panic：返回最后一个位置，最坏情况是抽样略有偏差，不会崩。
        self.order.len().saturating_sub(1)
    }

    /// 均匀抽 `k` 个（本轮内不放回）。`k` 超过本轮剩余时只返回剩余的。
    ///
    /// 抽完最后一个会自动开新一轮（下一次调用从新的一轮开始）。
    pub fn draw(&mut self, list: &PositiveList, k: usize, rng: &mut Rng) -> Vec<Acc> {
        self.sync(list);
        if self.remaining == 0 {
            if self.order.is_empty() {
                return Vec::new();
            }
            self.reset();
        }
        let take = k.min(self.remaining);
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            let j = rng.below(self.remaining);
            let pos = self.select_free(j);
            self.drawn[pos / 64] |= 1u64 << (pos % 64);
            self.remaining -= 1;
            out.push(self.order[pos]);
        }
        out
    }

    /// 整轮全取：把本轮剩下的**全部**候选按账户号升序返回，并把本轮抽空。
    ///
    /// 用于「启动时快速刷新」：不消耗随机数、结果完全确定、一次覆盖一整轮。
    /// 本轮已经抽空时返回空向量（不会自动开新一轮——那会让「快速刷新」
    /// 变成「再刷一遍」，语义不清楚）。
    pub fn draw_all(&mut self, list: &PositiveList) -> Vec<Acc> {
        self.sync(list);
        let mut out = Vec::with_capacity(self.remaining);
        for wi in 0..self.drawn.len() {
            let mut bits = !self.drawn[wi];
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                let pos = wi * 64 + b;
                if pos < self.order.len() {
                    out.push(self.order[pos]);
                }
                bits &= bits - 1;
            }
        }
        for w in self.drawn.iter_mut() {
            *w = u64::MAX;
        }
        self.remaining = 0;
        out.sort_unstable();
        out
    }
}
