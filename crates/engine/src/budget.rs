//! # 自适应抽样预算：把 A10 的硬约束做成可调用的纯函数
//!
//! `审计/A10-抽样参数k与轮次节奏.md` 的实测结论是：抽样器**一轮内不放回**，
//! 所以一个来源被重新取到观点的周期是
//!
//! ```text
//! P = (n_list / k) · T
//! ```
//!
//! 要让时间衰减成为**有意义的信号**（「最近的判断压过旧的判断」，而不是
//! 「谁被随机抽到谁说话」），需要
//!
//! ```text
//! P ≤ H / 2        ⟺        k ≥ 2 · n_list · T / H
//! ```
//!
//! 反例（A10 §6.2 实测）：n_list = 600、k = 8、T = 30h ⇒ P = 2250h = 93.75 天，
//! 而 H = 72h ⇒ 90.7% 的来源衰减因子 < 0.1、ESS 从 600 掉到 55.8。
//! 这时衰减并没有「失效」，它变成了一个**随机数发生器**：谁被重新抽到是均匀
//! 随机的，与来源是否可信无关，群体项实际上只由「最近被抽到的那几十个」决定。
//!
//! 本模块只提供**纯函数 + 结果结构体**，不做任何 I/O、不碰引擎状态：
//! 引擎侧的两个只读入口是 `Engine::recommended_k` 与 `Engine::freshness_budget`
//! （后者只读 `views` 的 `last_check` 与 `ratings`，不改变任何行为）。
//!
//! # 模型：为什么刷新周期是**向上取整**
//!
//! 轮长是整数：n_list = 46、k = 16 时，一轮只能覆盖 16 条，取满整个清单要
//! `⌈46/16⌉ = 3` 轮（A10 §6.1 的实测轮长同样是 `⌈n/k⌉`）。所以这里**不**直接
//! 用实数 `n/k`，而是
//!
//! ```text
//! refresh_period = ⌈n_list · T / k⌉
//! ```
//!
//! 两者在 A10 的推荐例子上给出同一答案：
//! n_list = 46、T = 12h、H = 72h ⇒ `recommended_k = ⌈2·46·12/72⌉ = 16`，
//! 此时 refresh_period = `⌈46·12/16⌉` = 35h ≤ H/2 = 36h（实数口径 34.5h 也 ≤ 36h）。
//! 向上取整只会让判定**更保守**：它不会把不健康的配置判成健康。
//!
//! 另有一条重要性质：`⌈n·T/k⌉` 恰好是 A10 反例里的数字
//! （n = 600、k = 8、T = 30 ⇒ 2250h，与报告逐位一致），因此本模块的
//! `refresh_period` 可以与 A10 的表格直接对账。
//!
//! # 不变量（都有测试钉住）
//!
//! 1. `healthy ⟺ refresh_period ≤ H/2 ⟺ refresh_ratio ≤ 0.5`，H = 0 按 1 算；
//! 2. `k ≥ recommended_k ⟹ healthy` —— **除非** T 本身已经 > H/2：那时任何一个
//!    轮次都已经越过 H/2，无论 k 多大都救不回来（`recommended_k` 退化为 n_list，
//!    表示「把清单抽满也就这样了，真正的解法是把 T 压下来」）；
//! 3. `recommended_k - 1` 一定不健康（在可达区间内），即推荐值不是保守的瞎猜，
//!    而是**最小可行值**；
//! 4. 所有乘法在 u128 里做、结果饱和回 u64：n_list = 10^9、T = u64::MAX
//!    这种输入不会 panic，也不会回绕成一个荒谬的小数字；
//! 5. 同样的输入必然返回逐位相同的输出（纯函数，无随机、无时间、无全局状态）。
//!
//! # 为什么不把 k 放进 `EngineConfig`
//!
//! `EngineConfig` 的**已有默认值必须逐位稳定**，而且库外有大量逐字段构造的
//! `EngineConfig { ... }` 字面量（`crates/exp/src/bin/` 里每个实验一个）。
//! 往结构体里加字段会让这些字面量全部编译失败，属于越界改动。
//! 所以抽样预算 `k` 由**调用方显式传入**：它本来就是应用层的部署参数
//! （A10 §8：手机端 k ≤ 16、桌面端按清单长度反解），而不是引擎的内部状态。

use tm_store::{Score, Time};

/// 半衰期归一化：0 按 1 处理（与 `tm_agg::decay` 的约定一致）。
///
/// 这样「H = 0」不会变成除零，也不会出现「连 dt = 0 都不新鲜」的荒谬结论。
#[must_use]
pub fn normalized_half_life(half_life: Time) -> Time {
    if half_life == 0 {
        1
    } else {
        half_life
    }
}

