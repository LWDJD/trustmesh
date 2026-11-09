# tm_sim · trustmesh 社会仿真框架

> 纯 std，零外部依赖。`cargo test` 全绿（114 个测试，0 failed）。

本 crate **不实现信任算法**。它负责造一个带社会细节的虚拟世界，跑时间，植入攻击者，
输出「谁给谁打了多少分」这种输入数据，以及一组可以直接对照实验的指标骨架。

```text
crates/sim/
├── Cargo.toml            # name = "tm_sim", edition = "2021", [dependencies] 为空
├── src/
│   ├── lib.rs            # crate 文档、常量、模块导出
│   ├── rng.rs            # xorshift64* + 正态/对数正态/泊松/帕累托/加权抽样
│   ├── config.rs         # SimConfig、AttackKind、参数夹紧
│   ├── interest.rs       # 8 维非负单位兴趣向量、余弦相似度
│   ├── community.rs      # 社团中心、成员表、跨社团比例公式
│   ├── account.rs        # 账户、种类、生命周期、关注与评分存储
│   ├── behavior.rs       # 评分公式（质量 × 匹配 × 噪声）
│   ├── attack.rs         # 攻击计划与收益方
│   ├── metrics.rs        # Snapshot / 分布 / 分类 / 社团 / 基尼 / 相关
│   ├── world.rs          # 时间循环：生命周期 → 攻击 → 关系 → 互动
│   └── bin/sim_report.rs # 示例实验（基线 / 攻击对比 / 同质性扫描 / 长跑）
└── tests/                # 8 个集成测试文件
```

---

## 1. 快速开始

```rust
use tm_sim::{AttackKind, SimConfig, World};

let cfg = SimConfig {
    n_initial: 200,
    steps: 200,
    seed: 7,
    attack_kind: AttackKind::Sybil { cluster: 3 },
    attack_ratio: 0.1,
    ..SimConfig::default()
};

let mut world = World::new(cfg);
world.run();

let ratings: Vec<(usize, usize, f32)> = world.ratings(); // (观察者, 被评者, 评分)
let snap = world.report();                               // 指标快照
println!("{}", snap.render());                            // 人类可读的报表
```

命令行：

```bash
cargo run --release --bin sim_report            # 全部
cargo run --release --bin sim_report base       # 基线（200 账户 200 步）
cargo run --release --bin sim_report attack     # 五类攻击对比
cargo run --release --bin sim_report homophily  # 同质性 0→1 扫描
cargo run --release --bin sim_report long       # 1000 步稳定性
```

---

## 2. 接口契约

### 2.1 SimConfig

`World::new` 会先调 `SimConfig::validated` 把非法参数夹紧（NaN/inf 回退默认值、
比例夹到 [0,1]、计数至少为 1、`follow_min ≤ follow_max ≤ max_follows`），
所以批量扫参时不会因为一个笔误崩掉整轮实验。

