# B2 · badlist 按新分母重新求解（弃权票口径对齐）

> 工作树：`wt/b_inv`　分支：`test/b-invariants`　基线：`214d515`
> 任务：交接文档 §7.2 / 共享任务 `task-2`（P0 核心修复报告 §7.3 登记）
> 改动文件：`crates/engine/src/badlist.rs`、`crates/engine/src/lib.rs`（只动 badlist 段）、
> `crates/engine/tests/badlist.rs`。`query` / `p0_core.rs` / 其它 crate 一行未动。

---

## 0. 一句话结论

P0① 把群体项分母改成「我评过的**全部**来源（含弃权票）」，而 badlist 的闭式解仍在
「只含有观点的来源」这个旧坐标系里解方程，于是求出的 `f` 只让**表过态的那部分**
回到 ā，弃权份额又把结果往下拖：目标 1/3 实测落到 **0.25**（罚得偏重）。

修法是**只把弃权票并进目标方程的分母**（记 `W_a`），**不并进 ā**——弃权票没有读数，
它不是「给 A 打 0 分」。改完之后解出的落点对任意 `W_a` 都精确等于 ā，
满覆盖时新公式退化成旧公式（逐位相同）。**结论：按新分母求解在数学上完全可行，
不是「方程无解」那类情况。**

---

## 1. 推导

### 1.1 记号

~~~text
a_v    = 来源 v 的观点对 A 的读数，已夹到 [0,1] 并过噪声底（query 与 badlist 共用 reading_of）
w_v    = 份额_v × 衰减_v      （不含折扣；与 query 的群体项同口径）
表过态 = 有观点缓存，且该观点确实覆盖了 A（A < mass_covers）
弃权   = 我评过分，但没有观点；或有观点但那份观点在 A 登记之前生成
~~~

- **ā**：只对**表过态**的读数求**未加权**平均。未加权是关键：加权平均恰好就是 A 现在的
  群体项得分，拿它当目标线等于「把 A 压到 A 自己」。
- **G_A**：A 的群体项（query 的口径，分母含弃权票）：

~~~text
G_A = Σ_{表过态} w_v·a_v(p) / ( Σ_{表过态} w_v + Σ_{弃权} w_v )
~~~

- **S+** = { 表过态来源 : a_v > ā }，**S−** = { 表过态来源 : a_v ≤ ā }。
  弃权票既不在 S+ 也不在 S−：没有读数就没有「高于/低于平均」可言，
  它永远不会被乘 f（一个没表态的人没有峰可削）。

### 1.2 方程

对 S+ 里的所有来源乘同一个折扣 f（S− 与弃权票不动），要求压完后的群体项精确等于 ā：

~~~text
(f·A1 + B1) / (f·A0 + B0 + W_a) = ā
   A0 = Σ_{S+} w_v      A1 = Σ_{S+} w_v·a_v
   B0 = Σ_{S−} w_v      B1 = Σ_{S−} w_v·a_v
   W_a = Σ_{弃权} w_v
~~~

解出：

~~~text
f = (ā·B0 − B1 + ā·W_a) / (A1 − ā·A0)
  = ( Σ_{S−} w_v·(ā − a_v) + ā·W_a ) / Σ_{S+} w_v·(a_v − ā)
~~~

第二种写法是**数值安全**的关键：分子里每一项都 ≥ 0（S− 的差距项和 `ā·W_a` 都非负），
分母也只对 S+ 累加非负项，不会出现「两个大数相减吃掉有效位」。
旧公式与它的唯一差别就是分子里没有 `ā·W_a`——**W_a = 0 时两者代数上完全相同**。

### 1.3 为什么 0.25 是「罚得偏重」

旧公式把 `f` 解成「让表过态那部分回到 ā」的值，但 query 的分母还挂着弃权份额：

~~~text
旧口径：G_A = ā · Σ_{表过态} w' / (Σ_{表过态} w' + W_a) = ā · (1 − W_a/Σ_all w) < ā
~~~

实测场景（scenario3 + 1 个已评分无观点的来源，份额 1/6）：

