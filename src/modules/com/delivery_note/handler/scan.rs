//! com::delivery_note 域扫码入单 / 扫码树 handler
//!
//! 两个端点同源（都以 `serial_no` 为入口），但职责严格分开：
//! - `GET /api/v2/com/delivery/note/scan/{serial_no}` —— **纯读**三层树。回答
//!   「这是谁的件 / 现在什么状态 / 每个批次能点什么动作 / 现有草稿在哪」。绝不建单。
//! - `POST /api/v2/com/delivery/note/scan` —— **唯一**入单入口。在同一事务内完成
//!   find-or-create 草稿 + DP 分配 + 拆批 + 挂单 + `note.version++`。
//!
//! 角色：Manager / Clerk / Inspector（与送货单其余端点同一组；`ShelfAccount` 不放行
//! —— 它只该扫码核销批次，不该开送货单）。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 借 `&mut *tx` 喂给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - WS 广播在 `tx.commit()` **之后**（handler 形态②）。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::com::delivery_note::dto::ScanEntryRequest;
use crate::modules::com::delivery_note::vo::{DeliveryNoteDetailOut, DeliveryScanTreeOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/com/delivery/note/scan/{serial_no}` —— 扫码三层树（纯读）。
///
/// `serial_no` 收 `Path<String>`（**不是** `ni64!` 数值提取器）：序列号是
/// `varchar(15)` 且可能含 `-`，前端必须 `encodeURIComponent`。
///
/// 命中口径：先查 `t_part.serial_no`（软删闸门 + `ORDER BY (status='CANCELLED') ASC,
/// id DESC LIMIT 1`），未命中再查 `t_assembly.serial_no`；都未命中 ⇒
/// `20101 BIZ_PART_NOT_FOUND`（HTTP 404）。trim 后为空同样按未命中。
pub async fn scan_tree(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<DeliveryScanTreeOut>>, AppError> {
    // 读端点：pool.acquire() → service → drop。不开事务、不发广播、不建单。
    let mut conn = state.pool.acquire().await?;
    let out = state
        .delivery_note_service
        .scan_tree(&mut *conn, &current, &serial_no)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/com/delivery/note/scan` —— 扫码入单（唯一入口）。
///
/// 出参 `DeliveryNoteDetailOut` 含**拆批后的完整行项** ⇒ 前端可就地替换草稿卡，
/// 不用重新扫一遍。
///
/// commit 后广播一次大屏事件 `DELIVERY_NOTE_SCAN_ADD`（轻量级 high-frequency）。
pub async fn scan_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ScanEntryRequest>,
) -> Result<Json<R<DeliveryNoteDetailOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .delivery_note_service
        .scan_entry(&mut *tx, req, &current)
        .await?;
    tx.commit().await?;

    let payload = serde_json::json!({
        "delivery_note_id": out.head.id,
        "delivery_note_no": out.head.delivery_note_no,
        "line_count": out.line_items.len(),
        "version": out.head.version,
    });
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
            kind: "DELIVERY_NOTE_SCAN_ADD".to_string(),
            payload: payload.clone(),
        });
    tracing::info!(?payload, "delivery_note scan entry");

    Ok(Json(R::ok(out)))
}
