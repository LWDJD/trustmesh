# A7 · 代码体量、冗余与「轻量平替」清单

> 对象：`D:\Project\deepseek_harness\去中心化算法探索-A\wt\a7_debt`（分支 `audit/a7-debt`，基线 `471ad0f`，本次提交 `fb1fc71`）
> 性质：**只测量 + 出清单，未删除、未修改任何一行 Rust 代码**；`EngineConfig` 默认值逐位未动。
> 全部数字由脚本产生（`审计/a7_tools/*.py`），命令见第 10 节，可逐条重跑。

---

## 0. 口径与三处勘误

**口径**

* 「生产代码」= `src/` 或 `src/bin/` 中、`#[cfg(test)]` 之前的非空非注释行；「测试代码」= `tests/` 目录全部非空非注释行 + src 内 `#[cfg(test)]` 之后的非空非注释行。
* 「行数」= 物理行（含文档注释与空行），与「代码行」分开列。
* 「引用次数」= 全仓库 `.rs` 文件里该标识符的整词出现次数（排除定义行、排除注释行）；同名标识符（`new`/`len`/`iter`/`get`/`set` 等 73 个）**无法按名去歧义，全部排除，不参与任何结论**。
* 本任务无随机性，不涉及 seed/样本量；「引用次数」是确定性静态计数。

**勘误（任务书与实际仓库不符处，以实测为准）**

| 任务书写法 | 实测 |
|---|---|
| crates/exp 有 13 个 bin | **17 个** |
| 根目录 v1.0 有 11 个 bin | **10 个**（`src/bin/*.rs` 与根 Cargo.toml 的 10 个 `[[bin]]` 一致） |
| 根目录 15 个 result/log 文本 | **14 个**；另有 5 个在 `crates/exp/` 下，合计 19 个 |

---

## 1. 体量（实测，非估算）

### 1.1 crate 总表

| crate | 文件 | 总行 | **生产代码** | 测试代码 | 测试区行 | `#[test]` | pub 项 |
|---|---:|---:|---:|---:|---:|---:|---:|
| store | 3 | 3,175 | **337** | 2,083 | 2,216 | 123 | 42 |
| view | 6 | 1,427 | **371** | 736 | 776 | 48 | 34 |
| agg | 3 | 1,586 | **138** | 1,176 | 1,267 | 60 | 7 |
| engine | 5 | 3,210 | **404** | 2,304 | 2,342 | 72 | 37 |
| sim | 20 | 6,249 | **2,276** | 2,895 | 3,041 | 129 | 130 |
| exp | 19 | 10,192 | **8,799** | 218 | 218 | 8 | 11 |
| **crates 小计** | **56** | **25,839** | **12,325** | **9,412** | **9,860** | **440** | **261** |
| v1（根 src/+tests/） | 30 | 6,838 | **4,371** | 982 | 1,043 | 57 | 158 |
| **全仓合计** | **86** | **32,677** | **16,696** | **10,394** | **10,903** | **497** | **419** |

**测试执行结果（本次实测，全绿）**：`cargo test` 六个 crate 全部 `exit=0`，合计 **455 通过 / 0 失败 / 0 ignored**（440 个 `#[test]` + 15 个 doctest）；根目录 v1 另 **58 通过**（57 + 1 doctest）。总计 **513 通过，0 失败**。

### 1.2 两个极端的测试密度

| 区域 | 生产代码 | 测试代码 | 测试/生产 |
|---|---:|---:|---:|
| 核心库 store+view+agg+engine | **1,250 行** | 6,299 行 | **5.04 : 1** |
| sim | 2,276 行 | 2,895 行 | 1.27 : 1 |
| exp（产出全部实验数字的地方） | **8,799 行** | 218 行 | **0.025 : 1** |

**结论 1**：真正承载「反女巫机制」的核心库只有 **1,250 行生产代码**；产出 S1–S3/S5 全部数字的 exp 有 **8,799 行**、8 个测试（S4 无 bin 覆盖，S6 由单测覆盖）。**数字的生产地几乎没有测试，被测得最狠的模块几乎不产出数字。**

### 1.3 日志/文档占比

* 生产区**注释行**极多：store 的 959 行生产区里只有 337 行代码，其余 622 行是文档与空行（注释:代码 ≈ 1.5:1）。
* 根目录 `.md` 17 个 / 537,845 B；全仓 `.md` 20 个 / 576,317 B。

### 1.4 逻辑模块（单文件 crate，行数=含文档与空行的跨度）

| 模块 | 跨度行 | 其中代码行（实测） |
|---|---:|---:|
| store::AccountTable（struct+impl） | 89 | — |
| store::PositiveList（struct+impl） | 280 | — |
| store::RoundSampler（struct+impl） | 278 | 90（impl） |
| store 退避工具（`pow2_neg`/`backoff_weight`/`next_unit`/`pick_weighted`） | 97 | 49 |
| store::Rng | 83 | 33 |
| view::CompressedView（struct+impl） | 264 | — |
| view::ViewParams/NoiseStats | 91 | — |
| view::hash（FNV/splitmix/derive_seed） | 53 | 24 |
| agg::decay | 56 | — |
| agg::aggregate + 4 口径实现 | 197 | 108 |
| agg::combine | 27 | — |
| engine::EngineConfig | 86 | — |
| engine::impl Engine | 648 | — |
| ↳ 其中两级衰减/覆盖率/live 6 方法 | 232 | **89** |
| engine 其余方法 | 416 | — |

