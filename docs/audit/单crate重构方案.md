# 单 crate 重构方案（store/view/agg → engine）

> 2026-11-21 22:00 写方案，22:20 执行完毕（提交 `09cc80c`）。
> **六条守卫全部通过，962 → 963 passed / 0 failed / 6 ignored（+1 是新增的 crate 级示例）。**
> 目标：用户只需要依赖一个 crate，同时**不损失任何能力**。

---

## 执行结果（后记，2026-11-21 22:20）

| 守卫 | 判据 | 结果 |
|---|---|---|
| 1. 测试总数 | 454 条非 doctest 一条不丢 | ✅ 139+50+60+205 = 454 全部在 engine |
| 2. 公开 API | 52 个 pub 项仍可达 | ✅ `tests/public_api_snapshot.rs` 逐项 import |
| 3. wasm 对拍 | engine + mincore IDENTICAL | ✅ 两套都 IDENTICAL |
| 4. 快照格式 | MAGIC / VERSION=2 不变 | ✅ 未动 |
| 5. 确定性 | rng_consistency + S6 | ✅ 全绿，两套 RNG 仍是两套 |
| 6. 端到端 | multinode 输出逐位相同 | ✅ 6 个场景数值全同（仅 wall clock 15.3s→15.4s 不同） |
| 6b. 兼容层（额外验证） | 只写旧路径的 crate 能编译 | ✅ `use tm_store::AccountTable` / `tm_view::CompressedView` / `tm_agg::decay` 全部可用 |
| **7. 后续：删掉兼容层** | 作者确认「还没发布过」 | ✅ 2026-11-22：三个空壳 crate 已删除，35 个仓内文件的 import 改到 `tm_engine::X::Y` |

**实际花费**：约 55 分钟（估 4 小时）。

**遇到的 4 个真问题**（方案没预见到的）：
1. `crate::` 前缀重复替换 → `use crate::crate::badlist`（E0433「crate in paths can only be used in start position」）
2. `snapshot.rs` 访问 `Engine` 的 14 个私有字段 → 全部改 `pub(crate)`（原本同 crate 在 lib.rs，现在跨模块）
3. 22 个 **doctest** 用旧 crate 名 → 全改成 `tm_engine::store::` 等
4. 空壳的 doctest 里写 `~~~rust` 会被编译 → 改 `~~~text`

---

## 原始方案（执行前的版本）


---

## 0. 一句话

> **8 个 workspace 是 2026-11-09 那次「4 个并行审计任务」留下的形状**，不是运行时的必需结构。
> 合并的代价可以做到**几乎为零**：测试一条不丢、公开 API 一个不少、wasm 对拍仍逐位相同。

---

## 1. 现状：为什么会有多个

### 1.1 git 时间线

~~~text
2026-11-06  根 v1.0（src/，6,277 行）
2026-11-09  同一天出现 6 个 crate：
            c17db33  新增 tm_store 存储层
            bb4602e  合并 P1：存储层
            782fd8b  tm_view: Count Sketch
            2eef000  合并 P2：压缩观点
            45bb9c2  feat(agg): tm_agg 衰减与聚合
            a6f8039  合并 P3：衰减与聚合
            53da3bd  引擎：集成 store/view/agg，3 个冒烟测试通过
2026-11-18  tm_mincore 出现（对照实验）
~~~

**P1/P2/P3/P4 是审计任务编号**，不是设计分层。当时 4 个子智能体并行，各交一个独立 crate。
⟹ 「多个」是**并行分工的副产物**。

### 1.2 依赖图（干净的单向树）

~~~text
tm_store  → (无依赖)
tm_view   → (无依赖)
tm_agg    → (无依赖)
tm_engine → store, view, agg      ← 唯一汇聚点
tm_sim    → (无依赖)
tm_exp    → sim, view, agg, store, engine
tm_mincore→ sim, engine, view, store   ← 但只有 src/bin 用，生产代码纯 std
~~~

**验证过：store/view/agg 对 engine 的引用全部在注释里，无真实反向依赖 ⟹ 合并无环。**