| 量 | 旧口径 | 新口径 |
|---|---:|---:|
| ā | 1/3 | 1/3（弃权票不进 ā，**目标线不变**） |
| 解出的 f | 1/3 | **1/2**（= (k+2)/6，k=1） |
| 压完后的 G_A | **0.25** | **1/3** |

> 注意 0.25 的巧合：它既等于旧口径的实际落点，也等于「把弃权票真的算进 ā 之后的
> 目标线（1/4）」。但两者原因完全不同——**我们没有、也不应该把弃权票算进 ā**，
> 那会让目标线随覆盖率漂移（覆盖率越低目标越低），机制就变成「覆盖不足就少罚」。

### 1.4 为什么弃权票不能进 ā

如果 ā 也按新分母（把弃权票按 0 读数计入）：

- ā 会从「一个普通来源怎么看 A」变成「**平均每个被我评过的人（含沉默者）**怎么看 A」，
  它是**覆盖率的函数**：覆盖率越低 ā 越低；
- 同一个 A、同一个观点集合，只要我多给几个还没取到观点的人打分，目标线就会下降，
  罚得就变轻。badlist 的输入是「A 正在攻击我」，跟我的取数进度无关；
- S+/S− 的划分也会跟着漂移，机制失去「谁比平均更亲近 A」这个语义；
- 端到端看，取数不全时所有人（包括真正包庇 A 的人）都会被算成 0 票，
  这等于把「还没取到」当成「表态了是 0」，方向恰好反过来。

所以分界线是：**弃权票进「度量衡」（分母），不进「观点」（分子 / ā）**。这与 query 的
`(份额, 0.0)` 是同一件事——权重进分母、值按 0，但 badlist 还要再多走一步：
它不能把那个 0 当成一个「读数」去算平均，因为那样 ā 会被沉默者拉低。

---

## 2. 改动（文件:行）

### 2.1 `crates/engine/src/lib.rs`

| 位置 | 改动 |
|---|---|
| `:581` | 文档：`f 不低于已有折扣` → `折扣（按 f32 口径）不低于已有折扣` |
| `:602-641` | `report_bad_account` 的扫描从 `for (owner, stored) in &self.views` 改成 `for (owner, rating, _) in self.ratings.iter()` ——**遍历集合与 query 的分母完全一致** |
| 同上 | 有观点：`decay = decay_factor(elapsed(last_check, now))`、读数走 `reading_of`（**只在 `idx < mass_covers` 时才算读数，否则 None**，与 query 的 `mass_covers` 守卫逐项一致）；没观点：`decay = 1.0`、读数 `None`（与 query 的弃权票逐项一致） |
| `:652-658` | `readings` 类型 `Vec<f64>` → `Vec<Option<f64>>` |
| `:660-680` | 折扣比较从 f64 改成 Score(f32)：`let new_discount = f as Score; if new_discount < old { … }`。理由见 §2.3 |

### 2.2 `crates/engine/src/badlist.rs`

| 位置 | 改动 |
|---|---|
| 模块文档 | 补 `W_a` 的定义、方程、以及「为什么弃权票只进分母不进 ā」整节 |
| `UniformSolution` | `abar`/`ga`/`above` 的文档写明弃权票语义 |
| `solve_uniform` | 签名 `readings: &[Option<f64>]`；ā 只对 `Some` 求平均（`stated == 0` 直接 `f = 1`）；`None` 的权重进 `weight_abstain`；分子加 `abar * weight_abstain`；`ga` 分母含弃权票（观测口径与 query 一致） |
| `SourceSnapshot` | `reading: f64` → `Option<f64>`，文档说明快照覆盖「我评过的全部来源」 |

### 2.3 顺手修的一个真实 bug（f64 比较 vs f32 存储）

原来的写入判断是 `if f < f64::from(self.penalty_of(idx))`，而折扣表存的是 `f32`。
当目标线恰好达成（`G_A = ā`）时，闭式解的 f64 可能算出 `0.999999999937`（来自
`Score`→f64 的往返误差），它**小于 1.0**，于是：