---

## 2. 公开 API 面

| crate | pub 项 | 名字可去歧义 | 零引用 | 仅测试引用 | 无跨 crate 消费者 |
|---|---:|---:|---:|---:|---:|
| store | 42 | 23 | 0 | 4 | 6 |
| view | 34 | 22 | 0 | 2 | 15 |
| agg | 7 | 6 | 0 | 0 | 2 |
| engine | 37 | 26 | 1 | 4 | 12 |
| sim | 130 | 101 | 2 | 9 | 61 |
| exp | 11 | 10 | 0 | 0 | 8 |
| **合计** | **261** | **188** | **3** | **19** | **104** |

* **104 / 188（55%）的 pub 项没有任何跨 crate 消费者**——它们 `pub` 只是因为躺在 lib 边界上。
* **73 个 pub 项因同名（`new`×18、`len`×6、`query`×5…）无法按名统计，已排除**，这是本节的已知盲区。

**零引用（3 项，全仓除定义外 0 次）**

| crate | 项 | 位置 |
|---|---|---|
| engine | `fn account_count` | `crates/engine/src/lib.rs:237` |
| sim | `fn Interest::uniform` | `crates/sim/src/interest.rs:23` |
| sim | `fn has_ratings` | `crates/sim/src/metrics.rs:503` |

**仅测试引用（19 项，生产代码 0 次）**

| crate | 项 | 位置 | 测试内引用 |
|---|---|---|---|
| store | `has` | store/src/lib.rs:332 | 19 |
| store | `touch` | store/src/lib.rs:339 | 10 |
| store | `indices` | store/src/lib.rs:478 | 10 |
| store | `draw` | store/src/lib.rs:576 | 69 |
| view | `encode` | view/src/lib.rs:335 | 13 |
| view | `decode` | view/src/lib.rs:349 | 9 |
| engine | `positive_count` | engine/src/lib.rs:269 | 4 |
| engine | `restart_round` | engine/src/lib.rs:327 | 2 |
| engine | `round_remaining` | engine/src/lib.rs:345 | 3 |
| engine | `prune_stale` | engine/src/lib.rs:502 | 14 |
| sim | `has_target`/`is_active`/`cluster_count`/`cluster_of` | attack.rs:43/48/53/58 | 1/1/2/6 |
| sim | `member_count` | community.rs:58 | 2 |
| sim | `class`/`all_finite` | metrics.rs:488/508 | 17/8 |
| sim | `shuffle` | rng.rs:217 | 1 |
| sim | `last_active_at` | world.rs:226 | 13 |

> 注意 `store::draw`（轮次抽样的**均匀不放回**原语，设计文档 3 节的主体）在生产代码里**一次都没被调用**：engine 走的是 `draw_backoff`/`draw_all`。测试里有 69 处引用，掩盖了这一点。
> `view::encode/decode`（序列化）没有任何 bin 使用——线上格式尚未落地。

完整 419 行表：[`审计/A7-pub-api.csv`](A7-pub-api.csv)（列：crate/kind/name/file/line/prod_refs/test_refs/cross_crate_refs/ambiguous）。

---

## 3. 死代码

### 3.1 cargo dead_code 警告：**8 条（6 处），全部在 exp 的 bin 里**

| 文件:行 | 内容 |
|---|---|
| `crates/exp/src/bin/latent.rs:19` | field `ret_ac` is never read |
| `crates/exp/src/bin/grid.rs:59` | fields `onl`/`alive` never read |
| `crates/exp/src/bin/l5_health.rs:368,369` | `n_att_all`/`n_hon_all` 赋值后未读 |
| `crates/exp/src/bin/l5_health.rs:182` | multiple fields are never read |
| `crates/exp/src/bin/d1_decay.rs:277` | field `p_abs` is never read |

**store / view / agg / engine / sim 五个 lib 目标：0 条 dead_code 警告。** 原因不是"没有死代码"，而是死代码全是 `pub`——rustc 不对 `pub` 项报 dead_code。**这正是上一节必须用 grep 而非编译器来找死 API 的原因。**

v1 根 crate 另有 2 条：`unused import: crate::sketch::Sketch`、`unused variable: prev`。

### 3.2 从未被任何 bin 赋过非默认值的配置字段：**engine 5 个（12/12 个 bin 全默认）**

| 字段 | 12 个 bin 实际传的值 | 默认值 |
|---|---|---|
| `group_half_life` | 0 | 0 |
| `coverage_exponent` | 1.0 | 1.0 |
| `unreachable_after` | 0 | 0 |
| `remove_when_coverage` | 0.0 | 0.0 |
| `min_group_sources` | 0 | 0 |

即：**「两级衰减 + 覆盖率 + live 集合」（设计规格 4.9，commit `3e7479d`/`12df94b`）在全部 17 个 bin 里都处于关闭状态，从未在任何一个实验里启用过。** 它只被 `crates/engine/tests/two_level_decay.rs`（1,217 行 / 32 个测试）覆盖。

### 3.3 设计上已删除、代码仍在：**banlist，5 个字段 + 64 行分支 + 15 行 bin 字面量**

commit `7376990`「移除 banlist：归应用层负责」只改了规格，没改代码：

