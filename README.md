# trustmesh

去中心化信任网络。核心是「伪名 + 信誉」：每个操作都挂在一个有历史的账户上，攻击成本从技术转移到时间。

## 它做什么

- 按评分加权的信任传播，每个节点只信自己评过分的人
- 线性衰减，旧关系 2 倍半衰期后归零
- Count Sketch 压缩，发布体积固定
- 极轻量运行时，不绑定 async，单线程无锁
- 四套实测参数，默认防攻击优先

## 它不做什么

- 不做全局共识（每个人看到的分数不同）
- 不做内容分类（只输出作者可信度）
- 不做公共可验证评分（第三方无法验证你算的对不对）

## 文档

| 文件 | 内容 |
|---|---|
| `SPEC.md` | 协议标准：数学模型、数据结构、算法、身份 |
| `SECURITY.md` | 安全模型：攻击方式、防御机制、实测数据 |
| `PARAMETERS.md` | 参数公式、调优流程、推荐配置 |
| `ARCHITECTURE.md` | 运行时分层、消息协议、测试策略 |
| `SIMULATION.md` | 全真模拟数据 |
| `AUDIT.md` | 实现审计：哪些正式、哪些实验、哪些废弃 |

## 核心数学

```
Π = β·P + (1-β)·W·Π
```

`Π[s][u]` 是观察者 s 眼中账户 u 的信任分。默认 β=0.3，表示个人判断占三成，群体占七成。

传播无跳数上限，第 t 跳权重 `(1-β)^t`。数值实现可截断，但误差需小于总质量 1%。

信任只从「我评过分的账户」进入，按评分比例加权。没评过的人，它的意见不影响我。

## 快速开始

```rust
use trustmesh::presets::Preset;

// 一套默认参数，实测最优（防攻击优先）
let p = Preset::recommended();

// 评分清单
let mut list = p.rating_list();
list.record(42, 0.8, 0);       // 给账户 42 打 0.8 分

// 抽样（按评分加权，半衰期 3 天线性衰减）
let mut rng = trustmesh::rng::Rng::new(1);
let samples = list.sample(p.samples, 0, &mut rng);

// 压缩
let mut sk = p.sketch(1);
sk.add_row_sparse(&[0.5; 100], 0.5, 1.0);
```

四套预设：

| 预设 | 个人:群体 | K | 半衰期 | 适用 |
|---|---|---|---|---|
| `recommended` | 3:7 | 50 | 3 天 | 防攻击优先（默认） |
| `balanced` | 5:5 | 50 | 7 天 | 平衡 |
| `personal` | 7:3 | 100 | 14 天 | 个人观点优先 |
| `mobile` | 3:7 | 20 | 3 天 | 手机、低功耗 |

详细参数见 `PARAMETERS.md`，实测依据见 `SIMULATION.md`。

## 命令行工具

```bash
cargo test                              # 28 个测试
cargo run --release --bin simulate      # 基础验证报告
cargo run --release --bin attack_report # 攻击模拟
cargo run --release --bin scope_report  # 范围化报告
cargo run --release --bin sketch_report # 压缩报告
```

## 核心接口

```rust
use trustmesh::{Graph, solve_exact, solve_local, LocalPushParams};

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
| `rating_list.rs` | 评分清单、线性衰减 | 完成 |
| `sketch.rs` | 压缩、加权合并 | 完成 |
| `presets.rs` | 四套实测最优参数 | 完成 |
| `local_push.rs` | 近似解 | 完成 |
| `exact.rs` | 精确解 | 完成 |
| `graph.rs` | 图结构 | 完成 |
| `dense.rs` | 矩阵运算 | 完成 |
| `rng.rs` | 确定性随机数 | 完成 |
| `fullsim.rs` | 全真模拟 | 实验 |
| `sim.rs` | 静态图生成 | 实验 |
| `policy.rs` | 精度策略接口 | 未接入 |
| `scope.rs` | 旧抽样 | 已废弃 |
| `attack.rs` | 旧攻击模拟 | 已废弃 |
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
