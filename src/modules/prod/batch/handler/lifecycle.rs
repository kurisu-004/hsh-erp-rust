//! 批次 lifecycle handler —— 终态 + 状态机扩展（全部 batch-anchored）。
//!
//! 2026-10-02 自 `part/handler/lifecycle.rs` 迁入 prod 域：以下端点的操作对象
//! 都是**批次**（OCC 锚 `t_part_batch.version`），路径锚由 `part_id` 改为 `batch_id`，
//! 入参 DTO 的 `batch_id` 字段随之删除（改由 URL 承载）。
//!
//! ## 端点
//! - `POST /api/v2/prod/batches/{batch_id}/deliver` / `complete` / `start-repair`
//! - `POST /api/v2/prod/batches/{batch_id}/place-on-shelf` / `recall-to-pending`
//! - `POST /api/v2/prod/batches/{batch_id}/release-from-programming`
//! - `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` / `receive-from-outsource`
//!   / `receive-from-outsource-to-inspection`
//! - `POST /api/v2/prod/batches/{batch_id}/complete-repair` / `repair-dispatch`
//! - `POST /api/v2/prod/batches/{batch_id}/split` / `cancel` / `pick-up`
//!
//! ## 权限模式
//! - deliver / complete / place-on-shelf / recall-to-pending / split / cancel：
//!   Manager + Clerk
//! - send-to-outsource / receive-from-outsource /
//!   receive-from-outsource-to-inspection / complete-repair / repair-dispatch：
//!   Manager + Clerk + Inspector
//! - start-repair：Manager + Clerk + Inspector
//!
//! ## 事务边界 + WS 广播
//! 事务边界在 handler（`pool.begin()` → service → `tx.commit()`）；WS 广播在
//! commit 之后（对齐 Python 延迟广播模式），`kind` 字符串逐字不变，payload 的
//! `part_id` 改取响应里的 `out.id` / 路径批次反查值（原先取 URL 的 `part_id`）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::batch::dto::{
    CancelBatchRequest, CompleteRepairRequest, CompleteRequest, DeliverRequest, PickUpRequest,
    PlaceOnShelfRequest, RecallToPendingRequest, ReceiveFromOutsourceToInspectionRequest,
    RepairDispatchRequest, SendToOutsourceRequest, SplitBatchRequest, StartRepairRequest,
};
use crate::modules::prod::batch::service::BatchService;
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

