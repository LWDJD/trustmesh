// wasm32 vs native 逐位对拍。
//
// **为什么 native 侧的结果从文件读，而不是在这里 spawn 出来跑**：
// 本机沙箱禁止程序通过管道打开子进程（Node 的 child_process 默认 stdio:'pipe' 会 EPERM），
// 这是环境边界不是 bug。所以由外层 shell 先跑 native 并落盘，本脚本只负责读文件 + 跑 wasm + 对拍。
//
// 用法（在 tools/wasm-smoke 下，或者直接跑 run-all.ps1）：
//   cargo build --release --lib --target wasm32-unknown-unknown
//   cargo build --release --bin tm_wasm_smoke
//   ./target/release/tm_wasm_smoke.exe > native.txt
//   node run-compare.mjs native.txt
//
// 退出码 0 = 逐位相同；1 = 有差异。

import { readFile } from "node:fs/promises";

const WASM = "./target/wasm32-unknown-unknown/release/tm_wasm_smoke.wasm";
const nativePath = process.argv[2] ?? "native.txt";

function parse(text, label) {
  // PowerShell 的 Out-File -Encoding utf8 会带 BOM，先剥掉再解析。
  text = text.replace(/^\uFEFF/, "");
  const m = /count=(\d+)\s*\n(.+?)\n/.exec(text);
  if (!m) throw new Error(label + " 输出格式不符：\n" + text.slice(0, 300));
  const n = Number(m[1]);
  const bits = m[2].trim().split(/\s+/).map((h) => Number.parseInt(h, 16));
  return { n, bits };
}

// ---- 1. native 侧（读文件，不 spawn）----
const native = parse(await readFile(nativePath, "utf8"), "native");

// ---- 2. wasm32 侧（在 Node 的 V8 里真跑）----
const bytes = await readFile(WASM);
const { instance } = await WebAssembly.instantiate(bytes, {});
const ex = instance.exports;
const n = ex.fill_scores();
const addr = ex.out_buf_addr();
const mem = new Uint32Array(ex.memory.buffer, addr, n);
const wasmBits = [];
for (let i = 0; i < n; i++) wasmBits.push(mem[i] >>> 0);

// ---- 3. 对拍 ----
const hex = (x) => "0x" + x.toString(16).padStart(8, "0");
console.log("native_count=" + native.n + "  wasm_count=" + n);
console.log("native[0..3] = " + native.bits.slice(0, 4).map(hex).join(" "));
console.log("wasm  [0..3] = " + wasmBits.slice(0, 4).map(hex).join(" "));

if (native.n !== n || native.bits.length !== wasmBits.length) {
  console.error("*** 长度不一致：native=" + native.n + " wasm=" + n);
  process.exit(1);
}

let diffs = 0;
for (let i = 0; i < n; i++) {
  if (native.bits[i] !== wasmBits[i]) {
    diffs++;
    if (diffs <= 5) {
      console.error("DIFF[" + i + "] native=" + hex(native.bits[i]) + " wasm=" + hex(wasmBits[i]));
    }
  }
}

if (diffs === 0) {
  console.log("*** IDENTICAL: " + n + " 个 f32 位模式，native x86_64 与 wasm32 逐位相同 ***");
  process.exit(0);
} else {
  console.error("*** " + diffs + " / " + n + " 个位模式不同 ***");
  process.exit(1);
}
