# P0-gap 审计与修复报告

- 分支：`fix/p0-gap`，基线 `75d8b5e`
- 日期：2026-11-13
- 范围：① `trust_distribution` 空清单语义；② `tm_view::median_sorted` 偶样本溢出；
  ③ `PositiveList::total` 的 f32 累加漂移；④ 正分份额下溢为 0；⑤ `tm_store` 评分上界注释。
- 硬约束：**未改 `crates/engine/src/lib.rs` 的 `query` / `on_view`**（diff 仅触及
  `trust_distribution` 的文档与对应测试）。除 ②③④ 明确要求的数值修复外，不改默认行为。
- 每个修复一个提交：

| 编号 | 提交 | 主题 |
| --- | --- | --- |
| ① | `6cc7a99` | 明确 `trust_distribution` 的分布未定义条件并加测试 |
| ② | `2a27a23` | 中位数偶数分支先折半再相加，避免大数值溢出 |
| ③ | `091cd55` | `PositiveList::total` 改用 f64 累加，份额和恢复到 1 附近 |
| ④ | `c1c046b` | 份额用 f64 计算并对下溢加正下限，正分不再等于零权重 |
| ⑤ | `742503c` | 评分上界注释改为与 `MAX_SCORE=1e9` 的实现一致 |

## 0. 结论速览

| 编号 | 改前实测 | 改后实测 | 行为是否变化 |
| --- | --- | --- | --- |
| ① | `len=0`，`Σ=-0.0`（bits `0x80000000`） | 同左（只把「未定义」写进文档并加测试） | 否 |
| ② | `noise_floor(999).median=2e38`；`noise_floor(1000).median=inf` | 两者均为 `2e38`，比值 1.0 | 是（要求修） |
| ③ | 1000×`MAX_SCORE`：`total=1.0000095e12`，份额和（f64 累加）= `0.999990501441` | `total=9.99999996e11`（f32 出口舍入，相对 −4.1e-9），份额和 = `1.000000047497` | 是（要求修） |
| ④ | `MIN_POSITIVE` 条目的 `share()` = `Some(0.0)`，`shares()` 中为 `0.0` | `1.1754944e-38`（=`f32::MIN_POSITIVE`），> 0 | 是（要求修） |
| ⑤ | 注释称「截断为 1.0，守住评分 ∈ (0,1]」，实现是截断到 `1e9` | 注释与实现一致；`set(1e10)` 断言为 `1e9` | 否（仅注释） |

---

## ① `trust_distribution` 在空引擎下总和为 0

### 现状与证据

两个账户已 intern、但一条正分都没有时：

```text
len=0 sum=-0.0 bits=0x80000000 shares_len=0
```

空 `Vec` 的 `Σ` 是 `-0.0`（Rust 当前 f32 `Sum` 对空迭代器的结果，bits `0x80000000`）；
符号无意义，语义是「零」。原文档却写「总和恒为 1」，与实测直接矛盾。

### 本次改动

- 保留返回值类型 `Vec<(AccountIdx, Score)>` 和现有行为（空清单 -> 空 `Vec`）。
- 在 [`crates/engine/src/lib.rs`](../crates/engine/src/lib.rs) 的 `trust_distribution`
  文档中把不变量写成**有条件**的：「除非我还没有给任何人打正分」；同时标注
  `β = 0` 且无观点缓存的退化情形，以及调用方必须把空结果当作「无分布」。
- 新增测试 `stage4_empty_positive_list_is_an_undefined_distribution`
  （`crates/engine/tests/normalization.rs`），钉住 `len=0`、`Σ=0`。
  既有 `stage4_empty_engine_is_safe` 保持不变。

### 两个可选修法（**不替作者选**）

本次提交只做文档化与测试，代码形态与 (a) 相同，但**不等于替作者拍板选 (a)**；
最终采用哪个方案仍由作者决定。

**(a) 返回空 + 文档写明例外（当前行为）**

- 改动：只需文档与测试，零 API 变化、零数值变化。
- 优点：语义诚实——「没有正分」确实没有归一化分母；不伪造偏好；历史实验口径不变。
- 代价：不变量从「恒为 1」降级为「有条件为 1」，所有调用方必须处理空结果；
  直接 `v * 100` 会得到空百分比；下游若默认「非空」会静默丢数据；
  只能靠文档约束，编译器帮不上忙。

**(b) 返回所有已知账户的均匀分布**

- 改动：遍历 `AccountTable`，给每个已知账户 `1/N`；账户表也为空时仍需特判
  （0 个账户没有 `1/0`），返回空。
- 优点：`Σ = 1` 无条件成立，调用方不必分支；百分比的语义简单。
- 代价：
  1. **伪造信任**：这些账户的 `query` 都是 0，均匀分布却宣称「我同等信任每个人」，
     与 `β·个人项 + (1−β)·群体项` 的模型不一致；
  2. **放大女巫可见度**：只要某个 key 被我 intern（收到观点/请求就可能发生），
     它无需我的任何正分就能分到 `1/N`；已知账户越多，诚实账户的份额反被稀释；
  3. 改变 `trust_distribution` 的对外语义，所有历史指标/实验对比需要重跑，
     断言「空引擎返回空」的既有测试需要改写；
  4. 「已知账户」的边界本身是任意的（为什么不是全网络 key？），会给未来留歧义。

