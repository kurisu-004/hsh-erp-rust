//! part 域集成测试（PR13 Phase C 拆分）
//!
//! ## 拆分映射
//! - helpers.rs            ← 原 part_api_helpers.rs
//! - crud.rs               ← 原 part_crud.rs
//! - lifecycle.rs          ← 原 part_lifecycle_api.rs
//! - batch.rs              ← 原 part_batch_api.rs
//! - file.rs               ← 原 part_file_api.rs
//! - list_enrichment.rs    ← 原 part_list_enrichment_api.rs
//! - repair.rs             ← 原 part_repair_api.rs
//! - to_ship.rs            ← 原 part_api_to_ship.rs
//! - to_inspection.rs      ← 原 part_api_to_inspection.rs
//! - to_process.rs         ← 原 part_api_to_process.rs
//! - inspection_batches.rs ← 原 part_api_inspection_batches.rs
//! - serial.rs             ← 原 serial_api.rs

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

// 2026-09-23 PR13 Phase C：`common` / `helpers` 由各 sub-file 自带 `#[path]`
// 引入（edition 2024 下 `mod foo;` 在 sub-file 中只查 sibling 目录、不向上到 crate root），
// main.rs 仅列 sub-file 入口，不重复声明。
mod crud;
mod lifecycle;
mod batch;
mod file;
mod list_enrichment;
mod repair;
mod to_ship;
mod to_inspection;
mod to_process;
mod inspection_batches;
mod serial;