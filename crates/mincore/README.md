# `tm_mincore` —— 最小反女巫内核

> 744 行生产代码，只用 `std`，零外部依赖。
> 它是 [`tm_engine`](../engine) 的**另一种视图表示**：定长稀疏表替代 Count Sketch。

## 0. 一句话

**同一个算法，换一种存法。** 视图从 40,960 字节降到 770~3,074 字节，换来：
女巫精确 0、预算恒 1、空账户精确 0、逐位确定性，代价是 cap 截断。

---

## 1. 它是什么

~~~text
个人正分清单（本地精确、无压缩）  ──┐
                                    ├─→ β 混合 ─→ 截断到 cap ─→ 定长编码 ─→ 发布
收到的别人的观点（每条 cap×6+2 B） ──┘
~~~

接收方 `decode` 一条视图（**字节数恰好** `cap×6+2`），回答「某个账户在我眼里有多少声望」。

### 1.1 与现行 `tm_engine` 的关系

两者是**同一种算法**（正分清单 + 群体项 + 衰减 + β 混合 + 全局归一化），只在「视图怎么存」上不同：

| | `tm_engine`（现行） | `tm_mincore` |
|---|---|---|
| 视图表示 | Count Sketch，`2048×5` f32 | 定长稀疏表，`cap` 条 `(u32 acc, u16 q)` |
| 视图字节数 | **40,960 B**（固定） | **770 / 1,538 / 3,074 B**（cap=128/256/512） |
| 总量估计 | `total_mass` 依赖接收方账户表长度 | **`total_q` 是精确 u32 整数和** |
| 空账户 | 有噪声底（S4 出现率 0.52%） | **精确 0** |
| 超量声明 | Σv 可达 K | **恒 1** |
| 逐位确定性 | 0/20 seed | **20/20 seed** |
| 截断 | 无（估计器天然有损） | **有**（cap 条以外的被丢） |

**关键洞察**：Count Sketch 是「压缩有界随机变量」，定长表是「截断精确值」。
前者的误差是**噪声**（不可预测），后者的误差是**截断**（只丢低分的、确定性的）。
反女巫场景里，「确定性的零」比「期望为零」有用得多 —— 这就是 S1 从 2.46% 变 0 的原因。

---

## 2. 怎么用

~~~rust
use tm_mincore::{Config, Node, Time};

let cfg = Config { cap: 256, ..Default::default() };   // 默认 cap=512，这里显式改成 256
let mut me = Node::new(cfg, 0);   // 第二个参数是抽样用的 seed

// 1. 给自己认识的人打分（本地精确存储，不压缩）
me.rate(alice, 0.9f32, now);
me.rate(bob,   0.4f32, now);

// 2. 发布：把「我的清单 + 我收到的观点」β 合成，截断到 cap，编码成定长字节
let view = me.publish(now);    // SparseView
let bytes = view.encode();     // Vec<u8>，长度恰为 cap*6+2；发给别人

// 3. 收到别人的视图
let incoming = SparseView::decode(&bytes, cfg.cap).ok_or("非法视图")?;
me.on_view(owner, incoming, published_at, now);  // 返回 bool：是否接受

// 4. 查任意账户在我眼里的声望
let d = me.distribution(now);
let score = me.query(someone, now);
~~~

完整可跑的例子见 `examples/`；17 个验收测试见 [`tests/anti_sybil.rs`](tests/anti_sybil.rs)。

---

## 3. 定长编码格式

> 两种编码（旧格式 / 带前缀格式）的完整规范、互不误判的同余类分析、以及
> 「为什么必须精确长度匹配」见 [`ENCODING.md`](ENCODING.md)。本节只讲旧格式的定长体。

~~~text
offset 0            : 槽位 0  ── 4 B 账户号(u32 LE) + 2 B 分数(u16 LE)
offset 6            : 槽位 1
...
offset (cap-1)*6    : 槽位 cap-1
offset cap*6        : 条数 len（小端 u16，2 B）
总长度              : cap*6 + 2
~~~

### 3.1 规范形式（`decode` 会强制检查）

1. **槽位严格升序**（按账户号）—— 保证 `q_of` 能二分查找，且编码唯一
2. **分数为 0 的槽位必须省略**（不写、不占位），`len` 之后全填零
3. **`len <= cap`**，`len` 之后的字节**必须全零**（否则拒绝）

违反任何一条 ⟹ `decode` 返回 `None`。这让「同一份分布」只有**唯一一种**字节表示，
所以跨节点比较可以逐字节做，不需要语义比较。

### 3.2 cap 自描述：两种编码并存

**旧格式** `encode()` / `decode(bytes, cap)` 不自描述 cap（长度 40,960 B 的 Count Sketch 同理）：

| 发送方 | 接收方 | 旧格式 | 前缀格式 |
|---|---|---|---|
| cap=256（1 538 B） | cap=256 | ✅ 正常 | ✅ |
| cap=256（1 538 / 1 540 B） | cap=128（期望 770 B） | ❌ **长度不符 ⟹ 整条拒绝 ⟹ 失去联系** | ✅ 按前缀解成 cap=256 |

