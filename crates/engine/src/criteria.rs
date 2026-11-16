//! # 反女巫判据的**验收口径**
//!
//! 这个模块存在的理由：判据本身必须写死在代码里，否则每次重跑都会换一套口径，
//! 数字就没法比较。历史上就吃过这个亏——`审计/A6-ablation.md` 定义的六条判据
//! （S1~S6）散落在十几份报告里，其中 S2 有两种粒度（逐观察者对 vs 逐女巫均值）、
//! S4 有五档清单长度下的完全不同结论、S5 有四套互不可比的算法。
//!
//! 本模块把这些口径**固化成常量与公式**，供 `crates/exp` 的验证装置与
//! 应用层的自检共用。它**不改变任何计算行为**，只提供判据。
//!
//! ## 六条判据（口径逐字照抄 `审计/A6-ablation.md` §1.2）
//!
//! * 观察者 = 活着的诚实账户；分布 = [crate::Engine::trust_distribution]（总和恒为 1）
//! * `hm` = 该观察者眼中**所有诚实账户**的均值（没出现在分布里的按 0 计入）
//! * 所有比值 = 该账户在分布里的值 ÷ `hm`
//! * **S1** = 女巫集群成员均值 / hm；另报出现率
//! * **S2** = 注入女巫均值 / hm；**越线率** = 超过 hm 的（观察者,女巫）对 ÷ 全部对
//! * **S3** = 收益方（Promoted）的 dist 值 / hm；另报攻击者自己 / hm
//! * **S4** = 40 个「已 intern 但没人评分」的新密钥：均值 / hm、单点最高 / hm、出现率
//! * **S5** = 把已被诚实观察者评分的来源的观点换成伪造视图后，它自己的声望 ÷ 伪造前
//! * **S6** = 同输入重跑，全部（观察者 × 账户）声望矩阵的最大绝对差
//!
//! ## S3 的特殊性：它**不是**「≤1 就算通过」
//!
//! β = 0.3 时存在一个**解析下限**：只要观察者自己评过收益方、且群体项把它顶满，那么
//!
//!
//! ```text
//! D[T] = β/N + (1−β)/N = 1/N        （T = 收益方，群体项被全部来源顶满）
//! D[H] = β/N                        （H = 一个我评过、但没人背书的诚实账户）
//! 比值 = 1/β
//! ```
//!
//! β 是 `f32`，`0.3f32 = 0.30000001192…`，所以这个地板是 **3.3333332**（不是 10/3）。
//! 也就是说 **S3 ≤ 1 在 β=0.3 下数学上不可能**；把它当验收线只会让人一直「修」一个
//! 不存在的毛病。
//!
//! 正确读法是「**实测值 ÷ 地板**」：`S3 / (1/β)`。
//! * 1.0 表示「群体项把收益方顶到了理论极限」；
//! * < 1 表示还有余量（覆盖率不足、来源衰减等稀释了群体项）；
//! * > 1 表示**超过了 β 能解释的范围**，需要单独解释（实测背书场景是 1.31）。
//!
//! 本模块用 [`s3_floor`] 与 [`s3_relative_to_floor`] 提供这两个量。

/// β 是 `f32`，`0.3f32` 的精确值是 `0.30000001192…`。
///
/// 所以 `1/β` 不能写成 `10.0/3.0`——那是 f64 的 10/3，与 β 的真实倒数差最后几位。
/// 这里用 `1.0 / 0.3f32 as f64` 保证与 [crate::EngineConfig::beta] 的默认值**逐位对齐**。
pub const BETA_DEFAULT: f32 = 0.3;

/// S3 的解析地板 = `1/β`。
///
/// β 取 [`BETA_DEFAULT`] 时为 **3.3333332**（f64 精确值 `3.3333332000000066`）。
///
/// 这个地板只在「观察者自己评过收益方」的前提下成立。观察者没评过收益方时，
/// 个人项是 0，地板不适用——此时比值完全由群体项决定，可能低于地板。
#[must_use]
pub fn s3_floor() -> f64 {
    1.0 / f64::from(BETA_DEFAULT)
}

