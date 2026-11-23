# P0 核心修复报告（三大缺陷 + 同源附带）

> 工作树：`wt/p1_core`　分支：`fix/p0-core`　基线：`75d8b5e`
> 真 runtime：`crates/engine/src/lib.rs`
> 提交：`22f99e9`（P0①）、`46b59df`（P0②）、`ca0322d`（P0③）、`6663102`（文档），本报告单独一个提交。
> 全部数字都是**本工作树自己跑出来的**（命令见 §8），没有抄审计报告；引用审计结论的地方
> 会明确标注「引用」。

---

## 0. 一句话结论

三个 P0 修的是同一段归一化逻辑的两个入口：**分母算谁**（①、附带）和**分子怎么夹**（②），
外加一个入口校验（③）。每一条都有最小复现、改前/改后数字，以及一条「合规输入逐位不变」的测试。
β、份额算法、压缩视图格式、`EngineConfig` 字段语义、badlist 公式全部未动。

---

## 1. P0① 群体项分母只算「已取到观点」的来源

### 1.1 改前

`query` 遍历的是 `self.views`（我手上有谁的观点），所以「我评过分、但还没取到观点」的
来源根本不进分母。覆盖率不足时，先到的那一份观点独占群体项：

```text
D[T] = β/N + (1−β)·(m/M)      M = 已缓存观点数
M = 1 时，T 的群体项 = 1，而 N 只出现在 β/N 里
```

### 1.2 改后

遍历 `self.ratings`（我评过谁）。没有观点的来源贡献一条 `(份额, 0.0)`：
**权重进分母、值为 0**（弃权票），与「低于噪声底 = 没表态」同口径。

权重公式仍然是 `w_v = 份额 × 衰减 × 折扣`；没有缓存观点时没有 `last_check`，衰减因子按 1。
badlist 折扣照乘（默认 1.0），所以「没人报告时」这一条与 `(份额, 0.0)` 完全一致。

### 1.3 最小复现与数字

场景：101 个已评分账户（T + 100 个来源，各 1.0），只有 1 份观点（来自 sources[0]，只抬举 T）。
对比对象：`sources[1]`——同样被我评过 1.0、但没人背书。

| 量 | 改前 | 改后 |
|---|---:|---:|
| `D[T]` | 0.7029716 | 0.032258052 |
| `D[honest]` | 0.0029703027 | 0.009677416 |
| **倍数** | **236.66664** | **3.3333333** |

改后正好是 `1/β = 3.3333`：群体项 = 1 份观点 ÷ 101 个已评分来源 = 1/101，
个人项也是 1/101，两者相加恰好 1/101。测试：
`crates/engine/tests/p0_core.rs::group_denominator_counts_rated_sources_without_views`（±2%）。

**合规输入没变**：所有已评分账户都有观点时，正分清单与观点缓存是同一个集合，分母完全重合，
结果**逐位相同**。测试里硬编码的位模式是改动前用同一场景打印的：
`crates/engine/tests/p0_core.rs::full_coverage_is_bit_identical`
（3 个来源、5 个目标，t=0 / t=37 / 分布全部 `f32::to_bits` 相等）。

---

## 2. P0② 归一化「先除后夹紧」

### 2.1 改前

```rust
let raw = stored.view.query(key) / stored.total_mass;   // 分子没夹
let v = raw.clamp(0.0, 1.0);
```

`total_mass` 是各已登记账户 `min(q,1)` 之和（受影响的账户数记为 K），于是声明值
`c >= K` 时 `q/T = c/K >= 1`：**一个来源的预算从 1 变成 K**。

### 2.2 改后

```rust
let raw = stored.view.query(key).clamp(0.0, 1.0) / stored.total_mass;
let v = raw.clamp(0.0, 1.0);
```

对合规视图（全部读数 ≤ 1）**逐位不变**：`q ≤ 1` 且 `T = Σ min(q,1) >= q` ⇒ `q/T ≤ 1`，
两种写法同值。改动落在 `reading_of`，`query` 与 badlist 共用这一份读数，两条路径不会再次漂移。

### 2.3 最小复现与数字

场景：1 个来源（份额 1），8 个已登记目标，观点声明 8 条各 1000。

| 量 | 改前 | 改后 |
|---|---:|---:|
| `Σv` | **8.0** | **1.0** |
| `max v` | 1.0（每个目标都拿满） | 0.125（= 1/8） |

阈值就是 K：声明 8（=K）、1000、1e6 的结果完全一致，都是饱和，所以测试把三种都跑了。
混合声明（1 条 1000 + 7 条 0.125）也守得住：`Σv = Σ min(c_i,1)/T = 1`。

测试：`p0_core.rs::oversized_claims_cannot_exceed_budget`、
`p0_core.rs::mixed_claims_still_sum_to_one`。

**合规输入没变**：8 条各 0.125 时，改前改后每个目标的查询值都是同一位模式
`1035154227`（0.7 × 0.125 = 0.0875），`Σv = 1`。测试：
`p0_core.rs::compliant_claims_are_bit_identical`。