- 折扣表被写进一个「1.0」的空条目（`penalty_of` 照样返回 1.0，但表不再为空，
  `query` 的热路径 `has_penalty` 分支开始生效）；
- `report_bad_account` 返回 **1** 而不是 **0**——「真正被压低的来源数」多算一个。

这不是理论噪声：新测试 `abstentions_can_make_the_target_unreachable_without_punishment`
（k=4 时压前 G_A 恰好 = ā = 1/3）第一次跑就是红的。改成在 f32 上比较后，这一类
「f64 里看起来更低了、f32 里其实一样」的情况不会再写表。**语义不变**（`min` 仍在
f32 口径下取最小），旧测试全部逐位通过。

---

## 3. 测试

### 3.1 求解器单测（`src/badlist.rs`，11 条，新增 5 条）

| 测试 | 钉住什么 |
|---|---|
| `uniform_closed_form_hits_target` | **回归保护**：4000 组全表过态随机样本，压完仍精确落在 ā（触发数 ≥ 1500） |
| `uniform_closed_form_hits_target_with_abstentions` | **新增**：4000 组随机含弃权票（权重跨 6 个数量级），压完仍精确落在 ā；弃权票永远不在 S+ |
| `abstention_only_enters_the_denominator` | **新增**：w=[3,1,1,1]、a=[1,0,0,弃权]，ā=1/3 → f=1/2 → 压完 1/3；旧公式 f=1/3 → 1/4 |
| `abstentions_do_not_shift_the_mean` | **新增**：加弃权票 ā 逐位不变，f 单调不减（罚得更重，不放水） |
| `no_stated_reading_exits` | **新增**：全是弃权 → ā 未定义 → f=1，不 panic |
| `zero_weight_abstention_is_a_noop` | **新增**：`w=0` 的弃权票对 f/ā/G_A **逐位**无影响 |
| 其余 6 条 | 原样保留（`Some(...)` 包装），语义未变 |

### 3.2 引擎集成测试（`tests/badlist.rs`，23 条，新增 4 条）

| 测试 | 钉住什么 |
|---|---|
| `abstentions_are_folded_into_the_solver_denominator` | **新增，核心**：部分覆盖 + 弃权票（k=1/2/3），压完精确落在 ā=1/3；s0 折扣 = (k+2)/6；弃权票与 S− 都不被罚；重复报告返回 0 |
| `abstentions_can_make_the_target_unreachable_without_punishment` | **新增**：k=4 时压前 G_A 恰好 = ā → `report_bad_account` 返回 **0**、折扣表为空 |
| `full_coverage_still_hits_the_plain_mean` | **新增，回归**：满覆盖 5 来源，读数 [1,.75,.5,.25,0]、权重 4:1:1:1:1，f=1/3，压完精确落在 ā=0.5 |
| `report_without_any_statement_about_a_is_a_defined_noop` | **新增**：① 一条观点都没有；② 有观点但 A 是 `on_view` 之后才登记（`mass_covers` 不覆盖）。两种都**不 panic、返回 0、折扣表为空、分布逐位不变** |
| 其余 19 条 | 全部原样通过，包括 `no_report_matches_new_denominator_baseline` 的 4 组硬编码位模式 |

### 3.3 「新测试能复现旧行为」的证据

做法：`git stash push -- crates/engine/src/{badlist,lib}.rs`（只留测试改动），
在**旧求解器**上跑同一条测试文件。

~~~text
===== 旧代码 + 新测试 =====
test abstentions_are_folded_into_the_solver_denominator ... FAILED
  k=1: 压完应精确落在 ā=1/3，实际 0.25（旧口径是 1/(k+3)）
test abstentions_can_make_the_target_unreachable_without_punishment ... FAILED
  assertion `left == right` failed: 已经在目标线上，不该罚
  left: 1
 right: 0
test report_without_any_statement_about_a_is_a_defined_noop ... FAILED
  assertion `left == right` failed: 观点不覆盖 A ⇒ 没人表态
  left: 2
 right: 0
