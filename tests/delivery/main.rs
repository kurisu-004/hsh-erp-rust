//! delivery 域集成测试（PR13 Phase B 拆分）
//!
//! 5 个原 test binary（delivery_group_api / delivery_attach_batches_api /
//! delivery_print_api / delivery_scan_api / delivery_note_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - group.rs         ← 原 delivery_group_api.rs
//! - attach_batches.rs ← 原 delivery_attach_batches_api.rs
//! - print.rs         ← 原 delivery_print_api.rs
//! - scan.rs          ← 原 delivery_scan_api.rs
//! - note.rs          ← 原 delivery_note_api.rs

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod group;
mod attach_batches;
mod print;
mod scan;
mod note;