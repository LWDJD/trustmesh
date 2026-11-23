# mincore-wasm —— mincore 的 wasm32 逐位对拍

证明 **`tm_mincore` 最小反女巫内核**（`crates/mincore`）在
**wasm32-unknown-unknown**（在 Node 的 V8 里**真执行**，不是只编译）与
**native x86_64** 上，对同一批输入给出：

* **逐位相同**的 f32 位模式（**2739** 个）；
* **逐字节相同**的定长编码快照（12 张，`cap×6+2` = 770 / 1538 / 3074 字节）。

不是测某个纯函数，而是跑完整流程（`cap = 128 / 256 / 512` 各跑一遍，其余输入三者完全相同）：

~~~text
建 Node → rate 704 个账户（含 0 分 / 负分 / NaN / 超量 / 满分）
        → publish(0)                  // 阶段 0：还没有任何观点
        → on_view 6 份观点（含 0 分 / NaN / 负分 / 超量槽位）+ 4 次被拒
        → publish(36)                 // 阶段 1：衰减因子 0.5^(36/72)
        → 刷新 2 份观点 + 1 次取失败
        → publish(520)                // 阶段 2：衰减因子 0.5^(520/72) 等
        → publish(1_000_000)          // 阶段 3：所有来源的衰减都撞 f32 下限
每次 publish 都做 encode → decode → encode 的逐字节往返断言
最后 query 全量账户、normalized / dequantize 读数、quantize 边界、decay 位模式、状态量
~~~

每个 cap 记录 **913 个 f32 位模式**（三个 cap 共 **2739 个**），布局见 `src/lib.rs` 的模块文档。
所有记录值都被断言为**有限 f32**（NaN 的 payload 跨平台不保证一致，不参与对拍）。

## 跑法

**注意一个坑**：本任务验收用的那条 `.\target\release\mc_wasm_cli.exe > native.txt`
在 PowerShell 5.1 下会把 `native.txt` 写成 **UTF-16 LE + BOM**（而且会覆盖入库的金标准）。
对拍器照常能跑（`decodeText` 认这种编码），但跑完 `git status` 会显示它为已修改。
**恢复办法**（两种都行，效果相同）：

~~~powershell
git checkout -- native.txt      # 从 git 取回金标准
.\run-all.ps1                   # 或者用真实 native 重新生成（写的是无 BOM UTF-8，与入库版本逐字节相同）
~~~


~~~powershell
$env:TMP="<仓库根>/.tmp"; $env:TEMP=$env:TMP
cd tools/mincore-wasm

# 一条命令全跑（编译 + native 落盘 + 两遍确定性自检 + 负对照 + 对拍）
.\run-all.ps1

# 或者手动四条
cargo build --release --lib --target wasm32-unknown-unknown
cargo build --release --bin mc_wasm_cli
.\target\release\mc_wasm_cli.exe > native.txt
node run-compare.mjs native.txt
~~~

退出码 0 = 逐位相同；1 = 有差异（会打印前几处差异的 cap / 段内偏移 / 位模式 / 字节下标）。

`run-all.ps1` 一共 6 步，其中两步是**防止假阳**的自检：

* **4/6 确定性自检**：native 连跑两遍，两遍输出必须逐字节相同；
  否则「native vs wasm 相同」可能只是两边都在抖动。
* **5/6 负对照**：把 native.txt 里 1 个位模式改成 `0x10000000`、1 个快照字节改掉，
  对拍器**必须**报差异并退出 1。这一步每次跑都做——它证明对拍逻辑本身抓得住差异，
  而不是永远打印 IDENTICAL。

## `native.txt` 是**入库的金标准**

`native.txt` 已经提交进仓库（native x86_64 的基准输出，73 KB）。
这样 `verify-mincore.ps1` 的「wasm32 逐位对拍」那一步在**全新 checkout** 上也能直接跑：
它只编 wasm lib，然后拿这份金标准去对拍，不需要先把 native bin 编出来。

金标准的身份：**73273 字节，sha256 = `d93403fe42981ca11608b783011f8246dadbc347c50ad2bb32db2b8c1f9617e6`**
（在 Windows x86_64-msvc 上生成）。

