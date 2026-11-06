//! 社交信任计算的核心数学实现。
//!
//! # 数学模型
//!
//! 设有 N 个用户，W 是行归一的邻接权重矩阵（W[s][v] = s 对 v 的相对权重），
//! P 是评分矩阵（P[s][u] = s 对 u 的直接评分，取值 [0,1]）。
//!
//! 定义信任矩阵 Π 为以下不动点方程的解：
//!
//! ```text
//! Π = β·P + (1-β)·W·Π
//! ```
//!
//! 其中 β ∈ [0,1) 是「个人权重」，1-β 是「群体权重」。
//!
//! 解的形式：
//!
//! ```text
//! Π = β·(I - (1-β)·W)⁻¹·P
//! ```
//!
//! Π[s][u] 的含义是：在观察者 s 的视角下，用户 u 的可信度。
//!
//! # 为什么可逆
//!
//! W 行随机（谱半径 ≤ 1），所以 (1-β)·W 的谱半径 ≤ 1-β < 1，
//! 因此 I - (1-β)·W 可逆，解唯一。
//!
//! # 算法
//!
//! - `solve_exact`：直接求逆，O(N³)，用于小图验证。
//! - `solve_local`：Local Push 近似，O(1/eps)，用于任意规模。

pub mod attack;
pub mod dense;
pub mod exact;
pub mod graph;
pub mod local_push;
pub mod rng;
pub mod scope;
pub mod sim;
pub mod sketch;

pub use exact::{residual, solve_exact};
pub use graph::Graph;
pub use local_push::{solve_local, LocalPushParams};

/// 默认个人权重。0.7 表示个人判断占七成，群体意见占三成。
pub const DEFAULT_BETA: f64 = 0.7;

/// 默认近似精度。
pub const DEFAULT_EPS: f64 = 1e-3;
