//! 全真模拟：动态网络、异构账户、攻击场景。
//!
//! 与 `sim` 模块的区别：`sim` 只生成静态图，这里跑时间循环。
//!
//! # 模拟维度
//!
//! - 时间：按小时推进，支持昼夜节律
//! - 账户：加入、离开、在线时段、活跃度、兴趣偏好
//! - 关系：关注、取关、屏蔽
//! - 攻击：女巫集群、垃圾投放、潜伏突变
//! - 信任：异步迭代，无极传播
//!
//! # 指标
//!
//! - 收敛度、信任分布、攻击提升、垃圾曝光、新用户可见性、圈子隔离度

use crate::rng::Rng;

// ============================================================
// 配置
// ============================================================

#[derive(Clone, Debug)]
pub struct SimConfig {
    /// 初始账户数
    pub n_initial: usize,
    /// 模拟天数
    pub days: usize,
    /// 每天推进的步数（每小时 1 步）
    pub steps_per_day: usize,
    /// 每小时新加入的概率
    pub join_rate: f64,
    /// 每小时离开的概率（基础值）
    pub leave_rate: f64,
    /// 兴趣维度
    pub interests: usize,
    /// 同质性强度：0 随机关注，1 只关注同好
    pub homophily: f64,
    /// 攻击者比例
    pub attack_ratio: f64,
    /// 垃圾投放者比例
    pub spam_ratio: f64,
    /// 个人权重
    pub beta: f64,
    /// 信任重算间隔（小时）
    pub recompute_every: usize,
    /// 迭代最大轮数
    pub max_iters: usize,
    /// 收敛阈值
    pub eps: f64,
    /// 昼夜节律开关
    pub enable_rhythm: bool,
    /// 随机种子
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            n_initial: 200,
            days: 30,
            steps_per_day: 24,
            join_rate: 1.0,
            leave_rate: 0.0008,
            interests: 8,
            homophily: 0.6,
            attack_ratio: 0.0,
            spam_ratio: 0.0,
            beta: 0.7,
            recompute_every: 24,
            max_iters: 30,
            eps: 1e-4,
            enable_rhythm: true,
            seed: 42,
        }
    }
}

// ============================================================
// 账户
// ============================================================

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Honest,
    Sybil { cluster: usize },
    Spammer,
    /// 潜伏者：前 `normal_hours` 小时正常，之后突变
    Sleeper { normal_hours: u64 },
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: usize,
    pub joined: u64,
    pub left: Option<u64>,
    /// 兴趣向量，归一化
    pub interest: Vec<f64>,
    /// 活跃时段起点（小时 0-23）
    pub wake: u64,
    /// 活跃时段长度（小时）
    pub awake: u64,
    /// 每小时互动次数
    pub activity: f64,
    pub kind: Kind,
    /// 本地评分：对目标的评分 [-1,1]
    pub ratings: Vec<f64>,
    /// 最近活跃时间
    pub last_active: u64,
}

impl Account {
    fn new(id: usize, now: u64, cfg: &SimConfig, rng: &mut Rng, kind: Kind) -> Self {
        let mut interest: Vec<f64> = (0..cfg.interests).map(|_| rng.next_f64()).collect();
        let sum: f64 = interest.iter().sum();
        if sum > 0.0 {
            for x in interest.iter_mut() {
                *x /= sum;
            }
        }

        let wake = rng.next_usize(24) as u64;
        let awake = 8 + rng.next_usize(10) as u64; // 8 到 17 小时

        Self {
            id,
            joined: now,
            left: None,
            interest,
            wake,
            awake,
            activity: 0.5 + rng.next_f64() * 2.0,
            kind,
            ratings: vec![0.0; cfg.n_initial + 10_000],
            last_active: now,
        }
    }

    fn is_online(&self, hour: u64, cfg: &SimConfig) -> bool {
        if self.left.is_some() {
            return false;
        }
        if !cfg.enable_rhythm {
            return true;
        }
        let h = hour % 24;
        let start = self.wake;
        let end = (self.wake + self.awake) % 24;
        if start <= end {
            h >= start && h < end
        } else {
            h >= start || h < end
        }
    }
}

// ============================================================
// 模拟器
// ============================================================

