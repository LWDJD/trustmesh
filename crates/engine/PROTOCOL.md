# 接入协议（应用层要做什么）

> 这个库只做**信任计算**，不做网络、不做存储、不做验签。
> 所以「接入一个应用」= 应用层按本协议实现四件事：**发布、抽取、投递、持久化**。
>
> 本文件是**接口契约**，不是教程。教程见 `README.md` 与 `examples/quickstart.rs`。

---

## 0. 节点模型的三个不变量

违反任何一条都会破坏正确性，而且**很难排查**：

| # | 不变量 | 违反了会怎样 |
|---|---|---|
| 1 | **账户编号顺序全网一致** | 编号是 `intern` 的先后次序。两个节点对同一批公钥的编号不同 ⇒ 观点里的 `AccountKey` 仍能对上（视图按 key 查），但 `mass_covers` 这个「已知账户数」上界会错 ⇒ 后登记账户的白送背书防护失效 |
| 2 | **`ViewParams` 全网一致**（buckets / hashers / seed） | 桶布局不同 ⇒ `on_view` 会**整条丢弃**（`view.params() != cfg.view_params` 时直接 return）。表现为「网络看起来通，但信任永远不更新」 |
| 3 | **`EngineConfig` 全网一致**（beta / half_life / agg_mode） | 各节点策略不同 ⇒ 声望不可比较，S6 跨节点对比失效。允许**部分**不一致（`noise_multiplier` / `decay_floor` / `penalty_strength` 是本地策略），但 beta 与 half_life 必须统一 |

---

## 1. 每轮要做的四步

一轮 = 一个 `T`（应用层定，例如 12 小时）。下面四步**顺序不能乱**：

~~~text
① 发布   export_view(now)                → bytes
② 抽取   tick(now, k)                    → Vec<FetchRequest>
③ 投递   [网络] + on_view / on_view_failed
④ 可选   probe(now)（兜底探测）/ prune_stale(now, max_age)
~~~

### ① 发布：把观点变成字节

~~~rust
let view = engine.export_view(now);       // CompressedView
let bytes = view.encode();                // Vec<u8>，长度恒为 buckets×hashers×4
~~~

* 发布前**不需要**做任何归一化：`export_view` 已经把总量归一化到 1（这是影响力预算的发布侧实现）。
* 字节里**没有**元数据（没有 owner、没有时间戳、没有签名）。
  **这些必须由应用层加上**，否则接收方无法知道这份观点是谁的、什么时候发的。
* 建议的信封（应用层自己定，本库不关心）：

  ~~~text
  { owner: <32 字节公钥>, published_at: <时间>, payload: <encode() 的字节>, sig: <签名> }
  ~~~

  ⚠️ **`published_at` 必须被签名覆盖**，否则对方可以伪造它。
  但注意：引擎**不用** `published_at` 做衰减基准（用 `last_check`，即我自己观察到的时刻），
  因为 `published_at` 可以随便填。见 `Engine::view_age` 的文档。

### ② 抽取：问「我要取谁的观点」

~~~rust
let reqs: Vec<FetchRequest> = engine.tick(now, k);   // req.idx / req.key
~~~

* `k` 是每轮抽几个。**必须满足 `(清单长度 / k) · T ≤ 半衰期 / 2`**，
  否则衰减变成噪声（A10 实测：n=600/k=8/T=30h 时 90.7% 的来源衰减因子 < 0.1）。
* 用 `Engine::freshness_stats(now)` 自查；`budget` 模块给出推荐 `k`。
* `tick` 内部会按「连续失败次数」退避（权重 1/2^n），**不需要**应用层重试。
* 同一轮内不会抽到重复条目（不放回）；抽空了自动开新一轮。

### ③ 投递：把拿到的观点交回来

~~~rust
// 成功
engine.on_view(owner_idx, view, published_at, now);
// 失败（超时 / 404 / 校验不通过）
engine.on_view_failed(owner_idx, now);
~~~

**应用层必须做的三件校验**（引擎只做后面两条）：

| 校验 | 谁做 | 不做会怎样 |
|---|---|---|
| **签名/来源可信** | **应用层** | 任意 40 KB 字节数组都是「合法观点」，可以冒充任何人 |
| **`ViewParams` 一致** | 引擎（`on_view` 内部） | 桶布局不同 ⇒ 整条丢弃 |
| **只投递真正的新观点** | **应用层** | 重放旧观点会刷新 `last_check` ⇒ 无限续命（A8 D4 实测 group 0.25→0.997） |

