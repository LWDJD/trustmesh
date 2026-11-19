//! `SparseView` 编码器的性质测试（P1~P5）。
//!
//! 本文件只加测试、零生产代码改动。它补的是 `view.rs` 与 `tests/anti_sybil.rs` 里
//! 还没有的那一层：
//!
//! | 组 | 性质 | 本文件怎么测 |
//! |---|---|---|
//! | P1 | 规范性 / 唯一性 | 独立参考解码器；`encode(decode(b)) == b`；单字节变异注入性；稠密小域全枚举；输入顺序无关 |
//! | P2 | 拒绝完备性 | 六条拒绝条件逐条系统扫：填充区全 `(count, 位置)` 双循环、`count` 从 `cap+1` 到 `u16::MAX` 全扫描、`cap` 从 1 到 4096 逐值 |
//! | P3 | 模糊测试 | xorshift64 确定性模糊：CI 2 万随机 + 2 万变异；`#[ignore]` 全量 100 万 + 10 万 |
//! | P4 | 交叉验证 | 测试内独立慢速实现（BTreeMap 合并 → (分数降序, 账户升序) 截断 → 账户升序落表），逐条对拍 |
//! | P5 | codec 层定点往返 | cap=1 手工字节，`q` 取 1..=65535 全量 encode↔decode 逐字节不变 |
//!
//! # 为什么不重复已有测试
//!
//! * **定点往返不重造**：`fixed.rs` 的 `quantize_dequantize_往返恒等` 已穷举 65536 个
//!   定点值。P5 只测 **codec 层**（`q` 经 u16 字节载荷往返后再逐字节写回），与定点数学正交。
//! * **单点拒绝不重造**：`view.rs` 已有 `decode_拒绝长度与cap不符` /
//!   `decode_拒绝不规范槽位区` / `decode_拒绝填充区夹带` / `decode_非法容量一律拒绝`，
//!   但它们各只用一两个固定样例（填充区那条只扫了 cap=2 的 6 个偏移）。
//!   本文件把每一条扩成**系统性扫描**：全部容量、全部条数、全部字节位置。
//! * **差分参考解码器是全新的**：`view.rs` 里没有第二个实现；`tests/anti_sybil.rs` 的
//!   `t05b_伪造负载被拒绝` 只做了负样例，没有「真实 decode vs 参考 decode」的全量对拍。
//!
//! # 参考解码器的独立性
//!
//! 为了不「用被测实现的思路验证被测实现」，`ref_decode` 刻意与生产实现不同：
//!
//! 1. **检查顺序相反**：先用 u128 算出期望长度再比长度，cap 范围检查放在其后
//!    （生产实现是先查 cap 再 `checked_mul`）；
//! 2. **不用 `from_le_bytes` 便利函数**：手工 `b[i] + 256 * b[i+1]` 拼小端；
//! 3. **不共享格式常量**：本文件写死 `REF_SLOT = 6` / `REF_OVERHEAD = 2` /
//!    `REF_MAX_CAP = 4096`，由 `p1_格式常量钉死` 把它们与 crate 常量焊在一起，
//!    防止改格式时参考实现静默守旧；
//! 4. **不同写法**：升序用「与上一槽比较」，填充区用 `while` 逐字节推进
//!    （生产实现用 `prev` 变量与 `.iter().any()`）。
//!
//! # 怎么跑
//!
//! ```text
//! cargo test --test codec_props                                      # CI 口径（2 万 + 2 万）
//! cargo test --release --test codec_props -- --ignored --nocapture  # 全量模糊（100 万 + 10 万）
//! ```
//!
//! # 已知的 API 面差异（钉住现状，不判为缺陷）
//!
//! * `SparseView::new` 对非法 cap panic，`decode` 对同样的 cap 返回 `None`，
//!   `truncated_mass` 干脆不校验 cap。三个入口契约本来就不同（new 是配置错误、
//!   decode 是不可信输入、truncated_mass 是报告侧估算），但运维侧容易把
//!   `truncated_mass` 的静默数值当成合法读数 —— 见 `附_非法cap三入口行为不同_现状钉住`。
//! * `encode` 用 `self.slots.len() as u16` 无校验写出。只有走私有
//!   `from_slots_unchecked` 且条数 > 65535 才可能造出自家 `decode` 拒收的字节；
//!   公开路径条数上界是 `MAX_CAP = 4096 < 65536`，不可达。
//!   见 `附_encode条数无校验但公开路径不可达`。

use std::collections::BTreeMap;
use std::time::Instant;

use tm_mincore::{
    dequantize, quantize, Acc, SparseView, MAX_CAP, Q_ONE, SLOT_BYTES, VIEW_OVERHEAD,
};

// ═══════════════════ 0. 参考实现（独立书写） ═══════════════════

/// 参考实现的槽位字节数（刻意不共享 crate 的 `SLOT_BYTES`）。
const REF_SLOT: usize = 6;
/// 参考实现的尾部条数字节数（刻意不共享 crate 的 `VIEW_OVERHEAD`）。
const REF_OVERHEAD: usize = 2;
/// 参考实现的最大容量（刻意不共享 crate 的 `MAX_CAP`）。
const REF_MAX_CAP: usize = 4096;

/// 参考解码器：与被测的 `SparseView::decode` 独立书写，用于差分对拍。
///
/// 返回 `(cap, 槽位表)`，槽位表按账户号严格升序、每槽 `q > 0`（与规范形式一致）。
/// 与生产实现刻意不同的四处见模块文档。
fn ref_decode(payload: &[u8], cap: usize) -> Option<(usize, Vec<(Acc, u16)>)> {
    // ① 先算期望长度（u128 防溢出），再比长度；cap 范围检查放在其后。
    let expected = (cap as u128) * (REF_SLOT as u128) + (REF_OVERHEAD as u128);
    if (payload.len() as u128) != expected {
        return None;
    }
    if cap == 0 || cap > REF_MAX_CAP {
        return None;
    }
    let fixed = cap * REF_SLOT;
    // ② 手工拼小端，不用 from_le_bytes。
    let count = (payload[fixed] as usize) + 256 * (payload[fixed + 1] as usize);
    if count > cap {
        return None;
    }
    let mut slots: Vec<(Acc, u16)> = Vec::new();
    for i in 0..count {
        let off = i * REF_SLOT;
        let acc = (payload[off] as u32)
            + 256 * (payload[off + 1] as u32)
            + 65_536 * (payload[off + 2] as u32)
            + 16_777_216 * (payload[off + 3] as u32);
        let q = (payload[off + 4] as u16) + 256 * (payload[off + 5] as u16);
        if q == 0 {
            return None;
        }
        // ③ 升序用「与上一槽比较」，而不是生产实现的 prev 变量。
        if let Some(&(last, _)) = slots.last() {
            if acc <= last {
                return None;
            }
        }
        slots.push((acc, q));
    }
    // ④ 填充区用 while 逐字节推进，而不是 iter().any()。
    let mut pos = count * REF_SLOT;
    while pos < fixed {
        if payload[pos] != 0 {
            return None;
        }
        pos += 1;
    }
    Some((cap, slots))
}

/// 无 panic 解码：任何输入（含非法 cap、任意长度）都不许 panic。
///
/// `catch_unwind` 把 panic 转成测试失败而不是让整个测试进程崩掉；
/// 生产实现若真的 panic，这里会给出「哪个 cap、多长」的现场。
fn decode_no_panic(payload: &[u8], cap: usize) -> Option<SparseView> {
    let len = payload.len();
    match std::panic::catch_unwind(|| SparseView::decode(payload, cap)) {
        Ok(view) => view,
        Err(_) => panic!("decode 在 cap={cap}、长度 {len} 的负载上 panic 了"),
    }
}

