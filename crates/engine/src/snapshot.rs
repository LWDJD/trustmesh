//! # snapshot —— 引擎完整状态的定长字节快照（P4）
//!
//! 引擎重启后必须能**逐位回到**关机前的状态：正分清单、观点缓存、账户编号、
//! 折扣表、时钟、统计、抽样轮次进度，以及抽样随机流的**内部状态**。
//! 少了任何一样，恢复出来的引擎都会在下一步分叉 —— 而且这种分叉不是
//! 「慢慢偏一点」，而是「同一次 tick 抽到不同的人」的立刻崩坏。
//!
//! 本模块提供三层：
//!
//! 1. [EngineSnapshot] / [ViewSnapshot]：纯数据结构，只装**运行状态**，不含 `cfg`；
//! 2. [EngineSnapshot::encode] / [EngineSnapshot::decode]：手写定长字节格式（无 serde）；
//! 3. [Engine::snapshot] / [Engine::restore]：引擎两侧的进出口。
//!
//! ## 格式（版本 1）
//!
//! 全部整数小端；全部浮点用 `to_bits()` 的位模式原样搬运（**不是**十进制往返），
//! 这是「恢复后逐位一致」的前提：`0.1 + 0.2` 这类值只有位模式能保证一字不差。
//!
//! ~~~text
//! 偏移   字段
//! 0      魔数 "TMSNAPSH"（8 字节）
//! 8      版本号 u32（当前 = 1；先读版本再解析 = 自描述）
//! 12     view_params_digest: u64（视图参数指纹，见下）
//! 20     accounts:      u32 个数 n + n × 32 字节公钥（下标即编号）
//! ...    ratings:       u32 个数 n + n × (u32 编号, u32 评分位模式, u64 last_check, u32 fail_streak)
//! ...    round_members: u32 个数 n + n × u32（本轮全量清单，升序）
//! ...    round:         u32 个数 n + n × u32（本轮剩余待抽，顺序即语义）
//! ...    views:         u32 个数 n + n × (u32 编号, u32 字节数 L, L 字节, u64 published_at,
//!                                              u64 last_check, f32×3 噪声底位模式,
//!                                              f32 total_mass 位模式, u32 mass_covers)
//! ...    penalties:     u32 个数 n + n × (u32 编号, u32 折扣位模式)
//! ...    clock:         u64
//! ...    stat_requests: u64
//! ...    stat_hits:     u64
//! ...    rng_state:     u64
//! 末尾   不得有多余字节（多出来的字节视为畸形）
//! ~~~
//!
//! 解码是**完全防御性**的：任何截断 / 版本不符 / 长度矛盾 / 非有限浮点一律
//! 返回 None，绝不 panic，也绝不按畸形长度预分配（先验算「剩余字节够不够
//! 装下这么多个元素」再 with_capacity）。
//!
//! 真正的语义校验（观点字节能否用 cfg.view_params 解出来）只有
//! [Engine::restore] 能做 —— 它才知道 cfg。
//!
//! ## 为什么快照不含 cfg
//!
//! cfg 是**部署配置**，不是运行状态。同一条快照要能在不同参数下被**拒绝**
//! （参数不匹配 → None），而不是被错误地「顺着配置继续跑」。调用方自己保管
//! cfg，恢复时原样传入 [Engine::restore]。
//!
//! 观点缓存里每份观点都是按某个 ViewParams 压缩出来的字节，restore 会用
//! 传入的参数解一遍来验证「这份快照确实属于这套参数」：buckets / hashers /
//! seed 任一不同，长度或桶布局就对不上，于是整份快照被拒。
//!
//! ### view_params_digest：为什么光靠 decode 不够
//!
//! CompressedView::decode 只校验**长度**与「桶值有限」，不校验 seed：
//! 同一组 buckets/hashers 配不同 seed 时，字节长度完全一样，decode 照样成功，
//! 但桶布局已经整体错位 —— 恢复出来的引擎会带着一份**读出来是别的东西**的观点
//! 继续跑，而且不报任何错。这是比「拒绝恢复」严重得多的失败模式。
//!
//! 所以快照额外带一份参数指纹（buckets / hashers / seed 的 FNV-1a 64 位摘要）。
//! 它是**指纹不是配置**：不可逆、不参与任何计算，只用于 restore 时回答
//! 「这份字节还是不是当初那套参数产出的」。指纹不符 → None，三个参数任一不同
//! 都能被抓住。空观点缓存的快照同样受这条约束：参数必须一致。
//!
//! 指纹只是**便宜的早退检查**：真正的证明仍是 CompressedView::decode
//! （它保证每个桶值都能被解释成有限的 f32）。
//!
//! ## 为什么 round_members 也要存
//!
//! 任务书里的结构只列了 round（= [RoundSampler::pending]）。但轮次抽样器
//! 有两份状态：**本轮剩余**（pending，顺序是随机化过的）和**本轮全量清单**
//! （members，本轮抽空后自动重置时用它）。只恢复 pending 的话，
//! 「本轮抽空 → 下一次 draw 自动开新一轮」这一步在恢复后的引擎里就没了来源。
//! 所以快照把两份都存下来（round / round_members），
//! 由 [RoundSampler::from_parts] 原样装回去。
//!
//! ## 不变量（由 [Engine::restore] 与 [EngineSnapshot::decode] 双重校验）
//!
//! * 账户公钥不重复（编号布局是快照的全部意义）；
//! * ratings / round_members / views / penalties 的编号**严格升序**
//!   （引擎内部用 BTreeMap / PositiveList，天然升序；不升序的字节不是本库产的）；
//! * ratings 的评分 ∈ (0, MAX_SCORE]，且为有限值（几何上就不该出现 NaN/∞）；
//! * round ⊆ round_members 且无重复（**不要求升序**：顺序就是随机化后的语义）；
//! * 每份 view_bytes 全是有限 f32，长度是 4 的倍数且不超过 ViewParams::MAX_CELLS 个单元；
//! * total_mass 有限且 > 0（on_view 里是 sum.max(1e-6)），噪声底三项有限且 ≥ 0；
//! * mass_covers ≤ 账户数（它记录的是 on_view 时刻的账户数上界）；
//! * rng_state ≠ 0（0 是 xorshift 的不动点，真实引擎永远走不到）。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tm_store::{
    AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler, Score, Time, MAX_SCORE,
};
use tm_view::{CompressedView, NoiseStats, ViewParams};

use crate::{Engine, EngineConfig, StoredView};

/// 快照格式的版本号（当前 = 1）。
///
/// 版本号写在编码字节的第 8..12 字节：先读版本、再解析其余字段，
/// 就是「自描述」。将来若要改布局，把版本号抬上去，旧 [EngineSnapshot::decode]
/// 会明确返回 None，而不是按旧布局错读新文件。
pub const SNAPSHOT_VERSION: u32 = 2;

// 版本历史：
//   1 —— 初版（accounts / ratings / round_members / round / views / penalties /
//        clock / stat_requests / stat_hits / rng_state / view_params_digest）
//   2 —— 尾部追加三个拒绝计数（stat_rejected_view / capacity / nonfinite）。
//        这几个量是「可观测状态」：恢复后 stats 相关的读数必须与关机前一致，
//        悄悄丢掉它们等于悄悄丢状态。旧版本字节会被 decode 明确拒绝（不做降级兼容），
//        因为跨版本的字段缺失无法安全推断。

/// 快照编码的魔数（8 字节 ASCII）：挡掉「随便一段字节」被当成快照。
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"TMSNAPSH";

/// 版本号在编码字节里的起始偏移（紧跟魔数）。
const VERSION_START: usize = SNAPSHOT_MAGIC.len();

/// 编码头部长度：魔数 8 + 版本号 4 + 视图参数指纹 8。
const HEADER_BYTES: usize = VERSION_START + 4 + PARAMS_DIGEST_BYTES;

/// 视图参数指纹的字节数（头部布局的一部分：魔数 8 + 版本 4 + 指纹 8 = 20）。
const PARAMS_DIGEST_BYTES: usize = 8;

/// 头部里版本号的结束偏移（version_of 用它切字节）。
const VERSION_END: usize = VERSION_START + 4;

/// 视图参数指纹（FNV-1a 64）。
///
/// 用途只有一个：在 [Engine::restore] 里做一个**便宜的早退检查** ——
/// 这份快照当初是不是按同一套 ViewParams 拍下来的。它**不是**配置，
/// 不参与任何计算，也不可逆；真正的校验仍是 CompressedView::decode。
///
/// 为什么需要它：seed 不同时压缩观点的**字节长度完全一样**，
/// CompressedView::decode 只校验长度与「桶值有限」，会照常解成功。
/// 于是「参数不匹配」在字节层面不可见，恢复出来的引擎会带着一份
/// 桶布局已经错位的观点继续跑 —— 这是比「拒绝恢复」严重得多的失败模式。
///
/// 参数与字节布局（usize → u64 的截断）都固定成不随平台变化的形式：
/// 快照文件要能跨平台读。
fn view_params_digest(params: ViewParams) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64 的初始值
    hash = fnv1a_update(hash, params.buckets as u64);
    hash = fnv1a_update(hash, params.hashers as u64);
    fnv1a_update(hash, params.seed)
}

