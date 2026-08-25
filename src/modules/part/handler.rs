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
    ScanInspectRequest, WorkerScanOut, WorkerScanRequest,
};
use crate::modules::part::service::{PartService, BATCH_PASS_INSPECTION_MAX_ITEMS};
use crate::modules::worker_pool::service::WorkerPoolService;
use crate::shared::error::{code, AppError};
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

/// POST /api/v2/parts/worker-scan
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
/// `part/handler.rs::pass_inspection` / `worker_pool/handler.rs` 同形）。
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
    {
        if !current.can_access_shelf(tid) {
            return Err(AppError::biz(
                crate::shared::error::code::SHELF_MISMATCH,
                format!("无权限访问 shelf {}", tid),
            ));
        }
    }
    let mut tx = state.pool.begin().await?;
    // scan（状态翻转 + 写事件日志）
    let scan_out = PartService::worker_scan_event(
        &mut tx,
        &state.snowflake,
        req.clone(),
        &current,
    )
    .await?;
    // refill（同事务；WorkerPoolService::refill_for_worker_with_work_type 内部对 work_type / process
    // 映射校验失败会抛业务错——事务自动回滚 scan 写入，保持原子语义）。
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
    )
    .await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）
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
            payload: serde_json::json!({
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

// ===== Phase PR-CRUD =====

use axum::extract::{Multipart, Query};
use axum::http::StatusCode;
use serde_json::json;

use crate::modules::part::dto_crud::{
    CancelRequest, CompleteRequest, DeliverRequest, PartBatchCreateRequest,
    PartBatchCreateOut, PartCreateRequest, PartDetailOut, PartListOut,
    PartListQuery, PartSoftDeleteRequest, PartUpdateRequest, StartRepairRequest,
};
use crate::modules::part_file::model::TPartFile;

/// 列表 / 详情 / by-serial 允许角色：4 角色全开放。
const LIST_PART_ROLES: &[Role] =
    &[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer];
/// CRUD（create / batch-create / update）允许角色：Manager + Clerk。
const CRUD_PART_ROLES: &[Role] = &[Role::Manager, Role::Clerk];

/// GET /api/v2/parts
///
/// 列表查询 + 分页（service 内已校验角色）。
pub async fn list_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<PartListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_parts(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/parts/{part_id}
///
/// 单件详情。`path` 段 `part_id` 是 i64；service 内 OCC 已用 version 守。
pub async fn get_part_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::get_part(&mut tx, part_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/parts/by-serial/{serial_no}
///
/// 通过序列号查详情（`part.serial_no` 唯一索引）。
pub async fn get_by_serial(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::get_part_by_serial(&mut tx, &serial_no, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts → 201 Created
///
/// 单件创建工单。响应只含 `PartDetailOut`（无 `PartCreateResult`，upload
/// drawing 由独立端点 `/upload-drawing` 处理）。
pub async fn create_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PartCreateRequest>,
) -> Result<(StatusCode, Json<R<PartDetailOut>>), AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::create_part(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/parts/batch
///
/// 批量创建（共享 `customer_id`）；per-item 失败不中断整体。
pub async fn batch_create_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PartBatchCreateRequest>,
) -> Result<Json<R<PartBatchCreateOut>>, AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_create_parts(&mut tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/update
///
/// 字段可选 UPDATE；OCC 通过 `req.version` 守。
pub async fn update_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PartUpdateRequest>,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::update_part(&mut tx, part_id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/soft-delete
///
/// Manager 专属软删；OCC 守；commit 后广播 `PART_SOFT_DELETED`。
pub async fn soft_delete_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<PartSoftDeleteRequest>,
) -> Result<Json<R<()>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    PartService::soft_delete_part(
        &mut tx,
        &state.snowflake,
        part_id,
        req.version,
        &current,
    )
    .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_SOFT_DELETED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/parts/{part_id}/upload-drawing
///
/// Multipart 严格校验（Finding F）：
/// - 必须恰好含一个 `file` 字段；缺字段 / 多 `file` / 未知字段名一律 40001
/// - 不为 `file` 默认 MIME —— service 层做严格 `application/pdf` 守卫，
///   客户端忘记设头会得到 21102 `BIZ_PART_FILE_BAD_TYPE`
///
/// 权限（Finding B）：先 `require_any_role` 再读 multipart，避免非授权请求
/// 触发 50 MB 内存分配。
pub async fn upload_drawing(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    mut multipart: Multipart,
) -> Result<Json<R<TPartFile>>, AppError> {
    // Finding B：权限守卫先于 multipart 解析 —— 拒绝未授权请求的内存分配。
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut bytes: Option<(Vec<u8>, String, Option<String>)> = None;
    let mut file_seen = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            // Finding F：拒绝第二个 `file` 字段（40001）。
            if file_seen {
                return Err(AppError::validation(
                    "multipart 包含多个 'file' 字段（仅允许 1 个）",
                ));
            }
            file_seen = true;
            let fname = field.file_name().unwrap_or("upload.pdf").to_string();
            // Finding F：不再 `unwrap_or("application/pdf")` 默认 MIME；
            // 保留客户端提供的原始 content_type（或 None），交给 service 层
            // 严格比对 `application/pdf`，非 PDF 直接 21102。
            let ct = field.content_type().map(|m| m.to_string());
            let data = field
                .bytes()
                .await
                .map_err(|e| AppError::validation(format!("file 读取失败: {e}")))?
                .to_vec();
            bytes = Some((data, fname, ct));
        } else {
            // Finding F：未知字段名一律 40001（避免静默接受错位数据）。
            return Err(AppError::validation(format!(
                "multipart 未知字段: '{name}'（仅接受 'file'）"
            )));
        }
    }
    let (data, fname, ct) = bytes
        .ok_or_else(|| AppError::validation("multipart 缺少 'file' 字段"))?;
    let ct = ct
        .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "file 缺少 content_type"))?;
    let mut tx = state.pool.begin().await?;
    let pf = PartService::upload_drawing(
        &mut tx, &state.snowflake, &state, part_id, &data, &fname, &ct, &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(pf)))
}

/// POST /api/v2/parts/{part_id}/deliver
///
/// READY_TO_SHIP → DELIVERED；commit 后广播 `PART_DELIVERED`。
pub async fn deliver(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<DeliverRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::deliver(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_DELIVERED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/cancel
///
/// 5 状态白名单 → CANCELLED；commit 后广播 `PART_CANCELLED`。
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::cancel(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_CANCELLED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/complete
///
/// DELIVERED → COMPLETED；commit 后广播 `PART_COMPLETED`。
pub async fn complete(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<CompleteRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::complete(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_COMPLETED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/parts/{part_id}/start-repair
///
/// IN_PROCESS → REPAIRING；commit 后广播 `PART_REPAIR_STARTED`。
pub async fn start_repair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Json(req): Json<StartRepairRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::start_repair(&mut tx, &state.snowflake, part_id, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_REPAIR_STARTED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}