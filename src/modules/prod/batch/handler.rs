//! prod::batch 子模块 handler 层 —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! 2026-09-29 新增：4 个端点全部以 `state.pool.begin()` → service →
//! `tx.commit()` → WS 广播 形态串接（与 worker_pool 范本一致）。
//!
//! ## 端点
//! - `GET  /api/v2/prod/batches/pending`         —— Manager+Clerk+Inspector
//! - `POST /api/v2/prod/batches/dispatch`        —— Manager+Clerk
//! - `POST /api/v2/prod/batches/bulk-dispatch`   —— Manager+Clerk
//! - `POST /api/v2/prod/batches/auto-dispatch`   —— Manager+Clerk
//!
//! ## 事务边界 + WS 广播（2026-09-29 与 worker_pool 范本对齐）
//! - 读端点（pending）：`pool.acquire()` 不开事务
//! - 写端点（dispatch / bulk / auto）：`pool.begin()` → service → commit 后发 WS
//!   `BATCH_PLACED_ON_SHELF`（payload = `DispatchResult`，多条走 `Vec<DispatchResult>`）
//!
//! ## 角色守卫
//! 在 service 第一行下沉（沿 worker_pool 范本），handler 仅做权限分发。
//! 当前 4 个端点的守卫下沉到 service；handler 不重复校验。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{AutoDispatchRequest, BulkDispatchRequest, DispatchRequest, ListPendingQuery};
use super::service::BatchService;
use super::vo::{AutoDispatchResult, BulkDispatchResult, PendingBatchListOut};

/// 通用 WS 广播 helper：单 kind + 单 payload 字段（与 worker_pool 同形）。
#[inline]
fn ws_broadcast(state: &AppState, kind: &str, payload: serde_json::Value) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload,
    });
}

/// `GET /api/v2/prod/batches/pending`
///
/// 角色：Manager + Clerk + Inspector（service 内守卫）。
/// 读端点：`pool.acquire()` 不开事务。
pub async fn list_pending(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<ListPendingQuery>,
) -> Result<Json<R<PendingBatchListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = BatchService::list_pending(&mut conn, &_current, q.limit, q.offset).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/batches/dispatch`
///
/// 角色：Manager + Clerk（service 内守卫）。
/// 写端点：`pool.begin()` → service → commit 后发 `BATCH_PLACED_ON_SHELF`。
pub async fn dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<DispatchRequest>,
) -> Result<Json<R<super::vo::DispatchResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let r = BatchService::dispatch_batch(
        &mut tx,
        req.batch_id,
        req.target_process_id,
        req.note.as_deref(),
        &state.snowflake,
        &current,
    )
    .await?;
    tx.commit().await?;
    ws_broadcast(
        &state,
        "BATCH_PLACED_ON_SHELF",
        json!({
            "batch_id": r.batch_id.to_string(),
            "target_process_id": r.target_process_id.to_string(),
            "shelf_id": r.shelf_id.to_string(),
            "version": r.version,
        }),
    );
    Ok(Json(R::ok(r)))
}

/// `POST /api/v2/prod/batches/bulk-dispatch`
///
/// 角色：Manager + Clerk（service 内守卫）。
/// 写端点：`pool.begin()` → service → commit 后发 `BATCH_PLACED_ON_SHELF`（payload
/// 含 succeeded 数组；任一失败走全回滚 + 抛 AppError 给 caller）。
pub async fn bulk_dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BulkDispatchRequest>,
) -> Result<Json<R<BulkDispatchResult>>, AppError> {
    let targets = req
        .targets
        .into_iter()
        .map(|t| (t.batch_id, t.target_process_id))
        .collect();
    let mut tx = state.pool.begin().await?;
    let r = BatchService::bulk_dispatch(&mut tx, targets, None, &state.snowflake, &current).await?;
    tx.commit().await?;
    if !r.succeeded.is_empty() {
        let payload_ids: Vec<serde_json::Value> = r
            .succeeded
            .iter()
            .map(|d| {
                json!({
                    "batch_id": d.batch_id.to_string(),
                    "target_process_id": d.target_process_id.to_string(),
                    "shelf_id": d.shelf_id.to_string(),
                    "version": d.version,
                })
            })
            .collect();
        ws_broadcast(
            &state,
            "BATCH_PLACED_ON_SHELF",
            json!({ "batches": payload_ids }),
        );
    }
    Ok(Json(R::ok(r)))
}

/// `POST /api/v2/prod/batches/auto-dispatch`
///
/// 角色：Manager + Clerk（service 内守卫）。
/// 写端点：`pool.begin()` → service → commit 后发 `BATCH_PLACED_ON_SHELF`
///（仅 succeeded 列表；skipped 不广播）。
/// 空 `batch_ids` → service `AppError::validation`（HTTP 422）。
pub async fn auto_dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AutoDispatchRequest>,
) -> Result<Json<R<AutoDispatchResult>>, AppError> {
    let batch_ids = req.batch_ids.unwrap_or_default();
    let mut tx = state.pool.begin().await?;
    let r = BatchService::auto_dispatch(&mut tx, batch_ids, &state.snowflake, &current).await?;
    tx.commit().await?;
    if !r.succeeded.is_empty() {
        let payload_ids: Vec<serde_json::Value> = r
            .succeeded
            .iter()
            .map(|d| {
                json!({
                    "batch_id": d.batch_id.to_string(),
                    "target_process_id": d.target_process_id.to_string(),
                    "shelf_id": d.shelf_id.to_string(),
                    "version": d.version,
                })
            })
            .collect();
        ws_broadcast(
            &state,
            "BATCH_PLACED_ON_SHELF",
            json!({ "batches": payload_ids }),
        );
    }
    Ok(Json(R::ok(r)))
}
