//! 结构性质测试：定长、单点精确、合并线性、编码往返、参数边界。

use tm_view::{AccountKey, CompressedView, ViewParams, XorShift64};

/// 用固定种子生成 n 个互不相同的 32 字节 key。
fn keys(n: usize, seed: u64) -> Vec<AccountKey> {
    let mut rng = XorShift64::new(seed);
    (0..n).map(|_| rng.next_key()).collect()
}

/// 生成 n 条 (key, score)，分数均匀落在 [0.1, 1.0]。
///
/// 避开 0：0 表示没表态，写进去等于没写，不适合做误差统计。
fn random_pairs(n: usize, key_seed: u64, score_seed: u64) -> Vec<(AccountKey, f32)> {
    let keys = keys(n, key_seed);
    let mut rng = XorShift64::new(score_seed);
    keys.into_iter()
        .map(|key| {
            let unit = (rng.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            (key, 0.1 + 0.9 * unit)
        })
        .collect()
}

#[test]
fn default_params_and_fixed_size() {
    let params = ViewParams::default();
    assert_eq!(params.buckets, 2048);
    assert_eq!(params.hashers, 5);
    assert_eq!(params.seed, 0);
    assert_eq!(params, ViewParams::new(2048, 5, 0));
    assert!(params.is_valid());

    let empty = CompressedView::new(params);
    assert_eq!(empty.bytes(), 2048 * 5 * 4);
    assert_eq!(empty.bytes(), 40 * 1024);
    assert_eq!(empty.bytes(), std::mem::size_of::<f32>() * 2048 * 5);
    assert_eq!(empty.encode().len(), empty.bytes());
    assert_eq!(empty.params(), params);
    assert_eq!(empty.buckets(), 2048);
    assert_eq!(empty.hashers(), 5);
}

#[test]
fn empty_view_queries_zero() {
    let view = CompressedView::new(ViewParams::default());
    for key in keys(500, 7) {
        assert_eq!(view.query(&key), 0.0);
    }
    let stats = view.noise_floor(500, 11);
    assert_eq!(stats.median, 0.0);
    assert_eq!(stats.p95, 0.0);
    assert_eq!(stats.max, 0.0);
    assert!(view.encode().iter().all(|&byte| byte == 0));
}

#[test]
fn single_write_is_recovered() {
    let mut view = CompressedView::new(ViewParams::default());
    let key = [42u8; 32];
    view.add(&key, 0.7);
    let query = view.query(&key);
    assert!((query - 0.7).abs() < 0.02, "单条写入误差过大: {query}");
    // 没有碰撞时中位数就是精确值。
    assert!((query - 0.7).abs() < 1e-6);
}

#[test]
fn single_write_does_not_leak_to_unrelated_keys() {
    let mut view = CompressedView::new(ViewParams::default());
    view.add(&[42u8; 32], 0.7);
    // 5 个哈希里最多容忍 2 行碰撞，中位数仍是 0。
    for key in keys(200, 8) {
        assert_eq!(view.query(&key), 0.0);
    }
}

#[test]
fn hundred_writes_have_bounded_error() {
    let params = ViewParams::default();
    let entries = random_pairs(100, 11, 22);
    let view = CompressedView::from_pairs(params, &entries);

    let mut error_sum = 0.0f32;
    let mut max_error = 0.0f32;
    let mut exact = 0usize;
    for (key, score) in &entries {
        let error = (view.query(key) - *score).abs();
        error_sum += error;
        max_error = max_error.max(error);
        if error < 1e-6 {
            exact += 1;
        }
    }
    let mean = error_sum / entries.len() as f32;
    println!("100 条写入：平均误差 {mean:.6}，最大误差 {max_error:.6}，精确 {exact}/100");

    assert!(mean < 0.01, "100 条写入的平均误差过大: {mean}");
    assert!(max_error < 0.6, "100 条写入的最大误差过大: {max_error}");
    assert!(exact >= 90, "精确查询太少: {exact}/100");
}

#[test]
fn dense_small_sketch_shows_collision_error() {
    // 100 条塞进 64 个桶 × 5 个哈希：碰撞必然发生，误差应明显但仍有界。
    // 这补上了默认参数下「100 条几乎精确」看不到的碰撞行为。
    let params = ViewParams::new(64, 5, 0);
    let entries = random_pairs(100, 77, 88);
    let view = CompressedView::from_pairs(params, &entries);

    let mut error_sum = 0.0f32;
    let mut exact = 0usize;
    for (key, score) in &entries {
        let error = (view.query(key) - *score).abs();
        error_sum += error;
        if error < 1e-6 {
            exact += 1;
        }
    }
    let mean = error_sum / entries.len() as f32;
    println!("64x5 草图写入 100 条：平均误差 {mean:.4}，精确 {exact}/100");

    assert!(mean > 0.001, "密集草图必然出现碰撞误差: {mean}");
    assert!(mean < 0.5, "误差不应失控: {mean}");
    assert!(exact < 100, "密集草图不应全部精确: {exact}/100");
}

#[test]
fn bytes_are_independent_of_content() {
    let params = ViewParams::default();
    let empty = CompressedView::new(params);
    let one = CompressedView::from_pairs(params, &random_pairs(1, 1, 2));
    let many = CompressedView::from_pairs(params, &random_pairs(5000, 3, 4));

    assert_eq!(empty.bytes(), one.bytes());
    assert_eq!(one.bytes(), many.bytes());
    assert_eq!(many.bytes(), params.buckets * params.hashers * 4);
    assert_eq!(many.encode().len(), many.bytes());
    assert_eq!(many.encode().len(), empty.encode().len());
}

#[test]
fn merge_single_key_is_exactly_linear() {
    let params = ViewParams::default();
    let key = [77u8; 32];
    let mut mine = CompressedView::new(params);
    mine.add(&key, 0.8);
    let mut theirs = CompressedView::new(params);
    theirs.add(&key, 0.3);

    mine.merge(&theirs, 0.25, 0.75);
    let expected = 0.25f32 * 0.8f32 + 0.75f32 * 0.3f32;
    assert!((mine.query(&key) - expected).abs() < 1e-6);
}

#[test]
fn merge_with_zero_weight_scales_queries() {
    let params = ViewParams::new(512, 5, 9);
    let base = CompressedView::from_pairs(params, &random_pairs(300, 5, 6));
    let zero = CompressedView::new(params);

    let mut doubled = base.clone();
    doubled.merge(&zero, 2.0, 0.0);
    let mut dropped = base.clone();
    dropped.merge(&zero, 0.0, 0.0);

    for key in keys(300, 7) {
        assert!((doubled.query(&key) - 2.0 * base.query(&key)).abs() < 1e-5);
        assert_eq!(dropped.query(&key), 0.0);
    }
}

#[test]
#[should_panic(expected = "ViewParams")]
fn merge_bucket_mismatch_panics() {
    let mut a = CompressedView::new(ViewParams::new(64, 5, 0));
    let b = CompressedView::new(ViewParams::new(128, 5, 0));
    a.merge(&b, 0.5, 0.5);
}

#[test]
#[should_panic(expected = "ViewParams")]
fn merge_seed_mismatch_panics() {
    let mut a = CompressedView::new(ViewParams::new(64, 5, 1));
    let b = CompressedView::new(ViewParams::new(64, 5, 2));
    a.merge(&b, 0.5, 0.5);
}

#[test]
#[should_panic(expected = "有限值")]
fn merge_nan_weight_panics() {
    let mut a = CompressedView::new(ViewParams::new(64, 5, 0));
    let b = CompressedView::new(ViewParams::new(64, 5, 0));
    a.merge(&b, f32::NAN, 0.5);
}

#[test]
fn encode_decode_roundtrip_preserves_queries() {
    let params = ViewParams::default();
    let view = CompressedView::from_pairs(params, &random_pairs(500, 13, 14));
    let bytes = view.encode();
    assert_eq!(bytes.len(), view.bytes());

    let decoded = CompressedView::decode(&bytes, params).expect("长度正确的负载应能解码");
    assert_eq!(decoded.params(), params);
    assert_eq!(decoded.encode(), bytes);
    for key in keys(300, 15) {
        assert_eq!(decoded.query(&key), view.query(&key));
    }
}

#[test]
fn encode_decode_empty_view() {
    let params = ViewParams::new(16, 3, 7);
    let view = CompressedView::new(params);
    let bytes = view.encode();
    let decoded = CompressedView::decode(&bytes, params).expect("空视图也应可解码");
    assert_eq!(decoded.bytes(), view.bytes());
    assert_eq!(decoded.encode(), bytes);
}

#[test]
fn decode_rejects_malformed_input() {
    let params = ViewParams::default();
    let view = CompressedView::from_pairs(params, &random_pairs(20, 16, 17));
    let bytes = view.encode();

    assert!(CompressedView::decode(&[], params).is_none());
    assert!(CompressedView::decode(&bytes[..bytes.len() - 1], params).is_none());
    let mut too_long = bytes.clone();
    too_long.push(0);
    assert!(CompressedView::decode(&too_long, params).is_none());

    // 参数不同：长度对不上，必须拒绝。
    assert!(CompressedView::decode(&bytes, ViewParams::new(1024, 5, 0)).is_none());
    // 参数本身非法。
    assert!(CompressedView::decode(&bytes, ViewParams::new(0, 5, 0)).is_none());
    assert!(CompressedView::decode(&bytes, ViewParams::new(2048, 0, 0)).is_none());

    // 负载里出现 NaN / 无穷：整份拒绝，避免污染所有查询。
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut corrupted = bytes.clone();
        corrupted[..4].copy_from_slice(&bad.to_le_bytes());
        assert!(
            CompressedView::decode(&corrupted, params).is_none(),
            "应拒绝包含 {bad} 的负载"
        );
    }
}