### 1.3 各 crate 的真实角色

| crate | 生产行 | 谁依赖 | 角色 |
|---|---:|---|---|
| `tm_store` | 2,681 | engine, exp, mincore | **引擎零件**（存） |
| `tm_view` | 1,400 | engine, exp, mincore | **引擎零件**（压） |
| `tm_agg` | 1,208 | engine, exp | **引擎零件**（算） |
| **`tm_engine`** | **9,832** | exp, mincore | **★ 引擎本体 = runtime** |
| `tm_sim` | 5,126 | exp, mincore | 实验基础设施 |
| `tm_exp` | 23,507 | 无人 | 纯实验壳 |
| `tm_mincore` | 10,602 | 无人 | 纯对照实验 |
| 根 v1.0 | 5,810 | 无人 | 已废止的旧实现 |

用户真正需要的是 **4 个名字**：`Engine, EngineConfig, CompressedView, ViewParams`。
但为了拿到它们，必须先理解 4 个 crate 的层次。

## 2. 能力基线（合并前 → 合并后必须逐项相同）

这是「不破坏能力」的**验收标准**。已实测捕获（`.tmp/baseline_before_merge.txt`）：

| 能力 | 合并前实测 | 合并后必须 |
|---|---|---|
| `store` 测试 | **157** passed | = 157（合并后计入 engine） |
| `view` 测试 | **51** | = 51 |
| `agg` 测试 | **63** | = 63 |
| `engine` 测试 | **206**（1 ignored） | **477**（= 157+51+63+206，1 ignored 仍在） |
| `sim` 测试 | **130** | = 130（不动） |
| `exp` 测试 | **29** | = 29（不动） |
| `mincore` 测试 | **268**（5 ignored） | = 268（不动） |
| 根 v1.0 测试 | **58** | = 58（不动） |
| **全仓合计** | **962 passed / 0 failed / 6 ignored** | **必须仍是 962 / 0 / 6** |
| 文档测试 | store 18 / view 1 / agg 3 / engine 1 | 合计 23（合并后都在 engine） |
| clippy | 7 crate 全 0 warning | 4 crate 全 0 |
| fmt | 7 crate 全 0 diff | 4 crate 全 0 |
| **wasm 对拍** | engine 36 位模式 + mincore 2739 位模式 | **仍 IDENTICAL** |
| 快照格式 | `SNAPSHOT_MAGIC` / `VERSION=2` | **字节格式一个字节都不变** |
| 公开项 | store 9 + view 6 + agg 7 + engine 30 = **52** | **仍 52 个可用** |

**铁律：测试总数只能是 962，不能少一条。** 合并只改组织方式，不改行为。

---

## 3. 三个技术障碍（已逐个验证）

### 3.1 顶层 glob re-export 会撞名（E0252）

| 名字 | 定义在哪 | 类型 |
|---|---|---|
| `AccountKey` | `store::AccountKey` | `[u8; 32]` |
| | `view::AccountKey` | `[u8; 32]` |
| `Score` | `store::Score` | `f32` |
| | `view::Score` | `f32` |
| | `agg::Score` | `f32` |

⟹ `pub use store::*; pub use view::*; pub use agg::*;` 会 E0252 报错。
⟹ **顶层 re-export 必须人工挑选**，不能 glob。方案见 §4.2。

### 3.2 集成测试文件名冲突

`store/tests/backoff.rs` 与 `engine/tests/backoff.rs` 同名。合并后必须重命名。

### 3.3 68 个文件引用了叶子 crate 的路径

按 crate 分布：

| 被引用 | 文件数 |
|---|---:|
| `tm_view` | 65 |
| `tm_agg` | 43 |
| `tm_store` | 15 |
| `tm_engine` | 57 |

**engine 之外**需要改 import 的文件：**68 个**（exp 30 / mincore 6 / tools 1 / engine 31）。

⟹ 这是最大的工作量来源，**但可以用 shim crate 把它降到 0**（见 §4.3）。

