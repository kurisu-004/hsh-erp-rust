//! assembly 域集成测试（PR13 Phase D 拆分 + 2026-09-25 D-7/D-8/D-9 端点）
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
//!
//! ## 2026-09-25 api-drift-fix 新增 3 sub-file
//! - children.rs    ← D-07 POST /assemblies/{id}/children
//! - by_part.rs     ← D-08 GET  /parts/{id}/assembly
//! - files_list.rs  ← D-09 GET  /assemblies/{id}/files

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod by_part;
mod children;
mod files;
mod files_list;
mod status_sync;