/// 把一个 u64 按字节折进 FNV-1a 64 状态（小端固定，不随平台变化）。
fn fnv1a_update(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3); // FNV 质数
    }
    hash
}

/// 编码尾部长度：clock / stat_requests / stat_hits / rng_state /
/// stat_rejected_view / stat_rejected_capacity / stat_rejected_nonfinite，各 8 字节。
const TAIL_BYTES: usize = 7 * 8;

/// 一个账户公钥的字节数。
const ACCOUNT_BYTES: usize = 32;

/// 一条评分记录的字节数：编号 4 + 评分位模式 4 + last_check 8 + fail_streak 4。
const RATING_BYTES: usize = 4 + 4 + 8 + 4;

/// 一个编号字段的字节数。
const INDEX_BYTES: usize = 4;

/// 一份观点记录里除 view_bytes 以外的固定字节数：
/// 编号 4 + 字节数 4 + published_at 8 + last_check 8 + 噪声底 3×4 + total_mass 4 + mass_covers 4。
const VIEW_FIXED_BYTES: usize = 4 + 4 + 8 + 8 + 3 * 4 + 4 + 4;

/// 一条折扣记录的字节数：编号 4 + 折扣位模式 4。
const PENALTY_BYTES: usize = 4 + 4;

/// 六个数组长度的字节数总和（accounts / ratings / round_members / round / views / penalties）。
const COUNT_BYTES: usize = 6 * 4;

/// 一份可序列化的引擎快照。
///
/// 只含**运行状态**，不含 cfg（配置由调用方保管、恢复时原样传入
/// [Engine::restore]；参数不匹配会被当场拒绝）。
///
/// 字段顺序与 [EngineSnapshot::encode] 的字节顺序**一致**，便于对照格式文档。
/// 全部 Vec 都是「按编号升序或按语义顺序」排好的；浮点用位模式保存，
/// 因此 EngineSnapshot::decode(bytes) == Some(snapshot) 是**逐位相等**。
#[derive(Clone, PartialEq)]
pub struct EngineSnapshot {
    /// 格式版本号，当前恒为 [SNAPSHOT_VERSION]。
    pub version: u32,
    /// 账户公钥表：编号 0..len 的 key（**编号即下标**）。
    pub accounts: Vec<AccountKey>,
    /// 正分清单：编号、评分、last_check、连续失败次数（升序）。
    pub ratings: Vec<(AccountIdx, Score, Time, u32)>,
    /// 本轮全量清单（[RoundSampler::members]），升序去重。
    ///
    /// 任务书列出的结构里没有这一项，但少了它「本轮抽空 → 自动开新一轮」
    /// 在恢复后就没有来源，见模块文档。
    pub round_members: Vec<u32>,
    /// 本轮剩余待抽的编号列表（[RoundSampler::pending]），**顺序即语义**。
    pub round: Vec<u32>,
    /// 观点缓存，按编号升序。
    pub views: Vec<(AccountIdx, ViewSnapshot)>,
    /// 折扣表（badlist 写入的来源折扣），按编号升序。
    pub penalties: Vec<(AccountIdx, Score)>,
    /// 引擎时钟：[Engine::report_bad_account] 的时间基准。
    pub clock: Time,
    /// 累计请求数。
    pub stat_requests: u64,
    /// 累计成功请求数。
    pub stat_hits: u64,
    /// 抽样随机流的**内部状态**（不是种子）：装回去后序列从同一个位置继续。
    pub rng_state: u64,
    /// 被整条丢弃的观点累计数（容量 + 非有限读数两类之和）。
    ///
    /// 这不是「运行状态」而是「可观测状态」：运维会看这个数判断有没有人在撑爆缓存。
    /// 恢复后必须与关机前一致，否则「快照 = 完整状态」这个承诺就是假的。
    pub stat_rejected_view: u64,
    /// 其中因容量上限被丢的条数。
    pub stat_rejected_capacity: u64,
    /// 其中因读数非有限（NaN / ±∞）被丢的条数。
    pub stat_rejected_nonfinite: u64,
    /// 视图参数指纹：这份快照是按哪套 ViewParams 拍下来的。
    ///
    /// 由 [Engine::snapshot] 从 cfg.view_params 算出，[Engine::restore] 时
    /// 与传入 cfg 的指纹比对。**不是**配置、不可逆、不参与任何计算，
    /// 只在 seed 不同（字节长度却相同）这种 decode 抓不到的情形里挡住误恢复。
    /// 见模块文档的「view_params_digest」一节。
    pub view_params_digest: u64,
}

/// 一份观点的可序列化投影（[StoredView] 去掉 CompressedView 本体）。
///
/// 观点本体只存 CompressedView::encode 出来的原始字节：解码时用 cfg.view_params
/// 重新解一遍，既验证「这份快照确实属于这套参数」，也保证桶值逐位不变。
#[derive(Clone, PartialEq)]
pub struct ViewSnapshot {
    /// CompressedView::encode 的原始字节（buckets × hashers × 4）。
    pub view_bytes: Vec<u8>,
    /// 对方声明的发布时间（只用于观测，**不是**衰减基准）。
    pub published_at: Time,
    /// 上次成功取到的时间（衰减基准）。
    pub last_check: Time,
    /// 噪声底中位数。
    pub noise_median: Score,
    /// 噪声底 p95。
    pub noise_p95: Score,
    /// 噪声底最大值。
    pub noise_max: Score,
    /// 这份观点文件的总质量（接收方自己估计）。
    pub total_mass: Score,
    /// total_mass 覆盖到的账户范围（on_view 时刻的已知账户数上界）。
    pub mass_covers: u32,
}

impl ViewSnapshot {
    /// 把快照投影回引擎内部表示；参数不匹配的字节返回 None。
    ///
    /// 这一步是「参数校验」的落点：CompressedView::decode 会用
    /// params 校验长度与桶布局，并拒绝 NaN/∞ 的桶值。
    fn to_stored_view(&self, params: ViewParams) -> Option<StoredView> {
        let view = CompressedView::decode(&self.view_bytes, params)?;
        Some(StoredView {
            view,
            published_at: self.published_at,
            last_check: self.last_check,
            noise: NoiseStats {
                median: self.noise_median,
                p95: self.noise_p95,
                max: self.noise_max,
            },
            total_mass: self.total_mass,
            mass_covers: self.mass_covers,
        })
    }
}

impl fmt::Debug for ViewSnapshot {
    /// 只打印形状，不打印几千字节的桶矩阵（失败信息里也清晰可读）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewSnapshot")
            .field("view_bytes", &self.view_bytes.len())
            .field("published_at", &self.published_at)
            .field("last_check", &self.last_check)
            .field("noise_median", &self.noise_median)
            .field("noise_p95", &self.noise_p95)
            .field("noise_max", &self.noise_max)
            .field("total_mass", &self.total_mass)
            .field("mass_covers", &self.mass_covers)
            .finish()
    }
}

impl fmt::Debug for EngineSnapshot {
    /// 只打印规模与标量（字段内容可能上兆字节，逐项打印没有意义）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineSnapshot")
            .field("version", &self.version)
            .field("accounts", &self.accounts.len())
            .field("ratings", &self.ratings.len())
            .field("round_members", &self.round_members.len())
            .field("round", &self.round.len())
            .field("views", &self.views.len())
            .field("penalties", &self.penalties.len())
            .field("clock", &self.clock)
            .field("stat_requests", &self.stat_requests)
            .field("stat_hits", &self.stat_hits)
            .field("rng_state", &self.rng_state)
            .field("stat_rejected_view", &self.stat_rejected_view)
            .field("stat_rejected_capacity", &self.stat_rejected_capacity)
            .field("stat_rejected_nonfinite", &self.stat_rejected_nonfinite)
            .field("view_params_digest", &self.view_params_digest)
            .finish()
    }
}

// ===========================================================================
// 编码 / 解码
// ===========================================================================

impl EngineSnapshot {
    /// 从编码字节的头部读出格式版本号（不要求其余内容完整）。
    ///
    /// 迁移工具可以先用它判断「这份文件是不是本库认识的版本」，
    /// 再决定是解码还是走升级路径 —— 这就是「自描述版本号」的对外入口。
    #[must_use]
    pub fn version_of(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < HEADER_BYTES {
            return None;
        }
        if bytes[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
            return None;
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[VERSION_START..VERSION_END]);
        Some(u32::from_le_bytes(raw))
    }

