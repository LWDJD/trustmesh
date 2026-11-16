# P2 安全加固报告

> 工作树：`wt/p_security`　分支：`feat/security-hardening`
> 基线：`2ceea00`（索引同步）
> 真 runtime：`crates/engine/src/lib.rs`
> 提交：`315bc5a`（①②）、`f360623`（D4 现状刻画）、`3916f0f`（注释与 exp 配置默认值）、
> `57454b4`（p_stress）、本报告单独一个提交。
>
> 全部数字都是**本工作树自己跑出来的**（命令见 §7）。引用审计结论的地方会明确标注「引用」。

---

## 0. 一句话结论

三个已知缺陷里，**两个修掉了**（①观点缓存无条目上限、②on_view 不校验 total_mass 有限性），
**一个按任务要求只做分析与测试、不改行为**（③重放旧观点刷新 last_check，即审计 A8 的 D4）。

修复的核心是把 on_view 的入口校验补全：三条丢弃路径（形状不符 / 容量已满 / 读数非有限）
共用同一段「整条丢弃 + 视同失败」语义，且 **默认行为逐位不变**
（`max_views = 0` 时与改造前逐位一致，有专门的钉子测试，见 §4.1）。

---

## 1. 缺陷 ①：观点缓存无条目数上限（审计 A13-spec U-01）

### 1.1 现状（改前）

`设计规格.md:37` 写：

> 唯一的资源保护：观点文件设条目数 / 字节数上限，超出即丢弃并视同失败。

而 `Engine::on_view` 无条件执行 `self.views.insert(owner, ...)`：
没有条数上限、没有字节上限、也没有淘汰策略。`views` 只会在应用层**主动**
调用 `prune_stale` 时才缩小。审计 A13-spec 的 U-01 结论是「未实现」。

后果：应用层（或被攻陷的应用层）持续 `on_view` 不同 owner，每条默认参数视图
40960 B，内存线性无界增长。这不是理论：§4.2 的对照组实测 3000 个 owner 就让 RSS 涨了 118 MiB。

### 1.2 修法

新增 `EngineConfig::max_views: usize`（[crates/engine/src/lib.rs:111](crates/engine/src/lib.rs)），
默认 `0 = 无限制`。判定放在 on_view 里、**噪声底与质量扫描之前**
（[crates/engine/src/lib.rs:438](crates/engine/src/lib.rs)）：

```rust
if self.cfg.max_views > 0
    && self.views.len() >= self.cfg.max_views
    && !self.views.contains_key(&owner)
{
    self.reject_view(owner, Rejection::Capacity);
    return;
}
```

三条设计要点：

1. **只有「新 owner 且已满」才丢。** 已缓存的 owner 更新既有条目是原地覆盖，
   内存不增长；丢掉它只会阻断正常的观点刷新，一分资源都省不下来。
2. **丢弃 = 视同失败。** `reject_view` 直接复用 `on_view_failed` 的两条
   效果（时钟前移 + 连败 +1），于是下一次 `tick` 会按 1/2^n 退避它，链路自然收敛。
   规格 §0.3 的原文就是「超出即丢弃并视同失败」，这里逐字实现。
   注意：连败只对**有评分条目**的来源生效（`PositiveList::record_failure` 对未评分
   账户是 no-op）——这是存储层的既有语义，不是本次引入的，但部署时要知道（§5.2 第 4 条）。
3. **字节数上限 = 条目数上限。** `view.params()` 在入口就被校验，每份缓存视图恒为
   `buckets × hashers × 4` 字节，所以条数上限就是内存上限
   `max_views × 视图字节数`（另加每条约 300~380 B 元数据，见 §4.2）。

### 1.3 可观测性：为什么不改返回类型

`on_view` 的返回类型保持 `()`。理由：

* 返回值形状一改，所有既有调用点（exp 里 20+ 个 bin、engine/sim/exp 的测试）都要动；
* 调用方本来就**分不清**成功与失败——失败走的是另一个方法 `on_view_failed`，
  应用层是按「我这次请求成没成功」来二选一调用的。丢弃在这里对上层透明。

