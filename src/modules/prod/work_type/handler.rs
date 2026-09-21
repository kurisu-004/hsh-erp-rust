//! work_type 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/work_type.py。
//!
//! ## 约定
//! - 事务边界在 handler（2026-09-22 D-2-simple 重构后）：handler 显式
//!   `state.pool.begin()` / `tx.commit()`，service 仅业务逻辑。
//! - service 形参：`repo: R: WorkTypeRepoTrait`（by-value；胖 trait 已合并
//!   `t_work_type_process` 的 4 个方法）。生产路径 `R = &mut PgConnection`，
//!   trait 已直接 `impl for &mut PgConnection`（见 `repo/mod.rs`）；handler 借
//!   `&mut *tx` 喂给 service。
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
use crate::modules::prod::work_type::dto::{
    SetWorkTypeProcessesRequest, WorkTypeCreateRequest, WorkTypeListOut, WorkTypeListQuery,
    WorkTypeOut, WorkTypeProcessMappingOut, WorkTypeUpdateRequest,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// GET /api/v2/prod/work-types
pub async fn list_work_types(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<WorkTypeListQuery>,
) -> Result<Json<R<WorkTypeListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .work_type_service
        .list_work_types(&mut *tx, &query, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/work-types → 201
pub async fn create_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkTypeCreateRequest>,
) -> Result<(StatusCode, Json<R<WorkTypeOut>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .work_type_service
        .create_work_type(&mut *tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// GET /api/v2/prod/work-types/{id}
pub async fn get_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkTypeOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .work_type_service
        .get_work_type(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/work-types/{id}/update
pub async fn update_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<WorkTypeUpdateRequest>,
) -> Result<Json<R<WorkTypeOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .work_type_service
        .update_work_type(&mut *tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/work-types/{id}/soft-delete
pub async fn soft_delete_work_type(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .work_type_service
        .soft_delete_work_type(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// GET /api/v2/prod/work-types/{id}/processes
pub async fn list_work_type_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<WorkTypeProcessMappingOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .work_type_process_service
        .list_work_type_processes(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/prod/work-types/{id}/processes → 整组替换 mapping
pub async fn set_work_type_processes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetWorkTypeProcessesRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .work_type_process_service
        .set_work_type_processes(&mut *tx, &state.snowflake, id, &req.items, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/prod/work-types`，见 `modules::prod::router`）
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
