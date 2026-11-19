//! 节点状态快照：重启后**逐位**回到关机前。
//!
//! # 为什么需要
//!
//! 「抽完一轮才重置」「改分不动版本号」这些性质都依赖进程活着。一旦重启，
//! 内存里的 [`Node`] 就没了，而**抽样进度**（本轮已经抽过哪些人）和**随机数状态**
//! 只要丢一位，重启后的抽样序列就与「不重启」分叉 —— 这不是精度问题，是可观测的
//! 分布偏移：凡是能让你重启的人，都能据此推断你什么时候会再抽到它。
//! 所以持久化必须**逐位恢复**，不能是「大致还原」。
//!
//! # 格式（第 1 版）
//!
//! ```text
//! 偏移   字段                                                       字节
//! 0      魔数 "TMMINSNAP"                                          9
//! 9      版本号 u32 = 1                                            4
//! 13     cfg_digest u64（FNV-1a 64，见 [`cfg_digest`]）           8
//! ---- 以下偏移从 21 起算 ----
//! --- 正分清单 ---
//! 21     list.version u64                                          8
//! 29     list.max_entries u64                                      8
//! 37     list.stat_rejected_limit u64                              8
//! 45     条数 n u32                                                4
//! 49     n × (acc u32, score 位模式 u32, last_check u64,
//!               fail_streak u16, 填充 u16 = 0)                     n × 16
//! --- 轮次位图 ---
//!        order 条数 u32 + 每条 acc u32                              4 + n × 4
//!        drawn 字数 u32 + 每字 u64                                 4 + w × 8
//!        remaining u64                                           8
//!        version u64                                             8
//!        ready u8（0 或 1，其余值拒绝）                              1
//! --- 观点缓存 ---
//!        条数 u32                                                  4
//!        每条：owner u32, 视图自己的 cap u32, 槽位数 L u32,
//!              L × (acc u32, q u16),
//!              published_at u64, last_check u64                    28 + L × 6
//! --- 尾部 ---
//!        rng.state u64                                            8
//!        stat_views u64                                           8
//!        stat_rejected u64                                        8
//!        stat_rejected_limit u64                                  8
//! 末尾   不得有多余字节
//! ```
//!
//! 五条设计取舍，与现行实现 tm_engine 的 snapshot.rs 同源：
//!
//! 1. **自描述**：魔数 + 版本号在最前面，先读版本再解析。版本不符直接整体拒绝，
//!    不做「宽松兼容」—— 半个旧格式比没有格式更危险。
//! 2. **整数小端、浮点搬位模式**：score 用 [`to_bits()`] 原样写入，**不做任何**
//!    数值转换。于是 -0.0 与 0.0、不同 NaN 载荷都各有唯一编码，往返逐位相同。
//! 3. **完全防御式解码**：先验算「这份负载放不放得下」再分配（见 [`Reader::count`]），
//!    所以 len = u32::MAX 的伪造负载**立刻**返回 None，绝不会触发 4 GB 的
//!    with_capacity。所有字段在入口按各自值域再校验一遍（升序、q > 0、
//!    score ∈ (0, 1]、ready ∈ {0, 1}、rng.state != 0 ……）。
//! 4. **末尾严格无多余字节**：截断与「尾巴塞东西」都被拒绝，格式没有扩展余地，
//!    要扩展只能改版本号。
//! 5. **快照不含 cfg**，但带 cfg_digest：恢复时参数不匹配直接拒绝，
//!    避免「用 512 的配置恢复 128 的视图」这类静默错误。
//!
//! # 视图段只存有效槽位，但**必须**存每份视图自己的 cap
//!
//! [`SparseView`] 的槽位是**规范形式**（严格升序、q > 0、尾部条数），
//! 所以「条数 + 每槽 (u32, u16)」的 n + n × 6 字节与定长表里**有效区**的字节布局
//! 逐字节相同，真正省掉的只是**空槽区的零填充**（cap = 512 时每份视图省 2926 字节）。
//! **同一份状态只有一种字节串**（不引入「稀疏存法 / 定长存法」二选一的歧义）。
//!
//! 那 cap 为什么不省？因为**它不能从 cfg.cap 推出来**：[`Node::on_view`] 接收的是
//! **对方**发来的视图，cap 由对方定、与我的 cfg.cap 无关（跨节点交换视图时两者不同是
//! 常态：我 cap = 512 也能收 cap = 64 的视图，mincore 自己的单测里就有这种混用）。
//! 省掉这 4 字节的代价是：恢复后每份视图的 cap 被统一改写成 cfg.cap，于是
//! [`SparseView::encode`] 输出的长度、[`SparseView`] 的 PartialEq 结果都会与关机前不同 ——
//! 那就不是「逐位回到关机前」了。计算路径（distribution / query）确实不看 cap，
//! 但**快照的承诺比「计算结果相同」更强**，所以这 4 字节不能省。
//!
//! # 边界（诚实说明）
//!
//! * cfg_digest 是 **FNV-1a 64**，只用来**拒绝参数不匹配**，不是密码学校验和。
//!   能改写快照文件的人当然也能改写摘要 —— 快照是**本地持久化**工件，
//!   不是网络上的认证载荷。要防篡改，请在存储层做认证（签名或带密钥的 MAC）。
//! * 摘要对 f32 走**位模式**而不是 ==：beta 为 NaN 的两份配置（同一位模式）摘要相同、
//!   可以互相恢复；这与 [`Config`] 的 PartialEq 语义（NaN != NaN）**不同**是刻意的 ——
//!   快照是位语义。
//! * 快照按设计**不**校验 views.len() <= cfg.max_views 之类的资源上限：
//!   上限是「拒绝新增」的运行时策略，不是状态的合法域（改小上限后既有条目仍合法）。

use std::cell::Cell;
use std::collections::BTreeMap;

use crate::list::{Acc, Entry, PositiveList};
use crate::node::{Config, Node, Received};
use crate::round::{Rng, RoundBitmap};
use crate::view::{SparseView, SLOT_BYTES};

/// 魔数：9 字节。故意不是常见英文单词，减少「另一个格式恰好同前缀」的概率。
const MAGIC: [u8; 9] = *b"TMMINSNAP";

/// 格式版本号。**任何**改变字节布局的改动都必须 +1。
const VERSION: u32 = 1;

/// 头部字节数：魔数 9 + 版本 4 + 摘要 8。
pub const HEADER_BYTES: usize = 9 + 4 + 8;

/// 清单段的固定部分：版本 8 + 容量上限 8 + 容量拒绝计数 8 + 条数 4。
pub const LIST_HEADER_BYTES: usize = 8 + 8 + 8 + 4;

/// 每个正分条目的字节数：acc 4 + score 4 + last_check 8 + fail_streak 2 + 填充 2。
pub const RATING_BYTES: usize = 4 + 4 + 8 + 2 + 2;

/// 位图段里除 order / drawn 两个变长数组之外的字节数：remaining 8 + version 8 + ready 1。
pub const SAMPLER_TAIL_BYTES: usize = 8 + 8 + 1;

/// 每条观点记录的固定字节数：owner 4 + 视图 cap 4 + 槽位数 4 + published_at 8 + last_check 8。
pub const VIEW_HEADER_BYTES: usize = 4 + 4 + 4 + 8 + 8;

/// 尾部统计字节数：收发成功 8 + 被拒 8 + 容量拒绝 8。
pub const STATS_BYTES: usize = 8 + 8 + 8;

/// 空节点的快照字节数 = 110。
///
/// 构成：头 21 + 清单固定 28 + 清单条数 4 + order 条数 4 + 位图尾 17
/// + 观点条数 4 + rng 8 + 统计 24。
pub const EMPTY_SNAPSHOT_BYTES: usize =
    HEADER_BYTES + LIST_HEADER_BYTES + 4 + 4 + SAMPLER_TAIL_BYTES + 4 + 8 + STATS_BYTES;

