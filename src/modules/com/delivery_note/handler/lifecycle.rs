//! delivery_note 域状态机转换 handler
//!
//! 范围：状态机迁移端点：submit / recall / pickup。
//!
//! 基础 CRUD 走 `crud.rs`；扫码入单走 `scan.rs`；打印走 `print.rs`。
//!
//! 2026-10-08：`POST /{id}/pickup-scan`（司机端逐件扫码核销）随送货台一并删除。
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

use crate::modules::com::delivery_note::dto::{
    DeliveryNotePath, DeliveryNotePickupRequest, DeliveryNoteVersionedRequest,
};
use crate::modules::com::delivery_note::vo::DeliveryNoteOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// POST /api/v2/com/delivery/note/{id}/submit
///
/// 成功即 DRAFT → SUBMITTED 已发生，出参 `R<String>` 是提交后的送货单 id
/// （雪花 id 序列化为 JSON string）。本次提交会发出 `DELIVERY_NOTE_SUBMITTED`
/// 大屏事件。
///
/// 2026-10-08：旧的 `CANDIDATES_AVAILABLE` 候选分流随入单只允许 `READY_TO_SHIP`
/// 一并删除（见 `service/lifecycle.rs::submit` 的理由）⇒ 现在只有「提交成功」
/// 与「硬错误」两条路径。
pub async fn submit_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<String>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let note_id = state
        .delivery_note_service
        .submit(&mut *tx, path.id, req.version, &current)
        .await?;
    tx.commit().await?;

    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_SUBMITTED".to_string(),
            payload: serde_json::json!({
                "delivery_note_id": note_id,
                "delivery_note_no": path.id.to_string(),
            }),
        });

    Ok(Json(R::ok(note_id.to_string())))
}

/// POST /api/v2/com/delivery/note/{id}/recall
pub async fn recall_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNoteVersionedRequest>,
) -> Result<Json<R<DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .recall(&mut *tx, path.id, req.version, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/com/delivery/note/{id}/pickup
pub async fn pickup_delivery_note(
    State(state): State<Arc<AppState>>,
    current: crate::auth::rbac::CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    Json(req): Json<DeliveryNotePickupRequest>,
) -> Result<Json<R<DeliveryNoteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .pickup(
            &mut *tx,
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
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_PICKED_UP".to_string(),
            payload: payload.clone(),
        });
    tracing::info!(?payload, "delivery_note picked up");

    Ok(Json(R::ok(out)))
}