新增两个接口：`rejected_count()`（累计丢弃条数，[lib.rs:983](crates/engine/src/lib.rs)）
与 `rejected_breakdown()`（细分为「容量丢弃 / 非有限丢弃」，[lib.rs:993](crates/engine/src/lib.rs)）。
`stats()` 的四元组**形状与含义一个字节都不变**（既有实验按位置解构它）。

`view.params()` 不匹配的视图仍然**静默丢弃且不计数**：那是协议层的「无法解释的
字节」，与「可解释但被拒」是两件事。混在一起会让「有人在喂脏视图」这个安全信号被
配置错误淹没。

---

## 2. 缺陷 ②：on_view 不校验 total_mass 有限性（审计 A8-attack D5）

### 2.1 现状（改前）

```rust
let mut total_mass = 0.0f32;
for i in 0..self.accounts.len() as u32 {
    if let Some(k) = self.accounts.key_of(i) {
        total_mass += view.query(&k).clamp(0.0, 1.0);
    }
}
let total_mass = total_mass.max(1e-6);   // 没有 is_finite 检查
```

`decode` 会拒绝非有限负载，所以正常网络路径上脏值进不来。但**应用层可以**
用公开 API 造出来：`merge(self, 0.0, 0.0)` 作用在含 ±∞ 的桶上会触发
`0 × ∞ = NaN`。审计 A8 §4 就是用这个方法在进程内构造 NaN 视图的，并明确
「部分行 NaN + 其余有限」那条只是**推理、没有实测**——本次把它实测了（§4.4）。

### 2.2 修法：为什么必须在求和**过程中**检查

如果照审计 A8 §4 的建议写成「求和之后再 `if !total_mass.is_finite() { return; }`」，
**这个防线是无效的**。本次专门写了一条测试把这件事钉死
（`capacity.rs::nan_max_semantics_make_post_summation_check_useless`）：

```text
NaN.max(1e-6)   =  1e-6     （f32::max 返回非 NaN 的那个操作数）
NaN.clamp(0,1)  =  NaN      （clamp 不消 NaN）
+inf.clamp(0,1) =  1.0      （+∞ 被静默收成 1.0，total_mass 之外看不出异常）
-inf.clamp(0,1) =  0.0
```

所以「在 `.max()` 之后查 `is_finite()`」永远查不出 NaN；它只会把总质量
塌到 `1e-6` 这个**放大器档位**，让整份视图的每条读数都被抬到 1.0
（与审计 A6 的 D4 记录的是同一个洞）。+∞ 更隐蔽：clamp 之后在总和里根本看不到它。

正确做法（[crates/engine/src/lib.rs:458](crates/engine/src/lib.rs)）：

```rust
let mut total_mass = 0.0f32;
let mut all_finite = true;
for i in 0..self.accounts.len() as u32 {
    if let Some(k) = self.accounts.key_of(i) {
        let q = view.query(&k);        // 看 clamp 之前的原始值
        if !q.is_finite() {
            all_finite = false;
            break;
        }
        total_mass += q.clamp(0.0, 1.0);
    }
}
let total_mass = total_mass.max(1e-6);
if !all_finite || !total_mass.is_finite() {    // 后者只是求和溢出的兜底
    self.reject_view(owner, Rejection::NonFinite);
    return;
}
```

丢弃语义与 ① 完全相同（同一处 `reject_view`），细分计数走
`Rejection::NonFinite`。正常路径（全有限视图）**一个数值都没变**：检查只在循环里
多了一次 `is_finite()`，累计结果与旧代码逐位相同（§4.1 的兼容性钉子与
§4.7 的 519 条测试共同覆盖）。

---

## 3. 缺陷 ③（D4）：重放旧观点刷新 last_check —— **只分析，不改**

### 3.1 现状

`on_view` 只看 `now`，不看 `published_at`，也不比较内容版本：

* 同一份**旧字节**重放一次，`last_check` 就被推到「刚刚」，该来源的衰减权重
  立刻回到 1.0；
