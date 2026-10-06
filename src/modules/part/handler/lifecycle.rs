//! part 域 lifecycle / 状态机扩展 handler
//!
//! 本文件只服务 part 维度的 2 条写端点 + 3 条批次行 list 端点：
//! - `POST /api/v2/parts/{part_id}/cancel`
//! - `POST /api/v2/parts/{part_id}/force-complete`
//! - `GET /api/v2/parts/by-work-type/{work_type_id}`
//! - `GET /api/v2/parts/pickable-by-work-type/{work_type_id}`
//! - `GET /api/v2/parts/by-worker/{worker_id}`
//!
//! 以**单个批次**为操作对象的端点不在 part 域：deliver / complete /
//! place-on-shelf / recall-to-pending / split-batch / cancel-batch /
//! release-from-programming / 外协 3 条 / 返修 2 条 / start-repair / pick-up，
//! 见 `crate::modules::prod::batch::handler::lifecycle`（URL 挂
//! `/api/v2/prod/batches/*`）—— 批次级动作按批次做权限与状态机判定更贴合语义，
//! 留在 part 域会让「谁有资格翻状态」这件事跨两个域分裂。
//!
//! WS 广播：commit 之后广播（对齐 Python 延迟广播模式）；事件名与 Python 一致。
//!
//! ## 权限模式
//! - cancel：`require_any_role([Manager, Clerk])` —— 撤销属仓库动作，放开 Clerk
//! - force-complete：`require_role(Manager)` —— 逃生通道（绕状态机强推），明确
//!   不下放 Clerk
//! - 3 条 list 端点：handler 层不设闸门，角色闸门在 service 层
//!   （`require_any_role([Manager, Clerk, Inspector, ShelfAccount])`）；其中
//!   pickable 额外按 `pickable_shelf_scope` 收窄货架范围，另两条不做货架收窄

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