> 引用：A8 用 400 账户 / 18 女巫（4.5%）测到「29.37% 的信任」，那是本缺陷的端到端规模；
> 本报告只复现了单来源最小例，没有重跑 20 seeds 的端到端实验。

---

## 3. P0③ `on_view` 不校验观点参数

### 3.1 改前

引擎配 2048×5，却接受任意 `buckets × hashers`。代价 = `(N + 1000 次噪声采样) × hashers`，
而 `hashers` 由对端声明；单视图体积上限 `MAX_CELLS = 2^26` 单元 = 256 MiB 也会原样进缓存。

### 3.2 改后

`on_view` 开头：

```rust
if view.params() != self.cfg.view_params {
    return;
}
```

整条丢弃：不计命中、不进缓存、不清退避、不更新时钟。参数一致时行为与改动前逐位相同。
（应用层若想把这种视图记成「取数失败」，自己调 `on_view_failed` —— 库不替它决定。）

### 3.3 最小复现与数字

场景 A（正确性）：引擎 512×5/seed 7，收到 16×4096/seed 7 的视图（256 KiB）。

| `stats()` = (请求, 命中, 缓存, 评分) | 改前 | 改后 |
|---|---|---|
| 收到参数不符的视图后 | (0, **1**, **1**, 1) | (0, **0**, **0**, 1) |

`view_age` 从 `Some(0)` 变回 `None`，查询值逐位不变。

场景 B（耗时，release，N=4000）：

| 视图 | 改前 | 改后 |
|---|---:|---:|
| 正常 2048×5（40 KiB） | 1.607 ms | 2.169 ms |
| 恶意 16×4096（256 KiB） | **835.213 ms**（×520） | **0.0498 ms**（直接返回） |

两次测量是不同进程/不同时刻，正常视图本身有 ±35% 抖动；恶意视图从 835 ms 掉到 0.05 ms，
量级差异不受抖动影响。

测试：`p0_core.rs::views_with_mismatched_params_are_dropped`（buckets / hashers / seed
三种不一致 + 恶意 16×4096）、`p0_core.rs::matching_params_are_accepted`（合规输入照常收下）。

---

## 4. 附带：`total_mass` 只是 `on_view` 时刻的快照

### 4.1 改前

`total_mass` 只统计 `on_view` 那一刻已登记的账户。之后才 `intern` 的账户没有进过分母，
查询它们时 `v = q / total_mass` 直接饱和到 1.0。

### 4.2 改后

`StoredView` 新增 `mass_covers: u32`（`on_view` 时的 `accounts.len()`）。`AccountIdx`
单调递增分配，所以 `target >= mass_covers` 精确等价于「这个账户是之后才登记的」；
这类账户一律按「这份观点没表态」处理（v = 0，份额仍进分母）。

### 4.3 数字

场景：1 个来源，观点声明 50 个当时**都还没登记**的账户（各 1.0），随后全部登记。

| 量 | 改前 | 改后 |
|---|---:|---:|
| 50 个后登记账户的 `Σv` | **50** | **0** |
| 第一个账户的 `v` | 1.0 | 0.0 |

合规输入没变：`on_view` 之前登记的账户照旧 `Σv = 1`，只有之后登记的那一半被丢弃。
测试：`p0_core.rs::late_interned_accounts_are_not_endorsed`、
`p0_core.rs::mass_snapshot_boundary_only_drops_later_accounts`。

---

## 5. 组合测试

`p0_core.rs::combined_attack_stays_within_budget`：一份超量视图（10 条各 1000）+ 只取到
1 份观点 + 21 个已评分来源 + 5 个目标在 `on_view` 前登记、5 个之后登记 + 一份参数不符的
恶意视图。

实测：早期目标各拿 `(1/5)/21 ≈ 0.00952`（群体项），后登记目标全部 0，总和
`1/21 ≈ 0.0476 ≤ 1`；每个查询值 ∈ [0,1]；`trust_distribution` 总和 = 1；
缓存里只有 1 份观点；被背书目标没有压过「我亲自评分、没人背书」的账户。

---

## 6. 既有测试影响清单

| 文件 | 结果 | 说明 |
|---|---|---|
| `src/lib.rs` smoke（16） | 全部通过，未改 | 单来源/全覆盖/无观点场景新旧口径同值 |
| `tests/backoff.rs`（3） | 全部通过，未改 | 不涉及 query 的权重 |
| `tests/badlist.rs`（18 + 1 ignored） | **改 1 条** | 见下 |
| `tests/decay_modes.rs`（6） | 全部通过，未改 | 来源都有观点，满覆盖 |
| `tests/normalization.rs`（10） | 全部通过，未改 | 同上；无观点时群体项 0 与 None 等价 |
| `tests/staleness.rs`（16） | 全部通过，未改 | 同上；`prune_stale` 后只剩评分、群体项仍为 0 |
| `tests/p0_core.rs`（10） | 新增 | 本报告的验收测试 |
| 其它 crate（store/view/agg/sim/exp） | 全部通过，未改 | 见 §8 计数 |