| 分组 | 字段 | 默认值 | 含义 |
|---|---|---|---|
| 人口 | `n_initial` | 200 | 初始账户数 |
| | `steps` | 200 | `World::run` 的步数（1 步 = 1 小时） |
| | `join_rate` | 0.15 | 每小时期望加入人数（泊松） |
| | `leave_rate` | 0.00012 | 活跃账户每小时基础离开概率（随低活跃放大最多 4 倍） |
| | `dormancy_rate` | 0.0005 | 每小时进入休眠的概率（同样随低活跃放大） |
| | `sleep_hours` | (48, 240) | 休眠时长区间 |
| | `return_rate` | 0.9 | 休眠结束回归概率，失败即永久流失 |
| | `max_accounts` | 20000 | 账户表硬上限（内存上界） |
| 兴趣/结构 | `interest_dims` | 8 | 兴趣维度 |
| | `homophily` | 0.6 | 同质性：同时压低跨社团比例、放大相似度指数 |
| | `topic_correlation` | 0.45 | 兴趣与社团中心的相关度 |
| | `community_count` | 4 | 社团数 |
| | `bridge_ratio` | 0.12 | 桥接节点比例 |
| | `bridge_gain` | 0.5 | 桥接节点额外跨社团的强度 |
| 关系/活跃 | `follow_min/follow_max` | 6/10 | 新账户初始关注数区间 |
| | `max_follows` | 48 | 单账户关注上限（内存上界的另一半） |
| | `follow_rate` | 0.008 | 每小时新增关注的期望（×活跃强度） |
| | `activity_alpha` | 1.7 | 活跃强度的幂律指数 |
| | `activity_cap` | 12 | 活跃强度上限 |
| | `interactions_per_step` | 0.9 | 每小时互动基准（×活跃强度） |
| | `enable_rhythm` | true | 昼夜节律开关（关掉＝活着就在线） |
| 评分 | `content_noise` | 0.15 | 每次发帖的质量抖动（正态，相对值） |
| | `rating_noise` | 0.25 | 评分的乘性噪声（对数正态 σ） |
| | `min_stance` | 0.03 | 表态阈值，低于它记 0（没表态） |
| | `reputation_feedback` | 0.0 | 声誉反馈强度（马太效应开关） |
| 攻击 | `attack_ratio` | 0.1 | 攻击者占初始人口比例 |
| | `attack_kind` | `None` | 见第 4 节 |
| | `infiltrate_at` | `None` | 渗透者收买时刻，默认总步数 1/3 |
| | `attack_focus` | 4.0 | 攻击者对被推广账户的互动加权 |
| 记录 | `snapshot_every` | 0 | 每 N 步自动存快照，0 = 不存 |
| | `max_history` | 4096 | 历史快照上限 |
| | `seed` | 1 | 随机种子 |

### 2.2 World

| 方法 | 说明 |
|---|---|
| `new(cfg) -> World` | 建初始人口 → 植入攻击者 → 建立初始关注；不 panic |
| `step(&mut self)` | 推进一小时 |
| `run(&mut self)` | 跑 `cfg.steps` 步 |
| `run_steps(n)` | 跑 n 步 |
| `ratings(&self) -> Vec<(usize, usize, f32)>` | **核心输出**：全部正分评分，顺序固定 |
| `report(&self) -> Snapshot` | 当前时刻的完整指标 |
| `alive_count / online_count / rating_count` | 快速计数 |
| `indices_of(kind)` | 某类账户的内部编号 |

账户表、社团、攻击计划、累计统计都是 `pub` 字段，实验脚本可以直接读。
**写请走 `step`**，否则会破坏不变式。

### 2.3 Snapshot

| 分组 | 字段 |
|---|---|
| 规模 | `step` `time` `accounts` `alive` `online` `dormant` `left` `bridges` `follow_edges` `rating_edges` |
| 分类 | `classes: Vec<ClassStat>`（7 类固定顺序：honest, sybil, sleeper, infiltrator, endorser, spammer, promoted） |
| 社团 | `community: CommunityStat`（内/间关注边与占比、内/间评分均分、圈子化） |
| 分布 | `distribution: Distribution`（count/mean/std/min/p10/p25/p50/p75/p90/p99/max） |
| 攻防 | `attacker_mean` `attacker_advantage` `target_mean` `target_ratings` `target_advantage` `observer_win_rate` `observers_compared` `honest_quality_corr` `gini_received` |

`ClassStat`：数量、存活、在线、质量均值、收到/给出评分条数与均分、声誉代理均值。
`Snapshot::all_finite()` 可以一次性检查所有浮点字段（写 CSV 前自检用）。

---

## 3. 社会细节怎么建模的

### 3.1 人口动态

- **加入**：每小时泊松 `join_rate` 个新账户。新账户加入时抽兴趣、质量、活跃强度、
  活跃时段，按规模加权选社团，然后立刻建立 `follow_min..follow_max` 条关注。
- **离开**：`p = leave_rate × (1 + 3 × 不活跃度)`，越不活跃越容易流失。离开是永久的，
  账户留在表里（编号不回收）、历史评分保留、不再上线。
- **休眠**：`p = dormancy_rate × (1 + 2 × 不活跃度)`，睡 `sleep_hours` 小时；
  醒来时以 `return_rate` 回归，否则**永久流失**——这条把「长期不上线」与「离开」区分开。

不活跃度定义为 `1 − min(活跃强度/4, 1)`：活跃强度是幂律分布，绝大多数人接近 1，
少数人高达几十，低活跃者（<4）按比例获得更高的流失/休眠概率。