⟹ 只用旧格式时，**网络会按 cap 分裂成互不通信的孤岛**。

**带前缀格式**（2026-11-19 新增）在旧负载前面多写 2 字节 cap，接收方不需要预先知道它：

```rust
let bytes = view.encode_prefixed();          // 长度 cap×6+4
let back  = SparseView::decode_prefixed(&bytes)?;   // cap 从头部读

// 可诊断版：失败时说出具体理由（BadCap / BadLength / BadCount / ...）
use tm_mincore::DecodeError;
match SparseView::decode_prefixed_result(&bytes) {
    Ok(v) => { /* v.cap() 就是发送方用的 cap */ }
    Err(DecodeError::BadCap) => { /* 前缀里的 cap 非法 */ }
    Err(e) => { /* e.as_str() 给出短标签 */ }
}
```

* `encode_prefixed()[2..] == encode()`：**定长体一个字节都没动**，所以现有 128 个测试、
  wasm 金标准快照、`tools/mincore-wasm` 全部不受影响；
* 两种格式**互不误判**，理由是总长落在模 6 的不同同余类（旧 `≡ 2`、新 `≡ 4`），
  **不是**前 2 字节的语义 —— 完整分析、以及「为什么分发必须精确长度匹配、
  为什么第三种格式必须加带内魔数」见 [`ENCODING.md`](ENCODING.md)；
* 接收方仍应自己施加资源上限（「我只接受 cap ≤ 我允许的最大值」）；
  `decode_prefixed` 内部已把 `cap > MAX_CAP` 挡掉，最坏分配 24 580 字节;
* `Config::cap` 现在只是**本机发布用的 cap**，不再是全网协议约定。

---

## 4. cap 该设多大

完整实测见 [`审计/cap截断损失曲线.md`](../../审计/cap截断损失曲线.md)。结论：

| 你的关注上限 | 建议 cap | 字节 | 截断损失 |
|---|---:|---:|---|
| ≤ 128 | 128 | 770 B | 零 |
| ≤ 256 | **256** | 1,538 B | 零 |
| ≤ 512 | 512 | 3,074 B | 零 |
| 想省流量 | 128 | 770 B | 20.1% 视图被截，平均丢 2.81% |

**经验规则**：`cap >= 你的关注上限` ⟹ 零损失。少于则按 `(score desc, acc asc)` 截断，只丢低分的。

---

## 5. 它**没有**什么（与 engine 的差距）

这是原型，不是生产实现。缺：

| 缺什么 | 影响 |
|---|---|
| 状态快照（`SNAPSHOT_VERSION`） | 不能持久化/重启恢复 |
| `max_views` 等资源上限 | 恶意节点可塞无限条视图 ⟹ **内存耗尽** |
| non-finite 防护（含 `untrusted_bytes` 那套） | `decode` 只查规范性，不查 f32 是否有限（u16 定点天然有限，所以这条其实安全） |
| badlist / 判据 / 抽样预算 / `recommended_k` | 那些是 `tm_engine` 的上层机制 |
| 协议文档 | 本文件 + [`ENCODING.md`](ENCODING.md)（字节编码规范） |
| wasm32 对拍 | **正在补**（`tools/mincore-wasm`） |
| 单元测试 | **正在补**（6 个模块当前零单测） |

⟹ **不要现在把 engine 换成它。** 两者并存，用 S1~S6 长期对照。
替换的正确时机见 [`审计/威胁模型.md` §8.4.4](../../审计/威胁模型.md)。

---

## 5.5 相关文档

| 文档 | 讲什么 |
|---|---|
| [`PROTOCOL.md`](PROTOCOL.md) | 接入协议：内核保证什么/不保证什么、每轮四步、eclipse 防护、部署检查 |
| [`DEPLOY.md`](DEPLOY.md) | 上线前逐条打勾的清单（含 cap 实测表、读数消费的坑） |
| [`ENCODING.md`](ENCODING.md) | 字节格式（补齐中：cap 自描述头） |
| [`../../审计/威胁模型.md`](../../审计/威胁模型.md) §8.4 | 与 engine 的对照、5 项缺口、补完之后仍缺什么 |
| [`../../审计/cap决策.md`](../../审计/cap决策.md) | cap 该设多大（两个探针的实测） |
| [`../../审计/mincore-rng种子缺陷.md`](../../审计/mincore-rng种子缺陷.md) | 一个「不影响数字但确实错了」的 bug 复盘 |
## 6. 验证命令

~~~powershell
$env:TMP="<仓库根>/.tmp"; $env:TEMP=$env:TMP
cd crates/mincore

cargo test                    # 17 集成 + 1 doctest
cargo clippy --all-targets    # 零 warning
cargo fmt --check             # 零差异

cargo run --release --bin a12_compare   # S1~S6 vs engine（20 seed，约 1 分钟）
cargo run --release --bin a12_perf      # 性能/内存对照
cargo run --release --example cap_loss  # cap 截断损失曲线
~~~
