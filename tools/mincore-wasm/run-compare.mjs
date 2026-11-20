// mincore wasm32 vs native 逐位对拍。
//
// **为什么 native 侧的结果从文件读，而不是在这里 spawn 出来跑**：
// 本机沙箱禁止程序通过管道打开子进程（Node 的 child_process 默认 stdio:'pipe' 会 EPERM），
// 这是环境边界不是 bug。所以由外层 shell 先跑 native 并落盘（native.txt），
// 本脚本只负责读文件 + 跑 wasm + 对拍。
//
// 用法（在 tools/mincore-wasm 下，或者直接跑 run-all.ps1）：
//   cargo build --release --lib --target wasm32-unknown-unknown
//   cargo build --release --bin mc_wasm_cli
//   ./target/release/mc_wasm_cli.exe > native.txt
//   node run-compare.mjs native.txt
//
// 退出码 0 = 逐位相同；1 = 有差异（会打印前几处差异）。

import { readFile } from "node:fs/promises";

const WASM = "./target/wasm32-unknown-unknown/release/tm_mincore_wasm.wasm";
const nativePath = process.argv[2] ?? "native.txt";

// 与 src/lib.rs 的常量一一对应：改一边必须改另一边。
const CAPS = [128, 256, 512];
const STAGES = 4;
const RECORDS_PER_CAP = 913;
const TOTAL_RECORDS = CAPS.length * RECORDS_PER_CAP; // 2739
const SNAP_COUNT = CAPS.length * STAGES; // 12
const SNAP_MAX = 512 * 6 + 2; // 3074

const hex = (x) => "0x" + (x >>> 0).toString(16).padStart(8, "0");

// 把落盘的 native 输出解码成文本。
///
// 三种情况都必须能读：
// * 无 BOM 的 UTF-8（run-all.ps1 用 .NET 写出）；
// * 带 BOM 的 UTF-8（某些编辑器 / Out-File -Encoding utf8）；
// * **UTF-16 LE 带 BOM**：PowerShell 5.1 的裸 `>` 重定向就是这种
//   （验收命令 `mc_wasm_cli.exe > native.txt` 正是这么写的），
//   如果只按 utf8 读会变成一屏 \u0000 分隔的乱码。CI 里是 bash 的 `>`，是 UTF-8。
function decodeText(buf) {
  if (buf.length >= 2 && buf[0] === 0xff && buf[1] === 0xfe) {
    // UTF-16 LE：从下标 2 开始按小端双字节解码。
    const body = buf.subarray(2, buf.length - (buf.length % 2 === 1 ? 1 : 0));
    const codes = [];
    for (let i = 0; i + 1 < body.length; i += 2) codes.push(body.readUInt16LE(i));
    // 一次解码成一个串，避免逐字符拼接。
    let out = "";
    const CHUNK = 4096;
    for (let i = 0; i < codes.length; i += CHUNK) {
      out += String.fromCharCode.apply(null, codes.slice(i, i + CHUNK));
    }
    return out;
  }
  if (buf.length >= 3 && buf[0] === 0xef && buf[1] === 0xbb && buf[2] === 0xbf) {
    return buf.subarray(3).toString("utf8");
  }
  return buf.toString("utf8");
}

