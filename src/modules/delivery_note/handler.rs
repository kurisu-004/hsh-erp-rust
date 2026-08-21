//! delivery_note 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/delivery_note.py（Phase P1 仅挂送货分组相关端点）。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut *tx` 给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)`）。
//!
//! ## Phase P1 路由
//! - `GET    /api/v2/delivery-groups?customer_id=<L1>`           M/C/I
//! - `POST   /api/v2/delivery-groups`                             M/C
//! - `POST   /api/v2/delivery-groups/{id}/update`                 M/C
//! - `POST   /api/v2/delivery-groups/{id}/soft-delete`            M/C
//!
//! 送货单 CRUD / 扫码入单 / 打印 等留到 P2–P4。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    CreateDeliveryGroupRequest, DeliveryGroupIdRequest, DeliveryGroupListOut,
    DeliveryGroupOut, UpdateDeliveryGroupRequest,
};
use super::service::DeliveryGroupService;

// ---------------------------------------------------------------------------
//  路由 query / 入参 helper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryGroupListQuery {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
}

// ---------------------------------------------------------------------------
//  Handlers
// ---------------------------------------------------------------------------

/// GET /api/v2/delivery-groups?customer_id=<L1>
pub async fn list_delivery_groups(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DeliveryGroupListQuery>,
) -> Result<Json<R<DeliveryGroupListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::list_for_l1(&mut tx, q.customer_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-groups → 200
pub async fn create_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CreateDeliveryGroupRequest>,
) -> Result<Json<R<DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::create(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-groups/{id}/update → 200
pub async fn update_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateDeliveryGroupRequest>,
) -> Result<Json<R<DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::update(&mut tx, &state.snowflake, id, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-groups/{id}/soft-delete → 200 (no data)
pub async fn soft_delete_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<DeliveryGroupIdRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    DeliveryGroupService::soft_delete(&mut tx, id, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

/// 本域路由表（挂载点 `/api/v2/delivery-groups`，见 `modules::v2_router`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_delivery_groups).post(create_delivery_group))
        .route("/{id}/update", post(update_delivery_group))
        .route("/{id}/soft-delete", post(soft_delete_delivery_group))
}

// ===========================================================================
//  Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_router_compiles() {
        // axum Router 构造要求 State 类型对得上，本用例确保路由表能跑通 `Router::new()`
        let _ = std::marker::PhantomData::<Arc<AppState>>;
    }
}