# 视图字节编码：两种格式

> 本文是 `tm_mincore` 的**格式规范**。源码见 [`src/view.rs`](src/view.rs)；
> 与引擎侧 Count Sketch 格式的关系见 [`README.md`](README.md) §3。

## 0. 一句话

视图有两种编码：**旧格式**（`encode` / `decode(bytes, cap)`，不带 cap，接收方必须带外知道它）
和**带前缀格式**（`encode_prefixed` / `decode_prefixed(bytes)`，前 2 字节自带 cap）。
旧格式原样保留（128 个现有测试、12 张 wasm 金标准都依赖它）；带前缀格式是**新增**的，
用来解掉「网络按 cap 分裂成互不通信的孤岛」。

两种格式**互不误判**——但理由不是「前 2 字节的语义」，而是**总长落在模 6 的不同同余类**。
这条安全完全依赖「两个解码器都要求长度严格相等」，详见 §4。

---

## 1. 共性：定长体

一份 `SparseView`（`cap` 个槽位）的**定长体**恒为 `cap × 6 + 2` 字节：

~~~text
offset 0                : 槽位 0  ── 4 B 账户号(u32 LE) + 2 B 分数(u16 LE)
offset 6                : 槽位 1
...
offset (cap-1)*6        : 槽位 cap-1
offset cap*6            : 条数 len（小端 u16，2 B）
总长度                  : cap*6 + 2
~~~

常量：`SLOT_BYTES = 6`、`VIEW_OVERHEAD = 2`、`MAX_CAP = 4096`（`src/view.rs` 顶部）。

### 1.1 规范形式（两个解码器都强制检查）

1. **槽位按账户号严格升序**（不是「非降」；相等 = 重复 = 非法）
2. **分数为 0 的槽位必须省略**（不写、不占位），`len` 之后**必须全零**（禁止夹带）
3. **`len <= cap`**

违反任何一条 ⟹ 拒绝。这让「同一份分布」只有**唯一一种**字节表示，
跨节点比较可以逐字节做，不需要语义比较（S6 逐位确定性的字节级落点）。

| cap | 定长体字节数 |
|---|---:|
| 1 | 8 |
| 2 | 14 |
| 128 | 770 |
| 256 | 1 538 |
| 512 | 3 074 |
| 4096 | 24 578 |

---

## 2. 旧格式（`encode` / `decode`）

~~~text
[0, cap×6)        定长体（见 §1）
总长              cap×6 + 2   ⟹   L ≡ 2 (mod 6)
~~~

* `SparseView::encode() -> Vec<u8>`，长度恒为 `bytes() = cap×6+2`；
* `SparseView::decode(bytes: &[u8], cap: usize) -> Option<SparseView>`，
  **cap 由调用方带外给出**。`cap == 0 || cap > MAX_CAP` 或长度不**恰好**相等 ⟹ `None`。

**适用场景**：双方已约定同一个 `cap`（同构网络、内部快照、wasm 金标准对拍）。
**不适用**：任何「节点的 cap 可以自己设」的拓扑 —— 见 §3。

---

## 3. 带前缀格式（`encode_prefixed` / `decode_prefixed`）

~~~text
offset 0           : cap（小端 u16，2 B）
offset 2           : 定长体（与 §1 **逐字节相同**，长度 cap×6+2）
总长度             : 2 + cap×6 + 2 = cap×6 + 4   ⟹   L ≡ 4 (mod 6)
~~~

* `SparseView::encode_prefixed() -> Vec<u8>`，长度恒为 `prefixed_bytes() = bytes() + 2`。
  `cap` 必须落在 `[1, 65535]` 才写得进 u16 —— 而 `MAX_CAP == 4096`，所有合法构造路径都满足。
* `SparseView::decode_prefixed(bytes) -> Option<SparseView>`：cap 从头部读，**不要**调用方给。
* `SparseView::decode_prefixed_result(bytes) -> Result<SparseView, DecodeError>`：
  同上，但失败时给出**为什么**（见 §5）。

