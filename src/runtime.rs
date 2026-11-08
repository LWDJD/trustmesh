//! 极轻量运行时。
//!
//! # 设计约束
//!
//! - 不绑定 async runtime（tokio、async-std 都不需要）
//! - 单线程，无锁
//! - 核心不做 IO，出站消息通过 `outbox` 交给外部
//! - 传输层是 trait，开发者自己实现
//!
//! # 与外部的关系
//!
//! ```text
//! 外部循环（tokio / 线程 / 嵌入式）
//!   ├─ 调 runtime.tick(now)          定时任务：抽样、衰减
//!   ├─ 调 runtime.on_message(from, m) 处理入站消息
//!   └─ 取 runtime.take_outbox()       发送出站消息
//! ```
//!
//! 运行时本身不碰网络，只生产 `Message`。

use std::collections::HashMap;

use crate::presets::Preset;
use crate::rating_list::RatingList;
use crate::rng::Rng;
use crate::sketch::Sketch;

/// 对端标识。实际项目中可以是地址、公钥哈希、libp2p PeerId。
pub type PeerId = u64;

/// 账户标识。
pub type AccountId = u64;

/// 单向映射：账户 → 应该去问哪个节点。
///
/// 只需要这一个方向，因为整个流程的输出只是「给某个账户算分」。
/// 生产环境用 SHA-256(address)[0..8]，这里用等价的确定性哈希避免引入依赖。
pub fn peer_of(account: AccountId) -> PeerId {
    let mut x = account.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// 运行时之间传递的消息。
#[derive(Clone, Debug)]
pub enum Message {
    /// 发布自己的信任摘要。
    /// 携带「我对各账户的 trust」，key 是发布者的账户 ID。
    Publish {
        owner: AccountId,
        /// 稀疏的 (账户, trust) 列表
        trust: Vec<(AccountId, f32)>,
        at: u64,
    },
    /// 请求某个账户的摘要。
    RequestSummary { target: AccountId, at: u64 },
    /// 回应摘要请求。
    SummaryResponse {
        target: AccountId,
        value: f32,
        at: u64,
    },
    /// 心跳，用于探测对端在线。
    Ping { at: u64 },
}

/// 运行时的出站动作。
#[derive(Clone, Debug)]
pub enum Action {
    Send { to: PeerId, msg: Message },
}

/// 传输层接口。开发者实现这个 trait 接入任意通信方式。
pub trait Transport {
    /// 发送一条消息。实现可以是异步的，但接口保持同步。
    fn send(&mut self, to: PeerId, msg: Message);
    /// 取出下一条入站消息，没有则返回 None。
    fn poll(&mut self) -> Option<(PeerId, Message)>;
}

/// 驱动运行时的主循环。
///
/// 外部只需提供一个 Transport 和推进次数，循环内部负责：
/// 1. tick（抽样、衰减、发布）
/// 2. 发送出站消息
/// 3. 处理入站消息
pub fn run_loop<T: Transport>(rt: &mut Runtime, transport: &mut T, ticks: u64) {
    for t in 0..ticks {
        rt.tick(t);
        for action in rt.take_outbox() {
            match action {
                Action::Send { to, msg } => transport.send(to, msg),
            }
        }
        while let Some((from, msg)) = transport.poll() {
            rt.on_message(from, msg);
        }
    }
}

/// 内存传输，用于测试。
#[derive(Default)]
pub struct MemoryTransport {
    inbox: Vec<(PeerId, Message)>,
    pub sent: Vec<(PeerId, Message)>,
}

impl MemoryTransport {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, from: PeerId, msg: Message) {
        self.inbox.push((from, msg));
    }
}

impl Transport for MemoryTransport {
    fn send(&mut self, to: PeerId, msg: Message) {
        self.sent.push((to, msg));
    }
    fn poll(&mut self) -> Option<(PeerId, Message)> {
        if self.inbox.is_empty() {
            None
        } else {
            Some(self.inbox.remove(0))
        }
    }
}

/// 运行时配置。
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub preset: Preset,
    /// 每次 tick 的抽样次数
    pub samples_per_tick: usize,
    /// 多久发布一次摘要（tick 数）
    pub publish_every: u64,
    /// 对端列表
    pub peers: Vec<PeerId>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            preset: Preset::recommended(),
            samples_per_tick: 20,
            publish_every: 60,
            peers: Vec::new(),
        }
    }
}

