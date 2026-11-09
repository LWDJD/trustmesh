//! 世界：把人口、结构、行为、攻击串成时间循环。
//!
//! # 每一步（= 一小时）做什么
//!
//! ```text
//! step += 1; time += 1
//!   ① 生命周期：休眠者醒来（或流失）→ 活跃者休眠 → 活跃者离开 → 泊松加入
//!   ② 攻击编排：潜伏者到点突变 → 渗透者被收买 → 确保攻击者关注被推广账户
//!   ③ 关系演化：在线账户按「兴趣 × 社团 × 声誉」加权抽新的关注
//!   ④ 互动评分：在线账户产生若干次互动，对被互动者打分
//! ```
//!
//! 阶段顺序固定是**复现性**的一部分：同一个种子必须得到同一个世界，
//! 因此所有遍历都按内部编号升序，所有随机数都来自同一个 `Rng` 流。
//!
//! # 不变式
//!
//! 1. 内部编号只增不减（离开只是换状态）；
//! 2. 评分只落在「已关注且在线」的账户上；
//! 3. 评分值 ∈ (0,1]，0 表示没表态、不写入；
//! 4. 关注数 ≤ `max_follows`，账户数 ≤ `max_accounts`，因此内存有上界；
//! 5. 女巫账户只关注同集群成员，绝不对外打分；
//! 6. `life_traces` 与 `accounts` 同下标同长度，生命周期变更只经 `set_life` 写入。

use crate::account::{Account, Kind, Life, LifeTrace};
use crate::attack::AttackPlan;
use crate::behavior;
use crate::community::{cross_community_share, Communities};
use crate::config::{AttackKind, SimConfig};
use crate::interest::Interest;
use crate::metrics::Snapshot;
use crate::rng::Rng;
use crate::HOMOPHILY_EXPONENT_GAIN;

/// 每人每小时最多互动次数（防御性上限，防止极端活跃值打爆单步耗时）。
const MAX_INTERACTIONS_PER_STEP: usize = 32;
/// 每人每小时最多新增关注数。
const MAX_NEW_FOLLOWS_PER_STEP: usize = 8;
/// 活跃时段长度下限（小时）。
const MIN_ACTIVE_SPAN: usize = 6;
/// 活跃时段长度上限（小时）。
const MAX_ACTIVE_SPAN: usize = 18;
/// 垃圾制造者的关注数倍率（刷量）。
const SPAM_FOLLOW_FACTOR: usize = 3;

/// 一个正在演化的社会。
///
/// 所有字段公开，方便实验脚本直接读账户做自定义统计；
/// 但写入请走 `step`，否则会破坏不变式。
#[derive(Debug)]
pub struct World {
    /// 生效后的配置（已夹紧）
    pub cfg: SimConfig,
    /// 随机源
    pub rng: Rng,
    /// 账户表，内部编号 = 下标，永不回收
    pub accounts: Vec<Account>,
    /// 生命周期轨迹，与 `accounts` 同下标、同长度（在线历史就是靠它回放的）
    pub life_traces: Vec<LifeTrace>,
    /// 社团
    pub communities: Communities,
    /// 已推进的步数（= 小时数）
    pub step: usize,
    /// 逻辑时间（小时）
    pub time: u64,
    /// 攻击计划与执行结果
    pub attack: AttackPlan,
    /// 自动记录的快照（`snapshot_every > 0` 时才有）
    pub history: Vec<Snapshot>,
    /// 累计加入人数
    pub joins: usize,
    /// 累计流失人数
    pub leaves: usize,
    /// 累计进入休眠次数
    pub dormancy_events: usize,
}

impl World {
    /// 造一个世界。做完初始人口、植入攻击者、建立初始关注。
    ///
    /// 不会 panic：非法参数会被 `SimConfig::validated` 夹紧；
    /// `n_initial = 0` 时得到空世界，之后靠 `join_rate` 长出来。
    pub fn new(cfg: SimConfig) -> Self {
        let cfg = cfg.validated();
        let mut rng = Rng::new(cfg.seed);
        let community_slots = cfg.community_count.min(cfg.n_initial.max(1));
        let communities = Communities::new(community_slots, cfg.interest_dims, &mut rng);

        let mut world = World {
            cfg,
            rng,
            accounts: Vec::new(),
            life_traces: Vec::new(),
            communities,
            step: 0,
            time: 0,
            attack: AttackPlan::empty(),
            history: Vec::new(),
            joins: 0,
            leaves: 0,
            dormancy_events: 0,
        };

        let n_initial = world.cfg.n_initial;
        for _ in 0..n_initial {
            world.spawn(Kind::Honest);
        }
        world.install_attack();
        world.bootstrap_follows();

        if world.cfg.snapshot_every > 0 {
            let snap = world.report();
            world.history.push(snap);
        }
        world
    }

    // ════════════════════════════ 对外接口 ════════════════════════════

    /// 推进一小时。
    pub fn step(&mut self) {
        self.step += 1;
        self.time += 1;
        self.phase_lifecycle();
        self.phase_attack();
        self.phase_relations();
        self.phase_interactions();
        let every = self.cfg.snapshot_every;
        if every > 0 && self.step.checked_rem(every) == Some(0) {
            let snap = self.report();
            if self.history.len() >= self.cfg.max_history && !self.history.is_empty() {
                self.history.remove(0);
            }
            self.history.push(snap);
        }
    }