* `PropConfig` 的 14 个字段中有 5 个是 banlist：`banlist`/`ban_penalty`/`ban_floor`/`ban_exclude`/`ban_lambda`（`crates/exp/src/lib.rs:51-60`）。
* `crates/exp/src/lib.rs` 里三段 banlist 逻辑共 **64 行跨度 / 52 行代码**：`:157-158`（n×n 的 `mul` 权重矩阵）、`:197-213`（`ban_exclude` 硬剔除）、`:230-251`（`ban_lambda` 指数降权）、`:253-266`（旧覆盖率软降权）。
* 5 个 bin×3 行字面量 = **15 行**恒为 `false/0.5/0.2/0.0/0.0`。

### 3.4 只服务测试的 API（与 2 节重复，此处按「能否删」归并）

store `draw/indices/has/touch`、view `encode/decode`、engine `prune_stale/positive_count/restart_round/round_remaining`、engine 两级衰减的 4 个只读接口 `group_scale/coverage/live_count/avg_source_age`（跨 crate 引用 0）。

---

## 4. 重复实现

### 4.1 四份 PRNG（+v1 第五份）逐行 diff

| 实现 | 位置 | 跨度行 | 代码行 | 算法 | 输出 | 种子=0 | 取 `[0,n)` |
|---|---|---:|---:|---|---|---|---|
| `store::Rng` | store/src/lib.rs:872-954 | 83 | 33 | xorshift64 (13,7,17) | `x` | →`0x9E37_79B9_7F4A_7C15` | **拒绝采样**（`wrapping_neg()%n` 区间） |
| `view::XorShift64` | view/src/rng.rs:1-57 | 57 | 35 | xorshift64 (13,7,17) | `x` | →`0x9e37_79b9_7f4a_7c15` | 无（提供 `next_key`/`state`） |
| `exp::R`（私有） | exp/src/lib.rs:86-100 | 15 | 14 | xorshift64 (13,7,17) | `x` | **无保护** | **取模（有偏）** |
| `sim::Rng` | sim/src/rng.rs:1-226 | 226 | 164 | xorshift64* (12,25,27)×`0x2545F4914F6CDD1D` | `x*const` | →`0x9E37_79B9_7F4A_7C15` | 拒绝采样 |
| `v1::Rng` | src/rng.rs（v1.0） | 45 | 31 | xorshift64* (12,25,27) | `x*const` | →`0x9E3779B97F4A7C15` | 取模 |

**差异摘要**

1. `store` 与 `view` 的**输出序列逐位相同**（同三元组、同 fallback 常量、输出都用 `x` 本身），只是类型名、模块位置、辅助方法（`next_below` vs `next_key`）不同——**纯重复**。
2. `exp::R` 是同算法第三份，但**没有 0 种子保护**，且 `below` 用取模（有偏）。当前唯一调用点传的是非零常量 `0x1234_5678_9ABC_DEF1`，所以 bug 未触发——属于「靠调用点运气正确」。
3. `sim` 与 `v1` 是另一族（xorshift64*，输出乘常数），逐行等价：v1 的 45 行是 sim 226 行的子集（sim 多 9 个分布采样器：`normal/lognormal/exponential/pareto/poisson/weighted_index/weighted_without_replacement/shuffle/range_inclusive`）。
4. **不存在「谁是谁的升级版」**：两族的输出序列完全不同，把 store 的 Rng 换成 sim 的会改变所有既有实验的随机流。

**能否合并：能，且收益明确。** `store`/`view`/`exp` 三份只需一份（`next_u64` + 拒绝采样 `next_below` + `next_key`），约 **70 行**替换现在的 154 行 → 省 **约 84 行**，并消灭「exp 用取模、store 用拒绝采样」的语义分裂（S6 要求同输入同输出，三份不同的取整方式是该判据的隐患）。`sim` 那份建议保留（分布采样是它独有的），但核心 25 行可以指向同一实现。
**风险**：合并会改变 `view`/`exp` 的随机流 → `noise_floor` 的 key 与 5 个小 bin 的抽样顺序会变，历史数字不可复现。因此建议**只在新增代码里统一，存量按 crate 各自冻结**。

### 4.2 crates/exp 的 17 个 bin

| bin | 行数 | 最后 commit（date） | 产物文件 | 被 .md 引用 | 判定 |
|---|---:|---|---|---|---|
| d1_decay | 1,368 | 71769b6（11-10） | exp/d1_result.txt (111KB) | 5 处 / 实验-D1 | 历史遗留（衰减模式矩阵） |
| d2_response | 1,335 | 71769b6 | d2_result.txt (297KB) | 7 处 / 计划 + 实验-D2 | 历史遗留 |
| d3_offline | 944 | 71769b6 | d3_result.txt (26KB) | 10 处 / 计划 + 实验-D3 | 历史遗留 |
| d4_health | 1,345 | 71769b6 | d4_result*.txt ×3 (46KB) | 8 处 / 计划+D4+交接 | 历史遗留 |
| l1_churn | 553 | 71769b6 | 无 | 10 处 / 实验-L1 | 历史遗留 |
| l2 | 840 | 71769b6 | l2_result.txt (43KB) | 26 处 / 5 篇 | 历史遗留（且被 d1/d3 当模板抄） |
| l3_sybil | 556 | 71769b6 | exp/l3_run3.txt、l3_diag.txt | 4 处 / 实验-L3 | **反女巫相关，保留候选** |
| l4_olduser | 1,005 | 71769b6 | l4_result/l4_result2/l4_probe | 9 处 / 4 篇 | 历史遗留 |
| l5_health | 972 | 71769b6 | d4_result_l5check.txt | 8 处 / 2 篇 | 历史遗留 |
| grid | 171 | 71769b6 | grid_result.txt | 7 处 / 3 篇 | 模板 + 潜伏网格，保留 |
| latent | 155 | 71769b6 | 无 | 3 处 | 保留（潜伏=女巫前奏） |
| multinode | 94 | 71769b6 | 无 | 3 处 | 保留（归一化） |
| e2e | 46 | 810b697 | exp/e2e_result.txt | 1 处 / 实验报告 | 保留 |
| factorial | 88 | 08c47c0 | 无 | **0 处** | 无主 |
| dist | 67 | 810b697 | 无 | **0 处** | 无主 |
| splitnorm | 66 | 08c47c0 | 无 | **0 处** | 无主（S5 素材） |
| sybilcap | 63 | 810b697 | 无 | **0 处** | 无主（S2/S3 素材） |