test result: FAILED. 19 passed; 3 failed; 1 ignored
~~~

第一行就是交接文档 §7.2 / `审计/P0-core.md` §7.3 记录的 **0.25**，现在被测试钉住。

改回新代码后：

~~~text
cargo test                                                    # 21 + 3 + 22 + 6 + 11 + 10 + 16 + 0
test result: ok. 21 passed   (src/lib.rs，含 11 条求解器单测)
test result: ok. 3 passed    (tests/backoff.rs)
test result: ok. 22 passed; 1 ignored   (tests/badlist.rs)
test result: ok. 6 passed    (tests/decay_modes.rs)
test result: ok. 11 passed   (tests/normalization.rs)
test result: ok. 10 passed   (tests/p0_core.rs)
test result: ok. 16 passed   (tests/staleness.rs)
合计 89 通过 + 1 ignored（改前是 79 通过 + 1 ignored，净增 10 条）
~~~

`crates/exp` 依赖 `tm_engine`，受 ctor 签名变化影响，也跑了一遍：
`cargo test` → 全部 ok（`tests/runtime.rs` 8 条通过，各 bin 0 条）。

---

## 4. 边界与遗留

1. **`W_a → 巨大` 时 f 会被夹到 1**。弃权权重越大，要把 G_A 压回 ā 需要的 f 越接近
   （甚至超过）1；当压前 `G_A ≤ ā`（弃权票多到把群体项稀释到目标线以下）时闭式解给出
   `f ≥ 1`，夹紧后就是不罚。这是「只削峰、不填谷」的正确行为，不是缺陷——
   没有谁的峰需要削。测试 `abstentions_can_make_the_target_unreachable_without_punishment`
   钉住了 k=4 这个分界点。
2. **`report_bad_account` 现在是 O(正分清单)**，不再是 O(观点缓存)：它要遍历全部
   评分条目（含没有观点的）。badlist 是应用层手工触发的一次性事件、不是热路径，
   这个代价可以接受；`query` 本来就是同一量级。
3. **`reading_of` 与 `mass_covers` 的分工没变**：badlist 现在也在 `idx < mass_covers`
   上做「没覆盖 ⇒ 弃权」的判断，与 query 逐项一致。这是 P0② 附带项想要的效果——
   两条路径对同一份字节给出同一个读数。
4. **P0-core.md §7.3 已过时**。它记录的「badlist 在弃权票下不再精确、0.25」是本工作树
   修掉的缺陷；该文档是历史报告，没有回改（避免篡改历史结论），修正记录在本文件。
   `交接文档.md` §7.2 同样待主智能体统一更新（不在本任务的 write scope 内）。
5. **没动的东西**：`query()`、`p0_core.rs`、`EngineConfig` 字段、折扣的 min 语义、
   不复合、不自动恢复、`penalty_strength` 的唯一旋钮地位、其它 crate。
6. **`crates/exp` 的独立框架（`run_case_full`）仍与库分叉**（交接文档 §7.4 / 共享任务
   `task-3`），那是另一条任务；本任务只保证 engine 一侧口径自洽。

---

## 5. 复现

~~~powershell
$env:TMP='D:\Project\deepseek_harness\去中心化算法探索-A\.tmp'; $env:TEMP=$env:TMP
Push-Location -LiteralPath 'D:\Project\deepseek_harness\去中心化算法探索-A\wt\b_inv\crates\engine'
cargo test                      # 89 通过 + 1 ignored
cargo test --test badlist       # 22 通过 + 1 ignored（含 4 条新增）
cargo test --lib                # 21 通过（含 11 条求解器单测）

# 「旧代码 + 新测试」的证据
Push-Location -LiteralPath 'D:\Project\deepseek_harness\去中心化算法探索-A\wt\b_inv'
git stash push -- crates/engine/src/badlist.rs crates/engine/src/lib.rs
Push-Location crates\engine; cargo test --test badlist   # 3 红，其中一条正是 0.25
git stash pop
~~~