    /// 按配置里的 `steps` 跑完。
    pub fn run(&mut self) {
        self.run_steps(self.cfg.steps);
    }

    /// 跑 `n` 步。
    pub fn run_steps(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    /// 全世界的评分输入：`(观察者, 被评者, 评分)`。
    ///
    /// 顺序固定：先按观察者内部编号升序，再按评分写入顺序。
    /// 只包含正分（0 表示没表态，不会出现在这里）。
    pub fn ratings(&self) -> Vec<(usize, usize, f32)> {
        let mut out: Vec<(usize, usize, f32)> = Vec::new();
        for (s, a) in self.accounts.iter().enumerate() {
            out.reserve(a.ratings.len());
            for &(u, score) in &a.ratings {
                out.push((s, u, score));
            }
        }
        out
    }

    /// 当前时刻的完整快照。
    pub fn report(&self) -> Snapshot {
        Snapshot::compute(
            &self.accounts,
            self.step,
            self.time,
            self.cfg.enable_rhythm,
            self.communities.len(),
            self.attack.target,
        )
    }

    /// 存活账户数。
    /// 只读访问账户表（实验用）。
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn alive_count(&self) -> usize {
        self.accounts.iter().filter(|a| a.is_alive()).count()
    }

    /// 在线账户数。
    pub fn online_count(&self) -> usize {
        self.accounts
            .iter()
            .filter(|a| a.is_online(self.time, self.cfg.enable_rhythm))
            .count()
    }

    // ════════════════════════ 在线历史（只读） ════════════════════════

    /// 账户 `idx` 在时刻 `t` 是否在线。
    ///
    /// 与 [`Account::is_online`] 同义，但回答的是**历史**：按 `life_traces` 里记录的
    /// 生命周期变更重建 `t` 时刻的状态，再叠上活跃时段的判定。因此它对任意 `t` 都成立：
    /// 早于加入时刻一律 `false`；超过当前时刻时按「最后一次已知状态延续」推断
    /// （休眠不会自己醒来，离开不会复活，活跃账户继续按活跃时段上下线）。
    /// `idx` 越界返回 `false`，不 panic。
    pub fn was_online_at(&self, idx: usize, t: u64) -> bool {
        let account = match self.accounts.get(idx) {
            Some(a) => a,
            None => return false,
        };
        let trace = match self.life_traces.get(idx) {
            Some(x) => x,
            None => return false,
        };
        match trace.state_at(t) {
            Some(Life::Active) => account.in_active_window(t, self.cfg.enable_rhythm),
            _ => false,
        }
    }

    /// 账户最后一次在线的时间。
    ///
    /// - 还在网络里：当前时刻之前（含）最后一个在线时刻；一次都没在线过
    ///   （比如刚加入就落在昼夜节律的离线窗口里）时返回加入时刻；
    /// - 已离开：在线状态在离开时刻终结，返回离开时刻——与 [`World::offline_duration`]
    ///   的起点保持一致；
    /// - `idx` 越界返回 0。
    pub fn last_active_at(&self, idx: usize) -> u64 {
        let account = match self.accounts.get(idx) {
            Some(a) => a,
            None => return 0,
        };
        if let Life::Left { at } = account.life {
            return at;
        }
        self.last_online_at_or_before(idx, self.time)
            .unwrap_or(account.joined_at)
    }

    /// 账户在 `t` 时刻已经离线多久（小时）；在线则返回 0。
    ///
    /// 口径：
    /// - 在线 → 0；
    /// - 尚未加入（`t < joined_at`）或 `idx` 越界 → 0（这时还谈不上「离线」）；
    /// - 休眠 / 活跃时段的离线窗口 → `t − 最后一次在线时刻`；
    /// - 已离开 → `t − 离开时刻`：离开之后视为永久离线，且离线时长从离开时刻起算。
    pub fn offline_duration(&self, idx: usize, t: u64) -> u64 {
        let account = match self.accounts.get(idx) {
            Some(a) => a,
            None => return 0,
        };
        if t < account.joined_at {
            return 0;
        }
        if let Some(Life::Left { at }) = self.life_at(idx, t) {
            if t >= at {
                return t - at;
            }
        }
        if self.was_online_at(idx, t) {
            return 0;
        }
        let since = self
            .last_online_at_or_before(idx, t)
            .unwrap_or(account.joined_at);
        t.saturating_sub(since)
    }

    /// `t` 时刻生效的生命周期状态（编号越界或早于加入时刻返回 `None`）。
    fn life_at(&self, idx: usize, t: u64) -> Option<Life> {
        self.life_traces.get(idx).and_then(|x| x.state_at(t))
    }

    /// `t` 之前（含）最后一个在线时刻；从未在线过返回 `None`。
    ///
    /// 逐段回放轨迹：只有 `Active` 段里才可能在线，段内再用活跃时段取最后一个时刻。
    fn last_online_at_or_before(&self, idx: usize, t: u64) -> Option<u64> {
        let account = self.accounts.get(idx)?;
        if t < account.joined_at {
            return None;
        }
        let trace = self.life_traces.get(idx)?;
        let events = trace.events();
        let rhythm = self.cfg.enable_rhythm;
        for (i, &(start, life)) in events.iter().enumerate().rev() {
            if start > t {
                continue;
            }
            // 本段状态的结束时刻：下一条变更的前一小时；最后一段延续到 t
            let end = match events.get(i + 1) {
                Some(&(next, _)) => next.saturating_sub(1),
                None => t,
            };
            let hi = end.min(t);
            if hi < start {
                continue;
            }
            if matches!(life, Life::Active) {
                if let Some(x) = account.last_in_window(start, hi, rhythm) {
                    return Some(x);
                }
            }
        }
        None
    }

    /// 评分边总数。
    pub fn rating_count(&self) -> usize {
        self.accounts.iter().map(|a| a.ratings.len()).sum()
    }

    /// 某类账户的内部编号（升序）。
    pub fn indices_of(&self, kind: Kind) -> Vec<usize> {
        self.accounts
            .iter()
            .filter(|a| a.kind == kind)
            .map(|a| a.idx)
            .collect()
    }

    // ════════════════════════════ 阶段 ①：生命周期 ════════════════════════════

    fn phase_lifecycle(&mut self) {
        let time = self.time;

        // 1. 休眠者醒来，或永久流失
        for i in 0..self.accounts.len() {
            let until = match self.accounts[i].life {
                Life::Dormant { until } => until,
                _ => continue,
            };
            if time < until {
                continue;
            }
            if self.rng.chance(self.cfg.return_rate) {
                self.set_life(i, Life::Active);
            } else {
                self.set_life(i, Life::Left { at: time });
                self.leaves += 1;
            }
        }

        // 2. 活跃者进入休眠：越不活跃越容易睡
        if self.cfg.dormancy_rate > 0.0 {
            for i in 0..self.accounts.len() {
                if !matches!(self.accounts[i].life, Life::Active) {
                    continue;
                }
                let act = self.accounts[i].activity;
                let idle = 1.0 - (act / 4.0).min(1.0);
                let p = self.cfg.dormancy_rate * (1.0 + 2.0 * idle);
                if self.rng.chance(p) {
                    let (lo, hi) = self.cfg.sleep_hours;
                    let hours = self.rng.range_inclusive(lo as usize, hi as usize) as u64;
                    let until = time + hours;
                    self.set_life(i, Life::Dormant { until });
                    self.dormancy_events += 1;
                }
            }
        }

        // 3. 活跃者永久离开：低活跃账户流失更快
        if self.cfg.leave_rate > 0.0 {
            for i in 0..self.accounts.len() {
                if !matches!(self.accounts[i].life, Life::Active) {
                    continue;
                }
                let act = self.accounts[i].activity;
                let idle = 1.0 - (act / 4.0).min(1.0);
                let p = self.cfg.leave_rate * (1.0 + 3.0 * idle);
                if self.rng.chance(p) {
                    self.set_life(i, Life::Left { at: time });
                    self.leaves += 1;
                }
            }
        }

        // 4. 新账户加入（泊松过程）
        let joins = self.rng.poisson(self.cfg.join_rate);
        for _ in 0..joins {
            if self.accounts.len() >= self.cfg.max_accounts {
                break;
            }
            let idx = self.spawn(Kind::Honest);
            let k = self
                .rng
                .range_inclusive(self.cfg.follow_min, self.cfg.follow_max);
            self.grow_follows(idx, k);
            self.joins += 1;
        }
    }

    // ════════════════════════════ 阶段 ②：攻击编排 ════════════════════════════

    fn phase_attack(&mut self) {
        let time = self.time;
        let target = self.attack.target;

        // 潜伏者到点突变：从此给被推广账户打满分，同时继续正常打分掩盖自己
        let mut wake: Vec<usize> = Vec::new();
        for i in 0..self.accounts.len() {
            if let Kind::Sleeper { normal_until } = self.accounts[i].kind {
                if !self.accounts[i].attack_active && time >= normal_until {
                    wake.push(i);
                }
            }
        }
        for i in wake {
            self.accounts[i].attack_active = true;
            if let Some(t) = target {
                self.push_follow(i, t);
            }
        }

        // 渗透者：到点按声誉从高到低收买
        if let AttackKind::Infiltrator = self.cfg.attack_kind {
            if !self.attack.done && self.attack.target.is_some() && time >= self.attack.purchased_at
            {
                self.attack.done = true;
                self.purchase_infiltrators();
            }
        }
    }

    /// 收买声誉最高的一批诚实账户。
    ///
    /// 排序规则：声誉降序，声誉相同则内部编号升序（保证可复现）。
    fn purchase_infiltrators(&mut self) {
        let target = match self.attack.target {
            Some(t) => t,
            None => return,
        };
        let want = self.cfg.attack_population();
        if want == 0 {
            return;
        }
        let mut candidates: Vec<usize> = self
            .accounts
            .iter()
            .filter(|a| a.kind == Kind::Honest && a.is_alive())
            .map(|a| a.idx)
            .collect();
        candidates.sort_by(|&a, &b| {
            let ra = self.accounts[a].reputation;
            let rb = self.accounts[b].reputation;
            rb.partial_cmp(&ra)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(&b))
        });
        let chosen: Vec<usize> = candidates.into_iter().take(want).collect();
        let bought_at = self.time;
        for &c in &chosen {
            self.accounts[c].kind = Kind::Infiltrator { bought_at };
            self.accounts[c].attack_active = true;
            self.push_follow(c, target);
        }
        self.attack.purchased = chosen.clone();
        self.attack.attackers = chosen;
    }

