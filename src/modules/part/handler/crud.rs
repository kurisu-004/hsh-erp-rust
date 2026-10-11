//! part 域单件 CRUD handler
//!
//! 对应 Python myERP `api/v1/part.py` 中的 list / detail / create / update /
//! soft-delete / by-serial 端点。
//!
//! 范围：本文件覆盖 list / detail / create / update / soft-delete / by-serial /
//! 位置树 / Excel 工具。lifecycle（part 级终态 + 各类 list）/ batch（批量创建 +
//! COS confirm）见同名文件。
//!
//! ## 权限
//! - 列表 / 详情 / by-serial：4 角色全开放（Manager / Clerk / Inspector / CncProgrammer）
//! - 写操作（create / update / soft-delete）：Manager + Clerk
//! - soft-delete：Manager 专属

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::assembly::vo::AssemblyDetail;
use crate::modules::part::dto_crud::{
    BatchUpdateOrderInfoRequest, MatchByExcelItemsRequest, PartCreateRequest, PartListQuery,
    PartSoftDeleteRequest, PartUpdateRequest,
};
use crate::modules::part::service::PartService;
use crate::modules::part::vo::{
    BatchUpdateOrderInfoOut, LocationTreeOut, MatchByExcelItemResult, PartBatchListItemOut,
    PartDetailOut, PartEventOut, PartListOut, PartScanContextOut,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 列表 / 详情 / by-serial 允许角色：4 角色全开放。
const LIST_PART_ROLES: &[Role] = &[
    Role::Manager,
    Role::Clerk,
    Role::Inspector,
    Role::CncProgrammer,
];

/// CRUD（create / update）允许角色。
///
/// ⚠️ 2026-10-13 PR1b：原注释里的 `upload-drawing` / `upload-3d-model` 两条端点
/// 已迁往 `files::part_file`（`POST /api/v2/files/objects`），本常量自此只服务
/// `create_part` / `update_part` 两处。
const CRUD_PART_ROLES: &[Role] = &[Role::Manager, Role::Clerk];

/// `DELETE` / 单件 `SOFT-DELETE` 软删后 WS 广播事件。
fn ws_broadcast_soft_deleted(state: &AppState, part_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_SOFT_DELETED".into(),
        payload: json!({ "part_id": part_id.to_string() }),
    });
}

