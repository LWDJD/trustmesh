//! 不可信字节的鲁棒性：`CompressedView::decode` 与 `EngineSnapshot::decode`。
//!
//! ## 为什么不做 `cargo-fuzz`
//!
//! 真正的 fuzz 需要 `libfuzzer-sys`（C 依赖），会破坏本仓「零外部依赖」的约束。
//! 而本库**只有两个**接收不可信字节的入口，输入类型是 `&[u8]`，空间可枚举得动。
//! 所以这里用「确定性伪随机字节 + 定向畸形构造」代替真 fuzz：**能复现、能进 CI、零依赖**。
//!
//! ## 三个必守的不变量
//!
//! 1. **绝不 panic**：`decode` 面对任何 `&[u8]` 都必须返回 `None` 或 `Some`，不能崩；
//! 2. **绝不分配超过输入规模的内存**（防「一个 20 字节的输入声称有 10^9 个元素」的放大器）；
//! 3. **解码出来的东西要么完全合法，要么直接 None**（不接受「部分有效」）。

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::EngineSnapshot;

/// xorshift64 伪随机字节源（与库内同算法，但这里是测试专用副本，
/// 刻意不用 `tm_engine::store::Rng` —— 否则测试与被测代码共用实现，出问题会一起错）。
struct ByteRng(u64);

impl ByteRng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

/// 随机字节：**长度不对的必须全部 None**；长度恰好的可以返回 Some，但那时
/// 负载里的每一个 f32 都必须**有限**（这是 decode 唯一的语义校验）。
///
/// ## 为什么不是「随机字节一律 None」
///
/// 我最初就是这么写的，然后测试红了——**这不是 bug，是我对 decode 的契约理解错了**。
/// `decode` 只校验三件事：参数合法、长度恰好、**每个 f32 有限**。
/// 一个 4 字节随机模式是 NaN/Inf 的概率是 0.39%（指数位全 1），
/// 而一份 64×3=192 个单元的视图里「全部有限」的概率是 (1−0.0039)^192 ≈ 47%——
/// 所以随机字节**经常**能通过长度与有限性校验。
///
/// 这在安全上是可以接受的，因为：
/// * Count Sketch 的桶值本来就是「任意有限的 f32」，没有取值范围约束；
/// * 真正用它的是 `Engine::on_view`，那里对 `total_mass` 做了 `clamp(0.0, 1.0)`，
///   并对非有限读数整条拒绝（P2 加固，见 `docs/audit/P2-security.md` §4）；
/// * 也就是说：**视图内容是"不可信但可解"，语义边界在引擎那侧**。
#[test]
fn random_bytes_of_wrong_length_are_always_rejected() {
    let params = ViewParams::new(64, 3, 7);
    let expected_len = CompressedView::new(params).bytes();
    let mut rng = ByteRng::new(0xF12D_0001);

    // 长度不等于 params.bytes() 的一律必须 None（含 0、1、短一点、长一点、极大）。
    let bad_lens = [
        0usize,
        1,
        3,
        4,
        expected_len - 1,
        expected_len + 1,
        expected_len * 3,
    ];
    for &len in &bad_lens {
        for _ in 0..200 {
            let bytes = rng.bytes(len);
            assert!(
                CompressedView::decode(&bytes, params).is_none(),
                "len={len}（应为 {expected_len}）被接受了，长度校验有漏洞"
            );
        }
    }
}

/// 长度恰好对时：decode 要么 None，要么返回的每一单元都**有限**。
/// 这条钉住 decode 唯一的语义承诺（也是引擎依赖它的前提）。
#[test]
fn random_bytes_of_exact_length_are_finite_if_accepted() {
    let params = ViewParams::new(64, 3, 7);
    let n = CompressedView::new(params).bytes();
    let mut rng = ByteRng::new(0xF12D_0003);
    let mut accepted = 0u32;
    let mut rejected = 0u32;

    for _ in 0..300 {
        let bytes = rng.bytes(n);
        match CompressedView::decode(&bytes, params) {
            None => rejected += 1,
            Some(v) => {
                accepted += 1;
                // 关键断言：接受了的负载，回编码必须逐字节相等（说明没有偷偷丢数据）。
                assert_eq!(v.encode(), bytes, "decode 后的 encode 与原始字节不同");
                // 且每个单元有限（通过一次全量 query 的有限性间接验证：
                // 若含 NaN/Inf，bucket 读数会是 NaN）。
                for i in 0..64 {
                    let mut k = [0u8; 32];
                    k[0] = i as u8;
                    assert!(
                        v.query(&k).is_finite(),
                        "接受的负载里出现了非有限读数（bucket {i}）"
                    );
                }
            }
        }
    }
    // 两种情形都应该出现（否则覆盖不足，测试会静默失去意义）。
    assert!(
        accepted > 0,
        "300 次里没有一次被接受，覆盖不足（预期约 40% 应被接受）"
    );
    assert!(rejected > 0, "300 次里没有一次被拒绝，说明非有限校验失效了");
}