/// 单节点运行时。
pub struct Runtime {
    pub id: PeerId,
    pub config: RuntimeConfig,
    list: RatingList,
    rng: Rng,
    now: u64,
    /// 我对各账户的当前信任分
    trust: HashMap<AccountId, f64>,
    /// 对端发布的信任摘要：发布者账户 → (目标账户 → trust)
    /// key 用账户 ID，不用 PeerId，因为抽样得到的是账户 ID。
    peer_trust: HashMap<AccountId, HashMap<AccountId, f32>>,
    outbox: Vec<Action>,
    last_publish: u64,
    /// 本轮抽样结果，用于调试
    pub last_samples: Vec<(AccountId, f64)>,
}

impl Runtime {
    pub fn new(id: PeerId, config: RuntimeConfig, seed: u64) -> Self {
        let list = config.preset.rating_list();
        Self {
            id,
            list,
            rng: Rng::new(seed),
            now: 0,
            trust: HashMap::new(),
            peer_trust: HashMap::new(),
            outbox: Vec::new(),
            last_publish: 0,
            config,
            last_samples: Vec::new(),
        }
    }

    /// 记录一次本地评分，并立即更新 trust。
    pub fn rate(&mut self, target: AccountId, score: f64) {
        self.list.record(target as usize, score, self.now);
        self.recompute_trust();
    }

    /// 推进时间。执行抽样、重算 trust、定期发布。
    pub fn tick(&mut self, now: u64) {
        self.now = now;

        // 抽样一次，复用于调试和计算
        let samples = self
            .list
            .sample(self.config.samples_per_tick, now, &mut self.rng);
        self.last_samples = samples
            .iter()
            .map(|(a, w)| (*a as AccountId, *w))
            .collect();

        self.recompute_trust_with(&samples);

        if now.saturating_sub(self.last_publish) >= self.config.publish_every {
            self.publish();
            self.last_publish = now;
        }
    }

    /// 重算 trust（自己抽一次样本，供 rate/on_message 调用）。
    fn recompute_trust(&mut self) {
        let samples = self
            .list
            .sample(self.config.samples_per_tick, self.now, &mut self.rng);
        self.recompute_trust_with(&samples);
    }

    /// 用给定样本重算 trust。
    ///
    /// 公式：
    /// trust[s][u] = β · 本地评分 + (1-β) · Σ w·trust[v][u]
    /// 其中 v 是「从正分账户中按评分加权抽到的样本」，w 是命中比例。
    fn recompute_trust_with(&mut self, samples: &[(usize, f64)]) {
        let beta = self.config.preset.beta;
        let now = self.now;

        // 收集账户：本地评过的 + 对端发布过的
        let local = self.list.decayed_weights(now);
        let mut accounts: std::collections::HashSet<AccountId> = std::collections::HashSet::new();
        for (a, _) in &local {
            accounts.insert(*a as AccountId);
        }
        for m in self.peer_trust.values() {
            for &a in m.keys() {
                accounts.insert(a);
            }
        }

        // 3. 逐账户计算
        let mut next = HashMap::with_capacity(accounts.len());
        for &u in &accounts {
            let own = local
                .iter()
                .find(|(a, _)| *a as AccountId == u)
                .map(|(_, w)| *w)
                .unwrap_or(0.5);

            // 群体：只来自抽到的样本，按命中比例加权
            let mut sum = 0.0;
            let mut wsum = 0.0;
            for &(v, w) in samples {
                // 单向映射只用于发送；这里直接用账户 ID 查已收到的发布
                if let Some(m) = self.peer_trust.get(&(v as AccountId)) {
                    if let Some(val) = m.get(&u) {
                        sum += w * (*val as f64);
                        wsum += w;
                    }
                }
            }
            let group = if wsum > 0.0 { sum / wsum } else { 0.5 };

            next.insert(u, beta * own + (1.0 - beta) * group);
        }

        self.trust = next;
    }

    /// 构造发布消息，放进 outbox。
    fn publish(&mut self) {
        let trust: Vec<(AccountId, f32)> = self
            .trust
            .iter()
            .map(|(&a, &v)| (a, v as f32))
            .collect();
        let msg = Message::Publish {
            owner: self.id as AccountId,
            trust,
            at: self.now,
        };
        for &peer in &self.config.peers {
            if peer != self.id {
                self.outbox.push(Action::Send { to: peer, msg: msg.clone() });
            }
        }
    }

