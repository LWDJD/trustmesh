# tm_view

trustmesh 的「压缩观点」：用 Count Sketch 把「我给每个账户的评分」压成**定长**草图。
纯 std，无任何外部依赖（`#![forbid(unsafe_code)]`）。

- **定长**：默认 2048 桶 × 5 个哈希 × f32 = 40 KiB，与账户数无关。
- **可查询**：任意 32 字节公钥 → 近似评分。
- **可合并**：逐元素加权线性相加，直接对应「声望 = β×个人项 + (1−β)×群体项」。
- **可实测**：`noise_floor` 用全新 key 查询，给出 median / p95 / max。

## 快速开始

```rust
use tm_view::{AccountKey, CompressedView, ViewParams};

let params = ViewParams::default();          // 2048, 5, 0
let mut view = CompressedView::new(params);
let alice: AccountKey = [1u8; 32];
view.add(&alice, 0.8);
assert!((view.query(&alice) - 0.8).abs() < 0.02);

let mut other = CompressedView::new(params);
other.add(&alice, 0.2);
view.merge(&other, 0.75, 0.25);             // 0.75*0.8 + 0.25*0.2 = 0.65
assert!((view.query(&alice) - 0.65).abs() < 0.02);

let wire = view.encode();                    // 恰好 40_960 字节
let back = CompressedView::decode(&wire, params).unwrap();
assert_eq!(back.query(&alice), view.query(&alice));
```

## API

| 项目 | 说明 |
| --- | --- |
| `ViewParams { buckets, hashers, seed }` | 形状参数。默认 2048 / 5 / 0；参数是全网约定 |
| `CompressedView::new(params)` | 全零视图；参数非法时 panic |
| `CompressedView::from_pairs(params, &[(key, score)])` | 从评分表构造 |
| `add(&key, score)` / `add_pairs(...)` | 增量写入；0 与 NaN/∞ 被忽略 |
| `query(&key) -> Score` | 取 `hashers` 个桶估值的中位数 |
| `merge(&other, w_self, w_other)` | `self = w_self*self + w_other*other`，逐元素 |
| `bytes() -> usize` | 恒为 `buckets * hashers * 4` |
| `encode() / decode(&[u8], params)` | 定长小端 f32 负载；decode 校验长度并拒绝 NaN/∞ |
| `noise_floor(samples, seed) -> NoiseStats` | 随机新 key 的噪声幅度统计 |
| `NoiseStats { median, p95, max }` | 噪声幅度的中位数 / 95 分位 / 最大值 |

辅助类型：`AccountKey = [u8; 32]`、`Score = f32`、`XorShift64`（确定性采样）。

## 结构

草图是 `hashers × buckets` 的 f32 矩阵，行优先。写入 `(key, score)` 时，
第 j 行用第 j 个独立种子把 key 映到桶，并按另一条独立哈希的最高位取 ±1 符号，
累加 `±score`。查询把桶值乘回符号，取 `hashers` 个估值的**中位数**。

定桶：带种子的 FNV-1a → splitmix64 终混合 → `% buckets`。
定符号：另一条带盐的 FNV-1a 取最高位（FNV 低位的奇偶性与 key 相关，不能用）。

## 编码格式

`encode()` 输出恰好 `bytes()` 字节：按行优先顺序把每个 f32 写成小端 4 字节。
**没有头部**，参数（buckets/hashers/seed）由通信双方带外约定；这样负载长度严格等于
`buckets × hashers × 4`。`decode(bytes, params)` 校验：

- 参数合法且长度恰好等于 `params.buckets * params.hashers * 4`；
- 每个 f32 都是有限值（拒绝 NaN/∞，一条坏数据会污染所有查询）。

参数不匹配但长度碰巧一致时无法自检——这是不带头的代价。

## 实测噪声（默认 2048×5）

评分在 [0.1, 1.0] 均匀分布；视图种子 0，评分表种子 1；
每个视图用 2000 个全新 key 测噪声（采样种子 42）。
「平均误差」是对**已写入 key** 的 `|query − score|` 平均值。