    /// encode 的输出长度（精确值）。
    ///
    /// 调用方可以用它在写盘前预分配缓冲、或判断「这份状态有多大」，
    /// 而不必先编码一遍。
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let mut total = HEADER_BYTES
            .saturating_add(COUNT_BYTES)
            .saturating_add(TAIL_BYTES);
        total = total.saturating_add(self.accounts.len().saturating_mul(ACCOUNT_BYTES));
        total = total.saturating_add(self.ratings.len().saturating_mul(RATING_BYTES));
        total = total.saturating_add(self.round_members.len().saturating_mul(INDEX_BYTES));
        total = total.saturating_add(self.round.len().saturating_mul(INDEX_BYTES));
        total = total.saturating_add(self.views.len().saturating_mul(VIEW_FIXED_BYTES));
        for (_, view) in &self.views {
            total = total.saturating_add(view.view_bytes.len());
        }
        total = total.saturating_add(self.penalties.len().saturating_mul(PENALTY_BYTES));
        total
    }

    /// 打包成字节。格式见模块文档；输出长度恒等于 [EngineSnapshot::encoded_len]。
    ///
    /// 本方法**不做校验**（快照本来就可以是手工构造的）：非法内容写出来之后，
    /// [EngineSnapshot::decode] / [Engine::restore] 会明确拒绝。
    /// 唯一的前提是「元素个数装得下 u32」—— 由真实引擎导出的快照永远满足。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(
            self.accounts.len() <= u32::MAX as usize
                && self.ratings.len() <= u32::MAX as usize
                && self.round_members.len() <= u32::MAX as usize
                && self.round.len() <= u32::MAX as usize
                && self.views.len() <= u32::MAX as usize
                && self.penalties.len() <= u32::MAX as usize,
            "快照的元素个数超出 u32：这不是真实引擎能产出的状态"
        );
        let mut w = Writer::with_capacity(self.encoded_len());
        w.bytes(&SNAPSHOT_MAGIC);
        w.u32(self.version);
        w.u64(self.view_params_digest);

        w.count(self.accounts.len());
        for key in &self.accounts {
            w.bytes(key);
        }

        w.count(self.ratings.len());
        for &(idx, score, last_check, fail_streak) in &self.ratings {
            w.u32(idx);
            w.f32_bits(score);
            w.u64(last_check);
            w.u32(fail_streak);
        }

        w.count(self.round_members.len());
        for &idx in &self.round_members {
            w.u32(idx);
        }

        w.count(self.round.len());
        for &idx in &self.round {
            w.u32(idx);
        }

        w.count(self.views.len());
        for (idx, view) in &self.views {
            w.u32(*idx);
            w.count(view.view_bytes.len());
            w.bytes(&view.view_bytes);
            w.u64(view.published_at);
            w.u64(view.last_check);
            w.f32_bits(view.noise_median);
            w.f32_bits(view.noise_p95);
            w.f32_bits(view.noise_max);
            w.f32_bits(view.total_mass);
            w.u32(view.mass_covers);
        }

        w.count(self.penalties.len());
        for &(idx, discount) in &self.penalties {
            w.u32(idx);
            w.f32_bits(discount);
        }

        w.u64(self.clock);
        w.u64(self.stat_requests);
        w.u64(self.stat_hits);
        w.u64(self.rng_state);
        w.u64(self.stat_rejected_view);
        w.u64(self.stat_rejected_capacity);
        w.u64(self.stat_rejected_nonfinite);
        w.finish()
    }

    /// 解码：任何畸形 / 截断 / 版本不符的输入都返回 None，**绝不 panic**。
    ///
    /// 解码分两步：
    ///
    /// 1. **语法**：魔数、版本号、逐字段读取（每次读取都先检查字节够不够，
    ///    数组长度也先按「剩余字节 / 单个元素最小字节数」验算，避免伪造
    ///    长度把内存打爆），最后要求正好读完（**末尾多余字节 = 畸形**）；
    /// 2. **结构**：EngineSnapshot::validate 检查全部不变量（升序、子集、
    ///    有限浮点、编号范围等）。
    ///
    /// 与 cfg 相关的校验（观点字节能否用这套参数解出来）不在这里做，
    /// 见 [Engine::restore]。
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        if r.take(SNAPSHOT_MAGIC.len())? != SNAPSHOT_MAGIC {
            return None;
        }
        let version = r.u32()?;
        if version != SNAPSHOT_VERSION {
            return None;
        }
        let view_params_digest = r.u64()?;

        let n_accounts = r.count(ACCOUNT_BYTES)?;
        let mut accounts = Vec::with_capacity(n_accounts);
        for _ in 0..n_accounts {
            let raw = r.take(ACCOUNT_BYTES)?;
            let mut key = [0u8; ACCOUNT_BYTES];
            key.copy_from_slice(raw);
            accounts.push(key);
        }

        let n_ratings = r.count(RATING_BYTES)?;
        let mut ratings = Vec::with_capacity(n_ratings);
        for _ in 0..n_ratings {
            let idx = r.u32()?;
            let score = f32::from_bits(r.u32()?);
            let last_check = r.u64()?;
            let fail_streak = r.u32()?;
            ratings.push((idx, score, last_check, fail_streak));
        }

        let n_members = r.count(INDEX_BYTES)?;
        let mut round_members = Vec::with_capacity(n_members);
        for _ in 0..n_members {
            round_members.push(r.u32()?);
        }

        let n_round = r.count(INDEX_BYTES)?;
        let mut round = Vec::with_capacity(n_round);
        for _ in 0..n_round {
            round.push(r.u32()?);
        }

        let n_views = r.count(VIEW_FIXED_BYTES)?;
        let mut views = Vec::with_capacity(n_views);
        for _ in 0..n_views {
            let idx = r.u32()?;
            let len = r.u32()? as usize;
            let view_bytes = r.take(len)?.to_vec();
            let published_at = r.u64()?;
            let last_check = r.u64()?;
            let noise_median = f32::from_bits(r.u32()?);
            let noise_p95 = f32::from_bits(r.u32()?);
            let noise_max = f32::from_bits(r.u32()?);
            let total_mass = f32::from_bits(r.u32()?);
            let mass_covers = r.u32()?;
            views.push((
                idx,
                ViewSnapshot {
                    view_bytes,
                    published_at,
                    last_check,
                    noise_median,
                    noise_p95,
                    noise_max,
                    total_mass,
                    mass_covers,
                },
            ));
        }

        let n_penalties = r.count(PENALTY_BYTES)?;
        let mut penalties = Vec::with_capacity(n_penalties);
        for _ in 0..n_penalties {
            let idx = r.u32()?;
            let discount = f32::from_bits(r.u32()?);
            penalties.push((idx, discount));
        }

        let clock = r.u64()?;
        let stat_requests = r.u64()?;
        let stat_hits = r.u64()?;
        let rng_state = r.u64()?;
        let stat_rejected_view = r.u64()?;
        let stat_rejected_capacity = r.u64()?;
        let stat_rejected_nonfinite = r.u64()?;
        if r.remaining() != 0 {
            // 末尾多余字节：不是本库写出来的文件（也挡住了「截断后又拼回来」）。
            return None;
        }

        let snap = Self {
            version,
            accounts,
            ratings,
            round_members,
            round,
            views,
            penalties,
            clock,
            stat_requests,
            stat_hits,
            rng_state,
            stat_rejected_view,
            stat_rejected_capacity,
            stat_rejected_nonfinite,
            view_params_digest,
        };
        if !snap.validate() {
            return None;
        }
        Some(snap)
    }

    /// 结构自洽校验（不需要 cfg）：全部不变量见模块文档。
    ///
    /// [EngineSnapshot::decode] 与 [Engine::restore] 都会调用它：前者保证了
    /// 「解出来的字节是自洽的」，后者保证了「手工构造的快照也逃不掉」。
    fn validate(&self) -> bool {
        if self.version != SNAPSHOT_VERSION {
            return false;
        }
        if self.accounts.len() > AccountIdx::MAX as usize {
            return false;
        }
        let n = self.accounts.len() as AccountIdx;

        // 公钥不得重复：编号布局是快照存在的意义，重复会让 from_keys 失败。
        let mut key_set: BTreeSet<AccountKey> = BTreeSet::new();
        for key in &self.accounts {
            if !key_set.insert(*key) {
                return false;
            }
        }

        // 评分：升序、编号在范围内、评分有限且落在 (0, MAX_SCORE]。
        if !strictly_increasing(self.ratings.iter().map(|r| r.0), n) {
            return false;
        }
        for &(_, score, _, _) in &self.ratings {
            if !score.is_finite() || score <= 0.0 || score > MAX_SCORE {
                return false;
            }
        }

        // 本轮全量清单：升序、编号在范围内。
        // 注意**不**要求它是 ratings 的子集：应用层可以先 sync 再撤销评分，
        // 此时 members 里会留下已经不在正分清单里的编号（引擎下一次 tick 会 sync 掉）。
        if !strictly_increasing(self.round_members.iter().copied(), n) {
            return false;
        }

        // 待抽集合：members 的子集、无重复；顺序是随机化过的，不要求升序。
        let member_set: BTreeSet<AccountIdx> = self.round_members.iter().copied().collect();
        let mut pending_set: BTreeSet<AccountIdx> = BTreeSet::new();
        for &idx in &self.round {
            if !member_set.contains(&idx) || !pending_set.insert(idx) {
                return false;
            }
        }

        // 观点：编号升序、字节全是有限浮点、统计量有限且落在物理范围内。
        let mut prev: Option<AccountIdx> = None;
        for (idx, view) in &self.views {
            if let Some(p) = prev {
                if *idx <= p {
                    return false;
                }
            }
            prev = Some(*idx);
            if !valid_view_bytes(&view.view_bytes) {
                return false;
            }
            if !view.total_mass.is_finite() || view.total_mass <= 0.0 {
                return false;
            }
            for noise in [view.noise_median, view.noise_p95, view.noise_max] {
                if !noise.is_finite() || noise < 0.0 {
                    return false;
                }
            }
            // on_view 记的是「那一刻的账户数上界」，账户只增不减，所以不可能超过现在。
            if view.mass_covers > n {
                return false;
            }
        }

        // 折扣：升序、编号在范围内、有限且落在 [0, 1]（report_bad_account 的闭式解口径）。
        let mut prev: Option<AccountIdx> = None;
        for &(idx, discount) in &self.penalties {
            if let Some(p) = prev {
                if idx <= p {
                    return false;
                }
            }
            prev = Some(idx);
            if idx >= n {
                return false;
            }
            if !discount.is_finite() || !(0.0..=1.0).contains(&discount) {
                return false;
            }
        }

        // 随机流状态 0 是 xorshift 的不动点：真实引擎永远走不到，看见就是畸形。
        self.rng_state != 0
    }
}