### 3.2 兴趣与同质性

兴趣是 **8 维非负单位向量**。生成方式：

```text
center[c]   = normalize(Exp(1) × 8)                     // 社团主题中心
raw         = (1 − ρ) × normalize(Exp(1) × 8) + ρ × center[c]   // ρ = topic_correlation
interest    = normalize(raw)                            // L2 范数 = 1
```

用 Dirichlet(1) 方向（指数分布再归一化）而不是「均匀分布再归一化」，是为了均匀覆盖单纯形，
不会让兴趣总是聚在中间。ρ = 0 表示社团与兴趣无关，ρ = 1 表示社团就是同好会。

**关注权重**（从候选池里不放回抽 k 个）：

```text
w(u) = ((0.5 + 0.5 × cos(s,u)) ^ (1 + h × 5)) × (0.25 + quality(u)) × (1 + β_rep × 3 × reputation(u))
```

- `h` = homophily：0 时指数为 1（温和偏好），1 时指数为 6（极端挑剔同类）；
- 质量项让「有内容的人」更容易被关注；
- 声誉项是可选开关（默认关），打开即为马太效应。

### 3.3 社团与桥接

- 前 K 个账户先铺满 K 个社团，保证没有空社团；之后新账户按 `成员数 + 1` 加权选社团
  （富者更富，但小社团不会被饿死）。
- **跨社团关注比例**：

```text
random_mix = 1 − 1/K
share      = random_mix × (1 − h)              // 同质性把跨社团压到 0
if 桥接:    share += (1 − share) × bridge_gain  // 桥接节点额外跨社团
share      = clamp(share, 0, 0.9)
```

所以 `homophily = 0` 时接近随机混合，`homophily = 1` 时只有桥接节点还在跨社团。
一个账户的 k 个关注被拆成「同社团若干 + 跨社团若干」两池分别抽，某一池不够时从另一池补。

### 3.4 活跃度

- **时段**：随机 `active_start ∈ [0,24)`，`active_span ∈ [6,18]` 小时，按 24 取模判定在线。
- **强度**：`activity = min(Pareto(α=1.7, x_min=1), cap)`，均长约 2.4，长尾可以到 12。
  幂律保证「少数超级活跃用户 + 大量普通用户」。
- **每步行为量**：互动次数 `~ Poisson(interactions_per_step × activity)`，
  新增关注 `~ Poisson(follow_rate × activity)`，两者都有硬上限兜底。

### 3.5 评分行为

```text
内容质量 = quality(u) × (1 + content_noise × N(0,1))     // 逐次抖动，截断到 [0,1]
兴趣匹配 = 0.5 × (1 + cos(s,u))                           // [0,1]
噪声     = LogNormal(0, rating_noise)                     // 乘性，中位数 1
评分     = clamp(内容质量 × 兴趣匹配 × 噪声, 0, 1)
若评分 < min_stance → 没表态，不写入
```

- 只有**关注且在线**的人才会被互动、被打分；一条评分一旦写入，后续只做覆盖更新。
- **0 不是差评**：评分只有 (0,1]，0 表示没表态，表示关系不存在。协议里没有负分。

### 3.6 声誉反馈（马太效应，可选）

```text
reputation += 0.05 × (新收到的评分 − reputation)     // 指数滑动平均
```

`reputation_feedback > 0` 时，它在关注权重和互动权重里都额外加权，
于是「被评得高 → 更容易被关注 → 更多人评它」形成正反馈。默认关闭，
打开后可以直接观察影响力基尼系数与质量-评分相关的漂移。

> ⚠️ 声誉代理不是信任算法的输出。它只是「最近收到的原始评分」的平滑值，
> 用来给「高声誉者权重更高」这个社会开关提供输入。

---

## 4. 攻击者

`attack_ratio` 决定攻击人口占初始人口的比例；`attack_kind` 决定类型。

