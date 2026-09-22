//! delivery_note 域状态机转换 handler
//!
//! 范围：状态机迁移端点：submit / recall / pickup-scan / pickup。
//!
//! 基础 CRUD 走 `crud.rs`；扫码入单走 `scan.rs`；打印走 `print.rs`。
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

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::modules::delivery_note::dto::{
    DeliveryNotePath, DeliveryNotePickupRequest, DeliveryNotePickupScanRequest,
    DeliveryNoteVersionedRequest,
};
use crate::modules::delivery_note::vo::{
    DeliveryNoteOut, DeliveryNotePickupScanOut, SubmitDeliveryOut,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// POST /api/v2/delivery-notes/{id}/submit
///
/// 出参 `SubmitDeliveryOut` 含两种 outcome，前端据此分支：
/// - `outcome = SUBMITTED`：`note` 为提交后的送货单投影；状态机 DRAFT → SUBMITTED 已发生；
///   本次提交会发出 `DELIVERY_NOTE_SUBMITTED` 大屏事件。
/// - `outcome = CANDIDATES_AVAILABLE`：存在仍在 `INSPECTION` 的已挂单批次，**本次未提交**；
///   `note` 为 `null`；`unresolved_targets` 按 part 分组列出未过检批次（含 `version`，
///   前端可一键转发到 `POST /parts/batch-to-ship` 让其到 READY_TO_SHIP 后再重提本接口）。
///   候选分支不写库、不发事件。
pub async fn submit_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<SubmitDeliveryOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .submit(&mut *tx, path.id, req.version, &current)
        .await?;
    tx.commit().await?;

    // 仅真正提交时广播；候选分支未写库，不发事件
    if let Some(note) = out.note.as_ref() {
        state
            .ws_hub
            .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
                kind: "DELIVERY_NOTE_SUBMITTED".to_string(),
                payload: serde_json::json!({
                    "delivery_note_id": note.id,
                    "delivery_note_no": note.delivery_note_no,
                }),
            });
    }

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/recall
pub async fn recall_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .recall(&mut *tx, path.id, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/pickup-scan
pub async fn pickup_scan(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNotePickupScanRequest>,
) -> Result<Json<R<DeliveryNotePickupScanOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .pickup_scan(
            &mut *tx,
            path.id,
            &req.part_serial,
            req.badge_code.as_deref(),
            &current,
        )
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/pickup
pub async fn pickup_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNotePickupRequest>,
) -> Result<Json<R<DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .pickup(
            &mut *tx,
            path.id,
            req.driver_worker_id,
            req.version,
            req.badge_code.as_deref(),
            &current,
        )
        .await?;
    tx.commit().await?;

    let payload = serde_json::json!({
        "delivery_note_id": out.id,
        "delivery_note_no": out.delivery_note_no,
        "part_count": out.part_count,
        "driver_worker_id": out.driver_worker_id,
    });
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_PICKED_UP".to_string(),
            payload: payload.clone(),
        });
    tracing::info!(?payload, "delivery_note picked up");

    Ok(Json(R::ok(out)))
}
