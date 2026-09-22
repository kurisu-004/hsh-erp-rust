//! delivery_note 域打印 handler
//!
//! 范围：print / print-labels 端点（设计 §8，P4）+ 内部助手 `parse_i64_opt` /
//! `parse_i64_map_opt`。
//!
//! 渲染送货单 / 标签 xlsx bytes；CPU 密集 umya 渲染走 `tokio::task::spawn_blocking`（由 service 实现）。
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
//! - 统一响应信封：`Result<Json<R<T>>, AppError>` 或返回 `axum::response::Response`（二进制下载）。
//! - 权限在 service 层（`current.require_any_role(...)`）；handler 这里只解析
//!   query / path / body。

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::delivery_note::dto::{
    DeliveryNotePath, PrintDeliveryNoteRequest, PrintLabelsRequest,
};
use crate::shared::error::AppError;
use crate::state::AppState;

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
    use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

    let custom_order = parse_i64_opt(req.custom_order.as_ref(), "custom_order")?;
    let merge_quantities = parse_i64_map_opt(req.merge_quantities.as_ref(), "merge_quantities")?;

    let bytes_prefix = state
        .delivery_note_service
        .print_xlsx(
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

    let filename = format!("F-{}-note.xlsx", chrono::Local::now().format("%Y-%m-%d"));
    let len = bytes.len();
    let resp = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(
            CONTENT_TYPE,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(CONTENT_LENGTH, len.to_string())
        .header(CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError::internal(format!("build print response: {e}")))?;

    // 渲染成功后广播（轻量：只推单据级事件，不按行推送）
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
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
    use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

    let custom_order = parse_i64_opt(req.custom_order.as_ref(), "custom_order")?;
    let merge_quantities = parse_i64_map_opt(req.merge_quantities.as_ref(), "merge_quantities")?;
    let line_item_ids = parse_i64_opt(req.line_item_ids.as_ref(), "line_item_ids")?;

    let bytes_prefix = state
        .delivery_note_service
        .print_xlsx(
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

    let filename = format!("F-{}-labels.xlsx", chrono::Local::now().format("%Y-%m-%d"));
    let len = bytes.len();
    let resp = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(
            CONTENT_TYPE,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(CONTENT_LENGTH, len.to_string())
        .header(CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError::internal(format!("build labels response: {e}")))?;

    // 渲染成功后广播（轻量：只推单据级事件，不按行推送）
    state
        .ws_hub
        .broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
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