/// 编号序列是否**严格升序**且每个编号都小于 n（空序列也算合法）。
fn strictly_increasing(indices: impl Iterator<Item = AccountIdx>, n: AccountIdx) -> bool {
    let mut prev: Option<AccountIdx> = None;
    for idx in indices {
        if idx >= n {
            return false;
        }
        if let Some(p) = prev {
            if idx <= p {
                return false;
            }
        }
        prev = Some(idx);
    }
    true
}

/// 一份观点负载是否**形状合法且全是有限浮点**。
///
/// 这里不校验「长度是否等于 cfg.view_params 算出来的长度」—— 那需要 cfg，
/// 由 [Engine::restore] 交给 CompressedView::decode 负责。这里只挡两件事：
///
/// * 长度不是 4 的倍数 / 为 0 / 超过 MAX_CELLS 个单元（形状明显不可能）；
/// * 任何一个桶值是非有限浮点（CompressedView::decode 也会拒绝，先挡一次
///   可以让 [EngineSnapshot::decode] 在没有 cfg 时也把它们判为畸形）。
fn valid_view_bytes(bytes: &[u8]) -> bool {
    let cell = std::mem::size_of::<Score>();
    if bytes.len() < cell || !bytes.len().is_multiple_of(cell) {
        return false;
    }
    // 单元数上限与 tm_view 的口径一致（2^26 个 f32 = 256 MiB）。
    let max_bytes = ViewParams::MAX_CELLS.saturating_mul(cell);
    if bytes.len() > max_bytes {
        return false;
    }
    for chunk in bytes.chunks_exact(cell) {
        if !f32::from_bits(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])).is_finite()
        {
            return false;
        }
    }
    true
}

// ===========================================================================
// 引擎进出口
// ===========================================================================

impl Engine {
    /// 导出一份可序列化的状态快照。
    ///
    /// now 是**导出时刻**，被写进 [EngineSnapshot::clock]（引擎内部「最近一次
    /// 带时间参数的调用」的基准）。它是 [Engine::report_bad_account] 唯一的时间
    /// 来源：关机前最后一次 tick 的时刻就是它，恢复后再调一次 tick(now)
    /// 也会把它刷成同一个值。
    ///
    /// 该方法只读：不推进抽样、不消耗随机数、不改任何状态。
    #[must_use]
    pub fn snapshot(&self, now: Time) -> EngineSnapshot {
        let views: Vec<(AccountIdx, ViewSnapshot)> = self
            .views
            .iter()
            .map(|(&idx, stored)| {
                (
                    idx,
                    ViewSnapshot {
                        view_bytes: stored.view.encode(),
                        published_at: stored.published_at,
                        last_check: stored.last_check,
                        noise_median: stored.noise.median,
                        noise_p95: stored.noise.p95,
                        noise_max: stored.noise.max,
                        total_mass: stored.total_mass,
                        mass_covers: stored.mass_covers,
                    },
                )
            })
            .collect();
        EngineSnapshot {
            version: SNAPSHOT_VERSION,
            accounts: self.accounts.keys(),
            ratings: self.ratings.items(),
            round_members: self.sampler.members().to_vec(),
            round: self.sampler.pending().to_vec(),
            views,
            penalties: self
                .penalties
                .iter()
                .map(|(&idx, &discount)| (idx, discount))
                .collect(),
            clock: now,
            stat_requests: self.stat_requests,
            stat_hits: self.stat_hits,
            rng_state: self.rng.state(),
            stat_rejected_view: self.stat_rejected_view,
            stat_rejected_capacity: self.stat_rejected_capacity,
            stat_rejected_nonfinite: self.stat_rejected_nonfinite,
            view_params_digest: view_params_digest(self.cfg.view_params),
        }
    }

    /// 由快照恢复一台引擎；cfg 必须与导出时**完全一致**。
    ///
    /// 校验分三层，任何一层不过都返回 None（绝不 panic、绝不「部分恢复」）：
    ///
    /// 1. 版本号 = [SNAPSHOT_VERSION]；
    /// 2. 结构自洽（EngineSnapshot::validate：升序、子集、有限浮点、编号范围）；
    /// 3. **观点字节能被 cfg.view_params 解出来**（长度 / 桶布局 / 无 NaN∞）。
    ///    参数不匹配（buckets / hashers / seed 任一不同）就走到这里被拒。
    ///
    /// 恢复后的可观测状态与原引擎**逐位一致**：account_count、positive_count、
    /// shares()、freshness_stats()、query()、trust_distribution()、stats()、
    /// round_remaining()，以及之后每一步 tick() 抽到的请求序列（抽样器的
    /// pending 数组与随机流状态都原样装回）。
    #[must_use]
    pub fn restore(cfg: EngineConfig, snap: EngineSnapshot) -> Option<Engine> {
        if !snap.validate() {
            return None;
        }
        // 便宜且**不可绕过**的一道闸：参数指纹不符直接拒绝。
        //
        // 只靠下面的 CompressedView::decode 是不够的 —— seed 不同时字节长度
        // 完全一样，decode 会照常成功，恢复出来的引擎会带着桶布局整体错位的
        // 观点继续跑。见模块文档的「view_params_digest」一节。
        if snap.view_params_digest != view_params_digest(cfg.view_params) {
            return None;
        }
        let accounts = AccountTable::from_keys(&snap.accounts)?;

        // 观点本体：一次性解出来，顺便验证「这套参数能不能解释这份字节」。
        let mut views: BTreeMap<AccountIdx, StoredView> = BTreeMap::new();
        for (idx, view_snap) in &snap.views {
            let stored = view_snap.to_stored_view(cfg.view_params)?;
            views.insert(*idx, stored);
        }
        if views.len() != snap.views.len() {
            // validate 已经保证编号升序，这里只是兜底：重复编号会让条数变少。
            return None;
        }

        let ratings = PositiveList::from_items(&snap.ratings);
        if ratings.len() != snap.ratings.len() {
            // 评分全部合法（validate 已查过），条数不符只可能是重复编号。
            return None;
        }

        let mut penalties: BTreeMap<AccountIdx, Score> = BTreeMap::new();
        for &(idx, discount) in &snap.penalties {
            if penalties.insert(idx, discount).is_some() {
                return None;
            }
        }

        let sampler = RoundSampler::from_parts(&snap.round_members, &snap.round);

        Some(Engine {
            accounts,
            ratings,
            sampler,
            views,
            cfg,
            penalties,
            clock: snap.clock,
            rng: Rng::from_state(snap.rng_state),
            stat_requests: snap.stat_requests,
            stat_hits: snap.stat_hits,
            stat_rejected_view: snap.stat_rejected_view,
            stat_rejected_capacity: snap.stat_rejected_capacity,
            stat_rejected_nonfinite: snap.stat_rejected_nonfinite,
            // last_k 是**观测值**（最近一次 tick 传的 k），不是运行状态：
            // 恢复后还没 tick 过，按「一次都没 tick 过」置 0。
            // 它只用于 freshness_budget 的 refresh_period 推算，不影响任何计算。
            last_k: 0,
        })
    }
}

// ===========================================================================
// 字节读写
// ===========================================================================