pub struct Sim {
    pub cfg: SimConfig,
    pub accounts: Vec<Account>,
    /// 邻接表：edges[s] = [(v, weight)]
    pub edges: Vec<Vec<(usize, f64)>>,
    /// trust[s][u] = s 视角下 u 的信任分
    pub trust: Vec<Vec<f64>>,
    pub rng: Rng,
    pub time: u64,
    pub stats: Vec<Snapshot>,
    next_id: usize,
    sybil_target: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub hour: u64,
    pub n_alive: usize,
    pub n_online: usize,
    pub n_edges: usize,
    pub max_change: f64,
    pub trust_mean: f64,
    pub trust_std: f64,
    pub target_trust: Option<f64>,
    pub spam_trust: f64,
    pub new_user_trust: f64,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Self {
        let mut rng = Rng::new(cfg.seed);
        let mut accounts = Vec::with_capacity(cfg.n_initial + 2000);
        let mut edges = Vec::new();
        let mut trust = Vec::new();

        let n_honest = cfg.n_initial;
        let n_sybil = ((cfg.n_initial as f64) * cfg.attack_ratio) as usize;
        let n_spam = ((cfg.n_initial as f64) * cfg.spam_ratio) as usize;

        for i in 0..n_honest {
            let kind = if cfg.attack_ratio > 0.0 && i < n_sybil.min(n_honest / 4) {
                Kind::Sleeper { normal_hours: 240 }
            } else {
                Kind::Honest
            };
            accounts.push(Account::new(i, 0, &cfg, &mut rng, kind));
            edges.push(Vec::new());
            trust.push(vec![0.0; cfg.n_initial + 10_000]);
        }
        for i in 0..n_sybil {
            let id = accounts.len();
            accounts.push(Account::new(
                id,
                0,
                &cfg,
                &mut rng,
                Kind::Sybil { cluster: i / 20 },
            ));
            edges.push(Vec::new());
            trust.push(vec![0.0; cfg.n_initial + 10_000]);
        }
        for _ in 0..n_spam {
            let id = accounts.len();
            accounts.push(Account::new(id, 0, &cfg, &mut rng, Kind::Spammer));
            edges.push(Vec::new());
            trust.push(vec![0.0; cfg.n_initial + 10_000]);
        }

        let mut sim = Self {
            cfg,
            accounts,
            edges,
            trust,
            rng,
            time: 0,
            stats: Vec::new(),
            next_id: 0,
            sybil_target: None,
        };
        sim.next_id = sim.accounts.len();

        // 初始关注：按同质性建立
        sim.build_initial_edges();
        // 初始评分
        sim.init_ratings();
        // 女巫内部互关
        sim.wire_sybils();

        sim
    }

    /// 按兴趣相似度建立初始关注。
    fn build_initial_edges(&mut self) {
        let n = self.accounts.len();
        let per = 8;
        for s in 0..n {
            let mut candidates: Vec<(usize, f64)> = (0..n)
                .filter(|&u| u != s)
                .map(|u| {
                    let sim = cosine(&self.accounts[s].interest, &self.accounts[u].interest);
                    // 垃圾账户很难被关注
                    let damp = if matches!(self.accounts[u].kind, Kind::Spammer) {
                        0.1
                    } else {
                        1.0
                    };
                    (u, sim * damp)
                })
                .collect();

            // 同质性：按相似度加权抽样
            for _ in 0..per {
                let total: f64 = candidates
                    .iter()
                    .map(|(_, w)| w.powf(self.cfg.homophily * 4.0) + 1e-6)
                    .sum();
                if total <= 0.0 {
                    break;
                }
                let mut r = self.rng.next_f64() * total;
                let mut chosen = None;
                for (u, w) in candidates.iter() {
                    let ww = w.powf(self.cfg.homophily * 4.0) + 1e-6;
                    if r < ww {
                        chosen = Some(*u);
                        break;
                    }
                    r -= ww;
                }
                if let Some(u) = chosen {
                    self.add_edge(s, u, 1.0);
                    candidates.retain(|(x, _)| *x != u);
                } else {
                    break;
                }
            }
        }
        self.normalize_edges();
    }

    fn init_ratings(&mut self) {
        let n = self.accounts.len();
        for s in 0..n {
            for u in 0..n {
                if s != u {
                    let base = 0.5 * cosine(&self.accounts[s].interest, &self.accounts[u].interest);
                    self.accounts[s].ratings[u] = (base + 0.1).clamp(-1.0, 1.0);
                }
            }
        }
    }