* 实测：两人各 50% 份额、诚实来源在 t=288 刷新、攻击者的观点停在 t=0 时，t=288 的群体项
  被压到 **5.88%**；重放同一份字节之后跳到 **50%**，**放大 8.5 倍**；
* 更狠的是**周期性重放**：每 72 个时间单位重放一次旧字节，40 轮后群体项从 0.25
  单调逼近 **1.0**（与解析式 `1/(1+0.5^r)` 逐点对齐，误差 < 1e-4）。
  这就是审计 A8-attack 记录的「0.25 → 0.997」在单来源上的最小复现；
* 成本为零：重放字节逐字节相同（`encode()` 一致），唯一变化是 `now`；
* 附带：`published_at` 从不参与判断，所以「声明发布于 100 的旧内容」可以覆盖
  「声明发布于 500 的新内容」——一次干净的内容回滚。

以上四条都写成了可执行测试：[crates/engine/tests/replay.rs](crates/engine/tests/replay.rs)（5 条，全绿）。
写成「断言现状」而不是失败/ignored 是有意的：保持 cargo test 全绿，同时在**将来真的修了
D4 时立刻变红**，提醒同步更新本节。

### 3.2 「修 / 不修」论证

#### 方案 A：库内加 published_at 单调性检查（最小改法）

```rust
// on_view 里，在 params 校验之后
if let Some(prev) = self.views.get(&owner) {
    if published_at < prev.published_at { return; }   // 旧观点不回滚新观点
}
```

**能挡住的**：网络层的重复投递、乱序到达，以及「拿旧内容覆盖新内容」这类无成本回滚。

**挡不住的**（这是关键）：`published_at` 由**对方**填写，它可以每次都填「现在」。
加了单调性检查之后，攻击者只要把每一份重放字节的 `published_at` 改成递增的伪时间戳
就完全绕过——而这不需要任何签名、任何成本。也就是说：

* 方案 A 只能挡住**诚实的传输层事故**（重传、乱序）；
* 挡不住**恶意来源**，而 D4 的危害恰恰来自恶意来源。

**代价**：

* 会让「同一份字节重放」变成静默丢弃。静默丢弃不刷新 `last_check`，但会 +1 连败
  （与 ① 的丢弃语义一致），把整个来源推向退避。对一个「只是重传了一次」的诚实节点是误伤
  （虽然下一次正常观点会清零）；
* 需要多看一个已有字段（`StoredView::published_at`），无内存代价；
* **默认行为会变**：既有重放场景（含 sim 里按轮次刷新的路径）都要重新验证；
* 全部衰减线实验（D1~D4、L1~L5）的时效前提建立在 `last_check` 上，改语义要重跑。

#### 方案 B：库内加「内容哈希 + 版本号」去重

挡住一切逐字节重放，但对「改一个字节的旧内容」无效，且要求协议层加版本号字段，
属于协议变更，不是本库单方面能定的。

#### 方案 C：应用层裁决（审计 A8 的结论）

审计 A8 §5 的原话是「**D4 重放/新鲜度的最终裁决权在应用层**（签名时间戳、内容版本号、
每来源的更新速率限制）」。理由很硬：

* 库看不到「签名」：`on_view(owner, view, ...)` 完全信任调用方传的 owner
  （审计 A8 把「签名与 owner 绑定」明确划给应用层）；
* 库没有网络层上下文（这是第几次投递、通过哪条通道来的）；
* 「多久刷新一次算正常」是业务判断（不同应用差别巨大）。

#### 结论：**本轮不修**

理由按优先级：

1. **作者没表态**。任务明确「只做分析和测试，不要改行为」；`设计规格.md:5` 写着
   「作者尚未表态的地方标为『待定』，实现者不得自行决定」。
2. **库内最小修法（方案 A）挡不住真正的威胁**：它只能挡住诚实传输层的重传，挡不住恶意
   来源伪造 `published_at`。为一个挡不住威胁的改动去动引擎的入口语义，
   收益与风险不成比例。
3. **它有一个必须应用层先做的配套改动**（签名时间戳 / 内容版本号），在此之前任何库内
   检查都只是「看起来修了」。
