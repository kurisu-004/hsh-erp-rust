//! part 域 lifecycle / 状态机扩展 handler
//!
//! 对应 Phase 1（2026-09-13）+ 4 个终态流转 + Phase 2（2026-09-13）pick-up：
//! - 1.1 上架 / 召回（place-on-shelf / recall-to-pending）
//! - 1.2 CNC 编程流转（release-from-programming）；2026-10-07 下线：
//!   `pending-programming` 列表（前端 2026-10-01 已迁至
//!   `GET /prod/programming/pending`）；`send-to-programming` /
//!   `recall-to-programming` 已于 2026-09-29 删除
//! - 1.3 外协流转（send-to-outsource / receive-from-outsource /
//!   receive-from-outsource-to-inspection）—— **2026-10-02 已随批次用例迁往
//!   `prod::batch`**；配套的 2 条外协 list 端点（`/outsource-in-flight` /
//!   `/outsource-sendable`）于 2026-10-03 因**返回形状与前端外协域不匹配**一并
//!   下线，取代者见 `outsource` 域的 `/outsource-shipments/in-flight` 与
//!   `/outsource-sendable`
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
//!   split-batch / cancel-batch：Manager + Clerk
//! - release-from-programming：Manager + CncProgrammer
//! - send-to-outsource / receive-from-outsource /
//!   receive-from-outsource-to-inspection / complete-repair / repair-dispatch：
//!   Manager + Clerk + Inspector
//! - start-repair：Manager + Clerk + Inspector
//! - force-complete（2026-09-30 新增）：**Manager 单角色** —— 逃生通道，明确不下放 Clerk

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto_crud::{
    ByWorkTypeQuery, ByWorkerQuery, CancelRequest, ForceCompleteRequest,
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

/// `POST /api/v2/parts/{part_id}/force-complete`（2026-09-30 新增）
///
/// MANAGER **单角色** 强推工单 + 该工单下所有活跃批次为 COMPLETED（绕状态机）。
/// 事件日志 `FORCE_COMPLETED` 区别常规 COMPLETED；commit 后广播
/// `PART_FORCE_COMPLETED`。
pub async fn force_complete(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ForceCompleteRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let out =
        PartService::force_complete(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "PART_FORCE_COMPLETED",
        json!({ "part_id": part_id.to_string() }),
    );
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-work-type/{work_type_id}`
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`。
pub async fn list_by_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(work_type_id): Path<i64>,
    Query(query): Query<ByWorkTypeQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::list_by_work_type(&mut *conn, work_type_id, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/pickable-by-work-type/{work_type_id}`
///
/// 「可领取」列表（与 by-work-type 同形，但限定 shelf.zone=PRODUCTION + active）。
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`。
pub async fn list_pickable_by_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(work_type_id): Path<i64>,
    Query(query): Query<ByWorkTypeQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out =
        PartService::list_pickable_by_work_type(&mut *conn, work_type_id, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-worker/{worker_id}`
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`。
pub async fn list_by_worker(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(worker_id): Path<i64>,
    Query(query): Query<ByWorkerQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::list_by_worker(&mut *conn, worker_id, &query, &current).await?;
    Ok(Json(R::ok(out)))
}