* **「最后一次被用到」的判定依据**：git 历史只有 2026-11-08~11-10 三天，最后 commit 无法区分「在用/弃用」；因此改用**产物文件是否还在 + 是否被实验报告引用**两个证据。上表两个证据都缺的只有 `factorial/dist/splitnorm/sybilcap` 四个。
* **两组互不相干的实验栈**：10 个「真多 runtime」重型 bin（9,089 行）**完全不使用** `tm_exp` 的 lib API（`use tm_exp` 命中 0 次）；只有 5 个小 bin（dist/e2e/factorial/splitnorm/sybilcap，合计 **329 行**）用 `tm_exp::{run_case, PropConfig}`（`crates/exp/src/lib.rs` 306 行 + `tests/runtime.rs` 218 行）。**524 行框架服务 329 行调用方，而 9,089 行主体一行都不用。**
* **S1/S3 的真身是 `multinode.rs`（94 行），不是那 9,089 行。** 交接文档 5.1/5.3 的「女巫 0.000 / 渗透 2.29 / 背书 1.97」全部由 `multinode.rs` 一个 bin 产出（它按 6 种 `AttackKind` 各跑一次并打印 `攻击/诚实` 比值）。本次实测复现（release，seed=7，n=200，steps=100，4 轮交换，耗时 29.3s）：

  | 场景 | 诚实均值 | 攻击均值 | 比值 |
  |---|---:|---:|---:|
  | 基线 | 0.004951 | 0.000000 | 0.000 |
  | 女巫 ×3 | 0.004902 | 0.000000 | **0.000**（S1 成立） |
  | 潜伏 50h | 0.004561 | 0.004062 | 0.891 |
  | 渗透 | 0.004323 | 0.009900 | **2.290**（S3 已知失败） |
  | 背书 20% | 0.004130 | 0.008121 | **1.966**（S3 已知失败） |
  | 垃圾 | 0.004918 | 0.000154 | 0.031 |

  即：**删掉其中 8 个重型 bin（8,362 行）不会丢失任何一条 S 判据的复现路径。**

* **重复度（实测）**：17 个 bin 共 8,567 行代码，其中 **2,036 行（23.8%）在别的 bin 里逐字重复**。同名函数的行级多重集重叠：`run_cell` 3 份 1,076 行（重叠 19.7%~41.3%）、`run_case` 4 份 1,047 行（重叠 10.1%~24.7%）、`calibrate` 9 份 257 行（重叠 11.4%~53.7%）。**是同名不同体的分叉副本，不是可机械抽取的公共库**——合并需要人工对账，不能 `git mv`。

### 4.3 v1.0（根 src/ + tests/）与 crates/ 的算法差异

| 维度 | v1.0 | crates/（现实现） |
|---|---|---|
| β | 0.3（`Preset::recommended`） | 0.3（`EngineConfig::default`） |
| 半衰期 H | 72h | 72h |
| **衰减形式** | **线性** `1 − Δt/(2H)`，2H 处归零（`src/rating_list.rs:121`） | **指数** `0.5^(Δt/H)`，用 `DECAY_FLOOR` 兜底永不归零（`agg::decay`） |
| 衰减基准 | 条目自己的 `sampled_at` | `last_check`（上次成功取到观点） |
| **抽样** | **按权重有放回**，K=50，权重=评分×衰减（`rating_list.rs:140`） | **均匀不放回**，轮次位图，K=8 |
| **抽样是否看评分** | **看**（评分越高越容易被抽到） | **不看**（只跟是否在清单里有关） |
| 归一化 | `normalize=true`（影响力预算） | 份额=评分/Σ评分 + 可选 view 归一化 |
| Sketch | 2048 桶 × 5 哈希 | 2048 × 5（`PARAMS_BUCKETS/HASHERS`） |
| 聚合 | 单一均值 | 4 种口径，实际只用 `Mean` |
| 额外机制 | 负分（`neg_attack`）、背书（`vouch`）、scope、identity、`PrecisionPolicy` 4 实现、DP `epsilon`（未实现） | 无负分、无背书、无 scope |
| 规模 | 6,838 行 / 57 测试 / 10 bin | 25,839 行 / 440 测试 / 17 bin |

**整体删除的影响面：零代码影响。** `grep -r "trustmesh" crates/ --include=*.rs` 只命中 **4 处文档注释**，没有任何 `use`/`extern crate`。`.md` 里也没有 v1 API 的引用（7 处 `trustmesh` 全是 `tm_engine`/`tm_sim` 的说明）。删除需要连带删掉根 `Cargo.toml`（59 行、10 个 `[[bin]]`），否则 `cargo test`（根）会因找不到 `src/lib.rs` 而失败。git 历史仍可回溯 `471ad0f`。**58 个测试会随之消失**，但它们测的是被推翻的算法（线性衰减 + 加权抽样），没有回归价值。

