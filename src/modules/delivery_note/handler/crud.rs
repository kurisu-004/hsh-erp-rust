//! delivery_note 域基础 CRUD handler
//!
//! 范围：list / get / create / update / add-parts / remove-parts / soft-delete /
//! batch-detail / candidate-parts / pickup-pending / events + P1 送货分组 CRUD。
//!
//! 状态机转换走 `lifecycle.rs`；扫码入单走 `scan.rs`；打印走 `print.rs`。
//!
//! ## 约定（2026-09-22 D-5 + review 第 1 轮）
//! - 事务边界在 handler：`state.pool.begin()` → 借 `&mut *tx` 喂给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - **service 形参 by-value trait**（iam 严格范本）：handler 借 `&mut *tx` 给
//!   `state.delivery_note_service.xxx(&mut *tx, ...)` 或 `&mut *conn` 给读端点。
//! - **handler 三形态**：
//!   - ① 纯写端点 `pool.begin() → service → commit`；
//!   - ② 写 + post-commit Redis / WS（broadcast 落 handler，service 不持有 WsHub）`pool.begin() → service → commit → state.ws_hub.broadcast(...)`；
//!   - ③ 读端点（list_*/get_*）`pool.acquire() → service`，不开事务。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)`）；handler 这里只解析
//!   query / path / body。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};

use serde::Deserialize;

use crate::modules::delivery_note::dto::{
    CreateDeliveryGroupRequest, DeliveryGroupIdRequest, DeliveryNoteAddPartsRequest,
    DeliveryNoteBatchDetailQuery, DeliveryNoteCandidatePartsQuery, DeliveryNoteCreateRequest,
    DeliveryNoteListQuery, DeliveryNotePath, DeliveryNotePickupPendingQuery,
    DeliveryNoteRemovePartsRequest, DeliveryNoteUpdateRequest, DeliveryNoteVersionedRequest,
    UpdateDeliveryGroupRequest,
};
use crate::modules::delivery_note::model::DeliveryNoteSortKey;
use crate::modules::delivery_note::repo::SortDir;
use crate::modules::delivery_note::vo::{
    BatchDeliveryDetailData, DeliveryGroupListOut, DeliveryGroupOut, DeliveryNoteCandidatePartsOut,
    DeliveryNoteDetailOut, DeliveryNoteEventOut, DeliveryNoteListOut, DeliveryNoteOut,
    DeliveryNotePickupListOut,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

const BATCH_DETAIL_MAX_IDS: usize = 200;

/// `GET /api/v2/delivery-notes/batch-detail?ids=1,2,3`
///
/// 入参 `ids` 是逗号分隔字符串；空 / 越界 / 重复（保留首次出现顺序）/ 非 i64
/// 都会被规范化或拒为 `BIZ_INVALID_VALUE`（20104）。缺失的 id 静默跳过（按
/// 入参顺序返回存在的那部分）。
pub async fn batch_get_delivery_notes(
    State(state): State<Arc<AppState>>,
    _current: crate::auth::rbac::CurrentUser,
    Query(q): Query<DeliveryNoteBatchDetailQuery>,
) -> Result<Json<R<BatchDeliveryDetailData>>, AppError> {
    // 解析：split + trim + filter empty + 保留首次出现顺序 dedupe
    let raw = q.ids.as_deref().unwrap_or("");
    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<i64> = Vec::new();
    for tok in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let n: i64 = tok.parse().map_err(|_| {
            AppError::biz(
                crate::shared::error::code::BIZ_INVALID_VALUE,
                "ids contains non-integer token",
            )
        })?;
        if seen.insert(n) {
            ids.push(n);
        }
    }
    if ids.is_empty() {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            "ids must contain 1..=200 items",
        ));
    }
    if ids.len() > BATCH_DETAIL_MAX_IDS {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            format!(
                "ids length exceeds {} (got {})",
                BATCH_DETAIL_MAX_IDS,
                ids.len()
            ),
        ));
    }

    // 读端点：pool.acquire() → service → drop。不开事务（与 iam me/list_users 同形）。
    let mut conn = state.pool.acquire().await?;
    let items = state
        .delivery_note_service
        .get_many_with_parts(&mut *conn, &ids)
        .await?;
    Ok(Json(R::ok(BatchDeliveryDetailData { items })))
}