/// 给定 β 的 S3 地板 = `1/β`。
///
/// * `beta <= 0` 或非有限 → 返回 `f64::INFINITY`（个人项权重为 0 时地板无穷大，
///   因为此时「诚实账户」的声望也完全来自群体项，比值不再有 1/β 这个结构）；
/// * `beta >= 1` → 返回 1.0（个人项独占，收益方与诚实账户的差别只在个人项里）。
#[must_use]
pub fn s3_floor_for(beta: f32) -> f64 {
    if !beta.is_finite() || beta <= 0.0 {
        return f64::INFINITY;
    }
    if beta >= 1.0 {
        return 1.0;
    }
    1.0 / f64::from(beta)
}

/// 把 S3 实测值换算成「**相对地板**」的倍数。
///
/// 返回 `Ok(ratio)` 当且仅当 `floor` 是有限正数；否则 `Err` 说明这个场景下
/// 地板不存在（见 [`s3_floor_for`]）。
///
/// 用法：
/// * `0.5` → 群体项只把收益方顶到理论极限的一半；
/// * `1.0` → 顶满；
/// * `> 1` → 超过 β 能解释的范围，必须单独归因。
pub fn s3_relative_to_floor(s3_value: f64, beta: f32) -> Result<f64, &'static str> {
    let floor = s3_floor_for(beta);
    if !floor.is_finite() || floor <= 0.0 {
        return Err("β 使 1/β 地板无定义（β<=0 或非有限）");
    }
    if !s3_value.is_finite() {
        return Err("S3 实测值非有限");
    }
    Ok(s3_value / floor)
}

/// 一条判据的**判定结果**。四档，不是二元「通过/失败」。
///
/// 二元判定会让「结构上不可能通过」的判据（S3）永远显示失败，
/// 从而掩盖「它其实已经贴着理论极限」这个事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 满足口径（例如 S1 = 0、S2 越线率 = 0、S6 最大差 = 0）
    Pass,
    /// 有明确阈值但未达到（例如 S4 单点最高超过真实均值）
    Fail,
    /// 判据本身在该配置下**结构上不可能达标**（如 S3 在 β<1 时 ≤1）
    /// 此时应看「相对地板」而不是绝对比值
    StructurallyImpossible,
    /// 该场景不产出这条判据（例如 sybil 场景没有 S3 的收益方）
    NotApplicable,
}

impl Verdict {
    /// 是不是「对使用者而言可以接受」的结果。
    ///
    /// [`Verdict::StructurallyImpossible`] 也算可接受——它表示我们已经到了理论上限，
    /// 再「修」也不会有改善，需要换 β 或改分母口径（那是设计决策，不是 bug）。
    #[must_use]
    pub fn acceptable(self) -> bool {
        matches!(
            self,
            Self::Pass | Self::StructurallyImpossible | Self::NotApplicable
        )
    }

    /// 简短中文标签，便于报告输出。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "通过",
            Self::Fail => "失败",
            Self::StructurallyImpossible => "结构上不可达（看相对地板）",
            Self::NotApplicable => "不适用",
        }
    }
}

