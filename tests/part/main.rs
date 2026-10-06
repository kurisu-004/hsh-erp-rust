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
//! - serial.rs            ← 原 serial_api.rs
//! - rollup_recompute.rs   ← 2026-10-01 新增：admin 对账端点（POST /admin/recompute-rollup）定点修正 + 幂等断言
//! - pickable_by_work_type.rs ← 2026-10-03 新增：`GET /parts/pickable-by-work-type/{id}`
//!   出参 `batch_id` / `batch_version` 批次锚点（本端点此前零覆盖）
//! - create_serial_price.rs ← 2026-10-05 新增：建单期序列号派发（`POST /parts` /
//!   `POST /parts/batch` / `POST /parts/batch-with-pdfs`）+ `unit_price` /
//!   `total_price` 入参落库
//! - purchase_order_import.rs ← 2026-10-06 新增：采购订单 Excel 导入两端点
//!   （`POST /parts/match-by-excel-items` 分档匹配 + `POST /parts/batch-update-order-info`
//!   三态回填 / skip）；此前这两条端点零覆盖

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

// 2026-09-23 PR13 Phase C：`common` / `helpers` 由各 sub-file 自带 `#[path]`
// 引入（edition 2024 下 `mod foo;` 在 sub-file 中只查 sibling 目录、不向上到 crate root），
// main.rs 仅列 sub-file 入口，不重复声明。
mod batch;
mod create_serial_price;
mod crud;
mod file;
mod inspection_batches;
mod lifecycle;
mod list_enrichment;
mod pickable_by_work_type;
// 2026-10-06 新增：采购订单 Excel 导入（match-by-excel-items + batch-update-order-info）
mod purchase_order_import;
mod repair;
mod rollup_recompute;
mod serial;
mod to_inspection;
mod to_process;
mod to_ship;
