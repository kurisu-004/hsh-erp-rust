//! work_type 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/work_type.py。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式 `tx.commit()`；
//!   提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//!   装进同一个 `R` 信封。
//! - 权限在 service 层（`current.require_any_role` / `require_role`），此处不重复校验。
//!
//! ## 7 端点
//! 读 3：list_work_types / get_work_type / list_work_type_processes
//! 写 4 (MANAGER)：create_work_type / update_work_type / soft_delete_work_type / set_work_type_processes

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::work_type::dto::{
    SetWorkTypeProcessesRequest, WorkTypeCreateRequest, WorkTypeListOut, WorkTypeListQuery,
    WorkTypeOut, WorkTypeProcessMappingOut, WorkTypeUpdateRequest,
};
use crate::modules::work_type::process_mapping::WorkTypeProcessService;
use crate::modules::work_type::service::WorkTypeService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/work-types
pub async fn list_work_types(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<WorkTypeListQuery>,
) -> Result<Json<R<WorkTypeListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkTypeService::list_work_types(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/work-types → 201
pub async fn create_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkTypeCreateRequest>,
) -> Result<(StatusCode, Json<R<WorkTypeOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkTypeService::create_work_type(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/work-types/{id}
pub async fn get_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkTypeOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkTypeService::get_work_type(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/work-types/{id}/update
pub async fn update_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WorkTypeUpdateRequest>,
) -> Result<Json<R<WorkTypeOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkTypeService::update_work_type(&mut tx, id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/work-types/{id}/soft-delete
pub async fn soft_delete_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    WorkTypeService::soft_delete_work_type(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// GET /api/v2/work-types/{id}/processes
pub async fn list_work_type_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkTypeProcessMappingOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = WorkTypeProcessService::list_work_type_processes(&mut tx, id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/work-types/{id}/processes → 整组替换 mapping
pub async fn set_work_type_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetWorkTypeProcessesRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    WorkTypeProcessService::set_work_type_processes(
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

/// 本域路由表（挂载点 `/api/v2/work-types`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_work_types).post(create_work_type))
        .route("/{id}", get(get_work_type))
        .route("/{id}/update", post(update_work_type))
        .route("/{id}/soft-delete", post(soft_delete_work_type))
        .route(
            "/{id}/processes",
            get(list_work_type_processes).post(set_work_type_processes),
        )
}