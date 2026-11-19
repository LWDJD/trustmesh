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
    ///
    /// # 为什么不是 `(seed ^ C) | 1`
    ///
    /// 早期实现用 `(seed ^ C) | 1` —— 既避开 xorshift 的不动点 0，又保证状态非零。
    /// 但那个 `| 1` **把种子的最低位吃掉了**：`seed = 2k` 与 `2k+1` 得到完全相同的状态，
    /// 于是「相邻 seed」给出同一串随机数，**有效种子空间从 2^64 缩到 2^63**。
    ///
    /// 实测后果：`seed 0..64` 只产生 **32 个**不同的首值（应为 64）。
    /// 更隐蔽的是 `seed 1..=20` 这种「看起来 20 个独立样本」的用法 ——
    /// 实际只有 **11 个**独立状态，会让标准差被低估、置信区间过窄。
    ///
    /// 现在的做法：先用 SplitMix64 的混合函数（双射，`u64 -> u64` 一一对应）
    /// 把种子铺开，再确保非零。混合函数是双射，所以**不同的 seed 一定得到不同的 state**。
    #[must_use]
    pub fn new(seed: u64) -> Self {
        // SplitMix64 的 finalizer：雪崩性好，且是双射（可逆），不会把两个种子映到同一状态。
        let mut x = seed;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        // 唯一例外：双射把某个种子映到 0 时（seed 使混合结果为 0），xorshift 会卡在不动点。
        // 用 | 1 兜底，但**此时状态的最低位本来就被强制**——代价是恰好 2 个种子里有 1 个被改，
        // 而不是所有种子都丢一位。
        Self {
            state: if x == 0 { 0x9E37_79B9_7F4A_7C15 } else { x },
        }
    }

    /// 从已保存的内部状态原样恢复（供快照使用）。
    ///
    /// # 调用方契约
    ///
    /// `state` 必须是 [Self::new] 产出过的状态，且**非零**：xorshift 的状态 0 是
    /// 不动点，`next_u64` 会永远返回 0、`below` 永远返回 0（抽样退化成「永远抽第 0 个」）。
    /// [`crate::snapshot::NodeSnapshot::decode`] 把 0 当作非法负载直接拒绝。
    pub(crate) fn from_state(state: u64) -> Self {
        Self { state }
    }

    /// 内部状态（供快照原样搬运）。
    pub(crate) fn state(&self) -> u64 {
        self.state
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

    /// 本轮快照里的账户号（升序）。
    pub(crate) fn order(&self) -> &[Acc] {
        &self.order
    }

    /// 已抽位图（最后一字的越界位恒为 1）。
    pub(crate) fn drawn(&self) -> &[u64] {
        &self.drawn
    }

    /// 快照对应的清单版本号。
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// 是否已建过快照（区分「空清单」和「还没同步」）。
    pub(crate) fn ready(&self) -> bool {
        self.ready
    }

    /// 由五个字段直接重建，并逐条校验内部不变量（供快照恢复使用）。
    ///
    /// 返回 `None` 表示这份组合在任何时刻都不可能出现——被截断或伪造的负载。
    /// 校验项（全部是 [`Self::draw`] / [`Self::draw_all`] / [`Self::reset`]
    /// 会维持的不变量）：
    ///
    /// * `order` 按账户号**严格升序**（它本来来自 `PositiveList::indices()`）；
    /// * `drawn` 的字数恰为 `ceil(order.len() / 64)`；
    /// * 最后一字的**越界位全为 1**（[Self::reset] 预置的，整字跳过靠它）；
    /// * `remaining` 等于合法位里的空闲位数，于是自动 `remaining <= order.len()`；
    /// * `ready == false` 时五个字段必须全是 [Self::new] 的默认值。
    pub(crate) fn from_parts(
        order: Vec<Acc>,
        drawn: Vec<u64>,
        remaining: usize,
        version: u64,
        ready: bool,
    ) -> Option<Self> {
        if !order.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        let n = order.len();
        if drawn.len() != n.div_ceil(64) {
            return None;
        }
        let used = n % 64;
        if used != 0 {
            let last = *drawn.last()?;
            let out_of_range = u64::MAX << used;
            if last & out_of_range != out_of_range {
                return None;
            }
        }
        // 越界位被强制为 1，所以整张位图的 0 位数恰是「合法区里还能抽的位置数」。
        let free: usize = drawn.iter().map(|&w| (!w).count_ones() as usize).sum();
        if free != remaining {
            return None;
        }
        // 「还没同步」时五个字段必须全是默认值。
        // 写法用 `!(ready || ...)` 而不是 `!ready && !(...)`：两者逐值等价，
        // 后者会被 clippy 的 nonminimal_bool 判为可简化。
        let default_sampler = !ready && n == 0 && remaining == 0 && version == 0;
        if !ready && !default_sampler {
            return None;
        }
        Some(Self {
            order,
            drawn,
            remaining,
            version,
            ready,
        })
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
    ///
    /// `k == 0` 是**纯查询**：返回空、不做任何状态变更（不 sync、不 reset、不吃随机数）。
    /// 否则「本轮已抽空」时调一次 `draw(0)` 会白开一轮，把 `remaining` 从 0 跳回 `round_len` ——
    /// 那会让调用方的「本轮抽完了吗」判断失效。
    pub fn draw(&mut self, list: &PositiveList, k: usize, rng: &mut Rng) -> Vec<Acc> {
        if k == 0 {
            return Vec::new();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::list::PositiveList;

    /// 造一份 n 条等权（0.5）的清单，账户号 0..n。
    fn list_of(n: usize) -> PositiveList {
        let mut l = PositiveList::new();
        for i in 0..n as u32 {
            l.set(i, 0.5, 0);
        }
        l
    }

    /// 去重后是否仍然 n 个（用来判「一轮内没重复」）。
    fn distinct(v: &[Acc]) -> Vec<Acc> {
        let mut c = v.to_vec();
        c.sort_unstable();
        c.dedup();
        c
    }

    // ══════════════════════════ Rng ══════════════════════════

    /// 守：同一个种子给出同一条序列（逐位钉住，防止有人改常数破坏确定性）。
    #[test]
    fn t01_rng_同种子同序列() {
        for seed in [0u64, 1, 7, 42, u64::MAX] {
            let mut a = Rng::new(seed);
            let mut b = Rng::new(seed);
            for step in 0..16 {
                let (x, y) = (a.next_u64(), b.next_u64());
                assert_eq!(x, y, "seed = {seed}，第 {step} 个数不一致");
            }
        }
        // seed = 42 的前 5 个 u64 位模式（实跑钉住，改 xorshift 常数会立刻红）。
        let mut r = Rng::new(42);
        let got: [u64; 5] = [
            r.next_u64(),
            r.next_u64(),
            r.next_u64(),
            r.next_u64(),
            r.next_u64(),
        ];
        assert_eq!(
            got,
            [
                0xD75F_B1E8_6299_B002,
                0x8213_F6CA_8439_876E,
                0x15BC_4499_6652_71A8,
                0x9140_4E1B_0070_C26B,
                0x3EFD_D40D_9132_6EAA,
            ],
            "seed 42 的随机序列变了——这会破坏全部确定性判据"
        );
    }

    /// 守：below(n) 落在 [0, n)，且 n == 0 时返回 0 而**不**消耗随机数。
    #[test]
    fn t02_below_范围与零元() {
        let mut r = Rng::new(9);
        for n in [1usize, 2, 3, 17, 64, 4096] {
            for _ in 0..64 {
                assert!(r.below(n) < n, "below({n}) 越界");
            }
        }
        // n == 0：返回 0，且不推进随机流。
        let mut a = Rng::new(7);
        assert_eq!(a.below(0), 0);
        let after_zero = a.next_u64();
        let mut b = Rng::new(7);
        assert_eq!(b.next_u64(), after_zero, "below(0) 不该推进随机流");

        // n == 1：会消耗一个随机数（实现是先取再取模）。
        let mut c = Rng::new(7);
        assert_eq!(c.below(1), 0);
        let mut d = Rng::new(7);
        d.next_u64();
        assert_eq!(
            c.next_u64(),
            d.next_u64(),
            "below(1) 应当正好消耗一个随机数"
        );
    }

    /// 守：**不同的种子给出不同的序列**（不能两根相邻 seed 撞同一串）。
    ///
    /// 历史：早期实现是 `state = (seed ^ C) | 1`。那个 `| 1` **吃掉了种子的最低位**：
    /// `2k` 与 `2k+1` 得到完全相同的状态，于是相邻 seed 给出同一串随机数，
    /// **有效种子空间从 2^64 缩到 2^63**。
    ///
    /// 为什么这是真 bug：调用方会写 `for seed in 1..=20 { ... }` 当作「20 个独立样本」，
    /// 实际只有 11 个独立状态 —— 标准差被低估、置信区间过窄，而**没有任何报错**。
    ///
    /// 现在是 SplitMix64 finalizer（双射混合）后接 `if x == 0 { 兜底 }`，
    /// 所以不同种子一定映射到不同状态。
    #[test]
    fn t03_rng_相邻种子不碰撞() {
        // 任何两个不同的 seed 都不该给出同一条序列。
        for pair in [(0u64, 1u64), (42, 43), (100, 101), (u64::MAX - 1, u64::MAX)] {
            let mut a = Rng::new(pair.0);
            let mut b = Rng::new(pair.1);
            let (x, y) = (a.next_u64(), b.next_u64());
            assert_ne!(
                x, y,
                "({}.0, {}.1) 给出了相同的首值 —— 种子最低位被吞掉是 bug",
                pair.0, pair.1
            );
        }
        // 更强的判据：0..64 这 64 个种子必须产生 64 个不同的首值（曾经只有 32 个）。
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64u64 {
            let mut r = Rng::new(seed);
            seen.insert(r.next_u64());
        }
        assert_eq!(seen.len(), 64, "0..64 这 64 个种子应有 64 个不同的首值");
    }

    // ══════════════════════════ 确定性 ══════════════════════════

    /// 守：同 seed、同清单 ⟹ 抽样序列**逐位**相同；两次独立构造的抽样器逐元素相等。
    #[test]
    fn t04_draw_同种子同清单同序列() {
        let l = list_of(10);
        let mut x = RoundBitmap::new();
        let mut y = RoundBitmap::new();
        let mut rx = Rng::new(42);
        let mut ry = Rng::new(42);
        // 跨两轮、每轮拆成多次调用，全程比较。
        for _ in 0..2 {
            for k in [3usize, 2, 5] {
                assert_eq!(x.draw(&l, k, &mut rx), y.draw(&l, k, &mut ry));
                assert_eq!(x.remaining(), y.remaining());
            }
        }
    }

    /// 守：seed = 42、清单 = 10 条等权（账户 0..9）时的**具体**抽样序列。
    ///
    /// 这组值是实跑得到的：同 seed 同清单 ⟹ 同序列的判据若被破坏，这条会立刻红。
    #[test]
    fn t05_draw_具体序列钉住() {
        let l = list_of(10);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(42);
        assert_eq!(s.draw(&l, 10, &mut rng), vec![8, 2, 0, 9, 6, 1, 4, 3, 5, 7]);
        assert_eq!(s.remaining(), 0);
        // 抽空之后自动开新一轮。
        assert_eq!(s.draw(&l, 3, &mut rng), vec![5, 4, 7]);
        assert_eq!(s.remaining(), 7);
        assert_eq!(s.draw_all(&l), vec![0, 1, 2, 3, 6, 8, 9]);
        assert_eq!(s.remaining(), 0);
        // 再来一轮。
        assert_eq!(s.draw(&l, 4, &mut rng), vec![9, 8, 2, 3]);
        assert_eq!(s.remaining(), 6);
    }

    // ══════════════════════════ 不放回 / 抽空 ══════════════════════════

    /// 守：一轮内绝不重复；抽完 round_len 个后 remaining == 0；覆盖全部候选。
    #[test]
    fn t06_draw_一轮内不重复() {
        for n in [2usize, 3, 63, 64, 65] {
            let l = list_of(n);
            let mut s = RoundBitmap::new();
            let mut rng = Rng::new(2024);
            let got = s.draw(&l, n, &mut rng);
            assert_eq!(got.len(), n);
            assert_eq!(distinct(&got).len(), n, "n = {n} 时一轮内出现重复");
            assert_eq!(s.remaining(), 0);
            // 覆盖的必须是全部候选（0..n），一条不漏。
            assert_eq!(distinct(&got), (0..n as u32).collect::<Vec<_>>());
        }
    }

    /// 守：k 超过本轮剩余时只返回剩余的（不补抽、不跨轮、不 panic）。
    #[test]
    fn t07_draw_k超过剩余只返回剩余() {
        let l = list_of(5);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(11);
        let first = s.draw(&l, 3, &mut rng);
        assert_eq!(first.len(), 3);
        assert_eq!(s.remaining(), 2);
        let over = s.draw(&l, 99, &mut rng);
        assert_eq!(over.len(), 2, "剩余 2 个时 k = 99 只该拿到 2 个");
        assert_eq!(s.remaining(), 0, "抽空即耗尽，不会跨轮补数");
        // 第一段和第二段合起来仍是一轮的不重复全集。
        let all: Vec<Acc> = first.iter().chain(over.iter()).copied().collect();
        assert_eq!(distinct(&all), (0..5u32).collect::<Vec<_>>());
        // 下一次调用才开新一轮。
        let next = s.draw(&l, 2, &mut rng);
        assert_eq!(next.len(), 2);
        assert_eq!(s.remaining(), 3);
    }

    /// 守：一轮抽空后，**下一次 draw** 自动重置；round_len 不变。
    #[test]
    fn t08_draw_抽空自动开新一轮() {
        let l = list_of(4);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(5);
        let r1 = s.draw(&l, 4, &mut rng);
        assert_eq!(r1.len(), 4);
        assert_eq!(s.remaining(), 0);
        assert_eq!(s.round_len(), 4);
        let r2 = s.draw(&l, 4, &mut rng);
        assert_eq!(r2.len(), 4);
        assert_eq!(distinct(&r2).len(), 4);
        assert_eq!(s.remaining(), 0);
        // 两轮各是一次完整覆盖。
        assert_eq!(distinct(&r1), (0..4u32).collect::<Vec<_>>());
        assert_eq!(distinct(&r2), (0..4u32).collect::<Vec<_>>());
    }

    /// 守：`draw(0)` 是**纯查询** —— 返回空、不消耗候选、不改变任何状态。
    ///
    /// 历史：早期实现先 sync/重置、再算 `take = k.min(remaining)`，所以在本轮已抽空时
    /// 调一次 `draw(0)` 会**白开一轮**（`remaining` 从 0 跳回 `round_len`）。
    /// 那会让调用方的「本轮抽完了吗」判断失效 —— 这属于「要 0 个却有副作用」，已修。
    #[test]
    fn t09_draw_k为零是纯查询() {
        let l = list_of(5);
        let mut fresh = RoundBitmap::new();
        let mut rng = Rng::new(3);
        // 从没 draw 过：draw(0) 不该触发 sync/reset（remaining 仍是初始的 0，不是 5）。
        assert!(fresh.draw(&l, 0, &mut rng).is_empty(), "k = 0 返回空向量");
        assert_eq!(fresh.remaining(), 0, "k = 0 不触发 sync/reset");
        assert_eq!(fresh.round_len(), 0);

        // 抽空之后再来一次 k = 0：状态完全不变（不会重新开一轮）。
        let drained = fresh.draw(&l, 5, &mut rng);
        assert_eq!(drained.len(), 5);
        assert_eq!(fresh.remaining(), 0);
        assert!(fresh.draw(&l, 0, &mut rng).is_empty());
        assert_eq!(fresh.remaining(), 0, "k = 0 在抽空的本轮上也不重置");
    }

    /// 守：空清单上 draw / draw_all 都安全返回空向量，remaining == 0、round_len == 0。
    #[test]
    fn t10_空清单安全返回空() {
        let empty = PositiveList::new();
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(1);
        assert!(s.draw(&empty, 3, &mut rng).is_empty());
        assert_eq!(s.remaining(), 0);
        assert_eq!(s.round_len(), 0);
        assert!(s.draw(&empty, 0, &mut rng).is_empty());
        assert_eq!(s.remaining(), 0);
        assert!(s.draw_all(&empty).is_empty());
        assert_eq!(s.remaining(), 0);
        // 清单从空变非空之后，下一次 draw 会同步并正常抽。
        let l = list_of(3);
        assert_eq!(s.draw(&l, 2, &mut rng).len(), 2);
        assert_eq!(s.round_len(), 3);
        assert_eq!(s.remaining(), 1);
    }

    // ══════════════════════════ sync / 清单变化 ══════════════════════════

    /// 守：清单**结构变化**（增删）会重建快照并开新一轮；重建后不会抽到已删除的账户。
    #[test]
    fn t11_sync_增删清单时重建() {
        let mut l = list_of(6);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(5);
        assert_eq!(s.draw(&l, 2, &mut rng), vec![5, 0]);
        assert_eq!(s.remaining(), 4);
        assert_eq!(s.round_len(), 6);

        // 新增 9 号：结构变化 ⟹ 重建 + 新一轮（remaining 回到 7）。
        l.set(9, 0.5, 0);
        assert_eq!(s.draw(&l, 2, &mut rng), vec![3, 1]);
        assert_eq!(s.remaining(), 5);
        assert_eq!(s.round_len(), 7, "快照必须包含新账户");

        // 删除 0 号：结构变化 ⟹ 重建，旧快照里的 0 号不能再出现。
        l.set(0, 0.0, 0);
        let got = s.draw(&l, 6, &mut rng);
        assert_eq!(got.len(), 6);
        assert_eq!(s.remaining(), 0);
        assert!(!got.contains(&0), "0 号已被删除，不该从旧快照抽到");
        assert_eq!(distinct(&got), vec![1, 2, 3, 4, 5, 9]);
    }

    /// 守：只改分数（不动结构）**不会**重建快照，本轮剩余数保持不变。
    #[test]
    fn t12_sync_改分不重建() {
        let mut l = list_of(4);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(5);
        assert_eq!(s.draw(&l, 2, &mut rng), vec![3, 1]);
        assert_eq!(s.remaining(), 2);
        let version_before = l.version();
        l.set(1, 0.9, 7); // 改分，版本不变
        assert_eq!(l.version(), version_before);
        let rest = s.draw(&l, 2, &mut rng);
        assert_eq!(rest, vec![2, 0], "改分不该打断本轮");
        assert_eq!(s.remaining(), 0);
        assert_eq!(s.round_len(), 4);
    }

    /// 守：清单变化后快照里的账户号仍然严格升序（浮点/遍历确定性的基础）。
    #[test]
    fn t13_sync_快照按账户号升序() {
        let mut l = PositiveList::new();
        for a in [1000u32, 3, 77, 5] {
            l.set(a, 1.0, 0);
        }
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(3);
        let got = s.draw(&l, 4, &mut rng);
        assert_eq!(distinct(&got), vec![3, 5, 77, 1000]);
        // draw_all 输出是账户号升序的。
        let mut s2 = RoundBitmap::new();
        assert_eq!(s2.draw_all(&l), vec![3, 5, 77, 1000]);
    }

    /// 守：draw_all 取完本轮剩下的全部候选，按账户号升序，且本轮变空；
    /// 再调用一次返回空（**不**自动开新一轮）。
    #[test]
    fn t14_draw_all_取完剩余且不自动重开() {
        let l = list_of(10);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(42);
        assert_eq!(s.draw(&l, 3, &mut rng), vec![8, 2, 0]);
        assert_eq!(s.remaining(), 7);
        let rest = s.draw_all(&l);
        assert_eq!(rest.len(), 7);
        assert!(rest.windows(2).all(|w| w[0] < w[1]), "draw_all 必须升序");
        assert!(!rest.contains(&8) && !rest.contains(&2) && !rest.contains(&0));
        assert_eq!(s.remaining(), 0);
        assert!(s.draw_all(&l).is_empty(), "本轮已空，draw_all 返回空");
        assert_eq!(s.remaining(), 0, "draw_all 不会自动开新一轮");
    }

    /// 守：`draw(k)` 恰好消耗 k 个随机数；`draw_all` 一个都不消耗。
    ///
    /// 用「镜像随机源跳过 k 步」的办法量消耗量：若实现多吞或少吞随机数，
    /// 后续序列立刻分叉。这条同时守住 `draw_all` 的「不消耗随机数」承诺
    /// （它的签名里根本没有 `&mut Rng`，这里把它钉成可回归的行为）。
    #[test]
    fn t15_draw消耗随机数_draw_all不消耗() {
        let l = list_of(8);
        for k in [1usize, 2, 3] {
            let mut s = RoundBitmap::new();
            let mut rng = Rng::new(77);
            let got = s.draw(&l, k, &mut rng);
            assert_eq!(got.len(), k);
            // 镜像：同 seed 跳过 k 个随机数，之后的流必须与 s 的 rng 一致。
            let mut mirror = Rng::new(77);
            for _ in 0..k {
                let _ = mirror.next_u64();
            }
            assert_eq!(
                rng.next_u64(),
                mirror.next_u64(),
                "draw({k}) 应当正好消耗 {k} 个随机数"
            );
        }

        // draw_all 消耗 0 个：先 draw(2)（吃 2 个）再 draw_all，总消耗仍是 2。
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(77);
        assert_eq!(s.draw(&l, 2, &mut rng), vec![0, 5]);
        let rest = s.draw_all(&l);
        assert_eq!(rest.len(), 6);
        let mut mirror = Rng::new(77);
        let _ = mirror.next_u64();
        let _ = mirror.next_u64();
        assert_eq!(rng.next_u64(), mirror.next_u64(), "draw_all 不该消耗随机数");
    }

    /// 守：位图布局——最后一个 u64 的越界位预置为 1，合法位为 0；
    /// 于是 !word 的 count_ones 恰好等于该字的空闲数（select_free 的前提）。
    #[test]
    fn t16_位图越界位预置为1() {
        for (n, words, last) in [
            (1usize, 1usize, u64::MAX << 1),
            (63, 1, u64::MAX << 63),
            (64, 1, 0),
            (65, 2, u64::MAX << 1),
            (129, 3, u64::MAX << 1),
            (192, 3, 0),
        ] {
            let l = list_of(n);
            let mut s = RoundBitmap::new();
            s.sync(&l);
            assert_eq!(s.drawn.len(), words, "n = {n} 的位图字数");
            assert_eq!(s.drawn.last().copied(), Some(last), "n = {n} 的哨兵位");
            assert_eq!(
                s.drawn[..words - 1].iter().sum::<u64>(),
                0,
                "n = {n} 的前面几字必须全零"
            );
            assert_eq!(s.remaining(), n);
            assert_eq!(s.round_len(), n);
        }
    }

    /// 守：select_free(0) 取到最小的空闲位置，select_free(n-1) 取到最大的。
    #[test]
    fn t17_select_free_首尾() {
        for n in [1usize, 2, 63, 64, 65, 130] {
            let l = list_of(n);
            let mut s = RoundBitmap::new();
            s.sync(&l);
            assert_eq!(s.select_free(0), 0, "n = {n}");
            assert_eq!(s.select_free(n - 1), n - 1, "n = {n}");
        }
        // 标记第 0 位之后，第 0 个空闲位变成 1。
        let l = list_of(4);
        let mut s = RoundBitmap::new();
        s.sync(&l);
        s.drawn[0] |= 1;
        assert_eq!(s.select_free(0), 1);
        assert_eq!(s.select_free(2), 3);
    }

    /// 守：reset() 把本轮全部候选放回：remaining == round_len，位图只剩下越界哨兵。
    #[test]
    fn t18_reset_回到满一轮() {
        let l = list_of(10);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(42);
        assert_eq!(s.draw(&l, 4, &mut rng).len(), 4);
        assert_eq!(s.remaining(), 6);
        s.reset();
        assert_eq!(s.remaining(), 10);
        assert_eq!(s.round_len(), 10);
        assert_eq!(s.drawn.len(), 1, "10 条只需 1 个 u64");
        // 10 条时 used = 10 % 64 = 10，越界位（第 10..64 位）全部预置为 1。
        assert_eq!(s.drawn[0], u64::MAX << 10, "reset 之后只剩越界哨兵");
        assert_eq!((!s.drawn[0]).count_ones(), 10, "合法位全空闲");
    }

    // ══════════════════════════ 不退避 ══════════════════════════

    /// 守：**不退避**——连败计数完全不影响抽样序列。
    ///
    /// 两份清单结构完全相同，只把其中一半来源的连败计数拉到 17；
    /// 版本号一致（连败不涨版本），同 seed 下的抽样序列必须逐元素相同。
    #[test]
    fn t19_连败计数不影响抽样序列() {
        let n = 8usize;
        let plain = list_of(n);
        let mut failing = list_of(n);
        for i in 0..4u32 {
            for _ in 0..17 {
                failing.record_failure(i);
            }
        }
        // 前提：两份清单确实只有 fail_streak 不同，结构版本相同。
        assert_eq!(plain.version(), failing.version(), "连败不该改变结构版本");
        assert_eq!(failing.fail_streak(0), 17);
        assert_eq!(failing.fail_streak(4), 0);
        assert_eq!(plain.indices(), failing.indices());

        let mut sa = RoundBitmap::new();
        let mut sb = RoundBitmap::new();
        let mut ra = Rng::new(77);
        let mut rb = Rng::new(77);
        // 覆盖多轮（每次 3 个、共 12 次 = 4 轮半）。
        for step in 0..12 {
            assert_eq!(
                sa.draw(&plain, 3, &mut ra),
                sb.draw(&failing, 3, &mut rb),
                "第 {step} 步的抽样序列被连败计数影响了"
            );
            assert_eq!(sa.remaining(), sb.remaining());
        }
    }

    /// 守：整轮全取时，连败计数同样不改变输出（draw_all 是完全确定的）。
    #[test]
    fn t20_连败计数不影响整轮全取() {
        let plain = list_of(6);
        let mut failing = list_of(6);
        for i in 0..3u32 {
            for _ in 0..5 {
                failing.record_failure(i);
            }
        }
        let mut sa = RoundBitmap::new();
        let mut sb = RoundBitmap::new();
        assert_eq!(sa.draw_all(&plain), sb.draw_all(&failing));
    }

    // ══════════════════════════ sync 与本轮的关系 ══════════════════════════

    /// 守：清单在**本轮中途**变化时，sync 会重建快照并开新一轮——
    /// 于是本轮已经抽过的账户会重新变得可抽（这是刻意的：快照变了，旧位图没有意义）。
    ///
    /// ⚠️ 提醒：运维别指望「一轮内不放回」能跨清单变化成立。这里把这条语义钉死，
    /// 免得有人把 sync 改成「只增量追加新账户」而悄悄破坏其它不变量。
    #[test]
    fn t21_清单变化会重置本轮并允许重复() {
        let mut l = list_of(4);
        let mut s = RoundBitmap::new();
        let mut rng = Rng::new(9);
        let first = s.draw(&l, 3, &mut rng);
        assert_eq!(first.len(), 3);
        assert_eq!(s.remaining(), 1);

        // 结构变化（新增账户 9）：下一次 draw 才触发 sync，届时本轮重置，
        // 5 个候选全部重新可用（此时 remaining 尚未变化，仍是 1）。
        l.set(9, 0.5, 0);
        assert_eq!(s.remaining(), 1, "sync 是惰性的，下一次 draw 才生效");
        let second = s.draw(&l, 3, &mut rng);
        assert_eq!(second.len(), 3);
        assert_eq!(s.round_len(), 5, "快照已重建为 5 个候选");
        assert_eq!(s.remaining(), 2, "新一轮里已抽 3 个");
        // 刚抽过的账户**可以**再次出现（新一轮的全部候选重新可用）：
        // 第二次抽 3 个里有 2 个命中上一轮抽过的账户（实跑值，新一轮不保证全命中）。
        let overlap = second.iter().filter(|a| first.contains(a)).count();
        assert_eq!(overlap, 2, "新一轮允许重复抽到上一轮抽过的账户");
        assert!(overlap > 0, "新一轮不该与上一轮完全互斥");
        // 但**这一轮内部**仍然不重复。
        assert_eq!(distinct(&second).len(), 3);

        // 对照：清单**没有**变化时，跨调用仍然不放回。
        let mut s2 = RoundBitmap::new();
        let mut rng2 = Rng::new(9);
        let a = s2.draw(&l, 2, &mut rng2);
        let b = s2.draw(&l, 2, &mut rng2);
        assert!(a.iter().all(|x| !b.contains(x)), "同一轮内跨调用不放回");
    }
}