## 4. 推荐方案：**合并进 engine，保留兼容 shim**

### 4.1 目标结构

~~~text
crates/engine/
  src/
    lib.rs          ← mod 声明 + 顶层 re-export（人工挑选，不 glob）
    runtime.rs      ← 原 lib.rs 的 Engine/EngineConfig（1250 行）
    store/
      mod.rs        ← 原 store/src/lib.rs 整份（2681 行生产代码）
    view/
      mod.rs        ← 原 view/src/lib.rs 整份
      hash.rs       ← 原样搬
      rng.rs        ← 原样搬（XorShift64，与 store::Rng 是两套，跨平台一致性测试锁着）
    agg/
      mod.rs        ← 原 agg/src/lib.rs 整份（纯函数）
    badlist.rs      ← 原样
    budget.rs       ← 原样
    criteria.rs     ← 原样
    snapshot.rs     ← 原样
  tests/
    （19 个集成测试，store_ 前缀重命名避免冲突）
~~~

**关键决定：`store/view/agg` 各自**原样整份**放进子目录的 `mod.rs`，不拆细。**

理由：拆细模块 = 大量行号变动 = 测试里的断言、文档里的 `file:line` 引用全要改 = 引入风险。
**先整份搬运（diff 只有 `mod` 包裹），再考虑要不要拆。**

### 4.2 顶层 re-export（人工挑选，解决 E0252）

~~~rust
// engine/src/lib.rs
pub mod store;
pub mod view;
pub mod agg;

// 人工挑选：冲突的名字只导出一个（用 engine 视角的名字）
pub use store::{AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler, Score, Time};
pub use view::{CompressedView, NoiseStats, ViewParams, XorShift64};
pub use agg::{aggregate, combine, decay, AggMode, MAX_TRIM};
pub use runtime::*;   // Engine, EngineConfig, FetchRequest, ...
~~~

冲突项的处理：

| 名字 | 处理 |
|---|---|
| `AccountKey` | 顶层只导 **store 的**（`[u8;32]`，索引键的语义）；view 的保留在 `view::AccountKey` |
| `Score` | 顶层只导 **store 的**（`f32`）；三者类型相同，语义兼容 |

⟹ 用户 `use tm_engine::Score` 拿到的是 f32，与三个 crate 里的完全相同。

---

## 5. 兼容 shim：把 68 个文件的改动降到 0

### 5.1 做法

`crates/store/src/lib.rs` 变成**空壳**：

~~~rust
//! 兼容层：已合并进 tm_engine。保留此 crate 仅为不破坏既有 import 路径。
#![forbid(unsafe_code)]
#![deprecated(note = "已合并进 tm_engine::store，本 crate 只做转发")]

pub use tm_engine::store::*;
~~~

`view` / `agg` 同理。

### 5.2 必须单向，不能成环

~~~text
✅ 正确：  store(空壳) → engine → (无 store 依赖)
❌ 错误：  engine → store → engine     ← 循环
~~~

所以 engine 的 `Cargo.toml` 里**必须删掉** `tm_store/tm_view/tm_agg` 三个依赖。

### 5.3 代价与收益

| | 路线 1（shim） | 路线 2（直改 import） |
|---|---|---|
| 要改的文件 | **3**（三个空壳） | **68** |
| 旧 import 路径 | **全部可用** | 全部失效 |
| crate 数 | 8 → 5（空壳仍在） | 8 → 4 |
| 风险 | **低** | 中（68 处机械改动，易漏） |

⟹ **先上路线 1**，跑绿之后如果确实想砍掉空壳，再单独一轮直改。

这样把「重构」和「改 68 处 import」拆成两步，任何一步出错都能定位。

## 6. 分阶段执行计划（每阶段可独立回退）

### 阶段 0：基线固化（不改代码）

| 步骤 | 命令 | 通过标准 |
|---|---|---|
| 0.1 | 存基线 | `.tmp/baseline_before_merge.txt`（已做） |
| 0.2 | 存公开项清单 | `52 个 pub 项`的完整列表（已做） |
| 0.3 | 存 wasm 金标准 sha | `tools/*/native.txt` 的 sha256 |
| 0.4 | 打 tag | `git tag pre-merge-20261121` |

