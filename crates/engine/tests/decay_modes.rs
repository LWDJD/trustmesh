//! 三种衰减模式的测试：标准 / 有下限 / 关闭。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_view::{CompressedView, ViewParams};

fn key(i: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i;
    k[1] = 0x5A;
    k
}

fn eng(decay_enabled: bool, decay_floor: f32) -> Engine {
    Engine::new(EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams { buckets: 512, hashers: 5, seed: 7 },
        agg_mode: AggMode::Mean,
        noise_multiplier: 1.0,
        decay_enabled,
        decay_floor,
    })
}

/// ① 关闭衰减：任何时间差因子都是 1
#[test]
fn disabled_decay_is_always_one() {
    let e = eng(false, 0.0);
    for dt in [0u64, 1, 72, 720, 72_000, u64::MAX] {
        assert_eq!(e.decay_factor(dt), 1.0, "关闭衰减时 dt={dt} 应为 1");
    }
}

/// ② 标准衰减：严格等于 0.5^(dt/72)
#[test]
fn standard_decay_matches_formula() {
    let e = eng(true, 0.0);
    assert!((e.decay_factor(0) - 1.0).abs() < 1e-6);
    assert!((e.decay_factor(72) - 0.5).abs() < 1e-6);
    assert!((e.decay_factor(144) - 0.25).abs() < 1e-6);
    assert!((e.decay_factor(720) - 0.5f32.powi(10)).abs() < 1e-6);
}

/// ③ 有下限：因子永不低于 floor
#[test]
fn floored_decay_never_below_floor() {
    let e = eng(true, 0.5);
    assert!((e.decay_factor(0) - 1.0).abs() < 1e-6, "刚收到时不衰减");
    assert!((e.decay_factor(72) - 0.5).abs() < 1e-6, "一个半衰期正好到下限");
    for dt in [72u64, 100, 720, 72_000, u64::MAX] {
        assert!(
            (e.decay_factor(dt) - 0.5).abs() < 1e-6,
            "dt={dt} 应被下限夹在 0.5，实际 {}",
            e.decay_factor(dt)
        );
    }
}

/// ④ 下限对群体项的实际影响：陈旧来源不再被挤掉
#[test]
fn floor_keeps_stale_source_relevant() {
    fn build(decay_enabled: bool, decay_floor: f32) -> f32 {
        let mut e = eng(decay_enabled, decay_floor);
        let f1 = e.intern(key(1));
        let f2 = e.intern(key(2));
        let t = e.intern(key(9));
        e.rate(f1, 1.0, 0);
        e.rate(f2, 1.0, 0);
        // f1 给 t 打满分，f2 给 t 打 0
        let v1 = CompressedView::from_pairs(
            ViewParams { buckets: 512, hashers: 5, seed: 7 },
            &[(key(9), 1.0)],
        );
        let v2 = CompressedView::from_pairs(
            ViewParams { buckets: 512, hashers: 5, seed: 7 },
            &[(key(9), 0.0)],
        );
        e.on_view(f1, v1, 0, 0);
        e.on_view(f2, v2, 0, 0);
        // f2 在 t=0 之后一直不更新；f1 在 t=1000 刷新
        let v1b = CompressedView::from_pairs(
            ViewParams { buckets: 512, hashers: 5, seed: 7 },
            &[(key(9), 1.0)],
        );
        e.on_view(f1, v1b, 1000, 1000);
        e.query(t, 1000)
    }
    let standard = build(true, 0.0);
    let floored = build(true, 0.5);
    let off = build(false, 0.0);
    // 陈旧来源 f2 的观点是 0，所以它保留的权重越大，群体项结果越低。
    // 三种模式对 f2 的抑制强度：标准 >> 有下限 > 无衰减。
    // 陈旧来源 f2 的观点是 0。抑制得越狠，它对群体项的稀释越小，结果越高。
    // 抑制强度：标准（≈0 权重） > 有下限（0.5） > 无衰减（1.0）。
    assert!(
        standard > floored,
        "标准衰减把旧来源压到接近 0，结果应最高：standard={standard} floored={floored}"
    );
    assert!(
        floored > off,
        "有下限仍保留一半抑制，结果应高于完全不衰减：floored={floored} off={off}"
    );
}

/// ⑤ 关闭衰减时，全体变旧完全不改变结果（与标准衰减的「均匀老化免疫」一致）
#[test]
fn disabled_decay_is_time_invariant() {
    let mut e = eng(false, 0.0);
    let f = e.intern(key(1));
    let t = e.intern(key(9));
    e.rate(f, 1.0, 0);
    let v = CompressedView::from_pairs(
        ViewParams { buckets: 512, hashers: 5, seed: 7 },
        &[(key(9), 0.6), (key(20), 0.4)],
    );
    e.on_view(f, v, 0, 0);
    let q0 = e.query(t, 0);
    let q1 = e.query(t, 100_000);
    assert!((q0 - q1).abs() < 1e-6, "关闭衰减后结果不随时间变：{q0} vs {q1}");
}

/// ⑥ 所有模式都保持信任分布总和为 1
#[test]
fn all_modes_keep_distribution_normalized() {
    for (en, fl) in [(true, 0.0f32), (true, 0.5), (false, 0.0)] {
        let mut e = eng(en, fl);
        let a = e.intern(key(1));
        let b = e.intern(key(2));
        e.rate(a, 3.0, 0);
        e.rate(b, 1.0, 0);
        let s: f32 = e.trust_distribution(10_000).iter().map(|(_, v)| *v).sum();
        assert!(
            (s - 1.0).abs() < 1e-4,
            "模式 (enabled={en}, floor={fl}) 分布总和应为 1，实际 {s}"
        );
    }
}