4. **代价明确**：真修 D4 需要同时定协议字段、改 on_view 的丢弃语义、重跑全部衰减线实验。

**如果将来要修，建议的落地顺序**（留给下一轮）：

1. 协议层加「签名时间戳 + 内容版本号」（应用层，不属本库）；
2. 库内加 `published_at` 单调性检查（一行，方案 A）作为**防御纵深**，
   文档写清「只挡传输层事故，不挡伪造」；
3. 库内加「每来源最小刷新间隔」的可选配置（默认 0 = 关闭），让应用层能配
   「同一来源 N 秒内只刷新一次」——这是唯一能在库内低风险缓解周期性重放的旋钮。

---

## 4. 实测数字

设备：本工作树所在 Windows 主机（Rust 1.96.0，release 模式，opt-level=3）。
命令见 §7。**完整跑一遍 p_stress 的墙钟时间是 251.9 s**（其中编译 < 1 s）。

### 4.1 兼容性：max_views = 0 逐位不变

`capacity.rs::max_views_zero_is_bitwise_identical_to_default_config` 用两台引擎
（一台显式 `max_views: 0`、一台 `EngineConfig::default()`）跑同一批
40 个来源 × 40 次 on_view（视图内容各不相同，覆盖个人项/群体项/噪声底/覆盖上界四条路径），
比较 `stats()` 四元组与 140 个账户的 `query().to_bits()` 快照 —— **逐位相同**。

`max_views_zero_caches_everything` 进一步钉住「0 不是『一条都不许收』」这个经典边界
错误（`0 >= 0` 恒真）：喂 300 个 owner，全部进缓存、丢弃计数 0。
默认值本身也有一条钉子：`default_max_views_is_zero_meaning_unlimited`。

### 4.2 场景 A：容量上限（10000 个 owner，max_views = 1000）

```text
喂入 10000 份观点，耗时 1.63 s（163.17 µs/次）
缓存条数 = 1000（上限 1000）
rejected_count = 9000，细分 = (9000, 0)
prune_stale 之后重新收满：缓存 = 1000，累计丢弃 = 9000
[内存] RSS 实测 6.41 MiB -> 45.80 MiB（Δ +39.39 MiB）
```

* 1000 份 2048×5 视图的净字节数是 `1000 × 40960 = 39.06 MiB`，RSS Δ 39.42 MiB，
  **差额 0.33 MiB ⇒ 每条元数据约 340 B**（BTreeMap 节点 + StoredView 字段 + Vec 头 +
  分配器记账；两次完整跑的实测分别是 375 B 与 340 B，故估算常量取更保守的 384 B）。
* 喂入 163 µs/次 比 --quick 跑的 49 µs/次 高，是因为其中 1000 次走完了完整的
  `noise_floor(1000)` + 质量扫描；剩下 9000 次被容量拒掉的点在噪声底**之前**就
  return 了，几乎零成本（§4.6 的 bench 解释了这块固定成本）。

**对照组（max_views = 0，旧行为）**：喂 3000 个 owner：

```text
喂入 3000 份 -> 缓存 3000 条，耗时 1.86 s
[内存] RSS 实测 Δ 118.11 MiB（40.3 KiB/owner）
=> 同样喂 10000 个，旧行为约 393.7 MiB，有界场景约 39.4 MiB：差 10 倍
```

这就是缺陷 ① 的量化：**旧行为的内存随「喂了多少个不同 owner」线性增长**，
修后随 `max_views` 有界。

### 4.3 场景 B：更新路径（同一 owner 更新 100 万次）

```text
耗时 127.44 s（127.44 µs/次），省略率 = 100
缓存条数 = 1，命中 = 1000000，丢弃 = 0
[内存] RSS 实测 5.61 MiB -> 5.45 MiB（Δ +0.00 MiB）
```

* 100 万次更新之后缓存**恒为 1 条**，丢弃计数为 0（更新既有条目永远接受）；
* 每 10000 次插入一次 `on_view_failed`（共 100 次），最后一次成功刷新把连败清零；
  断言 `fail_streak == 0`、`view_age == Some(0)` 都成立；