/// 差分断言：真实 `decode` / 参考解码 / 规范形式三方对拍。
///
/// * 两边都 `None`：拒绝一致；
/// * 两边都 `Some`：cap、条数、槽位、total_q 全等，且 `encode(decode(b)) == b`；
/// * 一边 `Some` 一边 `None`：立即失败（这就是差分要抓的分歧）。
fn xcheck_decode(payload: &[u8], cap: usize) -> Option<SparseView> {
    let real = decode_no_panic(payload, cap);
    let reference = ref_decode(payload, cap);
    match (real, reference) {
        (None, None) => None,
        (Some(view), Some((ref_cap, ref_slots))) => {
            assert_eq!(view.cap(), ref_cap, "cap 与参考实现不一致");
            assert_eq!(view.len(), ref_slots.len(), "条数与参考实现不一致");
            assert_eq!(view.slots(), ref_slots.as_slice(), "槽位与参考实现不一致");
            let ref_total: u32 = ref_slots.iter().map(|&(_, q)| u32::from(q)).sum();
            assert_eq!(view.total_q(), ref_total, "total_q 与参考实现不一致");
            // P1 的外环：被接受的负载必须已经逐字节就是规范形式。
            assert_eq!(
                view.encode().as_slice(),
                payload,
                "encode(decode(b)) 必须逐字节等于 b"
            );
            // 显式钉住「接受 ⟹ 规范形式」的每一条，不依赖 encode 的布局正确性。
            let len = view.len();
            assert!(len <= cap, "条数不得超过容量");
            assert!(
                (1..=REF_MAX_CAP).contains(&cap),
                "被接受的负载只能是合法 cap"
            );
            for pair in view.slots().windows(2) {
                assert!(pair[0].0 < pair[1].0, "接受负载的槽位必须严格升序");
            }
            assert!(
                view.slots().iter().all(|&(_, q)| q > 0),
                "每槽 q 必须大于 0"
            );
            let count_in_payload =
                (payload[cap * REF_SLOT] as usize) + 256 * (payload[cap * REF_SLOT + 1] as usize);
            assert_eq!(count_in_payload, len, "尾部条数必须等于实际条数");
            let fill_start = len * REF_SLOT;
            assert!(
                payload[fill_start..cap * REF_SLOT].iter().all(|&b| b == 0),
                "接受负载的填充区必须全零"
            );
            Some(view)
        }
        _ => {
            let len = payload.len();
            panic!("差分不一致：cap={cap}、长度 {len}；真实与参考一个接受一个拒绝");
        }
    }
}

/// 手工装配规范字节串（独立于 `encode` 的字节布局实现）。
///
/// 只写规定的部分：前 `slots.len()` 个槽、尾部条数，其余全零。
/// 字节序用移位与掩码手工拼，而不是 `to_le_bytes`。
fn manual_encode(cap: usize, slots: &[(Acc, u16)]) -> Vec<u8> {
    assert!(
        (1..=REF_MAX_CAP).contains(&cap),
        "手工编码的 cap 必须合法，收到 {cap}"
    );
    let n = slots.len();
    assert!(n <= cap, "条数不得超过容量");
    let mut payload = vec![0u8; cap * REF_SLOT + REF_OVERHEAD];
    for (i, &(acc, q)) in slots.iter().enumerate() {
        let off = i * REF_SLOT;
        payload[off] = (acc & 0xff) as u8;
        payload[off + 1] = ((acc >> 8) & 0xff) as u8;
        payload[off + 2] = ((acc >> 16) & 0xff) as u8;
        payload[off + 3] = ((acc >> 24) & 0xff) as u8;
        payload[off + 4] = (q & 0xff) as u8;
        payload[off + 5] = ((q >> 8) & 0xff) as u8;
    }
    let fixed = cap * REF_SLOT;
    payload[fixed] = (n & 0xff) as u8;
    payload[fixed + 1] = ((n >> 8) & 0xff) as u8;
    payload
}

// ═══════════════════ 0b. 确定性伪随机与负载构造 ═══════════════════

/// xorshift64：本仓零外部依赖，测试全部用同一个确定性随机源。
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `[0, bound)` 的整数（`bound` 必须 > 0）。
    fn next_below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "next_below 要求 bound > 0");
        self.next_u64() % bound
    }

    /// `[lo, hi]` 闭区间整数。
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo <= hi, "range 的上界小于下界");
        lo + self.next_below(hi - lo + 1)
    }
}

/// 一次取一个 u64、切 8 个字节填进去：比逐字节取随机数快一个量级。
fn fill_random_bytes(rng: &mut XorShift, dst: &mut [u8]) {
    let mut word = 0u64;
    for (i, b) in dst.iter_mut().enumerate() {
        if i % 8 == 0 {
            word = rng.next_u64();
        }
        *b = (word >> ((i % 8) * 8)) as u8;
    }
}

/// 随机容量：小容量高频、大容量低频（大负载的往返/填充区扫描更贵）。
const CAP_POOL: [usize; 20] = [
    1,
    2,
    3,
    4,
    5,
    8,
    13,
    16,
    32,
    64,
    100,
    128,
    200,
    256,
    512,
    600,
    1024,
    2048,
    4095,
    REF_MAX_CAP,
];

fn random_cap(rng: &mut XorShift) -> usize {
    CAP_POOL[rng.next_below(CAP_POOL.len() as u64) as usize]
}

/// 构造一条**合法**负载：条数 ≤ cap、账户号严格升序、`q` 属于 `[1, 65535]`、填充区全零。
fn random_valid_payload(rng: &mut XorShift, cap: usize) -> Vec<u8> {
    let count = rng.range(0, cap as u64) as usize;
    let mut slots: Vec<(Acc, u16)> = Vec::with_capacity(count);
    let mut acc: u32 = rng.next_below(16) as u32;
    for _ in 0..count {
        slots.push((acc, rng.range(1, u64::from(Q_ONE)) as u16));
        // 每步至少 +1，所以严格升序；条数 ≤ 4096、步长 ≤ 5，不会溢出 u32。
        acc += rng.range(1, 5) as u32;
    }
    manual_encode(cap, &slots)
}

/// 随机变异：先改 1~3 个字节（每个都改成真正的不同值），再随机删/插/截。
fn mutate(rng: &mut XorShift, original: &[u8]) -> Vec<u8> {
    let mut payload = original.to_vec();
    let flips = rng.range(1, 3);
    for _ in 0..flips {
        if payload.is_empty() {
            break;
        }
        let pos = rng.next_below(payload.len() as u64) as usize;
        // 增量属于 [1, 255]：模 256 后必然与原值不同。
        let delta = rng.range(1, 255) as u8;
        payload[pos] = payload[pos].wrapping_add(delta);
    }
    match rng.next_below(4) {
        0 if payload.len() > 1 => {
            let cut = rng.next_below(payload.len() as u64) as usize;
            payload.remove(cut);
        }
        1 => {
            let at = rng.next_below(payload.len() as u64 + 1) as usize;
            payload.insert(at, rng.next_below(256) as u8);
        }
        2 => {
            let keep = rng.next_below(payload.len() as u64 + 1) as usize;
            payload.truncate(keep);
        }
        _ => {}
    }
    payload
}

/// 任何被 `decode` 接受的视图都必须满足的读数性质（P1 内环 + P3 断言）。
///
/// * 读数全部有限、单条 `v` 属于 `[0, 1]`；
/// * 非空时 `Σv ≡ 1`（1e-5 内）；
/// * 规范字节再解码必须回到同一个视图（幂等）；
/// * 总质量不会溢出 u32（`len ≤ 4096` 且每槽 ≤ 65535 ⟹ 上界 268_431_360）。
fn check_accepted(view: &SparseView, payload: &[u8], cap: usize) {
    assert_eq!(view.cap(), cap, "cap 必须原样保留");
    assert!(view.len() <= cap, "条数不得超过容量");
    assert_eq!(
        view.encode().as_slice(),
        payload,
        "接受 ⟹ 规范形式：encode(decode(b)) 必须逐字节等于 b"
    );
    for pair in view.slots().windows(2) {
        assert!(pair[0].0 < pair[1].0, "槽位必须严格升序");
    }
    assert!(
        view.slots().iter().all(|&(_, q)| q > 0),
        "每槽 q 必须大于 0"
    );

    let readings = view.normalized_slots();
    assert_eq!(readings.len(), view.len());
    for &(acc, value) in &readings {
        assert!(value.is_finite(), "归一化读数不能是 NaN/无穷");
        assert!((0.0..=1.0).contains(&value), "单条归一化读数越界：{value}");
        assert!(view.q_of(acc) > 0, "表内账户必须读到正分");
    }
    for acc in [0u32, 1, 42, u32::MAX] {
        assert!(
            view.normalized(acc).is_finite(),
            "账户 {acc} 的归一化读数必须是有限值"
        );
    }
    assert!(
        (view.total_q() as u64) <= (REF_MAX_CAP as u64) * u64::from(Q_ONE),
        "总质量不得溢出 u32"
    );
    assert!(view.total_mass().is_finite(), "总质量必须是有限值");

    if view.is_empty() {
        assert_eq!(view.total_q(), 0);
        assert!(readings.is_empty());
    } else {
        let sum: f64 = readings.iter().map(|&(_, value)| f64::from(value)).sum();
        assert!((sum - 1.0).abs() < 1e-5, "非空视图必须 Σv ≡ 1，实测 {sum}");
        // 幂等：规范字节再解码必须回到同一个视图。
        assert_eq!(decode_no_panic(payload, cap).as_ref(), Some(view));
    }
}

