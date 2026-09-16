//! part 域单件 CRUD handler
//!
//! 对应 Python myERP `api/v1/part.py` 中的 list / detail / create / update /
//! soft-delete / by-serial 端点。
//!
//! 范围：本文件覆盖 11 个 CRUD 端点。lifecycle / inspection / batch 见同名文件。
//!
//! ## 权限
//! - 列表 / 详情 / by-serial：4 角色全开放（Manager / Clerk / Inspector / CncProgrammer）
//! - 写操作（create / update / soft-delete）：Manager + Clerk
//! - soft-delete：Manager 专属

use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::dto::{
    InspectionBatchListOut, InspectionBatchListQuery, PartScanContextOut,
};
use crate::modules::part::dto_crud::{
    BatchUpdateOrderInfoOut, BatchUpdateOrderInfoRequest, LocationTreeOut, MatchByExcelItemResult,
    MatchByExcelItemsRequest, PartBatchListItemOut, PartCreateRequest, PartDetailOut, PartEventOut,
    PartListOut, PartListQuery, PartSoftDeleteRequest, PartUpdateRequest,
};
use crate::modules::part::service::PartService;
use crate::modules::part_file::model::TPartFile;
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// 列表 / 详情 / by-serial 允许角色：4 角色全开放。
const LIST_PART_ROLES: &[Role] = &[
    Role::Manager,
    Role::Clerk,
    Role::Inspector,
    Role::CncProgrammer,
];

/// CRUD（create / batch-create / update / upload-drawing / upload-3d-model）允许角色。
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

/// `GET /api/v2/parts/inspection-batches`
///
/// 状态筛选列表：返回 `status='INSPECTION'` 全部活跃批次（含工单 + holder /
/// process / delivery_note / customer 名称一次解析）。前端用每行的
/// `batch_id + version` 直接拼 `POST /parts/{part_id}/to-ship` 或
/// `to-inspection` 的请求体，替代每次扫码 / 手动输入。
///
/// 行为：
/// - 权限：Manager 或 Inspector
/// - Query：`InspectionBatchListQuery { keyword?, customer_id?, serial_no?, planned_delivery_date_from?, planned_delivery_date_to?, limit?, offset? }`
/// - 业务流转：纯读，不开 WS 广播
/// - 响应：`InspectionBatchListOut { items, total, limit, offset }`
pub async fn list_inspection_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<InspectionBatchListQuery>,
) -> Result<Json<R<InspectionBatchListOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Inspector])?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_inspection_batches(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/{part_id}`
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

/// `GET /api/v2/parts/by-serial/{serial_no}`
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

/// `GET /api/v2/parts/by-serial/{serial_no}/part-batches`
///
/// 扫码快捷品检上下文：返回工单窄字段（8 列 + id）+ 全部活跃批次（含 holder 名称）。
/// 前端扫码弹窗据此拼 `POST /parts/{part_id}/to-ship` 的 `{ batch_id, version }`。
/// 与 `get_by_serial`（`PartDetailOut` 28 列）并存，互不替代。
pub async fn get_by_serial_part_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<PartScanContextOut>>, AppError> {
    current.require_any_role(LIST_PART_ROLES)?;
    let mut tx = state.pool.begin().await?;
    let out = PartService::get_part_batches_by_serial(&mut tx, &serial_no, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts` → 201 Created
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
    let out = PartService::update_part(&mut tx, part_id, &req, &current).await?;
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
    PartService::soft_delete_part(&mut tx, &state.snowflake, part_id, req.version, &current)
        .await?;
    tx.commit().await?;
    ws_broadcast_soft_deleted(&state, part_id);
    Ok(Json(R::ok_empty()))
}

/// `POST /api/v2/parts/{part_id}/upload-drawing`
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
    let (data, fname, ct) = read_single_file_field(&mut multipart).await?;
    let ct =
        ct.ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "file 缺少 content_type"))?;
    let mut tx = state.pool.begin().await?;
    let pf = PartService::upload_drawing(
        &mut tx,
        &state.snowflake,
        &state,
        part_id,
        &data,
        &fname,
        &ct,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(pf)))
}