/// 给定清单规模、轮次间隔、半衰期，推荐每轮抽多少条。
///
/// 依据 A10 实测约束 `(n_list / k) · T ≤ H / 2`，等价于
/// `k ≥ 2 · n_list · T / H`。实现上用整数口径 `⌈n_list · T / (H/2)⌉`
/// （见模块文档），返回值是**满足约束的最小 k**。
///
/// # 边界
/// * `n_list = 0` → 0（没有可抽的对象，也就没有预算可言）；
/// * 其它情况至少返回 1（T = 0 时刷新周期恒为 0，最小预算就够）；
/// * 需要的 k 超过 `n_list` 时返回 `n_list`：超过清单长度的预算没有意义，
///   它等价于「每轮全量」；此时约束通常仍不满足，说明 **T 本身太大**，
///   只能靠缩短轮间隔解决（`DecayDiluted` 会把这个事实一并报出来）；
/// * `half_life = 0` 按 1 处理，因此 H ∈ {0, 1} 时无论 k 多大都达不到阈值，
///   返回 `n_list`（同样是「最好努力」，不是可行解）。
///
/// # 溢出
/// `n_list · period` 在 u128 里算（饱和乘法），结果再夹到 `[1, n_list]`，
/// 因此 `n_list = 10^9`、`period = u64::MAX` 这类输入不会 panic、也不会回绕。
#[must_use]
pub fn recommended_k(n_list: usize, period: Time, half_life: Time) -> usize {
    if n_list == 0 {
        return 0;
    }
    // 轮间隔为 0：每个来源每轮都会被重新取到，刷新周期恒为 0，最小预算即可。
    if period == 0 {
        return 1;
    }
    let half_life = normalized_half_life(half_life);
    let allowed = half_life / 2; // H/2（整数下取整，作为允许的最大刷新周期）
    if allowed == 0 {
        // H = 1：允许的最大刷新周期是 0，而任何正轮间隔都会让 P ≥ T > 0。
        // 这是不可达区间：抽满整个清单也是这个结论。
        return n_list;
    }
    let need = (n_list as u128).saturating_mul(period as u128);
    // 需要 k ≥ n·T / allowed ⟺ ⌈n·T/k⌉ ≤ allowed ⟺ healthy。
    let k = need.div_ceil(allowed as u128);
    if k > n_list as u128 {
        n_list
    } else {
        (k as usize).max(1)
    }
}

/// 刷新周期：把整条正分清单重新取一遍需要的时间。
///
/// 一轮覆盖 k 条，取满 n_list 条要 `⌈n_list / k⌉` 轮，每轮间隔 `period`，
/// 所以 `refresh_period = ⌈n_list · period / k⌉`（见模块文档「为什么向上取整」）。
///
/// # 边界
/// * `n_list = 0` → 0（空清单没有「刷新」这回事）；
/// * `k = 0` → `Time::MAX`：一条都抽不到，刷新周期是**无穷**。
///   用 u64 上界表示，而不是 0 或 panic —— 0 会被误判成「非常健康」；
/// * `period = 0` → 0（轮次不消耗时间）；
/// * `k ≥ n_list` → `period`：预算夹到清单长度（一轮本来就能全量覆盖）。
///   A10 §6.2 的「全量对照」行就是这一条：T = 30h 时所有来源同龄 30h，
///   刷新周期如实报 30h，而不是被 `⌈n·T/k⌉` 算成「更快」；
/// * `n_list · period` 在 u128 里算，最终饱和回 [Time]。
#[must_use]
pub fn refresh_period(n_list: usize, k: usize, period: Time) -> Time {
    if n_list == 0 {
        return 0;
    }
    if k == 0 {
        return Time::MAX;
    }
    // 预算超过清单长度没有意义：一轮本来就能全量覆盖，刷新周期就是一个轮间隔。
    // 不夹的话 `⌈n·T/k⌉` 会给出比 T 还小的数（n=600、k=1000、T=30 ⇒ 18h），
    // 那等于宣称「每轮抽 1000 条比抽满 600 条还快」，与实际不符。
    let effective = k.min(n_list) as u128;
    let need = (n_list as u128).saturating_mul(period as u128);
    let value = need.div_ceil(effective);
    if value > Time::MAX as u128 {
        Time::MAX
    } else {
        value as Time
    }
}

/// 刷新周期与半衰期之比；健康阈值是 0.5。
///
/// 返回值非负有限（H 按 0 → 1 归一化；`refresh_period = Time::MAX` 时约 1.8e19，
/// 仍落在 f32 量程内）。判定**不要**拿它做浮点比较，用 [is_healthy]（整数口径）。
#[must_use]
pub fn refresh_ratio(refresh_period: Time, half_life: Time) -> f32 {
    let half_life = normalized_half_life(half_life);
    (refresh_period as f64 / half_life as f64) as f32
}

/// 「衰减还是有意义的信号」的判据：`refresh_period ≤ H / 2`。
///
/// 用整数比较 `2 · refresh_period ≤ H`（u128 里算，不会溢出），
/// 因此与本模块的 [refresh_ratio] 严格一致（ratio ≤ 0.5 ⟺ 本函数为 true）。
///
/// 半衰期 0 按 1 处理：阈值因此是 0，只有刷新周期恰为 0（T = 0 或空清单）才算健康。
#[must_use]
pub fn is_healthy(refresh_period: Time, half_life: Time) -> bool {
    let half_life = normalized_half_life(half_life);
    (refresh_period as u128) * 2 <= half_life as u128
}

