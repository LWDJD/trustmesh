//! # badlist —— 受害者反馈机制的核心算法（一次性事件）
//!
//! 这是整套设计里**唯一一个有受害者反馈的机制**：其它机制只看「你评了谁」，
//! 不看「你护了谁」。应用层判断「我正在被谁攻击」，把那个账户（记作 A）
//! 通过 [crate::Engine::report_bad_account] 报告给引擎；引擎再看
//! **我信任的来源**是怎么评价 A 的：
//!
//! ~~~text
//! a_v = 来源 v 的观点里对 A 的读数（0~1，与 query 的群体项同口径）
//! w_v = 份额_v × 衰减_v
//! ā   = 所有**表过态**来源的 a_v 的**未加权**平均
//! W_a = Σ_{弃权来源} w_v     （没有观点，或观点没覆盖 A：一票 0 但占着分母）
//! ~~~
//!
//! 用未加权平均，是因为**加权平均恰好就是 A 现在的群体项得分**，
//! 拿它当目标线等于「把 A 压到 A 自己」，机制就没有意义了。
//! ā 的语义是「一个普通来源怎么看 A」。
//!
//! ## 唯一变体：均值以上统一折扣
//!
//! ~~~text
//! S+ = { v : a_v > ā }       均值以上
//! S− = { v : a_v ≤ ā }       均值及以下（完全不动）
//!
//! 对 S+ 里的所有来源乘**同一个**折扣 f，令压完后的加权结果精确等于 ā：
//!
//!   (f·A1 + B1) / (f·A0 + B0 + W_a) = ā
//!   A0 = Σ_{S+} w_v,  A1 = Σ_{S+} w_v·a_v
//!   B0 = Σ_{S−} w_v,  B1 = Σ_{S−} w_v·a_v
//!   W_a = Σ_{弃权} w_v
//!
//!   ⇒ f = (ā·B0 − B1 + ā·W_a) / (A1 − ā·A0)
//!        = (Σ_{S−} w_v·(ā − a_v) + ā·W_a)  /  Σ_{S+} w_v·(a_v − ā)
//! ~~~
//!
//! 第二个等式的写法是**数值安全**的关键：分子只累加非负项（S− 的差距项和
//! 弃权票的 ā·W_a 都 ≥ 0），分母只对 S+ 累加非负项，天然不会出现
//! 「两个大数相减吃掉有效位」。这就是「只削峰、不填谷」：低于平均的来源
//! 完全不动，高于平均的来源一起被压。
//!
//! ## 为什么弃权票只进分母、不进 ā
//!
//! P0① 之后群体项的分母是**我评过的全部来源**：没有观点的来源照样拿走自己
//! 那份额度、投一张 0 票（弃权），而不是从分母里消失。闭式解必须用同一个
//! 坐标系，否则目标线对不上实测：
//!
//! * 只把弃权票并进**目标方程的分母**（`W_a`），不并进 ā——弃权票没有读数，
//!   它不是「给 A 打 0 分」，把它算进 ā 等于把目标线定义成「平均每个来源
//!   （含沉默者）怎么看我」，目标会随覆盖率漂移：覆盖率越低目标越低，
//!   机制就变成「覆盖不足就少罚」。
//! * 反过来，**不并进目标方程**就是 P0 之前的实测缺陷：3 个表过态来源
//!   （读数 1/0/0，份额 0.6/0.2/0.2）+ 1 个已评分无观点的来源时，目标 ā = 1/3，
//!   压完却落到 **0.25**（见 `审计/P0-core.md` §7.3）。旧公式对 S+ 求出的
//!   f = 1/3 只让**表过态那部分**回到 ā，弃权票那 1/6 的权重把结果又拖到
//!   `ā·Σw'/(Σw' + W_a) = 0.25`。
//! * 两个数字撞在一起纯属巧合：0.25 既等于旧口径的实际落点，也等于
//!   「把弃权票算进 ā 得到的目标线（1/4）」，但两者的原因完全不同。
//! * `W_a = 0`（每个评过分的来源都有观点）时新公式退化成旧公式，
//!   **逐位相同**：多出来的只有一次 `+ ā·0.0`。
//!
//! 这里刻意**不做分级惩罚**（例如 `1 − μ·(a_v−ā)`）：只要 a_v > ā 就一律处理，
//! 越接近均值的人也被同一个 f 处理。分级惩罚会让刚过均值的人几乎不被罚，
//! 不符合作者要的「一律处理」。
//!
//! 早期还有一个变体 P（逐条压到 ā，`f_v = ā / a_v`），它没有目标线保证、
//! 结果会过冲且幅度无上界，已判定不用并删除。
//!
//! ## 事件语义（本版的核心）
//!
//! * 引擎**不保存坏账户清单**：`report_bad_account` 是事件，调用时按**当前**观点
//!   算一次 f，写进折扣表就结束；
//! * 折扣**不复合**：`discount[v] = min(discount[v], f)`。重复报告同一个账户
//!   不会越压越狠，多次报告不同账户也只是取各自最严厉的一次；
//! * **没有自动恢复**：折扣一旦写下就保持不变，直到应用层显式调用
//!   [crate::Engine::clear_penalties]。作者的决定是「误报的代价是永久的」，
//!   库不替应用层猜测什么时候该撤销；
//! * 因此这里也不再有 EMA / 漂移：0 是一个可以停留的终值，
//!   不再需要「避免 EMA 吸收态」的存储下限。
//!
//! ## 自动退出的条件
//!
//! **S+ 为空 ⇔ 所有人对 A 的态度完全一样**（不管一样地高还是一样地低）：
//! 此时没有「谁比平均更亲近 A」可言，f = 1，机制什么也不做。
//! 这是数学上的自动退出，不是缺陷：它正好解释了为什么本机制抓不到
//! 「整个圈子都包庇 A」——那种情况下没有相对高低，无从下手，只能靠应用层兜底。
//!
//! 注意均值性质保证 **S− 永远不空**（至少有一个 a_v ≤ ā），所以「S− 为空」那条
//! 守卫只是防御性的，不是真实场景。
//!
//! ## 边界（全部有测试）
//!
//! * S+ 为空 → f = 1；
//! * S− 为空 / S− 的权重和为 0 → f = 1（统一缩放对加权平均无效，防御性）；
//! * A1 − ā·A0 ≤ eps（没有可压缩的差距）→ f = 1；
//! * f 夹到 [0,1]；非有限值一律按 1（不惩罚），绝不污染全表；
//! * 加权平均本来就不高于 ā（G_A ≤ ā）时，闭式解给出 f ≥ 1，夹紧后就是 1：
//!   本来没被放大，不用管。

