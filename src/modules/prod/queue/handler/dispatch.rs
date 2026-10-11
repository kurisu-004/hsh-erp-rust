//! prod::queue 下发流 3 端点（`pending` / `dispatch` / `auto-dispatch`）的 HTTP
//! 路由 + 角色守卫 + WS 广播。2026-10-08 自 `prod::batch::handler::dispatch` 搬入
//! （URL 由 `/prod/batches/*` 硬切到 `/prod/queue/*`，无 alias）。
//!
//! 待下发源状态白名单 = `PENDING` / `PROGRAMMING`（已废弃的 `PROGRAMMING` 只存量
//! 兼容，与 `PENDING` 同链路）。
//!
//! - dispatch 统一 bulk-only（单条下发即 `targets.length == 1`）
//! - auto-dispatch 改为只读查询（不开事务、不发 WS 广播）
//! - bulk-dispatch 端点删除（路由层不再挂载）
//!
//! ## 事务边界 + WS 广播
//! - 读端点（pending）：`pool.acquire()` 不开事务
//! - 写端点（dispatch）：`pool.begin()` → service → `tx.commit()` → 发 WS
//!   `BATCH_PLACED_ON_SHELF`（payload = succeeded 列表）
//! - 只读端点（auto-dispatch）：`pool.acquire()` 不开事务，不发 WS
//!
//! ## 权限守卫（2026-10-14 写路径迁权限点；2026-10-15 第二批全量迁点）
//! 守卫在 service 第一行下沉（沿本域 `move_batch` 范本），handler 仅做权限分发：
//! - `dispatch`：`queue.dispatch`（2026-10-14）；
//! - `pending`：`queue.view`（2026-10-15 自角色守卫迁移）；
//! - `auto-dispatch`：`queue.dispatch`（2026-10-15 **有意回改** —— 上批「读端点
//!   保持角色守卫不动」的决策被用户定稿推翻：预览与下发是同一个动作的两步，
//!   守卫集合统一为 M,C，权限点与 `dispatch` 一致）。
//!
//! 逐点口径见 `service/dispatch.rs` 模块头「事务 + 守卫」一节。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use crate::modules::prod::queue::dto::{AutoDispatchRequest, DispatchRequest, ListPendingQuery};
use crate::modules::prod::queue::service::queue::QueueService;
use crate::modules::prod::queue::vo::queue::{
    AutoDispatchResult, DispatchResult, PendingBatchListOut,
};

/// 通用 WS 广播 helper：单 kind + 单 payload 字段（与 queue 同形）。
#[inline]
fn ws_broadcast(state: &AppState, kind: &str, payload: serde_json::Value) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload,
    });
}

/// `GET /api/v2/prod/queue/pending`
///
/// 权限：`queue.view`（service 内守卫；2026-10-15 自角色守卫 M,C,I 迁移，
/// 矩阵逐格相同）。
/// 读端点：`pool.acquire()` 不开事务。
pub async fn list_pending(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<ListPendingQuery>,
) -> Result<Json<R<PendingBatchListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = QueueService::list_pending(&mut conn, &_current, q.limit, q.offset).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/queue/dispatch`
///
/// 权限：`queue.dispatch`（service 内守卫；2026-10-14 自 Manager + Clerk 角色
/// 守卫迁移，矩阵逐格等于原集合）。
///
/// 2026-09-30 重构：入参改为 bulk-only 形态（`DispatchRequest { targets, note? }`）。
/// 单条下发即 `targets.length == 1`；handler 把 targets 解构为 `(batch_id, target_process_id)`
/// 元组列表传给 service。
///
/// 写端点：`pool.begin()` → service → `tx.commit()` → 发 `BATCH_PLACED_ON_SHELF`
/// （payload = succeeded 列表；任一失败走全回滚，response 通过 succeeded/failed 区分）。
pub async fn dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<DispatchRequest>,
) -> Result<Json<R<DispatchResult>>, AppError> {
    let targets: Vec<(i64, i64)> = req
        .targets
        .into_iter()
        .map(|t| (t.batch_id, t.target_process_id))
        .collect();
    let mut tx = state.pool.begin().await?;
    let r = QueueService::dispatch_batch(
        &mut tx,
        targets,
        req.note.as_deref(),
        &state.snowflake,
        &current,
    )
    .await?;
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

/// `POST /api/v2/prod/queue/auto-dispatch`
///
/// 权限：`queue.dispatch`（service 内守卫；2026-10-15 自角色守卫 M,C **迁点**，
/// 集合逐格相同 —— 与 `dispatch` 是同一动作的两步，见文件头「有意回改」）。
///
/// 2026-09-30 重构：只读查询（不开事务、不发 WS 广播）。
/// 返回每个 batch 的「首道工序 + 首货架」+ skip_reason，caller 据此构造
/// `dispatch` 请求 targets 数组。
///
/// 读端点：`pool.acquire()` 不开事务。
/// 空 `batch_ids` → service `AppError::validation`（HTTP 422）。
pub async fn auto_dispatch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AutoDispatchRequest>,
) -> Result<Json<R<AutoDispatchResult>>, AppError> {
    let batch_ids = req.batch_ids.unwrap_or_default();
    let mut conn = state.pool.acquire().await?;
    let r = QueueService::auto_dispatch_preview(&mut conn, &current, batch_ids).await?;
    Ok(Json(R::ok(r)))
}