// 解析 native 侧的输出（格式见 src/main.rs 的模块文档）。
function parseNative(text, label) {
  // PowerShell 的 Out-File -Encoding utf8 会带 BOM，先剥掉再解析。
  text = text.replace(/^\uFEFF/, "").replace(/\r\n/g, "\n");
  const lines = text.split("\n").filter((l) => l.length > 0);

  const cm = /^count=(\d+)$/.exec(lines[0] ?? "");
  if (!cm) throw new Error(label + " 第一行应为 count=N，实际：" + (lines[0] ?? "<空>"));
  const n = Number(cm[1]);
  const bitsLine = lines[1] ?? "";
  const bits = bitsLine.trim();
  if (bits === "") throw new Error(label + " 第二行（位模式）是空的");
  const bitArray = bits.split(/\s+/).map((h) => Number.parseInt(h, 16));
  if (bitArray.length !== n) {
    throw new Error(label + " 声明 count=" + n + "，实际位模式 " + bitArray.length + " 个");
  }

  const sm = /^snaps=(\d+)$/.exec(lines[2] ?? "");
  if (!sm) throw new Error(label + " 第三行应为 snaps=N，实际：" + (lines[2] ?? "<空>"));
  const snapCount = Number(sm[1]);
  const lens = (lines[3] ?? "").trim().split(/\s+/).map(Number);
  if (lens.length !== snapCount) {
    throw new Error(label + " 声明 snaps=" + snapCount + "，实际长度表 " + lens.length + " 项");
  }
  const snaps = [];
  for (let i = 0; i < snapCount; i++) {
    const line = (lines[4 + i] ?? "").trim();
    if (line.length !== lens[i] * 2) {
      throw new Error(
        label + " 第 " + i + " 张快照：期望 " + lens[i] + " 字节（" + lens[i] * 2 + " 个十六进制字符），实际 " + line.length + " 个字符"
      );
    }
    if (!/^[0-9a-f]+$/.test(line)) {
      throw new Error(label + " 第 " + i + " 张快照含有非小写十六进制字符");
    }
    const bytes = new Uint8Array(lens[i]);
    for (let j = 0; j < lens[i]; j++) bytes[j] = Number.parseInt(line.slice(j * 2, j * 2 + 2), 16);
    snaps.push({ len: lens[i], bytes });
  }
  if (lines.length !== 4 + snapCount) {
    throw new Error(label + " 行数不符：期望 " + (4 + snapCount) + "，实际 " + lines.length);
  }
  return { n, bits: bitArray, snaps };
}