/// 有效样本量 `ESS = (Σw)² / Σw²`，衡量「群体项实际上由多少个来源决定」。
///
/// * 权重相等时 ESS 等于有效权重个数（例如 k 个 1.0 → k）；
/// * 权重越不均，ESS 越小：一个来源独占全部权重时 ESS = 1；
/// * 权重整体数乘不变（`[1, 0.5]` 与 `[2, 1]` 的 ESS 相同），所以「全体来源
///   一起变旧」不会改变 ESS —— A10 §6.2 里 ESS 塌到 55.8，是**年龄散布**造成的，
///   不是整体老化。这条性质在测试里有专门用例；
/// * 非法权重（NaN / ±inf / ≤ 0）一律剔除，与 `tm_agg::aggregate` 的卫生规则一致；
/// * 没有有效权重（含空切片、全部非法、极端量级下溢）→ 0.0；
/// * 内部先把权重按最大值归一化再累加，避免极端量级下 `Σw²` 下溢或 `(Σw)²` 溢出；
/// * 结果恒在 `[1, 有效权重个数]`（有有效权重时），非有限值按 0 兜底。
#[must_use]
pub fn effective_sample_size(weights: &[Score]) -> f32 {
    // 归一化基准：最大的有效权重。先把量级压到 (0, 1]，两个平方项都不可能溢出。
    let max = weights
        .iter()
        .copied()
        .filter(|w| w.is_finite() && *w > 0.0)
        .fold(0.0f32, f32::max);
    if !(max > 0.0) {
        // 空切片、全部非法、或全部下溢到 0。
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    for &w in weights {
        if !w.is_finite() || w <= 0.0 {
            continue;
        }
        let x = (w / max) as f64; // (0, 1]
        sum += x;
        sum_sq += x * x;
    }
    if !(sum_sq > 0.0) {
        return 0.0;
    }
    let ess = sum * sum / sum_sq;
    if ess.is_finite() {
        ess as f32
    } else {
        0.0
    }
}

/// 当前抽样预算的体检结论。
///
/// 判定顺序是**固定的**，且对同一份观测量是确定的：
/// 空清单 → 覆盖率不足 → 刷新周期过长 → 健康。
///
/// 为什么覆盖率排在刷新周期前面：覆盖率过低时群体项本身就在缩水
/// （没观点的来源按弃权票进分母，见 `Engine::query`），此时「把 k 调大」
/// 这条建议是隔靴搔痒 —— 先把观点取回来才是第一优先级。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetVerdict {
    /// 衰减有意义：刷新周期 ≤ H/2，且覆盖率 ≥ 0.5。
    Healthy,
    /// 衰减被稀释：刷新周期 > H/2，建议增大 k 或缩短 T。
    ///
    /// `recommended_k` 是 [recommended_k] 给出的最小可行预算（已夹到清单长度）。
    /// 如果它等于清单长度，说明**仅靠增大 k 已无法达标**：T 本身已经超过 H/2，
    /// 必须缩短轮间隔（或两者一起改）。
    DecayDiluted {
        /// 满足 `(n_list/k)·T ≤ H/2` 的最小 k（已夹到 `[1, n_list]`）。
        recommended_k: usize,
    },
    /// 覆盖率过低：拿到的观点不足一半，群体项会缩水。
    ///
    /// 这是**观测**结果（`views` 里有多少来源还在正分清单里），与 k、T 无关：
    /// 刚启动、长期断网、或清单刚大幅扩张时都会落到这里。
    LowCoverage {
        /// `|views ∩ 正分清单| / n_list`，范围 [0, 1]。
        coverage: f32,
    },
    /// 清单为空，无从判断。
    Empty,
}

impl BudgetVerdict {
    /// 稳定的短标签，供日志 / 报表使用（测试钉住它的取值，避免报表漂移）。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            BudgetVerdict::Healthy => "healthy",
            BudgetVerdict::DecayDiluted { .. } => "decay-diluted",
            BudgetVerdict::LowCoverage { .. } => "low-coverage",
            BudgetVerdict::Empty => "empty",
        }
    }
}

/// 一次抽样预算体检的完整结果（只读观测，构建它不改变任何引擎状态）。
///
/// 字段分两类：
/// * **输入**（`n_list` / `k` / `period` / `half_life`）：调用方给的部署参数与清单规模；
/// * **导出量**（其余）：由输入与引擎当前的 `views` / `ratings` 算出来。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreshnessBudget {
    /// 正分清单长度（`PositiveList::len`）。
    pub n_list: usize,
    /// 每轮抽样条数（应用层配置的预算）。
    pub k: usize,
    /// 轮次间隔（应用层给的，与半衰期同单位）。
    pub period: Time,
    /// 半衰期（配置原值；计算时 0 按 1 处理）。
    pub half_life: Time,
    /// 实际刷新周期 `⌈n_list·period/k⌉`（见 [refresh_period]）；空清单为 0。
    pub refresh_period: Time,
    /// `refresh_period / H`（H 按 0 → 1）；≤ 0.5 才算健康。
    pub refresh_ratio: f32,
    /// `refresh_period ≤ H/2`。**只反映刷新节奏这一维**，不含覆盖率；
    /// 与 [BudgetVerdict::Healthy] 的差别就在这里（后者还要求覆盖率 ≥ 0.5）。
    pub healthy: bool,
    /// 有效样本量 `(Σw)²/Σw²`，w = 份额 × 当前衰减因子，只统计**还在正分清单里**
    /// 且已缓存观点的来源（与 `Engine::query` 的群体项来源集合同口径）。
    pub ess: f32,
    /// `|views ∩ 正分清单| / n_list`，范围 [0, 1]；空清单为 0。
    pub coverage: f32,
    /// 年龄 ≤ H（H 按 0 → 1，恰好一个半衰期也算新鲜）且仍在正分清单里的来源数。
    pub fresh_within_halflife: usize,
    /// 判定结论（见 [BudgetVerdict]）。
    pub verdict: BudgetVerdict,
}

