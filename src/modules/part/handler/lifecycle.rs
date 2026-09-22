//! part 域 lifecycle / 状态机扩展 handler
//!
//! 对应 Phase 1（2026-09-13）+ 4 个终态流转 + Phase 2（2026-09-13）pick-up：
//! - 1.1 上架 / 召回（place-on-shelf / recall-to-pending）
//! - 1.2 CNC 编程流转（send-to-programming / release-from-programming /
//!   recall-to-programming / pending-programming 列表）
//! - 1.3 外协流转（send-to-outsource / receive-from-outsource /
//!   receive-from-outsource-to-inspection / outsource-in-flight /
//!   outsource-sendable 列表）
//! - 1.4 返修闭环（complete-repair / repair-dispatch / repair-batches /
//!   repairing-batches 列表）
//! - 1.5 批次拆分 / 取消（split-batch / cancel-batch）
//! - 终态（deliver / cancel / complete / start-repair）
//! - Phase 2（pick-up 手动领取）
//!
//! WS 广播：commit 之后广播（对齐 Python 延迟广播模式）；事件名与 Python 一致。
//!
//! ## 权限模式
//! - deliver / cancel / complete / place-on-shelf / recall-to-pending /
//!   send-to-programming / split-batch / cancel-batch：Manager + Clerk
//! - release-from-programming / recall-to-programming：Manager + CncProgrammer
//! - send-to-outsource / receive-from-outsource /
//!   receive-from-outsource-to-inspection / complete-repair / repair-dispatch：
//!   Manager + Clerk + Inspector
//! - start-repair：Manager + Clerk + Inspector

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto_crud::{
    ByWorkTypeQuery, ByWorkerQuery, CancelBatchRequest, CancelRequest, CompleteRepairRequest,
    CompleteRequest, DeliverRequest, PickUpRequest, PlaceOnShelfRequest,
    RecallToPendingRequest, RecallToProgrammingRequest, ReceiveFromOutsourceToInspectionRequest,
    RepairDispatchRequest, SendToOutsourceRequest, SendToProgrammingRequest, SplitBatchRequest,
    StartRepairRequest,
};
use crate::modules::part::service::PartService;
use crate::modules::part::vo::{PartListOut, PartOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 通用 WS 广播 helper：单 kind + 单 payload 字段。
#[inline]
fn ws_broadcast(state: &AppState, kind: &str, payload: serde_json::Value) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload,
    });
}