### 阶段 1：搬运文件（只动结构，不改一行逻辑）

| 步骤 | 动作 | 验证 |
|---|---|---|
| 1.1 | `crates/store/src/lib.rs` → `crates/engine/src/store/mod.rs` | 文件内容逐字节相同 |
| 1.2 | `crates/view/src/{lib,hash,rng}.rs` → `crates/engine/src/view/` | 同上 |
| 1.3 | `crates/agg/src/lib.rs` → `crates/engine/src/agg/mod.rs` | 同上 |
| 1.4 | `crates/engine/src/lib.rs` → `crates/engine/src/runtime.rs` | 同上 |
| 1.5 | 新建 `crates/engine/src/lib.rs`（mod 声明 + re-export） | 编译通过 |

**验证 1.5 的关键**：`cargo check` 必须报出**恰好**这些错误：`crate::store` 路径不存在（因为 engine 内部原来写 `tm_store::X`）。
这是预期的，下一步修。

### 阶段 2：改 engine 内部路径

| 步骤 | 动作 | 验证 |
|---|---|---|
| 2.1 | engine 里 `use tm_store::X` → `use crate::store::X` | 编译通过 |
| 2.2 | engine 里 `use tm_view::X` → `use crate::view::X` | 同上 |
| 2.3 | engine 里 `use tm_agg::X` → `use crate::agg::X` | 同上 |
| 2.4 | 子模块里 `tm_store::X` → `crate::store::X`（非 use 的路径引用） | 同上 |

改动点数量（已查）：`engine/src` 里 `tm_store` 4 处、`tm_view` 3 处、`tm_agg` 2 处 use；
加上 `tests/` 和 `examples/` 里的路径引用。**全部是机械替换。**

### 阶段 3：测试重组

| 步骤 | 动作 | 验证 |
|---|---|---|
| 3.1 | `store/tests/*.rs` → `engine/tests/store_*.rs`（加前缀） | 文件名生效 |
| 3.2 | `view/tests/*.rs` → `engine/tests/view_*.rs` | 同上 |
| 3.3 | `agg/tests/*.rs` → `engine/tests/agg_*.rs` | 同上 |
| 3.4 | engine 原有的 `backoff.rs` 保留原名（store 的改名 `store_backoff.rs`） | 无冲突 |
| 3.5 | 测试文件里的 `use tm_store::X` → `use tm_engine::store::X` | 编译通过 |

**验证 3.5 的关键指标**：`cargo test` 的 passed 数必须是 **634**（149 + 51 + 63 + 206 = 让实际跑一遍核对）。

### 阶段 4：空壳 crate

| 步骤 | 动作 | 验证 |
|---|---|---|
| 4.1 | 删 `crates/store/src/`（除 lib.rs），lib.rs 改成 3 行转发 | `cargo test` 在 store 目录仍通过（转发后跑 engine 的测试？不——空壳没有测试） |
| 4.2 | 同上 view / agg | |
| 4.3 | 删 engine 的 `tm_store/tm_view/tm_agg` 三个依赖 | 无环 |
| 4.4 | 删 `crates/store/tests/` 等（测试已搬走） | 无重复 |

**⚠️ 空壳 crate 没有测试了。** `verify-all.ps1` 的 `$crates` 列表要同步改：`store/view/agg` 从测试列表里去掉（否则会报 0 passed 或报错）。

### 阶段 5：全量验收（关键）

| 检查 | 通过标准 |
|---|---|
| `cargo test`（engine） | **477 passed / 0 failed / 1 ignored** |
| `cargo test --doc`（engine） | **23 passed** |
| 全仓合计 | **962 passed / 0 failed / 6 ignored**（与基线完全一致） |
| clippy | 4 crate 全 0 warning |
| fmt | 4 crate 全 0 diff |
| **wasm 对拍（engine）** | **IDENTICAL**（36 个 f32 位模式） |
| **wasm 对拍（mincore）** | **IDENTICAL**（2739 位模式 + 12 张快照） |
| 公开项 | **52 个仍可用**（逐个 `use` 验证） |
| 快照格式 | 字节格式不变（旧快照仍能 decode） |