---

## ② `tm_view::median_sorted` 偶样本溢出

### 改前 / 改后

复现视图：`ViewParams::new(1, 5, 0)`（每个查询都命中同一个桶），
写入一条 `2.0e38`。此时每次查询的幅度都是 `2e38`，
`2e38 + 2e38` 已超过 `f32::MAX ≈ 3.4028e38`。

| 采样数 | 改前 median | 改后 median |
| --- | --- | --- |
| 999（奇数） | `2e38` | `2e38` |
| 1000（偶数） | `inf` | `2e38` |

改后两者比值 = 1.0，均有限。`p95` / `max` 改前改后都是 `2e38`。

### 改动

`crates/view/src/lib.rs`：

```rust
// 旧：(sorted[len/2 - 1] + sorted[len/2]) * 0.5
sorted[len / 2 - 1] * 0.5 + sorted[len / 2] * 0.5
```

与 `tm_agg` 中位数分支已有的写法对齐，并加注释说明为什么要先折半。

### 测试

- 单元测试 `median_even_large_values_do_not_overflow`：直接断言
  `median_sorted(&[f32::MAX*0.75, f32::MAX*0.75])` 有限且等于原值，
  `median_sorted(&[f32::MAX, f32::MAX]) == f32::MAX`。
- 集成测试 `even_sample_median_does_not_overflow_large_view`
  （`crates/view/tests/noise.rs`）：用公开 API 构造上述视图，
  断言 `noise_floor(999)` / `noise_floor(1000)` 的 median 都有限、> 0、同量级。

---

## ③ `PositiveList::total` 的 f32 累加漂移

### 改前 / 改后：1000 条 `MAX_SCORE`

| 指标 | 改前 | 改后 |
| --- | --- | --- |
| `total()`（f32） | `1.0000095e12`（相对精确值 +9.5e-6） | `9.99999996e11`（相对 −4.1e-9，只剩出口舍入） |
| 份额和（把每条 f32 份额用 **f64** 累加） | `0.999990501441`（偏差 9.5e-6，即问题里的 79.7 ULP） | `1.000000047497`（偏差 4.75e-8） |
| 份额和（把每条 f32 份额用 **f32** 顺序累加） | `0.999987364` | `0.999990702` |
| 单条份额 | `0.0009999905` | `0.001000000047`（f32 能表示的 1/1000） |

说明：问题里给出的 `0.999990501` 正是「f32 份额再用 f64 累加」的值，本报告的验收
沿用同一口径。若再用 f32 顺序累加 1000 个 0.001，仍有约 9.3e-6 的**求和本身**的舍入
（不是分母口径问题）；在不给每条份额加约 1e-5 相对偏置的前提下，这个误差无法由
`total()` 消除。分母修正后，f32 累加误差也从 1.26e-5 降到 9.30e-6（约 −26%）。

### 随机评分对照（重尾 log-uniform，8 个 seed）

评分 = `10^(9u)`，`u` 为 `Rng` 产生的 [0,1) 均匀数，1000 条/seed：

| 指标 | 改前 | 改后 | 最大新/旧比 |
| --- | --- | --- | --- |
| `total` 相对误差 | 4.77e-7 ~ 9.64e-7 | 1.76e-11 ~ 4.61e-8 | ≤ 6% |
| 份额和绝对误差 | 4.75e-7 ~ 9.66e-7 | 7.2e-10 ~ 4.78e-8 | ≤ 6% |

满足「不超过原来的一半」：最坏的新/旧比约 6%，即至少降到原来的 1/17。

### 改动

`crates/store/src/lib.rs` 的 `total()`：中间用 `f64` 求和，出口转回 `f32`。
返回值类型不变。`share()` / `shares()` 的除法在 ④ 中一并改为 f64 中间量。

### 测试

- `shares_sum_to_one_with_1000_max_scores`：1000 条 `MAX_SCORE`，
  断言 `total` 相对误差 < 1e-6、份额和（f64 累加）偏差 < 1e-6、每条都 ≈ 0.001。
- `random_scores_improve_total_and_share_sum_by_at_least_2x`：8 个 seed 的重尾随机评分，
  逐 seed 断言新的 `total` 误差与份额和误差都 ≤ 旧口径（f32 连加）的一半。

---

## ④ 正分条目的份额下溢为 0

### 改前 / 改后

清单：`set(0, f32::MIN_POSITIVE)` + `set(1, MAX_SCORE)`。

| 量 | 数值 |
| --- | --- |
| 理论份额 `MIN_POSITIVE / MAX_SCORE` | `1.1754943508222876e-47` |
| f32 最小次正规数 `f32::from_bits(1)` | `1.401298464324817e-45` |
| 改前 `share(0)` / `shares()[0]` | `Some(0.0)` / `0.0` |
| 改后 `share(0)` / `shares()[0]` | `Some(1.1754944e-38)` / `1.1754944e-38` |
| 大账户份额（改后） | `1.0` |
| 份额和（改后） | `1.0`（下限小到不影响 f32 求和） |

