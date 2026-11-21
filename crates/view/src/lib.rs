//! # 兼容层：tm_view
//!
//! **本 crate 已于 2026-11-21 合并进 `tm_engine` 的 `view` 模块。**
//!
//! 保留这个空壳只有一个目的：不破坏既有的 import 路径。
//! 既有代码里的 `use tm_view::CompressedView;` 仍然可用。
//!
//! ## 新代码请用
//!
//! ~~~text
//! use tm_engine::view::CompressedView;
//! ~~~
//!
//! ## 为什么不直接删
//!
//! 仓内有 60+ 个文件引用旧路径（`crates/exp` 的实验 bin、`crates/mincore` 的对照实验、
//! `tools/` 的 wasm 对拍）。留一层转发能让「合并结构」和「改 68 处 import」分成两步，
//! 任何一步出问题都能独立回滚。

#![forbid(unsafe_code)]

pub use tm_engine::view::*;
