//! 正分清单：`(账户, 正分, 上次成功取到观点的时间, 连败计数)`。
//!
//! 与现行 `tm_store::PositiveList` 的信息量完全一致，但只保留能被反女巫用到的部分：
//!
//! * `score ∈ (0, 1]`：**份额的原料**。0 表示没表态，写入 0 等价于删除；
//! * `last_check`：**诊断字段**（`谁多久没说话了`），不参与任何计算。
//!   `set` 会把它刷新为 `now`，`record_success` 也会；`record_failure` 不动它。
//!   注意：**衰减的基准不是这个字段**，而是 `Node::views` 里 `Received::last_check`
//!   （由 `on_view` 在成功收观点时写入）——那是「我上次真的听到它说话」的证据，
//!   对方伪造不了。清单里的 `last_check` 只是给运维看的；
//! * `fail_streak`：连败计数。**只记账，不参与抽样**（这是砍掉退避的那一刀：
//!   字段留着是因为它是「谁失联了」的唯一观测，删掉会让运维变成瞎子，
//!   但它对 `RoundBitmap` 的影响被刻意设为零）。
//!
//! 用 `BTreeMap` 而不是 `HashMap`：遍历必须按账户号升序，否则浮点累加顺序会随
//! 哈希种子变化，破坏「同输入同输出」。

use std::collections::BTreeMap;

/// 账户号。全网统一分配，本内核只当作不透明的 u32。
pub type Acc = u32;

/// 时间刻度（单位由上层定，只要和半衰期一致）。不假设单调递增。
pub type Time = u64;

/// 正分清单里的一条。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    /// 原始正分，`(0, 1]`。
    pub score: f32,
    /// 上次**成功**取到对方观点的时间。失败不刷新。
    pub last_check: Time,
    /// 连续抽样失败次数。只记账，不参与抽样（见模块文档）。
    pub fail_streak: u16,
}

/// 本地维护的正分清单。
#[derive(Clone, Debug, Default)]
pub struct PositiveList {
    entries: BTreeMap<Acc, Entry>,
    /// 结构版本号：只有**增删条目**才 +1（改分、改时间不动它）。
    /// `RoundBitmap` 靠它判断要不要重建快照。
    version: u64,
}

impl PositiveList {
    /// 空清单。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入/更新一条正分。既有条目与新建条目都会把 `last_check` 刷新为 `now`
    /// （这个字段是诊断用途，不参与衰减计算，见模块文档）。
    ///
    /// * `score` 为 NaN、`<= 0` 或 `-0.0` → **删除**该条目（0 就是没表态）；
    /// * `score > 1` → 夹到 `1.0`（单条正分不许超过满额，否则预算会被一条撑爆）；
    /// * 新条目的连败计数从 0 开始；**已有条目的连败计数原样保留**——
    ///   改分不是「刚取到观点」。
    pub fn set(&mut self, acc: Acc, score: f32, now: Time) {
        // 语义：NaN / -0.0 / <= 0 → **删除**该条目（0 就是没表态）。
        //
        // 这里**必须**显式判 NaN：`!(score > 0.0)` 对 NaN 为真，`score <= 0.0` 对 NaN 为假。
        // 若写成后者，NaN 会穿过下面所有分支被**真的存进清单**（NaN > 1.0 为假 ⟹ 不夹紧），
        // 随后 `total()` 立刻变成 NaN，这份清单里所有份额一起报废——
        // 那是安静的数据污染，比「删一条」严重得多。
        // `is_nan() || <= 0.0` 与原来的 `!(score > 0.0)` 在全部 f32 位模式上逐值等价
        // （本轮已穷举 2^32 个位模式验证，0 处不一致）。
        if score.is_nan() || score <= 0.0 {
            if self.entries.remove(&acc).is_some() {
                self.version += 1;
            }
            return;
        }
        let score = if score > 1.0 { 1.0 } else { score };
        match self.entries.get_mut(&acc) {
            Some(e) => {
                e.score = score;
                e.last_check = now;
            }
            None => {
                self.entries.insert(
                    acc,
                    Entry {
                        score,
                        last_check: now,
                        fail_streak: 0,
                    },
                );
                self.version += 1;
            }
        }
    }

    /// 取原始正分；没表态返回 `None`。
    #[must_use]
    pub fn get(&self, acc: Acc) -> Option<f32> {
        self.entries.get(&acc).map(|e| e.score)
    }