/// 组装一份体检结果：把「观测量」与「部署参数」拼成 [FreshnessBudget]。
///
/// 引擎侧（`Engine::freshness_budget`）负责从 `views` / `ratings` 里
/// 算出 `coverage`、`fresh_within_halflife`、`ess`，然后调用本函数；
/// 本函数负责**纯算术**部分（刷新周期、比值、健康判据、判定），因此可以脱离引擎
/// 单测，也保证「同样的观测量 ⇒ 同样的结论」。
///
/// `coverage` 会被夹到 [0, 1]，NaN 按 0；`ess` 的 NaN / 负值 / 非有限按 0 处理
/// （与 `tm_agg` 的「NaN 一律当 0」约定一致）。
#[must_use]
pub fn assess(
    n_list: usize,
    k: usize,
    period: Time,
    half_life: Time,
    coverage: f32,
    fresh_within_halflife: usize,
    ess: f32,
) -> FreshnessBudget {
    let coverage = if coverage.is_finite() {
        coverage.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ess = if ess.is_finite() && ess > 0.0 {
        ess
    } else {
        0.0
    };
    let refresh_period = refresh_period(n_list, k, period);
    let refresh_ratio = refresh_ratio(refresh_period, half_life);
    let healthy = is_healthy(refresh_period, half_life);
    let verdict = if n_list == 0 {
        BudgetVerdict::Empty
    } else if coverage < 0.5 {
        BudgetVerdict::LowCoverage { coverage }
    } else if !healthy {
        BudgetVerdict::DecayDiluted {
            recommended_k: recommended_k(n_list, period, half_life),
        }
    } else {
        BudgetVerdict::Healthy
    };
    FreshnessBudget {
        n_list,
        k,
        period,
        half_life,
        refresh_period,
        refresh_ratio,
        healthy,
        ess,
        coverage,
        fresh_within_halflife,
        verdict,
    }
}

// ===========================================================================
// 单元测试：每一条都钉住一个**会回归**的性质，而不是「跑一遍不崩溃」
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 浮点比较的绝对容差：ESS 内部走 f64，只在出口转 f32 一次舍入。
    const EPS: f32 = 1e-5;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= EPS
    }

    // ---------------- recommended_k：边界 ----------------

    /// 防回归：空清单被当成「每轮至少抽 1 条」。没有可抽的对象时预算必须是 0，
    /// 否则调用方会拿着一个非零预算去对一个空清单做除法。
    #[test]
    fn 推荐值_空清单为零() {
        for period in [0u64, 1, 12, 30, u64::MAX] {
            for half_life in [0u64, 1, 72, u64::MAX] {
                assert_eq!(recommended_k(0, period, half_life), 0);
            }
        }
    }

    /// 防回归：清单非空却推荐 0 条。k = 0 在引擎里意味着「永远不刷新」，
    /// 任何非空清单的最小可行预算都是 1。
    #[test]
    fn 推荐值_至少为一() {
        for n in [1usize, 2, 46, 600, 1000] {
            for period in [0u64, 1, 12, 30] {
                for half_life in [1u64, 72, 10_000] {
                    let k = recommended_k(n, period, half_life);
                    assert!(k >= 1, "n={n} T={period} H={half_life} 推荐了 {k}");
                    assert!(k <= n, "推荐值不能超过清单长度：{k} > {n}");
                }
            }
        }
    }

    /// 防回归：半衰期为 0 时除零 / panic。约定与 `tm_agg::decay` 一致（H = 0 按 1），
    /// 因此 H ∈ {0, 1} 属于**不可达区间**：只有抽满整个清单才谈得上「尽量」。
    #[test]
    fn 推荐值_半衰期为零按一处理() {
        assert_eq!(recommended_k(10, 12, 0), 10);
        assert_eq!(recommended_k(10, 12, 1), 10);
        assert_eq!(
            recommended_k(10, 12, 2),
            10,
            "H=2 时阈值只剩 1 小时，12 小时间隔不可达"
        );
        // H = 24、T = 12：k ≥ 2·10·12/24 = 10，恰好等于清单长度
        assert_eq!(recommended_k(10, 12, 24), 10);
        // H 再大一点就落进可达区间
        assert_eq!(recommended_k(10, 12, 48), 5);
        // 与显式截断一致：H=0 的行为不会因为除数不同而漂移
        assert_eq!(recommended_k(10, 12, 0), recommended_k(10, 12, 1));
    }

    /// 防回归：把 A10 §8 的推荐值反解错。清单 46、T = 12h、H = 72h 时
    /// 2·46·12/72 = 15.33 ⇒ 最小可行 k = 16，正是 A10 §8 给出的「k = 16, T = 12h」。
    /// 这条用例是公式与报告结论的**对账**：两边必须一致。
    #[test]
    fn 推荐值_复现_a10_推荐的_k16() {
        assert_eq!(recommended_k(46, 12, 72), 16);
        // 实数约束确实卡在 15 与 16 之间
        assert!(2.0 * 46.0 * 12.0 / 72.0 > 15.0);
        assert!(2.0 * 46.0 * 12.0 / 72.0 < 16.0);
    }

    /// 防回归：H/2 边界被算成 H 或 2H。按 A10 硬约束 `k ≥ 2·n·T/H`：
    ///
    /// * n = 600、T = 12h、H = 72h ⇒ 2·600·12/72 = **200**。此时刷新周期
    ///   恰好 = 600·12/200 = 36h = H/2（边界本身，算健康）；
    /// * 100 是 **T = 6h** 时的值（2·600·6/72 = 100），不是 T = 12h 的值 ——
    ///   两种口径都在这里钉住，避免以后有人把 T 记错还以为是公式翻了倍。
    #[test]
    fn 推荐值_半衰期除以二的边界精确() {
        assert_eq!(recommended_k(600, 12, 72), 200);
        assert_eq!(recommended_k(600, 6, 72), 100);
        assert_eq!(
            refresh_period(600, 200, 12),
            36,
            "k=200 时刷新周期恰好是 H/2"
        );
        assert!(is_healthy(36, 72), "恰好 H/2 必须算健康（闭区间）");
        assert!(!is_healthy(37, 72));
        assert_eq!(refresh_period(600, 199, 12), 37, "少一条就掉出边界");
        // 半衰期减半时推荐值翻倍：阈值跟着变紧
        assert_eq!(recommended_k(600, 12, 36), 400);
        // 轮间隔 12 → 10：200 → 166.67 ⇒ 167
        assert_eq!(recommended_k(600, 10, 72), 167);
    }

    /// 防回归：推荐值只是「拍脑袋的保守值」而不是**最小可行值**。
    /// 在可达区间里必须有：用推荐值一定健康，用推荐值减一一定不健康。
    #[test]
    fn 推荐值是最小可行值() {
        let cases = [
            (46usize, 12u64, 72u64),
            (600, 12, 72),
            (200, 6, 72),
            (1000, 12, 72),
            (300, 1, 24),
            (50, 30, 72),
        ];
        for (n, t, h) in cases {
            let k = recommended_k(n, t, h);
            assert!(
                is_healthy(refresh_period(n, k, t), h),
                "n={n} T={t} H={h} k={k} 应当健康"
            );
            if k >= 2 {
                assert!(
                    !is_healthy(refresh_period(n, k - 1, t), h),
                    "n={n} T={t} H={h}：k={} 也够用，推荐值不是最小值",
                    k - 1
                );
            }
        }
    }

    /// 防回归：清单变长时推荐值不增（那会漏抽）。
    #[test]
    fn 推荐值对清单长度单调不减() {
        let mut prev = 0usize;
        for n in [0usize, 1, 2, 3, 10, 46, 100, 300, 600, 1000, 5000] {
            let k = recommended_k(n, 12, 72);
            assert!(k >= prev, "n={n} 时推荐值反而下降：{prev} -> {k}");
            prev = k;
        }
    }

    /// 防回归：轮间隔变长时推荐值不变大（那会漏抽）。
    #[test]
    fn 推荐值对轮间隔单调不减() {
        let mut prev = 0usize;
        for t in [0u64, 1, 2, 6, 12, 24, 30, 48, 72, 1000] {
            let k = recommended_k(600, t, 72);
            assert!(k >= prev, "T={t} 时推荐值反而下降：{prev} -> {k}");
            prev = k;
        }
    }

    /// 防回归：半衰期变长时推荐值变大（阈值变宽了）。
    #[test]
    fn 推荐值对半衰期单调不增() {
        let mut prev = usize::MAX;
        for h in [1u64, 2, 24, 36, 48, 72, 100, 168, 720, 10_000] {
            let k = recommended_k(600, 30, h);
            assert!(k <= prev, "H={h} 时推荐值反而上升：{prev} -> {k}");
            prev = k;
        }
    }

    /// 防回归：巨大的清单规模 × 巨大的轮间隔在 u64 里溢出成小数字或 panic。
    /// 1e9 条清单、u64::MAX 的轮间隔在 u128 里相乘仍然安全，结果被夹到清单长度。
    #[test]
    fn 推荐值_乘溢出不panic() {
        let huge = 1_000_000_000usize;
        assert_eq!(
            recommended_k(huge, u64::MAX, 72),
            huge,
            "需要抽满整个清单才谈得上约束"
        );
        assert_eq!(recommended_k(huge, u64::MAX - 1, 72), huge);
        let k = recommended_k(3, 6_148_914_691_236_517_205, 72); // ≈ u64::MAX / 3
        assert!((1..=3).contains(&k), "得到 {k}");
        // 精确边界：n·T 恰好越过 u64
        assert_eq!(recommended_k(2, u64::MAX / 2 + 1, 72), 2);
        // 正常量级必须给出精确值（不是被溢出污染的）
        assert_eq!(recommended_k(600, 12, 72), 200);
    }

    // ---------------- refresh_period ----------------

    /// 防回归：刷新周期与 A10 §6.2 的实测表对不上（那张表是本模块这两个数字的来源）。
    /// n = 600、k = 8、T = 30h ⇒ 2250h = 93.75 天，而 H = 72h 的健康阈值是 36h。
    #[test]
    fn 刷新周期与_a10_实测逐位对账() {
        assert_eq!(refresh_period(600, 8, 30), 2250);
        assert_eq!(refresh_period(600, 8, 30) as f64 / 24.0, 93.75);
        // A10 §6.2 的整行：T=1h → 75h、T=6h → 450h、T=30h → 2250h
        assert_eq!(refresh_period(600, 8, 1), 75);
        assert_eq!(refresh_period(600, 8, 6), 450);
        assert_eq!(refresh_period(600, 8, 30), 2250);
        // 全量对照：k ≥ n 时刷新周期就是一轮的长度
        assert_eq!(refresh_period(600, 600, 30), 30);
        assert_eq!(refresh_period(600, 1000, 30), 30);
        // A10 §8 的推荐配置：46 条、k = 16、T = 12h ⇒ ⌈46/16⌉ = 3 轮 = 35h ≤ 36h
        assert_eq!(refresh_period(46, 16, 12), 35);
        assert_eq!(refresh_period(46, 16, 12), (46u64 * 12).div_ceil(16));
    }

    /// 防回归：refresh_period 用实数除法 / 向下取整（46·12/16 = 34.5 截成 34）。
    /// 轮长是整数，取满清单要 ⌈n/k⌉ 轮，所以**必须**向上取整；
    /// 向上取整只会更保守，不会把不健康判成健康。
    #[test]
    fn 刷新周期向上取整() {
        // 预算超过清单长度时夹到清单长度：一轮内不可能抽到超过 n 条
        assert_eq!(refresh_period(10, 100, 12), 12);
        assert_eq!(refresh_period(10, 10, 12), 12);
        assert_eq!(
            refresh_period(10, 9, 12),
            14,
            "10·12/9 = 13.33 ⇒ 向上取整 14"
        );
        assert_eq!(refresh_period(10, 3, 1), 4, "⌈10/3⌉ = 4 轮");
        assert_eq!(refresh_period(10, 4, 1), 3, "⌈10/4⌉ = 3 轮");
        assert_eq!(refresh_period(10, 5, 1), 2);
        assert_eq!(refresh_period(1, 1, 12), 12);
        assert!(
            refresh_period(46, 16, 12) > 34,
            "实数口径 34.5 不能被截断成 34"
        );
    }

    /// 防回归：零预算被当成「刷新周期 0」—— 那是**最健康**的假象。
    /// k = 0 意味着一条都抽不到，刷新周期必须是无穷（u64 上界），判定必然不健康。
    #[test]
    fn 刷新周期_零预算表示无穷() {
        assert_eq!(refresh_period(10, 0, 12), Time::MAX);
        assert_eq!(
            refresh_period(1, 0, 0),
            Time::MAX,
            "即使轮间隔为 0，k=0 也永远抽不到"
        );
        assert!(!is_healthy(refresh_period(10, 0, 12), 72));
        assert_eq!(refresh_period(0, 0, 12), 0, "空清单不参与判定");
        // 轮间隔为 0：每个瞬间都在刷新
        assert_eq!(refresh_period(10, 1, 0), 0);
        assert!(is_healthy(0, 72));
    }

    /// 防回归：极大输入的乘法溢出成小数字或 panic。
    #[test]
    fn 刷新周期_极端输入饱和不panic() {
        assert_eq!(refresh_period(1_000_000_000, 1, u64::MAX), Time::MAX);
        assert_eq!(refresh_period(usize::MAX, 1, u64::MAX), Time::MAX);
        // 恰好能算的边界：n·T = u64::MAX 时不该饱和
        assert_eq!(refresh_period(1, 1, u64::MAX), u64::MAX);
        // 大但可表示：600 条、k = 1、T = 1e12 ⇒ 6e14，留在 u64 内
        assert_eq!(
            refresh_period(600, 1, 1_000_000_000_000),
            600_000_000_000_000
        );
    }

    // ---------------- 健康判据 ----------------

    /// 防回归：float 判据与整数判据分叉（ratio 的 0.5 舍入误差会让边界抖动）。
    /// 约定：healthy ⟺ 2·P ≤ H ⟺ ratio ≤ 0.5，三个口径必须**逐点一致**。
    #[test]
    fn 健康判定与比值严格一致() {
        let cases = [
            (0u64, 72u64),
            (36, 72),
            (37, 72),
            (72, 72),
            (1, 72),
            (100, 72),
            (0, 1),
            (1, 1),
            (0, 2),
            (1, 2),
            (2, 2),
            (5, 3),
            (u64::MAX, 72),
            (u64::MAX, u64::MAX),
        ];
        for (p, h) in cases {
            let healthy = is_healthy(p, h);
            let ratio = refresh_ratio(p, h);
            assert!(
                ratio.is_finite() && ratio >= 0.0,
                "P={p} H={h} ratio={ratio}"
            );
            assert_eq!(
                healthy,
                ratio <= 0.5,
                "P={p} H={h}：判定 {healthy} 与 ratio {ratio} 分叉"
            );
        }
        // H = 0 按 1 处理：阈值因此是 0，只有 P = 0 才健康
        assert!(is_healthy(0, 0));
        assert!(!is_healthy(1, 0));
        assert_eq!(refresh_ratio(1, 0), 1.0);
        // 边界是**闭区间**：恰好 H/2 算健康
        assert!(is_healthy(18, 36));
        assert!(!is_healthy(19, 36));
    }

    // ---------------- ESS ----------------

    /// 防回归：ESS 的公式写错（漏平方、除错分母）。手工构造的已知权重：
    /// 两个来源各 1/2 ⇒ ESS = 1²/(1/2) = **2**。
    #[test]
    fn 有效样本量_两个等权来源为二() {
        assert!(close(effective_sample_size(&[0.5, 0.5]), 2.0));
        // 同一组权重换个写法（份额 × 衰减因子）必须同值
        assert!(close(effective_sample_size(&[1.5, 1.5]), 2.0));
        // 六四开：(1)²/(0.36+0.16) = 1/0.52 ≈ 1.923
        assert!(close(effective_sample_size(&[0.6, 0.4]), 1.0 / 0.52));
        // 1 与 0.25：(1.25)²/(1+0.0625) ≈ 1.4706
        assert!(close(effective_sample_size(&[1.0, 0.25]), 1.5625 / 1.0625));
    }

    /// 防回归：等权时 ESS 不等于来源个数（说明公式退化成了别的量）。
    #[test]
    fn 有效样本量_等权时等于个数() {
        for n in 1..=10usize {
            let weights = vec![1.0f32; n];
            assert!(close(effective_sample_size(&weights), n as f32), "n={n}");
            // 全 0.01 也一样
            let weights = vec![0.01f32; n];
            assert!(close(effective_sample_size(&weights), n as f32));
        }
    }

    /// 防回归：ESS 随「整体变旧」而变小。A10 §6.2 的关键观察是
    /// 「ESS 塌到 55.8/600 是年龄**散布**造成的，不是整体老化」——
    /// 全体同乘一个因子时 ESS 必须**逐位不动**。
    #[test]
    fn 有效样本量_数乘不变() {
        let base = [1.0f32, 0.5, 0.25, 0.125];
        let ess = effective_sample_size(&base);
        for scale in [0.5f32, 0.25, 1e-3, 1e3, 1e6] {
            let scaled: Vec<f32> = base.iter().map(|w| w * scale).collect();
            assert!(
                close(effective_sample_size(&scaled), ess),
                "scale={scale}: {} vs {ess}",
                effective_sample_size(&scaled)
            );
        }
    }

    /// 防回归：ESS 的下界被写成 0。一个来源独占全部权重时 ESS = 1（不是 0）：
    /// 「有 1 个有效样本」和「没有样本」是两件事。
    #[test]
    fn 有效样本量_独占来源为一() {
        assert!(close(effective_sample_size(&[1.0]), 1.0));
        assert!(close(effective_sample_size(&[7.0]), 1.0));
        // 极端比值：1.0 与 1e-6 ⇒ ESS ≈ 1.000002（趋近 1，不会掉到 0）
        let dominant = effective_sample_size(&[1.0, 1e-6]);
        assert!(
            (0.999_99..=1.000_01).contains(&dominant),
            "一对悬殊权重的 ESS 应趋近 1：{dominant}"
        );
        // 再悬殊一档：1e-9 已经低于 f32 的 ulp(1.0)，ESS 精确落回 1.0
        assert!(close(effective_sample_size(&[1.0, 1e-9]), 1.0));
    }

    /// 防回归：非法权重（NaN / inf / ≤ 0）被算进分母，把 ESS 拉成 NaN 或缩小。
    /// 与 `tm_agg::aggregate` 的卫生规则一致：NaN 一律当 0（这里直接剔除）。
    #[test]
    fn 有效样本量_非法权重被剔除() {
        assert_eq!(effective_sample_size(&[]), 0.0);
        assert_eq!(effective_sample_size(&[0.0, 0.0]), 0.0);
        assert_eq!(effective_sample_size(&[f32::NAN, -1.0]), 0.0);
        assert_eq!(effective_sample_size(&[f32::INFINITY]), 0.0);
        assert_eq!(effective_sample_size(&[f32::NEG_INFINITY, 0.0]), 0.0);
        // 混入非法值：结果等于只算合法值
        let only_valid = effective_sample_size(&[1.0, 1.0]);
        assert!(close(
            effective_sample_size(&[1.0, f32::NAN, 1.0, -3.0, f32::INFINITY]),
            only_valid
        ));
        // 极端量级（f32 最小正规数）不下溢成 NaN
        let tiny = [f32::MIN_POSITIVE, f32::MIN_POSITIVE];
        let ess = effective_sample_size(&tiny);
        assert!(close(ess, 2.0), "最小正规数×2 的 ESS 应为 2，实际 {ess}");
        // 次正规数（最容易 flush-to-zero）同样不能变成 0
        let sub = [f32::from_bits(1), f32::from_bits(1)];
        assert!(close(effective_sample_size(&sub), 2.0));
    }

    // ---------------- assess：判定顺序 ----------------

    /// 防回归：空清单被判成「健康」。没有清单就没有可判断的对象，必须是 Empty。
    #[test]
    fn 体检_空清单为空() {
        let b = assess(0, 8, 12, 72, 0.0, 0, 0.0);
        assert_eq!(b.verdict, BudgetVerdict::Empty);
        assert_eq!(b.n_list, 0);
        assert_eq!(b.refresh_period, 0);
        assert_eq!(b.coverage, 0.0);
        assert!(
            b.healthy,
            "空清单的刷新周期是 0，按判据本身健康；判定另行给 Empty"
        );
    }

    /// 防回归：健康路径被误判。清单 100 条、k = 50、T = 12h ⇒ 刷新周期 24h ≤ 36h，
    /// 覆盖率 1.0 ⇒ Healthy（与引擎里「全员刚刚 on_view」一致）。
    #[test]
    fn 体检_全员新鲜判健康() {
        let b = assess(100, 50, 12, 72, 1.0, 100, 50.0);
        assert_eq!(b.verdict, BudgetVerdict::Healthy);
        assert_eq!(b.refresh_period, 24);
        assert!(b.healthy);
        assert!(close(b.refresh_ratio, 24.0 / 72.0));
        assert_eq!(b.fresh_within_halflife, 100);
    }

    /// 防回归：覆盖率过低却给了「把 k 调大」的建议 —— 真正的问题是观点没取回来。
    /// 判定顺序因此是 覆盖率 → 刷新周期 → 健康。
    #[test]
    fn 体检_覆盖率不足优先报低覆盖() {
        // 刷新周期本身也不健康，但覆盖率更差：必须先报覆盖率
        let b = assess(1000, 8, 12, 72, 0.3, 12, 8.0);
        assert!(matches!(
            b.verdict,
            BudgetVerdict::LowCoverage { coverage } if close(coverage, 0.3)
        ));
        assert!(!b.healthy);
        // 刷新周期健康但覆盖不足：同样报 LowCoverage
        let b = assess(100, 50, 12, 72, 0.49, 49, 20.0);
        assert!(matches!(
            b.verdict,
            BudgetVerdict::LowCoverage { coverage } if close(coverage, 0.49)
        ));
        assert!(b.healthy, "healthy 只反映刷新节奏");
        // 覆盖恰好 0.5：达标，不再报 LowCoverage
        let b = assess(100, 50, 12, 72, 0.5, 50, 20.0);
        assert_eq!(b.verdict, BudgetVerdict::Healthy);
    }

    /// 防回归：衰减被稀释的配置被判成健康、或推荐的 k 算错。
    /// A10 反例：n=600、k=8、T=30h、H=72h ⇒ P = 2250h ≫ 36h，
    /// 需要的 k ≥ 2·600·30/72 = **500**（不是 T=12h 那个 200）。
    #[test]
    fn 体检_超长刷新周期报稀释并给推荐值() {
        let b = assess(600, 8, 30, 72, 0.9, 550, 55.8);
        assert!(matches!(
            b.verdict,
            BudgetVerdict::DecayDiluted { recommended_k } if recommended_k == 500
        ));
        assert_eq!(b.refresh_period, 2250);
        assert!(!b.healthy);
        assert!(
            (b.refresh_ratio - 31.25).abs() < 1e-4,
            "2250/72 = 31.25，实际 {}",
            b.refresh_ratio
        );
        // 推荐值本身必须真的健康
        assert!(is_healthy(refresh_period(600, 500, 30), 72));
        // T 本身已经超过 H/2 时，推荐值被夹到清单长度（只能靠缩短 T）
        let b = assess(600, 8, 40, 72, 0.9, 550, 55.8);
        assert!(matches!(
            b.verdict,
            BudgetVerdict::DecayDiluted { recommended_k } if recommended_k == 600
        ));
    }

    /// 防回归：同样的输入两次调用得到不同结果（结构体里混进了随机量 / 时钟）。
    #[test]
    fn 体检_确定性() {
        let a = assess(600, 8, 30, 72, 0.9, 550, 55.8);
        let b = assess(600, 8, 30, 72, 0.9, 550, 55.8);
        assert_eq!(a, b, "同样的观测量必须给出逐位相同的结果");
        // 逐字段再比一遍，避免将来 PartialEq 的派生漏掉某个字段
        assert_eq!(a.n_list, b.n_list);
        assert_eq!(a.k, b.k);
        assert_eq!(a.refresh_period, b.refresh_period);
        assert_eq!(a.refresh_ratio.to_bits(), b.refresh_ratio.to_bits());
        assert_eq!(a.healthy, b.healthy);
        assert_eq!(a.ess.to_bits(), b.ess.to_bits());
        assert_eq!(a.coverage.to_bits(), b.coverage.to_bits());
        assert_eq!(a.fresh_within_halflife, b.fresh_within_halflife);
        assert_eq!(a.verdict, b.verdict);
    }

    /// 防回归：`healthy` 与 `verdict` 被当成同一件事。
    /// healthy 只回答「刷新节奏这一维」，覆盖率低于 0.5 时它照样可以是 true。
    #[test]
    fn 体检_healthy字段不含覆盖率() {
        let b = assess(100, 50, 12, 72, 0.1, 10, 3.0);
        assert!(b.healthy);
        assert!(matches!(b.verdict, BudgetVerdict::LowCoverage { .. }));
        // 反过来：健康节奏 + 达标覆盖率才是 Healthy
        let b = assess(100, 50, 12, 72, 0.5, 50, 20.0);
        assert!(b.healthy && matches!(b.verdict, BudgetVerdict::Healthy));
    }

    /// 防回归：报表标签漂移（实验脚本按标签筛行）。
    #[test]
    fn 判决标签稳定() {
        assert_eq!(BudgetVerdict::Healthy.label(), "healthy");
        assert_eq!(
            BudgetVerdict::DecayDiluted { recommended_k: 3 }.label(),
            "decay-diluted"
        );
        assert_eq!(
            BudgetVerdict::LowCoverage { coverage: 0.25 }.label(),
            "low-coverage"
        );
        assert_eq!(BudgetVerdict::Empty.label(), "empty");
    }

    /// 防回归：半衰期为 0 时除零 / panic / NaN。
    #[test]
    fn 体检_半衰期为零不panic() {
        let b = assess(100, 50, 12, 0, 1.0, 100, 50.0);
        // H=0 按 1：刷新周期 24h 远超阈值 0，判不健康
        assert!(!b.healthy);
        assert!(b.refresh_ratio.is_finite());
        assert!(matches!(
            b.verdict,
            BudgetVerdict::DecayDiluted { recommended_k } if recommended_k == 100
        ));
        // 轮间隔为 0 时 H=0 也健康（刷新周期恒为 0）
        let b = assess(100, 50, 0, 0, 1.0, 100, 50.0);
        assert!(b.healthy);
        assert_eq!(b.verdict, BudgetVerdict::Healthy);
    }

    /// 防回归：覆盖率 / ESS 的非法值把结构体污染成 NaN，下游报表跟着炸。
    /// 约定与 tm_agg 一致：NaN / 负值 / 非有限一律按 0，覆盖率另外夹到 [0,1]。
    #[test]
    fn 体检_非法观测量被夹紧() {
        let b = assess(100, 50, 12, 72, f32::NAN, 0, f32::NAN);
        assert_eq!(b.coverage, 0.0);
        assert_eq!(b.ess, 0.0);
        assert!(matches!(b.verdict, BudgetVerdict::LowCoverage { .. }));
        let b = assess(100, 50, 12, 72, 2.5, 100, -3.0);
        assert_eq!(b.coverage, 1.0);
        assert_eq!(b.ess, 0.0);
        let b = assess(100, 50, 12, 72, f32::INFINITY, 100, f32::INFINITY);
        assert_eq!(b.coverage, 0.0, "inf 不是有限值，按 0 处理");
        assert_eq!(b.ess, 0.0);
    }

    /// 防回归：「k ≥ 推荐值 ⇒ 健康」这条工程承诺被破坏
    /// （推荐值与判据用了不同口径时最容易分叉）。可达区间上全量扫描。
    #[test]
    fn 推荐值达标即健康() {
        for n in [1usize, 2, 10, 46, 100, 300, 600, 1000] {
            for t in [1u64, 6, 12, 24, 30] {
                for h in [24u64, 36, 48, 72, 168] {
                    let rec = recommended_k(n, t, h);
                    if is_healthy(refresh_period(n, n, t), h) {
                        // 可达区间：从推荐值往上全健康，推荐值本身也健康
                        for k in rec..=n {
                            assert!(
                                is_healthy(refresh_period(n, k, t), h),
                                "n={n} T={t} H={h}：k={k} ≥ 推荐值 {rec} 却不健康"
                            );
                        }
                    } else {
                        // 不可达区间（T 本身已超过 H/2）：推荐值必须被夹到清单长度
                        assert_eq!(
                            rec, n,
                            "n={n} T={t} H={h}：抽满也不健康，推荐值应被夹到 {n}，实际 {rec}"
                        );
                    }
                }
            }
        }
    }
}