**关键不变式**：`encode_prefixed()[2..] == encode()` —— 新格式只是在旧负载前面多写了一个
2 字节头，**定长体一个字节都没动**。因此：

* 128 个现有测试、`tests/e2e_asserts.rs` 的字节断言、`tools/mincore-wasm` 的 12 张金标准快照
  **全部不受影响**（已实测：重新生成的 `native.txt` 与入库版本逐字节相同）；
* 已有的旧格式生产者/消费者可以继续用旧入口，新拓扑用新入口，两者在同一进程里并存。

| cap | 旧格式 | 前缀格式 |
|---|---:|---:|
| 1 | 8 | 10 |
| 2 | 14 | 16 |
| 128 | 770 | 772 |
| 256 | 1 538 | 1 540 |
| 512 | 3 074 | 3 076 |
| 4096 | 24 578 | 24 580 |

### 3.1 它解掉的是什么

旧格式不自描述 cap ⟹ 发送方 cap=256（1 538 B）而接收方期望 770 B ⟹ 长度不符 ⟹ 整条拒绝 ⟹
**网络按 cap 分裂成互不通信的孤岛**（详见 `README.md` §3.2 与
[`审计/有损压缩架构.md`](../../审计/有损压缩架构.md)）。

带前缀格式让接收方**不需要预先知道 cap**：

| 发送方 | 接收方 | 旧格式 | 前缀格式 |
|---|---|---|---|
| cap=256 | cap=256 | ✅ | ✅ |
| cap=256 | cap=512 | ❌ 长度不符 ⟹ 拒绝 | ✅ 按前缀解成 cap=256 |
| cap=256 | 完全没有 cap 概念 | ❌ | ✅ 按前缀解成 cap=256 |

接收方仍应自己施加**资源上限**（「我只接受 cap ≤ 我允许的最大值」）：
那是一个显式的策略判断，不是格式的一部分。注意 `decode_prefixed` 内部已经把
`cap > MAX_CAP` 挡掉，所以最坏分配是 `4096×6+4 = 24 580` 字节。

---

## 4. 为什么两种格式不会互相误判

### 4.1 真正的理由：长度落在模 6 的不同同余类

* 旧格式总长 `L_old = 6·C_old + 2` ⟹ `L_old ≡ 2 (mod 6)`
* 前缀格式总长 `L_pre = 2 + 6·C + 2 = 6·C + 4` ⟹ `L_pre ≡ 4 (mod 6)`

于是：

1. **旧字节喂 `decode_prefixed`**：解码器把 `[0,2)` 读成 `P`（它其实是首账户号的低 16 位），
   要求 `L_old == 6·P + 4`，即 `6·C_old − 2 = 6·P`。左边 `≡ 4 (mod 6)`、右边 `≡ 0`，
   **无整数解** ⟹ 任何旧负载都被长度检查拒掉。
   （例外：若 `P==0` 或 `P > MAX_CAP`，会更早地在 cap 检查上被拒 —— 见 §4.2。）
2. **前缀字节喂 `decode(bytes, cap)`**：要求 `6·cap + 2 == 6·C + 4` ⟹ `6·cap = 6·C + 2`，
   同样无整数解 ⟹ 恒 `None`。

注意理由**不是**「前 2 字节的语义」：前缀格式没有魔数，前 2 字节就是一个裸 u16。带内唯一能
区分两格式的东西是**长度**。这也是为什么 `decode` 与 `decode_prefixed_result` 都坚持
`bytes.len() != 期望长度` 就立刻拒绝。

### 4.2 拒绝理由的顺序（只影响诊断，不影响安全）

`decode_prefixed_result` 先查 cap、再查长度：