    /// 全部正分之和（影响力预算的分母）。f64 累加、按账户号升序。
    #[must_use]
    pub fn total(&self) -> f64 {
        self.entries.values().map(|e| f64::from(e.score)).sum()
    }

    /// 份额 `score / Σscore`；没表态或总分为 0 时返回 `None`。
    #[must_use]
    pub fn share(&self, acc: Acc) -> Option<f64> {
        let s = f64::from(self.entries.get(&acc)?.score);
        let total = self.total();
        if total > 0.0 {
            Some(s / total)
        } else {
            None
        }
    }

    /// 是否有该条目。
    #[must_use]
    pub fn has(&self, acc: Acc) -> bool {
        self.entries.contains_key(&acc)
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 结构版本号（增删条目时变化）。
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 按账户号升序遍历 `(账户, 条目)`。
    pub fn iter(&self) -> impl Iterator<Item = (Acc, &Entry)> + '_ {
        self.entries.iter().map(|(&k, v)| (k, v))
    }

    /// 按账户号升序返回全部账户号。
    #[must_use]
    pub fn indices(&self) -> Vec<Acc> {
        self.entries.keys().copied().collect()
    }

    /// 记录一次抽样失败：连败 +1（饱和），**不动** `last_check`、**不动** 分数。
    ///
    /// 条目不存在时返回 0，什么也不做。
    pub fn record_failure(&mut self, acc: Acc) -> u16 {
        match self.entries.get_mut(&acc) {
            Some(e) => {
                e.fail_streak = e.fail_streak.saturating_add(1);
                e.fail_streak
            }
            None => 0,
        }
    }

    /// 记录一次成功：连败清零并把 `last_check` 刷新为 `now`。
    ///
    /// 这是**唯一**会清零连败、也是**唯一**会推进衰减基准的入口。
    pub fn record_success(&mut self, acc: Acc, now: Time) {
        if let Some(e) = self.entries.get_mut(&acc) {
            e.fail_streak = 0;
            e.last_check = now;
        }
    }

    /// 当前连败次数；无条目返回 0。
    #[must_use]
    pub fn fail_streak(&self, acc: Acc) -> u16 {
        self.entries.get(&acc).map_or(0, |e| e.fail_streak)
    }

    /// 上次成功取到观点的时间；无条目返回 `None`。
    #[must_use]
    pub fn last_check(&self, acc: Acc) -> Option<Time> {
        self.entries.get(&acc).map(|e| e.last_check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部条目的份额之和；空清单视作 0。
    fn share_sum(l: &PositiveList) -> f64 {
        l.indices().iter().filter_map(|&a| l.share(a)).sum()
    }

    /// 守：`set` 写正分时四个字段全部落位，结构版本 +1。
    #[test]
    fn t01_set_新条目写入全部字段() {
        let mut l = PositiveList::new();
        assert!(l.is_empty());
        assert_eq!(l.version(), 0);
        l.set(7, 0.5, 42);
        assert_eq!(l.len(), 1);
        assert!(!l.is_empty());
        assert_eq!(l.get(7), Some(0.5));
        assert_eq!(l.last_check(7), Some(42));
        assert_eq!(l.fail_streak(7), 0, "新条目的连败计数从 0 起");
        assert_eq!(l.total(), 0.5);
        assert_eq!(l.version(), 1);
        let (acc, e) = l.iter().next().expect("刚写入的条目");
        assert_eq!(acc, 7);
        assert_eq!(
            *e,
            Entry {
                score: 0.5,
                last_check: 42,
                fail_streak: 0
            }
        );
        assert_eq!(l.indices(), vec![7]);
    }

    /// 守：单条正分不许超过满额（`> 1` 一律夹到 1.0），否则一条就能撑爆预算。
    #[test]
    fn t02_set_超过1夹到1() {
        let mut l = PositiveList::new();
        for (acc, s) in [
            (1u32, 1.0f32),
            (2, 1.5),
            (3, f32::MAX),
            (4, f32::INFINITY),
            (5, 1e30),
        ] {
            l.set(acc, s, 0);
            assert_eq!(l.get(acc), Some(1.0), "账户 {acc} 的分数 {s} 应夹到 1.0");
        }
        assert_eq!(l.len(), 5);
        assert_eq!(l.total(), 5.0, "夹紧之后每条都恰好是 1.0");
        for acc in 1..=5u32 {
            assert_eq!(l.share(acc), Some(0.2), "5 条等权，份额应为 1/5");
        }
    }

    /// 守：0 / -0.0 / 负数 / NaN / -∞ 都表示「撤销表态」，删除条目并让版本 +1；
    /// 对本来就不存在的条目写这些值则什么都不发生（版本不涨）。
    #[test]
    fn t03_set_非正值等价删除() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 0);
        l.set(2, 0.5, 0);
        assert_eq!(l.version(), 2);
        let mut expect_version = 2u64;
        for bad in [0.0f32, -0.0, -1.0, f32::NAN, f32::NEG_INFINITY] {
            l.set(1, bad, 9);
            assert!(!l.has(1), "{bad:?} 应当被当成「没表态」");
            assert_eq!(l.len(), 1);
            expect_version += 1;
            assert_eq!(l.version(), expect_version, "删除已有条目时版本 +1");
            l.set(1, 0.5, 0);
            expect_version += 1;
            assert_eq!(l.get(1), Some(0.5));
        }
        // 不存在的条目：写非正值不该改变任何东西。
        let before = l.version();
        l.set(99, 0.0, 0);
        l.set(99, f32::NAN, 0);
        assert_eq!(l.version(), before, "删除不存在的条目不该涨版本");
        assert_eq!(l.len(), 2);
    }

