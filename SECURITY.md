# 安全策略

## 这个库的安全边界（**先读这个再报漏洞**）

`trustmesh` / `tm_*` 是**纯计算库**：

* 不做网络 I/O、不做存储、不做签名验证、不做节点发现；
* 它唯一接收**不可信输入**的地方是两个 `decode`：`CompressedView::decode(&[u8])` 与
  `EngineSnapshot::decode(&[u8])`，两者都返回 `Option`、**任何输入都不 panic**
  （`crates/engine/tests/untrusted_bytes.rs` 钉住了这一点）。

所以「这个库有漏洞」通常指两类之一：

| 类型 | 算不算本库的漏洞 |
|---|---|
| **解码器被畸形字节搞崩 / 越界 / 放大器** | ✅ **算**，请报 |
| **跨平台结果不一致**（同一输入在不同架构给出不同声望） | ✅ **算**，请报 |
| **快照 round-trip 丢数据** | ✅ **算**，请报 |
| 声望算法在某些攻击下失效 | ⚠️ **多数不算**——见 `审计/威胁模型.md` §2 的三条**已知数学缺陷**（S3 背书 4.37×、S4 空账户、S5 自吹 83%）。这些是 β=0.3 的算术后果，已在文档中声明，**部署前必须接受或由应用层兜住**。 |
| 反 DDoS / 内容托管 / 节点发现 / 验签缺失 | ❌ **不算**——`设计规格.md` §0.2 明确划给应用层 |
| 重放旧观点刷新新鲜度（D4） | ❌ **不算**——需要应用层签名/时间戳，`审计/P2-security.md` §3.2 已刻画并说明理由 |

**完整威胁模型见 [`审计/威胁模型.md`](审计/威胁模型.md)。部署前必读。**

---

## 报告漏洞

**请不要开公开 issue。** 用 GitHub 的 [Private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
（仓库 Security 标签页 → Report a vulnerability）。

请包含：

1. **复现步骤**（最好是能 `cargo test` 的最小用例）；
2. **受影响版本**（`git rev-parse HEAD` 或 tag）；
3. **影响面**（是解码器崩溃、跨平台不一致，还是算法失效）；
4. 若涉及算法失效：**说明它是否属于 `审计/威胁模型.md` §2 已声明的三条已知缺陷**。

---

## 本库的安全承诺（可以被验证，不是口号）

| 承诺 | 怎么验证 |
|---|---|
| **零 unsafe** | 6 个 crate 全部 `#![forbid(unsafe_code)]`，编译器强制 |
| **零外部依赖** | CI 的 `no-external-deps` job 会红 |
| **不可信字节不崩** | `crates/engine/tests/untrusted_bytes.rs` |
| **跨平台逐位一致** | CI 的 `wasm-parity` job（wasm32 vs native 36 个 f32 位模式） |
| **快照可精确恢复** | `crates/engine/src/snapshot.rs` 的 round-trip 测试（200,000 个点最大位差 0） |
| **资源可设上限** | `EngineConfig::max_views`（**注意默认 0 = 关闭，部署必须显式设**） |
| **抽样预算可自查** | `Engine::freshness_budget()` 会报 `DecayDiluted` |

---

## 不在范围内的

反 DDoS、流量清洗、限流、准入成本、节点发现、连接建立、加密与认证、
内容托管与多副本分发、可用性保证、「我是否正在受到攻击」的判断、本地 banlist ——
**全部由应用层负责**。详见 [`设计规格.md`](设计规格.md) §0.2。
