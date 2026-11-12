//! # badlist —— 受害者反馈机制的核心算法
//!
//! 这是整套设计里**唯一一个有受害者反馈的机制**：其它机制只看「你评了谁」，
//! 不看「你护了谁」。应用层判断「我正在被谁攻击」，把那些账户（记作 A）交给引擎；
//! 引擎再看**我信任的来源**是怎么评价 A 的：
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
//! ## 变体 U（默认，Uniform）：均值以上统一折扣
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
//! ## 变体 P（PerSource，对照用）：逐条压到均值
//!
//! ~~~text
//! 对 v ∈ S+：f_v = ā / a_v
//! ~~~
//!
//! 它会把每条**读数**都压到 ā，但权重也一起变了，所以最终加权结果会**过冲到 ā 以下**
//! （S− 完全不动，压得越狠，低分来源的权重占比反而越大）。变体 P 只用于对照实验，
//! 默认不启用。
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
//!
//! ## 多个攻击者：取最小值，不连乘
//!
//! 每个 A 独立算出一个 f_v^A，最终 `f_v = min_A f_v^A`。
//! **不能连乘**：连乘会复合放大——两个各罚 50% 就变成 75%，误伤没有上界；
//! 取 min 让每个来源的惩罚不超过「最严厉的那个攻击者」，有上界、可预测。

use tm_store::AccountIdx;

/// 除法保护阈值。
///
/// `A1 − ā·A0` 是「S+ 相对均值多出来的加权质量」。它小于这个量时，
/// 目标线 ā 与当前加权结果几乎重合，强行求解只会产生巨大的 f / 数值噪声，
/// 所以直接判为「不需要处理」。
pub(crate) const EPS: f64 = 1e-12;

/// 变体 U 的一次求解结果。
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

/// 变体 U：均值以上统一折扣。
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

/// 变体 P：逐条压到均值 `f_v = ā / a_v`（仅 S+；S− 为 1）。
///
/// 返回与输入等长的折扣数组，以及观测用的 ā。
/// 非有限读数按 0 处理；除法结果一律夹到 [0,1]。
pub(crate) fn solve_per_source(weights: &[f64], readings: &[f64]) -> (Vec<f64>, f64) {
    let n = weights.len().min(readings.len());
    let mut out = vec![1.0f64; n];
    if n == 0 {
        return (out, 0.0);
    }
    let a: Vec<f64> = readings[..n].iter().map(|&x| sanitize_reading(x)).collect();
    let abar = a.iter().sum::<f64>() / n as f64;
    if !abar.is_finite() {
        return (out, 0.0);
    }
    for i in 0..n {
        // 权重为 0 的来源不参与群体项，给它什么折扣都无所谓；保持 1 更干净。
        let w_ok = weights[i].is_finite() && weights[i] > 0.0;
        if !w_ok || a[i] <= abar {
            continue;
        }
        let f = abar / a[i];
        out[i] = if f.is_finite() {
            f.clamp(0.0, 1.0)
        } else {
            1.0
        };
    }
    (out, abar)
}

/// 把闭式解给出的原始折扣按「惩罚强度」λ 缩放：`f_eff = 1 − λ·(1 − f)`。
///
/// λ = 1（默认，[crate::EngineConfig::badlist_mu_max]）时原样使用，
/// 因此变体 U 能精确压到 ā；λ < 1 是刻意留手（例如只允许削掉一半差距）。
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

/// EMA 平滑：`new = old^(1−α) · f^α`。
///
/// α = 1 时直接返回 f（不平滑）。α 非法时按 1 处理。
/// 这样 badlist 清空后（f = 1）折扣会从旧值几何式地回到 1.0，误报可自然恢复。
pub(crate) fn ema(old: f64, f: f64, alpha: f64) -> f64 {
    let old = sanitize_discount(old);
    let f = sanitize_discount(f);
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if alpha >= 1.0 {
        return f;
    }
    if alpha <= 0.0 {
        return old;
    }
    sanitize_discount(old.powf(1.0 - alpha) * f.powf(alpha))
}

/// 一次扫描里，一个来源的快照（引擎内部用）。
#[derive(Clone, Debug)]
pub(crate) struct SourceSnapshot {
    /// 来源编号。
    pub idx: AccountIdx,
    /// `份额 × 衰减`（不含折扣）。
    pub weight: f64,
    /// 对 badlist 里每个攻击者的读数，顺序与 badlist 一致。
    pub readings: Vec<f64>,
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

    /// 变体 P：每条都压到 ā，结果必然**过冲**到 ā 以下。
    #[test]
    fn per_source_overshoots() {
        let w = [1.0, 1.0, 1.0];
        let a = [0.2, 0.5, 0.8];
        let (factors, abar) = solve_per_source(&w, &a);
        assert!((abar - 0.5).abs() < 1e-12);
        assert!((factors[0] - 1.0).abs() < 1e-12);
        assert!((factors[1] - 1.0).abs() < 1e-12);
        assert!((factors[2] - 0.625).abs() < 1e-12);
        let after = weighted_mean(&w, &a, &factors);
        assert!(after < abar - 1e-6, "P 应过冲: after={after}, abar={abar}");
    }

    /// 变体 P：全相等 → 全 1；非有限读数安全。
    #[test]
    fn per_source_guards() {
        let (f, _) = solve_per_source(&[1.0, 1.0], &[0.3, 0.3]);
        assert_eq!(f, vec![1.0, 1.0]);
        let (f, _) = solve_per_source(&[1.0, 1.0], &[f64::NAN, 0.9]);
        assert!(f.iter().all(|x| x.is_finite() && (0.0..=1.0).contains(x)));
        assert_eq!(solve_per_source(&[], &[]).0.len(), 0);
    }

    /// 强度 λ：λ=0 完全不罚；λ=0.5 只削一半差距。
    #[test]
    fn strength_interpolates() {
        assert_eq!(apply_strength(0.2, 0.0), 1.0);
        assert!((apply_strength(0.2, 0.5) - 0.6).abs() < 1e-12);
        assert!((apply_strength(0.2, 1.0) - 0.2).abs() < 1e-12);
        assert_eq!(apply_strength(f64::NAN, 1.0), 1.0);
        assert!((apply_strength(0.2, f64::NAN) - 0.2).abs() < 1e-12);
    }

    /// EMA：α=1 直接取新值；α<1 从旧值几何式靠近新值；非法 α 按 1。
    #[test]
    fn ema_smooths_and_recovers() {
        assert!((ema(0.5, 1.0, 1.0) - 1.0).abs() < 1e-12);
        let x = ema(0.25, 1.0, 0.5);
        assert!((x - 0.5).abs() < 1e-12, "x={x}");
        let y = ema(x, 1.0, 0.5);
        assert!(y > x && y < 1.0, "应单调回到 1: {x} -> {y}");
        assert!((ema(0.3, 0.8, f64::NAN) - 0.8).abs() < 1e-12);
        assert!((ema(f64::NAN, 0.8, 1.0) - 0.8).abs() < 1e-12);
    }
}