    /// 守：改分保留连败计数、不改变结构版本；改分会刷新 `last_check`。
    #[test]
    fn t04_set_改分保留连败计数且不动版本() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 10);
        for _ in 0..3 {
            l.record_failure(1);
        }
        assert_eq!(l.fail_streak(1), 3);
        l.set(1, 0.9, 20);
        assert_eq!(l.get(1), Some(0.9));
        assert_eq!(l.last_check(1), Some(20));
        assert_eq!(
            l.fail_streak(1),
            3,
            "改分不是「刚取到观点」，连败计数必须保留"
        );
        assert_eq!(l.version(), 1, "改分不属于结构变化");
    }

    /// 守：版本号只在**增删条目**时变化；改分、连败、成功都不涨。
    #[test]
    fn t05_version_只在增删时变化() {
        let mut l = PositiveList::new();
        assert_eq!(l.version(), 0);
        l.set(1, 0.5, 0);
        assert_eq!(l.version(), 1, "新增");
        l.set(1, 0.7, 1);
        assert_eq!(l.version(), 1, "改分");
        l.record_failure(1);
        assert_eq!(l.version(), 1, "连败 +1");
        l.record_success(1, 2);
        assert_eq!(l.version(), 1, "连败清零");
        l.set(1, 0.0, 3);
        assert_eq!(l.version(), 2, "删除");
        l.set(1, 0.0, 4);
        assert_eq!(l.version(), 2, "删除不存在的条目");
        l.set(2, 1.0, 5);
        assert_eq!(l.version(), 3, "再次新增");
        l.set(2, 0.0, 6);
        assert_eq!(l.version(), 4, "删了 2");
        l.set(3, 0.0, 7);
        assert_eq!(l.version(), 4, "再删不存在的 3");
    }

    /// 守：所有 getter 在缺失条目上给出约定值，空清单不 panic。
    #[test]
    fn t06_缺失条目的查询返回值() {
        let empty = PositiveList::new();
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert_eq!(empty.get(3), None);
        assert!(!empty.has(3));
        assert_eq!(empty.share(3), None);
        assert_eq!(empty.fail_streak(3), 0);
        assert_eq!(empty.last_check(3), None);
        assert_eq!(empty.total(), 0.0);
        assert!(empty.indices().is_empty());

        let mut l = PositiveList::new();
        l.set(1, 0.5, 0);
        assert_eq!(l.get(2), None);
        assert_eq!(l.share(2), None, "没表态的账户没有份额");
        assert_eq!(l.last_check(2), None);
        assert_eq!(l.fail_streak(2), 0);
        assert!(!l.has(2));
    }

    /// 守：总分只由「正分」构成，且按账户号升序遍历累加（与写入顺序无关、逐位可复现）。
    #[test]
    fn t07_total_按账户号升序累加且与写入顺序无关() {
        let mut l = PositiveList::new();
        for (a, s) in [(12u32, 0.5f32), (10, 0.2), (11, 0.3)] {
            l.set(a, s, 0);
        }
        let expect = f64::from(0.2f32) + f64::from(0.3f32) + f64::from(0.5f32);
        assert_eq!(l.total(), expect);
        assert_eq!(l.indices(), vec![10, 11, 12], "遍历必须按账户号升序");

        // 同一集合，用逆序写入：总分必须逐位相同（f64 累加顺序固定）。
        let mut r = PositiveList::new();
        r.set(11, 0.3, 0);
        r.set(10, 0.2, 0);
        r.set(12, 0.5, 0);
        assert_eq!(r.total(), l.total(), "总分与写入顺序无关");
        assert_eq!(r.total().to_bits(), l.total().to_bits());

        // 撤销一条，总分与份额一起变。
        l.set(11, 0.0, 1);
        assert_eq!(l.total(), f64::from(0.2f32) + f64::from(0.5f32));
        assert_eq!(l.share(11), None);
    }

    /// 守：份额 = score / Σscore；单条时精确为 1.0；Σ 份额恒为 1（容差 1e-6）。
    #[test]
    fn t08_share_等于分数除以总和() {
        let mut l = PositiveList::new();
        for (a, s) in [(10u32, 0.2f32), (11, 0.3), (12, 0.5)] {
            l.set(a, s, 0);
        }
        // 份额是 f64 精确商，可以直接断言到最后一个 bit（不是近似的 0.2/0.3/0.5）。
        let total = f64::from(0.2f32) + f64::from(0.3f32) + f64::from(0.5f32);
        assert_eq!(l.total(), total);
        assert_eq!(l.share(10), Some(f64::from(0.2f32) / total));
        assert_eq!(l.share(11), Some(f64::from(0.3f32) / total));
        assert_eq!(l.share(12), Some(f64::from(0.5f32) / total));
        // 十进制期望值只在 1e-7 内（0.3f32 - 0.3 ≈ 7.45e-9）。
        assert!((l.share(11).expect("11 号有份额") - 0.3).abs() < 1e-7);
        assert!(
            (share_sum(&l) - 1.0).abs() < 1e-6,
            "Σ 份额 = {}",
            share_sum(&l)
        );

        // 单条：份额精确等于 1.0（不是 0.999999）。
        let mut one = PositiveList::new();
        one.set(42, 0.3, 0);
        assert_eq!(one.share(42), Some(1.0), "单条时份额精确为 1.0");
        assert_eq!(one.total(), f64::from(0.3f32));
    }

    /// 守：任意条数（1~1000）下份额总和都 ≡ 1，不会因为 f32→f64 的累加漂出去。
    #[test]
    fn t09_share_sum_equals_one() {
        for n in [1usize, 2, 3, 7, 100, 1000] {
            let mut l = PositiveList::new();
            for i in 0..n as u32 {
                l.set(i, ((i % 17) + 1) as f32 / 17.0, 0);
            }
            assert_eq!(l.len(), n);
            let sum = share_sum(&l);
            assert!((sum - 1.0).abs() < 1e-6, "n = {n}：Σ 份额 = {sum}");
        }
    }

    /// 守：`record_failure` 连败 +1 并返回当前值；不存在条目返回 0；u16 上界饱和不回绕。
    #[test]
    fn t10_record_failure_连败递增并饱和() {
        let mut l = PositiveList::new();
        l.set(1, 1.0, 0);
        assert_eq!(l.record_failure(1), 1);
        assert_eq!(l.record_failure(1), 2);
        assert_eq!(l.record_failure(1), 3);
        assert_eq!(l.fail_streak(1), 3);
        assert_eq!(l.record_failure(2), 0, "条目不存在时返回 0、什么也不做");
        assert_eq!(l.len(), 1);

        let mut sat = PositiveList::new();
        sat.set(3, 1.0, 0);
        for _ in 0..(u16::MAX as usize + 2) {
            sat.record_failure(3);
        }
        assert_eq!(
            sat.fail_streak(3),
            u16::MAX,
            "连败计数在 u16 上界饱和，不回绕"
        );
    }

    /// 守：失败**只**记账——不动分数、不动 `last_check`（失败不是「刚取到观点」）、不动版本。
    #[test]
    fn t11_record_failure_不动分数与last_check() {
        let mut l = PositiveList::new();
        l.set(1, 0.8, 20);
        let total_before = l.total();
        for _ in 0..5 {
            l.record_failure(1);
        }
        assert_eq!(l.get(1), Some(0.8), "失败不改分数");
        assert_eq!(l.last_check(1), Some(20), "失败不刷新衰减基准");
        assert_eq!(l.fail_streak(1), 5);
        assert_eq!(l.total(), total_before);
        assert_eq!(l.version(), 1, "连败计数不属于结构变化");
    }

    /// 守：`record_success` 是唯一会清零连败的入口，并把 `last_check` 推进到 now。
    #[test]
    fn t12_record_success_清零连败并刷新last_check() {
        let mut l = PositiveList::new();
        l.set(1, 0.8, 0);
        for _ in 0..5 {
            l.record_failure(1);
        }
        assert_eq!(l.fail_streak(1), 5);
        l.record_success(1, 30);
        assert_eq!(l.fail_streak(1), 0, "成功清零连败");
        assert_eq!(l.last_check(1), Some(30), "成功把衰减基准推进到 now");
        assert_eq!(l.get(1), Some(0.8), "成功不改分数");
        assert_eq!(l.version(), 1);
        // 对不存在的条目是无操作。
        l.record_success(2, 40);
        assert_eq!(l.len(), 1);
        assert_eq!(l.version(), 1);
        assert_eq!(l.last_check(2), None);
    }

    /// 守 `last_check` 的刷新口径：**改分刷新、失败不刷新、成功刷新**。
    ///
    /// 为什么这样是对的：这个字段是**诊断用途**（「谁多久没说话了」），不参与任何计算。
    /// 真正的衰减基准是 `Node::views` 里 `Received::last_check`（由 `on_view` 写入），
    /// 那是「我上次真的听到它说话」的证据，对方伪造不了。
    ///
    /// 历史：模块文档曾写「只由成功取到观点刷新——改分不算」，与 `set` 的实现不符。
    /// 2026-11-18 已改成准确描述（诊断字段 + 衰减基准在别处）。断言没变。
    #[test]
    fn t13_last_check_改分刷新_失败不刷新() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 100);
        assert_eq!(l.last_check(1), Some(100));
        l.set(1, 0.7, 200);
        assert_eq!(
            l.last_check(1),
            Some(200),
            "改分会刷新（诊断字段，不参与计算）"
        );
        l.set(1, 0.5, 300);
        assert_eq!(l.last_check(1), Some(300), "写回原分也会刷新（无差别更新）");
        l.record_failure(1);
        assert_eq!(l.last_check(1), Some(300), "失败不刷新，这是刻意的");
        l.record_success(1, 400);
        assert_eq!(l.last_check(1), Some(400), "成功刷新");
    }

    /// 守：删除再重新加入是**全新条目**（连败归零、last_check 重新起算），版本两次 +1。
    #[test]
    fn t14_删除后重新加入_连败归零() {
        let mut l = PositiveList::new();
        l.set(1, 0.5, 0);
        for _ in 0..5 {
            l.record_failure(1);
        }
        assert_eq!(l.fail_streak(1), 5);
        assert_eq!(l.version(), 1);
        l.set(1, 0.0, 1);
        assert_eq!(l.version(), 2);
        assert!(!l.has(1));
        assert_eq!(l.last_check(1), None);
        l.set(1, 0.9, 2);
        assert_eq!(l.version(), 3);
        assert_eq!(l.get(1), Some(0.9));
        assert_eq!(l.fail_streak(1), 0, "重加是全新条目，连败从 0 起");
        assert_eq!(l.last_check(1), Some(2));
    }

    /// 守：`iter` / `indices` 一律按账户号升序（浮点累加顺序的确定性依赖它）。
    #[test]
    fn t15_iter与indices按账户号升序() {
        let mut l = PositiveList::new();
        for a in [1000u32, 3, 77, 5] {
            l.set(a, 0.5, 0);
        }
        assert_eq!(l.indices(), vec![3, 5, 77, 1000]);
        let pairs: Vec<(Acc, Entry)> = l.iter().map(|(a, e)| (a, *e)).collect();
        assert_eq!(pairs.len(), 4);
        let idx = l.indices();
        for (i, (a, e)) in pairs.iter().enumerate() {
            assert_eq!(*a, idx[i], "iter 必须与 indices 同序");
            assert_eq!(e.score, 0.5);
            assert_eq!(e.last_check, 0);
            assert_eq!(e.fail_streak, 0);
        }
    }

    /// 守：`default()` 与 `new()` 都是空清单（三个「空」口径一致）。
    #[test]
    fn t16_default与new都是空清单() {
        let a = PositiveList::default();
        let b = PositiveList::new();
        for l in [&a, &b] {
            assert!(l.is_empty());
            assert_eq!(l.len(), 0);
            assert_eq!(l.version(), 0);
            assert_eq!(l.total(), 0.0);
            assert_eq!(l.share(1), None);
            assert!(l.indices().is_empty());
        }
    }
}