#[test]
fn duplicate_keys_accumulate() {
    let key = [3u8; 32];
    let mut view = CompressedView::new(ViewParams::default());
    view.add(&key, 0.3);
    view.add(&key, 0.4);
    assert!((view.query(&key) - 0.7).abs() < 1e-6);

    let params = ViewParams::new(64, 5, 0);
    let pairs = [(key, 0.2), (key, 0.5)];
    let view = CompressedView::from_pairs(params, &pairs);
    assert!((view.query(&key) - 0.7).abs() < 1e-6);
}

#[test]
fn zero_and_non_finite_scores_are_ignored() {
    let params = ViewParams::new(64, 5, 2);
    let blank = CompressedView::new(params);
    let key = [8u8; 32];
    let mut view = CompressedView::new(params);
    for score in [0.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        view.add(&key, score);
    }
    assert_eq!(view.encode(), blank.encode());
    assert_eq!(view.query(&key), 0.0);
}

#[test]
fn tiny_param_shapes_are_supported() {
    // 单桶单哈希：没有中位数可投，碰撞会直接串味，用来说明结构下限。
    let params = ViewParams::new(1, 1, 0);
    let mut view = CompressedView::new(params);
    let a = [1u8; 32];
    let b = [2u8; 32];
    view.add(&a, 0.6);
    assert!((view.query(&a) - 0.6).abs() < 1e-6);

    view.add(&b, 0.2);
    let qa = view.query(&a);
    let qb = view.query(&b);
    assert!(
        (qa.abs() - 0.4).abs() < 1e-6 || (qa.abs() - 0.8).abs() < 1e-6,
        "单桶碰撞结果异常: {qa}"
    );
    // 两条记录的绝对值相同：同一个桶里就是 0.6 ± 0.2。
    assert!((qa.abs() - qb.abs()).abs() < 1e-6);

    // 偶数个 hasher：中位数取中间两个的平均。
    let mut even = CompressedView::new(ViewParams::new(64, 4, 0));
    even.add(&a, 0.4);
    assert!((even.query(&a) - 0.4).abs() < 1e-6);
    assert_eq!(even.encode().len(), 64 * 4 * 4);
}