| 写入条数 | median | p95 | max | 平均误差 | 体积 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 0.0000 | 0.0000 | 0.0000 | 0.00000 | 40 KiB |
| 100 | 0.0000 | 0.0000 | 0.0000 | 0.00000 | 40 KiB |
| 1000 | 0.0000 | 0.3202 | 0.9998 | 0.03418 | 40 KiB |
| 10000 | 0.4584 | 1.3671 | 2.4574 | 0.54766 | 40 KiB |

为什么 1000 条的 median 是 0：2048 个桶装 1000 条，平均每桶约 0.49 条，
88.7% 的全新 key 查询**恰好**精确（5 个带符号估值里空桶占多数，中位数落在 0）。
这恰恰说明视图在稀疏时接近精确；碰撞只体现在尾部（p95 ≈ 0.32）。
10000 条时平均每桶约 4.9 条，只有约 1.6% 的查询精确，median 才变成 0.46。

写入量固定为 10000 时，桶数的影响：

| buckets | median | p95 | max | 平均误差 | 体积 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1024 | 0.6664 | 1.9907 | 4.7280 | 0.79142 | 20 KiB |
| 2048 | 0.4584 | 1.3671 | 2.4574 | 0.54766 | 40 KiB |
| 4096 | 0.2872 | 0.9614 | 2.0604 | 0.34838 | 80 KiB |
| 8192 | 0.0000 | 0.6899 | 1.5621 | 0.17092 | 160 KiB |

8192 桶时超过一半的查询精确落在 0，所以 median 又回到 0；看 p95 与平均误差才知道
真实噪声（0.69 / 0.17）。

## 设计取舍

- **f32 而不是 f64**：每桶 4 字节，默认 40 KiB；评分是 [0,1]，噪声量级远大于 f32
  精度误差。传输体积减半。
- **中位数而不是平均值**：5 个哈希允许 2 个桶被污染；平均值会让一次碰撞直接偏移结果。
  代价是带符号估值的中位数会出现 0 原子（稀疏时是优点，密集时少量样本恰好为 0）。
- **FNV-1a + splitmix64 终混合**：FNV 简单、稳定、无平台差异；但低位雪崩弱，
  而桶数是 2 的幂、取模只看低位，所以定桶前先过 fmix64。实测 10000 个 key 在
  8192 桶上占用 5749 个桶（理论期望 5775），最大桶 7 条。
- **每个 hasher 独立种子**：由 `seed ^ (j+1)*黄金比例` 过 splitmix64 派生；
  两条 FNV（定桶 / 定符号）保证 5 行噪声不同向叠加。
- **零与坏值**：0 表示没表态，`add` 直接跳过；NaN/∞ 也跳过；重复 key 线性累加。
  协议层负责把评分限制在 [0,1]，本 crate 不截断。
- **合并是桶级线性**：`merge` 严格逐元素加权；`query` 取中位数不是线性运算，
  有碰撞时 `merged.query(k)` 不等于两次查询的加权和，无碰撞时相等。
- **参数错误用 panic**：`new` / `merge` 的非法参数属于编程错误（维度必须为正，
  `buckets × hashers ≤ 2^26`，`hashers ≤ 4096`）；
  `decode` 面对的是网络输入，用 `None` 拒绝，绝不 panic。

## 测试与示例

```sh
cargo test                       # 49 个测试：15 单元 + 24 结构 + 9 噪声 + 1 文档
cargo test -- --nocapture        # 查看测试里的实测打印
cargo run --example noise_report # 打印噪声底与桶数扫描
```

测试覆盖：空视图、单条精确、100 条误差、密集碰撞误差、`bytes()` 恒定、
merge 线性与参数检查、编码往返与坏负载拒绝、重复 key、0/NaN/∞、单桶单哈希、
偶数 hasher、参数 panic、不同 seed 同量级、噪声可复现、统计量有序。

## 文件

- `src/lib.rs`：公开 API、Count Sketch 核心、编码、噪声统计
- `src/hash.rs`：FNV-1a、splitmix64 终混合、种子派生
- `src/rng.rs`：xorshift64
- `tests/view.rs`：结构与边界测试
- `tests/noise.rs`：噪声底实测
- `examples/noise_report.rs`：可运行的噪声报告