* RSS Δ 0.00 MiB：更新是原地覆盖，内存完全持平。

### 4.4 场景 C：NaN / +inf 恶意视图

```text
脏视图喂入 200 次：容量丢弃 50 次、非有限丢弃 150 次
缓存条数 = 64（与干净引擎相同）
```

* 200 次里有 1/4 是**新 owner**（缓存已满 64 ⇒ 先撞**容量**判定，因为那条判定在质量扫描
  之前），所以计在 capacity；其余 150 次是已缓存 owner 的刷新，全部按 nonfinite 丢弃。
  这个细分本身就把「判定顺序」钉住了。
* 对照引擎（从未见过脏视图）与被污染引擎：`stats()` 相同、64 条既有观点的
  `last_check` 逐条相同、128 个账户的 `query().to_bits()` **逐位相同**。
* 丢弃之后继续喂干净观点，两台引擎仍然逐位一致；丢弃计数停在 200 不动（正常观点没被误杀）。

**本次实测了审计 A8 只做过推理的那条**（`部分行 NaN + 其余有限`）：
`capacity.rs::nan_view_with_key` 造出来的视图，其 `total_mass` 在改前代码下会经
`NaN.max(1e-6)` 变成 `1e-6`（这是 f32 语义，不是推测），于是**整条视图会进缓存**
——`nan_total_mass_is_rejected_whole` 与 `nan_view_from_new_owner_is_not_cached`
断言的就是「改后 `stats().2` 与命中数都不变」，它们一旦变红就说明脏视图又进得去了。
（改前的「会进缓存」是由代码与 f32 语义直接推得的结论，本轮没有为了它去 checkout 旧提交重跑；
改后的行为则是实测的。）至于「它能否把**别的** key 的读数抬到 1.0」，
本场景的构造做不到（被投毒的 key 只污染自己那 5 个桶，其余 key 的 5 个估值里至少 3 个是 0，
中位数仍是 0），所以那条推理**在本工作树里没有被证实为可利用**。修复的价值是防御纵深，
不是堵一个已证实的洞——这一点必须说清楚，不能夸大。

### 4.5 场景 D：混合压测（100 万次）

```text
耗时 106.43 s（106.43 µs/次）
tick 299653 次、probe 6239 次、on_view 500069 次、on_view_failed 99847 次、prune 94192 次
缓存条数 = 1000（峰值 1000，上限 1000），丢弃 = (247915, 0)，命中 = 252154
[内存] RSS 实测 5.98 MiB -> 6.83 MiB（Δ +0.85 MiB）
```

* 每次操作后断言 `views.len() <= max_views`（100 万次逐次检查，无一次越界）；
* 每 10000 次抽查：查询有限且在 [0,1]、丢弃计数**单调不减**、
  请求数 <= `tick 次数 × 8 + probe 次数 × 账户数`、命中 <= 喂入数、
  评分条目 <= 账户数；
* 账户数刻意取 `max(2 × max_views, 300) = 2000`，保证上限**真的被顶满**
  （峰值确实 = 1000，丢弃 247915 次）。若沿用固定的 300 个账户，缓存永远装不满，
  这个场景就只测了热路径、没测上限本身——这是设计时发现并修掉的一个空转风险。

### 4.6 bench：单次 on_view 的真实成本

```text
default 2048x5（40 KiB/视图）: 194.32 µs/次（200 次平均）
fast 64x1（256 B/视图）:      67.80 µs/次（200 次平均）
```

on_view 的固定成本是 `noise_floor(1000)`（1000 次查询）**加**对全部已知账户的质量
扫描，与 `max_views` 无关、也与「容量上限挡掉多少」无关。所以：

* 场景 B/D 用 64×1 的形状参数（`--params-bd=fast`，256 B/视图），把 100 万次压到
  ~106~127 s 量级（vs 默认参数下 ~194 s）；走的是**同一条** on_view 代码路径，只是噪声底便宜；
* 场景 A/C 用默认 2048×5（`--params-ac=default`），因为要测的是真实尺寸下的内存；
* 「容量上限能省 CPU」这个说法**不成立**：被拒的观点在噪声底之前 return，省的是内存不是
  时间——A 场景 9000 次被拒的边际成本接近 0。