⚠️ **最后一条是本库已知的最大接入风险**：`on_view` 不比较 `published_at` 的单调性。
应用层必须自己保证「同一份字节不重复投递」，例如按 `(owner, published_at)` 去重。

### ④ 兜底探测（建议每 24h 一次）

~~~rust
let reqs = engine.probe(now);   // 不走退避，强制扫一遍全部未抽过的条目
~~~

为什么需要：退避会让长期离线的来源永远抽不到，它也就永远回不到 live 集合。
引擎不维护定时器，**节奏由应用层掌握**。

~~~rust
// 顺手清理陈旧观点（可选）
let dropped = engine.prune_stale(now, max_age);   // 只清观点缓存，不动评分
~~~

---

## 2. 持久化（关机前必须做）

引擎状态在内存里，**重启即失**。生产部署必须能存盘恢复。

~~~rust
let snap = engine.snapshot(now);            // EngineSnapshot
let bytes = snap.encode();                  // Vec<u8>，自描述版本号
// ... 应用层写盘 ...

// 重启
let snap = EngineSnapshot::decode(&bytes)?;
let engine = Engine::restore(cfg.clone(), snap)?;   // cfg 由应用层保管，原样传入
~~~

**恢复后必须满足**：`query()` 逐位相同、`tick()` 产生的请求序列相同。

⚠️ `restore` 的 `cfg.view_params` 必须与快照里的观点匹配，否则返回 `None`（不会 panic）。

---

## 3. 声望查询与消费

~~~rust
// 某个目标的声望（0~1）
let score = engine.query(target, now);

// 归一化后的完整分布（总和恒为 1）
let dist: Vec<(AccountIdx, Score)> = engine.trust_distribution(now);
~~~

**怎么用这个分数**是应用自己的事。两个常见用法：

* **门限**：`score > θ` 才允许某操作（θ 由应用定，本库不给建议）
* **排序**：按 `score` 排序内容/用户（注意：**分数差距不等于排名差距**，B1 实测「压得住分数压不住排名」）

⚠️ **不要**把 `query()` 的绝对值当「可信度百分比」。它的语义是「在我这套参数下，这个人相对其他人的位置」。

---

## 4. 受害者反馈（可选）

~~~rust
// 我正在被 idx 攻击：先看谁对它异常热情
let affected = engine.report_bad_account(idx);   // 返回被压低的来源数

// 之后我发布的观点里，它的份额会自然变低（折扣通过 export_view 传播出去）
// 撤销（例如误报）
engine.clear_penalties();
~~~

* 折扣**不会自动恢复**，只有 `clear_penalties` 能清空。
* 折扣只压**群体项**里该来源的话语权，**不改**个人评分。
* 不调用这个接口时，行为与引入该机制之前逐位一致。

---

## 5. 部署检查清单（摘要，完整版见 `../../审计/威胁模型.md`）

- [ ] 清单长度 ≤ 300（否则空账户问题爆发，m=1000 时 30.4%）
- [ ] `(清单长度/k)·T ≤ 半衰期/2`
- [ ] `on_view` 只投递真正的新观点（按 `(owner, published_at)` 去重）
- [ ] 应用层验签，且 `published_at` 被签名覆盖
- [ ] 确认桶数并接受相应的 S1（2048 → ≤0.002；512 → 0.010~0.017）
- [ ] 持久化可用（`snapshot` / `restore` round-trip 逐位一致）
- [ ] 视图缓存上限 `EngineConfig::max_views` 已设（默认 0 = 无限制）
- [ ] 限流 / 防 DDoS / 内容托管由应用层负责（本库零此类代码）

---

## 6. 最小可运行参考

| 想要 | 看 |
|---|---|
| 五分钟跑通 | `examples/quickstart.rs` 或 crate 文档里的 doc test |
| 完整生命周期（多轮 + 快照） | `crates/exp/src/bin/p_persist.rs` |
| 大规模行为 | `crates/exp/src/bin/p_scale.rs` |
| 抽样节奏自查 | `crates/exp/src/bin/p_budget_check.rs` |
