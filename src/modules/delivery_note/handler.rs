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
//! - `POST   /scan`                           ← Phase P3 扫码建单（设计 §5）
//! - `GET    /candidate-parts?customer_id=...`
//! - `GET    /pickup-pending?customer_id=...`
//! - `GET    /`
//! - `POST   /`
//! - `GET    /{id}`
//! - `GET    /{id}/events`
//! - `POST   /{id}/update`
//! - `POST   /{id}/add-parts`
//! - `POST   /{id}/remove-parts`
//! - `POST   /{id}/attach-batches`            ← Phase P3+ 弹窗批量 attach A 组
//! - `POST   /{id}/submit`
//! - `POST   /{id}/recall`
//! - `POST   /{id}/pickup-scan`
//! - `POST   /{id}/pickup`
//! - `POST   /{id}/soft-delete`
//!
//! 打印 `/print` / `/print-labels` 留到 P4，本期不注册。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use serde::Deserialize;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::delivery_note::model::DeliveryNoteSortKey;
use crate::modules::delivery_note::repo::SortDir;
use crate::modules::delivery_note::service::DeliveryNoteService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    AttachBatchesOut, AttachBatchesRequest, BatchDeliveryDetailData,
    DeliveryNoteAddPartsRequest, DeliveryNoteBatchDetailQuery, DeliveryNoteCandidatePartsOut,
    DeliveryNoteCandidatePartsQuery, DeliveryNoteCreateRequest, DeliveryNoteListQuery,
    DeliveryNotePickupPendingQuery, DeliveryNotePickupRequest, DeliveryNotePickupScanOut,
    DeliveryNotePickupScanRequest, DeliveryNoteRemovePartsRequest, DeliveryNoteUpdateRequest,
    DeliveryNoteVersionedRequest, PrintDeliveryNoteRequest, PrintLabelsRequest, ScanDeliveryOut,
    ScanDeliveryRequest, SubmitDeliveryOut,
};

// ===========================================================================
//  业务端点（设计 §6.2，挂在 `/delivery-notes`）
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryNotePath {
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub id: i64,
}

const BATCH_DETAIL_MAX_IDS: usize = 200;

/// `GET /api/v2/delivery-notes/batch-detail?ids=1,2,3`
///
/// 入参 `ids` 是逗号分隔字符串；空 / 越界 / 重复（保留首次出现顺序）/ 非 i64
/// 都会被规范化或拒为 `BIZ_INVALID_VALUE`（20104）。缺失的 id 静默跳过（按
/// 入参顺序返回存在的那部分）。
pub async fn batch_get_delivery_notes(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
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

    let mut tx = state.pool.begin().await?;
    let items = DeliveryNoteService::get_many_with_parts(&mut tx, &ids).await?;
    tx.commit().await?;
    Ok(Json(R::ok(BatchDeliveryDetailData { items })))
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
///
/// 出参 `SubmitDeliveryOut` 含两种 outcome，前端据此分支：
/// - `outcome = SUBMITTED`：`note` 为提交后的送货单投影；状态机 DRAFT → SUBMITTED 已发生；
///   本次提交会发出 `DELIVERY_NOTE_SUBMITTED` 大屏事件。
/// - `outcome = CANDIDATES_AVAILABLE`：存在仍在 `INSPECTION` 的已挂单批次，**本次未提交**；
///   `note` 为 `null`；`unresolved_targets` 按 part 分组列出未过检批次（含 `version`，
///   前端可一键转发到 `POST /parts/batch-to-ship` 让其到 READY_TO_SHIP 后再重提本接口）。
///   候选分支不写库、不发事件。
pub async fn submit_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<SubmitDeliveryOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::submit(&mut tx, &state.snowflake, path.id, req.version, &current).await?;
    tx.commit().await?;

    // 仅真正提交时广播；候选分支未写库，不发事件
    if let Some(note) = out.note.as_ref() {
        state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_SUBMITTED".to_string(),
            payload: serde_json::json!({
                "delivery_note_id": note.id,
                "delivery_note_no": note.delivery_note_no,
            }),
        });
    }

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

