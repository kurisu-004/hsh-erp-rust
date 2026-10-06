//! prod::queue 召回端点 HTTP handler（`POST /api/v2/prod/queue/recall`）
//!
//! 2026-10-08 自 `prod::batch::handler::lifecycle::recall_to_pending` 搬入。
//!
//! ## 契约变更
//! 去掉 `Path(batch_id)` extractor，`batch_id` 自 `req.batch_id` 取（见
//! [`crate::modules::prod::queue::dto::RecallToPendingRequest`]）。
//!
//! ## 事务 + WS
//! `state.pool.begin()` → service → `tx.commit()` → 广播 `PART_RECALLED`。
//! 广播在 commit 之后（`CLAUDE.md` 架构约定第 6 条）。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::prod::queue::dto::RecallToPendingRequest;
use crate::modules::prod::queue::service::queue::QueueService;
use crate::modules::prod::queue::vo::queue::RecallOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/prod/queue/recall`
///
/// 角色：Manager + Clerk（service 内守卫）。
pub async fn recall(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<RecallToPendingRequest>,
) -> Result<Json<R<RecallOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = QueueService::recall_to_pending(
        &mut *tx,
        &state.snowflake,
        req.batch_id,
        req,
        &current,
    )
    .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_RECALLED".into(),
        payload: json!({ "part_id": out.part_id }),
    });
    Ok(Json(R::ok(out)))
}
