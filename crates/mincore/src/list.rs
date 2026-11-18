//! 正分清单：`(账户, 正分, 上次成功取到观点的时间, 连败计数)`。
//!
//! 与现行 `tm_store::PositiveList` 的信息量完全一致，但只保留能被反女巫用到的部分：
//!
//! * `score ∈ (0, 1]`：**份额的原料**。0 表示没表态，写入 0 等价于删除；
//! * `last_check`：衰减基准。**只由「成功取到观点」刷新**——改分不算，
//!   因为「我的判断变了」和「我刚听到它说话」是两件事；
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

    /// 写入/更新一条正分，并把 `last_check` 刷新为 `now`。
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