/// S1 的判定：女巫均值 / hm 必须为 0（结构上抽不到）。
///
/// 阈值 0 是**精确**的：女巫不在任何诚实账户的正分清单里 ⇒ 抽样抽不到 ⇒
/// 个人项 0、群体项 0。任何非 0 都说明结构被破坏了。
#[must_use]
pub fn judge_s1(s1_value: f64) -> Verdict {
    if !s1_value.is_finite() {
        return Verdict::NotApplicable;
    }
    if s1_value <= 0.0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// S2 的判定：越线率必须为 0（任何一对超过 hm 都算破防）。
///
/// 注意这里判的是**越线率**，不是「注入女巫均值」——后者是连续量，
/// 它的绝对值取决于覆盖率，不能当验收线（本库实测 0.030 → 0.053 都属正常范围）。
#[must_use]
pub fn judge_s2(cross_rate: f64) -> Verdict {
    if !cross_rate.is_finite() {
        return Verdict::NotApplicable;
    }
    if cross_rate <= 0.0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// S3 的判定：**看相对地板**，不看绝对比值。
///
/// * `relative <= 1.0` → [`Verdict::Pass`]（群体项没有超过 β 允许的范围）；
/// * `relative > 1.0` → [`Verdict::Fail`]（超过了，需要归因）；
///
/// 绝对比值是多少**不影响判定**——因为 ≤1 在 β<1 时不可能。
///
/// 为什么阈值取 1.0：1.0 意为「群体项把收益方顶到了 `1/β` 这个理论极限」，
/// 而 B0 实测背书场景是 1.31（最坏观察者 2.44），说明确实存在额外放大通道
/// （`审计/B0-baseline.md` §2.3）。这是**已知的失败项**，不是判据设计问题。
#[must_use]
pub fn judge_s3(s3_value: f64, beta: f32) -> (Verdict, f64) {
    match s3_relative_to_floor(s3_value, beta) {
        Err(_) => (Verdict::NotApplicable, f64::NAN),
        Ok(rel) => {
            if rel <= 1.0 {
                (Verdict::Pass, rel)
            } else {
                (Verdict::Fail, rel)
            }
        }
    }
}

/// S4 的判定：三条都要满足（采用 `审计/A3-view.md` §10 的**最宽松**成文口径）。
///
/// * 空账户出现率 ≤ 1%
/// * 空账户合计份额 ≤ 0.1%
/// * 空账户最大值 ≤ 真实均值（即比值 ≤ 1.0）
#[must_use]
pub fn judge_s4(present_rate: f64, total_share: f64, max_ratio: f64) -> Verdict {
    if !present_rate.is_finite() || !total_share.is_finite() || !max_ratio.is_finite() {
        return Verdict::NotApplicable;
    }
    if present_rate <= 0.01 && total_share <= 0.001 && max_ratio <= 1.0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// S5 的判定：自吹倍数 ≤ 1.0（伪造不该有收益）。
///
/// 「自吹 + 塞壳」通常 ≤ 1（防线有效），「纯自吹」实测 1.83（**未改善**，
/// 见 `审计/B0-baseline.md` §2.5）。
#[must_use]
pub fn judge_s5(self_inflation: f64) -> Verdict {
    if !self_inflation.is_finite() {
        return Verdict::NotApplicable;
    }
    if self_inflation <= 1.0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// S6 的判定：同输入重跑的矩阵最大绝对差必须为 **0（逐位）**。
///
/// 工程上允许 `1e-6` 的相对容差，但本库的实测一直是精确 0，
/// 所以这里坚持 0——放宽到 1e-6 会让真正的顺序回归被浮点噪声掩盖
/// （见 `crates/engine/tests/order_guards.rs` 的说明）。
#[must_use]
pub fn judge_s6(max_abs_diff: f64) -> Verdict {
    if !max_abs_diff.is_finite() {
        return Verdict::NotApplicable;
    }
    if max_abs_diff == 0.0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn s3_floor_is_not_ten_over_three() {
        // 这是本模块存在的核心理由：0.3f32 不是 0.3，1/β 不是 10/3。
        let floor = s3_floor();
        assert!(
            approx(floor, 3.333_333_2, 1e-6),
            "地板应为 3.3333332，实际 {floor}"
        );
        assert!(
            !approx(floor, 10.0 / 3.0, 1e-9),
            "地板*不是* 10/3（差在 f32 的 β 精度上），实际 {floor}"
        );
    }

    #[test]
    fn s3_floor_for_beta_one_is_one() {
        assert!(approx(s3_floor_for(1.0), 1.0, 1e-12));
        // 注意：β 是 f32，0.9f32 = 0.90000003576…，所以 1/β 与 f64 的 1/0.9 差在 1e-8 量级。
        // 这不是 bug——用 f32 的 β 算出来的地板才是与引擎一致的。
        let beta_f32 = 0.9f32;
        assert!(approx(
            s3_floor_for(beta_f32),
            1.0 / f64::from(beta_f32),
            1e-12
        ));
        assert!(approx(s3_floor_for(beta_f32), 1.0 / 0.9f64, 1e-7));
    }

    #[test]
    fn s3_floor_for_invalid_beta_is_infinite() {
        assert!(s3_floor_for(0.0).is_infinite());
        assert!(s3_floor_for(-1.0).is_infinite());
        assert!(s3_floor_for(f32::NAN).is_infinite());
        assert!(s3_floor_for(f32::INFINITY).is_infinite());
    }

    #[test]
    fn s3_relative_matches_manual_division() {
        let rel = s3_relative_to_floor(1.8207, 0.3).unwrap();
        assert!(approx(rel, 1.8207 / 3.333_333_2, 1e-6), "实际 {rel}");
    }

    #[test]
    fn s3_relative_rejects_non_finite() {
        assert!(s3_relative_to_floor(f64::NAN, 0.3).is_err());
        assert!(s3_relative_to_floor(f64::INFINITY, 0.3).is_err());
        assert!(s3_relative_to_floor(1.0, 0.0).is_err());
    }

    #[test]
    fn judge_s3_uses_relative_not_absolute() {
        // 绝对比值 1.82 > 1，但因为地板是 3.33，相对值 0.546 ⇒ 通过。
        let (v, rel) = judge_s3(1.8207, 0.3);
        assert_eq!(
            v,
            Verdict::Pass,
            "相对 {rel} 应判通过（绝对比值 1.82 不影响判定）"
        );
        assert!(rel < 1.0);
    }

    #[test]
    fn judge_s3_fails_when_above_floor() {
        // 背书场景实测 4.374，相对地板 1.312 ⇒ 超过，判失败。
        let (v, rel) = judge_s3(4.3742, 0.3);
        assert_eq!(v, Verdict::Fail);
        assert!(approx(rel, 1.312_2, 1e-3), "实际 {rel}");
    }

    #[test]
    fn judge_s3_not_applicable_when_beta_invalid() {
        let (v, _) = judge_s3(1.0, 0.0);
        assert_eq!(v, Verdict::NotApplicable);
    }

    #[test]
    fn judge_s1_is_exact_zero() {
        assert_eq!(judge_s1(0.0), Verdict::Pass);
        assert_eq!(judge_s1(1e-12), Verdict::Fail);
        assert_eq!(judge_s1(f64::NAN), Verdict::NotApplicable);
    }

    #[test]
    fn judge_s2_is_exact_zero() {
        assert_eq!(judge_s2(0.0), Verdict::Pass);
        assert_eq!(judge_s2(1.0 / 7690.0), Verdict::Fail);
    }

    #[test]
    fn judge_s4_requires_all_three() {
        assert_eq!(judge_s4(0.005, 0.0005, 0.9), Verdict::Pass);
        // 任一超标即失败
        assert_eq!(judge_s4(0.02, 0.0005, 0.9), Verdict::Fail);
        assert_eq!(judge_s4(0.005, 0.002, 0.9), Verdict::Fail);
        assert_eq!(judge_s4(0.005, 0.0005, 1.33), Verdict::Fail);
    }

    #[test]
    fn judge_s4_boundaries_are_inclusive() {
        // 恰好等于阈值应当通过（口径是「≤」）
        assert_eq!(judge_s4(0.01, 0.001, 1.0), Verdict::Pass);
    }

    #[test]
    fn judge_s5_boundary() {
        assert_eq!(judge_s5(1.0), Verdict::Pass);
        assert_eq!(judge_s5(1.829), Verdict::Fail);
        assert_eq!(judge_s5(0.994), Verdict::Pass);
    }

    #[test]
    fn judge_s6_is_bit_exact() {
        assert_eq!(judge_s6(0.0), Verdict::Pass);
        // 1e-9 也算失败：本库实测一直是精确 0
        assert_eq!(judge_s6(1e-9), Verdict::Fail);
    }

    #[test]
    fn verdict_acceptability() {
        assert!(Verdict::Pass.acceptable());
        assert!(Verdict::StructurallyImpossible.acceptable());
        assert!(Verdict::NotApplicable.acceptable());
        assert!(!Verdict::Fail.acceptable());
    }

    #[test]
    fn verdict_labels_are_chinese() {
        assert_eq!(Verdict::Pass.label(), "通过");
        assert_eq!(Verdict::Fail.label(), "失败");
        assert_eq!(
            Verdict::StructurallyImpossible.label(),
            "结构上不可达（看相对地板）"
        );
        assert_eq!(Verdict::NotApplicable.label(), "不适用");
    }

    #[test]
    fn beta_default_matches_engine_config_default() {
        // 本模块的 BETA_DEFAULT 必须与 EngineConfig::default().beta 逐位一致，
        // 否则算出来的地板会和引擎的真实行为对不上。
        assert_eq!(
            BETA_DEFAULT.to_bits(),
            crate::EngineConfig::default().beta.to_bits()
        );
    }
}