    // ════════════════════════════ 阶段 ③：关系演化 ════════════════════════════

    fn phase_relations(&mut self) {
        if self.cfg.follow_rate <= 0.0 {
            return;
        }
        let time = self.time;
        let rhythm = self.cfg.enable_rhythm;
        for i in 0..self.accounts.len() {
            if !self.accounts[i].kind.uses_social_discovery() {
                continue;
            }
            if !self.accounts[i].is_online(time, rhythm) {
                continue;
            }
            if self.accounts[i].follows.len() >= self.cfg.max_follows {
                continue;
            }
            let lambda = self.cfg.follow_rate * self.accounts[i].activity;
            let attempts = self.rng.poisson(lambda).min(MAX_NEW_FOLLOWS_PER_STEP);
            for _ in 0..attempts {
                if self.accounts[i].follows.len() >= self.cfg.max_follows {
                    break;
                }
                let picked = self.sample_follows(i, 1);
                match picked.first() {
                    Some(&u) => self.push_follow(i, u),
                    None => break,
                }
            }
        }
    }

    // ════════════════════════════ 阶段 ④：互动评分 ════════════════════════════

    fn phase_interactions(&mut self) {
        if self.cfg.interactions_per_step <= 0.0 {
            return;
        }
        let time = self.time;
        let rhythm = self.cfg.enable_rhythm;
        for i in 0..self.accounts.len() {
            if !self.accounts[i].is_online(time, rhythm) {
                continue;
            }
            if self.accounts[i].follows.is_empty() {
                continue;
            }
            let lambda = self.cfg.interactions_per_step * self.accounts[i].activity;
            let n = self.rng.poisson(lambda).min(MAX_INTERACTIONS_PER_STEP);
            for _ in 0..n {
                let partner = match self.pick_partner(i) {
                    Some(u) => u,
                    None => break,
                };
                if let Some(score) = self.compute_rating(i, partner) {
                    self.record_rating(i, partner, score);
                }
            }
        }
    }

