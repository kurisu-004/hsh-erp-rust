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
//! - `POST /api/v2/prod/batches/{batch_id}/complete-repair` / `repair-dispatch`
//! - `POST /api/v2/prod/batches/{batch_id}/cancel`
//!
//! ## 2026-10-09 拆批端点迁出
//! `POST /api/v2/prod/batches/{batch_id}/split` 提升为**顶层共用端点**
//! `POST /api/v2/batches/split`（`split_batch_by_body`，三个消费方：生产队列看板 /
//! 外协看板 / 零件详情页）。旧路径 404、**无 alias**，`batch_id` 改入 body，
//! 出参由 `R<i64>` 裸数字换成 `BatchSplitOut`（全 ID 字符串 —— 旧出参被 JS
//! 截断精度）。WS 事件名 `PART_BATCH_SPLIT` 与 payload 逐字不变。
//!
//! ## 2026-10-10 pick-up 迁出
//! `POST /api/v2/prod/batches/{batch_id}/pick-up` 连同其 service 与 DTO 迁往
//! `crate::modules::prod::scan`（新路径
//! `POST /api/v2/prod/scan/batches/{batch_id}/pick-up`，**无 alias**；响应体
//! `R<PartOut>` 与 WS 事件名 `PART_PICKED_UP` / `PART_BATCH_SPLIT` 逐字不变）。
//!
//! ## 2026-10-09 外协三端点迁出
//! `send-to-outsource` / `receive-from-outsource` /
//! `receive-from-outsource-to-inspection` 已合并为 `POST /api/v2/outsource-queue/move`
//! （`crate::modules::outsource::handler::move_batch`，文件 `handler/move.rs`），本文件
//! 三个 handler 与其 service 一并
//! 删除。随之删除的 WS 事件名是 `PART_SENT_TO_OUTSOURCE` /
//! `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED`，三合一
//! 后统一为 `OUTSOURCE_MOVE_DONE`；`t_part_event` 的审计字面量 `SENT_TO_OUTSOURCE` /
//! `RECEIVED_FROM_OUTSOURCE` 逐字保留（第三个 `RECEIVED_TO_INSPECTION` 随 2026-10-10
//! 的 `OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向下线而不再被写入）。
//!
//! ## 权限模式
//! - deliver / complete / place-on-shelf / recall-to-pending / split / cancel：
//!   Manager + Clerk
//! - complete-repair / repair-dispatch / start-repair：Manager + Clerk + Inspector
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
    CancelBatchRequest, CompleteRepairRequest, CompleteRequest, DeliverRequest,
    PlaceOnShelfRequest, RepairDispatchRequest, SplitBatchByBodyRequest, StartRepairRequest,
};
use crate::modules::prod::batch::service::BatchService;
use crate::modules::prod::batch::vo::BatchSplitOut;
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

/// `POST /api/v2/batches/split` —— **顶层**共用端点（挂载点
/// `/api/v2/batches`，见 `modules::mod` 的 `v2_router` 与
/// `prod::batch::handler::split_router`），不是本域 `/prod/batches/*` 下的路由。
///
/// `batch_id` 从 body 取（硬切自 `POST /api/v2/prod/batches/{batch_id}/split`
/// 的路径参数，旧路径已下线、无 alias）。
pub async fn split_batch_by_body(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<SplitBatchByBodyRequest>,
) -> Result<Json<R<BatchSplitOut>>, AppError> {
    let batch_id = req.batch_id;
    let mut tx = state.pool.begin().await?;
    let outcome = BatchService::split_batch(&mut *tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_BATCH_SPLIT",
        json!({
            "part_id": outcome.part_id.to_string(),
            "new_batch_id": outcome.new_batch_id.to_string(),
        }),
    );
    Ok(Json(R::ok(BatchSplitOut {
        batch_id,
        new_batch_id: outcome.new_batch_id,
        part_id: outcome.part_id,
        quantity: outcome.quantity,
        source_version: outcome.source_version,
    })))
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