1. 连前缀都不够 2 字节 ⟹ `BadLength`
2. `cap == 0 || cap > MAX_CAP` ⟹ `BadCap`
3. 总长 ≠ `2 + cap×6 + 2` ⟹ `BadLength`
4. 定长体的规范性检查（条数 / 升序 / 重复 / 零分 / 填充区）

所以两个边角案例会报 `BadCap` 而不是 `BadLength`：

* **旧空表** `[0; 8]`：首 2 字节是 `0x0000` ⟹ cap=0 ⟹ `BadCap`；
* **首账户号低 16 位为 0 的旧负载**（如 acc=65536）：`[0,2)` 恰为 0 ⟹ 同样 `BadCap`。

两条都有单元测试钉住。它们**不**改变 §4.1 的结论：不管先撞上哪一条，旧负载都解不出来。

### 4.3 ⚠️ 这条安全性的前提：必须精确长度匹配

**两个解码器都要求长度严格相等，缺一不可。** 格式里**没有带内标识**，所以：

* 任何一处放宽成「长度在某范围内」「按 `(len−2)/6` 猜 cap 再四舍五入」，
  都会立刻重新引入「**看似解码成功、语义全错**」的风险；
* 分发端**必须**精确长度匹配：要么发 `cap×6+2` 且双方约定同一 cap，要么发 `cap×6+4`
  且只走前缀解码器。不要从一个长度反推格式。

**若未来出现第三种格式，必须加带内魔数**（版本号 / magic），不能继续靠长度同余类这种
隐式契约。同余类只在「恰好两种格式、且都要求精确长度」这个前提下成立。

---

## 5. 可诊断错误：`DecodeError`

`decode_prefixed_result` 返回 `Result<SparseView, DecodeError>`（旧的 `decode` 保持
`Option` 签名不变，判据与新的**逐条一致**）：

| 变体 | `as_str` | 含义 |
|---|---|---|
| `BadCap` | `bad-cap` | 前缀 cap 为 0、超过 `MAX_CAP`，或压根读不出 |
| `BadLength` | `bad-length` | 长度与「前缀声明的 cap」算出的定长长度不恰好相等 |
| `BadCount` | `bad-count` | 尾部条数 `count > cap` |
| `NotAscending` | `not-ascending` | 账户号乱序 |
| `DuplicateAcc` | `duplicate-account` | 同一账户号出现两次（`NotAscending` 的特例） |
| `ZeroScore` | `zero-score` | 有效槽位里 `q == 0` |
| `NonZeroPadding` | `nonzero-padding` | 有效槽位之后的填充区非全零 |

`DecodeError` 实现了 `Display`（输出 `as_str`）与 `std::error::Error`。
`as_str` 是排查日志用的稳定短标签，但**不承诺跨版本不变**。

---

## 6. 防御清单（均已由单元测试钉住）

`decode_prefixed` / `decode_prefixed_result` 面对任意 `&[u8]` **绝不 panic**：

* 任何长度的截断 ⟹ 拒绝（`BadLength`，除 §4.2 的两个 `BadCap` 边角）；
* 尾部多余字节（0 或非零）⟹ 拒绝（长度必须**恰好**相等）；
* 撒谎的 cap（0、`MAX_CAP+1`、65535）⟹ `BadCap`；
* 长度计算全程 `checked_mul` / `checked_add`：前缀攻击者可控，32 位目标上也不回绕；
* 逐位置逐掩码篡改、2000 条随机字节：要么拒绝、要么解出一个**能再编码回同一串**的规范视图。

---

## 7. 验证命令

~~~powershell
$env:TMP="<仓库根>/.tmp"; $env:TEMP=$env:TMP
cd crates/mincore

cargo test                    # 128 单测 + 17 集成 + 8 e2e(3 ignored) + 1 doctest
cargo clippy --all-targets    # 零 warning
cargo fmt --check             # 零差异

cd ../../tools/mincore-wasm
.\run-all.ps1                 # wasm32 vs native 逐位对拍（金标准应仍新鲜）
~~~
