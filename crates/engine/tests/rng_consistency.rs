//! 两套 xorshift64 的交叉一致性：`tm_engine::view::XorShift64` 与 `tm_engine::store::Rng`。
//!
//! ## 为什么要有这个测试
//!
//! 本仓有**两套** xorshift64 实现，算法逐字相同（`x ^= x << 13; x ^= x >> 7; x ^= x << 17`）：
//!
//! | 实现 | 位置 | 用途 | 独有能力 |
//! |---|---|---|---|
//! | `tm_engine::view::XorShift64` | `crates/engine/src/view/rng.rs` | `noise_floor` 造全新 32 字节 key | `next_key()` |
//! | `tm_engine::store::Rng` | `crates/engine/src/store/mod.rs` | 抽样顺序、退避加权 | `from_state()` / `next_below()` |
//!
//! 两套**不能合并**，因为**能力集不同**（2026-11-21 合并后它们同属一个 crate 的两个模块）：
//! * `view::XorShift64` 有 [`next_key()`]（造全新 32 字节 key，噪声底采样用）；
//! * `store::Rng` 有 `from_state()/state()`（快照要**精确恢复**下一个随机数）与 `next_below()`（拒绝采样）。
//!
//! 注意：两套的**输出序列逐位相同**，本文件第 1 条断言就是钉死这一点。
//!
//! 但**它们必须保持在算法上一致**——否则：
//!
//! * 同一个种子在 view 侧和 store 侧产生**不同的随机流**；
//! * 而这会让「跨平台逐位一致」的验证出现两个互相独立的口径，
//!   一旦将来移植出错，排查成本极高（没人会想到「随机数发生器有两个版本」）。
//!
//! 所以这里钉死三条：
//!
//! 1. **同种子的前 N 个输出逐位相同**（含 seed = 0 这个「不动点」边界）；
//! 2. **同种子下的内部状态序列逐位相同**（不只是输出，状态也要一致）；
//! 3. **两边都是 2^64 周期的纯整数运算**——即"第 i 个输出"只由种子与 i 决定。
//!
//! 这是 `tm_engine` 的集成测试，因为只有它同时依赖 `tm_view` 与 `tm_store`。
//!
//! ## 如果你要改其中一套
//!
//! 改完这个测试会红。**两个选择，都允许，但都必须显式做**：
//!
//! * 想让两边继续一致 → 改另一套；
//! * 想它们不一致 → 就删掉本测试，并在两处文档里写明「两套算法已分叉」。
//!
//! 不允许的是"只改一边、测试默默变红又被人忽略"。

use tm_engine::store::Rng;
use tm_engine::view::XorShift64;

/// 抽样用的种子集合：0（不动点边界）、1、小值、大值、以及一个"看起来随机"的常量。
const SEEDS: [u64; 6] = [0, 1, 2, 7, 0xC1BA_D151_0000_0001, u64::MAX];

/// 比较两边前 `n` 个输出与状态。
///
/// 返回 `Ok(())` 表示逐位相同；否则返回第一处差异的描述。
fn compare_streams(seed: u64, n: usize) -> Result<(), String> {
    let mut a = XorShift64::new(seed);
    let mut b = Rng::new(seed);
    for i in 0..n {
        let out_a = a.next_u64();
        let out_b = b.next_u64();
        if out_a != out_b {
            return Err(format!(
                "seed={seed:#x} 第 {i} 个输出不同：view={out_a:#x} store={out_b:#x}"
            ));
        }
        // 状态也要一致：只比输出会漏掉「输出偶然相同但状态已分叉」的情形。
        if a.state() != b.state() {
            return Err(format!(
                "seed={seed:#x} 第 {i} 步后状态不同：view={:#x} store={:#x}",
                a.state(),
                b.state()
            ));
        }
    }
    Ok(())
}

/// 核心断言：六个种子 × 前 200 个输出，逐位相同。
///
/// 200 足够暴露任何位运算写错（xorshift 的 13/7/17 三个移位量、
/// 以及更新顺序都必须完全一致，写错任何一个第一个输出就不同）。
#[test]
fn both_xorshift64_implementations_produce_identical_streams() {
    for seed in SEEDS {
        compare_streams(seed, 200).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// 边界：`seed = 0` 是 xorshift 的**不动点**，两边必须用**同一个**兜底常量。
///
/// 这一条单独钉，因为它是唯一会"看起来正常但周期退化"的种子：
/// 如果哪一边忘了兜底，种子 0 会永远输出 0，而两边都忘了反而会"一致"。
/// 所以这里显式要求：种子 0 的输出**非零**，且两边相同。
#[test]
fn zero_seed_uses_the_same_nonzero_fallback() {
    let mut a = XorShift64::new(0);
    let mut b = Rng::new(0);
    let first_a = a.next_u64();
    let first_b = b.next_u64();
    assert_ne!(first_a, 0, "view 侧种子 0 退化为不动点");
    assert_ne!(first_b, 0, "store 侧种子 0 退化为不动点");
    assert_eq!(first_a, first_b, "两边的种子 0 兜底常量不同");
    assert_eq!(a.state(), b.state());
}

/// 交叉验证：`XorShift64` 的 `next_key()` 就是 4 个 `next_u64()` 的小端拼接。
///
/// 这条防止"只改 `next_key` 忘了改 `next_u64`"或反之。
#[test]
fn view_next_key_is_four_next_u64_le_chunks() {
    let mut k1 = XorShift64::new(0x1234_5678_9ABC_DEF0);
    let mut k2 = XorShift64::new(0x1234_5678_9ABC_DEF0);
    let key = k1.next_key();
    let mut expected = [0u8; 32];
    for chunk in expected.chunks_exact_mut(8) {
        chunk.copy_from_slice(&k2.next_u64().to_le_bytes());
    }
    assert_eq!(key, expected, "next_key 与 4 个 next_u64 的拼接不一致");
}

/// 字面量钉死：种子 7 的前 4 个输出**必须永不改变**。
///
/// 为什么用字面量而不是自证：这条测试守的是**兼容性**，不是**一致性**。
/// 随机流参与抽样顺序，所以它一变，历史快照重放就会不一致。
/// 如果只写"两边相等"，那么"两边一起改错"也能通过——字面量才拦得住这种情况。
///
/// 这 4 个常量由 `x ^= x << 13; x ^= x >> 7; x ^= x << 17` 在 seed=7 上
/// 逐步算出（纯整数运算，任何平台上数值相同）。
#[test]
fn stream_is_pinned_by_literal_values() {
    const SEED7_FIRST_FOUR: [u64; 4] = [
        0x0000_0001_c38e_e1c7,
        0x7001_c712_2401_6dc4,
        0xc158_1fc0_1194_9e1f,
        0x78b7_936e_c695_d0e3,
    ];

    let mut view = XorShift64::new(7);
    let got_view: [u64; 4] = [
        view.next_u64(),
        view.next_u64(),
        view.next_u64(),
        view.next_u64(),
    ];
    assert_eq!(
        got_view, SEED7_FIRST_FOUR,
        "view 侧的种子 7 随机流变了 —— 历史快照将无法重放"
    );

    let mut store = Rng::new(7);
    let got_store: [u64; 4] = [
        store.next_u64(),
        store.next_u64(),
        store.next_u64(),
        store.next_u64(),
    ];
    assert_eq!(
        got_store, SEED7_FIRST_FOUR,
        "store 侧的种子 7 随机流变了 —— 历史快照将无法重放"
    );
}