/// 只往前写的小缓冲（容量一次预分配）。
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// 浮点按**位模式**写出（不是十进制文本），这是逐位恢复的前提。
    fn f32_bits(&mut self, value: Score) {
        self.u32(value.to_bits());
    }

    /// 写一个数组长度（定长 u32，不用变长编码：格式要能算准长度）。
    fn count(&mut self, len: usize) {
        self.u32(len as u32);
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// 只往前读的小游标：每次读取都先验证剩余字节，读不到就返回 None。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        if end > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(self.take(4)?);
        Some(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Option<u64> {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(raw))
    }

    /// 读一个数组长度，并先验算「剩余字节装得下这么多个元素」。
    ///
    /// 没有这一步，一段伪造的 u32 = 0xFFFFFFFF 会让 Vec::with_capacity
    /// 直接申请几十 GB —— 那不是「返回 None」，是进程被 OOM 杀掉。
    /// min_item 是单个元素**至少**占用的字节数（视图这类变长元素取固定部分）。
    fn count(&mut self, min_item: usize) -> Option<usize> {
        let len = self.u32()? as usize;
        debug_assert!(min_item > 0);
        if len > self.remaining() / min_item {
            return None;
        }
        Some(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> AccountKey {
        let mut k = [0u8; 32];
        k[0..4].copy_from_slice(&n.to_le_bytes());
        k[4] = 0xA5;
        k
    }

    /// 测试用配置：视图参数故意比默认小（256 桶 × 5 哈希 = 5 KiB/份，跑得快）。
    fn cfg() -> EngineConfig {
        EngineConfig {
            view_params: ViewParams::new(256, 5, 0x1234),
            ..Default::default()
        }
    }

    fn view_of(params: ViewParams, pairs: &[(u32, Score)]) -> CompressedView {
        let p: Vec<(AccountKey, Score)> = pairs.iter().map(|&(i, s)| (key(i), s)).collect();
        CompressedView::from_pairs(params, &p)
    }

    /// 一台**内容非空**的引擎：账户、评分、观点、折扣、连败计数、部分轮次进度都有。
    fn busy_engine() -> Engine {
        let c = cfg();
        let mut e = Engine::with_seed(c.clone(), 0xC0FF_EE00_1234_5678);
        for i in 0..40u32 {
            e.intern(key(i)); // 编号 0..=39
        }
        // 评分不对称（份额 1/3、1/4、1/12、1/6、1/24、1/8），后面报告才压得动。
        for (i, s) in [(1u32, 4.0f32), (2, 3.0), (3, 1.0), (4, 2.0), (5, 0.5), (6, 1.5)] {
            e.rate(i, s, 0);
        }
        // 来源 1/2 对 A(=9) 热情、3/4 冷淡 —— 造出「均值以上」的来源。
        for (src, reading) in [(1u32, 1.0f32), (2, 1.0), (3, 0.0), (4, 0.0)] {
            let v = view_of(c.view_params, &[(9, reading), (10, 0.5), (11, 0.25)]);
            e.on_view(src, v, 10, 100);
        }
        // 连败计数：来源 3 连败 2 次、来源 5 连败 1 次（退避权重因此不再均匀）。
        e.on_view_failed(3, 120);
        e.on_view_failed(3, 130);
        e.on_view_failed(5, 140);
        // 折扣表：把 A=9 压下去，来源 1/2 被写进折扣表。
        let lowered = e.report_bad_account(9);
        assert!(lowered > 0, "构造失败：report_bad_account 没有压到任何来源");
        // 抽样推进两批：pending 里留下没抽的部分（轮次进度非空）。
        let _ = e.tick(200, 3);
        let _ = e.tick(220, 2);
        assert!(e.round_remaining() > 0, "构造失败：轮次进度被抽空了");
        e
    }

    /// 编码 → 解码 → 恢复的整条链路，返回（快照, 恢复后的引擎）。
    fn roundtrip(e: &Engine, now: Time) -> (EngineSnapshot, Engine) {
        let snap = e.snapshot(now);
        let bytes = snap.encode();
        let decoded = EngineSnapshot::decode(&bytes).expect("自家编码必须能解出来");
        assert_eq!(decoded, snap, "解码结果必须与原件逐项相同");
        let restored = Engine::restore(cfg(), decoded.clone()).expect("自家快照必须能恢复");
        (snap, restored)
    }

    fn same_bits(a: Score, b: Score) -> bool {
        a.to_bits() == b.to_bits()
    }

    // ------------------------------------------------------------------
    // 1. 空引擎 round-trip
    // ------------------------------------------------------------------
    #[test]
    fn empty_engine_roundtrip() {
        let e = Engine::new(cfg());
        let snap = e.snapshot(0);
        assert_eq!(snap.version, SNAPSHOT_VERSION);
        assert!(snap.accounts.is_empty());
        assert!(snap.ratings.is_empty());
        assert!(snap.round.is_empty() && snap.round_members.is_empty());
        assert!(snap.views.is_empty() && snap.penalties.is_empty());
        assert_eq!(snap.clock, 0);
        assert_eq!((snap.stat_requests, snap.stat_hits), (0, 0));

        let bytes = snap.encode();
        // 头部 20 + 6 个计数 24 + 尾部 56（7 个 u64）= 100 字节：空快照的固定开销。
        // 尾部从 32 涨到 56 是 v2 的事：新增三个拒绝计数（见 SNAPSHOT_VERSION 的版本历史）。
        assert_eq!(bytes.len(), 100);
        assert_eq!(bytes.len(), snap.encoded_len());
        assert_eq!(
            EngineSnapshot::version_of(&bytes),
            Some(SNAPSHOT_VERSION),
            "版本号必须自描述可读"
        );

        let decoded = EngineSnapshot::decode(&bytes).expect("空快照必须能解码");
        assert_eq!(decoded, snap);
        let restored = Engine::restore(cfg(), decoded).expect("空快照必须能恢复");
        assert_eq!(restored.account_count(), 0);
        assert_eq!(restored.positive_count(), 0);
        assert_eq!(restored.round_remaining(), 0);
        assert_eq!(restored.stats(), (0, 0, 0, 0));
        assert!(restored.shares().is_empty());
        assert!(restored.trust_distribution(0).is_empty());
        assert_eq!(
            restored.snapshot(0),
            snap,
            "恢复出的引擎再导出必须与原件完全相同"
        );
    }

    // ------------------------------------------------------------------
    // 2. 有账户/评分/观点/折扣的引擎 round-trip
    // ------------------------------------------------------------------
    #[test]
    fn populated_engine_roundtrip() {
        let e = busy_engine();
        let (snap, restored) = roundtrip(&e, 1000);

        assert_eq!(snap.accounts.len(), 40);
        assert_eq!(snap.ratings.len(), 6);
        assert_eq!(snap.views.len(), 4);
        assert_eq!(snap.penalties.len(), 2, "来源 1/2 应当被压过");
        assert_eq!(snap.round_members.len(), 6);
        assert_eq!(snap.round.len(), 1, "两批共抽走 5 个，6 个里剩 1 个");

        assert_eq!(restored.account_count(), e.account_count());
        assert_eq!(restored.positive_count(), e.positive_count());
        assert_eq!(restored.round_remaining(), e.round_remaining());
        assert_eq!(restored.stats(), e.stats());
        assert_eq!(restored.shares(), e.shares(), "份额表必须逐项相同");
        assert_eq!(
            restored.freshness_stats(1000),
            e.freshness_stats(1000),
            "观点年龄分布必须相同"
        );
        // 编号 -> key 的映射逐项相同（含已 intern 但没评分的账户）。
        for i in 0..40u32 {
            assert_eq!(restored.key_of(i), e.key_of(i));
            assert_eq!(restored.lookup(&key(i)), e.lookup(&key(i)));
            assert_eq!(restored.rating_of(i), e.rating_of(i));
            assert_eq!(restored.fail_streak(i), e.fail_streak(i));
            assert_eq!(
                restored.penalty_of(i).to_bits(),
                e.penalty_of(i).to_bits(),
                "折扣表必须逐位相同（编号 {i}）"
            );
        }
        // 再导出一遍：整份快照（含观点字节）必须完全相同。
        assert_eq!(restored.snapshot(1000), snap);
    }

    // ------------------------------------------------------------------
    // 3. round-trip 后 query() 逐位相同（20+ 个 target × 5 个时刻）
    // ------------------------------------------------------------------
    #[test]
    fn query_is_bit_identical_after_restore() {
        let e = busy_engine();
        let (_snap, restored) = roundtrip(&e, 1000);
        let times: [Time; 5] = [0, 100, 500, 5_000, 1_000_000];
        let targets: Vec<AccountIdx> = (0..30).collect(); // 30 × 5 = 150 个对比点
        for &t in &times {
            for &target in &targets {
                let before = e.query(target, t);
                let after = restored.query(target, t);
                assert!(
                    same_bits(before, after),
                    "query({target}, {t}) 位模式不同：{:08x} vs {:08x}",
                    before.to_bits(),
                    after.to_bits()
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // 4. round-trip 后 tick() 产生的请求序列相同（rng 状态恢复正确）
    // ------------------------------------------------------------------
    #[test]
    fn tick_sequence_is_identical_after_restore() {
        let mut e = busy_engine();
        let snap = e.snapshot(1000);
        let mut restored =
            Engine::restore(cfg(), EngineSnapshot::decode(&snap.encode()).unwrap())
                .expect("必须能恢复");

        // 6 批 × 每批 2 个：抽完一轮并触发自动重开，覆盖「pending 顺序 + rng 状态」两件事。
        let mut now = 1000;
        for step in 0..6 {
            now += 10;
            let a = e.tick(now, 2);
            let b = restored.tick(now, 2);
            assert_eq!(a.len(), b.len(), "第 {step} 批长度不同");
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(x.idx, y.idx, "第 {step} 批抽到不同的人");
                assert_eq!(x.key, y.key, "第 {step} 批请求的 key 不同");
            }
            assert_eq!(
                e.round_remaining(),
                restored.round_remaining(),
                "第 {step} 批之后轮次剩余不同"
            );
            // 随机流也必须停在同一位置（下一条的位模式相同）。
            let sa = e.snapshot(now).rng_state;
            let sb = restored.snapshot(now).rng_state;
            assert_eq!(sa, sb, "第 {step} 批之后 rng 状态分叉");
            assert_eq!(e.stats(), restored.stats(), "统计计数分叉");
        }
        // 兜底探测不消耗随机数，但同样必须给出同一份清单。
        let pa = e.probe(now);
        let pb = restored.probe(now);
        assert_eq!(pa.len(), pb.len());
        for (x, y) in pa.iter().zip(pb.iter()) {
            assert_eq!(x.idx, y.idx);
            assert_eq!(x.key, y.key);
        }
    }

    // ------------------------------------------------------------------
    // 5. round-trip 后 trust_distribution() 逐位相同
    // ------------------------------------------------------------------
    #[test]
    fn trust_distribution_is_bit_identical_after_restore() {
        let e = busy_engine();
        let (_snap, restored) = roundtrip(&e, 1000);
        for t in [0u64, 100, 1000, 10_000, 1_000_000] {
            let before = e.trust_distribution(t);
            let after = restored.trust_distribution(t);
            assert_eq!(before.len(), after.len(), "t={t} 时分布长度不同");
            for ((ai, av), (bi, bv)) in before.iter().zip(after.iter()) {
                assert_eq!(*ai, *bi, "t={t} 时编号顺序不同");
                assert!(
                    same_bits(*av, *bv),
                    "t={t} 时账户 {ai} 的份额位模式不同：{:08x} vs {:08x}",
                    av.to_bits(),
                    bv.to_bits()
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // 6. 畸形输入：截断、版本号错、view_bytes 损坏 → 全部 None，不 panic
    // ------------------------------------------------------------------
    #[test]
    fn malformed_inputs_are_rejected_without_panicking() {
        let e = busy_engine();
        let snap = e.snapshot(1000);
        let bytes = snap.encode();

        // 6a. 每一个严格前缀（截断）都必须被拒。
        for len in 0..bytes.len() {
            assert!(
                EngineSnapshot::decode(&bytes[..len]).is_none(),
                "截断到 {len} 字节的输入被错误接受"
            );
        }

        // 6b. 版本号错。
        let mut bad_version = bytes.clone();
        bad_version[VERSION_START..VERSION_END]
            .copy_from_slice(&(SNAPSHOT_VERSION + 1).to_le_bytes());
        assert!(EngineSnapshot::decode(&bad_version).is_none(), "版本不符必须拒绝");
        // 版本号可读，且能读出「不认识」。
        assert_eq!(
            EngineSnapshot::version_of(&bad_version),
            Some(SNAPSHOT_VERSION + 1)
        );

        // 6c. 魔数错。
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(EngineSnapshot::decode(&bad_magic).is_none(), "魔数不符必须拒绝");

        // 6d. 末尾多一个字节（长度对不上）。
        let mut too_long = bytes.clone();
        too_long.push(0xAA);
        assert!(EngineSnapshot::decode(&too_long).is_none(), "多余字节必须拒绝");

        // 6e. view_bytes 被改成 NaN / ∞：解码阶段（不需要 cfg）就必须拒绝。
        for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut poisoned = snap.clone();
            let view = &mut poisoned.views[0].1;
            view.view_bytes[0..4].copy_from_slice(&poison.to_le_bytes());
            let bad = poisoned.encode();
            assert!(
                EngineSnapshot::decode(&bad).is_none(),
                "view_bytes 含 {poison} 的快照被错误接受"
            );
        }

        // 6f. 纯垃圾字节：短缓冲与随机缓冲一律拒绝（且绝不 panic）。
        let mut rng = Rng::new(0xDEAD_BEEF);
        for _ in 0..64 {
            let len = rng.next_below(256);
            let junk: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let out = EngineSnapshot::decode(&junk);
            if len > 0 && junk[0] != SNAPSHOT_MAGIC[0] {
                assert!(out.is_none(), "垃圾字节被错误接受（长度 {len}）");
            }
        }

        // 6g. 把长度字段写成 u32::MAX 也不得按伪造长度预分配（返回 None 即证明）。
        let mut huge_len = bytes.clone();
        // accounts 计数在 HEADER_BYTES..HEADER_BYTES+4。
        huge_len[HEADER_BYTES..HEADER_BYTES + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            EngineSnapshot::decode(&huge_len).is_none(),
            "伪造的巨型长度必须被拒（否则会预分配 128 GiB）"
        );

        // 6h. 指纹字段被改坏：解码仍会成功，但 restore 必须拒绝。
        let mut bad_digest = snap.clone();
        bad_digest.view_params_digest ^= 0xFFFF_FFFF_FFFF_FFFF;
        let decoded = EngineSnapshot::decode(&bad_digest.encode()).expect("结构自洽，能解码");
        assert!(
            Engine::restore(cfg(), decoded).is_none(),
            "指纹不符必须拒绝恢复"
        );
    }

    // ------------------------------------------------------------------
    // 7. restore 时 cfg.view_params 不匹配 → None
    // ------------------------------------------------------------------
    #[test]
    fn restore_rejects_mismatched_view_params() {
        let e = busy_engine();
        let snap = e.snapshot(1000);
        let decoded = EngineSnapshot::decode(&snap.encode()).expect("能解码");

        // 参数完全一致：必须成功（对照组）。
        assert!(Engine::restore(cfg(), decoded.clone()).is_some());

        let mismatches = [
            ViewParams::new(256, 5, 0x5678), // seed 不同（字节长度一样，最隐蔽的一种）
            ViewParams::new(512, 5, 0x1234), // buckets 不同
            ViewParams::new(256, 3, 0x1234), // hashers 不同
            ViewParams::new(16, 5, 0x1234),  // buckets 更小
            ViewParams::new(1024, 7, 0),     // 三个都不同
        ];
        for params in mismatches {
            let mut bad_cfg = cfg();
            bad_cfg.view_params = params;
            assert!(
                Engine::restore(bad_cfg, decoded.clone()).is_none(),
                "view_params = {:?} 时不该恢复成功",
                params
            );
        }

        // **seed 不同但长度相同**：CompressedView::decode 会照常成功，
        // 只有参数指纹能挡住它 —— 这是本测试最核心的一条。
        let same_shape_digest_only = {
            let mut c = cfg();
            c.view_params = ViewParams::new(256, 5, 0x1234);
            view_params_digest(c.view_params)
        };
        let mut other = cfg();
        other.view_params = ViewParams::new(256, 5, 0x4321);
        assert_ne!(
            view_params_digest(other.view_params),
            same_shape_digest_only,
            "同形状不同 seed 的指纹必须不同"
        );
        assert!(
            Engine::restore(other, decoded).is_none(),
            "seed 不同时必须拒绝恢复"
        );

        // 空观点缓存的快照同样受参数约束：它记录的是「按哪套参数拍的」，
        // 而不是「有没有字节需要解释」。参数变了就意味着这份状态不属于新配置。
        let empty = Engine::new(cfg()).snapshot(0);
        let mut other_cfg = cfg();
        other_cfg.view_params = ViewParams::new(512, 5, 0x9999);
        assert!(
            Engine::restore(other_cfg, empty).is_none(),
            "空快照也必须拒绝参数不匹配的配置"
        );
    }

    // ------------------------------------------------------------------
    // 8. decode 后 encode 再 decode 仍相等（幂等）
    // ------------------------------------------------------------------
    #[test]
    fn encode_decode_is_idempotent() {
        let e = busy_engine();
        let snap = e.snapshot(1000);
        let bytes1 = snap.encode();
        let snap1 = EngineSnapshot::decode(&bytes1).expect("一次解码");
        assert_eq!(snap1, snap, "解出来的必须与原件相同");
        let bytes2 = snap1.encode();
        assert_eq!(bytes2, bytes1, "再编码必须逐字节相同（编码无隐藏状态）");
        let snap2 = EngineSnapshot::decode(&bytes2).expect("二次解码");
        assert_eq!(snap2, snap1);
        assert_eq!(snap2.encode(), bytes2, "二次往返仍然稳定");
        assert_eq!(snap2.encoded_len(), snap2.encode().len());
    }

    // ------------------------------------------------------------------
    // 9. NaN / ∞ 评分不会污染 snapshot（它们本来就不该进 PositiveList）
    // ------------------------------------------------------------------
    #[test]
    fn non_finite_ratings_never_reach_snapshot() {
        let c = cfg();
        let mut e = Engine::with_seed(c.clone(), 7);
        for i in 0..12u32 {
            e.intern(key(i));
        }
        // 各种「不该被存」的值：NaN、±∞、负数、-0.0 —— 存储层会把它们当作没表态/上界。
        for (i, s) in [
            (1u32, f32::NAN),
            (2, f32::INFINITY),
            (3, f32::NEG_INFINITY),
            (4, -5.0),
            (5, -0.0),
            (6, 2.5),       // 合法：0~n 的原始数值
            (7, MAX_SCORE), // 合法：恰好上界
            (8, 1.0e10),    // 合法但被截到 MAX_SCORE
        ] {
            e.rate(i, s, 0);
        }
        let snap = e.snapshot(0);
        for &(idx, score, _, _) in &snap.ratings {
            assert!(
                score.is_finite() && score > 0.0 && score <= MAX_SCORE,
                "编号 {idx} 的评分 {score} 不该进快照"
            );
        }
        // 只有 4 条合法评分（2 号 ∞ 被截到上界，6/7/8 号合法）。
        assert_eq!(snap.ratings.len(), 4, "非法评分没有被挡在快照之外");
        assert_eq!(snap.ratings[0], (2, MAX_SCORE, 0, 0));
        assert_eq!(snap.ratings[1], (6, 2.5, 0, 0));
        assert_eq!(snap.ratings[2], (7, MAX_SCORE, 0, 0));
        assert_eq!(snap.ratings[3], (8, MAX_SCORE, 0, 0));

        // 往返与恢复都不受影响，query 逐位相同。
        let (_s2, restored) = roundtrip(&e, 0);
        for target in 0..12u32 {
            assert!(
                same_bits(e.query(target, 0), restored.query(target, 0)),
                "编号 {target} 的 query 位模式不同"
            );
        }
        // 手工塞进快照的 NaN 评分必须被拒（decode 的结构校验就会拦）。
        let mut poisoned = snap.clone();
        poisoned.ratings[0].1 = f32::NAN;
        assert!(EngineSnapshot::decode(&poisoned.encode()).is_none());
        assert!(Engine::restore(cfg(), poisoned).is_none());
    }

    // ------------------------------------------------------------------
    // 10. 大量数据（1000 账户、500 观点）round-trip 正确
    // ------------------------------------------------------------------
    #[test]
    fn large_state_roundtrip() {
        const ACCOUNTS: u32 = 1000;
        const VIEWS: u32 = 500;
        let c = EngineConfig {
            // 桶数刻意取小（64×3 = 768 字节/份），否则 500 份 × 40 KiB 在 debug 下太慢。
            view_params: ViewParams::new(64, 3, 0x5EED),
            ..Default::default()
        };
        let mut e = Engine::with_seed(c.clone(), 0xABCD_1234);
        for i in 0..ACCOUNTS {
            e.intern(key(i));
        }
        for i in 0..300u32 {
            e.rate(i, (i % 9 + 1) as Score, 0);
        }
        for owner in 0..VIEWS {
            let pairs = [(600 + owner % 50, 1.0f32), (700, 0.5), (900, 0.25)];
            let view = view_of(c.view_params, &pairs);
            e.on_view(owner, view, 0, 0);
        }
        let snap = e.snapshot(0);
        assert_eq!(snap.accounts.len(), ACCOUNTS as usize);
        assert_eq!(snap.ratings.len(), 300);
        assert_eq!(snap.views.len(), VIEWS as usize);
        assert!(snap.round.is_empty() && snap.round_members.is_empty());
        // 每份观点 64 × 3 × 4 = 768 字节；快照体积必须对得上这份预算。
        let view_payload = VIEWS as usize * (768 + VIEW_FIXED_BYTES);
        assert!(
            snap.encoded_len() >= view_payload,
            "快照长度 {} 小于观点负载 {}",
            snap.encoded_len(),
            view_payload
        );

        let bytes = snap.encode();
        assert_eq!(bytes.len(), snap.encoded_len());
        let decoded = EngineSnapshot::decode(&bytes).expect("大快照必须能解码");
        assert_eq!(decoded, snap);
        let restored = Engine::restore(c.clone(), decoded).expect("大快照必须能恢复");
        assert_eq!(restored.account_count(), ACCOUNTS as usize);
        assert_eq!(restored.positive_count(), 300);
        assert_eq!(restored.stats(), e.stats());
        assert_eq!(restored.shares(), e.shares());
        assert_eq!(restored.freshness_stats(0), e.freshness_stats(0));
        for target in (0..ACCOUNTS).step_by(37) {
            for t in [0u64, 5_000, 500_000] {
                assert!(
                    same_bits(e.query(target, t), restored.query(target, t)),
                    "target={target} t={t} 的 query 位模式不同"
                );
            }
        }
        // 大快照再导出一遍：必须完全相同（含 500 份观点字节）。
        assert_eq!(restored.snapshot(0), snap);
    }

    // ------------------------------------------------------------------
    // 11. restore 拒绝「轮次状态自相矛盾」的快照（补测）
    // ------------------------------------------------------------------
    #[test]
    fn restore_rejects_inconsistent_round_state() {
        let e = busy_engine();
        let snap = e.snapshot(1000);

        // 11a. pending 里的编号不在 members 里。
        let mut bad = snap.clone();
        bad.round.push(20); // 20 号账户已 intern，但没评分，不在 members
        assert!(Engine::restore(cfg(), bad).is_none());

        // 11b. pending 有重复编号。
        let mut bad = snap.clone();
        let dup = bad.round[0];
        bad.round.push(dup);
        assert!(Engine::restore(cfg(), bad).is_none());

        // 11c. members 没排序。
        let mut bad = snap.clone();
        bad.round_members.reverse();
        assert!(Engine::restore(cfg(), bad).is_none());

        // 11d. members 里有越界编号。
        let mut bad = snap.clone();
        bad.round_members = vec![0, 1, 40];
        bad.round = vec![0];
        assert!(Engine::restore(cfg(), bad).is_none());

        // 11e. 评分编号越界（账户只有 0..=39）。
        let mut bad = snap.clone();
        bad.ratings[0].0 = 40;
        assert!(Engine::restore(cfg(), bad).is_none());

        // 11f. 评分编号重复（validate 的升序检查会拦）。
        let mut bad = snap.clone();
        bad.ratings[1].0 = bad.ratings[0].0;
        assert!(Engine::restore(cfg(), bad).is_none());
    }

    // ------------------------------------------------------------------
    // 12. restore 拒绝账户 / 评分 / 折扣 / 随机流层面的畸形（补测）
    // ------------------------------------------------------------------
    #[test]
    fn restore_rejects_malformed_accounts_and_scores() {
        let e = busy_engine();
        let base = e.snapshot(1000);

        // 12a. 重复公钥。
        let mut bad = base.clone();
        bad.accounts[1] = bad.accounts[0];
        assert!(Engine::restore(cfg(), bad).is_none(), "重复公钥必须拒绝");

        // 12b. 评分非正 / NaN / 超过上界。
        for poison in [0.0f32, -0.0, -1.0, f32::NAN, f32::INFINITY, MAX_SCORE * 2.0] {
            let mut bad = base.clone();
            bad.ratings[0].1 = poison;
            assert!(
                Engine::restore(cfg(), bad).is_none(),
                "评分 {poison} 必须被拒绝"
            );
        }

        // 12c. 折扣非有限 / 越界。
        for poison in [f32::NAN, f32::INFINITY, -1.0, 1.5] {
            let mut bad = base.clone();
            bad.penalties[0].1 = poison;
            assert!(
                Engine::restore(cfg(), bad).is_none(),
                "折扣 {poison} 必须被拒绝"
            );
        }

        // 12d. 随机流状态 0（xorshift 不动点，真实引擎走不到）。
        let mut bad = base.clone();
        bad.rng_state = 0;
        assert!(Engine::restore(cfg(), bad.clone()).is_none(), "rng 状态 0 必须拒绝");
        assert!(
            EngineSnapshot::decode(&bad.encode()).is_none(),
            "rng 状态 0 的快照在 decode 阶段就该被拒"
        );

        // 12e. 观点 / 折扣编号不升序。
        let mut bad = base.clone();
        bad.views.reverse();
        assert!(Engine::restore(cfg(), bad).is_none(), "观点未升序必须拒绝");
        let mut bad = base.clone();
        bad.penalties.reverse();
        assert!(Engine::restore(cfg(), bad).is_none(), "折扣未升序必须拒绝");

        // 12f. total_mass / 噪声底非法。
        let mut bad = base.clone();
        bad.views[0].1.total_mass = f32::NAN;
        assert!(Engine::restore(cfg(), bad).is_none());
        let mut bad = base.clone();
        bad.views[0].1.noise_median = -1.0;
        assert!(Engine::restore(cfg(), bad).is_none());
        let mut bad = base.clone();
        bad.views[0].1.total_mass = 0.0;
        assert!(Engine::restore(cfg(), bad).is_none());

        // 12g. mass_covers 超过账户数。
        let mut bad = base.clone();
        bad.views[0].1.mass_covers = 41;
        assert!(Engine::restore(cfg(), bad).is_none());

        // 12h. 版本号不是 1。
        let mut bad = base.clone();
        bad.version = SNAPSHOT_VERSION + 7;
        assert!(Engine::restore(cfg(), bad).is_none());

        // 12i. view_bytes 长度与参数算出来的不符（结构与参数都对不上）。
        let mut bad = base.clone();
        bad.views[0].1.view_bytes.truncate(100);
        assert!(Engine::restore(cfg(), bad).is_none());
    }

    // ------------------------------------------------------------------
    // 13. 恢复后的引擎「继续跑」与原引擎逐步同轨迹（补测）
    // ------------------------------------------------------------------
    #[test]
    fn restored_engine_continues_identically() {
        let mut orig = busy_engine();
        let snap = orig.snapshot(1000);
        let mut restored =
            Engine::restore(cfg(), EngineSnapshot::decode(&snap.encode()).unwrap()).unwrap();

        let mut now = 1000;
        for step in 0..8u32 {
            now += 10;
            let a = orig.tick(now, 3);
            let b = restored.tick(now, 3);
            assert_eq!(a.len(), b.len(), "第 {step} 步抽到的数量不同");
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(x.idx, y.idx, "第 {step} 步抽到不同的人");
                assert_eq!(x.key, y.key, "第 {step} 步请求不同");
            }
            // 同一份观点字节喂给两边：一半当成功、一半当失败，两条路径都覆盖。
            let view = view_of(cfg().view_params, &[(9, 0.6), (20, 0.3), (7, 0.1)]);
            for (i, req) in a.iter().enumerate() {
                if i % 2 == 0 {
                    orig.on_view(req.idx, view.clone(), now, now);
                    restored.on_view(req.idx, view.clone(), now, now);
                } else {
                    orig.on_view_failed(req.idx, now);
                    restored.on_view_failed(req.idx, now);
                }
            }
            // 周期性做一次报告与一次裁剪：这两条路径都改内部状态。
            if step == 2 {
                let la = orig.report_bad_account(9);
                let lb = restored.report_bad_account(9);
                assert_eq!(la, lb, "report_bad_account 的返回值不同");
            }
            if step == 5 {
                let pa = orig.prune_stale(now, 5_000);
                let pb = restored.prune_stale(now, 5_000);
                assert_eq!(pa, pb, "prune_stale 丢掉的条数不同");
            }
            // 每一步都校验全量可观测状态（含观点字节与折扣表）。
            assert_eq!(
                orig.snapshot(now),
                restored.snapshot(now),
                "第 {step} 步状态分叉"
            );
            assert_eq!(orig.stats(), restored.stats());
            assert_eq!(orig.round_remaining(), restored.round_remaining());
            for target in 0..40u32 {
                assert!(
                    same_bits(orig.query(target, now), restored.query(target, now)),
                    "第 {step} 步 target={target} 的 query 位模式不同"
                );
            }
        }
        assert_eq!(
            orig.trust_distribution(now),
            restored.trust_distribution(now),
            "最终信任分布必须逐位相同"
        );
    }

    // ------------------------------------------------------------------
    // 14. 快照字段与引擎可观测量一一对应（补测）
    // ------------------------------------------------------------------
    #[test]
    fn snapshot_fields_match_engine_observables() {
        let mut e = busy_engine();
        let snap = e.snapshot(5000);
        assert_eq!(snap.clock, 5000, "clock 必须写成调用方给的导出时刻");
        assert_eq!(snap.stat_requests, e.stats().0);
        assert_eq!(snap.stat_hits, e.stats().1);
        assert_eq!(snap.round.len(), e.round_remaining(), "轮次剩余条数");
        assert_eq!(snap.views.len(), e.stats().2);
        assert_eq!(snap.ratings.len(), e.stats().3);
        assert_eq!(
            snap.view_params_digest,
            view_params_digest(cfg().view_params),
            "指纹必须来自导出时的 cfg"
        );

        // 随机流状态：恢复后第一批抽签必须与原件相同（这是「位置」而非「种子」）。
        let mut restored = Engine::restore(cfg(), snap.clone()).unwrap();
        let a = e.tick(5000, 1);
        let b = restored.tick(5000, 1);
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].idx, b[0].idx, "第一批抽签分叉");
        // 抽完这一步之后，两边的随机流必须仍停在同一个位置（下一步才不会分叉）。
        assert_eq!(
            e.snapshot(5000).rng_state,
            restored.snapshot(5000).rng_state,
            "tick 之后随机流位置分叉"
        );
        // 从快照状态装回的随机流必须「原地不动」：装回去之后状态就是它自己。
        // （0 会被规范化成内置常量，这里断言的是「真实引擎的状态非 0」这条前提。）
        let from_snap = Rng::from_state(snap.rng_state);
        assert_eq!(from_snap.state(), snap.rng_state, "装回的状态必须原样");
        assert_ne!(snap.rng_state, 0, "真实引擎的随机流状态不可能为 0");
    }

    // ------------------------------------------------------------------
    // 15. 折扣表语义往返：恢复后再报告一次，结果与原件一致（补测）
    // ------------------------------------------------------------------
    #[test]
    fn penalty_table_roundtrips_and_keeps_min_semantics() {
        let mut e = busy_engine();
        let snap = e.snapshot(1000);
        let mut restored = Engine::restore(cfg(), snap.clone()).unwrap();

        // 折扣表逐位相同（busy_engine 里来源 1/2 被压过）。
        for i in 0..40u32 {
            assert_eq!(
                e.penalty_of(i).to_bits(),
                restored.penalty_of(i).to_bits(),
                "编号 {i} 的折扣不同"
            );
        }
        let discounted: Vec<u32> = (0..40u32).filter(|&i| e.penalty_of(i) < 1.0).collect();
        assert_eq!(discounted, vec![1, 2], "构造应当只压了来源 1/2");

        // 再报告一次同一个账户：min 语义下不应更狠（两边返回值相同）。
        let la = e.report_bad_account(9);
        let lb = restored.report_bad_account(9);
        assert_eq!(la, lb);
        assert_eq!(e.snapshot(1000), restored.snapshot(1000));
        // 手动清空折扣后两边仍一致。
        e.clear_penalties();
        restored.clear_penalties();
        assert_eq!(e.snapshot(1000), restored.snapshot(1000));
    }

    // ------------------------------------------------------------------
    // 16. 编码布局与版本号自描述（补测）
    // ------------------------------------------------------------------
    #[test]
    fn encoding_header_is_self_describing() {
        let e = busy_engine();
        let snap = e.snapshot(1000);
        let bytes = snap.encode();

        // 魔数 + 版本号在头部，且能从字节里独立读出来。
        assert_eq!(&bytes[..8], &SNAPSHOT_MAGIC);
        assert_eq!(EngineSnapshot::version_of(&bytes), Some(SNAPSHOT_VERSION));
        assert_eq!(
            &bytes[VERSION_START..VERSION_END],
            &SNAPSHOT_VERSION.to_le_bytes(),
            "版本号必须写在固定偏移上"
        );
        let mut digest_raw = [0u8; 8];
        digest_raw.copy_from_slice(&bytes[VERSION_END..HEADER_BYTES]);
        assert_eq!(
            u64::from_le_bytes(digest_raw),
            snap.view_params_digest,
            "指纹必须写在版本号之后、账户表之前"
        );
        assert_eq!(bytes.len(), snap.encoded_len());

        // 头部不足 12 字节时 version_of 返回 None（不越界、不 panic）。
        for len in 0..HEADER_BYTES {
            assert!(EngineSnapshot::version_of(&bytes[..len]).is_none());
        }
        // 魔数不对时 version_of 也返回 None。
        let mut bad = bytes.clone();
        bad[0] = b'?';
        assert!(EngineSnapshot::version_of(&bad).is_none());
    }

    // ------------------------------------------------------------------
    // 17. 时间衰减模式（decay_enabled / decay_floor）不影响快照往返（补测）
    // ------------------------------------------------------------------
    #[test]
    fn restore_respects_decay_configuration() {
        let e = busy_engine();
        let snap = e.snapshot(1000);
        let decoded = EngineSnapshot::decode(&snap.encode()).unwrap();

        // cfg 里除 view_params 之外的字段不参与校验：同一份状态在不同衰减配置下
        // 可以恢复，但**查询结果**会跟着配置走 —— 这正是「cfg 由调用方保管」的语义。
        let mut no_decay = cfg();
        no_decay.decay_enabled = false;
        let restored = Engine::restore(no_decay, decoded.clone()).expect("衰减开关不影响恢复");
        // 不衰减时 query 与原引擎（衰减开）本就不同 —— 这里只断言「能恢复且可查询」。
        for target in 0..40u32 {
            let q = restored.query(target, 1_000_000);
            assert!(q.is_finite() && (0.0..=1.0).contains(&q), "越界：{q}");
        }
        // beta 也要能跟着 cfg 变（快照只装运行状态）。
        let mut other_beta = cfg();
        other_beta.beta = 1.0;
        let restored = Engine::restore(other_beta, decoded).expect("beta 不影响恢复");
        for target in 0..40u32 {
            let q = restored.query(target, 1000);
            assert!(q.is_finite() && (0.0..=1.0).contains(&q), "越界：{q}");
        }
    }
}
