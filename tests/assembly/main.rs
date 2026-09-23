//! assembly 域集成测试（PR13 Phase D 拆分）
//!
//! 3 个原 test binary（assembly_api / assembly_files_api / assembly_status_sync）
//! 合并为 1 个 binary，入口 `main.rs`（cargo 1.98 auto-discover 约定；
//! `mod.rs` 不被识别 —— 由 Phase B 验证）。
//!
//! ## 拆分映射
//! - api.rs         ← 原 assembly_api.rs
//! - files.rs       ← 原 assembly_files_api.rs
//! - status_sync.rs ← 原 assembly_status_sync.rs（依赖 tests/part_api_helpers.rs，
//!   通过 `#[path = "../part_api_helpers.rs"]` 跨 subdir 引用，
//!   part 域拆分由 Phase C 处理，本 Phase 不动 part 侧）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod files;
mod status_sync;