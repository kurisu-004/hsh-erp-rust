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

use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

use crate::state::AppState;

// 2026-09-15 followup-cleanup A8：part 维度文件路由集中处（cad-files / cnc-programs /
// setup-sheets / cnc-pair / files）。`part::router()` 把 part_id 段 nest 到这个子路由。
use crate::modules::part_file;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ★ 静态段必须在 /{part_id}/... catch-all 之前注册，
        //   否则 axum 会把静态段（如 `batch`、`by-serial`、`worker-scan`、`batch-to-*`）
        //   解析成 part_id。
        // ---- 列表 / 静态段 ----
        .route("/", get(handler::list_parts).post(handler::create_part))
        .route("/batch", post(handler::batch_create_parts))
        .route("/by-serial/{serial_no}", get(handler::get_by_serial))
        .route(
            "/by-serial/{serial_no}/part-batches",
            get(handler::get_by_serial_part_batches),
        )
        .route("/batch-to-ship", post(handler::batch_to_ship))
        .route("/batch-to-inspection", post(handler::batch_to_inspection))
        // inspection 列表（status 筛选）也必须在 /{part_id} 之前注册，
        // 否则 axum 会把 `inspection-batches` 解析成 part_id 的 catch-all。
        .route("/inspection-batches", get(handler::list_inspection_batches))
        // worker-scan 静态段也必须在 /{part_id}/... 之前注册，
        // 否则 axum 会把 `worker-scan` 解析成 part_id=... 的 catch-all。
        .route("/worker-scan", post(handler::worker_scan))
        // ---- Phase 1（2026-09-13）静态段（在 {part_id} catch-all 之前注册）----
        .route(
            "/pending-programming",
            get(handler::list_pending_programming),
        )
        .route(
            "/outsource-in-flight",
            get(handler::list_outsource_in_flight),
        )
        .route("/outsource-sendable", get(handler::list_outsource_sendable))
        .route("/repair-batches", get(handler::list_repair_batches))
        .route("/repairing-batches", get(handler::list_repairing_batches))
        .route("/location-tree", get(handler::get_location_tree))
        .route("/scan/deliver-part", post(handler::scan_deliver_part))
        .route("/match-by-excel-items", post(handler::match_by_excel_items))
        .route(
            "/batch-update-order-info",
            post(handler::batch_update_order_info),
        )
        .route("/batch-with-pdfs", post(handler::batch_with_pdfs))
        // ---- Phase 2 (2026-09-13) 静态段（by-work-type / pickable / by-worker）----
        .route(
            "/by-work-type/{work_type_id}",
            get(handler::list_by_work_type),
        )
        .route(
            "/pickable-by-work-type/{work_type_id}",
            get(handler::list_pickable_by_work_type),
        )
        .route("/by-worker/{worker_id}", get(handler::list_by_worker))
        // ---- 单件 {part_id} ----
        .route("/{part_id}", get(handler::get_part_detail))
        .route("/{part_id}/update", post(handler::update_part))
        .route("/{part_id}/soft-delete", post(handler::soft_delete_part))
        .route("/{part_id}/upload-drawing", post(handler::upload_drawing))
        .route("/{part_id}/upload-3d-model", post(handler::upload_3d_model)) // 2026-09-11 新增：3D 模型上传
        .route("/{part_id}/deliver", post(handler::deliver))
        .route("/{part_id}/cancel", post(handler::cancel))
        .route("/{part_id}/complete", post(handler::complete))
        .route("/{part_id}/start-repair", post(handler::start_repair))
        // ---- Phase 1 单件端点 ----
        .route("/{part_id}/place-on-shelf", post(handler::place_on_shelf))
        .route(
            "/{part_id}/recall-to-pending",
            post(handler::recall_to_pending),
        )
        .route(
            "/{part_id}/send-to-programming",
            post(handler::send_to_programming),
        )
        .route(
            "/{part_id}/release-from-programming",
            post(handler::release_from_programming),
        )
        .route(
            "/{part_id}/recall-to-programming",
            post(handler::recall_to_programming),
        )
        .route(
            "/{part_id}/send-to-outsource",
            post(handler::send_to_outsource),
        )
        .route(
            "/{part_id}/receive-from-outsource",
            post(handler::receive_from_outsource),
        )
        .route(
            "/{part_id}/receive-from-outsource-to-inspection",
            post(handler::receive_from_outsource_to_inspection),
        )
        .route("/{part_id}/complete-repair", post(handler::complete_repair))
        .route("/{part_id}/repair-dispatch", post(handler::repair_dispatch))
        .route("/{part_id}/scan-inspect", post(handler::scan_inspect))
        .route("/{part_id}/events", get(handler::list_part_events))
        .route("/{part_id}/batches", get(handler::list_part_batches))
        .route("/{part_id}/batches/split", post(handler::split_batch))
        .route(
            "/{part_id}/batches/{batch_id}/cancel",
            post(handler::cancel_batch),
        )
        // ---- Phase 2 (2026-09-13) 手动 pick-up ----
        .route("/{part_id}/pick-up", post(handler::pick_up))
        // ---- to-XXX 流（替换 Phase F / F2 inspection）----
        .route("/{part_id}/to-ship", post(handler::to_ship))
        .route("/{part_id}/to-inspection", post(handler::to_inspection))
        .route("/{part_id}/to-process", post(handler::to_process))
        // ---- 2026-09-15 followup-cleanup A8：part 维度文件路由（cad-files / cnc-programs /
        //      setup-sheets / cnc-pair / files）已迁出到 part_file::part_nested_router()，
        //      在这里 nest 以保留历史 URL `/api/v2/parts/{part_id}/<file>...`。
        // ---- 2026-09-16 M2-B 新增：直传 COS 链路 confirm 端点（POST /parts/{id}/files/confirm）----
        .route("/{part_id}/files/confirm", post(handler::confirm_part_file))
        .nest("/{part_id}", part_file::handler::part_nested_router())
}