---

## 5. 仓库垃圾

| 项目 | 体积 | 位置 | 说明 |
|---|---:|---|---|
| 根目录 result/log ×14 | **596,403 B（582 KB）** | 工作树根 | d2/d3/d4/l2/l4/grid 等实验原始输出 |
| crates/exp 的 log/txt ×5 | **156,749 B（153 KB）** | `crates/exp/` | d1_result.txt 111KB、l3_run3.txt 19KB 等 |
| `repo-backup.bundle` | **8,972,940 B（8.56 MiB）** | **仅主 checkout**（不在工作树） | git 历史即备份，冗余 |
| `%SystemDrive%` 异常目录 | **0.9 MB / 4 文件** | **仅主 checkout** | ACL 修复脚本把 `%SystemDrive%` 当字面路径，生成了 `ProgramData\Microsoft\Windows\Caches\*.db` 的镜像树 |
| `target/`（本次实测） | **873 MB**（store 59.7 / view 33.4 / agg 28.1 / engine 84.5 / sim 128.6 / exp 538.7） | 各 crate 下 | 6 个独立 workspace 各一套；exp 一个占 539MB（17 个 bin × debug） |
| `target/`（主 checkout 存量） | **212.4 MB / 1,150 文件** | 主 checkout | 与上面不是同一份 |
| `.tmp/` | 0 B | 工作树 + 主 checkout | 空目录，仅 `TMP` 沙箱用 |

**可回收（不含 target）：约 10.6 MB**（596KB + 153KB + 8.56MiB + 0.9MB）。**含 target：约 883 MB**（工作树 873MB + 主 checkout 212MB，两者可各自 `cargo clean`）。

---

## 6. crate 拆分是否过度

**实测的依赖 DAG（`use tm_*` 实际命中 + Cargo.toml）**

```
store  view  agg  sim          <- 四个零依赖叶子（sim 谁都不依赖）
  \     |    /     /
   \    |   /     /
      engine        (依赖 store+view+agg，不用 sim)
         \         /
          \       /
            exp     (依赖全部 5 个；但 10 个重型 bin 只用 engine，5 个小 bin 只用 sim+view+agg)
```

**真实边界评估**

| 候选边界 | 是否成立 | 证据 |
|---|---|---|
| store 独立 | **成立** | 零依赖；engine 只用到 `AccountTable/PositiveList/RoundSampler/Rng` 4 个类型（含在 42 个 pub 里） |
| view 独立 | **成立但代价错位** | 零依赖，却重复定义了 `AccountKey` 与 `Score`（store 里也有）；只为「编译期保证 view 不依赖 store」 |
| agg 独立 | **成立** | 138 行生产代码、3 个纯函数、零依赖，是最干净的一个 |
| engine 独立 | **成立** | 唯一的「组装层」，把 3 个叶子拼起来 |
| sim 独立 | **成立且必要** | 谁都不依赖它，它也不依赖任何人；`tm_exp` 是唯一消费者 |
| exp 独立 | **不成立（对 bin 而言）** | `lib.rs` 只有 5 个小 bin 用；10 个重型 bin 是 9,089 行的独立程序，塞在同一个 crate 里共享一个 `target/`（539MB） |

**合并成 1~2 个 crate 能省多少样板（实测计数）**

| 样板 | 单份 | 6 份合计 | 合并为 1 个后 |
|---|---:|---:|---:|
| `Cargo.toml` | 9~21 行 | **94 行** | 省 77 行 |
| `Cargo.lock` | 7~39 行 | 6 个文件 | 省 5 个 |
| `.gitignore` | 1~2 行 | 6 个文件 | 省 5 个 |
| `[workspace]` 声明 | 2 行 | 4 份（agg/sim 漏了） | 归零 |
| `[profile.release]` | 4 行 | 4 份（view/agg 漏了） | 归零 |
| 独立 `target/` | — | **873 MB** | 约 540MB（单 workspace 一份） |

**合并会破坏什么（逐条实测）**

* **编译并行**：**不成立**。本次从零构建 6 个 crate（`--all-targets`）总耗时 **14 秒**（15:11:50→15:12:04），编译不是瓶颈。
* **测试隔离**：**不成立**。6 个 crate 各自 `cargo test` 仍然要跑全部 440 个测试，合并后一样。
* **编译期可见性边界**：**成立**，这是唯一真实收益。合并后 `pub` 会退化成 `pub(crate)` 或干脆私有，编译器不再帮你守住 store⊄view 这类约束。
* **下游用户**：**不成立**。6 个 crate 里 4 个 `publish = false` 或没有发布配置，只有根 `trustmesh` 面向外部（而它正是要删的 v1）。
* **增量编译粒度**：**成立但可忽略**。改 store 内部会连带重编 view/agg/engine/exp；实测增量 1~3 秒/crate。

**判断**：**6 个是过度拆分，但不是最严重的问题。** 12,325 行生产代码分 6 个 crate（平均 2,054 行/个），其中 agg（138 行）和 view（371 行）单独成 crate 的收益只有「强制不依赖」。建议 **6 → 3**：

