//! process_chain 域 HTTP handler
//!
//! 对应 Python myERP（无对位 Python 文件；本域为新增）。
//!
//! ## 端点（挂在 `/api/v2/process-chains`，由 `mod.rs::router()` 桥接）
//! - `GET  /by-part/{part_id}` —— 读 part 绑定的工艺链（404 + 20701）
//! - `PUT  /by-part/{part_id}` —— 整组 upsert：OCC + 软删旧 steps + INSERT 新 steps
//! - `GET  /{chain_id}` —— 按链 id 读工艺链（2026-09-16 FK 翻转新增；404 + 20701）
//!
//! 路由顺序说明：axum 静态段 `by-part` 优先于参数段 `{chain_id}`，
//! `/by-part/123` 不会被解析成 chain_id。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`
//! - 权限在 service 层（`current.require_role` / `require_any_role`）

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::process_chain::dto::{ProcessChainOut, UpsertChainRequest};
use crate::modules::prod::process_chain::service::crud::ProcessChainService;
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

/// GET /api/v2/process-chains/{chain_id}
///
/// 按链 id 读工艺链（header + steps）。2026-09-16 FK 翻转新增：
/// 前端在「工序制定」页点击零件后，按 `part.process_chain_id` 调本端点。
/// 无链 / 已软删 → 20701 `BIZ_PROCESS_CHAIN_NOT_FOUND`（HTTP 404）。
pub async fn get_by_id(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(chain_id): Path<i64>,
) -> Result<Json<R<ProcessChainOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ProcessChainService::get_chain_by_id(&mut tx, chain_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// PUT /api/v2/process-chains/by-part/{part_id}
///
/// 整组 upsert：
/// - 无链 → INSERT header + link 到 part + INSERT all steps
/// - 有链 → bump chain version（OCC）→ 软删旧 steps → INSERT 新 steps
/// - 守卫：part 不存在 → 20101；part.status 非 PENDING → 20705（2026-09-16 新增）
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
