//! # wasm32 冒烟测试
//!
//! 目的：证明**整个引擎**（不是某个纯函数）在 wasm32 和 native 上给出**逐位相同**的结果。
//!
//! 为什么这件事重要：这个库要跑在浏览器 / Electron / 各种 wasm runtime 里。
//! 如果同一个 seed、同样的输入在两个平台上给出不同的声望值，那么：
//! * S6（「同输入必然同输出」）在跨平台意义上失效；
//! * 未来的 ZKP 验证（证明要跨机器验）会失败；
//! * 用户会看到「我这边和服务器算的不一样」这种没法排查的问题。
//!
//! 本探针跑的是**完整流程**：rate → tick → export_view → on_view → query，
//! 三轮传播后把 6×6=36 个声望值写成 f32 位模式，供 native 侧逐位对拍。
//!
//! 用法（在仓库根目录）：
//!
//! ~~~powershell
//! cd tools/wasm-smoke
//! cargo build --release --lib --target wasm32-unknown-unknown
//! node run-compare.mjs
//! ~~~
//!
//! 脚本会自己跑一遍 native 侧再对比，最后打印「IDENTICAL」或第一个差异位置。

use tm_engine::{Engine, EngineConfig};
use tm_view::ViewParams;

/// 探针里用的账户号：可读、可复现。
fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = 0xA5;
    k
}

/// 跑完整流程，返回全部（观察者 × 账户）的声望。
///
/// 参数全部写死：任何平台的任何一次运行都必须得到同一份结果。
pub fn run() -> Vec<f32> {
    let cfg = EngineConfig {
        beta: 0.3,
        half_life: 72,
        view_params: ViewParams::new(512, 5, 7),
        ..EngineConfig::default()
    };

    // 6 个节点，每个一个引擎。编号顺序全网一致（intern 的先后）。
    let mut engines: Vec<Engine> = (0..6)
        .map(|_| {
            let mut e = Engine::with_seed(cfg.clone(), 42);
            for i in 0..6u32 {
                e.intern(key(i));
            }
            e
        })
        .collect();

    // 打分（0~1）
    engines[0].rate(1, 0.9, 0);
    engines[0].rate(2, 0.7, 0);
    engines[1].rate(0, 0.8, 0);
    engines[1].rate(5, 0.6, 0);
    engines[2].rate(0, 0.8, 0);
    engines[3].rate(1, 0.9, 0);

    // 三轮传播，每轮 12 小时
    for round in 0..3u64 {
        let now = 12 * (round + 1);
        let published: Vec<_> = engines.iter().map(|e| e.export_view(now)).collect();
        let fetches: Vec<Vec<u32>> = engines
            .iter_mut()
            .map(|e| e.tick(now, 3).into_iter().map(|r| r.idx).collect())
            .collect();
        for (i, reqs) in fetches.iter().enumerate() {
            for &owner in reqs {
                engines[i].on_view(owner, published[owner as usize].clone(), now, now);
            }
        }
    }

    let mut out = Vec::with_capacity(36);
    for e in &engines {
        for t in 0..6u32 {
            out.push(e.query(t, 36));
        }
    }
    out
}

/// 结果缓冲区。wasm 侧不能从 JS 传指针进来（TypedArray 会被强转成 0），
/// 所以由本模块自己持有内存并导出地址，JS 从 wasm 线性内存里读回。
static mut OUT_BUF: [u32; OUT_CAP] = [0; OUT_CAP];

/// 缓冲区容量（编译期常量，供检查与导出使用）。
const OUT_CAP: usize = 64;

// 编译期检查：缓冲区必须能装下 36 个结果。
const _: () = assert!(OUT_CAP >= 36);

/// 跑一遍并把结果位模式写进内部缓冲区，返回写入个数（= 36）。
#[no_mangle]
pub extern "C" fn fill_scores() -> u32 {
    let v = run();
    let n = v.len().min(OUT_CAP);
    for (i, x) in v.iter().enumerate().take(OUT_CAP) {
        // SAFETY: 单线程、无并发访问；写入范围由 n..min(64) 限制。
        unsafe { OUT_BUF[i] = x.to_bits() };
    }
    n as u32
}

/// 结果缓冲区的地址（wasm 线性内存内的字节偏移）。
#[no_mangle]
pub extern "C" fn out_buf_addr() -> u32 {
    // 用 addr_of! 取裸指针，**不创建**对 mutable static 的引用
    // （`OUT_BUF.as_ptr()` 会创建共享引用，在 Rust 2024 里属于未定义行为）。
    // addr_of! 本身不需要 unsafe：它只算地址、不解引用。
    std::ptr::addr_of!(OUT_BUF) as u32
}