* `tm_core` = store + view + agg + engine（1,250 行生产代码，3 层内部模块）。消掉 `Score`×3、`AccountKey`×2、`Rng`×2 的重复定义，消掉 3 份 manifest/lock/workspace 样板。
* `tm_sim` 保持独立（它是一套与信任模型无关的社会仿真，谁都不依赖它）。
* `tm_exp` 保留，但**把 10 个重型 bin 与 5 个小 bin 拆成两个 crate 或两个 `[[bin]]` 分组**——它们共享的只有 `target/` 目录。

---

## 7. 更轻量的平替（每模块一条）

| 模块 | 现状 | 平替方案 | 可省（**估算**） | 功能等价？ |
|---|---|---|---:|---|
| store::AccountTable | 89 行显式结构 | `BTreeMap<[u8;32],u32>` + `Vec<Option<[u8;32]>>` 直接内联进 engine | ~50 行 | 是（intern/lookup 语义不变） |
| store::RoundSampler | 278 行跨度 / 90 行代码（轮次位图 + 退避） | 「每轮把候选 `shuffle` 一次，`pop` 取 k 个」约 60 行 | ~180 行 | **仅当放弃退避**；退避版需保留权重抽样 |
| store 退避工具链 | 97 行跨度 / 49 行代码 + 641 行测试 | 直接删（engine.tick 不再按 `1/2^n` 降频） | 49 行 + 641 行测试 | 否（失去带宽优化），但**反女巫 0 贡献** |
| store::Rng | 83 行 / 33 行代码 | 与 view 共用一份，见 4.1 | ~84 行（三份合一） | 逐位相同 |
| view::CompressedView | 264 行跨度 | **保留**（Count Sketch 是 40KB 定长 + S5 的载体） | 0 | — |
| view `encode/decode` | 2 个 pub fn，仅测试引用 | 删，序列化归应用层 | ~40 行 + 100 行测试 | 是（无生产调用） |
| view `NoiseStats` + `noise_report` example | 91 行 + 80 行 example + 158 行测试 | 删（只有 example 与测试用） | ~80 行 + 158 行测试 | 是（噪声底是测量工具，不是机制） |
| agg 4 种口径 | 197 行跨度 / 108 行代码 | **只留 `Mean`**，删 `Median/WeightedMedian/Trimmed` + `MAX_TRIM` | ~59 行代码 + ~700 行测试 | 是（17 个 bin + engine 全部只用 `Mean`） |
| agg `decay`+`combine` | 83 行 | 保留（β 合成是核心公式） | 0 | — |
| engine 两级衰减/覆盖率/live | 6 方法 232 行跨度 / 89 行代码 + 86 行配置 + 1,217 行测试 | 删，或移出默认构建（feature gate） | 89 行 + 1,217 行测试 | 是（12/12 个 bin 全默认，等于从未启用） |
| engine 其余（tick/probe/query/export） | 416 行跨度 | 保留 | 0 | — |
| sim 全套 | 2,276 行 | 参数化人口生成器（~600 行） | ~1,600 行 | **否**——会改变输入分布，全部历史结论失效 |
| exp 的 8 个重型 bin | 8,362 行 | 1 个参数化 bin（`--mode` 选 衰减/规模/攻击面），约 1,200 行 | **~7,162 行** | 部分（S3 由 94 行的 `multinode` 独立覆盖） |
| exp banlist 残留 | 52 行代码 + 15 行 bin 字面量 | 删 | ~67 行 | 是（已从设计移除） |
| exp lib（5 小 bin 的框架） | 306 行 + 218 行测试 | 合并进唯一重型 bin 的框架 | ~400 行 | 是 |

---

## 8. 可回收总表

| # | 项目 | 体积 / 行数（实测） | 风险 | 建议 |
|---|---|---:|---|---|
| 1 | 根目录 v1.0（`src/`+`tests/`+10 bin+根 Cargo.toml） | **6,838 行**（prod 4,371 / 测试 982），57 测试，10 bin | **低**：crates/ 零代码引用、文档零引用、git 可回溯 | **删** |
| 2 | exp 的 8 个历史 bin（d1_decay / d2_response / d3_offline / d4_health / l1_churn / l2 / l4_olduser / l5_health） | **8,362 行** | **低**：S1/S3 的复现器是 94 行的 `multinode.rs`（已实测复现 2.290/1.966），这 9 个 bin 不拥有任何一条 S 判据 | **删**；若要留「衰减/流失」主题，合并成 1 个参数化 bin（~1,200 行） |
| 3 | exp banlist 残留（5 字段 + 3 段分支 + bin 字面量） | 52 行代码 + 15 行 = **67 行** | **低**：12/12 bin 传 `false/0`，设计已移除 | **删** |
| 4 | engine 两级衰减/覆盖率/live 集合 | 89 行代码 + 86 行配置 + **1,217 行测试** | **中低**：规格 4.9 刚写完且测试充分，但 17 个 bin 从未启用 | **留**（feature gate），暂不删 |
| 5 | agg 的 Median/WeightedMedian/Trimmed | 59 行代码 + ~700 行测试 | **低**：engine 与 17 个 bin 只用 `Mean` | **删**（或 feature gate） |
| 6 | 四份 PRNG | 154 行 → 70 行，省 **~84 行** | **中**：合并会改变随机流，历史实验不可复现 | **改**：新代码统一，存量冻结 |
| 7 | store 退避工具链 | 49 行代码 + **641 行测试** | **中**：engine.tick 依赖 `draw_backoff` | **留**，仅删 `draw` 等 4 个 test-only API |
| 8 | view `encode/decode` + `NoiseStats` | ~120 行 + ~258 行测试 | **低**：无生产调用 | **删** |
| 9 | 根目录 result/log ×14 | **582 KB** | 零（数据已在 17 篇 .md 里） | **删** |
| 10 | crates/exp 的 log/txt ×5 | **153 KB** | 零 | **删** |
| 11 | `repo-backup.bundle` | **8.56 MiB** | 零（git 历史即备份） | **删** |
| 12 | `%SystemDrive%` 异常目录 | **0.9 MB / 4 文件** | 零 | **删** |
| 13 | 6 个 `target/` | **873 MB**（+主 checkout 212MB） | 零（gitignore） | `cargo clean` |
| 14 | 三份 README（view/agg/sim） | 38,472 B / 504 行 | 低 | 合并为根 README |
| 15 | crate 拆分 6 → 3 | manifest/lock 样板 **~160 行**，target 省 ~330MB | 中：失去编译期可见性边界 | **建议改**（可延后） |
| 16 | exp 重框架（lib 306 + tests 218，服务 329 行调用方） | **524 行** | 中：5 个小 bin 依赖它 | 与 #2 一起改 |