| 类型 | 人口 | 行为 | 收益方 |
|---|---|---|---|
| `Sybil { cluster }` | `attack_ratio × n_initial` 个新账户，均分到 cluster 个集群 | 集群内部全连接、互评 **1.0**、绝不对外打分 | 第一个女巫 |
| `Sleeper { normal_until }` | 同上，质量正常（0.35~0.65） | `normal_until` 前完全正常，之后对收益方打 1.0 并继续正常打分掩盖 | 第一个潜伏者 |
| `Infiltrator` | 不新增攻击账户；在 `infiltrate_at` 按声誉从高到低收买 `attack_ratio × n_initial` 个诚实账户 | 被收买前完全正常；之后对收益方打 1.0，其余照旧 | `Promoted` 账户 |
| `Endorser { ratio }` | 开局把质量最高的 `ratio × n_initial` 个诚实账户变成背书者 | 公开给收益方打 1.0，其余照旧 | `Promoted` 账户 |
| `Spammer` | `attack_ratio × n_initial` 个新账户，质量 0.01~0.06 | 关注数 ×3，不看质量随机打 0.55~0.95 的虚高分 | 第一个垃圾账户 |

补充设计：

- **女巫对正常人不可见**。正常人的社交发现会跳过女巫账户：没有人为它背书时，
  它进不了任何人的视野。这是设计规格里「只有被评过分的人才能沿着评分链条被看到」的直接体现，
  也意味着**女巫不渗透就没有任何存在感**（渗透靠 Infiltrator / Endorser 场景）。
- **收益方（Promoted）行为与诚实账户完全一致**，只是内容质量极低（0.05~0.15）。
  攻击的唯一作用就是把它抬起来——这样「收益方优势」才能干净地度量攻击效果。
- 潜伏者/渗透者/背书者对被推广账户的互动权重乘 `attack_focus`（默认 4），
  保证它们在有限步数内确实会跟收益方发生交互，而不是靠运气。

---

## 5. 指标定义

| 指标 | 定义 | 备注 |
|---|---|---|
| 账户数 `accounts` | 账户表大小 | 只增不减 |
| 存活/在线/休眠/离开 | 生命周期计数 | `online ⊆ alive ⊆ accounts` |
| 边数 `follow_edges` / `rating_edges` | 关注边 / 评分边 | 评分边 ≤ 关注边 ≤ 账户数 × max_follows |
| 各类平均被评分 `classes[i].mean_received` | 该类账户收到的评分按条数加权平均 | 是**原始评分输入**，不是声望 |
| 社团内/间均分 `intra_mean` / `inter_mean` | 按观察者与被评者是否同社团切分 | 衡量兴趣匹配带来的偏好 |
| 圈子化 `cliquishness` | 社内均分 ÷ 社间均分 | 没有社间评分时为 0（无定义） |
| 社团内边占比 | 社内关注边 ÷ 全部关注边 | 同质性实验的主指标 |
| 评分分布 | 全部评分的 count/mean/std/分位数 | 分位数用线性插值 |
| 攻击者优势 `attacker_advantage` | 攻击类均分 ÷ 诚实均分 | 不含 Promoted |
| 收益方优势 `target_advantage` | 收益方均分 ÷ 诚实均分 | 收益方至少收到 5 条评分才计算，否则记 0 |
| 观察者胜率 | 同时评过两类人的观察者中，「给诚实均分 > 给攻击者均分」的比例 | `observers_compared = 0` 表示没有可比观察者 |
| 质量-评分相关 | 诚实账户「质量 ↔ 收到均分」的皮尔逊相关 | 至少 3 条评分才计入 |
| 影响力基尼 | 存活账户「收到的评分之和」的基尼系数 | 声望权重由信任算法再算，这里只到原始影响力 |

---

## 6. 复现性

1. 随机数自实现 xorshift64*，全部种子由 `cfg.seed` 派生；
2. 全程不使用 `HashMap`（std 的 HashMap 有随机哈希种子，会破坏复现），只用 `Vec`；
3. 所有遍历按内部编号升序，阶段顺序固定；
4. 浮点运算顺序固定，不依赖并行或迭代器的哈希顺序。

测试 `tests/determinism.rs` 验证：同种子两次运行的**评分明细逐位相同**、
**每份历史快照逐位相同**、逐步推进与一次跑完等价。

---

## 7. 示例输出（200 账户 × 200 步，seed = 7）