#[test]
#[should_panic(expected = "ViewParams")]
fn zero_buckets_panics() {
    let _ = CompressedView::new(ViewParams::new(0, 5, 0));
}

#[test]
#[should_panic(expected = "ViewParams")]
fn zero_hashers_panics() {
    let _ = CompressedView::new(ViewParams::new(2048, 0, 0));
}

#[test]
#[should_panic(expected = "ViewParams")]
fn oversized_params_panic() {
    let _ = CompressedView::new(ViewParams::new(ViewParams::MAX_CELLS, 2, 0));
}

#[test]
#[should_panic(expected = "ViewParams")]
fn too_many_hashers_panic() {
    let _ = CompressedView::new(ViewParams::new(1, ViewParams::MAX_HASHERS + 1, 0));
}

#[test]
fn same_inputs_produce_identical_views() {
    let params = ViewParams::new(256, 5, 42);
    let entries = random_pairs(200, 21, 22);
    let a = CompressedView::from_pairs(params, &entries);
    let b = CompressedView::from_pairs(params, &entries);
    assert_eq!(a.encode(), b.encode());
    for key in keys(100, 23) {
        assert_eq!(a.query(&key), b.query(&key));
    }
}

#[test]
fn insertion_order_does_not_matter_much() {
    let params = ViewParams::default();
    let mut entries = random_pairs(300, 31, 32);
    let forward = CompressedView::from_pairs(params, &entries);
    entries.reverse();
    let backward = CompressedView::from_pairs(params, &entries);

    // f32 加法不满足结合律，桶内顺序变化会有 ulp 级差异；
    // 查询值必须几乎一致。
    for key in keys(100, 33) {
        assert!((forward.query(&key) - backward.query(&key)).abs() < 1e-5);
    }
}
