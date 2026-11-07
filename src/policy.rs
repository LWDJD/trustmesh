//! 精度策略：把「怎么分配存储」交给开发者。
//!
//! 库只保证机制正确（压缩、传播、归一化），不规定参数。
//! 开发者按场景选策略：固定、线性、平方根、自定义。
//!
//! # 三种常见取舍
//!
//! - 固定桶数：简单，但评价多的人精度差
//! - 线性增长：精度恒定，体积随评分数线性涨
//! - 平方根：折中，体积涨得慢，精度降得也慢

/// 精度策略。决定「给定评分数，用多少个桶」。
pub trait PrecisionPolicy: Send + Sync {
    /// 给定一个账户的正分数量，返回桶数。
    fn buckets(&self, scores: usize) -> usize;

    /// 是否启用影响力归一化（预算）。
    ///
    /// true：每个账户总影响力恒定，背书有上限。
    /// false：不归一化，信号强，但背书无上限。
    fn normalize(&self) -> bool {
        true
    }

    /// 可选的差分隐私预算。None 表示不加噪。
    fn epsilon(&self) -> Option<f64> {
        None
    }

    /// 策略名称，用于日志和调试。
    fn name(&self) -> &'static str;
}

/// 固定桶数。最简单，适合规模可控的场景。
pub struct FixedBuckets(pub usize);

impl PrecisionPolicy for FixedBuckets {
    fn buckets(&self, _scores: usize) -> usize {
        self.0
    }
    fn name(&self) -> &'static str {
        "fixed"
    }
}

/// 线性增长：桶数 = base + per × scores，封顶 max。
///
/// 精度恒定，体积随评分数线性增长。
pub struct LinearBuckets {
    pub base: usize,
    pub per: usize,
    pub max: usize,
}

impl PrecisionPolicy for LinearBuckets {
    fn buckets(&self, scores: usize) -> usize {
        (self.base + self.per * scores).min(self.max)
    }
    fn name(&self) -> &'static str {
        "linear"
    }
}

/// 平方根增长：桶数 = base × √(scores/10)，封顶 max。
///
/// 体积涨得慢，精度降得也慢。适合评价数跨度大的网络。
pub struct SqrtBuckets {
    pub base: usize,
    pub max: usize,
}

impl PrecisionPolicy for SqrtBuckets {
    fn buckets(&self, scores: usize) -> usize {
        if scores == 0 {
            return self.base;
        }
        let scale = (scores as f64 / 10.0).sqrt();
        ((self.base as f64 * scale) as usize).clamp(self.base, self.max)
    }
    fn name(&self) -> &'static str {
        "sqrt"
    }
}

/// 自定义策略。
pub struct Custom<F> {
    pub f: F,
}

impl<F> PrecisionPolicy for Custom<F>
where
    F: Fn(usize) -> usize + Send + Sync,
{
    fn buckets(&self, scores: usize) -> usize {
        (self.f)(scores)
    }
    fn name(&self) -> &'static str {
        "custom"
    }
}

/// 完整配置：策略 + 归一化开关 + 混淆开关。
#[derive(Clone)]
pub struct SketchConfig {
    pub policy: std::sync::Arc<dyn PrecisionPolicy>,
    pub d: usize,
    /// 覆盖策略的 normalize 设置。None 表示用策略的默认值。
    pub normalize_override: Option<bool>,
    /// 覆盖策略的 epsilon。None 表示用策略的默认值。
    pub epsilon_override: Option<f64>,
}

impl SketchConfig {
    pub fn new(policy: std::sync::Arc<dyn PrecisionPolicy>) -> Self {
        Self {
            policy,
            d: 5,
            normalize_override: None,
            epsilon_override: None,
        }
    }

    /// 默认配置：固定 2048 桶，归一化开启。
    pub fn default_fixed() -> Self {
        Self::new(std::sync::Arc::new(FixedBuckets(2048)))
    }

    /// 自适应配置：随评分数平方根增长。
    pub fn adaptive() -> Self {
        Self::new(std::sync::Arc::new(SqrtBuckets {
            base: 1024,
            max: 8192,
        }))
    }

    pub fn buckets_for(&self, scores: usize) -> usize {
        self.policy.buckets(scores)
    }

    pub fn normalize(&self) -> bool {
        self.normalize_override.unwrap_or_else(|| self.policy.normalize())
    }

    pub fn epsilon(&self) -> Option<f64> {
        self.epsilon_override.or_else(|| self.policy.epsilon())
    }

    pub fn size_bytes(&self, scores: usize) -> usize {
        let b = self.buckets_for(scores);
        // f64 存储
        b * self.d * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_constant() {
        let p = FixedBuckets(2048);
        assert_eq!(p.buckets(10), 2048);
        assert_eq!(p.buckets(10000), 2048);
    }

    #[test]
    fn linear_grows() {
        let p = LinearBuckets {
            base: 1024,
            per: 4,
            max: 8192,
        };
        assert_eq!(p.buckets(0), 1024);
        assert_eq!(p.buckets(100), 1424);
        assert_eq!(p.buckets(5000), 8192);
    }

    #[test]
    fn sqrt_grows_slowly() {
        let p = SqrtBuckets {
            base: 1024,
            max: 8192,
        };
        assert_eq!(p.buckets(10), 1024);
        assert_eq!(p.buckets(1000), 10240.min(8192));
        assert_eq!(p.buckets(50000), 8192);
    }

    #[test]
    fn size_scales_with_buckets() {
        let cfg = SketchConfig::default_fixed();
        let s10 = cfg.size_bytes(10);
        let s1000 = cfg.size_bytes(1000);
        assert_eq!(s10, s1000, "固定策略下体积应恒定");
    }
}