### 改动

新增私有 `SHARE_FLOOR: Score = f32::MIN_POSITIVE`，以及
`normalized_share(score, total)`：

1. 先算 `f64::from(score) / f64::from(total)`，再转 `f32`；
2. 若转回后仍为 0 且 `score > 0`，返回 `SHARE_FLOOR`；
3. 正常份额（例如 1/N）不会被改动。

下限取最小正**正规数**而不是最小次正规数，理由写在常量注释里：次正规数在部分平台
可能被 flush-to-zero；而 `1.17e-38` 在任何实际分布里都可忽略，却足够保证权重大于 0。
`share()` 与 `shares()` 共用该函数，口径一致。

### 测试

`tiny_positive_score_never_underflows_to_zero_share`：先验证理论份额确实低于 f32
最小次正规数，再断言 `share()` 与 `shares()` 里该条目 > 0、下限为
`f32::MIN_POSITIVE`、大账户份额不被扰动、总和仍为 1。

---

## ⑤ 注释与实现矛盾

### 改前

`crates/store/src/lib.rs` 中：

- `PositiveList` 文档写「传入的分数若大于 MAX_SCORE（含 +∞）会被截断为 **1.0**，
  以此守住评分 ∈ (0, 1]」；
- `set()` 文档写「score > **1.0** 会被截断为 MAX_SCORE」；
- 模块文档与 `Score` 类型别名也写着「评分只有 (0, 1]」。

实现是 `MAX_SCORE = 1.0e9`：`set(1e10)` 存成 `1e9`，`set(2.5)` 原样保留 `2.5`；
1.0 只是普通数值，不是上界。

### 改后

四处同源表述统一改为：

- 评分只收正数，0 表示没表态；
- 存储层上界是 `MAX_SCORE = 1.0e9`（防求和溢出），**不是**语义上的 1.0；
- `set(1e10)` -> `1e9`，`set(2.5)` -> `2.5`。

行为零变化。测试方面，在既有的 `set_score_above_max_is_clamped` 中补了一条
`set(3, 1.0e10)` 断言（不新增测试函数）。

---

## 测试清单与总数

### 三个被改动的 crate（全部通过）

| crate | 测试构成 | 基线 | 改后 |
| --- | --- | --- | --- |
| `tm_store` | lib 72 + backoff 24 + store_integration 30 + doc 10 | 133 | **136** |
| `tm_view` | lib 16 + noise 10 + view 24 + doc 1 | 49 | **51** |
| `tm_engine` | lib 16 + backoff 3 + badlist 18 + decay_modes 6 + normalization 11 + staleness 16 + doc 0 | 69 | **70** |

**三个 crate 合计：251 -> 257（新增 6 条测试）**，全部 `ok`，0 failed。
另外对下游 `tm_exp` 做了回归：`cargo test` 通过（`tests/runtime.rs` 8 条，其余 bin 0 条）。

### 新增/加强的测试

| 编号 | 文件 | 测试 | 状态 |
| --- | --- | --- | --- |
| ① | `crates/engine/tests/normalization.rs` | `stage4_empty_positive_list_is_an_undefined_distribution` | 新增 |
| ② | `crates/view/src/lib.rs` | `median_even_large_values_do_not_overflow` | 新增 |
| ② | `crates/view/tests/noise.rs` | `even_sample_median_does_not_overflow_large_view` | 新增 |
| ③ | `crates/store/tests/store_integration.rs` | `shares_sum_to_one_with_1000_max_scores` | 新增 |
| ③ | `crates/store/tests/store_integration.rs` | `random_scores_improve_total_and_share_sum_by_at_least_2x` | 新增 |
| ④ | `crates/store/tests/store_integration.rs` | `tiny_positive_score_never_underflows_to_zero_share` | 新增 |
| ⑤ | `crates/store/src/lib.rs` | `set_score_above_max_is_clamped`（加 `1e10` 断言） | 加强 |

## 复现命令

每个 crate 是独立 workspace；`.tmp` 必须先存在（Windows 链接器会在这里落临时文件）：

```powershell
New-Item -ItemType Directory -Force .tmp | Out-Null
$env:TMP = "$PWD\.tmp"; $env:TEMP = $env:TMP

Push-Location crates/store;  cargo test; Pop-Location
Push-Location crates/view;   cargo test; Pop-Location
Push-Location crates/engine; cargo test; Pop-Location
Push-Location crates/exp;    cargo test; Pop-Location   # 下游回归
```

## 附注

- `query` / `on_view` 的实现零改动；`git diff` 中出现的 `query` 字样只在
  `trust_distribution` 的文档里。
- `cargo doc --no-deps` 在 `tm_store` 仍报告 2 条**既有** warning
  （`unresolved link to j`、`Vec<AccountKey>` 的 unclosed HTML tag），
  与本次改动无关，未在本分支处理。
