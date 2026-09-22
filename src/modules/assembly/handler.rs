//! assembly 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/assembly.py（设计 §7 — assembly CRUD）。
//!
//! ## 端点（挂在 `/api/v2/assemblies`，由 `mod.rs::router()` 桥接）
//! - `GET  /`                              —— 列表查询 + 分页
//! - `POST /`                              —— 创建（multipart：`data` + `files`）
//! - `GET  /{assembly_id}`                 —— 详情（含 children + files 占位）
//! - `POST /{assembly_id}/update`          —— 字段可选 UPDATE（OCC）
//! - `POST /{assembly_id}/soft-delete`     —— Manager 软删（OCC）
//! - `POST /{assembly_id}/cancel`          —— Manager/Clerk 取消
//! - `POST /{assembly_id}/start`           —— PENDING → IN_PROCESS 状态机守卫
//! - `POST /{assembly_id}/files`           —— multipart PDF 上传到 COS
//!
//! ## 约定（2026-09-22 Group D-3 重构对齐 iam 范本）
//! - 事务边界在 handler：`state.pool.begin()` → service（trait 注入式签名
//!   `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)` 收 `&mut tx`）→ 显式
//!   `tx.commit()`；提前 return 时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_role(...)` 守卫）。
//! - WS 广播在 commit 之后（对齐 Python 延迟广播模式）；service 不直接调 ws_hub。
//!
//! ## handler 三形态
//! - ① 纯写端点：`pool.begin() → service → commit`，再广播。
//! - ② 读端点（list / get）：`pool.acquire() → service`，不开事务。
//! - ③ multipart 上传（create / files）：`pool.begin() → service → commit`；commit 后
//!   广播（仅 create）。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::assembly::dto::{
    AssemblyCreateRequest, AssemblyListQuery, AssemblyUpdateRequest,
};
use crate::modules::assembly::service::AssemblyService;
use crate::modules::assembly::vo::{
    AssemblyCreateResult, AssemblyDetail, AssemblyListOut, AssemblyOut,
};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// 软删 / 取消的乐观锁版本号 body。
#[derive(Debug, Deserialize)]
pub struct VersionBody {
    pub version: i32,
}

/// GET /api/v2/assemblies
///
/// 列表查询 + 分页（service 内已校验角色）。读端点，不开事务。
///
/// 2026-09-22 D-3：handler 直接走 `AssemblyService::list_assemblies` 的 ZST 静态
/// wrapper（接受 `&mut PgConnection`），与既存集成测试一致。
pub async fn list_assemblies(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<AssemblyListQuery>,
) -> Result<Json<R<AssemblyListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = AssemblyService::list_assemblies(&mut conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/assemblies/{assembly_id}
///
/// 单条详情（assembly 行 + children parts + files 占位空数组）。读端点，不开事务。
pub async fn get_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyDetail>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = AssemblyService::get_assembly(&mut conn, assembly_id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/assemblies → 201 Created
///
/// multipart body：
/// - `data`：必填，文本字段，序列化后的 `AssemblyCreateRequest` JSON；
/// - `files`：可选，多个 PDF 二进制字段（首份会被 service 用作页数校验）。
///
/// 行为：
/// - 业务流转：service `create_assembly`（L2 customer 校验 + 子件上限 +
///   PDF 页数 == children.len()+1 + serial 派发 + children INSERT）。
/// - WS 广播：commit 后 `ASSEMBLY_CREATED`。
/// - 响应：`AssemblyCreateResult { assembly, created_children }`。
pub async fn create_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<AssemblyCreateResult>>), AppError> {
    let mut data_json: Option<String> = None;
    let mut pdf_files: Vec<Vec<u8>> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "data" => {
                data_json = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::validation(format!("data 字段读取失败: {e}")))?,
                );
            }
            "files" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("files 字段读取失败: {e}")))?;
                pdf_files.push(bytes.to_vec());
            }
            // 其它字段一律丢弃（与 Python 行为对齐）
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let data_json =
        data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let req: AssemblyCreateRequest = serde_json::from_str(&data_json)
        .map_err(|e| AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}")))?;

    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::create_assembly(
        &mut tx,
        &state.snowflake,
        &req,
        pdf_files,
        &current,
    )
    .await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_CREATED".into(),
        payload: json!({ "assembly_id": out.assembly.id.to_string() }),
    });
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/assemblies/{assembly_id}/update
///
/// 字段可选 UPDATE（含 customer_id 三态校验 + L2 校验）；OCC 通过 `req.version` 守。
///
/// 行为：
/// - 权限：Manager / Clerk（service 内守卫）
/// - WS 广播：commit 后 `ASSEMBLY_UPDATED`
/// - 响应：`AssemblyOut`
pub async fn update_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<AssemblyUpdateRequest>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::update_assembly(&mut tx, assembly_id, &req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/assemblies/{assembly_id}/soft-delete
///
/// Manager 专属软删；OCC 守；commit 后广播 `ASSEMBLY_DELETED`。
pub async fn soft_delete_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<VersionBody>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    AssemblyService::soft_delete_assembly(&mut tx, assembly_id, req.version, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_DELETED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/assemblies/{assembly_id}/cancel
///
/// Manager / Clerk；repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫，
/// 命中 0 行 → 终态禁 cancel（返回 `BIZ_INVALID_TRANSITION`）。
///
/// 行为：
/// - 权限：Manager / Clerk（service 内守卫）
/// - WS 广播：commit 后 `ASSEMBLY_CANCELLED`
/// - 响应：`AssemblyOut`
pub async fn cancel_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::cancel_assembly(&mut tx, assembly_id, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_CANCELLED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/assemblies/{assembly_id}/start` → 200 OK
///
/// 2026-09-14 Phase 3（deferred #4）：PENDING → IN_PROCESS 状态机守卫。
pub async fn start_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::start_assembly(&mut tx, assembly_id, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/assemblies/{assembly_id}/files` → 201 Created
///
/// 2026-09-14 Phase 3（deferred #1）：multipart PDF 上传到 COS。
/// body: 多个 `file` 二进制字段（PDF）。
pub async fn upload_assembly_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    mut multipart: Multipart,
) -> Result<
    (
        StatusCode,
        Json<R<Vec<crate::modules::assembly::vo::AssemblyFileRef>>>,
    ),
    AppError,
> {
    let mut files: Vec<(Vec<u8>, String, String)> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                let filename = field
                    .file_name()
                    .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "file 缺少文件名"))?
                    .to_string();
                let content_type = field
                    .content_type()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "application/pdf".to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("file 字段读取失败: {e}")))?
                    .to_vec();
                files.push((bytes, filename, content_type));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    if files.is_empty() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "至少需要一个 file 字段",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::upload_assembly_files(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        assembly_id,
        files,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// assembly 域 axum 子路由（不含公共前缀；由 `mod.rs::router()` 桥接到 `/assemblies`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_assemblies).post(create_assembly))
        .route("/{assembly_id}", get(get_assembly))
        .route("/{assembly_id}/update", post(update_assembly))
        .route("/{assembly_id}/soft-delete", post(soft_delete_assembly))
        .route("/{assembly_id}/cancel", post(cancel_assembly))
        .route("/{assembly_id}/start", post(start_assembly))
        .route("/{assembly_id}/files", post(upload_assembly_files))
}