use tm_store::AccountIdx;

/// 除法保护阈值。
///
/// `A1 − ā·A0` 是「S+ 相对均值多出来的加权质量」。它小于这个量时，
/// 目标线 ā 与当前加权结果几乎重合，强行求解只会产生巨大的 f / 数值噪声，
/// 所以直接判为「不需要处理」。
pub(crate) const EPS: f64 = 1e-12;

/// 统一折扣的一次求解结果。
///
/// `abar` / `ga` 只用于测试与诊断观测，不参与生产路径。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct UniformSolution {
    /// 统一折扣，已夹到 [0,1]。
    pub f: f64,
    /// 哪些来源属于 S+（需要乘 f）。与输入等长——**弃权票永远是 `false`**。
    pub above: Vec<bool>,
    /// 未加权平均 ā（观测用）。只对表过态的读数求，不含弃权票。
    pub abar: f64,
    /// 压缩前的加权平均 G_A（观测用）。分母含弃权票的权重，与 [crate::Engine::query] 同口径。
    pub ga: f64,
}

/// 把读数夹到 [0,1]；NaN / ±inf 一律当 0。
///
/// 压缩观点（Count Sketch）的估值可能是负数或超过 1，必须先夹紧；
/// 非有限值属于「观点损坏」，按「没表态」处理，绝不能让它进入均值或权重和。
pub(crate) fn sanitize_reading(a: f64) -> f64 {
    if a.is_finite() {
        a.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 把折扣夹到 [0,1]；非有限值当 1（不惩罚）。
pub(crate) fn sanitize_discount(d: f64) -> f64 {
    if d.is_finite() {
        d.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// 统一折扣：均值以上统一折扣。
///
/// `weights[i]` 是来源 i 的权重 `w_v = 份额 × 衰减`（必须非负）；
/// `readings[i]` 是它对攻击者 A 的读数（内部会夹到 [0,1]），
/// `Some(x)` 表示这份观点**确实覆盖了 A**，`None` 表示**弃权**：
/// 没有观点缓存，或观点是在 A 登记之前生成的（`mass_covers` 没覆盖到它）。
///
/// 两种来源在方程里的角色不同：
///
/// * 表过态的来源：读数进 ā 的未加权平均，权重进 A0/B0；
/// * 弃权来源：**不进 ā**（它没有读数，不是「给 A 打 0 分」），但权重进
///   目标方程的分母 `W_a`——[crate::Engine::query] 的分母含它，闭式解必须
///   用同一个坐标系，否则目标线与实测对不上（详见模块文档）。
///
/// 返回的 `f` 满足：把 S+ 里所有来源的权重乘 f 后，
/// `(f·A1 + B1) / (f·A0 + B0 + W_a)` 精确等于 ā（除非触发边界守卫）。
/// 一个表过态的人都没有时 ā 未定义，直接 `f = 1`（机制自动退出）。
pub(crate) fn solve_uniform(weights: &[f64], readings: &[Option<f64>]) -> UniformSolution {
    let n = weights.len().min(readings.len());
    let mut above = vec![false; n];
    if n == 0 {
        return UniformSolution {
            f: 1.0,
            above,
            abar: 0.0,
            ga: 0.0,
        };
    }

    // ā 只对**表过态**的读数求平均：弃权票没有读数，它不是「给 A 打 0 分」。
    // 把弃权票算进 ā 会让目标线随覆盖率漂移（覆盖率越低目标越低），
    // 等于「覆盖不足就少罚」——那不是这个机制想表达的东西。
    let mut stated = 0usize;
    let mut stated_sum = 0.0f64;
    for a in readings[..n].iter().flatten() {
        stated += 1;
        stated_sum += sanitize_reading(*a);
    }
    if stated == 0 {
        // 一个人都没对 A 表过态：目标线未定义，机制自动退出。
        return UniformSolution {
            f: 1.0,
            above,
            abar: 0.0,
            ga: 0.0,
        };
    }
    let abar = stated_sum / stated as f64;
    if !abar.is_finite() {
        return UniformSolution {
            f: 1.0,
            above,
            abar: 0.0,
            ga: 0.0,
        };
    }

    let mut num = 0.0f64; // Σ_{S−} w·(ā − a_v) ≥ 0
    let mut den = 0.0f64; // Σ_{S+} w·(a_v − ā) ≥ 0
    let mut weight_below = 0.0f64;
    let mut weight_above = 0.0f64;
    let mut weight_abstain = 0.0f64; // W_a：弃权票的权重和，进目标方程的分母
    let mut weighted_sum = 0.0f64;
    let mut weight_sum = 0.0f64;
    let mut has_above = false;
    let mut has_below = false;

    for i in 0..n {
        let w = weights[i];
        let w_ok = w.is_finite() && w > 0.0;

        let Some(raw) = readings[i] else {
            // 弃权票：不参与 S+/S− 的划分（没有读数可比），但份额照样进分母。
            // 它永远不会被乘 f —— 一个没表态的人没有「峰」可削。
            if w_ok {
                weight_abstain += w;
                weight_sum += w;
            }
            continue;
        };

        let ai = sanitize_reading(raw);
        if ai > abar {
            has_above = true;
            above[i] = true;
            if w_ok {
                den += w * (ai - abar);
                weight_above += w;
            }
        } else {
            has_below = true;
            if w_ok {
                num += w * (abar - ai);
                weight_below += w;
            }
        }

        if w_ok {
            weight_sum += w;
            weighted_sum += w * ai;
        }
    }

    // 观测口径与 query 一致：分母含弃权票的份额，分子只有表过态来源贡献读数。
    let ga = if weight_sum > 0.0 {
        weighted_sum / weight_sum
    } else {
        0.0
    };

    // 边界：没有「均值以上」的人（表过态的人态度一样）→ 机制自动退出。
    // 没有「均值及以下」的人只可能是异常输入；此时统一缩放对加权平均无效，
    // 同样退出。S− 的权重和为 0 是同一件事的数值版本（全部权重都在 S+）。
    if !has_above || !has_below || weight_above <= 0.0 || weight_below <= 0.0 {
        return UniformSolution {
            f: 1.0,
            above,
            abar,
            ga,
        };
    }
    // 分子 = S− 的差距项 + 弃权票贡献的 ā·W_a，两者都非负，不会吃掉有效位。
    let num = num + abar * weight_abstain;
    if !num.is_finite() || !den.is_finite() || den <= EPS {
        return UniformSolution {
            f: 1.0,
            above,
            abar,
            ga,
        };
    }
    let f = num / den;
    let f = if f.is_finite() {
        f.clamp(0.0, 1.0)
    } else {
        1.0
    };
    UniformSolution { f, above, abar, ga }
}

/// 把闭式解给出的原始折扣按「惩罚强度」λ 缩放：`f_eff = 1 − λ·(1 − f)`。
///
/// λ = 1（默认，[crate::EngineConfig::penalty_strength]）时原样使用，
/// 因此统一折扣能精确压到 ā；λ < 1 是刻意留手（例如只允许削掉一半差距）；
/// λ = 0 时 f_eff = 1，等于完全关闭（报告不会写入任何折扣）。
/// λ 非法（NaN / 负数 / inf）时按 1 处理，f 非法时按 1 处理。
pub(crate) fn apply_strength(f: f64, lambda: f64) -> f64 {
    let lambda = if lambda.is_finite() {
        lambda.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let f = sanitize_discount(f);
    sanitize_discount(1.0 - lambda * (1.0 - f))
}

/// 一次报告里，一个来源的快照（引擎内部用）。
///
/// 一次报告只针对一个坏账户，所以每条快照只需要一个读数；
/// 权重口径与 [crate::Engine::query] 的群体项完全一致（份额 × 衰减，不含折扣）。
///
/// 快照覆盖**我评过的全部来源**，不只是有观点的那部分——这是 P0① 之后的
/// 分母口径，也是 [solve_uniform] 求解时用的坐标系。
#[derive(Clone, Debug)]
pub(crate) struct SourceSnapshot {
    /// 来源编号。
    pub idx: AccountIdx,
    /// `份额 × 衰减`（不含折扣）。没有观点时没有衰减基准，因子按 1（同 query）。
    pub weight: f64,
    /// 对坏账户的读数：`Some` = 这份观点覆盖了 A（已夹到 [0,1] 并过了噪声底）;
    /// `None` = 弃权（没有观点，或观点是 A 登记之前生成的）。
    pub reading: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_store::Rng;

    /// 把 [0,1) 的 u64 变成 [0,1) 的 f64。
    fn unit(rng: &mut Rng) -> f64 {
        (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 与 [crate::Engine::query] 同口径的加权平均：
    /// 分母 = **全部**（含弃权）权重，分子只累加表过态来源的 `w·a`。
    /// `factors` 是每个来源的折扣。
    fn weighted_mean(weights: &[f64], readings: &[Option<f64>], factors: &[f64]) -> f64 {
        let mut num = 0.0;
        let mut den = 0.0;
        for i in 0..weights.len() {
            let w = weights[i] * factors[i];
            if let Some(a) = readings[i] {
                num += w * a;
            }
            den += w;
        }
        if den > 0.0 {
            num / den
        } else {
            0.0
        }
    }

    /// 全部表过态的随机样本：闭式解的主性质——压完后的加权平均精确落在 ā。
    ///
    /// 这条是**回归保护**：没有弃权票时新公式退化成旧公式，4000 组随机样本
    /// 仍必须逐组命中目标，触发惩罚的样本数也不能比旧口径少。
    #[test]
    fn uniform_closed_form_hits_target() {
        let mut rng = Rng::new(0xC1BA_D157);
        let mut triggered = 0usize;

        for case in 0..4000 {
            let n = 2 + rng.next_below(48);
            let weights: Vec<f64> = (0..n).map(|_| 0.01 + 4.0 * unit(&mut rng)).collect();
            let readings: Vec<Option<f64>> = (0..n).map(|_| Some(unit(&mut rng))).collect();

            let sol = solve_uniform(&weights, &readings);
            assert!(
                sol.f.is_finite() && (0.0..=1.0).contains(&sol.f),
                "case {case}: f 越界 {}",
                sol.f
            );

            let mut factors = vec![1.0; n];
            for (f, &above) in factors.iter_mut().zip(sol.above.iter()) {
                if above {
                    *f = sol.f;
                }
            }
            let before = weighted_mean(&weights, &readings, &vec![1.0; n]);

            if sol.f < 1.0 {
                triggered += 1;
                let after = weighted_mean(&weights, &readings, &factors);
                assert!(
                    (after - sol.abar).abs() < 1e-5,
                    "case {case}: f={} 压完后 {after} 未回到 ā={}（差 {}）",
                    sol.f,
                    sol.abar,
                    (after - sol.abar).abs()
                );
            } else if before <= sol.abar {
                // 本来就没被放大：不罚，符合「只削峰」。
                let after = weighted_mean(&weights, &readings, &factors);
                assert!((after - before).abs() < 1e-12);
            }
        }

        assert!(triggered >= 1500, "触发惩罚的样本太少: {triggered}");
    }

    /// 有弃权票的随机样本：同一个主性质必须成立。
    ///
    /// 弃权票只进分母、不进 ā，所以 `ga`（观测用的加权平均）会低于 `abar`，
    /// 但压完后 `G_A` 仍然精确等于 ā——这正是 P0 前实测落到 0.25 的那类场景。
    #[test]
    fn uniform_closed_form_hits_target_with_abstentions() {
        let mut rng = Rng::new(0xB1AD_1575);
        let mut triggered = 0usize;
        let mut with_abstain = 0usize;

        for case in 0..4000 {
            let n = 3 + rng.next_below(47);
            // 权重跨越 6 个数量级：这样「权重集中在高读数来源」的情形足够多，
            // 才压得出 G_A > ā 的样本（否则弃权票把 G_A 稀释到目标线以下，
            // 闭式解一律给出 f ≥ 1，测试就退化成了「从不触发」）。
            let weights: Vec<f64> = (0..n)
                .map(|_| 10f64.powf(-3.0 + 6.0 * unit(&mut rng)))
                .collect();
            // 每个来源独立地有 25% 概率弃权。
            let readings: Vec<Option<f64>> = (0..n)
                .map(|_| {
                    if unit(&mut rng) < 0.25 {
                        None
                    } else {
                        Some(unit(&mut rng))
                    }
                })
                .collect();
            let abstentions = readings.iter().filter(|r| r.is_none()).count();
            if abstentions > 0 && abstentions < n {
                with_abstain += 1;
            }

            let sol = solve_uniform(&weights, &readings);
            assert!(
                sol.f.is_finite() && (0.0..=1.0).contains(&sol.f),
                "case {case}: f 越界 {}",
                sol.f
            );
            assert!(
                sol.abar.is_finite() && (0.0..=1.0).contains(&sol.abar),
                "case {case}: ā 越界 {}",
                sol.abar
            );
            // 弃权票永远不该进入 S+。
            for (i, r) in readings.iter().enumerate() {
                if r.is_none() {
                    assert!(!sol.above[i], "case {case}: 弃权票 {i} 不应在 S+");
                }
            }

            let mut factors = vec![1.0; n];
            for (f, &above) in factors.iter_mut().zip(sol.above.iter()) {
                if above {
                    *f = sol.f;
                }
            }
            if sol.f < 1.0 {
                triggered += 1;
                let after = weighted_mean(&weights, &readings, &factors);
                assert!(
                    (after - sol.abar).abs() < 1e-5,
                    "case {case}: f={} 压完后 {after} 未回到 ā={}（差 {}）",
                    sol.f,
                    sol.abar,
                    (after - sol.abar).abs()
                );
            }
        }

        assert!(triggered >= 800, "触发惩罚的样本太少: {triggered}");
        assert!(with_abstain >= 3000, "含弃权票的样本太少: {with_abstain}");
    }

    /// 已知小例子：w=[1,1,2], a=[0.1,0.5,0.9], ā=0.5 → f=0.5。
    ///
    /// 没有弃权票时的新旧公式**等价**，这条钉住的就是那个「等价」。
    #[test]
    fn uniform_simple_example() {
        let w = [1.0, 1.0, 2.0];
        let a = [Some(0.1), Some(0.5), Some(0.9)];
        let sol = solve_uniform(&w, &a);
        assert!((sol.abar - 0.5).abs() < 1e-12);
        assert!((sol.f - 0.5).abs() < 1e-12, "f={}", sol.f);
        let factors = [1.0, 1.0, sol.f];
        let after = weighted_mean(&w, &a, &factors);
        assert!((after - 0.5).abs() < 1e-12, "after={after}");
    }

    /// **弃权票只进分母、不进 ā** 的最小例子（P0 实测 0.25 的那个场景）。
    ///
    /// ~~~text
    /// w       = [3, 1, 1, 1]       最后一个来源弃权（份额 1/6）
    /// a       = [1, 0, 0, 弃权]    ā = 1/3（只对三个表过态的求平均）
    /// 旧公式：f = (1/3)(1+1) / 3·(2/3) = 1/2 ... 旧口径没把 W_a 并进分母时 f = 1/3
    /// 新公式：f = (1/3)(1+1) + (1/3)·1 / 2 = 1/2
    ///   G_A = (0.5·3·1) / (0.5·3 + 2 + 1) = 1.5 / 4.5 = 1/3 = ā ✔
    ///   旧口径：f = 1/3 时 G_A = 1 / (1 + 2 + 1) = 1/4 ✘（实测就是 0.25）
    /// ~~~
    #[test]
    fn abstention_only_enters_the_denominator() {
        let w = [3.0, 1.0, 1.0, 1.0];
        let a = [Some(1.0), Some(0.0), Some(0.0), None];
        let sol = solve_uniform(&w, &a);
        assert!((sol.abar - 1.0 / 3.0).abs() < 1e-12, "ā={}", sol.abar);
        // 观测用的 G_A 与 query 同口径：分母包含弃权票 → 3/6 = 0.5。
        assert!((sol.ga - 0.5).abs() < 1e-12, "G_A={}", sol.ga);
        assert!((sol.f - 0.5).abs() < 1e-12, "f={}", sol.f);
        assert!(!sol.above[3], "弃权票不在 S+");

        let factors = [sol.f, 1.0, 1.0, 1.0];
        let after = weighted_mean(&w, &a, &factors);
        assert!((after - 1.0 / 3.0).abs() < 1e-12, "压完后 {after}");
    }

    /// 弃权票不会改变 ā：往任何场景里加弃权来源，目标线都不动。
    #[test]
    fn abstentions_do_not_shift_the_mean() {
        let stated_w = [2.0, 1.0, 1.0];
        let stated_a = [Some(1.0), Some(0.2), Some(0.1)];
        let sol0 = solve_uniform(&stated_w, &stated_a);

        let mut w = stated_w.to_vec();
        let mut a = stated_a.to_vec();
        for k in 0..5 {
            w.push(0.5 + k as f64);
            a.push(None);
            let sol = solve_uniform(&w, &a);
            assert!(
                (sol.abar - sol0.abar).abs() < 1e-12,
                "加了 {} 个弃权票后 ā 变了: {} != {}",
                k + 1,
                sol.abar,
                sol0.abar
            );
            // 但目标更难达到：f 随弃权权重单调上升（罚得更重）。
            if k > 0 {
                let prev = solve_uniform(&w[..w.len() - 1], &a[..a.len() - 1]);
                assert!(
                    sol.f >= prev.f - 1e-12,
                    "弃权票增加后 f 不应变小: {} -> {}",
                    prev.f,
                    sol.f
                );
            }
        }
    }

    /// 一个表过态的人都没有 → ā 未定义 → f = 1（自动退出，不 panic）。
    #[test]
    fn no_stated_reading_exits() {
        let w = [1.0, 2.0, 3.0];
        let a = [None, None, None];
        let sol = solve_uniform(&w, &a);
        assert_eq!(sol.f, 1.0);
        assert!((sol.abar - 0.0).abs() < 1e-12);
        assert!((sol.ga - 0.0).abs() < 1e-12);
        assert!(sol.above.iter().all(|x| !x));
        // 单个来源也没有意义（S− 为空的那类守卫）。
        assert_eq!(solve_uniform(&[1.0], &[None]).f, 1.0);
        assert_eq!(solve_uniform(&[1.0], &[Some(1.0)]).f, 1.0);
    }

    /// 权重为 0 的弃权票是纯 no-op：不改变 f / ā / G_A 的任何一位。
    #[test]
    fn zero_weight_abstention_is_a_noop() {
        let w = [3.0, 1.0, 1.0];
        let a = [Some(1.0), Some(0.0), Some(0.0)];
        let base = solve_uniform(&w, &a);

        let w2 = [3.0, 1.0, 1.0, 0.0];
        let a2 = [Some(1.0), Some(0.0), Some(0.0), None];
        let with = solve_uniform(&w2, &a2);
        assert_eq!(base.f.to_bits(), with.f.to_bits(), "f 逐位不变");
        assert_eq!(base.abar.to_bits(), with.abar.to_bits(), "ā 逐位不变");
        assert_eq!(base.ga.to_bits(), with.ga.to_bits(), "G_A 逐位不变");
    }

    /// 大家态度完全一样 → S+ 为空 → f=1（自动退出）。
    #[test]
    fn uniform_all_equal_exits() {
        for v in [0.0, 0.123, 0.5, 1.0] {
            let w = [1.0, 2.0, 3.0];
            let a = [Some(v), Some(v), Some(v)];
            let sol = solve_uniform(&w, &a);
            assert_eq!(sol.f, 1.0, "a 全为 {v} 时不应惩罚");
            assert!(sol.above.iter().all(|x| !x));
        }
    }

    /// 全部低于或等于均值（只有全相等才可能）→ 不罚。
    #[test]
    fn uniform_below_or_at_mean_does_nothing() {
        let w = [1.0, 1.0, 1.0];
        let a = [Some(0.1), Some(0.1), Some(0.1)];
        assert_eq!(solve_uniform(&w, &a).f, 1.0);
        let a = [Some(0.1), Some(0.2), Some(0.3)];
        let sol = solve_uniform(&w, &a);
        // 加权平均 = 0.2 = ā，已经在目标线上，不需要压。
        assert!((sol.ga - sol.abar).abs() < 1e-12);
        assert_eq!(sol.f, 1.0);
    }

    /// 边界守卫：没有 S+、没有 S−、没有可压差距、非有限值，一律 f=1。
    #[test]
    fn uniform_guards() {
        // 空输入
        assert_eq!(solve_uniform(&[], &[]).f, 1.0);
        // 权重全 0：S+ 非空但没有任何加权质量
        let sol = solve_uniform(&[0.0, 0.0], &[Some(0.1), Some(0.9)]);
        assert_eq!(sol.f, 1.0);
        // 读数非有限：NaN 当 0，inf 夹到 1，不能污染结果
        let sol = solve_uniform(
            &[1.0, 1.0, 1.0],
            &[Some(f64::NAN), Some(f64::INFINITY), Some(-3.0)],
        );
        assert!(sol.f.is_finite() && (0.0..=1.0).contains(&sol.f));
        assert!(sol.abar.is_finite());
        // 权重 NaN / inf：按无效权重处理，不能产生 NaN 折扣
        let sol = solve_uniform(
            &[f64::NAN, f64::INFINITY, 1.0],
            &[Some(0.1), Some(0.5), Some(0.9)],
        );
        assert!(sol.f.is_finite() && (0.0..=1.0).contains(&sol.f));
        // 弃权票的权重是 NaN / inf：同样按无效权重处理，不进分母。
        let clean = solve_uniform(&[3.0, 1.0, 1.0], &[Some(1.0), Some(0.0), Some(0.0)]);
        let dirty = solve_uniform(
            &[3.0, 1.0, 1.0, f64::NAN, f64::INFINITY],
            &[Some(1.0), Some(0.0), Some(0.0), None, None],
        );
        assert_eq!(clean.f.to_bits(), dirty.f.to_bits(), "坏权重不改变 f");
    }

    /// 闭式解可能 >1（加权平均本来就低于 ā）：必须夹到 1，不能「反向奖励」。
    #[test]
    fn uniform_clamps_to_one_when_already_low() {
        // 高读数来源权重极小：G_A < ā
        let w = [100.0, 0.001];
        let a = [Some(0.0), Some(1.0)];
        let sol = solve_uniform(&w, &a);
        assert!(sol.ga < sol.abar);
        assert_eq!(sol.f, 1.0);
    }

    /// 符号性质：随机输入下 f 永远在 [0,1]（不会奖励、不会变负）。
    #[test]
    fn uniform_factor_always_unit_interval() {
        let mut rng = Rng::new(0xC1BA_D151);
        for _ in 0..2000 {
            let n = 1 + rng.next_below(30);
            let w: Vec<f64> = (0..n)
                .map(|_| 10f64.powf(-6.0 + 12.0 * unit(&mut rng)))
                .collect();
            let a: Vec<Option<f64>> = (0..n)
                .map(|_| {
                    if unit(&mut rng) < 0.3 {
                        None
                    } else {
                        Some(unit(&mut rng))
                    }
                })
                .collect();
            let sol = solve_uniform(&w, &a);
            assert!((0.0..=1.0).contains(&sol.f), "f={}", sol.f);
            assert!(sol.f.is_finite());
        }
    }

    /// 强度 λ：λ=0 完全不罚；λ=0.5 只削一半差距；非法 λ 按 1。
    #[test]
    fn strength_interpolates() {
        assert_eq!(apply_strength(0.2, 0.0), 1.0);
        assert!((apply_strength(0.2, 0.5) - 0.6).abs() < 1e-12);
        assert!((apply_strength(0.2, 1.0) - 0.2).abs() < 1e-12);
        assert_eq!(apply_strength(f64::NAN, 1.0), 1.0);
        assert!((apply_strength(0.2, f64::NAN) - 0.2).abs() < 1e-12);
        assert!((apply_strength(0.2, -1.0) - 1.0).abs() < 1e-12);
    }
}