**唯一被改的既有测试**：`tests/badlist.rs::no_report_is_bit_identical_to_pre_change`
→ 重命名为 `no_report_matches_new_denominator_baseline`。

原因：该场景里**来源 3 已评分（0.9）、但从未取到观点**。按新口径它现在进分母，
位模式必然变化——这不是 badlist 变了，是分母口径变了。处理：

1. 设置步骤抽成 `baseline_engine()`，两条测试共用；
2. 新测试硬编码**新口径**的位模式（t=0 / 37 / 150 / 1000 + query 位）；
3. 原测试里 `bits(1000) == bits(150)` 的断言删掉：来源 3 是弃权票，没有 `last_check`、
   不衰减；三条观点一起变旧时它的相对权重会上升，所以「全体老化是恒等变换」在这个场景
   不再成立（满覆盖场景仍然成立，见 `s4`）。t=1000 改为单独钉一条位模式；
4. 新增 `#[ignore]` 的 `old_denominator_baseline_is_recorded`，保存**旧口径**的全部位模式：
   修复后它必然失败，用 `--ignored` 可以看到差异；哪天有人把分母改回去，它会变绿而
   上面那条会变红。

没有其它测试因为口径变化被修改或跳过。

---

## 7. 风险与其它发现

1. **所有历史的 S 判据数字失效。** 分母口径变了，任何用 `tm_engine` 复跑的
   S1–S6 / 覆盖率 / 冷启动数字都必须重跑；236.67×、3.36×、Σv=K、29.37% 这些数
   属于改前口径，不能再引用。
2. **`crates/exp` 的框架不会自动跟着变。** 它是独立实现（`crates/exp/src/lib.rs:190-215`）：
   分母只含「本轮抽到的来源」、默认 `exclude_zero` 把 0 读数从分母剔除、clamp 顺序是
   正确的那一版。修复后库与框架的分叉扩大（库现在把弃权票计入分母），**复跑实验前
   必须先把框架对齐**，否则 engine 与框架的数字不可比。本次没有改 exp（不在 P0 范围）。
3. **badlist 闭式解在「有弃权票」时不再精确落在 ā。** `report_bad_account` 的
   `weights/readings` 只取「有观点」的来源，而 query 的分母现在含弃权票，于是压完后的
   群体项 = `ā · Σw' / (Σw' + 弃权权重) < ā`。实测（scenario3 + 一个已评分无观点的来源）：
   目标 ā = 1/3，改后落到 **0.25**；满覆盖时仍精确落在 **0.3333333**。
   方向是「罚得更重」而不是更轻，且不影响既有 badlist 测试（它们的场景都是满覆盖）。
   若要求精确落点，需要让 `report_bad_account` 也按全分母求解——属于后续工作，本次未动。
4. **「全体变旧 ⇒ 结果不变」变成有条件成立。** 只有当每个已评分来源都有观点时才是恒等变换；
   有弃权票时，弃权权重不衰减，观点整体变旧会让群体项向 0 漂移。这是「没有观点就没有
   衰减基准」的直接后果，已写进 `query` 的文档。
5. **公开 API 变化**：`StoredView` 新增公开字段 `mass_covers`。仓库内只有 `on_view` 构造它；
   外部若有手写结构体字面量需要补字段。
6. **NaN 读数的处理变了（更保守）**：`query` 现在走 `reading_of`，非有限读数按 0 处理，
   而不是把整条（连同权重）交给 `aggregate` 剔除。网络路径 `CompressedView::decode`
   已经拒绝 NaN/∞，所以只在进程内构造的视图上有差别。

---

## 8. 复现

```powershell
# 工作树根目录
$env:TMP="$PWD\.tmp"; $env:TEMP="$env:TMP"

cd crates\engine
cargo test                      # 79 通过 + 1 ignored
cargo test --test p0_core       # 10 条 P0 验收
cargo test -- --ignored         # 旧口径对照（预期失败，位模式不同）

# 其它 crate（各自独立 workspace）
cd ..\store ; cargo test       # 133
cd ..\view  ; cargo test       # 49
cd ..\agg   ; cargo test       # 63
cd ..\exp   ; cargo test       # 8
cd ..\sim   ; cargo test       # 130
```

合计：**462 通过 + 1 ignored**（engine 79、store 133、view 49、agg 63、sim 130、exp 8）。

---

## 9. 没有动的东西

β（0.3）、份额算法（评分 ÷ 总和）、压缩视图格式（buckets×hashers×4 字节、编码/解码）、
`EngineConfig` 的既有字段语义、badlist 的公式/接口/折扣语义、抽样与退避、
`MAX_CELLS` / `MAX_HASHERS` 常量、噪声底逻辑。
