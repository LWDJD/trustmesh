//! native 侧的对拍基准：走**完全相同的源码**（`tm_wasm_smoke::run`），打印位模式。
//!
//! 这样对比才有意义——如果 native 侧另写一份代码，比的就不是平台差异了。

fn main() {
    let v = tm_wasm_smoke::run();
    println!("count={}", v.len());
    let hex: Vec<String> = v.iter().map(|x| format!("0x{:08x}", x.to_bits())).collect();
    println!("{}", hex.join(" "));
}
