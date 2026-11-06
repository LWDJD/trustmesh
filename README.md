# 社交信任库

去中心化社交信任的 Rust 实现。核心是「以自己为中心」的评分传播 + 抽样压缩。

## 文档

| 文件 | 内容 |
|---|---|
| `SPEC.md` | 协议标准：数学模型、数据结构、算法、身份 |
| `SECURITY.md` | 安全模型：攻击方式、防御机制、实测数据 |
| `PARAMETERS.md` | 参数公式、调优流程、推荐配置 |

三份文档配套读。`SPEC.md` 说「应该怎样」，`SECURITY.md` 说「为什么这样」，`PARAMETERS.md` 说「参数取多少」。

## 核心数学

```
Π = β·P + (1-β)·W·Π
Π = β·(I - (1-β)·W)⁻¹·P
```

`Π[s][u]` 是观察者 s 眼中账户 u 的信任分。β=0.7 表示个人判断占七成。

传播无跳数上限，第 t 跳权重 `(1-β)^t`。数值实现可截断，但误差需小于总质量 1%。

## 快速开始

```bash
cargo test                              # 28 个测试
cargo run --release --bin simulate      # 基础验证报告
cargo run --release --bin attack_report # 攻击模拟
cargo run --release --bin scope_report  # 范围化报告
cargo run --release --bin sketch_report # 压缩报告
```

## 核心接口

```rust
use social_trust::{Graph, solve_exact, solve_local, LocalPushParams};

let mut g = Graph::new(n);
g.add_edge(0, 1, 1.0);
g.set_rating(0, 1, 0.5);
g.finalize();

let pi = solve_exact(&g, 0.7);   // 精确解，小图
let score = pi[0][1];

let row = solve_local(&g, 0, &LocalPushParams::default());  // 近似解
let score = row[1];
```

## 模块

| 文件 | 职责 | 状态 |
|---|---|---|
| `src/graph.rs` | 图结构、评分矩阵 | 完成 |
| `src/dense.rs` | 矩阵、高斯消元 | 完成 |
| `src/exact.rs` | 精确解 | 完成 |
| `src/local_push.rs` | 近似解 | 完成 |
| `src/scope.rs` | 抽样、合并 | 完成 |
| `src/sketch.rs` | 压缩、加权合并 | 完成 |
| `src/attack.rs` | 攻击模拟 | 完成 |
| `src/sim.rs` | 图生成 | 完成 |
| `src/rng.rs` | 确定性随机数 | 完成 |
| 异步迭代 | 持续传播 | 未实现 |
| 双时间尺度累积 | 突变检测 | 未实现 |
| 地址与身份 | CAIP-10、多身份绑定 | 未实现 |
| 审计 | 虚报检测 | 未实现 |

## 关键数字

- 精确解与近似解误差：6e-4（eps=1e-4）
- 抽样 k=6400 时误差：3.2e-4
- 压缩 S=500, B=2048 时误差：<1%
- 发布体积：40 KB（B=1024）到 320 KB（B=8192）
- 女巫攻击（10% 被骗，β=0.7）：目标分提升 2%
- N 从 1000 到 100 万，体积不变

## 未解决

冷启动、长尾内容发现、审计激励、移动端功耗、检测延迟量化。详见 `SPEC.md` 第 14 章。
