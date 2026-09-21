//! shelf 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/shelf.py。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`，按读写分三形态：
//! - ① **纯写端点**（create_shelf / update_shelf / deactivate_shelf /
//!   set_shelf_processes）：`pool.begin()` → service call → `tx.commit()`，
//!   错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：shelf 域当前无 Redis / WS 副作用需求，故
//!   全部写端点走形态 ①。
//! - ③ **读端点**（list_shelves / get_shelf / list_for_return / list_for_inspection /
//!   list_all_process_mappings / list_shelf_processes）：`pool.acquire()` 不开
//!   事务，service 借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：`repo: R: ShelfRepoTrait`（by-value）。生产路径
//! `R = &mut PgConnection`，trait `ShelfRepoTrait` 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs`）。service 方法**不**收第二个 conn 参数——跨域
//! `ProcessRepo` 静态调用由 trait helper（`proc_check_process_exists` /
//! `proc_list_existing_process_ids`）封装。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role` / `require_role`），handler
//! 不重复校验。
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
    AllShelfProcessMappingOut, SetShelfProcessesRequest, ShelfCreateRequest, ShelfForInspectionOut,
    ShelfForReturnOut, ShelfForReturnQuery, ShelfListOut, ShelfListQuery, ShelfOut,
    ShelfProcessMappingOut, ShelfUpdateRequest,
};
use crate::modules::shelf::process_mapping::ShelfProcessService;
use crate::modules::shelf::service::ShelfService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/shelves —— 读端点，acquire 不开事务
pub async fn list_shelves(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ShelfListQuery>,
) -> Result<Json<R<ShelfListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfService
        .list_shelves(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves → 201 —— 纯写端点
pub async fn create_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ShelfCreateRequest>,
) -> Result<(StatusCode, Json<R<ShelfOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService
        .create_shelf(&mut *tx, &state.snowflake, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/shelves/{id} —— 读端点，acquire 不开事务
pub async fn get_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ShelfOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfService.get_shelf(&mut *conn, id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/update —— 纯写端点
pub async fn update_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ShelfUpdateRequest>,
) -> Result<Json<R<ShelfOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ShelfService
        .update_shelf(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/deactivate —— 纯写端点
pub async fn deactivate_shelf(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ShelfService.soft_delete_shelf(&mut *tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// GET /api/v2/shelves/for-return?next_process_id= —— 读端点，acquire 不开事务
pub async fn list_for_return(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ShelfForReturnQuery>,
) -> Result<Json<R<ShelfForReturnOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfService
        .list_for_return(&mut *conn, &query, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/for-inspection —— 读端点，acquire 不开事务
pub async fn list_for_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<ShelfForInspectionOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfService
        .list_for_inspection(&mut *conn, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/processes —— 读端点，acquire 不开事务
pub async fn list_all_process_mappings(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<AllShelfProcessMappingOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfService
        .list_all_process_mappings(&mut *conn, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/shelves/{id}/processes —— 读端点，acquire 不开事务
pub async fn list_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<ShelfProcessMappingOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ShelfProcessService
        .list_shelf_processes(&mut *conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/shelves/{id}/processes → 整组替换 mapping —— 纯写端点
pub async fn set_shelf_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetShelfProcessesRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    ShelfProcessService
        .set_shelf_processes(&mut *tx, &state.snowflake, id, &req.items, &current)
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
        .route(
            "/{id}/processes",
            get(list_shelf_processes).post(set_shelf_processes),
        )
}