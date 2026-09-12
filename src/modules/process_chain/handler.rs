//! process_chain 域 HTTP handler
//!
//! 对应 Python myERP（无对位 Python 文件；本域为新增）。
//!
//! ## 端点（挂在 `/api/v2/process-chains`，由 `mod.rs::router()` 桥接）
//! - `GET  /by-part/{part_id}` —— 读 part 绑定的工艺链（404 + 20701）
//! - `PUT  /by-part/{part_id}` —— 整组 upsert：OCC + 软删旧 steps + INSERT 新 steps
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`
//! - 权限在 service 层（`current.require_role` / `require_any_role`）
//!
//! 当前 handler 文件 91 行（远低于 250 行硬红线），预留 process_chain 其他子端点（list
//! all / delete / reorder 等）的扩展空间。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::process_chain::dto::{ProcessChainOut, UpsertChainRequest};
use crate::modules::process_chain::service::crud::ProcessChainService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/process-chains/by-part/{part_id}
///
/// 读 part 绑定的工艺链（header + steps）。
/// 无链 → 20701 `BIZ_PROCESS_CHAIN_NOT_FOUND`（HTTP 404）。
pub async fn get_by_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<ProcessChainOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessChainService::get_by_part(&mut tx, part_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// PUT /api/v2/process-chains/by-part/{part_id}
///
/// 整组 upsert：
/// - 无链 → INSERT header + INSERT all steps
/// - 有链 → bump chain version（OCC）→ 软删旧 steps → INSERT 新 steps
///
/// Manager only。完成后回返更新后的整链。
pub async fn upsert(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<UpsertChainRequest>,
) -> Result<Json<R<ProcessChainOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessChainService::upsert_chain(&mut tx, &state.snowflake, part_id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// 本域路由表（挂载点 `/api/v2/process-chains`，见 `mod.rs::router()`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/by-part/{part_id}", get(get_by_part).put(upsert))
}