# wasm-smoke —— wasm32 逐位对拍

证明**整个引擎**在 wasm32 和 native 上给出逐位相同的结果。

不是测某个纯函数（`decay` 已经单独测过），而是跑完整流程：

~~~text
rate → tick → export_view → on_view → query
~~~ 

三轮传播后，把 6×6 = 36 个声望值写成 f32 位模式，两边逐位比较。

## 跑法

~~~powershell
$env:TMP="<仓库根>/.tmp"; $env:TEMP=$env:TMP
cd tools/wasm-smoke

# 1) 编 wasm
cargo build --release --lib --target wasm32-unknown-unknown
# 2) 编 native
cargo build --release --bin tm_wasm_smoke
# 3) 对拍（Node 里真跑 wasm）
node run-compare.mjs
~~~

退出码 0 = 逐位相同；1 = 有差异。

## 为什么这件事重要

如果同一份输入在两个平台上给出不同的声望值，那么：

* S6（「同输入必然同输出」）在跨平台意义上失效；
* 未来的 ZKP 验证（证明要跨机器验）会失败；
* 用户会看到「我这边和服务器算的不一样」这种没法排查的问题。

## 已知的坑

* wasm 侧**不能从 JS 直接传指针进来**（TypedArray 会被强转成 0，写进线性内存开头）。
  所以探针自己持有缓冲区并导出地址（`out_buf_addr()`），JS 从 `memory.buffer` 读回。
* 这个探针只验证了「同一台机器上的 native vs 同一台机器上的 wasm32」。
  跨 CPU 架构（aarch64 / riscv）还没有实测，见 `../../docs/audit/威胁模型.md` §5。