    /// 从「我关注且在线」的人里挑一个互动对象。
    ///
    /// 权重 = 1 + 声誉反馈，攻击者对被推广账户额外加权（`attack_focus`）。
    fn pick_partner(&mut self, s: usize) -> Option<usize> {
        let time = self.time;
        let rhythm = self.cfg.enable_rhythm;
        let target = self.attack.target;
        let focus = self.cfg.attack_focus;
        let rep_fb = self.cfg.reputation_feedback as f64;
        let batch = {
            let me = &self.accounts[s];
            let mut cands: Vec<(usize, f64)> = Vec::new();
            for &u in &me.follows {
                let other = &self.accounts[u];
                if !other.is_online(time, rhythm) {
                    continue;
                }
                let mut w = 1.0 + rep_fb * other.reputation as f64 * 2.0;
                if me.attack_active && target == Some(u) {
                    w *= focus;
                }
                cands.push((u, w));
            }
            cands
        };
        if batch.is_empty() {
            return None;
        }
        let weights: Vec<f64> = batch.iter().map(|(_, w)| *w).collect();
        let pick = self.rng.weighted_index(&weights)?;
        Some(batch[pick].0)
    }

    /// 我（`s`）这次给 `u` 打多少分。
    ///
    /// 返回 `None` 表示「没表态」，不写入任何记录。
    fn compute_rating(&mut self, s: usize, u: usize) -> Option<f32> {
        let min_stance = self.cfg.min_stance;
        let kind = self.accounts[s].kind;
        let active = self.accounts[s].attack_active;
        let target = self.attack.target;

        match kind {
            // 女巫：只对同集群成员打满分，绝不对外打分
            Kind::Sybil { cluster } => {
                if let Kind::Sybil { cluster: c2 } = self.accounts[u].kind {
                    if c2 == cluster {
                        return behavior::stance(behavior::sybil_score(), min_stance);
                    }
                }
                None
            }
            // 垃圾制造者：不看质量，随机虚高
            Kind::Spammer => {
                let u01 = self.rng.next_f64();
                behavior::stance(behavior::spam_score(u01), min_stance)
            }
            // 攻击生效后的潜伏者/渗透者/背书者：对被推广账户打满分
            Kind::Sleeper { .. } | Kind::Infiltrator { .. } | Kind::Endorser
                if active && target == Some(u) =>
            {
                behavior::stance(behavior::attack_score(), min_stance)
            }
            // 其余一律按诚实逻辑：内容质量 × 兴趣匹配 × 噪声
            _ => {
                let quality = self.accounts[u].quality as f64;
                let jitter = 1.0 + self.cfg.content_noise * self.rng.normal(0.0, 1.0);
                let q_eff = (quality * jitter).clamp(0.0, 1.0) as f32;
                let m = self.accounts[s]
                    .interest
                    .match_score(&self.accounts[u].interest);
                let noise = self.rng.lognormal(0.0, self.cfg.rating_noise);
                let score = behavior::content_score(q_eff, m, noise);
                behavior::stance(score, min_stance)
            }
        }
    }