/// GET /api/v2/delivery-notes/candidate-parts?customer_id=...
pub async fn list_candidate_parts(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Query(q): Query<DeliveryNoteCandidatePartsQuery>,
) -> Result<Json<R<DeliveryNoteCandidatePartsOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。
    let mut conn = state.pool.acquire().await?;
    let items = state
        .delivery_note_service
        .list_candidate_parts(&mut *conn, q.customer_id, &current)
        .await?;
    Ok(Json(R::ok(DeliveryNoteCandidatePartsOut { items })))
}

/// GET /api/v2/delivery-notes/pickup-pending?customer_id=...
pub async fn list_pickup_pending(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Query(q): Query<DeliveryNotePickupPendingQuery>,
) -> Result<Json<R<DeliveryNotePickupListOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。
    let mut conn = state.pool.acquire().await?;
    let items = state
        .delivery_note_service
        .list_for_pickup(&mut *conn, q.customer_id, &current)
        .await?;
    Ok(Json(R::ok(DeliveryNotePickupListOut { items })))
}

/// GET /api/v2/delivery-notes
pub async fn list_delivery_notes(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Query(q): Query<DeliveryNoteListQuery>,
) -> Result<Json<R<DeliveryNoteListOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。

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

    let mut conn = state.pool.acquire().await?;
    let out = state
        .delivery_note_service
        .list_with_filters(
            &mut *conn,
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
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes
pub async fn create_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Json(req): Json<DeliveryNoteCreateRequest>,
) -> Result<Json<R<DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .create_draft(&mut *tx, req, &current)
        .await?;
    tx.commit().await?;

    // commit 后广播（设计 §5：commit 之后再 push，避免回滚后误推）
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
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
    _current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
) -> Result<Json<R<DeliveryNoteDetailOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。
    let mut conn = state.pool.acquire().await?;
    let out = state
        .delivery_note_service
        .get_with_parts(&mut *conn, path.id)
        .await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/delivery-notes/{id}/events
pub async fn list_delivery_note_events(
    State(state): State<Arc<AppState>>,
    _current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
) -> Result<Json<R<Vec<DeliveryNoteEventOut>>>, AppError> {
    // 读端点：pool.acquire() → service → drop。
    let mut conn = state.pool.acquire().await?;
    let events = state
        .delivery_note_service
        .list_events(&mut *conn, path.id)
        .await?;
    Ok(Json(R::ok(events)))
}

/// POST /api/v2/delivery-notes/{id}/update
pub async fn update_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteUpdateRequest>,
) -> Result<Json<R<DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .update(&mut *tx, path.id, req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/add-parts
pub async fn add_delivery_note_parts(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteAddPartsRequest>,
) -> Result<Json<R<DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .add_parts(&mut *tx, path.id, &req.items, req.version, &current)
        .await?;
    tx.commit().await?;

    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_PARTS_ADDED".to_string(),
            payload: serde_json::json!({"delivery_note_id": out.head.id}),
        });

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/remove-parts
pub async fn remove_delivery_note_parts(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteRemovePartsRequest>,
) -> Result<Json<R<DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .remove_parts(&mut *tx, path.id, &req.batch_ids, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{id}/soft-delete
pub async fn soft_delete_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .delivery_note_service
        .soft_delete(&mut *tx, path.id, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}

// ===========================================================================
//  P1 handler thin wrappers（直接复用 P1 handler 函数）
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryGroupListQuery {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
}

pub(super) async fn p1_list_delivery_groups(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Query(q): Query<DeliveryGroupListQuery>,
) -> Result<Json<R<DeliveryGroupListOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。
    let mut conn = state.pool.acquire().await?;
    let out = state
        .delivery_group_service
        .list_for_l1(&mut *conn, q.customer_id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

pub(super) async fn p1_create_delivery_group(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Json(req): Json<CreateDeliveryGroupRequest>,
) -> Result<Json<R<DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_group_service
        .create(&mut *tx, req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

pub(super) async fn p1_update_delivery_group(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateDeliveryGroupRequest>,
) -> Result<Json<R<DeliveryGroupOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_group_service
        .update(&mut *tx, id, req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

pub(super) async fn p1_soft_delete_delivery_group(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<DeliveryGroupIdRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    state
        .delivery_group_service
        .soft_delete(&mut *tx, id, req, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok_empty()))
}
