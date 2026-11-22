//! # tm_engine —— trustmesh 运行时（单 crate）
//!
//! 用户只需要依赖**这一个** crate：
//!
//! ~~~rust
//! use tm_engine::{Engine, EngineConfig, CompressedView, ViewParams};
//! ~~~
//!
//! ## 四个模块
//!
//! | 模块 | 原 crate | 职责 |
//! |---|---|---|
//! | [`store`] | `tm_store` | 账户表、正分清单、轮次抽样 |
//! | [`view`] | `tm_view` | Count Sketch 定长压缩观点 |
//! | [`agg`] | `tm_agg` | 指数衰减与聚合（3 个纯函数） |
//! | [`runtime`] | `tm_engine` | 运行时本体：Engine / 抽样 / 查询 / 快照 / badlist |
//!
//! ## 顶层 re-export
//!
//! 为避免跨模块重名（`AccountKey` / `Score` 在多个原 crate 里都有定义），
//! 顶层 re-export 是**人工挑选**的，不是 glob：
//!
//! * [`AccountKey`] / [`Score`] 取 **store 版本**（类型与原 view/agg 版本完全相同，语义兼容）；
//! * 需要 view 版本语义时用 [`view::AccountKey`]。
//!
//! ## 命名说明
//!
//! 四个模块的内容原先是四个独立 crate（`tm_store` / `tm_view` / `tm_agg` / `tm_engine`），
//! 2026-11-21 合并进本 crate。合并时**没有保留兼容层空壳**——本库尚未发布过任何版本，
//! 不存在依赖旧路径的外部用户，留着空壳只是给未来的自己挖坑。
//!
//! ## 引擎本身不做任何 I/O
//!
//! * 它产出「我需要谁的观点」的请求意图，由应用层去真正发起
//! * 应用层拿到观点后回调 `on_view`
//!
//! 详细运行时文档见 [`runtime`]。

#![forbid(unsafe_code)]

// 运行时的内部模块（原 engine/src 下的顶层模块）
mod badlist;
pub mod budget;
pub mod criteria;
mod snapshot;

pub mod agg;
pub mod runtime;
pub mod store;
pub mod view;

pub use snapshot::{EngineSnapshot, ViewSnapshot, SNAPSHOT_MAGIC, SNAPSHOT_VERSION};

// runtime 的公开项（Engine / EngineConfig / StoredView / 等）
pub use runtime::{Engine, EngineConfig, FetchRequest, FreshnessStats, Rejection, StoredView};

// ---- 顶层 re-export（人工挑选，避免 E0252 重名冲突）----

// store 版本（AccountKey = [u8;32]，Score = f32）
pub use store::{
    AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler, Score, Time,
};

// view：AccountKey / Score 与 store 同类型，不重复导出
pub use view::{CompressedView, NoiseStats, ViewParams, XorShift64};

// agg：Score 与 store 同类型，不重复导出
pub use agg::{aggregate, combine, decay, AggMode, DECAY_FLOOR, MAX_TRIM};