/// `POST /api/v2/parts/{part_id}/upload-3d-model`
///
/// 上传 part 3D 模型（STEP/STP/IGES/IGS/STL/OBJ/3MF），对齐 Python 后端
/// `POST /api/v1/parts/{part_id}/3d-models`（2026-09-11 新增）。
///
/// Multipart 字段：
/// - `file`：3D 模型字节（≤ 50 MB）；扩展名 + content_type 由 service 层
///   `policy::allowed_exts("3D_MODEL")` / `expected_content_types_for_ext` 校验
///
/// 权限：`Manager` / `Clerk`（同 upload-drawing）
///
/// 错误码：
/// - 21102 `BIZ_PART_FILE_BAD_TYPE` — 扩展名不在白名单 / content_type 与扩展名不一致
/// - 21103 `BIZ_PART_FILE_TOO_LARGE` — 空字节 / > 50 MB
/// - 21104 `BIZ_PART_FILE_UPLOAD_FAILED` — COS SDK 抛错
/// - 21105 `BIZ_PART_FILE_OWNER_NOT_FOUND` — part 不存在
/// - 21108 `BIZ_PART_FILE_DUPLICATE` — 同 part + kind + sha256 撞唯一索引
pub async fn upload_3d_model(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    mut multipart: Multipart,
) -> Result<Json<R<TPartFile>>, AppError> {
    // 权限守卫先于 multipart 解析（同 upload_drawing）
    current.require_any_role(CRUD_PART_ROLES)?;
    let (data, fname, ct) = read_single_file_field(&mut multipart).await?;
    let ct =
        ct.ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "file 缺少 content_type"))?;
    let mut tx = state.pool.begin().await?;
    let pf = PartService::upload_3d_model(
        &mut tx,
        &state.snowflake,
        &state,
        part_id,
        &data,
        &fname,
        &ct,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(pf)))
}

/// 共享 helper：从 multipart 流里读恰好一个 `file` 字段（拒绝多 / 缺 / 未知字段）。
///
/// `upload_drawing` / `upload_3d_model` 共用同一段 multipart 严格校验：
/// - 必须恰好含一个 `file` 字段；缺字段 / 多 `file` / 未知字段名一律 40001。
async fn read_single_file_field(
    multipart: &mut Multipart,
) -> Result<(Vec<u8>, String, Option<String>), AppError> {
    let mut bytes: Option<(Vec<u8>, String, Option<String>)> = None;
    let mut file_seen = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            if file_seen {
                return Err(AppError::validation(
                    "multipart 包含多个 'file' 字段（仅允许 1 个）",
                ));
            }
            file_seen = true;
            let fname = field.file_name().unwrap_or("upload.bin").to_string();
            let ct = field.content_type().map(|m| m.to_string());
            let data = field
                .bytes()
                .await
                .map_err(|e| AppError::validation(format!("file 读取失败: {e}")))?
                .to_vec();
            bytes = Some((data, fname, ct));
        } else {
            return Err(AppError::validation(format!(
                "multipart 未知字段: '{name}'（仅接受 'file'）"
            )));
        }
    }
    bytes.ok_or_else(|| AppError::validation("multipart 缺少 'file' 字段"))
}

/// `GET /api/v2/parts/{part_id}/events`
///
/// 事件历史（按 created_at DESC）。
pub async fn list_part_events(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<Vec<PartEventOut>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_events(&mut tx, part_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/location-tree`
///
/// 按 shelf/status 聚合位置树（OFFICE / PRODUCTION_SHELF / WORKER /
/// INSPECTION_SHELF / OUTSOURCE_COMPANY 五区 + 各 holder 子节点）。
pub async fn get_location_tree(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<LocationTreeOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::location_tree(&mut tx, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/match-by-excel-items`
///
/// 用 Excel 序列号清单反查 part 匹配结果（批量导入前置校验）。
pub async fn match_by_excel_items(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<MatchByExcelItemsRequest>,
) -> Result<Json<R<Vec<MatchByExcelItemResult>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::match_by_excel_items(&mut tx, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/parts/batch-update-order-info`
///
/// 批量更新工单 order_no / system_delivery_date / note。
pub async fn batch_update_order_info(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<BatchUpdateOrderInfoRequest>,
) -> Result<Json<R<BatchUpdateOrderInfoOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::batch_update_order_info(&mut tx, &req, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/parts/{part_id}/batches`
///
/// 工单全部活跃批次列表（含 holder 名称 / 下一工序 / 父批次等元信息）。
pub async fn list_part_batches(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<Vec<PartBatchListItemOut>>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartService::list_batches(&mut tx, part_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}
