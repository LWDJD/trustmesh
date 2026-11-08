# 运行时架构

> 配套代码：`src/runtime.rs`
> 架构图：见对话中的分层示意图

---

## 1. 分层

```
应用层     社交 / IPFS / DHT / 自定义
    ↓ rate() query()
运行时     tick() on_message() take_outbox()
    ↓
核心层     RatingList · Sketch · PPR
传输层     Transport trait（开发者实现）
```

### 1.1 核心层

纯计算，无 IO，无网络，无时间依赖。所有接口都是同步的。

- `RatingList`：评分清单、线性衰减、按权重抽样
- `Sketch`：压缩、合并
- `local_push`：PPR 近似
- `presets`：四套实测参数

### 1.2 运行时层

单节点的事件循环，单线程，无锁，不依赖 async runtime。

```rust
pub struct Runtime {
    id: PeerId,
    list: RatingList,
    sketch: Sketch,
    peer_summaries: HashMap<PeerId, HashMap<AccountId, f32>>,
    outbox: Vec<Action>,
    ...
}

impl Runtime {
    pub fn rate(&mut self, target: AccountId, score: f64);
    pub fn tick(&mut self, now: u64);
    pub fn on_message(&mut self, from: PeerId, msg: Message);
    pub fn take_outbox(&mut self) -> Vec<Action>;
    pub fn query(&self, target: AccountId) -> f64;
}
```

运行时不做网络操作，只生产 `Action`。

### 1.3 传输层

```rust
pub trait Transport {
    fn send(&mut self, to: PeerId, msg: Message);
    fn poll(&mut self) -> Option<(PeerId, Message)>;
}
```

两种内置实现：

- `MemoryTransport`：进程内测试
- 其余由开发者实现：HTTP、WebSocket、libp2p、文件、串口

### 1.4 外部驱动

```rust
pub fn run_loop<T: Transport>(rt: &mut Runtime, transport: &mut T, ticks: u64)
```

内部顺序：tick → 发消息 → 收消息。

三种驱动方式：

| 方式 | 适合 |
|---|---|
| tokio 定时器 | 服务器 |
| 独立线程 + sleep | 桌面 |
| 裸循环（no_std） | 嵌入式 |

---

## 2. 消息协议

```rust
pub enum Message {
    Publish { owner, buckets, count, at },
    RequestSummary { target, at },
    SummaryResponse { target, value, at },
    Ping { at },
}
```

### 2.1 Publish

节点定期广播自己的评分摘要（压缩后的桶）。接收方缓存。

频率由 `publish_every` 控制，按 tick 计。

### 2.2 RequestSummary

查询某个账户时，向对端请求摘要值。

### 2.3 SummaryResponse

回应摘要请求，带自己的 trust 值。

### 2.4 消息序列化

当前用 `Vec<f32>` 直接传递，跨进程需要编码。建议：

```rust
// 桶 → 小端 f32 数组
fn encode_buckets(b: &[f32]) -> Vec<u8>;
fn decode_buckets(b: &[u8]) -> Vec<f32>;
```

体积：`buckets × hashers × 4` 字节。2048 桶、5 哈希时 40 KB。

---

## 3. 数据流

### 3.1 定时

```
tick(now)
  ├─ 抽样：从 RatingList 抽 samples_per_tick 个
  ├─ 衰减：线性，逐条计算
  └─ 定期发布：到达 publish_every 时生成 Publish
```

### 3.2 查询

```
query(target)
  ├─ 本地：RatingList 中 target 的衰减权重
  ├─ 群体：peer_summaries 中 target 的平均值
  └─ 合成：β × 本地 + (1-β) × 群体
```

### 3.3 入站

```
on_message(from, msg)
  ├─ Publish → 缓存到 peer_summaries
  ├─ RequestSummary → 生成 SummaryResponse 到 outbox
  └─ SummaryResponse → 缓存
```

---

## 4. 轻量化的具体做法

| 做法 | 收益 |
|---|---|
| 无 async 依赖 | 二进制约 300KB，无 tokio 运行时开销 |
| 单线程无锁 | 无 Arc/Mutex，无竞态 |
| 核心纯函数 | 可单测，可复现 |
| outbox 模式 | 网络由外部控制，运行时阻塞时间为零 |
| 固定大小 sketch | 内存可预测，无动态分配抖动 |

目标：release 构建 < 1MB，单 tick < 1ms（N=1000）。

---

## 5. 测试策略

### 5.1 单元测试（`src/*.rs` 的 `#[cfg(test)]`）

| 模块 | 覆盖点 | 数量 |
|---|---|---|
| `rating_list` | 线性衰减、抽样、容量淘汰 | 6 |
| `sketch` | 常量大小、精确查询、线性合并 | 4 |
| `runtime` | 记录、抽样、发布、消息、双节点 | 6 |
| `presets` | 默认值、构造、体积、误差 | 5 |
| `policy` | 三种策略、体积计算 | 4 |
| `dense` | 高斯消元 | 2 |
| `rng` | 确定性、范围 | 2 |

### 5.2 集成测试（`tests/`）

| 文件 | 内容 |
|---|---|
| `validation.rs` | 精确解与近似解对比、单调性、有界性 |
| `scope.rs` | 抽样收敛、合并、模糊化 |
| `sketch_scale.rs` | 规模无关性、稠密误差 |

### 5.3 基线测试（`src/bin/baseline_compare.rs`）

精确迭代 vs 按评分加权抽样，测误差和排序一致率。

### 5.4 网络模拟（`src/bin/fullsim.rs`、`large_scale.rs`）

动态网络、攻击场景、规模扫描。

### 5.5 攻击测试（`src/bin/neg_attack.rs`、`vouch.rs`）

负权重攻击、介绍人认证传播。

### 5.6 不变量

| 不变量 | 检查方式 |
|---|---|
| `trust ∈ [-1, 1]` | 属性测试 + 断言 |
| 衰减单调递减 | 单元测试 |
| 合并线性 | 单元测试 |
| 抽样概率正比权重 | 统计测试 |
| 精确解满足不动点 | 残差 < 1e-10 |

---

## 6. 部署形态

### 6.1 单进程（桌面/服务器）

```
进程
 ├─ Runtime
 ├─ Transport: TCP/WS
 └─ 驱动: tokio 定时器
```

### 6.2 多进程（去中心网络）

每个节点一个进程，通过任意传输通信：

```
节点 A                节点 B
Runtime A  ←─ 网络 ─→  Runtime B
```

运行时不知道对端是谁，只知道 `PeerId`。

### 6.3 嵌入式

```
MCU
 ├─ Runtime（no_std 可裁剪）
 ├─ Transport: LoRa / BLE
 └─ 驱动: 主循环
```

需要关闭 `std` 特性，把 `HashMap` 换成固定容量数组。

---

## 7. 已知限制

| 限制 | 影响 | 缓解 |
|---|---|---|
| 消息无认证 | 任何人都能伪造 Publish | 需要签名层 |
| 无重放保护 | 旧消息可以重放 | 需要时间戳 + nonce |
| 摘要只存单值 | 无法逐目标查询 | 改为存完整桶 |
| 无持久化 | 重启丢状态 | 需要 `Store` trait |
| 传输无背压 | 慢消费者堆积 | 需要 outbox 上限 |

这些是 v1 的边界，不是 bug。
