//! prod::queue 队列板聚合端点 HTTP handler（2026-10-08 新增）
//!
//! - `GET /api/v2/prod/queue/snapshot` —— 工序序列板
//! - `GET /api/v2/prod/queue/processes/{process_id}` —— 单工序板
//!
//! 两个都是**纯读**：handler `pool.acquire()` 不开事务，不发 WS 广播。
//!
//! ## 角色守卫
//! 下沉到 service（`QueueBoardService::build_*` 入口 `require_any_role`），
//! handler 不重复校验 —— 与 work_type / assembly 域惯例一致。
//!
//! ## 取代关系（**无 alias**，旧路径 404）
//! - `GET /queue/counts` → `/queue/snapshot`（后者多工序元数据 + `pending_count`）
//! - `GET /queue/state?worker_id=` → `/queue/processes/{id}` 的 `workers[].held_batches`
//! - `GET /queue/{process_id}` → `/queue/processes/{id}`

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::queue::board::QueueBoardService;
use crate::modules::prod::queue::vo::board::{QueueBoardSnapshot, QueueProcessBoardDetail};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/prod/queue/snapshot`
///
/// 取代原 `GET /prod/pool/counts`。角色：Manager + Clerk + Inspector（service 内
/// 守卫）。3 条 SQL，与工序数无关。
pub async fn board_snapshot(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<QueueBoardSnapshot>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = QueueBoardService::build_snapshot(&mut conn, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/prod/queue/processes/{process_id}`
///
/// 取代原 `GET /prod/pool/{process_id}` + 逐 worker 的 `GET /prod/pool/state`。
/// 角色：Manager + Clerk + Inspector（service 内守卫）。**固定 6 条 SQL**，
/// 与工人数无关（见 `board/repo.rs::board_process_detail` doc）。
///
/// 工序不存在 / 已软删 → `20801 BIZ_PROCESS_NOT_FOUND`（HTTP 404）。
pub async fn board_process_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(process_id): Path<i64>,
) -> Result<Json<R<QueueProcessBoardDetail>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = QueueBoardService::build_process_detail(&mut conn, &current, process_id).await?;
    Ok(Json(R::ok(out)))
}
