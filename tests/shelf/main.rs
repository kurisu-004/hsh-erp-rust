//! shelf 域集成测试（PR13 Phase D 拆分）
//!
//! 2 个原 test binary（shelf_api / worker_shelf_deactivate_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - api.rs       ← 原 shelf_api.rs
//! - deactivate.rs ← 原 worker_shelf_deactivate_api.rs

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod deactivate;