```text
═══ 基线：200 账户 × 200 步 ═══
配置：初始 200 账户，200 步（小时），社团 4，同质性 0.60，桥接比例 0.12，加入率 0.15/h，攻击 无攻击
=== 快照 · 第 200 步 / t = 200h ===
账户 237 ｜ 存活 225 ｜ 在线 109 ｜ 休眠 30 ｜ 离开 12 ｜ 桥接 28
关注边 2237 ｜ 评分边 1854 ｜ 人均关注 9.94

[分类账户]
honest      账户  237 ｜ 存活  225 ｜ 在线  109 ｜ 质量 0.529 ｜ 收评分  1854（均分 0.519）｜ 给评分  1854（均分 0.519）｜ 声誉 0.433
...

[社团结构]
社团 4 ｜ 关注边：内 1553 / 间 684（内占比 0.694）
评分：内 1292 / 间 562 ｜ 社内均分 0.527 ｜ 社间均分 0.500 ｜ 圈子化 1.053

[评分分布]
样本 1854 ｜ 均值 0.519 ｜ 标准差 0.200
最小 0.088 ｜ p10 0.274 ｜ p25 0.370 ｜ 中位 0.492 ｜ p75 0.640 ｜ p90 0.805 ｜ p99 1.000 ｜ 最大 1.000

[攻防指标]
攻击者均分 0.000 ｜ 攻击者/诚实 0.000 ｜ 收益方均分 0.000（0 条）｜ 收益方优势 0.000
观察者胜率 0.000（0 位观察者）｜ 质量-评分相关 0.923 ｜ 影响力基尼 0.387

累计：加入 37 人，流失 12 人，休眠 39 次
评分总量 1854 条，平均每人给出 7.82 条
```

攻击对比（同样的 200 × 200，`attack_ratio = 0.1`）：

```text
scenario         ratings   honest  attacker     a/h     target(n)   t-adv       win(n)
none                1854    0.519     0.000   0.000      0.000(0)   0.000     0.000(0)
sybil x3            1950    0.510     1.000   1.960      1.000(6)   1.960     0.000(0)
sleeper 60h         2044    0.512     0.506   0.988     0.792(20)   1.545   0.580(119)
infiltrator         1871    0.478     0.722   1.511     0.781(26)   1.635     0.091(165)
endorser 20%        1825    0.456     0.671   1.473     0.894(45)   1.962     0.066(198)
spammer             2174    0.561     0.204   0.363      0.796(1)   0.000     0.887(62)
```

读法：

- `sybil`：女巫只在集群内部互评满分，所以它们的原始均分是 1.0；
  但因为正常人根本发现不了它们，**没有任何观察者同时评过两类人**（win(0)），
  女巫在诚实观察者的原始评分数据里是**完全缺席**的。
- `sleeper`：12 个潜伏者的类别均分和诚实账户几乎一样（0.988）——潜伏期无法从分数上识别；
  但收益方优势 1.545，说明「长期正常、突变后刷分」确实能把目标抬起来。
- `infiltrator` / `endorser`：攻击者本身是高声誉账户，类别均分更高；
  只有 9.1% / 6.6% 的观察者还认为诚实账户更好，攻击效果最明显。
- `spammer`：垃圾账户均分只有诚实账户的 36%，刷量不等于刷分。

同质性扫描：

```text
h         intra    inter    share      avg
0.00        387     1754    0.181     9.91
0.25        712     1387    0.339     9.72
0.50       1363      817    0.625     9.73
0.75       1674      497    0.771    10.24
1.00       2093      151    0.933    10.02
```

`homophily` 从 0 到 1，社团内关注边占比 18% → 93%，单调上升。

---

## 8. 设计取舍与已知局限

1. **女巫对正常人不可见**：这是刻意的建模选择（见 4 节）。代价是「女巫攻击是否有效」
   不会体现在原始评分里，需要用 Infiltrator/Endorser 场景来测渗透。
   如果要做「女巫直接渗透」，把 `World::draw_from_pool` 里跳过女巫的那两行删掉即可。
2. **评分不带时间戳**：协议里衰减作用在「别人的观点」而不是「我的评分」上，
   而观点的刷新时间由信任引擎自己记录，所以仿真只需要给出当前评分集合。
   世界时钟 `world.time` 与 `world.history` 足以做时间序列分析。
3. **每个观察者对被评者只保留一条最新评分**：符合「正分清单」的语义（一个账户一项）。
   历史轨迹要靠 `snapshot_every` 记快照，而不是记每次评分事件。