// 把不一致的字节排成密集的十六进制，方便直接目视。
function hexBytes(bytes) {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

// ───────────────── 1. native 侧（读文件，不 spawn） ─────────────────
const native = parseNative(decodeText(await readFile(nativePath)), "native");

// 与源码常量对一遍：文件格式漂移必须在这里就炸，而不是悄悄比少了。
if (native.n !== TOTAL_RECORDS) {
  throw new Error("native 记录了 " + native.n + " 个位模式，源码布局表要求 " + TOTAL_RECORDS);
}
if (native.snaps.length !== SNAP_COUNT) {
  throw new Error("native 快照 " + native.snaps.length + " 张，源码布局表要求 " + SNAP_COUNT);
}
for (let i = 0; i < SNAP_COUNT; i++) {
  const want = CAPS[Math.floor(i / STAGES)] * 6 + 2;
  if (native.snaps[i].len !== want) {
    throw new Error("native 第 " + i + " 张快照长度 " + native.snaps[i].len + "，应为 " + want);
  }
  if (want > SNAP_MAX) throw new Error("快照超过 SNAP_MAX");
}

// ───────────────── 2. wasm32 侧（在 Node 的 V8 里真执行） ─────────────────
const wasmBytes = await readFile(WASM);
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
const ex = instance.exports;

const n = ex.fill_scores(); // 内部跑完整流程；任何断言失败都会 trap
const addr = ex.out_buf_addr();
const mem = new Uint32Array(ex.memory.buffer, addr, n);
const wasmBits = new Uint32Array(n);
for (let i = 0; i < n; i++) wasmBits[i] = mem[i] >>> 0; // wasm 侧是 u32，转成无符号

const snapAddr = ex.snap_buf_addr();
const stride = ex.snap_stride();
if (stride !== SNAP_MAX) {
  throw new Error("wasm 导出的 snap_stride=" + stride + "，与源码常量 SNAP_MAX=" + SNAP_MAX + " 不一致");
}
const wasmSnaps = [];
for (let slot = 0; slot < ex.snap_count(); slot++) {
  const len = ex.snap_len(slot);
  const off = snapAddr + slot * stride;
  const view = new Uint8Array(ex.memory.buffer, off, len);
  const copy = new Uint8Array(len);
  copy.set(view);
  wasmSnaps.push({ len, bytes: copy });
}

// ───────────────── 3. 对拍 ─────────────────
// 注意：TypedArray.prototype.map 会把回调返回值再强转回 uint32，
// 所以展示前必须先用 Array.from 变成普通数组，否则 "0x..." 会被截成 0。
const show = (arr, from, to) => Array.from(arr.slice(from, to)).map(hex).join(" ");

console.log("native_count=" + native.n + "  wasm_count=" + n);
console.log("native[0..6]    = " + show(native.bits, 0, 6) + " ... [892..900] = " + show(native.bits, 892, 900));
console.log("wasm  [0..6]    = " + show(wasmBits, 0, 6) + " ... [892..900] = " + show(wasmBits, 892, 900));
// decay 段（每个 cap 段的偏移 905..913）：这是唯一的 powf 调用，单独亮出来方便目视。
console.log("decay (cap=128): native = " + show(native.bits, 905, 913));
console.log("decay (cap=128): wasm   = " + show(wasmBits, 905, 913));
console.log("snaps: native=" + native.snaps.length + " wasm=" + wasmSnaps.length);

// 「非零记录数」用来排除「两边都是全零所以看起来相同」这种假阳性。
let nativeNonZero = 0;
for (const b of native.bits) if (b !== 0) nativeNonZero++;
let wasmNonZero = 0;
for (const b of wasmBits) if (b !== 0) wasmNonZero++;
console.log("非零记录数: native=" + nativeNonZero + " wasm=" + wasmNonZero + " / " + n);
if (nativeNonZero === 0 || wasmNonZero === 0) {
  console.error("*** 有一边全是 0，这多半是探针没跑起来，不是对拍成功 ***");
  process.exit(1);
}
// 快照也不能全是 0 字节（编码里必有条数与账户号）。
const nativeSnapNonZero = native.snaps.reduce((s, x) => s + x.bytes.reduce((t, b) => t + (b !== 0 ? 1 : 0), 0), 0);
const wasmSnapNonZero = wasmSnaps.reduce((s, x) => s + x.bytes.reduce((t, b) => t + (b !== 0 ? 1 : 0), 0), 0);
console.log("快照非零字节数: native=" + nativeSnapNonZero + " wasm=" + wasmSnapNonZero);
if (nativeSnapNonZero === 0 || wasmSnapNonZero === 0) {
  console.error("*** 有一边快照全零，对拍无意义 ***");
  process.exit(1);
}

if (native.n !== n || native.bits.length !== wasmBits.length) {
  console.error("*** 长度不一致：native=" + native.n + " wasm=" + n);
  process.exit(1);
}

let bitDiffs = 0;
for (let i = 0; i < n; i++) {
  if (native.bits[i] !== wasmBits[i]) {
    bitDiffs++;
    if (bitDiffs <= 8) {
      const cap = CAPS[Math.floor(i / RECORDS_PER_CAP)];
      const off = i % RECORDS_PER_CAP;
      console.error(
        "DIFF[bit " + i + " @cap=" + cap + " offset=" + off + "] native=" + hex(native.bits[i]) + " wasm=" + hex(wasmBits[i])
      );
    }
  }
}

let snapDiffs = 0;
if (native.snaps.length !== wasmSnaps.length) {
  console.error("*** 快照张数不一致：" + native.snaps.length + " vs " + wasmSnaps.length);
  snapDiffs++;
} else {
  for (let i = 0; i < native.snaps.length; i++) {
    const a = native.snaps[i];
    const b = wasmSnaps[i];
    const cap = CAPS[Math.floor(i / STAGES)];
    if (a.len !== b.len) {
      snapDiffs++;
      console.error("DIFF[snap " + i + " cap=" + cap + "] 长度 " + a.len + " vs " + b.len);
      continue;
    }
    let byteDiffs = 0;
    const firstDiff = [];
    for (let j = 0; j < a.len; j++) {
      if (a.bytes[j] !== b.bytes[j]) {
        byteDiffs++;
        if (firstDiff.length < 8) firstDiff.push(j);
      }
    }
    if (byteDiffs > 0) {
      snapDiffs++;
      console.error(
        "DIFF[snap " + i + " cap=" + cap + " stage=" + (i % STAGES) + "] " + byteDiffs + "/" + a.len +
        " 字节不同，首个差异下标 " + firstDiff.join(",")
      );
      console.error("  native 前 64 字节 = " + hexBytes(a.bytes.subarray(0, Math.min(a.len, 64))));
      console.error("  wasm   前 64 字节 = " + hexBytes(b.bytes.subarray(0, Math.min(b.len, 64))));
    }
  }
}

if (bitDiffs === 0 && snapDiffs === 0) {
  console.log(
    "*** IDENTICAL: " + n + " 个 f32 位模式 + " + native.snaps.length + " 张定长编码快照" +
    "（" + CAPS.map((c) => c + "×6+2=" + (c * 6 + 2)).join(" / ") + " 字节），" +
    "native x86_64 与 wasm32-unknown-unknown 逐位相同 ***"
  );
  process.exit(0);
} else {
  console.error("*** " + bitDiffs + " / " + n + " 个位模式不同，" + snapDiffs + " / " + native.snaps.length + " 张快照不同 ***");
  process.exit(1);
}