/// POST /api/v2/delivery-notes/scan  （设计 §5；P3）
///
/// 扫码入单：trim → 解析（part → assembly）→ 分类 → find-or-create 草稿 → 批次
/// 评估 → 写 `delivery_note_id`（整个流程在事务内）。commit 后广播一次大屏事件
/// `DELIVERY_NOTE_SCAN_ADD`（轻量级 high-frequency）。
///
/// 角色：M / C / I（与 Python `pickup_scan` 对应，但 Python 仅 I；这里放宽允许
/// MANAGER/CLERK 调试用，与 `create_draft` 一致）。
pub async fn scan_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ScanDeliveryRequest>,
) -> Result<Json<R<ScanDeliveryOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out =
        DeliveryNoteService::scan_add(&mut tx, &state.snowflake, &req.code, &current).await?;
    tx.commit().await?;

    let added_count = out.added_batches.len();
    let note_id = out.note.id;
    let note_no = out.note.delivery_note_no.clone();
    let unresolved_count = out.unresolved_targets.as_ref().map(|v| v.len()).unwrap_or(0);
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_SCAN_ADD".to_string(),
        payload: serde_json::json!({
            "delivery_note_id": note_id,
            "delivery_note_no": note_no,
            "added_count": added_count,
            "unresolved_count": unresolved_count,
            "line_count": out.note.line_count,
            "resolved_kind": match out.resolved.kind {
                super::dto::ResolvedKindDto::Part => "PART",
                super::dto::ResolvedKindDto::Assembly => "ASSEMBLY",
            },
            "outcome": match out.outcome {
                super::dto::ScanOutcomeDto::Added => "ADDED",
                super::dto::ScanOutcomeDto::AlreadyPresent => "ALREADY_PRESENT",
                super::dto::ScanOutcomeDto::CandidatesAvailable => "CANDIDATES_AVAILABLE",
                super::dto::ScanOutcomeDto::PartialAdded => "PARTIAL_ADDED",
            },
        }),
    });

    Ok(Json(R::ok(out)))
}

/// POST /api/v2/delivery-notes/{note_id}/attach-batches
///
/// 弹窗提交时调用，把 A 组（INSPECTION / READY_TO_SHIP）批次 attach 到指定 DRAFT 送货单。
/// 部分失败（OCC / 状态非法 / 重复）→ 200 + conflicts 列表。
/// note 非 DRAFT → 409 `BIZ_DELIVERY_NOTE_NOT_DRAFT`（HTTP 409 由 biz_with_status 强制）。
///
/// RBAC：Manager / Clerk（**比 add_parts 更严格**：本端点只在 DRAFT 草稿做显式
/// attach，不走扫码 / 工人路径，故不放宽到 Inspector）。
pub async fn attach_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<AttachBatchesRequest>,
) -> Result<Json<R<AttachBatchesOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])?;

    // 批量上限：单事务内对每个 item 至少 2 次 DB 调用（get_by_id + attach_to_note），
    // 上限 200 防恶意请求长期持有连接。参考既有 batch-detail 的 BATCH_DETAIL_MAX_IDS 风格。
    const ATTACH_BATCHES_MAX_ITEMS: usize = 200;
    if req.batches.len() > ATTACH_BATCHES_MAX_ITEMS {
        return Err(AppError::biz(
            crate::shared::error::code::BIZ_INVALID_VALUE,
            format!(
                "too many batches: {} (max {})",
                req.batches.len(),
                ATTACH_BATCHES_MAX_ITEMS
            ),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let out = DeliveryNoteService::attach_batches(&mut tx, path.id, req.batches, &current).await?;
    tx.commit().await?;

    // 提交成功后广播（部分成功也广播，但 frontend 可用 conflicts 长度判断是否需要回滚 UI）
    let payload = serde_json::json!({
        "delivery_note_id": path.id,
        "attached_count": out.attached,
        "conflict_count": out.conflicts.len(),
    });
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_BATCHES_ATTACHED".to_string(),
        payload,
    });

    Ok(Json(R::ok(out)))
}

// ===========================================================================
//  P4 打印端点（设计 §6.2 + §8）
// ===========================================================================

/// POST /api/v2/delivery-notes/{id}/print  （设计 §8，P4）
///
/// 渲染送货单 → xlsx bytes；CPU 密集 umya 渲染走 `tokio::task::spawn_blocking`。
/// 角色：M / C / I（与 Python `print_note` 对齐）。
pub async fn print_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<PrintDeliveryNoteRequest>,
) -> Result<axum::response::Response, AppError> {
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, CACHE_CONTROL};
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

    let custom_order = parse_i64_opt(req.custom_order.as_ref(), "custom_order")?;
    let merge_quantities = parse_i64_map_opt(req.merge_quantities.as_ref(), "merge_quantities")?;

    let bytes_prefix = DeliveryNoteService::print_xlsx(
        &state.pool,
        path.id,
        custom_order,
        req.merge_assemblies.unwrap_or(false),
        merge_quantities,
        None,
        &state.config.delivery_note_template_dir,
        &current,
    )
    .await?;
    let (bytes, _prefix) = bytes_prefix;

    let filename = format!(
        "F-{}-note.xlsx",
        chrono::Local::now().format("%Y-%m-%d")
    );
    let len = bytes.len();
    let resp = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(CONTENT_TYPE, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(CONTENT_LENGTH, len.to_string())
        .header(CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError::internal(format!("build print response: {e}")))?;

    // 渲染成功后广播（轻量：只推单据级事件，不按行推送）
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_PRINTED".to_string(),
        payload: serde_json::json!({
            "delivery_note_id": path.id,
            "kind": "note",
        }),
    });

    Ok(resp)
}