金标准为什么不会过期：探针是确定性的（实测 native 连跑两遍输出逐字节相同），
所以 `run-all.ps1` 用真实 native 二进制重新生成出来的字节与入库版本完全一致，
跑完之后 `git status` 仍是干净的。**但如果 `crates/mincore` 的行为变了而没人重新生成
`native.txt`，那么金标准会过期**——下面这条单行检查就会响（本机实测 exit=0）：

~~~powershell
.\target\release\mc_wasm_cli.exe > regenerated.txt
git diff --no-index --exit-code native.txt regenerated.txt   # 0 = 金标准仍新鲜
~~~

## 为什么这件事重要

如果同一份输入在两个平台上给出不同的声望值，那么：

* S6（「同输入必然同输出」）在跨平台意义上失效；
* 未来的 ZKP 验证（证明要跨机器验）会失败；
* 用户会看到「我这边和服务器算的不一样」这种没法排查的问题。

另外，定长编码（`SparseView::encode`）是**规范形式**：同一份观点只有唯一字节串。
它在任何平台上都必须逐字节相同——这是「格式即防御」的地基，也是将来做签名 / 存档的前提。

## 探针特意覆盖的跨平台风险点

* **唯一的 libm 调用**：`decay` 里的 `0.5f64.powf(dt/half_life)`。定点、归一化、编码全是
  整数运算，天然一致；只有 powf 可能在 native（MSVC CRT）与 wasm32（V8 libm）之间差一个 ulp。
  所以 `decay` 的 8 个输出位模式被单独记录（段内偏移 905..913），
  覆盖 dt = 0 / 1 / 36 / 72 / 143 / 520 / 1e6 / u64::MAX。
* **f64 → f32 的每一次舍入**：`distribution` 全程 f64 累加、最后统一 `as f32`；
  `query` 里 `beta * personal + (1-beta) * group` 也是 f64 算完再 `as f32`。
  这些半途舍入点全部落在被记录的 `query` 值里。
* **整数编码**：`encode` 的字节序固定小端、槽位严格升序、填充区全零——
  12 张快照逐字节比较，任何一处平台差异（哪怕是 padding）都会被抓住。
* **入口夹紧**：NaN / 负分 / -0.0 / 超量 / ∞ / 天文值走 `quantize` 与 `PositiveList::set` 的
  钳位分支，结果也被记录。

## native 侧为什么由 shell 跑、只落盘成文件

本机沙箱禁止 Node 用管道打开子进程（`child_process` 默认 `stdio:'pipe'` → EPERM）。
这是环境边界不是 bug，所以 `run-compare.mjs` 只读文件 + 跑 wasm，不 spawn 任何东西。
照抄 `tools/wasm-smoke` 的解法。

## 已知的坑（都已在代码里绕开）

* **wasm 侧不能从 JS 传 TypedArray 指针进来**（会被强转成 0，写进线性内存开头）。
  所以探针自己持有缓冲区并导出地址：`out_buf_addr()` / `snap_buf_addr()`，
  JS 从 `instance.exports.memory.buffer` 读回。
* `static mut` 只取地址、**不创建引用**（`std::ptr::addr_of!` / `addr_of_mut!`），
  避免 `static_mut_refs` 警告。
* cdylib 的 **lib 名与 bin 名必须不同**（`tm_mincore_wasm` vs `mc_wasm_cli`），
  否则 cargo 报 `output filename collision`。
* `run-all.ps1` **必须是 UTF-8 带 BOM**，否则 PS 5.1 会按 GBK 解码中文导致语法错误。
  （本目录没有装 git pre-commit 钩子去守这件事——本机沙箱里 `git hook run` 起不了 `sh`，
  装上去只会变成一个没法验证的摆设。改这个文件时请手工确认前 3 字节是 `ef bb bf`；
  `run-all.ps1` 一旦被改成无 BOM，本机跑出来的报错是「语法错误」，指不到根因。）
