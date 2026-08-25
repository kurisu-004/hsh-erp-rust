//! part 域
//!
//! 对应 Python myERP：
//! - api/v1/part.py
//! - service/part_service.py
//! - repository/part_repository.py
//! - model/part.py
//! - schema/part.py
pub mod dto;
pub mod dto_crud;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;
use axum::{
    routing::{get, post},
    Router,
};

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ★ 静态段必须在 /{part_id}/... catch-all 之前注册，
//   否则 axum 会把静态段（如 `batch`、`by-serial`、`worker-scan`、`batch-*-inspection`）
//   解析成 part_id。
        // ---- 列表 / 静态段 ----
        .route("/", get(handler::list_parts).post(handler::create_part))
        .route("/batch", post(handler::batch_create_parts))
        .route("/by-serial/{serial_no}", get(handler::get_by_serial))
        .route("/batch-pass-inspection", post(handler::batch_pass_inspection))
        .route("/batch-scan-inspect", post(handler::batch_scan_inspect))
        // worker-scan 静态段也必须在 /{part_id}/... 之前注册，
        // 否则 axum 会把 `worker-scan` 解析成 part_id=... 的 catch-all。
        .route("/worker-scan", post(handler::worker_scan))
        // ---- 单件 {part_id} ----
        .route("/{part_id}", get(handler::get_part_detail))
        .route("/{part_id}/update", post(handler::update_part))
        .route("/{part_id}/soft-delete", post(handler::soft_delete_part))
        .route("/{part_id}/upload-drawing", post(handler::upload_drawing))
        .route("/{part_id}/deliver", post(handler::deliver))
        .route("/{part_id}/cancel", post(handler::cancel))
        .route("/{part_id}/complete", post(handler::complete))
        .route("/{part_id}/start-repair", post(handler::start_repair))
        // ---- 既有 Phase F / F2 inspection 流 ----
        .route("/{part_id}/pass-inspection", post(handler::pass_inspection))
        .route("/{part_id}/scan-inspect", post(handler::scan_inspect))
        .route("/{part_id}/fail-inspection", post(handler::fail_inspection))
}