/// 一次模糊运行的统计。
struct FuzzStats {
    rand_n: u64,
    rand_ok: u64,
    mut_n: u64,
    mut_ok: u64,
}

impl FuzzStats {
    fn describe(&self) -> String {
        let FuzzStats {
            rand_n,
            rand_ok,
            mut_n,
            mut_ok,
        } = *self;
        let rand_pct = pct(rand_ok, rand_n);
        let mut_pct = pct(mut_ok, mut_n);
        format!(
            "随机 {rand_n}（接受 {rand_ok}，{rand_pct:.2}%）+ 变异 {mut_n}（接受 {mut_ok}，{mut_pct:.2}%）"
        )
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// 模糊驱动：A 段随机负载（4 种构造交替），B 段合法负载的随机变异。
///
/// 每个样本都走三方差分；被接受的样本再加读数有限性、`Σv ≡ 1`、幂等、注入性断言。
fn run_fuzz(rand_samples: u64, mut_samples: u64, seed: u64) -> FuzzStats {
    let mut rng = XorShift::new(seed);
    let mut stats = FuzzStats {
        rand_n: 0,
        rand_ok: 0,
        mut_n: 0,
        mut_ok: 0,
    };

    // A 段：随机负载。四种构造交替，保证接受面与拒绝面都被打到。
    for index in 0..rand_samples {
        let cap = random_cap(&mut rng);
        let expected_len = cap * REF_SLOT + REF_OVERHEAD;
        let (payload, must_accept) = match index % 4 {
            // (a) 长度随机：绝大多数撞「长度与 cap 不符」。
            0 => {
                let len = match rng.next_below(5) {
                    0 => expected_len - 1,
                    1 => expected_len + 1,
                    2 => rng.next_below(expected_len as u64 + 1) as usize,
                    3 => 0,
                    _ => expected_len,
                };
                let mut payload = vec![0u8; len];
                fill_random_bytes(&mut rng, &mut payload);
                (payload, false)
            }
            // (b) 长度恰好、内容全随机：靠条数/槽位/填充区拒绝。
            1 => {
                let mut payload = vec![0u8; expected_len];
                fill_random_bytes(&mut rng, &mut payload);
                (payload, false)
            }
            // (c) 合法构造：必须被接受（构造函数是规范形式本身）。
            2 => (random_valid_payload(&mut rng, cap), true),
            // (d) 合法构造后做少量变异：半合法输入，两边都可能。
            _ => {
                let original = random_valid_payload(&mut rng, cap);
                (mutate(&mut rng, &original), false)
            }
        };
        let len = payload.len();
        let view = xcheck_decode(&payload, cap);
        stats.rand_n += 1;
        match view {
            Some(view) => {
                check_accepted(&view, &payload, cap);
                stats.rand_ok += 1;
            }
            None => assert!(
                !must_accept,
                "合法构造被自家 decode 拒绝：cap={cap}、长度 {len}"
            ),
        }
    }

    // B 段：合法负载的随机变异（1~3 字节 + 随机改长度）。拒绝路径的主要来源。
    for _ in 0..mut_samples {
        let cap = random_cap(&mut rng);
        let original = random_valid_payload(&mut rng, cap);
        let original_len = original.len();
        let original_view = xcheck_decode(&original, cap);
        assert!(
            original_view.is_some(),
            "随机合法负载被自家 decode 拒绝：cap={cap}、长度 {original_len}"
        );
        let original_view = original_view.expect("刚断言过是 Some");
        let variant = mutate(&mut rng, &original);
        assert_ne!(
            variant, original,
            "变异必须真的改动负载（否则注入性断言失去意义）"
        );
        stats.mut_n += 1;
        if let Some(view) = xcheck_decode(&variant, cap) {
            assert_ne!(
                view, original_view,
                "改过字节的规范负载必须给出不同视图（cap={cap}）"
            );
            check_accepted(&view, &variant, cap);
            stats.mut_ok += 1;
        }
    }

    stats
}

// ═══════════════════ P1. 规范性 / 唯一性 ═══════════════════

/// 格式常量钉死：参考实现写死的字面量必须与 crate 常量一致。
///
/// 参考解码器刻意不共享 `SLOT_BYTES` / `VIEW_OVERHEAD` / `MAX_CAP`，所以这里必须有
/// 一处把两者焊在一起，否则改格式时参考实现会静默守旧格式。
#[test]
fn p1_格式常量钉死() {
    assert_eq!(SLOT_BYTES, REF_SLOT, "参考实现的槽字节数过时了");
    assert_eq!(VIEW_OVERHEAD, REF_OVERHEAD, "参考实现的额外字节数过时了");
    assert_eq!(MAX_CAP, REF_MAX_CAP, "参考实现的最大容量过时了");
    assert_eq!(REF_SLOT, 6);
    assert_eq!(REF_OVERHEAD, 2);
}

/// 合法视图往返逐字节闭合：`encode(decode(b)) == b` 与 `decode(encode(v)) == v`。
///
/// 覆盖空表、单条、满表、账户号两端、`q` 两端、连续账户号，以及 `from_scores` 的
/// 公开构造路径（含 `MAX_CAP` 满额）。
#[test]
fn p1_合法视图往返逐字节闭合() {
    let cases: Vec<(usize, Vec<(Acc, u16)>)> = vec![
        (1, vec![]),
        (1, vec![(7, 1)]),
        (1, vec![(0, Q_ONE)]),
        (1, vec![(u32::MAX, Q_ONE)]),
        (2, vec![(0, 1), (u32::MAX, Q_ONE)]),
        (2, vec![(0, 1), (1, 2)]),
        (3, vec![(10, 3), (11, 5), (12, 7)]),
        (8, (0..8u32).map(|i| (i, i as u16 + 1)).collect()),
        (128, (0..128u32).map(|i| (i, Q_ONE)).collect()),
        (512, (0..300u32).map(|i| (i, i as u16 + 1)).collect()),
        (MAX_CAP, (0..MAX_CAP as u32).map(|i| (i, Q_ONE)).collect()),
    ];
    for (cap, slots) in cases {
        let payload = manual_encode(cap, &slots);
        let view = decode_no_panic(&payload, cap);
        assert!(view.is_some(), "手工构造的规范负载必须被接受（cap={cap}）");
        let view = view.expect("刚断言过是 Some");
        assert_eq!(view.cap(), cap);
        assert_eq!(view.slots(), slots.as_slice(), "槽位必须原样读回");
        assert_eq!(view.len(), slots.len());
        assert_eq!(
            view.total_q(),
            slots.iter().map(|&(_, q)| u32::from(q)).sum::<u32>()
        );
        assert_eq!(
            view.encode().as_slice(),
            payload.as_slice(),
            "encode(decode(b)) 必须逐字节等于 b（cap={cap}）"
        );
    }

    // 公开构造路径：from_scores -> encode -> decode -> 再编码，全闭合。
    for cap in [1usize, 2, 3, 8, 128, 512, MAX_CAP] {
        let pairs: Vec<(Acc, f32)> = (0..cap as u32).map(|i| (i, 1.0)).collect();
        let view = SparseView::from_scores(cap, &pairs);
        assert_eq!(view.len(), cap, "cap={cap} 时这里不该触发截断");
        let payload = view.encode();
        let back = decode_no_panic(&payload, cap);
        assert_eq!(back.as_ref(), Some(&view), "decode(encode(v)) 必须等于 v");
        assert_eq!(view.encode(), payload, "再一次编码必须逐字节相同");
    }
}

/// 解码端小端字节序：账户号低字节在前、分数低字节在前、条数在尾部。
///
/// 参考解码器手工拼小端，这里同时验证两边解释一致；并验证账户号升序比的是
/// **数值**而不是字节串字典序（`0x0000_0100 > 0x0000_0001`，尽管低字节 0 < 1）。
#[test]
fn p1_解码端小端字节序() {
    let samples = [
        (0x0000_0001u32, 0x0001u16),
        (0x0000_0100, 0x0100),
        (0x0102_0304, 0x0506),
        (0xFFFF_FFFE, 0xFFFE),
        (u32::MAX, Q_ONE),
    ];
    for &(acc, q) in &samples {
        let payload = manual_encode(1, &[(acc, q)]);
        let expected = vec![
            (acc & 0xff) as u8,
            ((acc >> 8) & 0xff) as u8,
            ((acc >> 16) & 0xff) as u8,
            ((acc >> 24) & 0xff) as u8,
            (q & 0xff) as u8,
            ((q >> 8) & 0xff) as u8,
            1,
            0,
        ];
        assert_eq!(payload, expected);
        let view = decode_no_panic(&payload, 1).expect("合法负载必须被接受");
        assert_eq!(view.slots(), [(acc, q)]);
        assert_eq!(view.encode(), payload);
        assert_eq!(ref_decode(&payload, 1), Some((1, vec![(acc, q)])));
    }

    let payload = manual_encode(2, &[(0x0000_0001, 1), (0x0000_0100, 2)]);
    let view = decode_no_panic(&payload, 2).expect("按数值升序的两个账户号应被接受");
    assert_eq!(view.slots(), [(0x0000_0001, 1), (0x0000_0100, 2)]);
    assert_eq!(
        ref_decode(&payload, 2),
        Some((2, vec![(0x0000_0001, 1), (0x0000_0100, 2)]))
    );
}

/// **注入性**：合法字节串的每一个字节位置改成任意不同值，
/// 只要 `decode` 成功，结果视图必与原来不相等（逐位比较 `encode`）。
///
/// 这是「不同字节 ⟹ 不同视图」的直接落点：若某个单字节改动被接受却返回同一视图，
/// 规范形式就有歧义（同一份观点会有两份字节串）。
#[test]
fn p1_单字节变异必改变视图() {
    // (cap, 槽位)：第一组满表（无填充区），第二组半满（有 36 字节填充区）。
    let configs: [(usize, Vec<(Acc, u16)>); 2] = [
        (2, vec![(1, 32768), (3, 16384)]),
        (8, vec![(1, 32768), (3, 16384)]),
    ];
    for (cap, slots) in configs {
        let base = manual_encode(cap, &slots);
        let base_view = decode_no_panic(&base, cap).expect("基线必须被接受");
        let mut accepted = 0usize;
        for (pos, old) in base.iter().copied().enumerate() {
            for cand in 0..=255u8 {
                if cand == old {
                    continue;
                }
                let mut bad = base.clone();
                bad[pos] = cand;
                if let Some(view) = xcheck_decode(&bad, cap) {
                    assert_ne!(
                        view.encode(),
                        base,
                        "不同字节必须给出不同视图（pos={pos}，{old} -> {cand}）"
                    );
                    assert_ne!(
                        view, base_view,
                        "单字节变异（pos={pos}，{old} -> {cand}）解出了同一个视图"
                    );
                    accepted += 1;
                }
            }
        }
        assert!(
            accepted > 0,
            "cap={cap} 的单字节变异没有一个被接受，注入性测试失去意义"
        );
    }

    // 大表逐字节 × 255 太贵，改成「全部位置 × 四个典型脏值」。
    let cap = 128usize;
    let slots: Vec<(Acc, u16)> = (0..64u32).map(|i| (i * 2, i as u16 % 1000 + 1)).collect();
    let base = manual_encode(cap, &slots);
    let base_view = decode_no_panic(&base, cap).expect("基线必须被接受");
    let mut accepted = 0usize;
    for (pos, old) in base.iter().copied().enumerate() {
        for cand in [0x01u8, 0x80, 0xff, old.wrapping_add(1)] {
            if cand == old {
                continue;
            }
            let mut bad = base.clone();
            bad[pos] = cand;
            if let Some(view) = xcheck_decode(&bad, cap) {
                assert_ne!(
                    view.encode(),
                    base,
                    "不同字节必须给出不同视图（cap=128，pos={pos}）"
                );
                assert_ne!(
                    view, base_view,
                    "cap=128 的单字节变异（pos={pos}）解出了同一个视图"
                );
                accepted += 1;
            }
        }
    }
    assert!(accepted > 0, "cap=128 的单字节变异没有一个被接受");
}

/// 编码是单射：不同视图必有不同字节串；同一字节串不可能解出两个不同视图。
#[test]
fn p1_编码是单射() {
    let mut known: BTreeMap<Vec<u8>, SparseView> = BTreeMap::new();
    let mut rng = XorShift::new(0x1A2B_3C4D_5E6F_7788);
    for _ in 0..400 {
        let cap = random_cap(&mut rng);
        let payload = random_valid_payload(&mut rng, cap);
        let view = decode_no_panic(&payload, cap).expect("合法负载必须被接受");
        if let Some(old) = known.get(&payload) {
            assert_eq!(old, &view, "同一字节串不能解出两个不同视图");
        } else {
            known.insert(payload, view);
        }
    }
    let distinct = known.len();
    assert!(
        distinct > 100,
        "去重后的视图样本太少（{distinct}），单射测试样本不足"
    );

    // 同槽位表、不同 cap：编码长度不同 ⟹ 字节必然不同。
    let narrow = SparseView::from_scores(1, &[(7, 0.5)]);
    let wide = SparseView::from_scores(2, &[(7, 0.5)]);
    assert_ne!(narrow, wide);
    assert_ne!(narrow.encode(), wide.encode());
    assert_ne!(narrow.encode().len(), wide.encode().len());

    // 同 cap、不同分数：语义不同 ⟹ 字节不同。
    let half = SparseView::from_scores(2, &[(7, 0.5)]);
    let quarter = SparseView::from_scores(2, &[(7, 0.25)]);
    assert_ne!(half, quarter);
    assert_ne!(half.encode(), quarter.encode());
}

/// 同一批 `(账户, 分数)` 无论输入顺序如何，编码结果逐字节相同。
///
/// 这是 `from_scores` 的「可交换、可结合」承诺的字节级落点，
/// 也是 S6 逐位确定性的前提。
#[test]
fn p1_输入顺序无关_编码逐字节相同() {
    let mut rng = XorShift::new(0x0F1E_2D3C_4B5A_6978);
    for seed_cap in [1usize, 2, 7, 16, 128] {
        for _ in 0..20 {
            let count = rng.range(0, seed_cap as u64 * 4) as usize;
            let mut pairs: Vec<(Acc, f32)> = Vec::with_capacity(count);
            for _ in 0..count {
                let acc = rng.next_below(seed_cap as u64 * 2 + 2) as u32;
                pairs.push((acc, random_score(&mut rng)));
            }
            let base = SparseView::from_scores(seed_cap, &pairs);
            let base_bytes = base.encode();

            // 逆序。
            let mut reversed = pairs.clone();
            reversed.reverse();
            assert_eq!(
                SparseView::from_scores(seed_cap, &reversed).encode(),
                base_bytes,
                "cap={seed_cap} 逆序输入应给出相同字节"
            );

            // 确定性打乱。
            let mut shuffled = pairs.clone();
            let n = shuffled.len();
            for i in (1..n).rev() {
                let j = rng.next_below(i as u64 + 1) as usize;
                shuffled.swap(i, j);
            }
            assert_eq!(
                SparseView::from_scores(seed_cap, &shuffled).encode(),
                base_bytes,
                "cap={seed_cap} 打乱输入应给出相同字节"
            );

            // 同一个输入跑两遍（没有隐藏全局状态）。
            assert_eq!(
                SparseView::from_scores(seed_cap, &pairs).encode(),
                base_bytes
            );
        }
    }
}

/// 把编码按 `base` 进制展开成 `len` 位下标（用于枚举小域组合）。
fn digits(base: usize, len: usize, code: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(len);
    let mut rest = code;
    for _ in 0..len {
        out.push(rest % base);
        rest /= base;
    }
    out
}

/// 稠密小域全枚举：`cap` 属于 `{1,2,3}`、条数从 0 到 `cap+1`、账户号/分数的全部小域组合。
///
/// 把「条数超容量、`q=0`、账户号重复/降序、填充区非零」这些分支在同一个枚举里
/// 全部走到，并与参考解码器逐条对拍：两边对每条输入的拒绝/接受必须完全一致。
#[test]
fn p1_稠密小域解码与参考一致() {
    let account_pool = [0u32, 1, 2, u32::MAX];
    let score_pool = [0u16, 1, Q_ONE];
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for cap in [1usize, 2, 3] {
        for count in 0..=(cap + 1) {
            let fixed = cap * REF_SLOT;
            let account_codes = account_pool.len().pow(count as u32);
            let score_codes = score_pool.len().pow(count as u32);
            for account_code in 0..account_codes {
                let account_digits = digits(account_pool.len(), count, account_code);
                for score_code in 0..score_codes {
                    let score_digits = digits(score_pool.len(), count, score_code);
                    let slots: Vec<(Acc, u16)> = account_digits
                        .iter()
                        .zip(&score_digits)
                        .map(|(&a, &q)| (account_pool[a], score_pool[q]))
                        .collect();
                    // 槽区只写得下 cap 条；条数 = cap+1 时第 cap+1 条不存在，
                    // 正好构造出「尾部条数 > 容量」的负载。
                    let writable = count.min(cap);
                    let mut payload = vec![0u8; fixed + REF_OVERHEAD];
                    for (i, &(acc, q)) in slots.iter().take(writable).enumerate() {
                        let off = i * REF_SLOT;
                        payload[off] = (acc & 0xff) as u8;
                        payload[off + 1] = ((acc >> 8) & 0xff) as u8;
                        payload[off + 2] = ((acc >> 16) & 0xff) as u8;
                        payload[off + 3] = ((acc >> 24) & 0xff) as u8;
                        payload[off + 4] = (q & 0xff) as u8;
                        payload[off + 5] = ((q >> 8) & 0xff) as u8;
                    }
                    payload[fixed] = (count & 0xff) as u8;
                    payload[fixed + 1] = ((count >> 8) & 0xff) as u8;

                    match xcheck_decode(&payload, cap) {
                        Some(view) => {
                            check_accepted(&view, &payload, cap);
                            accepted += 1;
                        }
                        None => rejected += 1,
                    }

                    // 填充区（条数 ≤ cap 时才是真的有填充区）首字节非零 ⟹ 必须拒绝。
                    if count <= cap {
                        let start = count * REF_SLOT;
                        if start < fixed {
                            let mut dirty = payload.clone();
                            dirty[start] = 1;
                            assert!(
                                xcheck_decode(&dirty, cap).is_none(),
                                "cap={cap} 条数={count} 时填充区首字节非零必须拒绝"
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(accepted > 0, "稠密枚举里没有一条被接受，枚举规模不对");
    assert!(rejected > 0, "稠密枚举里没有一条被拒绝，枚举规模不对");
}

// ═══════════════════ P2. 拒绝完备性 ═══════════════════

/// 逐个合法容量值钉边界：长度不符、条数超容量、cap 与负载不符都必须拒绝。
///
/// `cap` 从 1 到 4096 **逐值**扫拒绝路径（都在解析槽位之前返回，成本很低）；
/// 接受侧对 `1..=512` 逐值、对大容量抽样（空表与满表两种规范负载）。
#[test]
fn p2_每个容量值的拒绝与接受边界() {
    let mut buf = vec![0u8; REF_MAX_CAP * REF_SLOT + REF_OVERHEAD + 2];
    for cap in 1..=REF_MAX_CAP {
        let fixed = cap * REF_SLOT;
        let expected_len = fixed + REF_OVERHEAD;
        buf[fixed] = ((cap + 1) & 0xff) as u8;
        buf[fixed + 1] = (((cap + 1) >> 8) & 0xff) as u8;
        // 条数 = cap + 1 > cap：必须在解析槽位之前拒绝。
        assert!(
            xcheck_decode(&buf[..expected_len], cap).is_none(),
            "cap={cap}、条数 cap+1 必须拒绝"
        );
        // 长度差一：短 1 字节与长 1 字节都必须拒绝。
        assert!(
            xcheck_decode(&buf[..expected_len - 1], cap).is_none(),
            "cap={cap}、长度少 1 必须拒绝"
        );
        assert!(
            xcheck_decode(&buf[..expected_len + 1], cap).is_none(),
            "cap={cap}、长度多 1 必须拒绝"
        );
        // cap 与负载不符：每槽 6 字节，换 cap 后长度必然不符。
        if cap > 1 {
            let other = cap - 1;
            assert!(
                xcheck_decode(&buf[..expected_len], other).is_none(),
                "cap={cap} 的负载用 cap={other} 解码必须拒绝"
            );
        }
        if cap < REF_MAX_CAP {
            let other = cap + 1;
            assert!(
                xcheck_decode(&buf[..expected_len], other).is_none(),
                "cap={cap} 的负载用 cap={other} 解码必须拒绝"
            );
        }
    }

    // 接受侧：1..=512 逐值 + 大容量抽样，两种规范负载（空表、满表）。
    let mut sampled: Vec<usize> = (1..=512).collect();
    sampled.extend([768, 1024, 1536, 2048, 3072, 4095, REF_MAX_CAP]);
    for cap in sampled {
        let empty = vec![0u8; cap * REF_SLOT + REF_OVERHEAD];
        assert!(
            xcheck_decode(&empty, cap).is_some(),
            "cap={cap} 的空表规范负载必须被接受"
        );
        let full_slots: Vec<(Acc, u16)> = (0..cap as u32).map(|i| (i, 1u16)).collect();
        let full = manual_encode(cap, &full_slots);
        assert!(
            xcheck_decode(&full, cap).is_some(),
            "cap={cap} 的满表规范负载必须被接受"
        );
    }

    // 非法 cap：0 / 超过 MAX_CAP / 荒谬大，任何长度都必须拒绝且不 panic。
    let max_len = REF_MAX_CAP * REF_SLOT + REF_OVERHEAD;
    let lengths = [0usize, 1, 2, REF_SLOT, max_len, max_len + 1];
    for cap in [
        0usize,
        REF_MAX_CAP + 1,
        REF_MAX_CAP + 2,
        100_000,
        u32::MAX as usize,
        usize::MAX,
    ] {
        for &len in &lengths {
            let payload = vec![0u8; len];
            assert!(
                xcheck_decode(&payload, cap).is_none(),
                "非法 cap={cap}、长度 {len} 必须拒绝"
            );
        }
    }
}

/// 条数超容量：`cap=1/2` 时把 `count` 从 `cap+1` **全扫描**到 `u16::MAX`。
///
/// 其余容量抽样覆盖边界（含 `count == cap` 的接受侧，避免拒绝检查误伤合法上界）。
#[test]
fn p2_条数超容量全扫描() {
    for cap in [1usize, 2] {
        let fixed = cap * REF_SLOT;
        let mut payload = vec![0u8; fixed + REF_OVERHEAD];
        let first = cap as u16 + 1;
        for count in first..=u16::MAX {
            payload[fixed] = (count & 0xff) as u8;
            payload[fixed + 1] = ((count >> 8) & 0xff) as u8;
            assert!(
                xcheck_decode(&payload, cap).is_none(),
                "cap={cap}、条数 {count} 必须拒绝"
            );
        }
    }

    for cap in [3usize, 8, 128, 512, REF_MAX_CAP] {
        let fixed = cap * REF_SLOT;
        let mut payload = vec![0u8; fixed + REF_OVERHEAD];
        let boundary = [
            cap as u16 + 1,
            cap as u16 + 2,
            1000,
            4096,
            32_767,
            65_534,
            u16::MAX,
        ];
        for count in boundary {
            if count <= cap as u16 {
                continue; // 不超容量的取法由下面的接受侧负责
            }
            payload[fixed] = (count & 0xff) as u8;
            payload[fixed + 1] = ((count >> 8) & 0xff) as u8;
            assert!(
                xcheck_decode(&payload, cap).is_none(),
                "cap={cap}、条数 {count} 必须拒绝"
            );
        }
        // count == cap：满表规范负载必须接受。
        let full_slots: Vec<(Acc, u16)> = (0..cap as u32).map(|i| (i, 1u16)).collect();
        let full = manual_encode(cap, &full_slots);
        assert!(
            xcheck_decode(&full, cap).is_some(),
            "cap={cap}、条数 == cap 必须接受"
        );
    }
}

/// `q == 0` 的槽必须拒绝：0 就是「没表态」，规范形式里不能占槽位。
///
/// 对 `cap` 属于 `{1,2,3,8,128}` 的每个条数、**每个槽位**都把 `q` 清零；
/// 同时反向验证「`q` 低字节非零、高字节为 0」的槽仍然被接受
/// （拒绝的是 `q == 0`，不是某个字节为 0）。
#[test]
fn p2_零分槽一律拒绝() {
    for cap in [1usize, 2, 3, 8, 128] {
        for count in 1..=cap {
            let slots: Vec<(Acc, u16)> = (0..count).map(|i| (i as u32, i as u16 + 1)).collect();
            let base = manual_encode(cap, &slots);
            assert!(xcheck_decode(&base, cap).is_some(), "基线必须被接受");
            let sum_all: u32 = slots.iter().map(|&(_, q)| u32::from(q)).sum();
            for (slot, &(acc, original_q)) in slots.iter().enumerate() {
                let off = slot * REF_SLOT;
                let mut bad = base.clone();
                bad[off + 4] = 0;
                bad[off + 5] = 0;
                assert!(
                    xcheck_decode(&bad, cap).is_none(),
                    "cap={cap} 条数={count} 第 {slot} 槽 q=0 必须拒绝"
                );
                let mut nonzero = base.clone();
                nonzero[off + 4] = 1;
                nonzero[off + 5] = 0;
                let view = xcheck_decode(&nonzero, cap).expect("q=1 的槽必须被接受");
                assert_eq!(view.q_of(acc), 1, "q=1 必须原样读回");
                let expect_total = sum_all - u32::from(original_q) + 1;
                assert_eq!(view.total_q(), expect_total, "其余槽不变时总质量可精确算出");
            }
        }
    }
}

/// 账户号必须**严格**升序：相等（重复账户）与降序都必须拒绝。
///
/// 对每个相邻位置做三种破坏：与前槽相等、比前槽小、把前槽抬到比后槽大。
/// 拒绝理由不同（重复 / 降序 / 乱序），但都是同一条严格升序契约。
#[test]
fn p2_非严格升序一律拒绝() {
    for cap in [2usize, 3, 8, 128] {
        for count in 2..=cap {
            let slots: Vec<(Acc, u16)> =
                (0..count).map(|i| (i as u32 * 3 + 1, 32_768u16)).collect();
            let base = manual_encode(cap, &slots);
            assert!(xcheck_decode(&base, cap).is_some(), "基线必须被接受");
            for slot in 1..count {
                let prev_acc = slots[slot - 1].0;
                let off = slot * REF_SLOT;
                // (a) 相等：重复账户 = 双倍预算，必须拒绝。
                let mut equal = base.clone();
                equal[off..off + 4].copy_from_slice(&prev_acc.to_le_bytes());
                assert!(
                    xcheck_decode(&equal, cap).is_none(),
                    "cap={cap} 条数={count} 第 {slot} 槽与前槽相等必须拒绝"
                );
                // (b) 降序：比前槽小 1。
                let lower = prev_acc - 1;
                let mut desc = base.clone();
                desc[off..off + 4].copy_from_slice(&lower.to_le_bytes());
                assert!(
                    xcheck_decode(&desc, cap).is_none(),
                    "cap={cap} 条数={count} 第 {slot} 槽降序必须拒绝"
                );
                // (c) 逆序：把前槽抬到比后槽大，跨越一处违规。
                let next_acc = slots[slot].0;
                if next_acc < u32::MAX {
                    let raised = next_acc + 1;
                    let prev_off = (slot - 1) * REF_SLOT;
                    let mut swapped = base.clone();
                    swapped[prev_off..prev_off + 4].copy_from_slice(&raised.to_le_bytes());
                    let before = slot - 1;
                    assert!(
                        xcheck_decode(&swapped, cap).is_none(),
                        "cap={cap} 条数={count} 第 {before} 槽逆序必须拒绝"
                    );
                }
            }
        }
    }
}

/// 填充区夹带：**每一个** `(条数, 字节位置)` 组合都要拒绝。
///
/// `cap` 属于 `{1,2,3,8,128}` 全 count 全位置双循环；`cap=512` 抽样
/// `count` 属于 `{0,1,2,511,512}` 的全位置扫描；另在 `cap=128` 的填充区首/中/尾
/// 用 `1 / 0x80 / 0xFF` 三种脏值复核。
#[test]
fn p2_填充区逐字节全扫描() {
    for cap in [1usize, 2, 3, 8, 128] {
        let fixed = cap * REF_SLOT;
        for count in 0..=cap {
            let slots: Vec<(Acc, u16)> =
                (0..count).map(|i| (i as u32 * 7 + 1, 32_768u16)).collect();
            let mut payload = manual_encode(cap, &slots);
            assert!(
                xcheck_decode(&payload, cap).is_some(),
                "基线必须被接受（cap={cap}、条数={count}）"
            );
            let start = count * REF_SLOT;
            let mut pos = start;
            while pos < fixed {
                payload[pos] = 1;
                assert!(
                    xcheck_decode(&payload, cap).is_none(),
                    "cap={cap} 条数={count}：填充区偏移 {pos} 非零必须拒绝"
                );
                payload[pos] = 0;
                pos += 1;
            }
        }
    }

    // 大容量抽样：cap=512，count 属于 {0,1,2,511,512}（含「没有填充区」的边界）。
    let cap = 512usize;
    let fixed = cap * REF_SLOT;
    for count in [0usize, 1, 2, 511, 512] {
        let slots: Vec<(Acc, u16)> = (0..count).map(|i| (i as u32, 32_768u16)).collect();
        let mut payload = manual_encode(cap, &slots);
        assert!(
            xcheck_decode(&payload, cap).is_some(),
            "基线必须被接受（cap={cap}、条数={count}）"
        );
        let start = count * REF_SLOT;
        let mut pos = start;
        while pos < fixed {
            payload[pos] = 0x80;
            assert!(
                xcheck_decode(&payload, cap).is_none(),
                "cap={cap} 条数={count}：填充区偏移 {pos} 非零必须拒绝"
            );
            payload[pos] = 0;
            pos += 1;
        }
    }

    // 不同脏值（1 / 0x80 / 0xFF）在填充区首 / 中 / 尾都必须拒绝。
    let cap = 128usize;
    let fixed = cap * REF_SLOT;
    for count in [0usize, 1, 64, 127] {
        let slots: Vec<(Acc, u16)> = (0..count).map(|i| (i as u32 * 2, 16_384u16)).collect();
        let base = manual_encode(cap, &slots);
        let start = count * REF_SLOT;
        let last = fixed - 1;
        let middle = start + (fixed - start) / 2;
        for pos in [start, middle, last] {
            for dirty in [1u8, 0x80, 0xff] {
                let mut payload = base.clone();
                payload[pos] = dirty;
                assert!(
                    xcheck_decode(&payload, cap).is_none(),
                    "cap={cap} 条数={count} 填充区偏移 {pos} 写成 {dirty} 必须拒绝"
                );
            }
        }
    }
}

/// 长度与容量不符：长度必须**恰好** `cap*6+2`，差 1 字节都不行。
///
/// 另测「负载长度恰好是另一个 cap 的规范长度」：换 cap 解码必须拒绝。
#[test]
fn p2_长度与容量不符() {
    for cap in [1usize, 2, 3, 8, 128, 512, REF_MAX_CAP] {
        let expected_len = cap * REF_SLOT + REF_OVERHEAD;
        let mut lengths = vec![
            0usize,
            1,
            2,
            expected_len - REF_SLOT,
            expected_len - 2,
            expected_len - 1,
            expected_len + 1,
            expected_len + 2,
            expected_len + REF_SLOT,
        ];
        lengths.sort_unstable();
        for len in lengths {
            if len == expected_len {
                continue;
            }
            let payload = vec![0u8; len];
            assert!(
                xcheck_decode(&payload, cap).is_none(),
                "cap={cap}、长度 {len}（期望 {expected_len}）必须拒绝"
            );
        }
    }

    // 另一个 cap 的规范负载：长度按 6 字节一格变化，任何别的 cap 都不可能匹配。
    for source_cap in [1usize, 2, 8, 128, 512] {
        let payload = vec![0u8; source_cap * REF_SLOT + REF_OVERHEAD];
        for target_cap in [1usize, 2, 8, 128, 512, REF_MAX_CAP] {
            if target_cap == source_cap {
                continue;
            }
            assert!(
                xcheck_decode(&payload, target_cap).is_none(),
                "源 cap={source_cap} 的负载用目标 cap={target_cap} 解码必须拒绝"
            );
        }
    }
}

// ═══════════════════ P3. 模糊测试 ═══════════════════

/// CI 口径的模糊测试：2 万随机负载 + 2 万合法负载变异，全确定性可复现。
///
/// 函数名用 `ci` 而不是 `CI`：前者不触发 `non_snake_case`，后者会让 clippy 报 warning。
///
/// 每个样本都做三方差分与接受侧读数检查（见 `run_fuzz`）。计时只做宽松看门狗，
/// 不作为性能预算断言，避免 CI 抖动误报；实测耗时写在报告里。
#[test]
fn p3_模糊_ci口径() {
    let start = Instant::now();
    let stats = run_fuzz(20_000, 20_000, 0x5EED_C0DE_1234_5678);
    let elapsed = start.elapsed();
    let desc = stats.describe();
    println!("P3 模糊（CI 口径，种子 0x5EED_C0DE_1234_5678）：{desc}，耗时 {elapsed:?}");
    assert!(
        stats.rand_ok > 0,
        "随机样本没有一条被接受，模糊测试没打到接受面"
    );
    assert!(
        stats.mut_ok > 0,
        "变异样本没有一条被接受，模糊测试没打到「改字节仍合法」的面"
    );
    assert!(
        stats.rand_n > stats.rand_ok,
        "随机样本必须含被拒绝的输入，否则拒绝面没被覆盖"
    );
    assert!(
        elapsed.as_secs() < 120,
        "CI 口径的模糊测试超出看门狗，实测 {elapsed:?}"
    );
}

/// 全量模糊：100 万随机负载 + 10 万变异（`#[ignore]`，不进 CI）。
///
/// 手动跑：`cargo test --release --test codec_props -- --ignored --nocapture`
#[test]
#[ignore = "全量模糊：100 万随机 + 10 万变异，用 --release --ignored 手动跑"]
fn p3_模糊_全量() {
    let start = Instant::now();
    let stats = run_fuzz(1_000_000, 100_000, 0x0D15_EA5E_0BAD_F00D);
    let elapsed = start.elapsed();
    let desc = stats.describe();
    println!("P3 模糊（全量，种子 0x0D15_EA5E_0BAD_F00D）：{desc}，耗时 {elapsed:?}");
    assert!(stats.rand_ok > 0);
    assert!(stats.mut_ok > 0);
}

// ═══════════════════ P4. 交叉验证 ═══════════════════

/// 独立慢速实现（合并部分）：`BTreeMap` 合并（重复账户取最大、0 分丢弃），按账户号升序。
///
/// 与生产实现的差异：这里用 `get_mut`/`insert` 的显式分支，而不是
/// `entry().and_modify().or_insert()` 的链式写法。
fn slow_merge(pairs: &[(Acc, f32)]) -> Vec<(Acc, u16)> {
    let mut merged: BTreeMap<Acc, u16> = BTreeMap::new();
    for &(acc, score) in pairs {
        let q = quantize(score);
        if q == 0 {
            continue;
        }
        match merged.get_mut(&acc) {
            Some(current) => {
                if q > *current {
                    *current = q;
                }
            }
            None => {
                merged.insert(acc, q);
            }
        }
    }
    merged.into_iter().collect()
}

/// 独立慢速实现（截断部分）：`(分数降序, 账户号升序)` 保留前 `cap` 条，再按账户号升序落表。
fn slow_truncate(merged: &[(Acc, u16)], cap: usize) -> Vec<(Acc, u16)> {
    if merged.len() <= cap {
        return merged.to_vec();
    }
    let mut candidates = merged.to_vec();
    candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    candidates.truncate(cap);
    candidates.sort_unstable_by_key(|&(acc, _)| acc);
    candidates
}

/// 随机分数：混合「精确网格值」（用 `dequantize` 造，保证量化后就是想要的 `q`）、
/// 并列值（小 `q` 大量重复）、以及 NaN / 负数 / 超大值 / 归零的小值。
fn random_score(rng: &mut XorShift) -> f32 {
    match rng.next_below(10) {
        0 => 0.0,
        1 => -1.0,
        2 => f32::NAN,
        3 => 1e9,
        4 => 0.5 / 65535.0 * 0.5,
        5 => dequantize(rng.range(1, 65_535) as u16),
        6 => dequantize(rng.range(1, 4) as u16),
        7 => dequantize(rng.range(1, 8) as u16),
        8 => 1.0,
        _ => rng.range(1, 1000) as f32 / 1000.0,
    }
}

/// P4：`from_scores` 与测试内独立慢速实现逐条对拍。
///
/// 每个用例检查：槽位表 / 条数 / `total_q` 全等、手工装配字节 == `encode`、
/// 编码解码往返、截断不变式（保留的是合并集合的子集、被丢的读作 0、
/// 条数 = `min(合并条数, cap)`）、`truncated_mass` 口径、输入顺序无关。
#[test]
fn p4_与独立慢速实现交叉验证() {
    let mut rng = XorShift::new(0xA11C_E5EE_1234_5678);
    let caps = [1usize, 2, 3, 5, 7, 13, 32, 128, 512];
    for round in 0..300usize {
        let cap = caps[round % caps.len()];
        let pair_count = rng.range(0, cap as u64 * 3 + 8) as usize;
        let acc_bound = rng.range(1, cap as u64 * 4 + 4);
        let mut pairs: Vec<(Acc, f32)> = Vec::with_capacity(pair_count);
        for _ in 0..pair_count {
            let acc = rng.next_below(acc_bound) as u32;
            let score = random_score(&mut rng);
            pairs.push((acc, score));
        }

        let view = SparseView::from_scores(cap, &pairs);
        let merged = slow_merge(&pairs);
        let expected = slow_truncate(&merged, cap);
        assert_eq!(
            view.slots(),
            expected.as_slice(),
            "第 {round} 轮（cap={cap}）槽位与慢速实现不一致"
        );
        assert_eq!(view.cap(), cap);
        assert_eq!(view.len(), expected.len());
        assert_eq!(
            view.total_q(),
            expected.iter().map(|&(_, q)| u32::from(q)).sum::<u32>()
        );

        // 手工装配的规范字节串必须与 encode 逐字节相同。
        let manual = manual_encode(cap, &expected);
        assert_eq!(
            view.encode(),
            manual,
            "第 {round} 轮编码字节与手工装配不一致"
        );
        assert_eq!(decode_no_panic(&manual, cap).as_ref(), Some(&view));

        // 截断不变式：条数 = min(合并条数, cap)；保留的来自合并集合且 q 不变。
        assert_eq!(expected.len(), merged.len().min(cap));
        let merged_map: BTreeMap<Acc, u16> = merged.iter().copied().collect();
        let expected_map: BTreeMap<Acc, u16> = expected.iter().copied().collect();
        for &(acc, q) in &expected {
            assert_eq!(merged_map.get(&acc), Some(&q), "保留的槽必须来自合并集合");
            assert_eq!(view.q_of(acc), q);
        }
        for (&acc, &q) in &merged_map {
            match expected_map.get(&acc) {
                Some(&kept) => assert_eq!(kept, q),
                None => assert_eq!(view.q_of(acc), 0, "被截断的账户必须读作 0"),
            }
        }

        // truncated_mass 口径：1 - 保留质量 / 全部质量（独立重算后逐位比较）。
        let all_mass: u64 = merged.iter().map(|&(_, q)| u64::from(q)).sum();
        let kept_mass: u64 = u64::from(view.total_q());
        let expected_loss = if all_mass == 0 {
            0.0
        } else {
            1.0 - kept_mass as f64 / all_mass as f64
        };
        assert_eq!(
            SparseView::truncated_mass(cap, &pairs).to_bits(),
            expected_loss.to_bits(),
            "第 {round} 轮（cap={cap}）truncated_mass 口径不一致"
        );

        // 输入顺序无关：逆序输入必须逐字节相同；每 10 轮再做一次随机打乱。
        let mut reversed = pairs.clone();
        reversed.reverse();
        assert_eq!(
            SparseView::from_scores(cap, &reversed).encode(),
            manual,
            "第 {round} 轮逆序输入改变了编码"
        );
        if round % 10 == 0 {
            let mut shuffled = pairs.clone();
            let n = shuffled.len();
            for i in (1..n).rev() {
                let j = rng.next_below(i as u64 + 1) as usize;
                shuffled.swap(i, j);
            }
            assert_eq!(
                SparseView::from_scores(cap, &shuffled).encode(),
                manual,
                "第 {round} 轮打乱输入改变了编码"
            );
        }

        // 归一化读数：非空时 Σv ≡ 1（1e-6 内）。
        if !view.is_empty() {
            let sum: f64 = view
                .normalized_slots()
                .iter()
                .map(|&(_, value)| f64::from(value))
                .sum();
            assert!((sum - 1.0).abs() < 1e-6, "第 {round} 轮 Σv = {sum}");
        }
    }
}

/// P4 的边界用例：不同账户数恰好等于 / 多一个 `cap` 时，截断与否的分界。
#[test]
fn p4_截断边界_精确容量() {
    for cap in [1usize, 2, 3, 7] {
        for acc_count in [cap, cap + 1] {
            let pairs: Vec<(Acc, f32)> = (0..acc_count as u32).map(|i| (i, 1.0)).collect();
            let view = SparseView::from_scores(cap, &pairs);
            let merged = slow_merge(&pairs);
            let expected = slow_truncate(&merged, cap);
            assert_eq!(view.len(), cap.min(acc_count));
            assert_eq!(view.slots(), expected.as_slice());
            assert_eq!(view.encode(), manual_encode(cap, &expected));
            // 超过容量的部分：账户号大的被丢（同分并列按账户号升序保留）。
            if acc_count > cap {
                for acc in cap as u32..acc_count as u32 {
                    assert_eq!(view.q_of(acc), 0, "并列截断必须保留账户号小的");
                }
            }
        }
    }
}

// ═══════════════════ P5. codec 层定点往返 ═══════════════════

/// P5：`q` 属于 `[1, 65535]` 全量经 **u16 字节载荷** `encode`↔`decode` 逐字节往返。
///
/// **不重复** `fixed.rs` 的 `quantize_dequantize_往返恒等`（它已穷举 65536 个定点值的
/// 定点数学往返）。这里只测 codec 层：`q` 以小端 u16 写进负载、`decode` 读回、
/// `encode` 再写回，逐字节不变；`q=0` 则必须被拒绝（0 就是「没表态」）。
#[test]
fn p5_codec层定点全程往返() {
    let cap = 1usize;
    let acc = 9u32;
    for q in 1..=Q_ONE {
        let payload = manual_encode(cap, &[(acc, q)]);
        let view = decode_no_panic(&payload, cap).expect("q>0 的规范负载必须被接受");
        assert_eq!(view.q_of(acc), q, "q={q} 经字节载荷往返后变了");
        assert_eq!(view.total_q(), u32::from(q));
        assert_eq!(
            view.encode().as_slice(),
            payload.as_slice(),
            "q={q} 往返后字节变了"
        );
        assert_eq!(ref_decode(&payload, cap), Some((cap, vec![(acc, q)])));
    }

    // q = 0 不是合法槽位（规范形式里 0 分不占槽）。
    let zero = manual_encode(cap, &[(acc, 0)]);
    assert!(xcheck_decode(&zero, cap).is_none(), "q=0 的槽必须拒绝");

    // 两个端点的字节形态（低字节在前）。
    assert_eq!(
        manual_encode(cap, &[(acc, 1)]),
        vec![9, 0, 0, 0, 1, 0, 1, 0]
    );
    assert_eq!(
        manual_encode(cap, &[(acc, Q_ONE)]),
        vec![9, 0, 0, 0, 255, 255, 1, 0]
    );
}

// ═══════════════════ 附. 已知 API 面差异（钉住现状，不判为缺陷） ═══════════════════

/// 同一个非法 cap 在三个公开入口上的行为不同：`new` panic、`decode` 返回 `None`、
/// `truncated_mass` 静默给数值。
///
/// `new` 的 panic 已由 `view.rs` 的 `new_cap为零必须panic` / `new_cap超上限必须panic`
/// 钉住，`truncated_mass` 的不校验已由 `truncated_mass_非法cap不校验` 钉住 —— 这里
/// 不重复，只把「同一个非法 cap、`decode` 拒绝而 `truncated_mass` 静默出数值」的
/// 对照钉出来。这是 API 面一致性的观察，**不是安全缺陷**：三个入口契约本来就不同
/// （new 是配置错误、decode 是不可信输入、truncated_mass 是报告侧估算）。
#[test]
fn 附_非法cap三入口行为不同_现状钉住() {
    // decode：非法 cap 一律拒绝，不 panic（走 catch_unwind 版本）。
    let payload = vec![0u8; 2];
    assert!(decode_no_panic(&payload, 0).is_none());
    assert!(decode_no_panic(&[], REF_MAX_CAP + 1).is_none());
    assert!(decode_no_panic(&[], usize::MAX).is_none());

    // 同一个 cap = 0：decode 拒绝，truncated_mass 却给出 1.0（「全部被截掉」）。
    assert!(decode_no_panic(&payload, 0).is_none());
    assert_eq!(SparseView::truncated_mass(0, &[(1, 1.0)]), 1.0);
    // cap = MAX_CAP + 1：decode 拒绝，truncated_mass 却给出 0.0（「没截断」）。
    assert!(decode_no_panic(&[], REF_MAX_CAP + 1).is_none());
    assert_eq!(
        SparseView::truncated_mass(REF_MAX_CAP + 1, &[(1, 1.0), (2, 1.0)]),
        0.0
    );
}

/// `encode` 里 `self.slots.len() as u16` 没有校验，但公开路径不可达。
///
/// 只有走私有 `from_slots_unchecked` 且条数 > 65535 时，才会写出「自家 `decode`
/// 都拒收」的条数；而所有公开构造器都把条数压在 `cap ≤ MAX_CAP = 4096 < 65536` 以内，
/// 所以这条不可达。本测试钉住这个「不可达但仍应守」的算术前提。
#[test]
fn 附_encode条数无校验但公开路径不可达() {
    // 算术前提：公开路径的条数上界 MAX_CAP 严格小于 u16 容量。
    for cap in [1usize, 2, 128, 512, REF_MAX_CAP] {
        assert!(cap <= REF_MAX_CAP);
        assert!(
            cap <= u16::MAX as usize,
            "cap={cap} 必须能无损写进 u16 条数"
        );
    }

    for cap in [1usize, 2, 3, 128, 512, REF_MAX_CAP] {
        let pairs: Vec<(Acc, f32)> = (0..cap as u32 * 2).map(|i| (i, 1.0)).collect();
        let view = SparseView::from_scores(cap, &pairs);
        assert_eq!(view.len(), cap, "cap={cap} 时截断到满容量");
        assert!(view.len() <= u16::MAX as usize);
        let payload = view.encode();
        let fixed = cap * REF_SLOT;
        let count = (payload[fixed] as usize) + 256 * (payload[fixed + 1] as usize);
        assert_eq!(count, view.len(), "尾部条数是 len 的无损写入");
        assert!(
            xcheck_decode(&payload, cap).is_some(),
            "公开路径造出的字节必须被自家 decode 接受"
        );
    }

    // 公开路径能造出的最多条数：MAX_CAP 满额，仍远小于 65536。
    let pairs: Vec<(Acc, f32)> = (0..REF_MAX_CAP as u32).map(|i| (i, 1.0)).collect();
    let full = SparseView::from_scores(REF_MAX_CAP, &pairs);
    assert_eq!(full.len(), REF_MAX_CAP);
    assert!(full.len() < 65_536);
    assert!(xcheck_decode(&full.encode(), REF_MAX_CAP).is_some());
}