* **`>` 重定向的编码是玄学**：CI 里是 bash 的 `>`（UTF-8），
  PowerShell 5.1 的裸 `>` 是 **UTF-16 LE + BOM**，`Out-File -Encoding utf8` 是 UTF-8 + BOM，
  `[System.IO.File]::WriteAllText` 可以不插 BOM。
  `run-compare.mjs` 的 `decodeText` 三种都认——所以验收命令里那条裸 `>` 也能直接跑通。
* `TypedArray.prototype.map` 会把回调返回值**强转回 uint32**（`"0x..."` 会变成 0），
  所以打印位模式前要先 `Array.from`。（这个坑真的踩到了，输出一开始全 `0`。）

## 本探针验证了 / 没验证什么

* 验证了：同一台机器上 native x86_64（MSVC, x86-64）vs wasm32（V8 的 wasm 后端）的
  **同一份源码路径**（`run_all()` 是 lib 里的同一个函数，native bin 只是打印它）。
* 没验证：跨 CPU 架构（aarch64 / riscv）、跨编译器（gcc/clang 的 native）、
  跨 wasm runtime（wasmer / wasmtime）。见 `../../docs/audit/威胁模型.md` §5。

## 与 `tools/wasm-smoke` 的关系

同一个模式（`README + Cargo.toml + src/lib.rs + src/main.rs + run-compare.mjs + run-all.ps1 + .gitignore`），
但探针覆盖的是 **mincore**（定长稀疏表 + 精确整数归一化）而不是 **engine**（Count Sketch）。
两者互补：engine 那一套证明「现行实现」跨平台一致，这一套证明「候选内核」跨平台一致。

`crates/mincore` 的测试（`cargo test`）与本探针**互不依赖**：前者只跑 native，
后者是唯一真正执行 wasm 的地方，跑不进 `cargo test`（需要 Node）。

## 接进 CI（建议，本目录未改动 `.github/`）

本目录之外的文件**不在本任务的写作用域**，所以 CI 接线留给集成者。加一段即可：

~~~yaml
  mincore-wasm-parity:
    name: mincore wasm32 bit-parity
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: 1.96.0
          targets: wasm32-unknown-unknown
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
      - name: build wasm32 (lib -> .wasm)
        working-directory: tools/mincore-wasm
        run: cargo build --release --lib --target wasm32-unknown-unknown
      - name: build native (bin)
        working-directory: tools/mincore-wasm
        run: cargo build --release --bin mc_wasm_cli
      - name: run native and dump
        working-directory: tools/mincore-wasm
        run: ./target/release/mc_wasm_cli > native.txt
      - name: compare wasm32 vs native
        working-directory: tools/mincore-wasm
        run: node run-compare.mjs native.txt
~~~

**CI 里请用「当场编出来的 native」对拍，不要拿入库的 `native.txt` 当基准**（原因见下）。
`run-all.ps1` 只在 Windows 上跑；CI 用上面这 4 条就够，负对照是本地开发时的额外保险。

## 金标准 `native.txt` 的适用范围（重要）

入库的 `native.txt` 是在 **Windows + x86_64-pc-windows-msvc** 上生成的。它服务于本机的
`verify-mincore.ps1`（那一步只编 wasm lib，没有可用的 native 二进制，所以需要一个现成基准）。

在**别的平台**（Linux glibc / macOS）上重新生成的 native 输出**可能**在
`decay` 的 8 条记录上差 1 个 ulp——因为 `decay` 里的 `0.5f64.powf` 会落到各平台自己的 libm
（MSVC CRT / glibc / Rust 的 compiler-builtins），这三者不保证逐位一致。
定点、整数归一化、编码这些**纯整数路径**在任何平台上都必然一致。

所以：

* 本机（Windows）的 `verify-mincore.ps1` 用入库金标准 → 实测 IDENTICAL，没问题；
* CI（Linux）请**当场编 native 再对拍**（上面的 YAML 就是这么写的）；
* 如果一定要在 Linux 上校验金标准，请只比较 `decay` 段以外的记录
  （即每个 cap 段偏移 `0..905`），或者给这一步加 `continue-on-error`。

（这也正是本探针的意义：它把「哪个平台/哪条路径可能漂移」精确到段，
而不是笼统地报「两个平台不一致」。本机 Windows 上这 8 条完全一致。）
