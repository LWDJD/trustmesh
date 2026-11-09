//! trustmesh 社会仿真框架（`tm_sim`）。
//!
//! 目标：造一个**带社会细节**的虚拟世界，跑时间，植入攻击者，输出「谁给谁打了多少分」
//! 这种输入数据，以及一组可以直接对照实验的指标骨架。
//!
//! 本 crate **不实现信任算法本身**。它只负责：
//!
//! 1. 生成随时间演化的人群与关系（谁关注谁）；
//! 2. 生成评分行为（谁给谁打了多少分，取值 (0,1]）；
//! 3. 统计结构化指标（账户数、边数、分类均分、圈子化、评分分布……）。
//!
//! # 世界模型
//!
//! 时间以**小时**为单位推进，`step()` 前进一小时。选小时是因为信任半衰期 H 的单位是小时，
//! 这样仿真时间戳可以直接喂给衰减公式。
//!
//! 每一小时做四件事，顺序固定（顺序固定是复现性的前提）：
//!
//! ```text
//! 1. 生命周期   休眠者醒来或永久离开；活跃者休眠/离开；按泊松过程加入新账户
//! 2. 攻击编排   潜伏者到点突变；渗透者被收买；确保攻击者关注被推广账户
//! 3. 关系演化   在线账户按兴趣 × 社团 × 声誉反馈加权，抽新的关注对象
//! 4. 互动评分   在线账户按幂律活跃强度产生若干次互动，对被互动者打分
//! ```
//!
//! # 社会细节
//!
//! | 维度 | 建模方式 |
//! |---|---|
//! | 人口 | 泊松加入、基础离开率（随活跃度放大）、休眠与回归（回归失败即流失） |
//! | 兴趣 | 8 维非负单位向量，关注概率按余弦相似度加权 |
//! | 同质性 | `homophily` ∈ `[0,1]`：同时压低跨社团关注比例、放大相似度指数 |
//! | 结构 | `community_count` 个社团，各有主题中心；`bridge_ratio` 比例的桥接节点额外跨社团 |
//! | 活跃度 | 每人有活跃时段（昼夜节律）+ 幂律活跃强度（帕累托分布） |
//! | 行为 | 评分 = 内容质量 × 兴趣匹配 × 噪声，映射到 `[0,1]`；低于表态阈值视为「没表态」 |
//! | 声誉反馈 | 可选开关：被评得高的人更容易被关注、被互动（马太效应） |
//!
//! # 攻击者
//!
//! - [`AttackKind::Sybil`]：女巫集群，只在集群内部互评满分，绝不对外打分
//! - [`AttackKind::Sleeper`]：潜伏者，`normal_until` 之前行为完全正常，之后给目标打满分
//! - [`AttackKind::Infiltrator`]：潜伏一段时间后，按声誉从高到低收买高分账户
//! - [`AttackKind::Endorser`]：按比例选出高质量账户，公开背书被推广账户
//! - [`AttackKind::Spammer`]：垃圾制造者，内容质量极低、活跃度极高、不看质量乱打分
//!
//! # 复现性
//!
//! 同一个 `seed` 必然产生逐位相同的世界：随机数自实现（xorshift64*），
//! 全程不使用 `HashMap` 等无序容器，所有遍历都按内部编号升序。
//!
//! ```
//! use tm_sim::{SimConfig, World};
//!
//! let cfg = SimConfig { n_initial: 200, steps: 200, seed: 7, ..SimConfig::default() };
//! let mut world = World::new(cfg);
//! world.run();
//!
//! let snap = world.report();
//! assert!(snap.alive <= snap.accounts);   // 账户表只增不减，离开只是换状态
//! assert!(snap.online <= snap.alive);     // 在线 ⊆ 存活
//! assert!(snap.all_finite());             // 指标里不会出现 NaN / inf
//!
//! let ratings = world.ratings();          // Vec<(观察者, 被评者, 评分)>
//! assert!(!ratings.is_empty());
//! assert!(ratings.iter().all(|(_, _, s)| *s > 0.0 && *s <= 1.0));
//!
//! println!("{}", snap.render());          // 人类可读的报表
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]
// 浮点判断经常写成 `!(x > 0.0)`：这种写法对 NaN 也成立，是本 crate 刻意选择的
// 「NaN 一律当非法值」语义。clippy 的这条风格提示在这里是噪音。
#![allow(clippy::neg_cmp_op_on_partial_ord)]

pub mod account;
pub mod attack;
pub mod behavior;
pub mod community;
pub mod config;
pub mod interest;
pub mod metrics;
pub mod rng;
pub mod world;

pub use account::{Account, Kind, Life};
pub use attack::AttackPlan;
pub use community::Communities;
pub use config::{AttackKind, SimConfig};
pub use interest::Interest;
pub use metrics::{ClassStat, CommunityStat, Distribution, Snapshot};
pub use rng::Rng;
pub use world::World;

/// 兴趣向量默认维度。
pub const INTEREST_DIMS: usize = 8;

/// 声誉代理的指数滑动平均系数。
///
/// 声誉只是「最近收到的评分」的平滑值，用于可选的马太效应开关，
/// 不是信任算法的输出（信任算法由 tm_store / tm_agg 等模块负责）。
pub const REPUTATION_ALPHA: f32 = 0.05;

/// 同质性对「相似度指数」的放大倍数。
///
/// homophily = 0 时指数为 1（温和偏好），homophily = 1 时指数为 1 + 5（强烈偏好同类）。
pub const HOMOPHILY_EXPONENT_GAIN: f64 = 5.0;