### 4.7 回归：所有 crate 的测试

```text
engine  114 passed; 0 failed; 1 ignored     （新增 capacity 17 + replay 5）
exp      25 passed; 0 failed; 0 ignored
store   136 passed; 0 failed; 0 ignored
agg      63 passed; 0 failed; 0 ignored
view     51 passed; 0 failed; 0 ignored
sim     130 passed; 0 failed; 0 ignored
```

engine 的那 1 条 ignored 是**既有**的对照测试（`tests/badlist.rs:261`，
注释写明「旧口径对照：分母只含有观点的来源，修复后必然失败」），不是本次引入的。

---

## 5. 剩余风险（修完之后仍然存在的）

按「谁能兜住」分层，全部明确写出来，不含糊。

### 5.1 库内仍然挡不住（属于应用层）

| 风险 | 为什么库挡不住 | 应用层该做什么 |
| --- | --- | --- |
| **D4 重放 / 内容回滚** | published_at 是对方填的；库看不到签名与网络上下文 | 签名时间戳 + 内容版本号 + 每来源刷新速率限制（§3.2） |
| **owner 绑定** | `on_view(owner, view, ...)` 完全信任调用方传的 owner，负载里没有 owner、没有签名 | 负载里放 owner 并验签（审计 A8 §5 已明确） |
| **非有限视图从网络进来** | `decode` 已拒绝非有限负载，库内这条是**防御纵深** | 保持 decode 前校验；不要绕开 decode 直接喂进程内构造的视图 |
| **带宽 / 请求速率** | 库不做 I/O，也不该决定带宽策略 | 限流、单来源视图大小配额 |
| **「谁算坏账户」** | `report_bad_account` 不自动纠错，折扣写进去只能手动撤销 | 判断逻辑 + 定期 `clear_penalties` |

### 5.2 本次修法自身的残余风险

1. **max_views 的取值需要应用层配**。默认 0 = 无限制，意味着**默认配置仍然无界**。
   这是刻意的：默认值必须逐位不变（否则会悄悄改变所有既有实验的口径）。
   部署时**必须**显式设上限（建议量级：`max_views × 视图字节数` 不超过进程内存
   预算的合理份额；2048×5 下 1000 条约 39 MiB，实测 RSS Δ 39.4 MiB）。
2. **容量丢弃会「误伤」被挤出的新来源**。缓存满了之后新来源永远进不来（直到
   `prune_stale` 被调用、或某个 owner 掉出正分清单）。这是「丢弃新来的」而不是
   「淘汰旧的」——本轮刻意不做 LRU/LFU 淘汰（那是业务判断，且会引入不可复现的随机性）。
   应用层应当给 `prune_stale` 配一个定期节奏。
3. **丢弃计数要有人读**。它是 u64、不会溢出，但应用层不读就收不到信号。
   `rejected_breakdown()` 的 nonfinite 项持续大于 0 是明确的安全告警。
4. **连败只对有评分条目的来源生效**。被容量拒掉的**未评分**来源不会累计连败（存储层既有
   语义），所以「退避它」对这类来源不成立。这一条在 §1.2 已写明；不是新引入的缺陷，
   但部署时要知道。
5. **质量扫描是 O(已知账户数)**，与被丢弃的视图无关。账户数很大时 on_view 的成本仍随 N
   线性增长（审计 A11 记录的 D4：每份 O(n) 质量扫描 + 1000 次噪声标定）。本次没有改这一点
   （不在任务范围，且改它会动数值口径）。

### 5.3 明确没有做的事

* **没有修 D4**（按任务要求；§3.2 给了论证与落地顺序）；
* **没有做 LRU / 条款淘汰**（业务判断 + 确定性风险）；
* **没有加「每来源最小刷新间隔」配置项**（属于 D4 的连带修法，见 §3.2 建议第 3 条）；
* **没有改 query / tick / decay 的任何数值行为**（§4.7 的 519 条测试共同钉住：
  engine 114 + exp 25 + store 136 + agg 63 + view 51 + sim 130）。

