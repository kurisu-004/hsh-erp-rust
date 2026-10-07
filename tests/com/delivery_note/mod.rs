//! com::delivery_note 域集成测试（2026-10-08 随域平移 com 域）
//!
//! cargo 1.98 auto-discover 只认 `tests/*.rs` 与 `tests/*/main.rs`（**一级**目录）。
//! 本目录在 `tests/com/` 之下（两级），故入口文件叫 **`mod.rs`** 而不是 `main.rs`，
//! 并由上层 `tests/com/main.rs` 的 `mod delivery_note;` 引入 —— 与
//! `tests/part/mod.rs` / `tests/production/…` 的既有分层做法一致。
//!
//! ## 拆分映射
//! - group.rs        ← 原 tests/delivery/group.rs（送货分组 4 端点）
//! - note.rs         ← 原 tests/delivery/note.rs（送货单 CRUD + 生命周期）
//! - scan.rs         ← 原 tests/delivery/scan.rs（扫码入单）
//! - scan_tree.rs    ← 2026-10-08 新增（`GET /com/delivery/note/scan/{serial_no}`
//!   三层树：形状 / 命中口径 / 批次不过滤 status / occupied_by_note_no / draft 判定）
//! - entry_gate.rs   ← 2026-10-08 新增（`POST /com/delivery/note/scan` 的 21405 /
//!   21406 / 21416 闸门）
//! - batch_allocation.rs ← 2026-10-08 新增（DP 分配算法端到端）
//! - drivers.rs      ← 2026-10-08 新增（`GET /com/delivery/drivers` 只返送货司机）
//! - driver.rs       ← 2026-10-08 新增（`POST /{id}/driver` + `validate_driver` 5 条
//!   + `/pickup` 重校 + 入参不再有 `driver_worker_id`）
//!
//! ## 2026-10-08 删除
//! - `attach_batches.rs` ← 原 tests/delivery/attach_batches.rs（`POST /{id}/attach-batches`
//!   端点随入单入口收敛为扫码单一入口一并删除）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod batch_allocation;
mod driver;
mod drivers;
mod entry_gate;
mod group;
mod note;
mod scan;
mod scan_tree;
