//! delivery_note 域扫码入单 handler
//!
//! 范围：扫码入单端点 `/scan`（`scan_delivery_note`）+ 弹窗附挂批次端点
//! `/attach-batches`（`attach_batches`）。
//!
//! 流程（`scan_delivery_note`）：trim → 解析（part → assembly）→ 分类 → find-or-create 草稿 → 批次
//! 评估 → 写 `delivery_note_id`（整个流程在事务内）。commit 后广播一次大屏事件
//! `DELIVERY_NOTE_SCAN_ADD`（轻量级 high-frequency）。
//!
//! 角色：M / C / I（与 Python `pickup_scan` 对应，但 Python 仅 I；这里放宽允许
//! MANAGER/CLERK 调试用，与 `create_draft` 一致）。
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
use axum::extract::{Path, State};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::delivery_note::dto::{
    AttachBatchesRequest, DeliveryNotePath, ScanDeliveryRequest,
};
use crate::modules::delivery_note::vo::{
    AttachBatchesOut, ResolvedKindDto, ScanDeliveryOut, ScanOutcomeDto,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

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
    let out = state
        .delivery_note_service
        .scan_add(&mut *tx, &req.code, &current)
        .await?;
    tx.commit().await?;

    let added_count = out.added_batches.len();
    let note_id = out.note.id;
    let note_no = out.note.delivery_note_no.clone();
    let unresolved_count = out
        .unresolved_targets
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(0);
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_SCAN_ADD".to_string(),
            payload: serde_json::json!({
                "delivery_note_id": note_id,
                "delivery_note_no": note_no,
                "added_count": added_count,
                "unresolved_count": unresolved_count,
                "line_count": out.note.line_count,
                "resolved_kind": match out.resolved.kind {
                    ResolvedKindDto::Part => "PART",
                    ResolvedKindDto::Assembly => "ASSEMBLY",
                },
                "outcome": match out.outcome {
                    ScanOutcomeDto::Added => "ADDED",
                    ScanOutcomeDto::AlreadyPresent => "ALREADY_PRESENT",
                    ScanOutcomeDto::CandidatesAvailable => "CANDIDATES_AVAILABLE",
                    ScanOutcomeDto::PartialAdded => "PARTIAL_ADDED",
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
    let out = state
        .delivery_note_service
        .attach_batches(&mut *tx, path.id, req.batches, &current)
        .await?;
    tx.commit().await?;

    // 提交成功后广播（部分成功也广播，但 frontend 可用 conflicts 长度判断是否需要回滚 UI）
    let payload = serde_json::json!({
        "delivery_note_id": path.id,
        "attached_count": out.attached,
        "conflict_count": out.conflicts.len(),
    });
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_BATCHES_ATTACHED".to_string(),
            payload,
        });

    Ok(Json(R::ok(out)))
}
