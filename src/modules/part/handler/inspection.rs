//! part 域 inspection / 扫码 handler
//!
//! 对应 Phase F2/F3 to-XXX 流 + 扫码快捷路径：
//! - to-ship / to-inspection / to-process 单件流转（含批次级 OCC + 多批次 rollup 守卫）
//! - batch-to-ship / batch-to-inspection 批量流转（per-item 独立事务边界外的循环）
//! - scan-inspect / scan-deliver-part / worker-scan 扫码快捷入口
//! - list_repair_batches / list_repairing_batches 返修批次列表
//!
//! WS 广播：commit 之后广播（对齐 Python 延迟广播模式）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto::{
    BatchToShipRequest, InspectionBatchListQuery, ToInspectionRequest, ToProcessRequest,
    ToShipRequest, WorkerScanRequest,
};
use crate::modules::part::dto_crud::{ScanDeliverPartRequest, ScanInspectRequest};
use crate::modules::part::service::{BATCH_TO_SHIP_MAX_ITEMS, PartService};
use crate::modules::part::vo::{
    BatchToXxxOut, InspectionBatchListOut, PartOut, ToXxxOut, WorkerScanOut,
};
use crate::modules::prod::worker_pool::service::WorkerPoolService;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// to-XXX 流均允许的角色：Manager 或 Inspector。
const TO_XXX_ROLES: &[Role] = &[Role::Manager, Role::Inspector];

/// to-ship WS 广播事件（commit 后调用）。
fn ws_broadcast_to_ship(state: &AppState, part_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_TO_SHIP".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
}

/// to-inspection WS 广播事件（commit 后调用）。
fn ws_broadcast_to_inspection(state: &AppState, part_id: i64, shelf_code: &str) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_TO_INSPECTION".into(),
        payload: json!({
            "part_id": part_id.to_string(),
            "shelf_code": shelf_code,
        }),
    });
}

/// to-process WS 广播事件（commit 后调用）。
fn ws_broadcast_to_process(state: &AppState, part_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_TO_PROCESS".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
}

/// 父装配件自动同步 WS 广播事件（commit 后调用；inspection 流触发父
/// status 翻转时推送给 dashboard）。
fn ws_broadcast_assembly_updated(state: &AppState, assembly_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
}