    fn wire_sybils(&mut self) {
        let sybils: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, Kind::Sybil { .. }))
            .map(|a| a.id)
            .collect();

        for i in 0..sybils.len() {
            for j in 0..sybils.len() {
                if i != j {
                    self.add_edge(sybils[i], sybils[j], 10.0);
                }
            }
        }

        // 渗透：每个女巫关注 2 个诚实账户
        let honest: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, Kind::Honest))
            .map(|a| a.id)
            .collect();
        if !honest.is_empty() {
            for &s in &sybils {
                for _ in 0..2 {
                    let v = honest[self.rng.next_usize(honest.len())];
                    self.add_edge(s, v, 1.0);
                }
            }
        }

        // 选一个目标
        if let Some(&t) = honest.first() {
            self.sybil_target = Some(t);
        }

        self.normalize_edges();
    }

    fn add_edge(&mut self, s: usize, v: usize, w: f64) {
        if s == v || s >= self.edges.len() || v >= self.edges.len() {
            return;
        }
        if let Some(slot) = self.edges[s].iter_mut().find(|(x, _)| *x == v) {
            slot.1 += w;
        } else {
            self.edges[s].push((v, w));
        }
    }

    fn normalize_edges(&mut self) {
        for es in self.edges.iter_mut() {
            let total: f64 = es.iter().map(|(_, w)| w).sum();
            if total > 0.0 {
                for (_, w) in es.iter_mut() {
                    *w /= total;
                }
            }
        }
    }

    /// 推进一步（1 小时）。
    pub fn step(&mut self) {
        let t = self.time;

        // 1. 新账户加入
        if self.rng.chance(self.cfg.join_rate) {
            self.spawn(t);
        }

        // 2. 离开
        self.churn(t);

        // 3. 互动
        self.interact(t);

        // 4. 定期重算信任
        if t % self.cfg.recompute_every as u64 == 0 && t > 0 {
            self.recompute_trust();
            self.collect(t);
        }

        self.time += 1;
    }

    fn spawn(&mut self, now: u64) {
        if self.accounts.len() >= self.accounts[0].ratings.len() {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;

        let kind = if self.rng.chance(self.cfg.attack_ratio.min(0.3)) {
            let cluster = self.rng.next_usize(5);
            Kind::Sybil { cluster }
        } else if self.rng.chance(self.cfg.spam_ratio.min(0.3)) {
            Kind::Spammer
        } else {
            Kind::Honest
        };

        let acc = Account::new(id, now, &self.cfg, &mut self.rng, kind);
        self.accounts.push(acc);
        self.edges.push(Vec::new());
        self.trust.push(vec![0.0; self.accounts[0].ratings.len()]);

        // 新账户关注几个同好
        let n = self.accounts.len();
        for _ in 0..5 {
            let total: f64 = (0..n - 1)
                .map(|u| cosine(&self.accounts[id].interest, &self.accounts[u].interest) + 0.01)
                .sum();
            if total <= 0.0 {
                break;
            }
            let mut r = self.rng.next_f64() * total;
            let mut chosen = id;
            for u in 0..n - 1 {
                let w = cosine(&self.accounts[id].interest, &self.accounts[u].interest) + 0.01;
                if r < w {
                    chosen = u;
                    break;
                }
                r -= w;
            }
            if chosen != id {
                self.add_edge(id, chosen, 1.0);
            }
        }
        self.normalize_edges();
    }

    fn churn(&mut self, now: u64) {
        let mut to_leave = Vec::new();
        for a in self.accounts.iter() {
            if a.left.is_some() {
                continue;
            }
            // 离开概率随「不活跃」上升
            let idle = now.saturating_sub(a.last_active);
            let p = self.cfg.leave_rate * (1.0 + idle as f64 / 720.0);
            if self.rng.chance(p) {
                to_leave.push(a.id);
            }
        }
        for id in to_leave {
            self.accounts[id].left = Some(now);
        }
    }

    fn interact(&mut self, now: u64) {
        let n = self.accounts.len();
        for s in 0..n {
            if !self.accounts[s].is_online(now, &self.cfg) {
                continue;
            }
            self.accounts[s].last_active = now;

            let actions = (self.accounts[s].activity * 2.0) as usize;
            for _ in 0..actions {
                // 选一个关注对象
                if self.edges[s].is_empty() {
                    break;
                }
                let idx = self.rng.next_usize(self.edges[s].len());
                let (v, _) = self.edges[s][idx];
                if v >= n || self.accounts[v].left.is_some() {
                    continue;
                }

                // 给 v 打分：基于兴趣相似度 + 噪声
                let sim = cosine(&self.accounts[s].interest, &self.accounts[v].interest);
                let noise = (self.rng.next_f64() - 0.5) * 0.3;
                let rating = (sim * 1.2 - 0.1 + noise).clamp(-1.0, 1.0);
                self.accounts[s].ratings[v] = rating;

                // 潜伏者突变
                if let Kind::Sleeper { normal_hours } = self.accounts[s].kind {
                    if now > normal_hours {
                        if let Some(t) = self.sybil_target {
                            self.accounts[s].ratings[t] = 1.0;
                        }
                    }
                }

                // 女巫给目标打满分
                if matches!(self.accounts[s].kind, Kind::Sybil { .. }) {
                    if let Some(t) = self.sybil_target {
                        self.accounts[s].ratings[t] = 1.0;
                    }
                }
            }
        }
    }

    /// 异步迭代重算信任。
    pub fn recompute_trust(&mut self) {
        let n = self.accounts.len();
        let beta = self.cfg.beta;
        let mut max_change = 0.0;

        for _ in 0..self.cfg.max_iters {
            let mut next = self.trust.clone();
            max_change = 0.0;

            for s in 0..n {
                for u in 0..n {
                    let mut val = beta * self.accounts[s].ratings[u];
                    for &(v, w) in &self.edges[s] {
                        if v < n {
                            val += (1.0 - beta) * w * self.trust[v][u];
                        }
                    }
                    let d = (val - next[s][u]).abs();
                    if d > max_change {
                        max_change = d;
                    }
                    next[s][u] = val;
                }
            }

            self.trust = next;
            if max_change < self.cfg.eps {
                break;
            }
        }
    }

    fn collect(&mut self, now: u64) {
        let n = self.accounts.len();
        let alive: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| a.left.is_none())
            .map(|a| a.id)
            .collect();

        let online = self
            .accounts
            .iter()
            .filter(|a| a.is_online(now, &self.cfg))
            .count();

        let n_edges: usize = self.edges.iter().map(|e| e.len()).sum();

        // 信任分布（只统计加入超过 7 天的诚实账户）
        let mut vals = Vec::new();
        for &s in &alive {
            if now.saturating_sub(self.accounts[s].joined) < 168 {
                continue;
            }
            for &u in &alive {
                if s != u && now.saturating_sub(self.accounts[u].joined) >= 168 {
                    vals.push(self.trust[s][u]);
                }
            }
        }
        let mean = if vals.is_empty() {
            0.0
        } else {
            vals.iter().sum::<f64>() / vals.len() as f64
        };
        let var = if vals.is_empty() {
            0.0
        } else {
            vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64
        };

        // 目标账户在诚实观察者眼中的平均信任
        let target_trust = self.sybil_target.map(|t| {
            let honest: Vec<usize> = self
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, Kind::Honest))
                .map(|a| a.id)
                .collect();
            if honest.is_empty() {
                0.0
            } else {
                honest.iter().map(|&s| self.trust[s][t]).sum::<f64>() / honest.len() as f64
            }
        });

        // 垃圾账户的平均信任
        let spammers: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| matches!(a.kind, Kind::Spammer))
            .map(|a| a.id)
            .collect();
        let spam_trust = if spammers.is_empty() {
            0.0
        } else {
            let honest: Vec<usize> = self
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, Kind::Honest))
                .map(|a| a.id)
                .collect();
            let mut sum = 0.0;
            let mut cnt = 0;
            for &s in &honest {
                for &sp in &spammers {
                    sum += self.trust[s][sp];
                    cnt += 1;
                }
            }
            if cnt == 0 { 0.0 } else { sum / cnt as f64 }
        };

        // 新用户（最近 7 天加入）的平均信任
        let recent: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| a.left.is_none() && now.saturating_sub(a.joined) < 168)
            .map(|a| a.id)
            .collect();
        let new_user_trust = if recent.is_empty() {
            0.0
        } else {
            let honest: Vec<usize> = self
                .accounts
                .iter()
                .filter(|a| matches!(a.kind, Kind::Honest))
                .map(|a| a.id)
                .collect();
            let mut sum = 0.0;
            let mut cnt = 0;
            for &s in &honest {
                for &u in &recent {
                    if s != u {
                        sum += self.trust[s][u];
                        cnt += 1;
                    }
                }
            }
            if cnt == 0 { 0.0 } else { sum / cnt as f64 }
        };

        self.stats.push(Snapshot {
            hour: now,
            n_alive: alive.len(),
            n_online: online,
            n_edges,
            max_change: 0.0,
            trust_mean: mean,
            trust_std: var.sqrt(),
            target_trust,
            spam_trust,
            new_user_trust,
        });
    }

    pub fn run(&mut self) {
        let total = self.cfg.days * self.cfg.steps_per_day;
        for _ in 0..total {
            self.step();
        }
    }
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}