// ---------------- 解码失败点（仅诊断用） ----------------

const TAG_OK: u8 = 0;
const TAG_MAGIC: u8 = 1;
const TAG_VERSION: u8 = 2;
const TAG_DIGEST: u8 = 3;
const TAG_LIST: u8 = 4;
const TAG_SAMPLER: u8 = 5;
const TAG_VIEWS: u8 = 6;
const TAG_RNG: u8 = 7;
const TAG_STATS: u8 = 8;
const TAG_TRAILING: u8 = 9;

thread_local! {
    /// 本线程最近一次解码失败发生在哪一段。
    ///
    /// 为什么不做成错误类型：数据面只需要 Option（能恢复 / 不能恢复），
    /// 而「为什么不能」是**测试与排障**才需要的。做成 thread-local 的 Cell
    /// 而不是全局可变状态，一是守住 crate 级 forbid(unsafe_code)，二是让并行测试互不踩踏。
    static FAILED_AT: Cell<u8> = const { Cell::new(TAG_OK) };
}

/// 记下失败段。
fn note_failure(tag: u8) {
    FAILED_AT.with(|c| c.set(tag));
}

/// 上一次解码失败发生在哪一段：0 = 空/成功、1 魔数、2 版本、3 摘要、4 清单、
/// 5 位图、6 观点、7 随机源、8 统计、9 末尾多余字节。
///
/// **仅供测试与排障**：它只反映本线程最近一次 [`NodeSnapshot::decode`] 的结果，
/// 生产代码不该依赖它（那等于把诊断信息当控制流）。
#[doc(hidden)]
#[must_use]
pub fn last_failure_tag() -> u8 {
    FAILED_AT.with(Cell::get)
}