/// `POST /api/v2/parts/{part_id}/deliver`
///
/// READY_TO_SHIP → DELIVERED；commit 后广播 `PART_DELIVERED`。
pub async fn deliver(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<DeliverRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::deliver(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_DELIVERED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/cancel`
///
/// 5 状态白名单 → CANCELLED；commit 后广播 `PART_CANCELLED`。
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::cancel(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_CANCELLED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/complete`
///
/// DELIVERED → COMPLETED；commit 后广播 `PART_COMPLETED`。
pub async fn complete(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CompleteRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::complete(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_COMPLETED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/start-repair`
///
/// IN_PROCESS → REPAIRING；commit 后广播 `PART_REPAIR_STARTED`。
pub async fn start_repair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<StartRepairRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::start_repair(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_STARTED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

// ===== Phase 1（2026-09-13）14 端点 =====

/// `POST /api/v2/parts/{part_id}/place-on-shelf`
pub async fn place_on_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::place_on_shelf(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_PLACED_ON_SHELF",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/recall-to-pending`
pub async fn recall_to_pending(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<RecallToPendingRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::recall_to_pending(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECALLED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/send-to-programming`
pub async fn send_to_programming(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<SendToProgrammingRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::send_to_programming(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_SENT_TO_PROGRAMMING",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/release-from-programming`
pub async fn release_from_programming(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::release_from_programming(&mut *tx, &state.snowflake, part_id, req, &current)
            .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RELEASED_FROM_PROGRAMMING",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/recall-to-programming`
pub async fn recall_to_programming(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<RecallToProgrammingRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::recall_to_programming(&mut *tx, &state.snowflake, part_id, req, &current)
        .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECALLED_TO_PROGRAMMING",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/pending-programming`
pub async fn list_pending_programming(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<crate::modules::part::dto_crud::PartListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_pending_programming(&mut *tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/send-to-outsource`
pub async fn send_to_outsource(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<SendToOutsourceRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::send_to_outsource(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_SENT_TO_OUTSOURCE",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/receive-from-outsource`
pub async fn receive_from_outsource(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::receive_from_outsource(&mut *tx, &state.snowflake, part_id, req, &current)
            .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECEIVED_FROM_OUTSOURCE",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/receive-from-outsource-to-inspection`
pub async fn receive_from_outsource_to_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ReceiveFromOutsourceToInspectionRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::receive_from_outsource_to_inspection(
        &mut *tx,
        &state.snowflake,
        part_id,
        req,
        &current,
    )
    .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECEIVED_FROM_OUTSOURCE_INSPECTED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/outsource-in-flight`
pub async fn list_outsource_in_flight(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<crate::modules::part::dto_crud::PartListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_outsource_in_flight(&mut *tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/outsource-sendable`
pub async fn list_outsource_sendable(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<crate::modules::part::dto_crud::PartListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_outsource_sendable(&mut *tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/complete-repair`
///
/// REPAIRING → IN_PROCESS（落回生产架）或 REPAIRING → INSPECTION（送检区）。
pub async fn complete_repair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CompleteRepairRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::complete_repair(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_COMPLETED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/repair-dispatch`
///
/// 一步式返修下发（INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED →
/// 由 shelf.zone 决定的 IN_PROCESS / INSPECTION）。
pub async fn repair_dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<RepairDispatchRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::repair_dispatch(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_DISPATCHED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/batches/split`
pub async fn split_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<SplitBatchRequest>,
) -> Result<Json<R<i64>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let new_batch_id =
        PartService::split_batch(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_BATCH_SPLIT",
        json!({
            "part_id": part_id.to_string(),
            "new_batch_id": new_batch_id.to_string(),
        }),
    );
    Ok(Json(R::ok(new_batch_id)))
}

/// `POST /api/v2/parts/{part_id}/batches/{batch_id}/cancel`
pub async fn cancel_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path((part_id, batch_id)): Path<(i64, i64)>,
    Json(req): Json<CancelBatchRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::cancel_batch(&mut *tx, &state.snowflake, part_id, batch_id, req, &current)
            .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_BATCH_CANCELLED",
        json!({
            "part_id": part_id.to_string(),
            "batch_id": batch_id.to_string(),
        }),
    );
    Ok(Json(R::ok(out)))
}

// ===== Phase 2（2026-09-13）手动 pick-up =====

/// `POST /api/v2/parts/{part_id}/pick-up`
///
/// 手动 pick-up（B 方案）：PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。
pub async fn pick_up(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PickUpRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::pick_up(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_PICKED_UP",
        json!({
            "part_id": part_id.to_string(),
            "worker_id": out.id.to_string(),
        }),
    );
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-work-type/{work_type_id}`
pub async fn list_by_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(work_type_id): Path<i64>,
    Query(query): Query<ByWorkTypeQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_by_work_type(&mut *tx, work_type_id, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/pickable-by-work-type/{work_type_id}`
///
/// 「可领取」列表（与 by-work-type 同形，但限定 shelf.zone=PRODUCTION + active）。
pub async fn list_pickable_by_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(work_type_id): Path<i64>,
    Query(query): Query<ByWorkTypeQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::list_pickable_by_work_type(&mut *tx, work_type_id, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-worker/{worker_id}`
pub async fn list_by_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(worker_id): Path<i64>,
    Query(query): Query<ByWorkerQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_by_worker(&mut *tx, worker_id, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}