/// `POST /api/v2/prod/batches/{batch_id}/deliver`
///
/// READY_TO_SHIP → DELIVERED；commit 后广播 `PART_DELIVERED`。
pub async fn deliver(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<DeliverRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = BatchService::deliver(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_DELIVERED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/complete`
///
/// DELIVERED → COMPLETED；commit 后广播 `PART_COMPLETED`。
pub async fn complete(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<CompleteRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out = BatchService::complete(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_COMPLETED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/start-repair`
///
/// 源状态必须 `IN_PROCESS` **且** `is_repairing = false`（REPAIRING 是
/// `t_part_batch.is_repairing` 标记列，本端点**不再发生 status 迁移**，只把标记
/// 置 true）。commit 后广播 `PART_REPAIR_STARTED`。
pub async fn start_repair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<StartRepairRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::start_repair(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_STARTED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/place-on-shelf`
pub async fn place_on_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::place_on_shelf(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_PLACED_ON_SHELF",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/recall-to-pending`
pub async fn recall_to_pending(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<RecallToPendingRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = BatchService::recall_to_pending(&mut *tx, &state.snowflake, batch_id, req, &current)
        .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECALLED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/release-from-programming`
pub async fn release_from_programming(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::release_from_programming(&mut *tx, &state.snowflake, batch_id, req, &current)
            .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RELEASED_FROM_PROGRAMMING",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/send-to-outsource`
pub async fn send_to_outsource(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<SendToOutsourceRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = BatchService::send_to_outsource(&mut *tx, &state.snowflake, batch_id, req, &current)
        .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_SENT_TO_OUTSOURCE",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource`
pub async fn receive_from_outsource(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<PlaceOnShelfRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::receive_from_outsource(&mut *tx, &state.snowflake, batch_id, req, &current)
            .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECEIVED_FROM_OUTSOURCE",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection`
pub async fn receive_from_outsource_to_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<ReceiveFromOutsourceToInspectionRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = BatchService::receive_from_outsource_to_inspection(
        &mut *tx,
        &state.snowflake,
        batch_id,
        req,
        &current,
    )
    .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_RECEIVED_FROM_OUTSOURCE_INSPECTED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/complete-repair`
///
/// 要求批次 `is_repairing = true`（确实在返修中）。按 shelf.zone 决定去向：
/// PRODUCTION → `IN_PROCESS`（落回生产架、重新入池）或 INSPECTION →
/// `INSPECTION`（送检区）。两条路径都把 `is_repairing` 清回 false。
pub async fn complete_repair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<CompleteRepairRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::complete_repair(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_COMPLETED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/repair-dispatch`
///
/// 一步式返修下发（INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED →
/// 由 shelf.zone 决定的 IN_PROCESS / INSPECTION）。
pub async fn repair_dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<RepairDispatchRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::repair_dispatch(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_REPAIR_DISPATCHED",
        json!({ "part_id": out.id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/split`
pub async fn split_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<SplitBatchRequest>,
) -> Result<Json<R<i64>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let (new_batch_id, part_id) =
        BatchService::split_batch(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
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

/// `POST /api/v2/prod/batches/{batch_id}/cancel`
pub async fn cancel_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<CancelBatchRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        BatchService::cancel_batch(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_BATCH_CANCELLED",
        json!({
            "part_id": out.id.to_string(),
            "batch_id": batch_id.to_string(),
        }),
    );
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/{batch_id}/pick-up`
///
/// 手动 pick-up（B 方案）：PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。
///
/// 2026-10-03：`quantity` 支持部分领取（service 自动拆批）。**响应体形状不变**
/// （仍 `R<PartOut>`），拆批信息只走 WS。
pub async fn pick_up(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<PickUpRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    // 2026-10-02 修正：原填 `out.id`，而 `out: PartOut` 的 `id` 是 part id，
    // 与 `worker_id` 字段名不符。取值改回请求携带的 `req.worker_id`
    // （在 `req` 被 service 消费前先取出）。消费方只读 `kind`，payload 修正
    // 对现有前端无影响。
    let worker_id = req.worker_id;
    let outcome =
        BatchService::pick_up(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    // 2026-10-03：部分领取发生了拆批 → 补发 PART_BATCH_SPLIT。
    // 必须发：拆批把源批次的 quantity 静默扣减、并新建了一个批次行，其它端的
    // 批次视图不收到这条事件就永远看不到「源批次余量变了 / 多了一个批次」。
    // payload 字段与 `split_batch` 端点的 PART_BATCH_SPLIT 保持同形（消费方
    // 按 event type 分派，两处字段名必须一致）。
    if let Some(split) = outcome.split.as_ref() {
        ws_broadcast(
            &state,
            "PART_BATCH_SPLIT",
            json!({
                "part_id": split.part_id.to_string(),
                "new_batch_id": split.new_batch_id.to_string(),
                "source_batch_id": batch_id.to_string(),
                "quantity": split.quantity,
            }),
        );
    }
    // 2026-10-03：补 batch_id + quantity（整批路径 = 源批次 / 整批量；
    // 部分路径 = 拆出来的新批次 / 拆走量），消费方据此知道工人领走了哪一批。
    ws_broadcast(
        &state,
        "PART_PICKED_UP",
        json!({
            "part_id": outcome.part.id.to_string(),
            "worker_id": worker_id.to_string(),
            "batch_id": outcome.picked_batch_id.to_string(),
            "quantity": outcome.picked_quantity,
        }),
    );
    Ok(Json(R::ok(outcome.part)))
}