// ---------------- 摘要 ----------------

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// 把一个 u64 的小端字节逐个折进 FNV-1a 64。
fn fold_u64(mut h: u64, v: u64) -> u64 {
    for b in v.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Config 的摘要：六个旋钮逐个按小端折进 FNV-1a 64。
///
/// 顺序**固定**（beta、half_life、cap、decay_enabled、max_views、max_list）。
/// 将来若加字段，必须**追加在末尾**并同时升 [`VERSION`]：改顺序会让旧快照静默错配。
///
/// f32 走位模式而不是数值：-0.0 与 0.0 摘要不同 —— Config 的 PartialEq 认为它们相等，
/// 但快照是位语义，宁可严一点。
fn cfg_digest(cfg: &Config) -> u64 {
    let mut h = FNV_OFFSET;
    h = fold_u64(h, u64::from(cfg.beta.to_bits()));
    h = fold_u64(h, cfg.half_life);
    h = fold_u64(h, cfg.cap as u64);
    h = fold_u64(h, u64::from(cfg.decay_enabled));
    h = fold_u64(h, cfg.max_views as u64);
    h = fold_u64(h, cfg.max_list as u64);
    h
}

// ---------------- 写端 ----------------

/// 只追加、不回读的顺序写入器。
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.raw(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.raw(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }

    /// 浮点**只搬位模式**：to_bits() 之后按 u32 写，不做任何数值转换。
    fn f32_bits(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
}

// ---------------- 读端 ----------------

/// 顺序读取器：所有读取都先算剩余字节，**从不按声明长度预分配**。
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    tag: u8,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            tag: TAG_OK,
        }
    }

    /// 还剩多少字节。
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// 记下当前段并返回 None（泛型是为了在任意返回类型上都好用）。
    fn fail<T>(&self) -> Option<T> {
        note_failure(self.tag);
        None
    }

    /// **先验算、后分配**的条数检查。
    ///
    /// need = count × item 超过剩余字节就立刻失败 —— 于是「len = u32::MAX、负载 0 字节」
    /// 的伪造快照在**读第一个元素之前**就被拒，绝不触发 Vec::with_capacity(4e9) 那种
    /// 预分配炸弹。返回的 count 一旦通过，就一定不超过 remaining() / item，
    /// 所以调用方按它 with_capacity 是安全的（上界由**输入长度**决定，不由谎言决定）。
    fn count(&self, count: u32, item: usize) -> Option<usize> {
        let n = usize::try_from(count).ok()?;
        let need = match n.checked_mul(item) {
            Some(x) => x,
            None => return self.fail(),
        };
        if need > self.remaining() {
            return self.fail();
        }
        Some(n)
    }

    fn chunk(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.remaining() {
            return self.fail();
        }
        let s = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.chunk(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.chunk(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.chunk(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let b = self.chunk(8)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn f32_bits(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }
}

// ---------------- 快照本体 ----------------

/// 一个 [`Node`] 的完整状态（**不含** cfg，但含它的摘要）。
///
/// 快照是纯值：Node::snapshot() 产出、[`Self::encode`] 落盘、
/// [`Self::decode`] 读回、Node::restore 装回。
///
/// 字段全部私有：快照只能由 Node::snapshot() 或 [`Self::decode`] 产生，
/// 所以「装回一个 Node」之前**一定**经过了一次完整的结构校验。
#[derive(Clone, Debug)]
pub struct NodeSnapshot {
    digest: u64,
    list: PositiveList,
    sampler: RoundBitmap,
    views: BTreeMap<Acc, Received>,
    rng: Rng,
    stat_views: u64,
    stat_rejected: u64,
    stat_rejected_limit: u64,
}

impl NodeSnapshot {
    /// 从一个节点抓取状态。
    pub(crate) fn capture(node: &Node) -> Self {
        Self {
            digest: cfg_digest(&node.cfg),
            list: node.list.clone(),
            sampler: node.sampler.clone(),
            views: node.views.clone(),
            rng: node.rng.clone(),
            stat_views: node.stat_views,
            stat_rejected: node.stat_rejected,
            stat_rejected_limit: node.stat_rejected_limit,
        }
    }

    /// 装回一个节点；cfg 必须与抓取时**逐位一致**（摘要相同），否则返回 None。
    ///
    /// 这里再校验一次摘要是纵深防御：NodeSnapshot 的字段私有，目前只有 capture 与
    /// decode 能造出它、而两者都已校验过一遍；但 restore 是公开的最后一关，
    /// 重复一次比将来某个新入口漏掉强。
    pub(crate) fn into_node(self, cfg: Config) -> Option<Node> {
        if self.digest != cfg_digest(&cfg) {
            return None;
        }
        Some(Node {
            cfg,
            list: self.list,
            sampler: self.sampler,
            views: self.views,
            rng: self.rng,
            stat_views: self.stat_views,
            stat_rejected: self.stat_rejected,
            stat_rejected_limit: self.stat_rejected_limit,
        })
    }

    /// 编码后的字节数（**不分配**，供调用方估算落盘预算）。
    #[must_use]
    pub fn bytes(&self) -> usize {
        let mut n = EMPTY_SNAPSHOT_BYTES;
        n += self.list.len() * RATING_BYTES;
        n += self.sampler.order().len() * 4;
        n += self.sampler.drawn().len() * 8;
        n += self
            .views
            .values()
            .map(|rec| VIEW_HEADER_BYTES + rec.view.len() * SLOT_BYTES)
            .sum::<usize>();
        n
    }

    /// 序列化成自描述的字节串。
    ///
    /// 编码是**确定性**的：同一份状态恒得同一串字节（条目按账户号升序、位图按字序、
    /// 填充区为零），因此 encode -> decode -> encode 逐字节相同。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new(self.bytes());
        w.raw(&MAGIC);
        w.u32(VERSION);
        w.u64(self.digest);

        // ---- 正分清单 ----
        w.u64(self.list.version());
        w.u64(self.list.max_entries() as u64);
        w.u64(self.list.stat_rejected_limit());
        // 条目数：Acc 是 u32、每条 16 字节，实际条目数不可能越过 u32::MAX。
        w.u32(self.list.len() as u32);
        for (acc, entry) in self.list.iter() {
            w.u32(acc);
            w.f32_bits(entry.score);
            w.u64(entry.last_check);
            w.u16(entry.fail_streak);
            w.u16(0); // 对齐填充：恒为 0，解码侧校验
        }

        // ---- 轮次位图（五个字段一个都不能少） ----
        //
        // 少存 version / ready 的后果不是「丢一点进度」而是**分叉**：恢复后 sync()
        // 会因为版本号对不上而重建 order 并 reset()，把「本轮已经抽过谁」整个抹掉，
        // 下一次 tick() 立刻与关机前不同。
        let order = self.sampler.order();
        w.u32(order.len() as u32);
        for &acc in order {
            w.u32(acc);
        }
        let drawn = self.sampler.drawn();
        w.u32(drawn.len() as u32);
        for &word in drawn {
            w.u64(word);
        }
        w.u64(self.sampler.remaining() as u64);
        w.u64(self.sampler.version());
        w.u8(u8::from(self.sampler.ready()));

        // ---- 观点缓存 ----
        w.u32(self.views.len() as u32);
        for (&owner, rec) in &self.views {
            w.u32(owner);
            w.u32(rec.view.cap() as u32);
            w.u32(rec.view.len() as u32);
            for &(acc, q) in rec.view.slots() {
                w.u32(acc);
                w.u16(q);
            }
            w.u64(rec.published_at);
            w.u64(rec.last_check);
        }

        // ---- 尾部 ----
        w.u64(self.rng.state());
        w.u64(self.stat_views);
        w.u64(self.stat_rejected);
        w.u64(self.stat_rejected_limit);

        w.out
    }

    /// 反序列化。**任何**结构不合法、不满足不变式、或与 cfg 不匹配的负载
    /// 都返回 None，且绝不 panic。
    ///
    /// views 里的定长表用 cfg.cap 重建（快照本身不存 cap）。
    #[must_use]
    pub fn decode(bytes: &[u8], cfg: Config) -> Option<Self> {
        let mut r = Reader::new(bytes);

        r.tag = TAG_MAGIC;
        let magic = r.chunk(MAGIC.len())?;
        if magic != MAGIC {
            return r.fail();
        }

        r.tag = TAG_VERSION;
        if r.u32()? != VERSION {
            return r.fail();
        }

        r.tag = TAG_DIGEST;
        let digest = r.u64()?;
        // 摘要为 0 只可能是「未初始化」的负载：合法配置撞上 0 的概率是 2^-64，
        // 而它作为可检测的哨兵值远比这点概率有用。
        if digest == 0 || digest != cfg_digest(&cfg) {
            return r.fail();
        }

        r.tag = TAG_LIST;
        let list = read_list(&mut r)?;

        r.tag = TAG_SAMPLER;
        let sampler = read_sampler(&mut r)?;

        r.tag = TAG_VIEWS;
        let views = read_views(&mut r)?;

        r.tag = TAG_RNG;
        let rng_state = r.u64()?;
        // xorshift 的状态 0 是不动点：恢复它会让 next_u64() 永远返回 0、
        // below() 永远返回 0（抽样退化成「永远抽第 0 个」）。合法运行不可能产出 0。
        if rng_state == 0 {
            return r.fail();
        }

        r.tag = TAG_STATS;
        let stat_views = r.u64()?;
        let stat_rejected = r.u64()?;
        let stat_rejected_limit = r.u64()?;
        // 不变式（node.rs 的「容量上限」一节）：容量拒绝是总拒绝的子集。
        if stat_rejected_limit > stat_rejected {
            return r.fail();
        }

        r.tag = TAG_TRAILING;
        if r.remaining() != 0 {
            return r.fail();
        }

        note_failure(TAG_OK);
        Some(Self {
            digest,
            list,
            sampler,
            views,
            rng: Rng::from_state(rng_state),
            stat_views,
            stat_rejected,
            stat_rejected_limit,
        })
    }
}

// ---------------- 分段解析 ----------------

/// 正分清单段。
fn read_list(r: &mut Reader<'_>) -> Option<PositiveList> {
    let version = r.u64()?;
    let max_entries = match usize::try_from(r.u64()?) {
        Ok(x) => x,
        Err(_) => return r.fail(),
    };
    let stat_rejected_limit = r.u64()?;
    let count = r.u32()?;
    // 先验算：n × 16 字节必须真在负载里，再分配。
    let n = r.count(count, RATING_BYTES)?;

    let mut entries = BTreeMap::new();
    let mut prev: Option<Acc> = None;
    for _ in 0..n {
        let acc = r.u32()?;
        let score = r.f32_bits()?;
        let last_check = r.u64()?;
        let fail_streak = r.u16()?;
        let pad = r.u16()?;
        if pad != 0 {
            return r.fail(); // 填充区必须全零：禁止夹带
        }
        if let Some(p) = prev {
            if acc <= p {
                return r.fail(); // 规范形式：按账户号严格升序
            }
        }
        prev = Some(acc);
        // 值域：try_set 只会产出 (0, 1] 内的有限值。NaN 走 is_finite 被拒，
        // ±∞ 与 > 1 走范围被拒，0 与负数同理 —— 这些只可能来自篡改。
        if !score.is_finite() || score <= 0.0 || score > 1.0 {
            return r.fail();
        }
        entries.insert(
            acc,
            Entry {
                score,
                last_check,
                fail_streak,
            },
        );
    }
    Some(PositiveList::from_parts(
        entries,
        version,
        max_entries,
        stat_rejected_limit,
    ))
}

/// 轮次位图段（五个字段）。重建时由 RoundBitmap::from_parts 逐条校验不变量。
fn read_sampler(r: &mut Reader<'_>) -> Option<RoundBitmap> {
    let order_count = r.u32()?;
    let no = r.count(order_count, 4)?;
    let mut order = Vec::with_capacity(no);
    for _ in 0..no {
        order.push(r.u32()?);
    }

    let drawn_count = r.u32()?;
    let nd = r.count(drawn_count, 8)?;
    let mut drawn = Vec::with_capacity(nd);
    for _ in 0..nd {
        drawn.push(r.u64()?);
    }

    let remaining = match usize::try_from(r.u64()?) {
        Ok(x) => x,
        Err(_) => return r.fail(),
    };
    let version = r.u64()?;
    let ready = match r.u8()? {
        0 => false,
        1 => true,
        _ => return r.fail(),
    };

    match RoundBitmap::from_parts(order, drawn, remaining, version, ready) {
        Some(x) => Some(x),
        None => r.fail(),
    }
}

/// 观点缓存段。每条记录 = 头 + L × 6 字节槽位；表本身用规范槽位入口重建。
/// 每份视图**自带 cap**，所以这里不需要 cfg.cap（见模块文档「必须存每份视图自己的 cap」）。
fn read_views(r: &mut Reader<'_>) -> Option<BTreeMap<Acc, Received>> {
    let count = r.u32()?;
    // 每条记录至少 VIEW_HEADER_BYTES 字节（空表也合法，槽位数可以为 0）。
    let n = r.count(count, VIEW_HEADER_BYTES)?;

    let mut views = BTreeMap::new();
    let mut prev: Option<Acc> = None;
    for _ in 0..n {
        let owner = r.u32()?;
        if let Some(p) = prev {
            if owner <= p {
                return r.fail(); // 规范形式：按 owner 严格升序
            }
        }
        prev = Some(owner);
        // cap 是**这份视图自己的**，不是 cfg.cap（见模块文档「必须存每份视图自己的 cap」）。
        let view_cap = match usize::try_from(r.u32()?) {
            Ok(x) => x,
            Err(_) => return r.fail(),
        };
        let slot_count = r.u32()?;
        // 同样是「先验算再分配」：L × 6 必须真在负载里。
        let ns = r.count(slot_count, SLOT_BYTES)?;
        let mut slots = Vec::with_capacity(ns);
        for _ in 0..ns {
            let acc = r.u32()?;
            let q = r.u16()?;
            slots.push((acc, q));
        }
        let view = match SparseView::from_canonical_slots(view_cap, slots) {
            Some(v) => v,
            None => return r.fail(),
        };
        let published_at = r.u64()?;
        let last_check = r.u64()?;
        views.insert(
            owner,
            Received {
                view,
                published_at,
                last_check,
            },
        );
    }
    Some(views)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Distribution;
    use crate::view::MAX_CAP;
    use std::time::{Duration, Instant};

    /// 测试用的确定性伪随机。故意**不复用** crate 的 [`Rng`]：
    /// 用被测物造测试输入，等于自己给自己出题。
    struct TestRng(u64);

    impl TestRng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn next_u32(&mut self) -> u32 {
            (self.next_u64() >> 32) as u32
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    /// 从**编码串本身**解析出各字段的偏移。
    ///
    /// 为什么不用手算：手算公式一错，测试就在「篡改一个不该被校验的位置」上
    /// 空转（第一版就踩了这个坑：把视图段偏移算小 4 字节，结果改的是别人的字段）。
    /// 这里按模块文档的布局逐段跳过，与编码器走同一条路线；任何一处布局改动都会
    /// 让 `t00_布局解析与编码长度自洽` 立刻失败，而不是让别的测试悄悄失效。
    struct Offsets {
        list_count: usize,
        order_count: usize,
        drawn_count: usize,
        remaining: usize,
        sampler_version: usize,
        ready: usize,
        views_count: usize,
        /// 每份视图记录的起始偏移（按 owner 升序）。
        views: Vec<usize>,
        rng: usize,
        stats: usize,
    }

    fn read_u32(bytes: &[u8], at: usize) -> usize {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }

    fn offsets(bytes: &[u8]) -> Offsets {
        // 清单固定区（version 8 + max_entries 8 + stat_rejected_limit 8 + 条数 4）。
        let mut p = HEADER_BYTES + LIST_HEADER_BYTES;
        let list_count = p - 4;
        let list_len = read_u32(bytes, list_count);
        p += list_len * RATING_BYTES;
        let order_count = p;
        let order_len = read_u32(bytes, order_count);
        p += 4 + order_len * 4;
        let drawn_count = p;
        let drawn_len = read_u32(bytes, drawn_count);
        p += 4 + drawn_len * 8;
        let remaining = p;
        p += 8;
        let sampler_version = p;
        p += 8;
        let ready = p;
        p += 1;
        let views_count = p;
        let views_n = read_u32(bytes, views_count);
        p += 4;
        let mut views = Vec::with_capacity(views_n);
        for _ in 0..views_n {
            views.push(p);
            let slots = read_u32(bytes, p + 8);
            p += VIEW_HEADER_BYTES + slots * SLOT_BYTES;
        }
        // 尾部：rng 8 + 统计 24，位置从**串尾**倒推（与视图份数无关，最稳）。
        let stats = bytes.len() - STATS_BYTES;
        let rng = stats - 8;
        Offsets {
            list_count,
            order_count,
            drawn_count,
            remaining,
            sampler_version,
            ready,
            views_count,
            views,
            rng,
            stats,
        }
    }

    /// 有状态的节点：清单 + 五份观点（cap 与 cfg.cap 混用）+ 半轮抽样 + 连败 + 统计。
    fn rich() -> Node {
        let mut n = Node::new(cfg(), 7);
        for (i, s) in [
            (1u32, 0.9f32),
            (2, 0.3),
            (3, 1.0),
            (4, 0.05),
            (5, 0.5),
            (6, 0.7),
        ] {
            n.rate(i, s, u64::from(i) * 10);
        }
        // 撤销一个本来就不存在的账户：不涨版本、不写任何字段。
        n.rate(99, 0.0, 0);
        let pairs = [(30u32, 1.0f32), (31, 0.5), (32, 0.25)];
        // 每份视图的 cap 都不一样：512（= cfg.cap）、64、128、8。恢复后必须原样保留。
        let caps = [512usize, 64, 128, 8, 8];
        for (k, owner) in [1u32, 2, 3, 4, 5].iter().enumerate() {
            let cap = caps[k];
            let view = SparseView::from_scores(cap, &pairs[..(k % 3 + 1)]);
            assert!(n.on_view(*owner, view, 100 + k as u64, 200 + k as u64));
        }
        // 抽两个出来：本轮剩余 4，drawn 第一字被写、version/ready 就位。
        assert_eq!(n.tick(2).len(), 2);
        // 记一次失败：连败 +1，last_check 不动。
        n.on_view_failed(2);
        // 造两次被拒（空表 / owner 无正分），只涨 stat_rejected、不动 stat_views。
        assert!(!n.on_view(6, SparseView::new(8), 0, 0));
        assert!(!n.on_view(7, SparseView::from_scores(8, &[(40, 1.0)]), 0, 0));
        n
    }

    /// 只装了两份观点、还没抽过的新节点（视图段的偏移因此是固定已知的）。
    fn views_only() -> Node {
        let mut n = Node::new(cfg(), 11);
        n.rate(1, 0.9, 0);
        n.rate(2, 0.5, 0);
        assert!(n.on_view(1, SparseView::from_scores(8, &[(30, 1.0), (31, 1.0)]), 0, 0));
        assert!(n.on_view(2, SparseView::from_scores(8, &[(32, 1.0)]), 0, 0));
        n
    }

    /// 把节点的分布摊成 (账户, f32 位模式)，用于逐位比较。
    fn bits(d: &Distribution) -> Vec<(Acc, u32)> {
        d.entries.iter().map(|&(a, v)| (a, v.to_bits())).collect()
    }

    /// 走完整条链：快照 -> 编码 -> 解码 -> 装回。
    fn round_trip(n: &Node) -> (Vec<u8>, Node) {
        let snap = n.snapshot();
        let bytes = snap.encode();
        let decoded = NodeSnapshot::decode(&bytes, n.cfg).expect("解码必须成功");
        let restored = Node::restore(decoded, n.cfg).expect("恢复必须成功");
        (bytes, restored)
    }

    // ---------------- 往返 ----------------

    #[test]
    fn t01_空节点的快照字节数等于常量() {
        let n = Node::new(cfg(), 3);
        let snap = n.snapshot();
        assert_eq!(snap.bytes(), EMPTY_SNAPSHOT_BYTES);
        assert_eq!(snap.encode().len(), EMPTY_SNAPSHOT_BYTES);
        // 与模块文档里手算的 110 一致。
        assert_eq!(EMPTY_SNAPSHOT_BYTES, 110);
    }

    #[test]
    fn t02_空节点往返逐字节相同() {
        let n = Node::new(cfg(), 3);
        let (bytes, restored) = round_trip(&n);
        assert_eq!(
            restored.snapshot().encode(),
            bytes,
            "空节点也必须逐字节往返"
        );
        assert_eq!(restored.claims().len(), 0);
        assert_eq!(restored.view_count(), 0);
        assert_eq!(restored.stats(), (0, 0));
        assert_eq!(restored.rng.state(), n.rng.state(), "rng 状态必须原样装回");
        assert!(!restored.sampler.ready(), "新建节点的采样器是「还没同步」");
    }

    #[test]
    fn t03_空节点恢复后抽样仍为空且不消耗随机数() {
        let mut a = Node::new(cfg(), 5);
        assert!(a.tick(3).is_empty());
        let (_, mut b) = round_trip(&a);
        assert!(b.tick(3).is_empty());
        assert!(b.probe().is_empty());
        assert_eq!(a.rng.state(), b.rng.state(), "空清单上的 tick 不吃随机数");
        assert_eq!(a.round_remaining(), b.round_remaining());
    }

    #[test]
    fn t04_有状态节点往返逐字节相同() {
        let a = rich();
        let (bytes, restored) = round_trip(&a);
        assert_eq!(
            restored.snapshot().encode(),
            bytes,
            "encode -> decode -> encode 必须逐字节相同"
        );
    }

    #[test]
    fn t05_快照字节数函数与实际编码长度一致() {
        let a = rich();
        assert_eq!(a.snapshot().bytes(), a.snapshot().encode().len());
        let e = Node::new(cfg(), 3);
        assert_eq!(e.snapshot().bytes(), e.snapshot().encode().len());
    }

    // ---------------- 行为逐位 ----------------

    #[test]
    fn t06_恢复后分布逐位相同() {
        let a = rich();
        let (_, b) = round_trip(&a);
        let now = 700;
        assert_eq!(bits(&a.distribution(now)), bits(&b.distribution(now)));
        assert_eq!(a.distribution(now).mass, b.distribution(now).mass);
    }

    #[test]
    fn t07_恢复后query逐个逐位相同() {
        let a = rich();
        let (_, b) = round_trip(&a);
        for acc in 0..48u32 {
            assert_eq!(
                a.query(acc, 700).to_bits(),
                b.query(acc, 700).to_bits(),
                "账户 {acc} 的 query 位模式必须相同"
            );
        }
    }

    #[test]
    fn t08_恢复后下一次tick抽到的人完全相同() {
        let mut a = rich();
        let (_, mut b) = round_trip(&a);
        let ra = a.tick(3);
        let rb = b.tick(3);
        assert_eq!(ra, rb, "恢复后第一次 tick 必须抽到同一批人");
        assert_eq!(ra.len(), 3);
        assert!(!ra.is_empty());
    }

    #[test]
    fn t09_恢复后连续十次tick逐次相同() {
        let mut a = rich();
        let (_, mut b) = round_trip(&a);
        for round in 0..10 {
            let ra = a.tick(2);
            let rb = b.tick(2);
            assert_eq!(ra, rb, "第 {round} 次 tick 分叉");
            assert_eq!(a.round_remaining(), b.round_remaining());
            assert_eq!(
                a.rng.state(),
                b.rng.state(),
                "第 {round} 次 tick 后随机数状态分叉"
            );
        }
    }

    #[test]
    fn t10_恢复后probe取到的整轮剩余完全相同() {
        let mut a = rich();
        let (_, mut b) = round_trip(&a);
        assert_eq!(a.round_remaining(), 4);
        assert_eq!(a.probe(), b.probe());
        assert_eq!(a.round_remaining(), 0);
        assert_eq!(a.round_remaining(), b.round_remaining());
    }

    #[test]
    fn t11_半轮进度原样恢复() {
        let a = rich();
        let (_, b) = round_trip(&a);
        assert_eq!(
            a.sampler.order(),
            b.sampler.order(),
            "本轮 order 必须原样恢复"
        );
        assert_eq!(a.sampler.drawn(), b.sampler.drawn(), "已抽位图必须原样恢复");
        assert_eq!(a.sampler.remaining(), b.sampler.remaining());
        assert_eq!(a.round_remaining(), 4, "6 个人抽掉 2 个，还剩 4 个");
    }

    #[test]
    fn t12_位图version与ready原样恢复() {
        let a = rich();
        let (_, b) = round_trip(&a);
        assert_eq!(
            a.sampler.version(),
            b.sampler.version(),
            "version 丢了会导致恢复后重建 order"
        );
        assert_eq!(a.sampler.ready(), b.sampler.ready());
        assert!(b.sampler.ready(), "抽过一轮的节点恢复后必须是「已同步」");
        assert_eq!(b.sampler.version(), a.claims().version());
    }

    #[test]
    fn t13_连败计数与last_check原样恢复() {
        let a = rich();
        let (_, b) = round_trip(&a);
        assert_eq!(a.claims().fail_streak(2), 1);
        assert_eq!(b.claims().fail_streak(2), 1);
        for acc in 1..=6u32 {
            assert_eq!(a.claims().last_check(acc), b.claims().last_check(acc));
            assert_eq!(
                a.claims().get(acc).map(f32::to_bits),
                b.claims().get(acc).map(f32::to_bits)
            );
        }
        assert_eq!(a.claims().version(), b.claims().version());
    }

    #[test]
    fn t14_收发统计原样恢复() {
        let a = rich();
        let (_, b) = round_trip(&a);
        assert_eq!(a.stats(), (5, 2));
        assert_eq!(b.stats(), a.stats(), "stat_views / stat_rejected 必须一致");
        assert_eq!(b.stat_rejected_limit(), a.stat_rejected_limit());
    }

    #[test]
    fn t34_观点段被篡改时逐项拒绝() {
        let n = views_only();
        let bytes = n.snapshot().encode();
        let ofs = offsets(&bytes);
        let first_view = ofs.views[0];
        // (a) owner 非升序：第二条的 owner 改成 0（比第一条的 1 小 ⟹ 降序）。
        //     注意不能改成 u32::MAX ——那仍然是升序，会**合法通过**。
        let second_view = ofs.views[1];
        let mut bad = bytes.clone();
        bad[second_view..second_view + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(
            NodeSnapshot::decode(&bad, cfg()).is_none(),
            "owner 降序必须被拒"
        );
        assert_eq!(last_failure_tag(), 6);
        // (b) 视图 cap = 0
        let mut bad = bytes.clone();
        bad[first_view + 4..first_view + 8].copy_from_slice(&0u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        // (c) 视图 cap = MAX_CAP + 1
        let mut bad = bytes.clone();
        bad[first_view + 4..first_view + 8].copy_from_slice(&((MAX_CAP + 1) as u32).to_le_bytes());
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        // (d) 槽位数超过 cap（cap = 8，槽位数写成 9）
        let mut bad = bytes.clone();
        bad[first_view + 8..first_view + 12].copy_from_slice(&9u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        assert_eq!(last_failure_tag(), 6);
        // (e) 槽位 q = 0（第一个槽位的 q 在记录起点 + 12 + 4）
        let mut bad = bytes.clone();
        let q_at = first_view + 12 + 4;
        bad[q_at..q_at + 2].copy_from_slice(&0u16.to_le_bytes());
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        // (f) 槽位非升序（把第一个槽位的 acc 改到最大）
        let mut bad = bytes.clone();
        bad[first_view + 12..first_view + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        // (g) 槽位数撒谎
        let mut bad = bytes.clone();
        bad[first_view + 8..first_view + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        let started = Instant::now();
        assert!(NodeSnapshot::decode(&bad, cfg()).is_none());
        assert!(started.elapsed() < Duration::from_millis(20));
        assert_eq!(last_failure_tag(), 6);
    }

    #[test]
    fn t35_逐字节逐位翻转全部不panic() {
        // 穷举：对整串快照的**每一个字节的每一位**各翻一次。断言两条：
        // (1) 绝不 panic；(2) 能解出来的话，必须能逐字节重编（说明改动落在有效载荷里）。
        let a = rich();
        let bytes = a.snapshot().encode();
        let mut decoded = 0usize;
        let mut rejected = 0usize;
        for i in 0..bytes.len() {
            for bit in 0..8u32 {
                let mut probe = bytes.clone();
                probe[i] ^= 1u8 << bit;
                match NodeSnapshot::decode(&probe, cfg()) {
                    Some(s) => {
                        assert_eq!(
                            s.encode(),
                            probe,
                            "字节 {i} 位 {bit}：能解出就必须逐字节重编"
                        );
                        decoded += 1;
                    }
                    None => rejected += 1,
                }
            }
        }
        // 防退化抽查：必须有相当比例的位翻转被拒（否则说明这个测试在空转）。
        // 实测约 55%：剩下的是「合法但改语义」的字段（last_check / published_at /
        // 各计数 / 清单版本 / 位图 version），它们没有值域约束，改了就只是换一份合法状态。
        assert!(
            rejected * 10 >= bytes.len() * 8 * 4,
            "位翻转被拒比例过低（实际 decoded = {decoded}, rejected = {rejected}, total = {}）",
            bytes.len() * 8
        );
        assert_eq!(decoded + rejected, bytes.len() * 8);
    }

    #[test]
    fn t36_视图cap小于槽位数的负载被拒() {
        let n = views_only();
        let mut bytes = n.snapshot().encode();
        let first_view = offsets(&bytes).views[0];
        bytes[first_view + 4..first_view + 8].copy_from_slice(&1u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 6);
    }

    #[test]
    fn t37_单槽视图能往返() {
        let mut n = Node::new(cfg(), 3);
        n.rate(1, 0.5, 0);
        assert!(n.on_view(1, SparseView::from_scores(8, &[(2, 1.0)]), 0, 0));
        let (bytes, b) = round_trip(&n);
        assert_eq!(b.snapshot().encode(), bytes);
        assert_eq!(b.view_count(), 1);
        assert_eq!(b.views[&1].view.slots().len(), 1);
    }

    #[test]
    fn t38_大负载往返_512视图全槽位() {
        let mut n = Node::new(cfg(), 9);
        for i in 0..64u32 {
            n.rate(i, 0.9, 0);
        }
        // 每个 owner 发一份塞满 cap 的视图。
        for i in 0..64u32 {
            let pairs: Vec<(u32, f32)> = (0..512).map(|k| (1000 + i * 512 + k, 0.5)).collect();
            assert!(n.on_view(i, SparseView::from_scores(512, &pairs), 0, 0));
        }
        let (bytes, b) = round_trip(&n);
        assert_eq!(b.snapshot().encode(), bytes);
        assert_eq!(b.view_count(), 64);
        // 手算：空节点基线 + 清单 64 条 + 64 份满槽视图。
        // 注意 order / drawn 都是**空**的：这个节点从没抽过，采样器还没 sync。
        let expect =
            EMPTY_SNAPSHOT_BYTES + 64 * RATING_BYTES + 64 * (VIEW_HEADER_BYTES + 512 * SLOT_BYTES);
        assert_eq!(bytes.len(), expect, "字节数必须与手算公式一致");
        assert!(
            bytes.len() > 190_000,
            "这份负载应当接近 20 万字节（实际 {}）",
            bytes.len()
        );
    }

    // ---------------- 与配置 / 资源上限的交互 ----------------

    #[test]
    fn t39_摘要对不同max_views与max_list敏感() {
        let a = Node::new(
            Config {
                max_views: 4096,
                ..cfg()
            },
            3,
        );
        let bytes = a.snapshot().encode();
        assert!(NodeSnapshot::decode(
            &bytes,
            Config {
                max_views: 2048,
                ..cfg()
            }
        )
        .is_none());
        assert!(NodeSnapshot::decode(
            &bytes,
            Config {
                max_list: 2048,
                ..cfg()
            }
        )
        .is_none());
        assert!(NodeSnapshot::decode(
            &bytes,
            Config {
                max_views: 0,
                max_list: 0,
                ..cfg()
            }
        )
        .is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn t40_恢复时配置不符也返回None() {
        let a = rich();
        let snap = a.snapshot();
        assert!(Node::restore(snap.clone(), Config { beta: 0.5, ..cfg() }).is_none());
        assert!(Node::restore(snap, cfg()).is_some());
    }

    #[test]
    fn t41_清单容量拒绝计数原样恢复() {
        // max_list = 3：第 4 次新增被拒，计数 +1、清单版本不动。
        let mut a = Node::new(
            Config {
                max_list: 3,
                ..cfg()
            },
            3,
        );
        for i in 1..=3u32 {
            assert!(a.try_rate(i, 0.5, 0));
        }
        assert!(!a.try_rate(4, 0.5, 0));
        assert_eq!(a.stat_rejected_list(), 1);
        let (_, b) = round_trip(&a);
        assert_eq!(b.stat_rejected_list(), 1, "清单容量拒绝计数必须原样恢复");
        assert_eq!(b.claims().max_entries(), 3, "清单上限本身也必须入档");
        assert_eq!(b.claims().len(), 3);
    }

    #[test]
    fn t42_视图容量拒绝计数原样恢复() {
        let mut a = Node::new(
            Config {
                max_views: 2,
                ..cfg()
            },
            3,
        );
        for i in 1..=3u32 {
            a.rate(i, 0.5, 0);
        }
        assert!(a.on_view(1, SparseView::from_scores(8, &[(9, 1.0)]), 0, 0));
        assert!(a.on_view(2, SparseView::from_scores(8, &[(9, 1.0)]), 0, 0));
        assert!(!a.on_view(3, SparseView::from_scores(8, &[(9, 1.0)]), 0, 0));
        assert_eq!(a.stat_rejected_limit(), 1);
        let (_, b) = round_trip(&a);
        assert_eq!(b.stats(), a.stats());
        assert_eq!(b.stat_rejected_limit(), 1, "视图容量拒绝计数必须原样恢复");
    }

    #[test]
    fn t43_恢复后记录失败接着之前的值往上加() {
        let (_, mut b) = round_trip(&rich());
        assert_eq!(b.claims().fail_streak(2), 1);
        b.on_view_failed(2);
        assert_eq!(
            b.claims().fail_streak(2),
            2,
            "恢复后连败必须接着之前的值往上加"
        );
        assert_eq!(b.claims().last_check(2), Some(201), "失败不动 last_check");
    }

    #[test]
    fn t44_clone出来的快照不改变任何东西() {
        let a = rich();
        let s = a.snapshot();
        let desc = s.bytes();
        assert_eq!(desc, s.encode().len());
        let c = s.clone();
        assert_eq!(c.bytes(), desc);
        assert_eq!(c.encode(), s.encode());
    }

    #[test]
    fn t46_实测字节数钉住() {
        // 把「某类状态占多少字节」钉成回归断言：格式一改就会在这里炸，
        // 而不是等到线上快照变大一倍才发现。
        //
        // 空节点 110 = 头 21 + 清单固定 28 + 清单条数 4 + order 条数 4
        //             + 位图尾 17 + 观点条数 4 + rng 8 + 统计 24。
        assert_eq!(Node::new(cfg(), 3).snapshot().bytes(), 110);
        // rich()：110 基线 + 6 条正分（6×16 = 96）+ order 6 项（6×4 = 24）
        //         + drawn 1 字（8）+ 5 份视图（5×28 头 + 12 个槽 × 6 = 212）= 456。
        //         （视图槽数：pairs[..1] / [..2] / [..3] 各一份 ×3 位来源 = 1+2+3+1+2+3 = 12。）
        assert_eq!(rich().snapshot().bytes(), 456);
        // 64 份塞满 cap=512 的视图：110 + 64×16（正分）+ 64×(28 + 512×6)（视图）。
        // ⚠️ 公式里**没有** order/drawn 项：那个节点从没抽过，采样器还没 sync。
        // 用常量而不是字面量算，避免「常量改了这条断言还硬编码旧数」的假绿。
        assert_eq!(
            EMPTY_SNAPSHOT_BYTES + 64 * RATING_BYTES + 64 * (VIEW_HEADER_BYTES + 512 * SLOT_BYTES),
            199_790
        );
    }

    #[test]
    fn t45_配置摘要覆盖全部六个旋钮() {
        // 逐个旋钮单独改动，摘要必须都变（等价于「没有哪个旋钮被漏掉」）。
        let base = cfg();
        let cfgs = [
            base,
            Config {
                beta: 0.300_001,
                ..base
            },
            Config {
                half_life: base.half_life + 1,
                ..base
            },
            Config {
                cap: base.cap + 1,
                ..base
            },
            Config {
                decay_enabled: !base.decay_enabled,
                ..base
            },
            Config {
                max_views: base.max_views + 1,
                ..base
            },
            Config {
                max_list: base.max_list + 1,
                ..base
            },
        ];
        let mut seen: Vec<u64> = Vec::new();
        for c in &cfgs {
            let d = cfg_digest(c);
            assert_ne!(d, 0, "合法配置的摘要不该是 0（那是保留给伪造负载的哨兵）");
            assert!(!seen.contains(&d), "两个不同配置撞上了同一个摘要：{c:?}");
            seen.push(d);
        }
        assert_eq!(seen.len(), 7);
    }

    #[test]
    fn t15_每份视图逐字节恢复且各自的cap保留() {
        let a = rich();
        let (_, b) = round_trip(&a);
        assert_eq!(a.view_count(), b.view_count(), "观点份数必须一致");
        for (&owner, rec_a) in &a.views {
            let rec_b = &b.views[&owner];
            assert_eq!(
                rec_a.view.cap(),
                rec_b.view.cap(),
                "owner {owner} 的视图 cap 必须原样保留"
            );
            assert_eq!(
                rec_a.view.encode(),
                rec_b.view.encode(),
                "owner {owner} 的视图必须逐字节相同"
            );
            assert_eq!(rec_a.view.slots(), rec_b.view.slots());
            assert_eq!(rec_a.published_at, rec_b.published_at);
            assert_eq!(rec_a.last_check, rec_b.last_check);
        }
        // cap 混用是刻意造的：512 / 64 / 128 / 8 —— 全部保住。
        let caps: Vec<usize> = b.views.values().map(|r| r.view.cap()).collect();
        assert_eq!(caps, vec![512, 64, 128, 8, 8]);
    }

    #[test]
    fn t16_编码是确定性的两次编码逐字节相同() {
        let a = rich();
        let s = a.snapshot();
        assert_eq!(s.encode(), s.encode(), "同一份快照两次编码必须相同");
        // 再抓一次（clone 出来的快照与重新抓取的必须一致）。
        assert_eq!(a.snapshot().encode(), s.encode());
    }

    #[test]
    fn t17_恢复后的节点可以继续被快照() {
        let a = rich();
        let (bytes, b) = round_trip(&a);
        // 恢复 -> 再抓 -> 再编码：仍然是同一串字节。
        assert_eq!(b.snapshot().encode(), bytes);
        // 再走一次 decode/restore 链路。
        let c = Node::restore(NodeSnapshot::decode(&bytes, cfg()).unwrap(), cfg()).unwrap();
        assert_eq!(c.snapshot().encode(), bytes);
    }

    // ---------------- 防御 ----------------

    #[test]
    fn t18_每个前缀截断都必须被拒且不panic() {
        let a = rich();
        let bytes = a.snapshot().encode();
        for i in 0..bytes.len() {
            assert!(
                NodeSnapshot::decode(&bytes[..i], cfg()).is_none(),
                "截断到 {i} 字节的负载必须被拒绝"
            );
        }
        // 完整长度仍然可以解。
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_some());
    }

    #[test]
    fn t19_空负载与超短负载被拒() {
        for len in 0..HEADER_BYTES {
            assert!(NodeSnapshot::decode(&vec![b'X'; len], cfg()).is_none());
            assert_eq!(
                last_failure_tag(),
                1,
                "读不到魔数必须在魔数处失败（len = {len}）"
            );
        }
        // 注意：失败标记只在下一次**成功**解码时清零（见 t25 末尾），
        // 所以这里仍然是「魔数」——这本身就是一条契约：不要期望失败会自己消失。
        assert_eq!(last_failure_tag(), 1);
    }

    #[test]
    fn t20_末尾多余字节被拒() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        bytes.push(0);
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 9, "必须是在「末尾多余字节」这一关被拒");
        // 末尾接一整个合法快照也不行。
        let mut bytes2 = a.snapshot().encode();
        bytes2.extend_from_slice(&Node::new(cfg(), 1).snapshot().encode());
        assert!(NodeSnapshot::decode(&bytes2, cfg()).is_none());
    }

    #[test]
    fn t21_魔数不符被拒() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        bytes[0] ^= 0xFF;
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 1, "必须在魔数这一关被拒");
    }

    #[test]
    fn t22_版本号不符被拒() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        bytes[HEADER_BYTES - 12] = 2; // 版本字段在 8..12
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 2, "必须在版本这一关被拒");
        // 版本 = 0 同样拒绝。
        let mut v0 = a.snapshot().encode();
        v0[HEADER_BYTES - 12..HEADER_BYTES - 8].copy_from_slice(&0u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&v0, cfg()).is_none());
    }

    #[test]
    fn t23_六个cfg旋钮任一不符都被拒() {
        let a = rich();
        let bytes = a.snapshot().encode();
        let base_cfg = cfg();
        let variants = [
            Config {
                beta: 0.31,
                ..base_cfg
            },
            Config {
                half_life: 73,
                ..base_cfg
            },
            Config {
                cap: 256,
                ..base_cfg
            },
            Config {
                decay_enabled: false,
                ..base_cfg
            },
            Config {
                max_views: 0,
                ..base_cfg
            },
            Config {
                max_list: 0,
                ..base_cfg
            },
            // 位语义：-0.0 与 0.0 的 bits 不同，但都合法；beta 改成 -0.0 也必须是「不同配置」。
            Config {
                beta: -0.0,
                ..base_cfg
            },
        ];
        for (i, c) in variants.iter().enumerate() {
            assert!(
                NodeSnapshot::decode(&bytes, *c).is_none(),
                "第 {i} 个变体配置不该解出快照"
            );
            assert_eq!(last_failure_tag(), 3, "第 {i} 个变体必须在摘要这一关被拒");
        }
        // 对照组：原配置当然能解。
        assert!(NodeSnapshot::decode(&bytes, base_cfg).is_some());
    }

    #[test]
    fn t24_手工摘要为0的负载被拒() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        bytes[HEADER_BYTES - 8..HEADER_BYTES].copy_from_slice(&0u64.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 3);
    }

    #[test]
    fn t25_随机字节一律不panic() {
        let mut r = TestRng::new(0xDEAD_BEEF);
        for round in 0..300 {
            let len = (r.next_u32() % 400) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (r.next_u64() >> 56) as u8).collect();
            if let Some(snap) = NodeSnapshot::decode(&bytes, cfg()) {
                // 极少数情况下随机串可能恰好合法：那就必须能原样再编回去。
                assert_eq!(
                    snap.encode(),
                    bytes,
                    "第 {round} 轮：能解出的负载必须逐字节重编"
                );
            }
        }
    }

    #[test]
    fn t26_清单条数撒谎不会预分配() {
        let a = Node::new(cfg(), 3);
        let base = a.snapshot().encode();
        // 空节点的清单条数字段 = HEADER(21) + 24。
        let at = HEADER_BYTES + 24;
        let mut bytes = base.clone();
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let started = Instant::now();
        assert!(
            NodeSnapshot::decode(&bytes, cfg()).is_none(),
            "撒谎的条数必须被拒"
        );
        let elapsed = started.elapsed();
        // 「先验算再分配」的意义就在这条断言上：4e9 条 × 16 字节 = 64 GB，
        // 真预分配的话这次调用不是 OOM 就是几十毫秒起。
        assert!(
            elapsed < Duration::from_millis(20),
            "条数撒谎必须立刻被拒（实测 {elapsed:?}）"
        );
        assert_eq!(last_failure_tag(), 4);
    }

    #[test]
    fn t27_位图两个数组的条数撒谎都被拒() {
        let a = rich();
        let bytes0 = a.snapshot().encode();
        let ofs = offsets(&bytes0);
        for at in [ofs.order_count, ofs.drawn_count] {
            let mut bytes = a.snapshot().encode();
            bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            let started = Instant::now();
            assert!(
                NodeSnapshot::decode(&bytes, cfg()).is_none(),
                "偏移 {at} 的条数撒谎必须被拒"
            );
            assert!(started.elapsed() < Duration::from_millis(20));
            assert_eq!(last_failure_tag(), 5);
        }
        // 观点条数同样。
        let mut bytes = a.snapshot().encode();
        bytes[ofs.views_count..ofs.views_count + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 6);
    }

    #[test]
    fn t28_清单分数非有限或越界都被拒() {
        let a = Node::new(cfg(), 3);
        let mut a = a;
        a.rate(7, 0.5, 5);
        let base_bytes = a.snapshot().encode();
        // 第一条 score 的位模式字段：HEADER(21) + 24 + 4 + 4（acc 之后）。
        let score_at = HEADER_BYTES + 24 + 4 + 4;
        let bad: [u32; 9] = [
            0x7FC0_0000, // NaN
            0xFFC0_0000, // -NaN
            0x7F80_0000, // +inf
            0xFF80_0000, // -inf
            0x8000_0000, // -0.0
            0x0000_0000, // +0.0
            0x3F80_0001, // 略大于 1.0
            0xBF80_0000, // -1.0
            0x7F7F_FFFF, // 最大有限值（远大于 1）
        ];
        for (i, bits) in bad.iter().enumerate() {
            let mut bytes = base_bytes.clone();
            bytes[score_at..score_at + 4].copy_from_slice(&bits.to_le_bytes());
            assert!(
                NodeSnapshot::decode(&bytes, cfg()).is_none(),
                "第 {i} 个非法 score 必须被拒"
            );
            assert_eq!(last_failure_tag(), 4);
        }
        // 合法边界仍然通过：1.0 与最小正规格化数。
        for bits in [0x3F80_0000u32, 0x0080_0000] {
            let mut bytes = base_bytes.clone();
            bytes[score_at..score_at + 4].copy_from_slice(&bits.to_le_bytes());
            assert!(
                NodeSnapshot::decode(&bytes, cfg()).is_some(),
                "{bits:#x} 是合法 score"
            );
        }
    }

    #[test]
    fn t29_清单条目乱序或重复被拒() {
        let mut a = Node::new(cfg(), 3);
        a.rate(7, 0.5, 5);
        a.rate(8, 0.5, 5);
        let bytes = a.snapshot().encode();
        let at = HEADER_BYTES + 24 + 4;
        // 把第二条的 acc 改成第一条的 acc（重复且乱序）。
        let mut dup = bytes.clone();
        dup[at + RATING_BYTES..at + RATING_BYTES + 4].copy_from_slice(&7u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&dup, cfg()).is_none());
        assert_eq!(last_failure_tag(), 4);
        // 只把第二条改成更小的 acc（单纯乱序）。
        let mut unsorted = bytes.clone();
        unsorted[at + RATING_BYTES..at + RATING_BYTES + 4].copy_from_slice(&6u32.to_le_bytes());
        assert!(NodeSnapshot::decode(&unsorted, cfg()).is_none());
        assert_eq!(last_failure_tag(), 4);
    }

    #[test]
    fn t30_清单填充非零被拒() {
        let mut a = Node::new(cfg(), 3);
        a.rate(7, 0.5, 5);
        let mut bytes = a.snapshot().encode();
        let ofs = offsets(&bytes);
        // 第一条记录的最后两字节是填充（acc 4 + score 4 + last_check 8 = 16，减去填充 2）。
        let pad_at = ofs.list_count + 4 + RATING_BYTES - 2;
        bytes[pad_at] = 1;
        assert!(
            NodeSnapshot::decode(&bytes, cfg()).is_none(),
            "填充区夹带必须被拒"
        );
        assert_eq!(last_failure_tag(), 4, "必须在清单段被拒");
    }

    #[test]
    fn t31_位图五个字段各自被篡改都被拒() {
        let a = rich();
        let bytes0 = a.snapshot().encode();
        let ofs = offsets(&bytes0);
        // (0) version 字段必须存在且与清单版本一致（这条同时消掉「未读字段」警告）。
        assert_eq!(
            u64::from_le_bytes(
                bytes0[ofs.sampler_version..ofs.sampler_version + 8]
                    .try_into()
                    .unwrap()
            ),
            a.claims().version(),
            "位图 version 必须等于清单版本"
        );
        // (a) ready 非 0/1
        for bad in [2u8, 0xFFu8] {
            let mut bytes = a.snapshot().encode();
            bytes[ofs.ready] = bad;
            assert!(
                NodeSnapshot::decode(&bytes, cfg()).is_none(),
                "ready = {bad} 必须被拒"
            );
            assert_eq!(last_failure_tag(), 5);
        }
        // (b) remaining 与位图不一致（改成 0，而实际上还剩 4）
        let mut bytes = a.snapshot().encode();
        bytes[ofs.remaining..ofs.remaining + 8].copy_from_slice(&0u64.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 5);
        // (c) remaining 超出 order 长度
        let mut bytes = a.snapshot().encode();
        bytes[ofs.remaining..ofs.remaining + 8].copy_from_slice(&99u64.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        // (d) order 非升序（把第一个 acc 改到最大）
        let mut bytes = a.snapshot().encode();
        bytes[ofs.order_count + 4..ofs.order_count + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 5);
        // (e) drawn 位图的越界位没预置为 1（最后一字的最高位是越界位：6 个候选）
        let mut bytes = a.snapshot().encode();
        let last_word_at = ofs.drawn_count + 4;
        let mut w = u64::from_le_bytes(bytes[last_word_at..last_word_at + 8].try_into().unwrap());
        w &= !(u64::MAX << 6); // 把越界位清掉
        bytes[last_word_at..last_word_at + 8].copy_from_slice(&w.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 5);
    }

    #[test]
    fn t32_随机源状态为0被拒() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        let ofs = offsets(&bytes);
        bytes[ofs.rng..ofs.rng + 8].copy_from_slice(&0u64.to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 7, "xorshift 的 0 是不动点，必须拒绝");
    }

    #[test]
    fn t33_统计不变式被破坏时拒绝() {
        let a = rich();
        let mut bytes = a.snapshot().encode();
        let ofs = offsets(&bytes);
        // stat_rejected_limit 必须 <= stat_rejected；把前者改成后者 + 1。
        let rejected = u64::from_le_bytes(bytes[ofs.stats + 8..ofs.stats + 16].try_into().unwrap());
        bytes[ofs.stats + 16..ofs.stats + 24].copy_from_slice(&(rejected + 1).to_le_bytes());
        assert!(NodeSnapshot::decode(&bytes, cfg()).is_none());
        assert_eq!(last_failure_tag(), 8);
    }
}
