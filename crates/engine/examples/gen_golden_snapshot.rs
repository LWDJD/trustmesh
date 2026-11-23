//! 生成"黄金快照"字节，供 `tests/snapshot_golden.rs` 硬编码使用。
//!
//! ## 为什么需要这个
//!
//! `SNAPSHOT_VERSION` 从 2 改成 3 时，全部 479 条测试仍然全绿——因为所有测试都用
//! 同一个常量（编码用 VERSION、解码也检查 VERSION），常量一改两边一起变，永远自洽。
//! 真实后果是「已存的 v2 快照文件全部解不开了，而测试全绿」。
//!
//! 这个 example 生成一段**当前 v2 格式**的确定性字节，粘进
//! `tests/snapshot_golden.rs` 后，将来任何人抬 VERSION 都会立刻看到那条测试变红，
//! 从而必须**显式决定**是否要破坏旧快照。
//!
//! ## 用法
//!
//! ```text
//! cd crates/engine && cargo run --example gen_golden_snapshot > ../../.tmp/golden.txt
//! ```
//!
//! 输出就是 `tests/snapshot_golden.rs` 里 `GOLDEN_BYTES` 那段，直接整段替换即可。
//!
//! ## 为什么桶数取 64×3
//!
//! 只为**覆盖格式**，不为测性能。512×5（10240 B/份）会让 golden 变成 20 KB，
//! 放进测试文件太笨重；64×3 = 768 B/份，两份视图 → 快照约 2 KB，够看清所有字段。

use tm_engine::view::ViewParams;
use tm_engine::{Engine, EngineConfig};

fn key(name: &str) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..name.len()].copy_from_slice(name.as_bytes());
    k
}

fn main() {
    // 覆盖尽量多的状态字段，将来改格式时这条测试最可能抓到问题。
    let params = ViewParams::new(64, 3, 7);
    let cfg = EngineConfig {
        view_params: params,
        max_views: 16,
        ..EngineConfig::default()
    };

    let mut e = Engine::with_seed(cfg, 42);
    for n in ["alice", "bob", "carol", "dave"] {
        e.intern(key(n));
    }
    e.rate(1, 0.9, 0);
    e.rate(2, 0.7, 0);
    e.rate(3, 0.3, 0);

    // 收两份观点（覆盖 views 缓存 + total_mass + noise + mass_covers）
    let v1 = e.export_view(10);
    e.on_view(1, v1, 10, 10);
    let v2 = e.export_view(10);
    e.on_view(2, v2, 10, 10);

    // 一次失败（覆盖 fail_streak + 轮次位图）
    e.on_view_failed(3, 10);

    // 一次抽样（覆盖 rng 状态 + last_k）
    let _ = e.tick(10, 2);

    // 一次 badlist（覆盖 penalties 表）
    let _ = e.report_bad_account(3);

    let snap = e.snapshot(10);
    let bytes = snap.encode();

    println!("//! 由 `cargo run --example gen_golden_snapshot` 生成，**不要手改**。");
    println!("//!");
    println!(
        "//! 生成日期：2026-11-22（SNAPSHOT_VERSION = {}）",
        snap.version
    );
    println!("//! 生成脚本的自检：同一代码重跑必须给出**逐字节相同**的输出。");
    println!();
    println!("/// 这段字节里记录的格式版本号。");
    println!("pub const GOLDEN_VERSION: u32 = {};", snap.version);
    println!();
    println!("/// 字节长度（长度本身也是格式的一部分）。");
    println!("pub const GOLDEN_LEN: usize = {};", bytes.len());
    println!();
    println!("/// 2026-11-22 由 gen_golden_snapshot 生成的 v2 快照。");
    println!("///");
    println!("/// ## 里面有什么（覆盖了哪些状态字段）");
    println!("///");
    println!("/// * 4 个账户（alice / bob / carol / dave）");
    println!("/// * 3 条正分清单（0.9 / 0.7 / 0.3，含份额与 last_check）");
    println!("/// * 2 份缓存的观点（含 total_mass / noise / mass_covers）");
    println!("/// * 1 条失败记录（fail_streak）");
    println!("/// * rng 状态（一次 tick 之后）");
    println!("/// * 1 条 penalties 记录（badlist 报告后）");
    println!("pub const GOLDEN_BYTES: &[u8] = &[");
    for chunk in bytes.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
        println!("    {},", hex.join(", "));
    }
    println!("];");
}
