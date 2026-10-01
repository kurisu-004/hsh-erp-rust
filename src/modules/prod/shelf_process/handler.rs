//! prod::shelf_process 子模块 HTTP handler
//!
//! ## 事务边界
//! 与其余 20 个 handler 文件一致，事务边界在 handler：
//! - ① **纯写端点**（`set_shelf_processes`）：`pool.begin()` → service →
//!   `tx.commit()`，错误路径 tx drop 隐式回滚
//! - ② **读端点**（`list_all_mappings` / `list_shelf_processes`）：`pool.acquire()`
//!   不开事务，service 借 `&mut *conn` 执行查询，用完即 drop
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role` / `require_role`），handler
//! 不重复校验。
//!
//! ## 3 端点（2026-10-02 自 shelf 域硬切，旧路径无 alias）
//! 读 2：`GET /api/v2/prod/shelf-processes`（全集）/
//!        `GET /api/v2/prod/shelf-processes/{shelf_id}`（单架）
//! 写 1 (MANAGER)：`POST /api/v2/prod/shelf-processes/{shelf_id}`（整组替换）
//!
//! 旧路径（404，不再挂载）：`GET /api/v2/shelves/processes` /
//! `GET /api/v2/shelves/{id}/processes` / `POST /api/v2/shelves/{id}/processes`。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::shelf_process::dto::SetShelfProcessesRequest;
use crate::modules::prod::shelf_process::service::ShelfProcessService;
use crate::modules::prod::shelf_process::vo::{AllShelfProcessMappingOut, ShelfProcessMappingOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/prod/shelf-processes —— 读端点，acquire 不开事务
pub async fn list_all_mappings(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<AllShelfProcessMappingOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfProcessService
        .list_all_mappings(&mut conn, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/prod/shelf-processes/{shelf_id} —— 读端点，acquire 不开事务
pub async fn list_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(shelf_id): Path<i64>,
) -> Result<Json<R<ShelfProcessMappingOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfProcessService
        .list_shelf_processes(&mut conn, shelf_id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/shelf-processes/{shelf_id} → 整组替换 mapping —— 纯写端点
pub async fn set_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(shelf_id): Path<i64>,
    Json(req): Json<SetShelfProcessesRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ShelfProcessService
        .set_shelf_processes(&mut tx, &state.snowflake, shelf_id, &req.items, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/prod/shelf-processes`，见 `modules::prod::router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/", get(list_all_mappings)).route(
        "/{shelf_id}",
        get(list_shelf_processes).post(set_shelf_processes),
    )
}
