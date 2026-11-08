# 实现审计

> 生成于 v1.0 阶段。目的是区分「正式实现」「实验代码」「已废弃」。

---

## 1. 模块清单

### 1.1 正式实现（核心路径）

| 模块 | 职责 | 被谁使用 | 状态 |
|---|---|---|---|
| `rating_list.rs` | 评分清单、线性衰减、按权重抽样 | `baseline_compare` | 正式 |
| `sketch.rs` | Count Sketch 压缩、合并 | `merge_report`、`sketch_report` | 正式 |
| `graph.rs` | 图结构、评分矩阵 | `local_push`、`exact`、`scope` | 正式 |
| `local_push.rs` | PPR 近似解 | `baseline_compare`、`merge_report` | 正式 |
| `exact.rs` | 精确解（小图验证） | 测试 | 正式 |
| `dense.rs` | 矩阵与高斯消元 | `exact` | 正式 |
| `rng.rs` | 确定性随机数 | 全部 | 正式 |

### 1.2 实验代码

| 模块 | 职责 | 用途 |
|---|---|---|
| `fullsim.rs` | 全真模拟：动态网络、攻击、节律 | `fullsim` bin |
| `sim.rs` | 静态图生成（BA/WS/ER） | 多个 bin |
| `attack.rs` | 旧攻击模拟（女巫、潜伏） | `attack_sim` bin |
| `scope.rs` | 旧抽样（按边权重游走） | `merge_report`、`scope_report` |

### 1.3 设计但未接入

| 模块 | 问题 |
|---|---|
| `policy.rs` | 定义了 `PrecisionPolicy` 和 `SketchConfig`，但没有任何 bin 或核心模块引用。只在自己的测试里跑。 |

---

## 2. 被覆盖的旧实现

### 2.1 抽样方式

**旧：`scope::sample`**

从观察者出发，按边权重随机游走，每步以 β 停止。权重是「边权重」，不是「评分」。

```rust
// scope.rs
pub fn sample(g: &Graph, s: usize, k: usize, beta: f64, rng: &mut Rng) -> Scope
```

**新：`rating_list::sample`**

从「我评过分的账户」里按评分概率抽，权重是评分。

```rust
// rating_list.rs
pub fn sample(&self, k: usize, now: u64, rng: &mut Rng) -> Vec<(usize, f64)>
```

**差异**：旧版抽样池是关注列表，新版是评分清单。后者符合设计意图。

**状态**：`scope` 应标记为 deprecated，`merge_report` 和 `scope_report` 是历史实验。

### 2.2 影响力预算

**旧：`fullsim::influence_budget`**

打正分超过 K 后开始衰减。

**新：完全按比例归一化**

```
normalized[v][u] = max(0, P[v][u]) / Σ_x max(0, P[v][x])
```

**状态**：`influence_budget` 字段保留但标记 `#[allow(dead_code)]`，不再参与计算。

### 2.3 攻击模拟

**旧：`attack::sybil_attack` / `latent_attack`**

独立的攻击构造，参数固定。

**新：`fullsim` 的场景配置**

支持攻击比例、背书、节律、曝光的组合。

**状态**：`attack` 只被 `attack_sim` 使用，属于历史实验。

---

## 3. 设计过但未实现

| 设计 | 文档位置 | 状态 |
|---|---|---|
| 介绍人认证的特殊传播通道 | 设计阶段 | 未实现，只验证了 PPR 自然传播（三跳消失） |
| 审计机制（抽查虚报 S） | `SPEC.md` 10.3 | 未实现 |
| 地址规范化与多身份绑定 | `SPEC.md` 第 9 章 | 未实现 |
| 存盘持久化 | `rating_list.rs` 的 `compact` | 只有接口，没有实际序列化 |
| 差分隐私加噪 | `sketch.rs` 的 `publish` | 实现存在，但未接入主路径 |
| 内容评分 | — | 明确不做 |

---

## 4. 文档与代码不一致

### 4.1 衰减方式

| 文档 | 代码 |
|---|---|
| `SPEC.md` 7.x：指数衰减 γ | `rating_list.rs`：线性衰减，`max(0, 1 - t/(2H))` |
| `PARAMETERS.md`：γ=0.99，half_life=7 天 | `RatingList::new(72, ...)`：H=3 天 |

**需要统一**：以 `rating_list` 的线性衰减为准，更新文档。

### 4.2 传播跳数

| 文档 | 代码 |
|---|---|
| `SPEC.md` 3.3：允许三跳截断，误差 2.7% | `fullsim.rs`：迭代到收敛，`max_iters=30` |

**需要统一**：正式实现不截断，实验代码才截断。

### 4.3 抽样方式

| 文档 | 代码 |
|---|---|
| `SPEC.md` 7.1：从观察者出发随机游走 | `rating_list.rs`：从评分清单按权重抽 |

**需要统一**：以 `rating_list` 为准。

### 4.4 攻击数据

| 文档 | 代码 |
|---|---|
| `SIMULATION.md`：旧模型的攻击数据（评分只按 quality） | `fullsim.rs`：评分含 quality + 兴趣 |

**需要重跑**：旧数据不代表当前模型。

---

## 5. 建议清理顺序

1. **统一衰减描述**：`SPEC.md` 和 `PARAMETERS.md` 改成线性衰减
2. **标记废弃**：`scope`、`attack` 加 `#[deprecated]` 注释
3. **决定 policy 去留**：要么接入 `sketch`，要么删除
4. **重跑 SIMULATION**：用当前模型的数据替换旧数据
5. **补测试**：`rating_list` 与 `baseline_compare` 的集成测试

---

## 6. 当前可用的正式 API

```rust
// 评分清单
let mut list = RatingList::new(half_life, capacity, StorageMode::Memory);
list.record(account, weight, now);
let samples = list.sample(k, now, &mut rng);
let weights = list.merge_weights(now);

// 压缩
let mut sk = Sketch::new(d, buckets, seed);
sk.add_row_sparse(&row, neutral, weight);
let est = sk.query_with_baseline(u, neutral);
let merged = a.merge(&b, wa, wb);

// 传播
let trust = solve_local(&graph, observer, &params);
let exact = solve_exact(&graph, beta);
```

这三块是稳定的，其余是实验。