---

## 6. 验收对照

| 任务要求 | 结果 | 证据 |
| --- | --- | --- |
| cargo test 在 engine / exp 全绿 | ✅ engine 114 通过 1 ignored、exp 25 通过 | §4.7 |
| p_stress 四场景全跑通，真实耗时与内存 | ✅ 墙钟 251.9 s，数字见 §4.2~4.5 | §4 |
| max_views=0 向后兼容测试存在并绿 | ✅ 两条（逐位一致 + 300 条全收） | §4.1 |
| 至少 3 个 commit | ✅ 4 个 + 本报告 | 抬头 |
| 零外部依赖 | ✅ 只用 std | §6.1 |
| 不改 query / tick / decay | ✅ 只加了 on_view 的入口守卫与 EngineConfig 的新字段 | git diff |
| 不修 D4，只分析 | ✅ 5 条现状刻画测试 + §3.2 论证 | §3 |

### 6.1 关于「零外部依赖」

新增代码全部只用 `std`：

* `p_stress` 的 RSS 测量走 `std::process::Command` 调系统的
  `powershell -NoProfile -NonInteractive -Command "(Get-Process -Id PID).WorkingSet64"`
  （Windows）或读 `/proc/self/statm`（Linux）。两条都是平台原语，没有引入 crate；
  两者都不可用时退化为「条数 × 视图字节数 + 每条约 384 B」的估算，并在输出里标注
  是实测还是估算。
* 其余全部是 `std::time::Instant` / `std::env::args` 这类标准库设施。

---

## 7. 复现命令

工作树根目录：`D:\Project\deepseek_harness\去中心化算法探索-A\wt\p_security`

```powershell
$env:TMP='D:\Project\deepseek_harness\去中心化算法探索-A\.tmp'; $env:TEMP=$env:TMP

# 1. 引擎测试（含新增的 capacity 17 条 + replay 5 条）
cd crates\engine
cargo test

# 2. 实验框架测试（验证没有破坏 exp）
cd ..\exp
cargo test

# 3. 压力测试：全量（约 252 s，本机实测墙钟 251.9 s）
cargo run --release --bin p_stress

# 4. 快速冒烟（约 10 s）
cargo run --release --bin p_stress -- --quick

# 5. 只跑某个场景 / 改规模
cargo run --release --bin p_stress -- --only=a,c --owners=20000 --cap=500
cargo run --release --bin p_stress -- --only=d --updates=100000
```

---

## 8. 改动的文件

| 文件 | 改动 |
| --- | --- |
| [crates/engine/src/lib.rs](crates/engine/src/lib.rs) | `EngineConfig::max_views`、`Rejection` 枚举、on_view 的两处守卫、`reject_view`、三个统计字段 + 两个公开接口 |
| [crates/engine/tests/capacity.rs](crates/engine/tests/capacity.rs) | **新增**，17 条容量 / 非有限 / 兼容性测试 |
| [crates/engine/tests/replay.rs](crates/engine/tests/replay.rs) | **新增**，5 条 D4 现状刻画测试 |
| [crates/engine/tests/decay_modes.rs](crates/engine/tests/decay_modes.rs) | 配置字面量补 `..Default::default()`（补的是默认值，不改数值） |
| [crates/engine/tests/normalization.rs](crates/engine/tests/normalization.rs) | 同上 |
| [crates/exp/src/bin/a5_group.rs](crates/exp/src/bin/a5_group.rs) | 同上 |
| [crates/exp/src/bin/c1_badlist.rs](crates/exp/src/bin/c1_badlist.rs) | 同上 |
| [crates/exp/src/bin/c2_report.rs](crates/exp/src/bin/c2_report.rs) | 同上 |
| [crates/exp/src/bin/d4_health.rs](crates/exp/src/bin/d4_health.rs) | 同上 |
| [crates/exp/src/bin/p_stress.rs](crates/exp/src/bin/p_stress.rs) | **新增**，911 行压力测试 |
| [审计/P2-security.md](审计/P2-security.md) | **新增**，本文件 |