/// 对每个长度的**每一种截断前缀**都不 panic（真 fuzz 之外最有效的定向覆盖）。
#[test]
fn every_truncation_prefix_is_handled() {
    let params = ViewParams::new(16, 2, 3);
    // 先造一份**合法**的负载，再逐字节截断。
    let pairs: Vec<([u8; 32], f32)> = (0..8u8)
        .map(|i| {
            let mut k = [0u8; 32];
            k[0] = i;
            (k, 0.5 + i as f32 * 0.05)
        })
        .collect();
    let good = CompressedView::from_pairs(params, &pairs);
    let bytes = good.encode();
    assert_eq!(bytes.len(), good.bytes());

    for cut in 0..bytes.len() {
        // 截断前缀：长度必不等于 params.bytes()，所以必然 None。
        assert!(
            CompressedView::decode(&bytes[..cut], params).is_none(),
            "cut={cut} 的截断前缀被接受了"
        );
    }
    // 完整的那份必须能 decode 回来（逐字节比对，因为 CompressedView 没实现 PartialEq）。
    let back = CompressedView::decode(&bytes, params).expect("完整负载必须能解码");
    assert_eq!(back.encode(), good.encode());
}

/// 非有限值必须被拒：一条 NaN 会让后续所有查询变 NaN。
#[test]
fn non_finite_payload_is_rejected() {
    let params = ViewParams::new(8, 2, 5);
    let n_bytes = CompressedView::new(params).bytes();
    let mut bytes = vec![0u8; n_bytes];
    for pat in [
        f32::NAN.to_bits(),
        f32::INFINITY.to_bits(),
        f32::NEG_INFINITY.to_bits(),
    ] {
        // 把整个负载写得极干净，只在**一个位置**插入一个非有限值。
        for b in bytes.iter_mut() {
            *b = 0;
        }
        bytes[4..8].copy_from_slice(&pat.to_le_bytes());
        assert!(
            CompressedView::decode(&bytes, params).is_none(),
            "非有限值 {pat:#x} 被接受了"
        );
    }
}

/// 每个字节位置上的每一种极端 f32 都不能让 decode panic。
#[test]
fn extreme_f32_values_anywhere_never_panic() {
    let params = ViewParams::new(4, 2, 11);
    let n = CompressedView::new(params).bytes();
    let extremes = [
        f32::NAN.to_bits(),
        f32::INFINITY.to_bits(),
        f32::NEG_INFINITY.to_bits(),
        f32::MAX.to_bits(),
        f32::MIN.to_bits(),
        0x7F80_0001, // signaling NaN
        0x0000_0001, // subnormal
        0xFF80_0000, // -0.0
    ];
    for cell in 0..(n / 4) {
        for &pat in &extremes {
            let mut bytes = vec![0u8; n];
            let off = cell * 4;
            bytes[off..off + 4].copy_from_slice(&pat.to_le_bytes());
            // 只要不 panic就算通过；返回值是 None 或 Some 都合法（极端但有限的值可以合法存在）。
            let _ = CompressedView::decode(&bytes, params);
        }
    }
}

/// 快照解码：随机字节一律 None，任何截断都不 panic。
#[test]
fn random_snapshot_bytes_never_panic() {
    let mut rng = ByteRng::new(0xF12D_0002);
    for len in [0usize, 1, 12, 32, 56, 100, 4096, 65536] {
        for _ in 0..50 {
            let bytes = rng.bytes(len);
            assert!(
                EngineSnapshot::decode(&bytes).is_none(),
                "len={len} 的随机字节被 EngineSnapshot::decode 接受了"
            );
        }
    }
}

/// 一个合法快照的每一个前缀截断都必须 None（不 panic、不部分恢复）。
#[test]
fn snapshot_truncation_is_always_none() {
    use tm_engine::{Engine, EngineConfig};

    let cfg = EngineConfig::default();
    let mut e = Engine::with_seed(cfg, 0x5EED);
    for i in 0..8u32 {
        let mut k = [0u8; 32];
        k[0] = i as u8;
        e.intern(k);
        e.rate(i, 0.5 + i as f32 * 0.05, 0);
    }
    let snap = e.snapshot(12);
    let bytes = snap.encode();
    assert!(!bytes.is_empty());

    // 只抽样截断点（完整扫一遍在 34 MB 快照上太慢；这里只要覆盖形态）。
    let step = (bytes.len() / 64).max(1);
    for cut in (0..bytes.len()).step_by(step) {
        assert!(
            EngineSnapshot::decode(&bytes[..cut]).is_none(),
            "cut={cut}/{} 的截断快照被接受了",
            bytes.len()
        );
    }
    // 完整的那份必须能 decode。
    assert!(EngineSnapshot::decode(&bytes).is_some());
}