    /// 写入一条评分，并同步维护对方的「收到合计」与声誉代理。
    ///
    /// 同一对观察者/被评者只保留一条最新评分；更新时只调整增量，不重复计数。
    fn record_rating(&mut self, s: usize, u: usize, score: f32) {
        if !score.is_finite() || score <= 0.0 {
            return;
        }
        let score = score.clamp(0.0, 1.0);
        let mut old: Option<f32> = None;
        {
            let me = &mut self.accounts[s];
            match me.ratings.iter().position(|(t, _)| *t == u) {
                Some(pos) => {
                    old = Some(me.ratings[pos].1);
                    me.ratings[pos].1 = score;
                }
                None => me.ratings.push((u, score)),
            }
        }
        let other = &mut self.accounts[u];
        match old {
            Some(prev) => other.received_sum += (score - prev) as f64,
            None => {
                other.received_sum += score as f64;
                other.received_count += 1;
            }
        }
        other.reputation += crate::REPUTATION_ALPHA * (score - other.reputation);
    }

    // ════════════════════════════ 人口与关系的基础操作 ════════════════════════════

    /// 修改生命周期状态，并同步记进轨迹。
    ///
    /// 这是全世界**唯一**允许改 `life` 的入口：在线历史（`was_online_at` 等）
    /// 直接由轨迹回放得到，绕开它写 `life` 会让历史和现状对不上。
    fn set_life(&mut self, idx: usize, life: Life) {
        if let Some(account) = self.accounts.get_mut(idx) {
            account.life = life;
        }
        if let Some(trace) = self.life_traces.get_mut(idx) {
            trace.record(self.time, life);
        }
    }

    /// 创建一个账户并登记进社团，返回内部编号。
    fn spawn(&mut self, kind: Kind) -> usize {
        let idx = self.accounts.len();
        let time = self.time;
        let community = self.pick_community(idx);
        let dims = self.cfg.interest_dims;
        let center = self.communities.center(community).clone();
        let interest =
            Interest::mixed(&center, self.cfg.topic_correlation as f32, dims, &mut self.rng);
        let quality = self.sample_quality(kind);
        let activity = self
            .rng
            .pareto(self.cfg.activity_alpha, 1.0)
            .min(self.cfg.activity_cap)
            .max(1.0);
        let active_start = self.rng.below(24) as u8;
        let span = self
            .rng
            .range_inclusive(MIN_ACTIVE_SPAN, MAX_ACTIVE_SPAN) as u8;
        let is_bridge = self.rng.chance(self.cfg.bridge_ratio);

        let account = Account {
            idx,
            kind,
            joined_at: time,
            community,
            is_bridge,
            interest,
            quality,
            activity,
            active_start,
            active_span: span,
            life: Life::Active,
            attack_active: false,
            reputation: 0.0,
            received_sum: 0.0,
            received_count: 0,
            follows: Vec::new(),
            ratings: Vec::new(),
        };
        self.accounts.push(account);
        // 轨迹与账户表同下标：新账户从「加入即活跃」开始记
        self.life_traces.push(LifeTrace::new(time));
        self.communities.add_member(community, idx);
        idx
    }

    /// 前 K 个账户先铺满每个社团，之后按规模加权（富者更富，但空社团不会被饿死）。
    fn pick_community(&mut self, idx: usize) -> usize {
        let k = self.communities.len();
        if idx < k {
            idx
        } else {
            self.communities.choose(&mut self.rng)
        }
    }

    /// 按种类抽内容质量。
    ///
    /// 攻击者的质量差异是刻意设置的：
    /// - 女巫/垃圾：内容极差（0.02~0.08），所以除非互刷，否则拿不到高分；
    /// - 潜伏者：正常水平（0.35~0.65），不可从质量上识别；
    /// - 背书者：高质量（0.64~0.80），所以他们的背书「看起来可信」；
    /// - 被推广账户：极差（0.05~0.15），攻击的唯一作用就是把它抬起来。
    fn sample_quality(&mut self, kind: Kind) -> f32 {
        match kind {
            Kind::Honest => self.rng.normal(0.55, 0.15).clamp(0.05, 1.0) as f32,
            Kind::Promoted => (0.05 + 0.10 * self.rng.next_f64()) as f32,
            Kind::Sybil { .. } => (0.02 + 0.06 * self.rng.next_f64()) as f32,
            Kind::Sleeper { .. } => self.rng.normal(0.50, 0.15).clamp(0.15, 0.90) as f32,
            Kind::Infiltrator { .. } => self.rng.normal(0.60, 0.10).clamp(0.30, 0.95) as f32,
            Kind::Endorser => self.rng.normal(0.72, 0.08).clamp(0.45, 1.0) as f32,
            Kind::Spammer => (0.01 + 0.05 * self.rng.next_f64()) as f32,
        }
    }

    /// 我的跨社团关注比例。
    fn cross_share_of(&self, s: usize) -> f64 {
        cross_community_share(
            self.cfg.homophily,
            self.accounts[s].is_bridge,
            self.cfg.bridge_gain,
            self.communities.len(),
        )
    }