**汇总（逐项相加，可复核）**

| 口径 | 行数 |
|---|---:|
| v1.0 全部 | 6,838 |
| 8 个重型 bin → 1 个 1,200 行参数化 bin（8,362 − 1,200） | 7,162 |
| exp banlist 残留（64 跨度 + 15 bin 字面量） | 79 |
| agg 三种非 Mean 口径（108 行跨度实测 + ~700 行测试估算） | ~808 |
| view `encode/decode` + `NoiseStats` + example（~120 + ~258 + 80） | ~458 |
| 四份 PRNG 合一（154 → 70） | ~84 |
| exp lib + tests/runtime（服务 329 行调用方） | 524 |
| **合计** | **~15,953 行（占 32,677 行的 49%）** |

其中生产代码约 **11,200 行**、测试代码约 **1,900 行**。磁盘可回收 **约 10.6 MB 垃圾 + 1,085 MB 构建产物**。
**口径说明**：行数是「被删目标文件的实测行数之和」，不是重构后的净节省——合并 bin 需要新写约 1,200 行框架，已从 8,362 中扣除。

---

## 9. 缺陷清单

| 编号 | 严重度 | 缺陷 | 最小复现 | 影响哪条 S |
|---|---|---|---|---|
| D1 | **高** | `crates/exp/src/lib.rs` 是**无衰减**的传播实现（文件头自认：`:7-11`），却驱动 e2e/sybilcap/factorial/splitnorm/dist 5 个 bin；这些 bin 的数字与真 runtime 不可比 | 在 `crates/exp` 跑 `python -c` 或直接 `grep -n "decay" src/lib.rs` → 只命中 2 行**说明自己没有 decay** 的注释；跑 `cargo run --release --bin e2e` 与 `--bin multinode` 对比同一网络 | S1/S2/S3/S5 的结论有效性 |
| D2 | **高** | `exp/src/lib.rs:197-213` 的 `ban_exclude` 分支 **zip 错位**（`:199-201` 的 FIXME 自认）：`exclude_zero` 的 `continue` 会让 `items` 比 `picked` 短，权重配到错误来源 | `sed -n '197,213p' crates/exp/src/lib.rs`；把任一 bin 的 `ban_exclude` 从 0.0 改成 0.5 后跑 → 结果无意义 | S5（若将来接线） |
| D3 | **高** | engine 的 5 个新配置字段在 **12/12 个使用 EngineConfig 的 bin 中恒为默认值**（0/1.0/0/0.0/0；另 5 个小 bin 根本不用 engine），两级衰减从未在实验里启用 | `grep -n "group_half_life:" crates/exp/src/bin/*.rs` → 12 处全是 `0`；其余 4 个字段同理 | S1–S6 全部（机制未被实验覆盖） |
| D4 | **中** | `exp/src/lib.rs:13-15` 自认：新账户编号用 `alive_count()`，有账户离开时会**与已有账户重号** | `sed -n '13,15p' crates/exp/src/lib.rs`；构造有离场的 `SimConfig` 跑 sybilcap | S1/S2 的样本编号正确性 |
| D5 | **中** | 四份 PRNG 语义不一致：`exp::R::below` 取模（有偏）、无 0 种子保护；store/sim 用拒绝采样 | `sed -n '97,99p' crates/exp/src/lib.rs` | S6（确定性） |
| D6 | **中** | agg 的 3 种聚合口径（Median/WeightedMedian/Trimmed）**无任何生产使用者**——engine 与 17 个 bin 全用 `Mean`。抗操纵杠杆（中位数免疫单个撒谎者，见 `sybil_report.rs`）从未进 runtime | `grep -rn "AggMode::" crates/engine/src crates/exp/src/bin` → 全是 `Mean` | S5（抗单点操纵本可由此获得） |
| D7 | **中** | 两套并行实验栈：10 个重型 bin（9,089 行）不用 `tm_exp`；5 个小 bin（329 行）用 `tm_exp`（524 行框架）。同一份报告里的数字来自不同传播实现 | `grep -rn "use tm_exp" crates/exp/src/bin` → 只有 5 个文件 | S1–S5 的横向可比性 |
| D8 | **中** | **S4（空账户）无任何 bin 覆盖**，唯一覆盖是 `crates/exp/tests/runtime.rs:123` 的 1 个单测 | `grep -rn "unrated\|未评分" crates/exp` → 只命中这一个测试 | S4（从未测） |
| D9 | 低 | 6 处 `dead_code`（`latent.ret_ac`、`grid.onl/alive`、`l5.n_att_all/n_hon_all`、`d1.p_abs`）+ v1 的 2 处 unused | 第 10 节命令 #2：`cargo build --all-targets --message-format=short \| Select-String warning` | 无（纯维护成本） |
| D10 | 低 | 4 个 bin（`factorial/dist/splitnorm/sybilcap`，284 行）既无产物文件也无文档引用，属于「写完即弃」 | `grep -c "factorial\|splitnorm\|sybilcap" *.md` → 0 | 无（但**它们正是 S2/S5 的素材**，需决定去留） |
| D11 | 低 | crate 样板不一致：`agg` 缺 `[workspace]` 与 `[profile.release]`；`sim` 缺 `[workspace]`；`view` 的 `.gitignore` 多一行且不提交 `Cargo.lock`（其余 5 个提交） | `Get-Content crates/agg/Cargo.toml`；`Get-Content crates/view/.gitignore` | 无 |
| D12 | 低 | `NoiseStats`（噪声底）只被 example 与测试使用，`encode/decode` 无生产调用：线上格式与噪声测量都还没接线 | `grep -rn "encode(\|noise_floor(" crates/*/src/bin crates/exp/src/bin` → 0 | 无 |