4. **评分上限截断**：质量高、兴趣匹配高、噪声大时乘积会超过 1，被截断成 1.0。
   这会让分布顶端出现一小撮满分（p99 = 1.0），是「满分」语义的正常结果，不是 bug。
5. **不含负分**：协议明确砍掉负分（避免负分攻击与 DDoS），本仿真严格遵守。
   所以「已识别的坏人」只能通过不打分（0）来冷处理，不能扣分。
6. **冷启动没有曝光机制**：新账户只能靠自己主动关注别人进入网络，
   在别的账户眼里它一开始是 0。这与当前设计规格一致（冷启动是产品层问题）。
7. **内容层缺失**：只有「内容质量」这个标量，"帖子"不存在，也没有排序与曝光。
   垃圾攻击因此只能体现为「高活跃 + 乱打分 + 被低分」，测不到曝光挤占。
8. **声誉反馈是开关而不是算法**：默认关闭，避免干扰「质量 → 评分」的基本检验。

---

## 9. 测试清单（114 个，全部通过）

| 文件 | 数量 | 覆盖 |
|---|---|---|
| `src/*.rs` 单元测试 | 66 | 随机数分布、兴趣向量、社团公式、配置夹紧、评分公式、指标计算、世界不变式 |
| `tests/determinism.rs` | 6 | 同种子逐位一致、换种子不同、逐步=一次跑完、历史快照间隔、构造一致 |
| `tests/sybil.rs` | 7 | 女巫只对同集群打满分、外部零给分、内部均分 1.0、集群规模、比例为零 |
| `tests/attacks.rs` | 6 | 潜伏者突变前后、渗透者按声誉挑选与收买时刻、背书者按质量挑选、垃圾低分 |
| `tests/homophily.rs` | 5 | 0→1 显著提升、单调、桥接提高跨社团、单社团全内边、社团数效应 |
| `tests/quality.rs` | 4 | 质量-均分强相关、评分落在 (0,1]、兴趣匹配影响、去噪后单调 |
| `tests/population.rs` | 7 | 加入/离开、休眠回归、离开永久、加入时间、账户上限、节律开关 |
| `tests/longrun.rs` | 4 | 1000 步稳定有界、极端参数不崩、零步长、边数不爆炸 |
| `tests/metrics_consistency.rs` | 8 | 快照与账户表逐项重算一致、分类/社团/分布、评分明细、在线状态 |
| `doc` | 1 | 文档示例可运行 |

关键判据（与任务书一致）：

- 固定 seed 两次结果**逐位**相同（不是「近似相同」）；
- 女巫的每一条出边都指向同集群、且分数恰好是 1.0；
- `homophily` 0 → 1 时，社团内关注边占比提升 **> 0.30**（实测 0.18 → 0.93）；
- 无攻击时，诚实账户「质量 ↔ 收到均分」相关 **> 0.75**（实测 0.92），
  质量前 1/4 的均分 > 后 1/4 的 1.3 倍；
- 1000 步后账户数 ≤ `max_accounts`、评分边 ≤ 账户数 × `max_follows`、
  内存估计 < 64 MB（实测约 50 KB 级）。

---

## 10. 与任务书的对应

| 任务书要求 | 实现位置 |
|---|---|
| 人口动态：加入时间、离开概率、休眠与回归 | `World::phase_lifecycle`、`Account::joined_at`、`Life` |
| 兴趣：8 维归一化向量、余弦加权、同质性 0~1 | `interest.rs`、`World::draw_from_pool` |
| 结构：社团划分、桥接比例可调 | `community.rs`、`Config::bridge_ratio` |
| 活跃度：活跃时段 + 幂律强度 | `World::spawn`（Pareto）、`Account::is_online` |
| 行为：质量 × 匹配 × 噪声 → [0,1] | `behavior::content_score`、`World::compute_rating` |
| 声誉反馈开关（马太效应） | `reputation_feedback`、`Account::reputation` |
| 五类攻击者 | `config::AttackKind`、`World::install_attack` |
| 指标：账户/边/在线、分类均分、社团内间、分布 | `metrics::Snapshot` |
| 固定种子可复现 | 第 6 节 |
