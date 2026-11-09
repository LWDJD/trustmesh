//! 评分行为：把「内容质量 × 兴趣匹配 × 噪声」落成具体数值。
//!
//! 拆成纯函数，是为了让公式可以被单独测、单独调，不和世界的状态纠缠。
//!
//! # 公式
//!
//! ```text
//! 诚实评分 = clamp(内容质量 × 兴趣匹配 × 噪声, 0, 1)
//! 内容质量 = 账户质量 × 逐次抖动        （每次发帖质量围绕账户水平波动）
//! 兴趣匹配 = 0.5 × (1 + 余弦相似度)      （落在 `[0,1]`）
//! 噪声     = LogNormal(0, sigma)        （乘性，中位数 1，不会出负数）
//! ```
//!
//! 算出来低于「表态阈值」就不打分：0 表示「没表态」，不是「评价很差」。
//! 这条规则来自协议本身——评分只有正数和 0，没有负分。

/// 诚实评分：`质量 × 匹配 × 噪声`，再截断到 `[0,1]`。
///
/// 三个因子都先夹到 `[0,1]`，非法输入（NaN）按 0 处理，保证结果永远是有限的概率值。
pub fn content_score(quality: f32, interest_match: f32, noise: f64) -> f32 {
    let q = if quality.is_finite() {
        quality.clamp(0.0, 1.0) as f64
    } else {
        0.0
    };
    let m = if interest_match.is_finite() {
        interest_match.clamp(0.0, 1.0) as f64
    } else {
        0.0
    };
    let n = if noise.is_finite() && noise > 0.0 {
        noise
    } else {
        0.0
    };
    (q * m * n).clamp(0.0, 1.0) as f32
}

/// 把原始分数变成「表态」：达到阈值才算真的打分，否则视为没表态（`None`）。
///
/// 阈值本身被夹到 `[0,1]`；分数为 0 一律不算表态。
pub fn stance(score: f32, min_stance: f32) -> Option<f32> {
    let floor = if min_stance.is_finite() {
        min_stance.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if !score.is_finite() || score <= 0.0 || score < floor {
        None
    } else {
        Some(score.clamp(0.0, 1.0))
    }
}

/// 女巫对被推广账户的评分：满分。
///
/// 协议里没有负分，所以女巫能做的上限就是「无脑打 1.0」。
pub fn sybil_score() -> f32 {
    1.0
}

/// 攻击者（潜伏者/渗透者/背书者）对被推广账户的评分：满分。
pub fn attack_score() -> f32 {
    1.0
}

/// 垃圾制造者的评分：不看内容质量，随机给一个虚高分。
///
/// 这模拟「刷量」——垃圾账户的输出和内容的真实质量无关，
/// 它只想把自己在别人观点里的存在感刷上去。
pub fn spam_score(u: f64) -> f32 {
    (0.55 + 0.40 * u.clamp(0.0, 1.0)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 评分公式是三个因子相乘() {
        let s = content_score(0.5, 0.5, 2.0);
        assert!((s - 0.5).abs() < 1e-6);
        let s = content_score(1.0, 1.0, 1.0);
        assert!((s - 1.0).abs() < 1e-6);
        let s = content_score(0.0, 1.0, 1.0);
        assert_eq!(s, 0.0);
        let s = content_score(1.0, 0.0, 1.0);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn 评分被截断在单位区间() {
        // 噪声很大也不会超过 1
        assert_eq!(content_score(1.0, 1.0, 9.0), 1.0);
        // 负质量按 0 处理
        assert_eq!(content_score(-1.0, 1.0, 1.0), 0.0);
        // NaN 不会传播
        assert!(content_score(f32::NAN, 0.5, 1.0).is_finite());
        assert!(content_score(0.5, f32::NAN, 1.0).is_finite());
        assert!(content_score(0.5, 0.5, f64::NAN).is_finite());
        assert!(content_score(0.5, 0.5, f64::INFINITY).is_finite());
    }

    #[test]
    fn 表态阈值() {
        assert_eq!(stance(0.0, 0.1), None);
        assert_eq!(stance(0.05, 0.1), None);
        assert_eq!(stance(0.1, 0.1), Some(0.1));
        assert_eq!(stance(0.5, 0.1), Some(0.5));
        assert_eq!(stance(2.0, 0.1), Some(1.0));
        assert_eq!(stance(-1.0, 0.0), None);
        assert_eq!(stance(f32::NAN, 0.0), None);
        // 非法阈值按 0 处理
        assert_eq!(stance(0.01, f32::NAN), Some(0.01));
    }

    #[test]
    fn 攻击评分固定为满分() {
        assert_eq!(sybil_score(), 1.0);
        assert_eq!(attack_score(), 1.0);
    }

    #[test]
    fn 垃圾评分虚高且有限() {
        for i in 0..100 {
            let s = spam_score(i as f64 / 99.0);
            assert!((0.55..=0.95).contains(&s), "垃圾评分应在 0.55~0.95: {s}");
        }
        // 越界输入被夹紧
        assert!((0.55..=0.95).contains(&spam_score(-5.0)));
        assert!((0.55..=0.95).contains(&spam_score(5.0)));
    }

    #[test]
    fn 质量与均分单调相关() {
        // 同一兴趣匹配、同一噪声下，质量越高分越高
        let mut prev = -1.0;
        for i in 0..=20 {
            let q = i as f32 / 20.0;
            let s = content_score(q, 0.7, 1.0);
            assert!(s >= prev);
            prev = s;
        }
    }
}