/// POST /api/v2/delivery-notes/{id}/print-labels  （设计 §8，P4）
///
/// 标签渲染（不走模板，直接 `openpyxl.Workbook` 等价）
pub async fn print_labels(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<PrintLabelsRequest>,
) -> Result<axum::response::Response, AppError> {
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, CACHE_CONTROL};
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

    let custom_order = parse_i64_opt(req.custom_order.as_ref(), "custom_order")?;
    let merge_quantities = parse_i64_map_opt(req.merge_quantities.as_ref(), "merge_quantities")?;
    let line_item_ids = parse_i64_opt(req.line_item_ids.as_ref(), "line_item_ids")?;

    let bytes_prefix = DeliveryNoteService::print_xlsx(
        &state.pool,
        path.id,
        custom_order,
        req.merge_assemblies.unwrap_or(true), // labels 默认 true（与 Python 一致）
        merge_quantities,
        line_item_ids,
        &state.config.delivery_note_template_dir,
        &current,
    )
    .await?;
    let (bytes, _prefix) = bytes_prefix;

    let filename = format!(
        "F-{}-labels.xlsx",
        chrono::Local::now().format("%Y-%m-%d")
    );
    let len = bytes.len();
    let resp = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(CONTENT_TYPE, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(CONTENT_LENGTH, len.to_string())
        .header(CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError::internal(format!("build labels response: {e}")))?;

    // 渲染成功后广播（轻量：只推单据级事件，不按行推送）
    state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
        kind: "DELIVERY_NOTE_PRINTED".to_string(),
        payload: serde_json::json!({
            "delivery_note_id": path.id,
            "kind": "label",
        }),
    });

    Ok(resp)
}

// 解析 JSON 字符串键的 i64 / HashMap
fn parse_i64_opt(field: Option<&Vec<String>>, name: &str) -> Result<Option<Vec<i64>>, AppError> {
    match field {
        None => Ok(None),
        Some(v) => {
            let mut out = Vec::with_capacity(v.len());
            for s in v {
                let n: i64 = s.parse().map_err(|_| {
                    AppError::biz(
                        crate::shared::error::code::BIZ_INVALID_VALUE,
                        format!("{name} contains non-integer id: {s:?}"),
                    )
                })?;
                out.push(n);
            }
            Ok(Some(out))
        }
    }
}

fn parse_i64_map_opt(
    field: Option<&HashMap<String, i32>>,
    name: &str,
) -> Result<HashMap<i64, i32>, AppError> {
    match field {
        None => Ok(HashMap::new()),
        Some(m) => {
            let mut out = HashMap::with_capacity(m.len());
            for (k, v) in m {
                let n: i64 = k.parse().map_err(|_| {
                    AppError::biz(
                        crate::shared::error::code::BIZ_INVALID_VALUE,
                        format!("{name} contains non-integer key: {k:?}"),
                    )
                })?;
                out.insert(n, *v);
            }
            Ok(out)
        }
    }
}

// ===========================================================================
//  路由表
// ===========================================================================

/// 本域路由表（设计 §6 + §6.2：delivery-notes）。
///
/// axum 静态段优先于参数段；`/scan`、`/candidate-parts`、`/pickup-pending` 必须
/// 在 `/{id}` 之前注册。`/print[/-labels]` 注册在 `/{id}/...` 段里，路径不冲突。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ---- delivery-notes/* ----
        .route("/batch-detail", get(batch_get_delivery_notes)) // ★静态段必须早于 /{id}
        .route("/scan", post(scan_delivery_note))
        .route(
            "/candidate-parts",
            get(list_candidate_parts),
        )
        .route("/pickup-pending", get(list_pickup_pending))
        .route("/", get(list_delivery_notes).post(create_delivery_note))
        .route("/{id}/events", get(list_delivery_note_events))
        .route("/{id}/update", post(update_delivery_note))
        .route("/{id}/add-parts", post(add_delivery_note_parts))
        .route("/{id}/attach-batches", post(attach_batches))
        .route(
            "/{id}/remove-parts",
            post(remove_delivery_note_parts),
        )
        .route("/{id}/submit", post(submit_delivery_note))
        .route("/{id}/recall", post(recall_delivery_note))
        .route("/{id}/pickup-scan", post(pickup_scan))
        .route("/{id}/pickup", post(pickup_delivery_note))
        .route("/{id}/soft-delete", post(soft_delete_delivery_note))
        .route("/{id}/print", post(print_delivery_note))
        .route("/{id}/print-labels", post(print_labels))
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