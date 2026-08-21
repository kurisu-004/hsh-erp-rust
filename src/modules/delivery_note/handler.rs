//! delivery_note 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/delivery_note.py（设计 §6）。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)`）；handler 这里
//!   只解析 query / path / body。
//!
//! ## Phase 路由（设计 §6 + §6.2）
//! 业务端点统一 `/api/v2/delivery-notes/*`：
//! - `GET    /candidate-parts?customer_id=...`
//! - `GET    /pickup-pending?customer_id=...`
//! - `GET    /`
//! - `POST   /`
//! - `GET    /{id}`
//! - `GET    /{id}/events`
//! - `POST   /{id}/update`
//! - `POST   /{id}/add-parts`
//! - `POST   /{id}/remove-parts`
//! - `POST   /{id}/submit`
//! - `POST   /{id}/recall`
//! - `POST   /{id}/pickup-scan`
//! - `POST   /{id}/pickup`
//! - `POST   /{id}/soft-delete`
//!
//! 打印 `/print` / `/print-labels` 与扫码建单 `/scan` 留到 P3/P4，本期不注册。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::delivery_note::model::DeliveryNoteSortKey;
use crate::modules::delivery_note::repo::SortDir;
use crate::modules::delivery_note::service::DeliveryNoteService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    DeliveryNoteAddPartsRequest, DeliveryNoteCandidatePartsOut, DeliveryNoteCandidatePartsQuery,
    DeliveryNoteCreateRequest, DeliveryNoteListQuery, DeliveryNotePickupPendingQuery,
    DeliveryNotePickupRequest, DeliveryNotePickupScanOut, DeliveryNotePickupScanRequest,
    DeliveryNoteRemovePartsRequest, DeliveryNoteUpdateRequest, DeliveryNoteVersionedRequest,
};

// ===========================================================================
//  业务端点（设计 §6.2，挂在 `/delivery-notes`）
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePath {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub id: i64,
}

/// GET /api/v2/delivery-notes/candidate-parts?customer_id=...
pub async fn list_candidate_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DeliveryNoteCandidatePartsQuery>,
) -> Result<Json<R<DeliveryNoteCandidatePartsOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let items =
        DeliveryNoteService::list_candidate_parts(&mut tx, q.customer_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(DeliveryNoteCandidatePartsOut { items })))
}

/// GET /api/v2/delivery-notes/pickup-pending?customer_id=...
pub async fn list_pickup_pending(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DeliveryNotePickupPendingQuery>,
) -> Result<Json<R<super::dto::DeliveryNotePickupListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let items = DeliveryNoteService::list_for_pickup(&mut tx, q.customer_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(super::dto::DeliveryNotePickupListOut { items })))
}

/// GET /api/v2/delivery-notes
pub async fn list_delivery_notes(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DeliveryNoteListQuery>,
) -> Result<Json<R<super::dto::DeliveryNoteListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;

    // 解析 statuses：query string `?statuses=A,B` → vec!["A","B"]
    let status_vec: Vec<String> = match q.statuses.as_deref() {
        Some(s) => s
            .split(',')
            .map(|x: &str| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None => Vec::new(),
    };
    let sort_by = match q.sort_by.as_deref() {
        Some("SUBMITTED_AT") => DeliveryNoteSortKey::SubmittedAt,
        Some("PICKED_UP_AT") => DeliveryNoteSortKey::PickedUpAt,
        Some("DELIVERY_NOTE_NO") => DeliveryNoteSortKey::DeliveryNoteNo,
        _ => DeliveryNoteSortKey::CreatedAt,
    };
    let sort_dir = match q.sort_dir.as_deref() {
        Some("ASC") => SortDir::Asc,
        _ => SortDir::Desc,
    };
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let status_strs: Vec<&str> = status_vec.iter().map(|s| s.as_str()).collect();
    let out = DeliveryNoteService::list_with_filters(
        &mut tx,
        &status_strs,
        q.customer_id,
        q.keyword.as_deref(),
        sort_by,
        sort_dir,
        limit,
        offset,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes
pub async fn create_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<DeliveryNoteCreateRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::create_draft(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;

    // commit 后广播（设计 §5：commit 之后再 push，避免回滚后误推）
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_CREATED".to_string(),
        payload: serde_json::json!({
            "delivery_note_id": out.head.id,
            "delivery_note_no": out.head.delivery_note_no,
            "customer_id": out.head.customer_id,
        }),
    });

    Ok(Json(R::ok(out)))
}

/// GET /api/v2/delivery-notes/{id}
pub async fn get_delivery_note(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
) -> Result<Json<R<super::dto::DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::get_with_parts(&mut tx, path.id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/delivery-notes/{id}/events
pub async fn list_delivery_note_events(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
) -> Result<Json<R<Vec<super::dto::DeliveryNoteEventOut>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let events = DeliveryNoteService::list_events(&mut tx, path.id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(events)))
}

