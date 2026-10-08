//! shelf 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/shelf.py。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! handler 负责 `pool.begin()` / `tx.commit()`，按读写分三形态：
//! - ① **纯写端点**（create_shelf / update_shelf / deactivate_shelf）：
//!   `pool.begin()` → service call → `tx.commit()`，错误路径 tx drop 隐式回滚。
//! - ② **写 + post-commit 副作用**：shelf 域当前无 Redis / WS 副作用需求，故
//!   全部写端点走形态 ①。
//! - ③ **读端点**（list_shelves / get_shelf）：`pool.acquire()` 不开事务，service
//!   借 `&mut *conn` 执行查询，用完即 drop。
//!
//! service 形参：`repo: R: ShelfRepoTrait`（by-value）。生产路径
//! `R = &mut PgConnection`，trait `ShelfRepoTrait` 已直接对 `&mut PgConnection`
//! 实现（见 `repo/mod.rs`）。service 方法**不**收第二个 conn 参数——service 内已无
//! 跨域调用（`list_for_return` 的 `next_process_id` 占位校验 2026-10-02 删除）。
//!
//! ## 统一响应信封
//! handler 返回 `Result<Json<R<T>>, AppError>`，错误由 `AppError::into_response()`
//! 装进同一个 `R` 信封。
//!
//! ## 权限
//! 权限守卫在 service 层（`current.require_any_role` / `require_role`），handler
//! 不重复校验。
//!
//! ## 5 端点（2026-10-10：picker 两条端点下线）
//! 读 2：list_shelves / get_shelf
//! 写 3 (MANAGER)：create_shelf / update_shelf / soft_delete_shelf
//!
//! - `GET /api/v2/shelves/for-return` 与 `GET /api/v2/shelves/for-inspection` 于
//!   2026-10-10 **下线（404，无 alias）**：货架改由服务端按负载自动选
//!   （`shared::shelf::select::pick_least_loaded`），前端不再需要「挑一个架」这个
//!   动作。移除记录见 `docs/api/shelves.md`。
//! - `GET|POST /api/v2/shelves/{id}/processes` 与 `GET /api/v2/shelves/processes`
//!   已于 2026-10-02 删除（404，无 alias），新路径见
//!   `src/modules/prod/shelf_process/mod.rs`。
//!
//! ## 路由注册顺序
//! `/{id}` 是 catch-all，必须**排在**它的静态兄弟段之后 —— axum 的 matchit 0.8
//! 对 `/{id}` 与 `/for-return` 这类同层段按注册顺序消歧，先注册 catch-all 会把静态段
//! 吃掉。现有两条 `/{id}/update` / `/{id}/deactivate` 是**两段**路径，与 `/{id}` 不
//! 同形，故不受影响。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::shelf::dto::{ShelfCreateRequest, ShelfListQuery, ShelfUpdateRequest};
use crate::modules::shelf::service::ShelfService;
use crate::modules::shelf::vo::{ShelfListOut, ShelfOut};
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
    ShelfService
        .soft_delete_shelf(&mut *tx, id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(())))
}

/// 本域路由表（挂载点 `/api/v2/shelves`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_shelves).post(create_shelf))
        .route("/{id}", get(get_shelf))
        .route("/{id}/update", post(update_shelf))
        .route("/{id}/deactivate", post(deactivate_shelf))
}
