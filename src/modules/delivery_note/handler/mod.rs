//! delivery_note 域 HTTP handler 总入口（2026-09-22 PR7 重构）
//!
//! 按业务域拆分为多文件（沿用 part/handler/{crud,batch,lifecycle,inspection}.rs 范本）：
//! - `crud.rs` —— 基础 CRUD：create / list / get / update / add-parts / remove-parts /
//!   soft-delete / batch-detail / candidate-parts / pickup-pending / events + P1 送货分组 CRUD
//! - `lifecycle.rs` —— 状态机转换：submit / recall / pickup-scan / pickup
//! - `print.rs` —— 打印：print / print-labels（含 parse_i64_opt / parse_i64_map_opt 助手）
//! - `scan.rs` —— 扫码入单：scan（/scan 端点）+ attach-batches（弹窗提交时附挂批次）
//!
//! ## 约定（2026-09-22 D-5 + review 第 1 轮）
//! - 事务边界在 handler：`state.pool.begin()` → 借 `&mut *tx` 喂给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - **service 形参 by-value trait**（iam 严格范本）：handler 借 `&mut *tx` 给
//!   `state.delivery_note_service.xxx(&mut *tx, ...)` 或 `&mut *conn` 给读端点。
//! - **handler 三形态**：
//!   - ① 纯写端点 `pool.begin() → service → commit`；
//!   - ② 写 + post-commit Redis / WS（broadcast 落 handler，service 不持有 WsHub）`pool.begin() → service → commit → state.ws_hub.broadcast(...)`；
//!   - ③ 读端点（list_*/get_*）`pool.acquire() → service`，不开事务。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)`）；handler 这里只解析
//!   query / path / body。
//!
//! ## axum 0.8 path 占位符
//! `matchit` 风格 `{id}`（不再用 axum 0.7 的 `:id`）。

pub mod crud;
pub mod lifecycle;
pub mod print;
pub mod scan;

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;

use crate::state::AppState;

/// 本域路由表（设计 §6 + §6.2：delivery-notes）。
///
/// axum 静态段优先于参数段；`/scan`、`/candidate-parts`、`/pickup-pending` 必须
/// 在 `/{id}` 之前注册。`/print[/-labels]` 注册在 `/{id}/...` 段里，路径不冲突。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ---- delivery-notes/* ----
        .route("/batch-detail", get(crud::batch_get_delivery_notes)) // ★静态段必须早于 /{id}
        .route("/scan", post(scan::scan_delivery_note))
        .route("/candidate-parts", get(crud::list_candidate_parts))
        .route("/pickup-pending", get(crud::list_pickup_pending))
        .route(
            "/",
            get(crud::list_delivery_notes).post(crud::create_delivery_note),
        )
        .route("/{id}/events", get(crud::list_delivery_note_events))
        .route("/{id}/update", post(crud::update_delivery_note))
        .route("/{id}/add-parts", post(crud::add_delivery_note_parts))
        .route("/{id}/attach-batches", post(scan::attach_batches))
        .route("/{id}/remove-parts", post(crud::remove_delivery_note_parts))
        .route("/{id}/submit", post(lifecycle::submit_delivery_note))
        .route("/{id}/recall", post(lifecycle::recall_delivery_note))
        .route("/{id}/pickup-scan", post(lifecycle::pickup_scan))
        .route("/{id}/pickup", post(lifecycle::pickup_delivery_note))
        .route("/{id}/soft-delete", post(crud::soft_delete_delivery_note))
        .route("/{id}/print", post(print::print_delivery_note))
        .route("/{id}/print-labels", post(print::print_labels))
        .route("/{id}", get(crud::get_delivery_note))
}

/// P1 送货分组路由表（独立挂在 `/api/v2/delivery-groups`）。
pub fn p1_router() -> Router<Arc<AppState>> {
    p1_group_router()
}

// ===========================================================================
//  P1 送货分组 router（保留供 router() nest）
// ===========================================================================

/// P1 送货分组路由子表；前缀 `/delivery-groups` 已由 `router()` nest 上去。
fn p1_group_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/",
            get(crud::p1_list_delivery_groups).post(crud::p1_create_delivery_group),
        )
        .route("/{id}/update", post(crud::p1_update_delivery_group))
        .route(
            "/{id}/soft-delete",
            post(crud::p1_soft_delete_delivery_group),
        )
}

// ===========================================================================
//  Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_router_compiles() {
        let _ = std::marker::PhantomData::<Arc<AppState>>;
    }
}
