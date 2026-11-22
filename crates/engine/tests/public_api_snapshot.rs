//! 守卫 2：公开 API 快照测试。
//!
//! 每个原 crate（store / view / agg / engine）的公开项都 import 一次 ——
//! 编译通过即证明**合并后的路径仍然可达**。行为由搬过来的 454 条测试负责。

#![allow(unused_imports)]

use tm_engine::agg::{aggregate, combine, decay};
use tm_engine::budget::BudgetVerdict;
use tm_engine::criteria::{s3_floor, s3_relative_to_floor, BETA_DEFAULT};
use tm_engine::store::MAX_SCORE;
use tm_engine::{
    AccountIdx, AccountKey, AccountTable, AggMode, CompressedView, Engine, EngineConfig,
    EngineSnapshot, FetchRequest, FreshnessStats, NoiseStats, PositiveList, Rejection, Rng,
    RoundSampler, Score, StoredView, Time, ViewParams, ViewSnapshot, XorShift64, DECAY_FLOOR,
    MAX_TRIM, SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};

#[test]
fn all_pre_merge_public_items_are_reachable() {
    // 只验证能 import；行为断言由搬过来的测试负责。
    // 用运行期可变值承载，避免 clippy::assertions_on_constants
    let v: u32 = SNAPSHOT_VERSION;
    let m: [u8; 8] = SNAPSHOT_MAGIC;
    let s: f32 = MAX_SCORE;
    let d: f32 = DECAY_FLOOR;
    let tr: f32 = MAX_TRIM;
    let b: f32 = BETA_DEFAULT;
    assert!(v > 0);
    assert!(!m.is_empty());
    assert!(s > 0.0);
    assert!(d >= 0.0);
    assert!(tr > 0.0);
    assert!(b > 0.0);

    // 重新导出到模块下的路径也在
    let _ = tm_engine::store::AccountTable::new();
    let _ = tm_engine::view::ViewParams::new(64, 2, 7);
    let _ = tm_engine::runtime::EngineConfig::default();
}
