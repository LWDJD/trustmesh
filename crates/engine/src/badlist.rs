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
//! ā   = 所有来源的 a_v 的**未加权**平均
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
//!   (f·A1 + B1) / (f·A0 + B0) = ā
//!   A0 = Σ_{S+} w_v,  A1 = Σ_{S+} w_v·a_v
//!   B0 = Σ_{S−} w_v,  B1 = Σ_{S−} w_v·a_v
//!
//!   ⇒ f = (ā·B0 − B1) / (A1 − ā·A0)
//!        = Σ_{S−} w_v·(ā − a_v)  /  Σ_{S+} w_v·(a_v − ā)
//! ~~~
//!
//! 第二个等式的写法是**数值安全**的关键：分子只对 S− 累加非负项，
//! 分母只对 S+ 累加非负项，天然不会出现「两个大数相减吃掉有效位」。
//! 这就是「只削峰、不填谷」：低于平均的来源完全不动，高于平均的来源一起被压。
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
    /// 哪些来源属于 S+（需要乘 f）。与输入等长。
    pub above: Vec<bool>,
    /// 未加权平均 ā（观测用）。
    pub abar: f64,
    /// 压缩前的加权平均 G_A（观测用）。
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
/// `readings[i]` 是它对攻击者 A 的读数（内部会夹到 [0,1]）。
///
/// 返回的 `f` 满足：把 S+ 里所有来源的权重乘 f 后，**全体来源的加权平均**
/// 精确等于 ā（除非触发了上面列的边界守卫）。
pub(crate) fn solve_uniform(weights: &[f64], readings: &[f64]) -> UniformSolution {
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

    let a: Vec<f64> = readings[..n].iter().map(|&x| sanitize_reading(x)).collect();
    let abar = a.iter().sum::<f64>() / n as f64;
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
    let mut weighted_sum = 0.0f64;
    let mut weight_sum = 0.0f64;
    let mut has_above = false;
    let mut has_below = false;

    for i in 0..n {
        let ai = a[i];
        let w = weights[i];
        let w_ok = w.is_finite() && w > 0.0;

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

    let ga = if weight_sum > 0.0 {
        weighted_sum / weight_sum
    } else {
        0.0
    };

    // 边界：没有「均值以上」的人（大家态度一样）→ 机制自动退出。
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
    if !den.is_finite() || den <= EPS {
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
#[derive(Clone, Debug)]
pub(crate) struct SourceSnapshot {
    /// 来源编号。
    pub idx: AccountIdx,
    /// `份额 × 衰减`（不含折扣）。
    pub weight: f64,
    /// 对坏账户的读数，已夹到 [0,1] 并过了噪声底。
    pub reading: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_store::Rng;

    /// 把 [0,1) 的 u64 变成 [0,1) 的 f64。
    fn unit(rng: &mut Rng) -> f64 {
        (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn weighted_mean(weights: &[f64], readings: &[f64], factors: &[f64]) -> f64 {
        let mut num = 0.0;
        let mut den = 0.0;
        for i in 0..weights.len() {
            let w = weights[i] * factors[i];
            num += w * readings[i];
            den += w;
        }
        if den > 0.0 {
            num / den
        } else {
            0.0
        }
    }

    /// 闭式解的主性质：随机 4000 组，凡是真的触发惩罚（f < 1）的，
    /// 压完后的加权平均必须精确落在 ā（误差 < 1e-5）。
    #[test]
    fn uniform_closed_form_hits_target() {
        let mut rng = Rng::new(0xC1BA_D157);
        let mut triggered = 0usize;

        for case in 0..4000 {
            let n = 2 + rng.next_below(48);
            let weights: Vec<f64> = (0..n).map(|_| 0.01 + 4.0 * unit(&mut rng)).collect();
            let readings: Vec<f64> = (0..n).map(|_| unit(&mut rng)).collect();

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

    /// 已知小例子：w=[1,1,2], a=[0.1,0.5,0.9], ā=0.5 → f=0.5。
    #[test]
    fn uniform_simple_example() {
        let w = [1.0, 1.0, 2.0];
        let a = [0.1, 0.5, 0.9];
        let sol = solve_uniform(&w, &a);
        assert!((sol.abar - 0.5).abs() < 1e-12);
        assert!((sol.f - 0.5).abs() < 1e-12, "f={}", sol.f);
        let factors = [1.0, 1.0, sol.f];
        let after = weighted_mean(&w, &a, &factors);
        assert!((after - 0.5).abs() < 1e-12, "after={after}");
    }

    /// 大家态度完全一样 → S+ 为空 → f=1（自动退出）。
    #[test]
    fn uniform_all_equal_exits() {
        for v in [0.0, 0.123, 0.5, 1.0] {
            let w = [1.0, 2.0, 3.0];
            let a = [v, v, v];
            let sol = solve_uniform(&w, &a);
            assert_eq!(sol.f, 1.0, "a 全为 {v} 时不应惩罚");
            assert!(sol.above.iter().all(|x| !x));
        }
    }

    /// 全部低于或等于均值（只有全相等才可能）→ 不罚。
    #[test]
    fn uniform_below_or_at_mean_does_nothing() {
        let w = [1.0, 1.0, 1.0];
        let a = [0.1, 0.1, 0.1];
        assert_eq!(solve_uniform(&w, &a).f, 1.0);
        let a = [0.1, 0.2, 0.3];
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
        let sol = solve_uniform(&[0.0, 0.0], &[0.1, 0.9]);
        assert_eq!(sol.f, 1.0);
        // 读数非有限：NaN 当 0，inf 夹到 1，不能污染结果
        let sol = solve_uniform(&[1.0, 1.0, 1.0], &[f64::NAN, f64::INFINITY, -3.0]);
        assert!(sol.f.is_finite() && (0.0..=1.0).contains(&sol.f));
        assert!(sol.abar.is_finite());
        // 权重 NaN / inf：按无效权重处理，不能产生 NaN 折扣
        let sol = solve_uniform(&[f64::NAN, f64::INFINITY, 1.0], &[0.1, 0.5, 0.9]);
        assert!(sol.f.is_finite() && (0.0..=1.0).contains(&sol.f));
    }

    /// 闭式解可能 >1（加权平均本来就低于 ā）：必须夹到 1，不能「反向奖励」。
    #[test]
    fn uniform_clamps_to_one_when_already_low() {
        // 高读数来源权重极小：G_A < ā
        let w = [100.0, 0.001];
        let a = [0.0, 1.0];
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
            let a: Vec<f64> = (0..n).map(|_| unit(&mut rng)).collect();
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