### 阶段 6：文档同步

| 文件 | 改什么 |
|---|---|
| `README.md` | crate 表格 7 行 → 5 行（4 个真 crate + 3 个空壳标注） |
| `索引.md` | §4.1~4.4 四个小节合并成一节 |
| `交接文档.md` | 结构章节更新 |
| `verify-all.ps1` | `$crates` 列表 |
| `.github/workflows/ci.yml` | matrix crate 列表 ×2（`test` 和 `lint`） |

**⚠️ ci.yml 要网页上传**（workflow scope 问题已知）。

## 7. 「不破坏能力」的六条守卫

### 守卫 1：测试总数锁

合并前后都跑：

~~~powershell
cd crates/engine; cargo test 2>&1 | Select-String 'test result: ok\. (\d+) passed'
~~~

必须 **477**（157+51+63+206，精确相加；ignored 1 条仍在 engine）。
**判据：全仓合计 962，一条不能少。**

### 守卫 2：公开 API 快照

合并前后各生成一份「哪些 `pub` 项可用」的清单，逐项对比：

~~~powershell
# 用 cargo doc 或简单的 grep 生成清单
~~~

判据：**52 个 pub 项一个不少**（路径可以变，项必须在）。

### 守卫 3：wasm 逐位对拍

`tools/wasm-smoke` 不依赖 store/view/agg 的路径——它 `use tm_engine::{Engine, EngineConfig}` + `tm_view::ViewParams`。

合并后 `ViewParams` 从 `tm_view` 变成 `tm_engine::view::ViewParams`，**shim 会让旧路径仍可用** ⟹ `tools/wasm-smoke` **一行都不用改**。

判据：`run-all.ps1` 报 **IDENTICAL**（36 个 f32 位模式）。

### 守卫 4：快照字节格式不变

`SNAPSHOT_MAGIC` / `SNAPSHOT_VERSION=2` 一个字节都不能变。

**这是最容易破的一条**：如果有人「顺手」改了 snapshot.rs 的模块路径顺带改了字段，旧快照就解不出来。

判据：用**合并前**生成的快照字节，在**合并后**的代码里 decode，必须成功且结果逐位相同。

~~~rust
// 新增测试（合并后加，合并前也加，双向锁）
#[test]
fn merged_crate_reads_pre_merge_snapshot() {
    // 硬编码一段合并前生成的快照字节，decode 后查 query 结果
}
~~~

### 守卫 5：确定性不变（S6）

同一 seed + 同一输入 ⟹ 逐位相同。

`rng_consistency.rs`（4 个测试）已经锁住两套 RNG 的一致性。合并后**必须原样保留**。

判据：`mincore` 的 S6 测试（20/20 seed）+ engine 的确定性测试全绿。

### 守卫 6：行为等价（端到端）

用 `crates/exp` 的同一批 bin，在合并前后各跑一次，**输出逐字节比较**：

~~~text
合并前: cargo run --release --bin multinode > before.txt
合并后: cargo run --release --bin multinode > after.txt
diff before.txt after.txt   ⟹ 必须为空
~~~

这比单元测试更能证明「能力没变」。

**建议选这 3 个 bin**（覆盖核心路径 + 全部机制）：

| bin | 覆盖 |
|---|---|
| `multinode` | 多节点传播 + S1/S3 |
| `c1_badlist` | badlist 折扣 |
| `p_persist` | 快照往返 |

## 8. 风险清单与缓解

| # | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | 顶层 re-export 撞名导致 API 变化 | 中 | 用户 `use tm_engine::AccountKey` 拿到的是 store 版本 | 已在 §4.2 定方案；加测试逐个 `use` 验证 |
| R2 | 65 个文件里 `tm_view::ViewParams` 路径失效 | 中→**零**（用 shim） | 编译失败 | shim crate 转发 |
| R3 | 集成测试改名后漏掉某个文件 | 低 | 少跑测试 | 用**文件数 + 测试数双重**核对（19 个文件 / 477 条 + 23 doctest） |
| R4 | 快照格式被顺手改坏 | 低 | 旧快照解不出 | 守卫 4：合并前生成的字节必须能在合并后 decode |
| R5 | `verify-all.ps1` 的 `$crates` 列表没改 | 高 | 空壳 crate 报 0 passed → 误判 | 阶段 4 明确要求同步改 |
| R6 | ci.yml matrix 没改 | 高 | CI 红 | 阶段 6 明确要求；**网页上传** |
| R7 | `crates/exp` 的 26 个 bin 漏改某处 | 中 | 编译失败（但测试会发现） | 阶段 5 的「端到端输出逐字节比较」 |
| R8 | 两套 RNG 被合并成一套 | 低 | 抽样顺序改变 ⟹ 快照无法重放 | 守卫 5；交接文档 §7.3 已明确「保留两套」 |
| R9 | 行数统计 / 文档 `file:line` 引用全失效 | 高 | 文档索引指向错误代码 | 阶段 6；接受这个代价（索引本质会过期） |

**R8 要特别警惕**：`view::XorShift64` 与 `store::Rng` 是**两套独立实现**，
`engine/tests/rng_consistency.rs` 的 4 个测试锁着它们必须一致。
合并到同一个 crate 后，很容易「顺手合并成一个」——**绝对不要做**（会破坏确定性）。

---

## 9. 工作量估算

| 阶段 | 内容 | 估算 |
|---|---|---|
| 0 | 基线固化 | 10 分钟（已做一半） |
| 1 | 搬运文件（4 个 crate → engine 子模块） | 30 分钟（`git mv` + 确认逐字节相同） |
| 2 | 改 engine 内部路径 | 30 分钟（18 处 use + 若干路径引用） |
| 3 | 测试重组（19 个文件改名 + 改 import） | 40 分钟 |
| 4 | 空壳 crate + 删依赖 | 20 分钟 |
| 5 | 全量验收 + 守卫 1~6 | **1~2 小时**（含 wasm 对拍、端到端 diff） |
| 6 | 文档同步（README/索引/交接/verify-all/ci.yml） | 40 分钟 |
| | **合计** | **约 4 小时** |

---

## 10. 不做什么（明确边界）

| 不做 | 原因 |
|---|---|
| **不删任何测试** | 作者明确要求「测试和仿真的代码不要减」 |
| **不动 `sim` / `exp` / `mincore`** | 它们是实验基础设施 / 对照实验，不属于「用户要的 runtime」 |
| **不动根 v1.0** | 作者明确要求保留 |
| **不合并两套 RNG** | 会破坏抽样顺序与快照重放（见 R8） |
| **不改任何行为逻辑** | 本轮目标只是**改组织方式**，不是优化或重构逻辑 |
| **不拆细子模块** | `store/mod.rs` 就是 2681 行整份搬；拆细会引入行号变动风险 |
| **不删空壳 crate 的测试目录以外的东西** | 空壳后 `tests/` 已搬走，但 `examples/` 要跟着搬或改路径 |

---

## 11. 需要作者拍板的三件事

| # | 问题 | 我的建议 |
|---|---|---|
| **Q1** | 空壳 `crates/store`/`view`/`agg` 要不要留？ | **留**（标 deprecated），把 68 处 import 改动降到 0。将来想砍再单独一轮 |
| **Q2** | 合并后的 crate 叫什么？ | 保持 `tm_engine`（它的文档第一行已经写着「trustmesh 运行时」，名实相符） |
| **Q3** | `sim`/`exp`/`mincore` 三个怎么处置？ | **本节不动它们**。它们在合并后仍是独立 crate，只是不再是「引擎零件」而是「实验层」 |

如果 Q1 选「不留」，那就要接受 68 处 import 改动（工作量 +1.5 小时，风险从低升到中）。