---

## 10. 复现命令

全部在 `D:\Project\deepseek_harness\去中心化算法探索-A\wt\a7_debt` 下执行；跑 cargo 前先设 `$env:TMP="$PWD\.tmp"; $env:TEMP="$PWD\.tmp"`。

```powershell
# 0) 行数 / 测试 / pub API / 重复度 / 死代码（本报告全部数字）
$py = "C:\Users\15081\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe"
$env:PYTHONIOENCODING="utf-8"
& $py "审计\a7_tools\count_debt2.py"     # 1.1 表：生产/测试代码行、#[test]
& $py "审计\a7_tools\api_scan.py"         # 2 节：419 个 pub 项的引用计数（写 api.json）
& $py "审计\a7_tools\api_unamb.py"        # 2 节：去歧义后的零引用/仅测试/无跨crate消费者
& $py "审计\a7_tools\dup_scan.py"         # 4.2：17 个 bin 的重复行系数 23.8%
& $py "审计\a7_tools\fn_dup.py"           # 4.2：run_cell/run_case/calibrate 分叉重叠度
& $py "审计\a7_tools\code_only.py"        # 3/7 节：各特性块的代码行数
& $py "审计\a7_tools\engine_methods.py"   # 1.4：impl Engine 逐方法行数

# 1) 测试基线（455 + 58，全绿）
foreach ($c in "store","view","agg","engine","sim","exp") {
  Push-Location "crates\$c"; $env:TMP="$PWD\..\..\.tmp"; $env:TEMP=$env:TMP; cargo test; Pop-Location
}
cargo test    # v1.0：58 通过

# 2) dead_code 警告（8 条，全在 exp 的 bin）
foreach ($c in "store","view","agg","engine","sim","exp") {
  Push-Location "crates\$c"; cargo build --all-targets --message-format=short 2>&1 | Select-String "warning"; Pop-Location
}

# 3) 垃圾体积
Get-ChildItem -File | Where-Object { $_.Name -match "\.(txt|log)$" } | Measure-Object Length -Sum   # 596,403 B / 14 个
Get-ChildItem "crates\exp" -File | Where-Object { $_.Name -match "\.(txt|log)$" } | Measure-Object Length -Sum  # 156,749 B / 5 个
Get-ChildItem crates\*\target -Recurse -File | Measure-Object Length -Sum   # 873 MB
Get-Item "..\..\repo-backup.bundle" | Select-Object Length                  # 8,972,940
Get-ChildItem "..\..\%SystemDrive%" -Recurse -File | Measure-Object Length -Sum  # ~0.9 MB / 4

# 4) 关键结论的定点核对
grep -n "group_half_life:" crates/exp/src/bin/*.rs      # 12 处全 0  -> D3
grep -rn "AggMode::" crates/engine/src crates/exp/src/bin | grep -v Mean   # 空 -> D6
grep -rn "use tm_exp" crates/exp/src/bin                 # 只有 5 个文件 -> D7
grep -n "decay" crates/exp/src/lib.rs                    # 2 行，都是"我没有 decay" -> D1
```

---

## 11. 一句话结论

**项目 32,677 行里有 9,412 行是测试代码、8,799 行是一套几乎没有测试的实验脚手架、6,838 行是被推翻的 v1.0；真正的反女巫机制只有 1,250 行生产代码，而 S1/S3 的权威复现器只有 94 行（`multinode.rs`）。**

收缩到「只做反女巫」最划算的三刀：

1. **删 v1.0** —— 6,838 行，风险零（crates/ 零代码引用）。
2. **删 8 个重型 bin** —— 8,362 行，风险低（不拥有任何 S 判据；S1/S3 已由 94 行的 `multinode` 实测复现：0.000 / 2.290 / 1.966）。
3. **删 banlist 残留 + 3 种无主聚合口径 + view 的两块无生产调用 API** —— 745 行，风险低。

engine 的两级衰减（89 行代码）与 store 的退避（49 行代码）虽「0 实验覆盖」，但代码量小、测试厚（1,217 / 641 行），建议 feature gate 而非删除。