    /// 给账户 `s` 建立初始关注。
    fn bootstrap_follows(&mut self) {
        for i in 0..self.accounts.len() {
            let kind = self.accounts[i].kind;
            if !kind.uses_social_discovery() {
                continue;
            }
            let mut k = self
                .rng
                .range_inclusive(self.cfg.follow_min, self.cfg.follow_max);
            if matches!(kind, Kind::Spammer) {
                k = (k * SPAM_FOLLOW_FACTOR).min(self.cfg.max_follows);
            }
            self.grow_follows(i, k);
        }
    }

    /// 尝试新增 `k` 个关注。
    fn grow_follows(&mut self, s: usize, k: usize) {
        if k == 0 {
            return;
        }
        let picked = self.sample_follows(s, k);
        for u in picked {
            self.push_follow(s, u);
        }
    }

    /// 抽 `k` 个候选关注对象（同社团 + 跨社团两池）。
    fn sample_follows(&mut self, s: usize, k: usize) -> Vec<usize> {
        let room = self
            .cfg
            .max_follows
            .saturating_sub(self.accounts[s].follows.len());
        let k = k.min(room);
        if k == 0 {
            return Vec::new();
        }
        let share = self.cross_share_of(s);
        let cross_n = ((k as f64) * share).round() as usize;
        let cross_n = cross_n.min(k);
        let same_n = k - cross_n;

        let mut out = self.draw_from_pool(s, same_n, true);
        out.extend(self.draw_from_pool(s, cross_n, false));

        // 某一侧候选不足时从另一侧补，避免新账户「关注不到人」
        if out.len() < k {
            let need = k - out.len();
            let mut extra = if share < 0.5 {
                self.draw_from_pool(s, need, false)
            } else {
                self.draw_from_pool(s, need, true)
            };
            out.append(&mut extra);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 从「同社团」或「某个其它社团」的成员池里，按权重不放回抽 `n` 个。
    ///
    /// 权重 = 兴趣相似度^指数 × 内容质量 × 声誉反馈：
    /// - 相似度指数随同质性上升（1 → 1+5），同质性越高越挑剔同类；
    /// - 内容质量让「有内容的人」更容易被关注；
    /// - 声誉反馈打开时，高声誉者额外加权（马太效应）。
    ///
    /// **女巫不会出现在正常社交发现里**：没有人为它背书时，它进不了任何人的视野。
    /// 这是设计里「只有被评过分的人才能顺着评分链条被看到」的直接体现；
    /// 女巫想渗透，只能靠显式买边（渗透者 / 背书者场景）。
    fn draw_from_pool(&mut self, s: usize, n: usize, same: bool) -> Vec<usize> {
        if n == 0 {
            return Vec::new();
        }
        let s_comm = self.accounts[s].community;
        let pool: Vec<usize> = if same {
            self.communities.members(s_comm).to_vec()
        } else {
            if self.communities.len() <= 1 {
                return Vec::new();
            }
            match self.communities.other_than(s_comm, &mut self.rng) {
                Some(c) => self.communities.members(c).to_vec(),
                None => return Vec::new(),
            }
        };

        let me = &self.accounts[s];
        let homophily = self.cfg.homophily;
        let exponent = 1.0 + homophily * HOMOPHILY_EXPONENT_GAIN;
        let rep_fb = self.cfg.reputation_feedback as f64;

        let mut targets: Vec<usize> = Vec::with_capacity(pool.len());
        let mut weights: Vec<f64> = Vec::with_capacity(pool.len());
        for &u in &pool {
            if u == s {
                continue;
            }
            let other = &self.accounts[u];
            if !other.is_alive() || me.follows(u) {
                continue;
            }
            // 女巫对正常人不可见（没有背书就没有曝光）
            if matches!(other.kind, Kind::Sybil { .. }) {
                continue;
            }
            let sim = me.interest.cosine(&other.interest) as f64;
            let w_interest = (0.5 + 0.5 * sim).powf(exponent);
            let w_quality = 0.25 + other.quality as f64;
            let w_rep = 1.0 + rep_fb * other.reputation as f64 * 3.0;
            targets.push(u);
            weights.push(w_interest * w_quality * w_rep);
        }
        let picked = self.rng.weighted_without_replacement(&weights, n);
        picked.into_iter().map(|i| targets[i]).collect()
    }

    /// 加一条关注边（去重、不关注自己、不关注已离开的人、不超过上限）。
    fn push_follow(&mut self, s: usize, u: usize) {
        if s == u || u >= self.accounts.len() || s >= self.accounts.len() {
            return;
        }
        if !self.accounts[u].is_alive() {
            return;
        }
        if self.accounts[s].follows.len() >= self.cfg.max_follows {
            return;
        }
        if self.accounts[s].follows.contains(&u) {
            return;
        }
        self.accounts[s].follows.push(u);
    }

    /// 植入攻击者。每个场景的收益方（`target`）在这里确定。
    fn install_attack(&mut self) {
        let kind = self.cfg.attack_kind;
        let n_attack = self.cfg.attack_population();

        match kind {
            AttackKind::None => {}
            AttackKind::Sybil { cluster } => {
                let k = cluster.max(1);
                let mut clusters: Vec<Vec<usize>> = vec![Vec::new(); k];
                for i in 0..n_attack {
                    let c = i % k;
                    let idx = self.spawn(Kind::Sybil { cluster: c });
                    clusters[c].push(idx);
                }
                // 集群内部全连接：女巫只跟同集群的人互评
                let mut attackers: Vec<usize> = Vec::new();
                for group in &clusters {
                    for &s in group {
                        for &u in group {
                            if s != u {
                                self.push_follow(s, u);
                            }
                        }
                        attackers.push(s);
                    }
                }
                // 攻击比例为 0 时会留下空集群，清掉以免报表出现幻影集群
                clusters.retain(|g| !g.is_empty());
                self.attack.target = clusters.iter().flatten().next().copied();
                self.attack.attackers = attackers;
                self.attack.clusters = clusters;
            }
            AttackKind::Sleeper { normal_until } => {
                let mut list: Vec<usize> = Vec::new();
                for _ in 0..n_attack {
                    list.push(self.spawn(Kind::Sleeper { normal_until }));
                }
                // 潜伏者必须有一个「被推广」的外部目标。
                // 早期实现把 target 指向第一个潜伏者自己，而 push_follow 拒绝自环，
                // 于是单个潜伏者的攻击载荷完全空转（只测到它作为普通账户的结构位置）。
                if !list.is_empty() {
                    let promoted = self.spawn(Kind::Promoted);
                    self.attack.target = Some(promoted);
                    for &s in &list {
                        self.push_follow(s, promoted);
                    }
                    // 突变后要给目标打满分，得先关注它
                }
                self.attack.attackers = list;
            }
            AttackKind::Infiltrator => {
                if self.cfg.attack_ratio > 0.0 {
                    let promoted = self.spawn(Kind::Promoted);
                    self.attack.target = Some(promoted);
                    self.attack.purchased_at = self.cfg.resolved_infiltrate_at();
                }
            }
            AttackKind::Endorser { ratio } => {
                let ratio = if ratio.is_finite() {
                    ratio.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let n_end = ((self.cfg.n_initial as f64) * ratio as f64).round() as usize;
                if n_end > 0 && self.cfg.n_initial > 0 {
                    let promoted = self.spawn(Kind::Promoted);
                    self.attack.target = Some(promoted);
                    // 挑质量最高的诚实账户去背书：只有「看起来可信」的账户背书才有效
                    let mut honest: Vec<usize> = self
                        .accounts
                        .iter()
                        .filter(|a| a.kind == Kind::Honest)
                        .map(|a| a.idx)
                        .collect();
                    honest.sort_by(|&a, &b| {
                        let qa = self.accounts[a].quality;
                        let qb = self.accounts[b].quality;
                        qb.partial_cmp(&qa)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.cmp(&b))
                    });
                    let chosen: Vec<usize> = honest.into_iter().take(n_end).collect();
                    for &e in &chosen {
                        self.accounts[e].kind = Kind::Endorser;
                        self.accounts[e].attack_active = true;
                        self.push_follow(e, promoted);
                    }
                    self.attack.endorsers = chosen.len();
                    self.attack.attackers = chosen;
                }
            }
            AttackKind::Spammer => {
                let mut list: Vec<usize> = Vec::new();
                for _ in 0..n_attack {
                    list.push(self.spawn(Kind::Spammer));
                }
                self.attack.target = list.first().copied();
                self.attack.attackers = list;
            }
        }
        self.attack.kind = Some(kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 小世界() -> World {
        World::new(SimConfig {
            n_initial: 40,
            steps: 30,
            join_rate: 0.3,
            community_count: 3,
            ..SimConfig::default()
        })
    }

    #[test]
    fn 初始人口与社团铺满() {
        let w = 小世界();
        assert_eq!(w.accounts.len(), 40);
        assert_eq!(w.alive_count(), 40);
        assert_eq!(w.communities.len(), 3);
        // 前 3 个账户分别落在 3 个社团，保证没有空社团
        assert_eq!(w.accounts[0].community, 0);
        assert_eq!(w.accounts[1].community, 1);
        assert_eq!(w.accounts[2].community, 2);
        for c in 0..3 {
            assert!(w.communities.member_count(c) > 0);
        }
        // 每个账户都有初始关注
        assert!(w.accounts.iter().all(|a| !a.follows.is_empty()));
    }

    #[test]
    fn 空世界不崩() {
        let mut w = World::new(SimConfig {
            n_initial: 0,
            steps: 5,
            join_rate: 0.0,
            ..SimConfig::default()
        });
        assert_eq!(w.accounts.len(), 0);
        w.run();
        let s = w.report();
        assert_eq!(s.accounts, 0);
        assert_eq!(s.alive, 0);
        assert!(s.all_finite());
        // 没有攻击者时也安全
        assert!(w.ratings().is_empty());
    }

    #[test]
    fn 关注不超过上限且不重复() {
        let w = 小世界();
        for a in &w.accounts {
            assert!(a.follows.len() <= w.cfg.max_follows);
            let mut v = a.follows.clone();
            v.sort_unstable();
            v.dedup();
            assert_eq!(v.len(), a.follows.len(), "账户 {} 有重复关注", a.idx);
            assert!(!a.follows.contains(&a.idx), "账户 {} 关注了自己", a.idx);
        }
    }

    #[test]
    fn 评分只落在关注对象上且都是正分() {
        let mut w = 小世界();
        w.run();
        let ratings = w.ratings();
        assert!(!ratings.is_empty());
        for (s, u, score) in ratings {
            assert!(
                w.accounts[s].follows.contains(&u),
                "账户 {s} 给没关注的 {u} 打了分"
            );
            assert!(score > 0.0 && score <= 1.0, "评分越界: {score}");
        }
        // 收到的合计与逐条重算一致
        for a in &w.accounts {
            let sum: f64 = w
                .accounts
                .iter()
                .filter_map(|o| o.rating_of(a.idx))
                .map(|x| x as f64)
                .sum();
            assert!((sum - a.received_sum).abs() < 1e-4, "账户 {} 收到合计不一致", a.idx);
            let n = w
                .accounts
                .iter()
                .filter(|o| o.rating_of(a.idx).is_some())
                .count();
            assert_eq!(n, a.received_count);
        }
    }

    #[test]
    fn 时间与步数推进() {
        let mut w = 小世界();
        assert_eq!(w.time, 0);
        assert_eq!(w.step, 0);
        w.step();
        assert_eq!(w.time, 1);
        assert_eq!(w.step, 1);
        w.run();
        assert_eq!(w.step, 1 + w.cfg.steps);
        assert_eq!(w.time, 1 + w.cfg.steps as u64);
    }

    #[test]
    fn 重复评分只保留最新一条() {
        let mut w = 小世界();
        let s = 0usize;
        let u = w.accounts[0].follows[0];
        w.record_rating(s, u, 0.4);
        let after_first = w.accounts[u].received_sum;
        w.record_rating(s, u, 0.9);
        // 只更新，不累加
        assert!((w.accounts[u].received_sum - (after_first - 0.4 + 0.9)).abs() < 1e-6);
        assert_eq!(w.accounts[u].received_count, 1);
        assert_eq!(w.accounts[s].rating_of(u), Some(0.9));
        // 非法评分被忽略
        let before = w.accounts[u].received_sum;
        w.record_rating(s, u, 0.0);
        w.record_rating(s, u, f32::NAN);
        assert_eq!(w.accounts[u].received_sum, before);
        // 声誉代理是收到评分的指数滑动平均，落在 `[0,1]`
        assert!((0.0..=1.0).contains(&w.accounts[u].reputation));
    }

    #[test]
    fn 平均关注数与在线数合理() {
        let w = 小世界();
        let total: usize = w.accounts.iter().map(|a| a.follows.len()).sum();
        let avg = total as f64 / w.accounts.len() as f64;
        assert!((4.0..=12.0).contains(&avg), "平均关注数 {avg}");
        // 节律打开时不可能全员在线
        let online = w.online_count();
        assert!(online < w.accounts.len());
        assert!(online > w.accounts.len() / 5);
    }

    #[test]
    fn 关闭节律后全员在线() {
        let w = World::new(SimConfig {
            n_initial: 20,
            enable_rhythm: false,
            ..SimConfig::default()
        });
        assert_eq!(w.online_count(), 20);
    }

    #[test]
    fn 攻击比例为零时不产生攻击者() {
        let w = World::new(SimConfig {
            n_initial: 20,
            attack_kind: AttackKind::Sybil { cluster: 2 },
            attack_ratio: 0.0,
            ..SimConfig::default()
        });
        assert!(w.attack.attackers.is_empty());
        assert!(w.attack.clusters.is_empty());
        assert!(w.attack.target.is_none());
        assert!(w.indices_of(Kind::Honest).len() == 20);
    }

    #[test]
    fn 女巫集群内部全连接() {
        let w = World::new(SimConfig {
            n_initial: 50,
            attack_kind: AttackKind::Sybil { cluster: 2 },
            attack_ratio: 0.2, // 10 个女巫，每个集群 5 个
            ..SimConfig::default()
        });
        assert_eq!(w.attack.clusters.len(), 2);
        for group in &w.attack.clusters {
            assert_eq!(group.len(), 5);
            for &s in group {
                for &u in group {
                    if s != u {
                        assert!(
                            w.accounts[s].follows.contains(&u),
                            "女巫 {s} 没有关注同集群的 {u}"
                        );
                    }
                }
                // 不关注集群外的人
                for &u in &w.accounts[s].follows {
                    assert!(
                        group.contains(&u),
                        "女巫 {s} 关注了集群外的 {u}"
                    );
                }
            }
        }
    }

    #[test]
    fn 插入评分不会破坏接收统计() {
        let mut w = 小世界();
        w.run_steps(10);
        for a in &w.accounts {
            let mut n = 0;
            let mut sum = 0.0;
            for o in &w.accounts {
                if let Some(x) = o.rating_of(a.idx) {
                    n += 1;
                    sum += x as f64;
                }
            }
            assert_eq!(n, a.received_count, "账户 {} 收到条数不一致", a.idx);
            assert!((sum - a.received_sum).abs() < 1e-4);
        }
    }
}