/// POST /api/v2/delivery-notes/{id}/update
pub async fn update_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteUpdateRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::update(&mut tx, &state.snowflake, path.id, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/add-parts
pub async fn add_delivery_note_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteAddPartsRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::add_parts(
        &mut tx,
        &state.snowflake,
        path.id,
        &req.items,
        req.version,
        &current,
    )
    .await?;
    tx.commit().await?;

    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_PARTS_ADDED".to_string(),
        payload: serde_json::json!({"delivery_note_id": out.head.id}),
    });

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/remove-parts
pub async fn remove_delivery_note_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteRemovePartsRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::remove_parts(&mut tx, path.id, &req.batch_ids, req.version, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/submit
pub async fn submit_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::submit(&mut tx, &state.snowflake, path.id, req.version, &current).await?;
    tx.commit().await?;

    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_SUBMITTED".to_string(),
        payload: serde_json::json!({
            "delivery_note_id": out.id,
            "delivery_note_no": out.delivery_note_no,
        }),
    });

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/recall
pub async fn recall_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::recall(&mut tx, &state.snowflake, path.id, req.version, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/pickup-scan
pub async fn pickup_scan(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNotePickupScanRequest>,
) -> Result<Json<R<DeliveryNotePickupScanOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::pickup_scan(
        &mut tx,
        path.id,
        &req.part_serial,
        req.badge_code.as_deref(),
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/pickup
pub async fn pickup_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNotePickupRequest>,
) -> Result<Json<R<super::dto::DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::pickup(
        &mut tx,
        &state.snowflake,
        path.id,
        req.driver_worker_id,
        req.version,
        req.badge_code.as_deref(),
        &current,
    )
    .await?;
    tx.commit().await?;

    let payload = serde_json::json!({
        "delivery_note_id": out.id,
        "delivery_note_no": out.delivery_note_no,
        "part_count": out.part_count,
        "driver_worker_id": out.driver_worker_id,
    });
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_PICKED_UP".to_string(),
        payload: payload.clone(),
    });
    tracing::info!(?payload, "delivery_note picked up");

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/soft-delete
pub async fn soft_delete_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    DeliveryNoteService::soft_delete(&mut tx, path.id, req.version, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
//  路由表
// ===========================================================================

/// 本域路由表（设计 §6 + §6.2：delivery-notes）。
///
/// axum 静态段优先于参数段；`/candidate-parts`、`/pickup-pending` 必须
/// 在 `/{id}` 之前注册。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ---- delivery-notes/* ----
        .route(
            "/candidate-parts",
            get(list_candidate_parts),
        )
        .route("/pickup-pending", get(list_pickup_pending))
        .route("/", get(list_delivery_notes).post(create_delivery_note))
        .route("/{id}/events", get(list_delivery_note_events))
        .route("/{id}/update", post(update_delivery_note))
        .route("/{id}/add-parts", post(add_delivery_note_parts))
        .route(
            "/{id}/remove-parts",
            post(remove_delivery_note_parts),
        )
        .route("/{id}/submit", post(submit_delivery_note))
        .route("/{id}/recall", post(recall_delivery_note))
        .route("/{id}/pickup-scan", post(pickup_scan))
        .route("/{id}/pickup", post(pickup_delivery_note))
        .route("/{id}/soft-delete", post(soft_delete_delivery_note))
        .route("/{id}", get(get_delivery_note))
}

/// P1 送货分组路由表（独立挂在 `/api/v2/delivery-groups`）。
pub fn p1_router() -> Router<Arc<AppState>> {
    p1_group_router()
}

// ===========================================================================
//  Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_router_compiles() {
        let _ = std::marker::PhantomData::<Arc<AppState>>;
    }
}

// ===========================================================================
//  P1 送货分组 router（保留供 router() nest）
// ===========================================================================

/// P1 送货分组路由子表；前缀 `/delivery-groups` 已由 `router()` nest 上去。
fn p1_group_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(p1_list_delivery_groups).post(p1_create_delivery_group))
        .route("/{id}/update", post(p1_update_delivery_group))
        .route("/{id}/soft-delete", post(p1_soft_delete_delivery_group))
}

// ===========================================================================
//  P1 handler thin wrappers（直接复用 P1 handler 函数）
// ===========================================================================

use crate::modules::delivery_note::service::DeliveryGroupService;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeliveryGroupListQuery {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
}

async fn p1_list_delivery_groups(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<DeliveryGroupListQuery>,
) -> Result<Json<R<super::dto::DeliveryGroupListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::list_for_l1(&mut tx, q.customer_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

async fn p1_create_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<super::dto::CreateDeliveryGroupRequest>,
) -> Result<Json<R<super::dto::DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::create(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

async fn p1_update_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<super::dto::UpdateDeliveryGroupRequest>,
) -> Result<Json<R<super::dto::DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryGroupService::update(&mut tx, &state.snowflake, id, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

async fn p1_soft_delete_delivery_group(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<super::dto::DeliveryGroupIdRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    DeliveryGroupService::soft_delete(&mut tx, id, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}