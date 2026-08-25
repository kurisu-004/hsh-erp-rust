//! part 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/part.py（设计 §6 + §6.2 — pass_inspection）。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 handler（`current.require_any_role(...)`）；业务层 service 也会
//!   再校验一次（双层守卫，与现有其他域保持一致）。
//!
//! ## Phase F 路由（挂在 `/parts`）
//! - `POST /batch-pass-inspection`         —— 批量送检（per-item 独立事务边界外的循环）
//! - `POST /{part_id}/pass-inspection`     —— 单件送检（payload 可空 `Option<Json<…>>`）
//!
//! 路由顺序敏感：`/batch-pass-inspection` 必须在 `/{part_id}/pass-inspection` 之前
//! 注册，否则 axum 会把 `batch-pass-inspection` 解析成 `part_id`。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto::{
    BatchPassInspectionOut, BatchPassInspectionRequest, BatchScanInspectOut,
    BatchScanInspectRequest, FailInspectionRequest, PartOut, PassInspectionRequest,
    ScanInspectRequest,
};
use crate::modules::part::service::{PartService, BATCH_PASS_INSPECTION_MAX_ITEMS};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 单件 / 批量送检均允许的角色：Manager 或 Inspector。
const PASS_INSPECTION_ROLES: &[Role] = &[Role::Manager, Role::Inspector];

/// scan-inspect 第一步 WS 广播事件（commit 后调用）。
fn ws_broadcast_inspected(state: &AppState, part_id: i64, shelf_code: &str) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "INSPECTED".into(),
        payload: serde_json::json!({
            "part_id": part_id.to_string(),
            "shelf_code": shelf_code,
        }),
    });
}

/// fail-inspection WS 广播事件。
fn ws_broadcast_inspection_failed(state: &AppState, part_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "INSPECTION_FAILED".into(),
        payload: serde_json::json!({ "part_id": part_id.to_string() }),
    });
}

/// POST /api/v2/parts/{part_id}/pass-inspection
///
/// payload 可空（`Option<Json<PassInspectionRequest>>` —— axum 0.8 中 `Json<T>`
/// 已实现 `OptionalFromRequest`，无需 `Optional<T>` 包装）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：path `part_id` + 可选 body `{ batch_id?, quantity? }`
/// - 业务流转：`INSPECTION` → `READY_TO_SHIP`（含多批次 rollup 守卫 + OCC）
/// - 响应：单条 `PartOut`
pub async fn pass_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    payload: Option<Json<PassInspectionRequest>>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(PASS_INSPECTION_ROLES)?;
    let req = payload.map(|j| j.0).unwrap_or_default();
    let mut tx = state.pool.begin().await?;
    let out = PartService::pass_inspection(
        &mut tx,
        &state.snowflake,
        part_id,
        req.batch_id.as_deref().and_then(|s| s.parse().ok()),
        req.quantity,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/batch-pass-inspection
///
/// 批量送检：每个 item 在 handler 共享的外层事务内执行；失败 item 不中断
/// 后续 item，失败原因收集到 `failed` Vec（与 service 内契约一致）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：`{ items: [{ part_id, batch_id?, quantity? }, ...] }`
/// - 入参 shape 校验：handler 先做一次（兜底），service 再做一次（防御性双校验）
/// - 业务流转：per-item 独立 `pass_inspection_core`（共享事务）
/// - 响应：`{ passed: [PartOut, ...], failed: [{ part_id, code, message }, ...] }`
pub async fn batch_pass_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchPassInspectionRequest>,
) -> Result<Json<R<BatchPassInspectionOut>>, AppError> {
    current.require_any_role(PASS_INSPECTION_ROLES)?;
    if req.items.is_empty() {
        return Err(AppError::validation("items 不能为空"));
    }
    if req.items.len() > BATCH_PASS_INSPECTION_MAX_ITEMS {
        return Err(AppError::validation(format!(
            "items 数量 {} 超过上限 {}",
            req.items.len(),
            BATCH_PASS_INSPECTION_MAX_ITEMS,
        )));
    }
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_pass_inspection(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/scan-inspect
///
/// 单件一键送检（`PENDING / PROGRAMMING / IN_PROCESS` → `INSPECTION` → PASS/FAIL）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：path `part_id` + body `ScanInspectRequest`
/// - 业务流转：见 service `scan_inspect_core`
/// - WS 广播：commit 后 `INSPECTED` 事件
/// - 响应：`PartOut`
pub async fn scan_inspect(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ScanInspectRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(PASS_INSPECTION_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::scan_inspect(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast_inspected(&state, part_id, "scan-inspect");
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/batch-scan-inspect
///
/// 批量一键送检（共享品检架 + per-item decision）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：`{ target_inspection_shelf_id, items: [...] }`
/// - 业务流转：service `batch_scan_inspect`（共享外层事务 + per-item 独立 core）
/// - 响应：`{ submitted, failed }`
pub async fn batch_scan_inspect(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchScanInspectRequest>,
) -> Result<Json<R<BatchScanInspectOut>>, AppError> {
    current.require_any_role(PASS_INSPECTION_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_scan_inspect(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "BATCH_INSPECTED".into(),
        payload: serde_json::json!({
            "submitted": out.submitted.len(),
            "failed": out.failed.len(),
        }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/fail-inspection
///
/// 单件品检打回（`INSPECTION` → `IN_PROCESS`，推荐需求 3）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 业务流转：见 service `fail_inspection_core`
/// - WS 广播：commit 后 `INSPECTION_FAILED`
pub async fn fail_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<FailInspectionRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(PASS_INSPECTION_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::fail_inspection(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    ws_broadcast_inspection_failed(&state, part_id);
    Ok(Json(R::ok(out)))
}