    /// 处理入站消息。
    pub fn on_message(&mut self, from: PeerId, msg: Message) {
        match msg {
            Message::Publish { owner, trust, at } => {
                let entry = self.peer_trust.entry(owner).or_default();
                for (a, v) in trust {
                    entry.insert(a, v);
                }
                let _ = at;
            }
            Message::RequestSummary { target, at } => {
                let value = self.query(target) as f32;
                self.outbox.push(Action::Send {
                    to: from,
                    msg: Message::SummaryResponse {
                        target,
                        value,
                        at,
                    },
                });
            }
            Message::SummaryResponse {
                target,
                value,
                at: _,
            } => {
                // 无法从 PeerId 反查账户，所以用发送方 id 当账户占位
                self.peer_trust
                    .entry(from as AccountId)
                    .or_default()
                    .insert(target, value);
            }
            Message::Ping { at } => {
                let _ = at;
            }
        }
        // 状态变了，立即重算
        self.recompute_trust();
    }

    /// 取走本轮产生的出站动作。
    pub fn take_outbox(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.outbox)
    }

    /// 查询某个账户的当前信任分。
    pub fn query(&self, target: AccountId) -> f64 {
        self.trust.get(&target).copied().unwrap_or(0.5)
    }

    /// 抽样结果（调试用）。
    pub fn samples(&self) -> &[(AccountId, f64)] {
        &self.last_samples
    }

    /// 清单长度。
    pub fn rating_count(&self) -> usize {
        self.list.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(id: PeerId, peers: Vec<PeerId>) -> Runtime {
        let mut cfg = RuntimeConfig::default();
        cfg.peers = peers;
        cfg.publish_every = 10;
        Runtime::new(id, cfg, 42)
    }

    #[test]
    fn records_and_queries() {
        let mut r = rt(1, vec![]);
        r.rate(100, 0.9);
        assert_eq!(r.rating_count(), 1);
        let q = r.query(100);
        assert!(q > 0.0 && q <= 1.0);
    }

    #[test]
    fn tick_samples() {
        let mut r = rt(1, vec![]);
        for i in 0..10 {
            r.rate(100 + i, 0.5 + i as f64 * 0.05);
        }
        r.tick(1);
        assert!(!r.samples().is_empty(), "tick 后应有抽样结果");
    }

    #[test]
    fn publishes_periodically() {
        let mut r = rt(1, vec![2, 3]);
        r.rate(100, 0.8);
        r.tick(1);
        assert!(r.take_outbox().is_empty(), "未到发布周期");
        r.tick(10);
        let out = r.take_outbox();
        assert_eq!(out.len(), 2, "应发给两个对端");
    }

    #[test]
    fn handles_summary_request() {
        let mut r = rt(1, vec![]);
        r.rate(100, 0.9);
        r.on_message(2, Message::RequestSummary { target: 100, at: 0 });
        let out = r.take_outbox();
        assert_eq!(out.len(), 1);
        match &out[0] {
            Action::Send { to, msg } => {
                assert_eq!(*to, 2);
                assert!(matches!(msg, Message::SummaryResponse { .. }));
            }
        }
    }

    #[test]
    fn peer_mapping_deterministic() {
        assert_eq!(peer_of(100), peer_of(100));
        assert_ne!(peer_of(100), peer_of(101));
    }

    #[test]
    fn run_loop_drives() {
        let mut r = rt(1, vec![2]);
        r.rate(100, 0.9);
        let mut tp = MemoryTransport::new();
        run_loop(&mut r, &mut tp, 20);
        assert!(!tp.sent.is_empty(), "run_loop 应产生出站消息");
    }

    #[test]
    fn two_nodes_converge() {
        let mut a = rt(1, vec![2]);
        let mut b = rt(2, vec![1]);

        a.rate(100, 1.0);
        b.rate(100, 0.0);

        // 互发请求
        a.on_message(2, Message::RequestSummary { target: 100, at: 0 });
        b.on_message(1, Message::RequestSummary { target: 100, at: 0 });

        // 交换出站
        let a_out = a.take_outbox();
        let b_out = b.take_outbox();
        for act in a_out {
            if let Action::Send { to, msg } = act {
                if to == 2 {
                    b.on_message(1, msg);
                }
            }
        }
        for act in b_out {
            if let Action::Send { to, msg } = act {
                if to == 1 {
                    a.on_message(2, msg);
                }
            }
        }

        // 双方对 100 的查询应该介于 0 和 1 之间
        let qa = a.query(100);
        let qb = b.query(100);
        assert!(qa > 0.0 && qa < 1.0, "a 的查询应被对端影响: {}", qa);
        assert!(qb > 0.0 && qb < 1.0, "b 的查询应被对端影响: {}", qb);
    }
}