/// `POST /api/v2/parts/{part_id}/to-ship`
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：path `part_id` + body `ToShipRequest`（`batch_id` / `version` 必填）
/// - 业务流转：`INSPECTION` → `READY_TO_SHIP`（含多批次 rollup 守卫 + 批次级 OCC）
/// - WS 广播：commit 后 `PART_TO_SHIP`
/// - 响应：`ToXxxOut { part, new_batch_id }`
pub async fn to_ship(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ToShipRequest>,
) -> Result<Json<R<ToXxxOut>>, AppError> {
    current.require_any_role(TO_XXX_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::to_ship(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    if let Some(aid) = out.synced_assembly_id {
        ws_broadcast_assembly_updated(&state, aid);
    }
    ws_broadcast_to_ship(&state, part_id);
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/batch-to-ship`
///
/// 批量通过品检：每个 item 在 handler 共享的外层事务内执行；失败 item 不中断
/// 后续 item，失败原因收集到 `failed` Vec（与 service 内契约一致）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：`{ items: [{ batch_id, quantity? }, ...] }`
/// - 入参 shape 校验：handler 先做一次（兜底），service 再做一次（防御性双校验）
/// - 业务流转：per-item 独立 `to_ship_core`（共享事务）
/// - WS 广播：commit 后 `BATCH_TO_SHIP`
/// - 响应：`{ submitted: [ToXxxOut, ...], failed: [{ batch_id, code, message }, ...] }`
pub async fn batch_to_ship(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchToShipRequest>,
) -> Result<Json<R<BatchToXxxOut>>, AppError> {
    current.require_any_role(TO_XXX_ROLES)?;
    if req.items.is_empty() {
        return Err(AppError::validation("items 不能为空"));
    }
    if req.items.len() > BATCH_TO_SHIP_MAX_ITEMS {
        return Err(AppError::validation(format!(
            "items 数量 {} 超过上限 {}",
            req.items.len(),
            BATCH_TO_SHIP_MAX_ITEMS,
        )));
    }
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_to_ship(&mut *tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    let mut seen_assemblies = std::collections::HashSet::new();
    for item in &out.submitted {
        if let Some(aid) = item.synced_assembly_id
            && seen_assemblies.insert(aid)
        {
            ws_broadcast_assembly_updated(&state, aid);
        }
    }
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "BATCH_TO_SHIP".into(),
        payload: json!({
            "submitted": out.submitted.len(),
            "failed": out.failed.len(),
        }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/to-inspection`
///
/// 单件送检（`PENDING / PROGRAMMING / IN_PROCESS` → `INSPECTION`）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：path `part_id` + body `ToInspectionRequest`（`batch_id` / `version` 必填）
/// - 业务流转：见 service `to_inspection_core`（含批次级 OCC）
/// - WS 广播：commit 后 `PART_TO_INSPECTION`
/// - 响应：`ToXxxOut { part, new_batch_id }`
pub async fn to_inspection(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ToInspectionRequest>,
) -> Result<Json<R<ToXxxOut>>, AppError> {
    current.require_any_role(TO_XXX_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::to_inspection(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    if let Some(aid) = out.synced_assembly_id {
        ws_broadcast_assembly_updated(&state, aid);
    }
    ws_broadcast_to_inspection(&state, part_id, "to-inspection");
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/to-process`
///
/// 单件品检打回（`INSPECTION` → `IN_PROCESS`，推荐需求 3）。
///
/// 行为：
/// - 权限：`Manager` 或 `Inspector`
/// - 入参：path `part_id` + body `ToProcessRequest`（`batch_id` / `version` 必填）
/// - 业务流转：见 service `to_process_core`（含批次级 OCC）
/// - WS 广播：commit 后 `PART_TO_PROCESS`
/// - 响应：`ToXxxOut { part, new_batch_id }`
pub async fn to_process(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ToProcessRequest>,
) -> Result<Json<R<ToXxxOut>>, AppError> {
    current.require_any_role(TO_XXX_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::to_process(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    if let Some(aid) = out.synced_assembly_id {
        ws_broadcast_assembly_updated(&state, aid);
    }
    ws_broadcast_to_process(&state, part_id);
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/scan-inspect`
///
/// 扫码快捷品检：一步式 `{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION →
/// READY_TO_SHIP（pass=true）或 REPAIRING（pass=false）。
pub async fn scan_inspect(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<ScanInspectRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::scan_inspect(&mut *tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    let kind = if out.status == "READY_TO_SHIP" {
        "PART_SCAN_INSPECT_PASSED"
    } else {
        "PART_SCAN_INSPECT_FAILED"
    };
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/scan/deliver-part`
///
/// 司机扫码发货：part_serial_no 反查 part_id + worker_badge_code 校验 DRIVER 工种。
pub async fn scan_deliver_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ScanDeliverPartRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::scan_deliver_part(&mut *tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_DELIVERED".into(),
        payload: json!({ "part_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/worker-scan`
///
/// 工人扫码台主入口：RETURNED / INSPECTED 二合一。**同事务**调 scan →
/// refill_for_worker（OM-6 决议：scan 与 refill 必须原子，否则扫描放回 →
/// refill 抢批中间会被并发抢走同批）。
///
/// 行为：
/// - 权限：`Manager` 或 `ShelfAccount`（不是 Inspector——工人持有件自有工人操作）
/// - 入参：`WorkerScanRequest { serial_no, badge_code, event_type, shelf_id, ... }`
/// - 业务流转：
///   - `RETURNED`：worker 把 IN_PROCESS+WORKER 批次放回生产架（next_process_id 必填，
///     shelf ↔ process 必须有映射）；
///   - `INSPECTED`：worker 把持有件直接送检（target_inspection_shelf_id 必填，
///     target shelf ∈ INSPECTION 区）；
///   - 任一成功后同事务 `WorkerPoolService::refill_for_worker`。
/// - WS 广播：commit 后
///   - `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 event_type）；
///   - `WORKER_POOL_REFILL_DONE`（refill 抢到一批）或
///   - `WORKER_POOL_EMPTY`（refill 池空）。
///
/// `current: CurrentUser` 直接参数：依赖 `CurrentUser` 的
/// `FromRequestParts<Arc<AppState>>` impl 从 Bearer JWT 解析（与
/// `part/handler.rs::to_ship` / `worker_pool/handler.rs` 同形）。
pub async fn worker_scan(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkerScanRequest>,
) -> Result<Json<R<WorkerScanOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
    // 防御性：shelf_ids 是手填白名单，manager 因 wildcard=true 自动通过
    if !current.can_access_shelf(req.shelf_id) {
        return Err(AppError::biz(
            crate::shared::error::code::SHELF_MISMATCH,
            format!("无权限访问 shelf {}", req.shelf_id),
        ));
    }
    // INSPECTED 时 target_inspection_shelf_id 也必须校验（防御性，避免 SHELF_ACCOUNT
    // 用户手填两个不在 scope 内的 shelf_id）
    if let Some(tid) = req
        .target_inspection_shelf_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        && !current.can_access_shelf(tid)
    {
        return Err(AppError::biz(
            crate::shared::error::code::SHELF_MISMATCH,
            format!("无权限访问 shelf {}", tid),
        ));
    }
    let mut tx = state.pool.begin().await?;
    // scan（状态翻转 + 写事件日志）
    let scan_out =
        PartService::worker_scan_event(&mut *tx, &state.snowflake, req.clone(), &current).await?;
    // refill（同事务；WorkerPoolService::refill_for_worker_with_work_type 内部
    // 对 work_type / process 映射校验失败会抛业务错——事务自动回滚 scan 写入，保持原子语义）。
    // 复用 worker_scan_event 已经 fetch 过的 work_type_id + badge_code，
    // 跳过 worker_pool service 内的 WorkerRepo::get_by_id 重复查询。
    let refill_out = WorkerPoolService::refill_for_worker_with_work_type(
        &mut tx,
        &state.snowflake,
        scan_out.worker_id,
        scan_out.work_type_id,
            req.shelf_id,
            &scan_out.badge_code,
            current.id,
            &current,
        )
        .await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）
    if let Some(aid) = scan_out.synced_assembly_id {
        ws_broadcast_assembly_updated(&state, aid);
    }
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: scan_out.event_type.clone(),
        payload: serde_json::to_value(&scan_out).unwrap_or_default(),
    });
    if !refill_out.taken.is_empty() {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_REFILL_DONE".into(),
            payload: serde_json::to_value(&refill_out).unwrap_or_default(),
        });
    } else if refill_out.pool_empty {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_EMPTY".into(),
            payload: json!({
                "worker_id": scan_out.worker_id.to_string(),
                "shelf_id": req.shelf_id.to_string(),
                "pool_empty": true,
            }),
        });
    }
    Ok(Json(R::ok(WorkerScanOut {
        scan: scan_out,
        refill: refill_out,
    })))
}

/// `GET /api/v2/parts/repair-batches`
///
/// DELIVERED 批次列表（Manager + Clerk + Inspector）。
pub async fn list_repair_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<InspectionBatchListQuery>,
) -> Result<Json<R<InspectionBatchListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_repair_batches(&mut *tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/repairing-batches`
///
/// REPAIRING 批次列表（Manager + Clerk + Inspector）。
pub async fn list_repairing_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<InspectionBatchListQuery>,
) -> Result<Json<R<InspectionBatchListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_repairing_batches(&mut *tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}
