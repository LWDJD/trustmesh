//! native 侧的对拍基准：走**完全相同的源码**（tm_mincore_wasm::run_all），打印位模式与快照字节。
//!
//! 这样对比才有意义——如果 native 侧另写一份代码，比的就不是平台差异了。
//!
//! 输出格式（run-compare.mjs 与 run-all.ps1 都依赖它，改格式要同步改两边）：
//!
//! ~~~text
//! count=2739
//! 0x........ 0x........ ...        // 一行，2739 个 f32 位模式
//! snaps=12
//! 770 770 770 770 1538 ...         // 一行，12 张快照各自的字节长度
//! <770 字节的十六进制>              // 每张快照一行，小写、不带 0x 前缀
//! ...
//! ~~~
//!
//! 用十六进制而不是 base64：十六进制是**纯 ASCII**，不会碰 PowerShell 重定向的编码问题。

fn main() {
    let probe = tm_mincore_wasm::run_all();

    println!("count={}", probe.bits.len());
    let hex: Vec<String> = probe.bits.iter().map(|x| format!("0x{:08x}", x)).collect();
    println!("{}", hex.join(" "));

    println!("snaps={}", probe.snaps.len());
    let lens: Vec<String> = probe
        .snaps
        .iter()
        .map(|s| s.bytes.len().to_string())
        .collect();
    println!("{}", lens.join(" "));
    for s in &probe.snaps {
        let mut line = String::with_capacity(s.bytes.len() * 2);
        for b in &s.bytes {
            line.push(hex_digit(b >> 4));
            line.push(hex_digit(b & 0x0f));
        }
        println!("{line}");
    }
}

/// 0..16 转一个十六进制小写字符。
fn hex_digit(n: u8) -> char {
    debug_assert!(n < 16);
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + (n - 10)) as char
    }
}