/// `GET /api/v2/parts`
///
/// 列表查询 + 分页（service 内已校验角色）。
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`，不开事务（连接用完即 drop 归还）。
pub async fn list_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<PartListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = PartService::list_parts(&mut *conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/{part_id}`
///
/// 单件详情。`path` 段 `part_id` 是 i64；service 内 OCC 已用 version 守。
///
/// 2026-09-22 PR5：只读 get 端点改 `pool.acquire()`。
pub async fn get_part_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = PartService::get_part(&mut *conn, part_id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-serial/{serial_no}`
///
/// 通过序列号查详情（`part.serial_no` 唯一索引）。
///
/// 2026-09-22 PR5：只读 get 端点改 `pool.acquire()`。
pub async fn get_by_serial(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<PartDetailOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = PartService::get_part_by_serial(&mut *conn, &serial_no, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/by-serial/{serial_no}/part-batches`
///
/// 扫码快捷品检上下文：返回工单窄字段（8 列 + id）+ 全部活跃批次（含 holder 名称）。
/// 前端扫码弹窗据此拼 `POST /prod/batches/{batch_id}/to-ship` 的 `{ version }`。
/// 与 `get_by_serial`（`PartDetailOut` 28 列）并存，互不替代。
///
/// 2026-09-22 PR5：只读 get 端点改 `pool.acquire()`。
pub async fn get_by_serial_part_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<PartScanContextOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = PartService::get_part_batches_by_serial(&mut *conn, &serial_no, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/{part_id}/assembly` → 200 OK
///
/// 2026-09-25 新增（D-08 api-drift-fix）：按 part 反查其所属装配体。
///
/// 行为：
/// - 权限：Manager / Clerk / Inspector / CncProgrammer（4 角色全开放，与
///   `GET /assemblies/{id}` 对齐）
/// - 读端点：不开事务（`pool.acquire()`）
/// - 业务流转：service 层 `get_assembly_by_part`：
///   1. `part` 不存在 → `20101 PART_NOT_FOUND`（HTTP 404）
///   2. `part.assembly_id IS NULL` → 返回 `null`（无父装配体）
///   3. 否则委托 assembly service `get_assembly` 拿 AssemblyDetail
/// - 响应：`R<Option<AssemblyDetail>>` —— 前端拿 `null` 表示 part 不属于任何装配体
///
/// 设计意图：路由注册在 `part::router()`（而不是 `part_file` 等其它 nest 下），
/// 是因为前端 API 客户端（`src/api/assembly.ts:30`）直接走 `parts/{part_id}/assembly`
/// 路径，避免前端再加一层 `assembly/part/{id}` 的间接寻址。
pub async fn get_assembly_by_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<Option<AssemblyDetail>>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = PartService::get_assembly_by_part(&mut *conn, part_id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts` → 201 Created
///
/// 单件创建工单。响应只含 `PartDetailOut`（无 `PartCreateResult`；图纸上传由独立
/// 端点 `POST /api/v2/files/objects` 处理 —— 2026-10-13 PR1b 前是本域的
/// `/upload-drawing`）。
pub async fn create_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PartCreateRequest>,
) -> Result<(StatusCode, Json<R<PartDetailOut>>), AppError> {
    current.require_any_role(CRUD_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::create_part(&mut *tx, &state.snowflake, &req, &current).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `POST /api/v2/parts/{part_id}/update`
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
    let out = PartService::update_part(&mut *tx, part_id, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/{part_id}/soft-delete`
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
    PartService::soft_delete_part(&mut *tx, &state.snowflake, part_id, req.version, &current)
        .await?;
    tx.commit().await?;
    ws_broadcast_soft_deleted(&state, part_id);
    Ok(Json(R::ok_empty()))
}

/// `GET /api/v2/parts/{part_id}/events`
///
/// 事件历史（按 `id` DESC，**不是** `created_at` DESC —— `t_part_event.id` 空间异质：
/// 雪花 / migration 回填的连续密集块 / 未被 nextval 用过的序列，按 `created_at` 排会
/// 在一堆 7 月事件里夹一条迁移执行时刻的「10-01」。完整取舍见
/// `PartService::list_events` 的 SQL 注释与 `PartEventOut` 的 doc）。
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`。
pub async fn list_part_events(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<Vec<PartEventOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::list_events(&mut *conn, part_id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/location-tree`
///
/// 按 shelf/status 聚合位置树（OFFICE / PRODUCTION_SHELF / WORKER /
/// INSPECTION_SHELF / OUTSOURCE_COMPANY 五区 + 各 holder 子节点）。
///
/// 2026-09-22 PR5：只读 aggregate 端点改 `pool.acquire()`。
pub async fn get_location_tree(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<LocationTreeOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::location_tree(&mut *conn, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/match-by-excel-items`
///
/// 采购订单 Excel 明细行 → 候选零件（回填 order_no / system_delivery_date 的前置）。
///
/// 2026-09-22 PR5：POST 入参但纯只读匹配，改 `pool.acquire()`。
/// 2026-10-06：service 侧从「逐行发 SQL」改成「整请求 ≤4 条查询」，逻辑迁往
/// `part::service::phase1::excel_match`。
pub async fn match_by_excel_items(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<MatchByExcelItemsRequest>,
) -> Result<Json<R<Vec<MatchByExcelItemResult>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::match_by_excel_items(&mut *conn, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/batch-update-order-info`
///
/// 批量更新工单 order_no / system_delivery_date / note。
///
/// 2026-10-06：**不再用事务**，改 `pool.acquire()`。原因：本端点的契约是
/// 「HTTP 200 + 信封，逐行部分成功」（`failed` 逐条列出行级错误），而 PG 事务内
/// 一条语句报错后整笔进入 aborted 状态 —— 其余行的 UPDATE 会连带失败，
/// 末尾 `commit()` 也会报错，于是「部分成功」被整体降级成 500。各行写的
/// 本来是互不相干的行、每条 UPDATE 自身原子，事务没有额外收益。
pub async fn batch_update_order_info(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchUpdateOrderInfoRequest>,
) -> Result<Json<R<BatchUpdateOrderInfoOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::batch_update_order_info(&mut *conn, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/{part_id}/batches`
///
/// 工单全部活跃批次列表（含 holder 名称 / 下一工序 / 父批次等元信息）。
///
/// 2026-09-22 PR5：只读 list 端点改 `pool.acquire()`。
pub async fn list_part_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<Vec<PartBatchListItemOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartService::list_batches(&mut *conn, part_id, &current).await?;
    Ok(Json(R::ok(out)))
}
