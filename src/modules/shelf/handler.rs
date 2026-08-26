//! shelf 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/shelf.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`current.require_any_role` / `require_role`），此处不重复校验。
//!
//! ## 11 端点
//! 读 3：list_shelves / get_shelf / list_shelf_processes
//! picker 3：list_for_return / list_for_inspection / list_all_process_mappings
//! 写 5 (MANAGER)：create_shelf / update_shelf / soft_delete_shelf / set_shelf_processes

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::shelf::dto::{
    AllShelfProcessMappingOut, SetShelfProcessesRequest, ShelfForInspectionOut, ShelfForReturnOut,
    ShelfForReturnQuery, ShelfListOut, ShelfListQuery, ShelfOut, ShelfCreateRequest,
    ShelfProcessMappingOut, ShelfUpdateRequest,
};
use crate::modules::shelf::process_mapping::ShelfProcessService;
use crate::modules::shelf::service::ShelfService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/shelves
pub async fn list_shelves(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ShelfListQuery>,
) -> Result<Json<R<ShelfListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::list_shelves(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves → 201
pub async fn create_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ShelfCreateRequest>,
) -> Result<(StatusCode, Json<R<ShelfOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::create_shelf(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/shelves/{id}
pub async fn get_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ShelfOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::get_shelf(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/update
pub async fn update_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ShelfUpdateRequest>,
) -> Result<Json<R<ShelfOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::update_shelf(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/deactivate
pub async fn deactivate_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ShelfService::soft_delete_shelf(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// GET /api/v2/shelves/for-return?next_process_id=
pub async fn list_for_return(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ShelfForReturnQuery>,
) -> Result<Json<R<ShelfForReturnOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::list_for_return(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/for-inspection
pub async fn list_for_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<ShelfForInspectionOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::list_for_inspection(&mut tx, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/processes
pub async fn list_all_process_mappings(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<AllShelfProcessMappingOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService::list_all_process_mappings(&mut tx, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/{id}/processes
pub async fn list_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ShelfProcessMappingOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfProcessService::list_shelf_processes(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/processes → 整组替换 mapping
pub async fn set_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetShelfProcessesRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ShelfProcessService::set_shelf_processes(
        &mut tx,
        &state.snowflake,
        id,
        &req.items,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/shelves`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_shelves).post(create_shelf))
        .route("/for-return", get(list_for_return))
        .route("/for-inspection", get(list_for_inspection))
        .route("/processes", get(list_all_process_mappings))
        .route("/{id}", get(get_shelf))
        .route("/{id}/update", post(update_shelf))
        .route("/{id}/deactivate", post(deactivate_shelf))
        .route("/{id}/processes", get(list_shelf_processes).post(set_shelf_processes))
}