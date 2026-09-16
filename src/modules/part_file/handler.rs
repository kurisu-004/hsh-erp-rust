//! part_file 域 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill + followup-cleanup + 2026-09-16 M2-B 业务层）
//!
//! 对应 Python myERP/api/v1/part_file.py。
//!
//! ## 端点
//! - 挂在 `/api/v2/part-files`（由 `mod.rs::router()` 桥接）：
//!   - `POST /`                            —— 单文件上传（multipart：`data` JSON + `file` 二进制）
//!   - `POST /upload-intents`              —— 一次性签 STS + 预生成 tmp_key（M2-B 新增，场景 A/B）
//!   - `GET  /`                            —— 列表查询 + 分页（`owner_kind` / `owner_id` / `kind` 过滤）
//!   - `GET  /{file_id}/url`               —— 单条详情 + COS 预签下载 URL
//!   - `GET  /{file_id}/content`           —— 后端代理文件内容（Phase 3 补齐）
//!   - `POST /{file_id}/delete`            —— 软删 + COS 异步清理（Phase 3 补齐）
//!
//! - 挂在 `/api/v2/part-files/parts/{part_id}`（由 `part_nested_router()` 提供；
//!   同时也被 `part::router()` 通过 `nest("/parts/{part_id}", ...)` 挂在
//!   `/api/v2/parts/{part_id}` 下，保留历史 URL `POST /parts/{part_id}/cad-files` 等）：
//!   - `POST /cad-files`                   —— 上传 CAD（kind=CAD_2D）
//!   - `POST /cnc-programs` / `GET /cnc-programs`  —— 上传 / 列出 G_CODE
//!   - `POST /setup-sheets` / `GET /setup-sheets`  —— 上传 / 列出 SETUP_SHEET
//!   - `POST /cnc-pair`                    —— 一次提交 G_CODE + SETUP_SHEET
//!   - `GET  /files`                       —— 列出 part 全部文件（kind 可选过滤）
//!
//! 2026-09-15 followup A8：从 `part/handler.rs` 拆过来，原 1491 行单文件降到 1000 行内。
//!
//! ## 约束
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return 时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫）。
//! - COS 客户端从 `state.cos` 拿；handler 注入到 service。
//! - WS 广播：本域不上报 WS 事件（part_file 是只读资产）。

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::cnc_program::service::CncProgramService;
use crate::modules::part_file::dto::{
    PartFileListOut, PartFileListQuery, PartFileOut, PartFileWithUrlOut, UploadIntentsIn,
    UploadIntentsOut,
};
use crate::modules::part_file::service::PartFileService;
use crate::shared::error::{code, AppError};
use crate::shared::response::R;
use crate::state::AppState;

/// 单文件上传的 multipart `data` JSON 字段。
#[derive(Debug, Deserialize)]
pub struct UploadFileData {
    pub owner_kind: String, // PART / ASSEMBLY
    pub owner_id: String,   // 雪花 id 字符串
    pub kind: String,       // DRAWING / 3D_MODEL / ...
}

/// `POST /{file_id}/delete` 入参：版本号 OCC。
#[derive(Debug, Deserialize)]
pub struct DeletePartFileRequest {
    pub version: i32,
}

/// `POST /api/v2/part-files`（multipart） → 201 Created
///
/// body：
/// - `data`：必填文本字段，序列化后的 `UploadFileData` JSON
/// - `file`：必填二进制字段（PDF / STEP / STL 等）
pub async fn upload_part_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<PartFileOut>>), AppError> {
    let mut data_json: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut file_content_type: Option<String> = None;

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
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                file_content_type = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("file 字段读取失败: {e}")))?;
                file_bytes = Some(bytes.to_vec());
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let data_json = data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let data: UploadFileData = serde_json::from_str(&data_json).map_err(|e| {
        AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}"))
    })?;
    let owner_id: i64 = data.owner_id.parse().map_err(|_| {
        AppError::biz(code::BIZ_INVALID_VALUE, format!("owner_id 非法: {}", data.owner_id))
    })?;
    let bytes = file_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 file 字段"))?;
    let filename = file_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少文件名"))?;
    let content_type = file_content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    let mut tx = state.pool.begin().await?;
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        &data.owner_kind,
        owner_id,
        &data.kind,
        &filename,
        &content_type,
        bytes,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `GET /api/v2/part-files` → 200 OK
pub async fn list_part_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<PartFileListQuery>,
) -> Result<Json<R<PartFileListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::list_files(&mut tx, &query, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/part-files/upload-intents` → 200 OK
///
/// 一次性签 STS + 预生成 tmp_key（COS 直传链路入口）。
///
/// - 场景 A（`owner_part_id` 空）：批量预生成；part 还未建，按 batch_uuid 派生 tmp 前缀。
/// - 场景 B（`owner_part_id` 非空）：单 part 补传 / 详情页加文件；同
///   `(owner_id, kind, sha)` 已存在 → `dedup_hit=true` 复用，不分配 tmp_key。
///
/// 权限：Manager + Clerk（service 内 `require_any_role`）。
/// 2026-09-16 M2-B 新增。
pub async fn upload_intents(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UploadIntentsIn>,
) -> Result<Json<R<UploadIntentsOut>>, AppError> {
    let out = PartFileService::upload_intents(
        &state.pool,
        &state.config.cos.tmp_prefix,
        state.sts.clone(),
        &req,
        &current,
    )
    .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/part-files/{file_id}/url` → 200 OK
pub async fn get_part_file_url(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Json<R<PartFileWithUrlOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::get_file_with_url(&mut tx, state.cos.clone(), file_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/part-files/{file_id}/content` —— 后端代理文件二进制流。
///
/// 拉 `t_part_file.object_key` + COS `get_object`，透传 `content_type` 头。
/// 权限：与 `get_part_file_url` 一致（任意已登录可读）。
pub async fn get_part_file_content(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Response, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::get_file_content(
        &mut tx,
        state.cos.clone(),
        file_id,
        &current,
    )
    .await?;
    tx.commit().await?;
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            out.content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
        )
        .header(header::CONTENT_LENGTH, out.bytes.len())
        .body(Body::from(out.bytes))
        .map_err(|e| AppError::internal(format!("response build: {e}")))?;
    Ok(resp)
}

/// `POST /api/v2/part-files/{file_id}/delete` —— 软删 + COS 异步清理。
///
/// 入参（JSON）：`{ version: i32 }`（OCC）。
/// 权限：按 kind 派生角色（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；
/// G_CODE → M+CNC）。
///
/// 2026-09-15 review A2 修：service 返回 `object_key` 后，handler 必须
/// 先 `tx.commit()` 再 `tokio::spawn(cos.delete_object(...))`，避免
/// commit 失败却已触发 COS 删除、产生孤儿对象。
pub async fn soft_delete_part_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
    Json(req): Json<DeletePartFileRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let object_key = PartFileService::soft_delete_file(
        &mut tx,
        state.cos.clone(),
        file_id,
        req.version,
        &current,
    )
    .await?;
    // commit 在前：DB 已是最终态，再触发副作用
    tx.commit().await?;
    // commit 后再异步触发 COS 删除（best-effort，失败仅 warn）
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&object_key).await {
            tracing::warn!(
                key = %object_key,
                error = %e,
                "part_file COS 异步清理失败（已软删，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok_empty()))
}

/// part_file 域 axum 子路由。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(upload_part_file).get(list_part_files))
        // 2026-09-16 M2-B 新增：直传 COS 入口。
        // POST `/upload-intents` 与 POST `/` 同 method 但不同 path，axum 允许共存。
        .route("/upload-intents", post(upload_intents))
        .route("/{file_id}/url", get(get_part_file_url))
        .route("/{file_id}/content", get(get_part_file_content))
        .route("/{file_id}/delete", post(soft_delete_part_file))
        // 2026-09-15 followup-cleanup A8：原 part/handler.rs 的 7 个 part 维度
        // 文件路由（`POST /parts/{part_id}/cad-files` 等）也通过 `/part-files/parts/{part_id}`
        // 路径对外暴露，便于前端 / 第三方客户端不依赖 parts 入口也能命中。
        .nest("/parts/{part_id}", part_nested_router())
}

// ===========================================================================
// 2026-09-15 followup-cleanup A8：从 part/handler.rs 拆出的 part 维度文件路由
// ===========================================================================

/// `GET /api/v2/parts/{part_id}/files` —— 列出 part 文件（kind 可选过滤）。
#[derive(Debug, serde::Deserialize)]
pub struct PartFilesQuery {
    pub kind: Option<String>,
}

/// 内部辅助：解析 multipart `file` 字段（与 upload_drawing / upload_3d_model 同形）。
///
/// 2026-09-15 followup-cleanup A10：错误码统一为 `AppError::validation(...)`
/// （原 content_type 缺失走 `BIZ_PART_FILE_BAD_TYPE` 与其他 multipart 错误
/// 语义不一致，且缺 content_type 本质是客户端未填字段而非「类型不合法」）。
async fn read_part_file_multipart(
    mut multipart: Multipart,
) -> Result<(Vec<u8>, String, String), AppError> {
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
                return Err(AppError::validation("multipart 包含多个 'file' 字段"));
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
    let (data, fname, ct) = bytes.ok_or_else(|| AppError::validation("multipart 缺少 'file' 字段"))?;
    let ct = ct.ok_or_else(|| AppError::validation("file 缺少 content_type"))?;
    Ok((data, fname, ct))
}

/// `POST /parts/{part_id}/cad-files` —— 上传 CAD 文件（kind=CAD_2D）。
pub async fn upload_cad_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    multipart: Multipart,
) -> Result<Json<R<PartFileOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::CncProgrammer])?;
    let (data, fname, ct) = read_part_file_multipart(multipart).await?;
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        "PART",
        part_id,
        "CAD_2D",
        &fname,
        &ct,
        data,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /parts/{part_id}/cnc-programs` —— 上传 G_CODE（kind=G_CODE；M+CNC）。
pub async fn upload_cnc_program(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    multipart: Multipart,
) -> Result<Json<R<PartFileOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
    let (data, fname, ct) = read_part_file_multipart(multipart).await?;
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        "PART",
        part_id,
        "G_CODE",
        &fname,
        &ct,
        data,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /parts/{part_id}/setup-sheets` —— 上传工艺卡 PDF（kind=SETUP_SHEET；M+CNC）。
pub async fn upload_setup_sheet(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    multipart: Multipart,
) -> Result<Json<R<PartFileOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
    let (data, fname, ct) = read_part_file_multipart(multipart).await?;
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        "PART",
        part_id,
        "SETUP_SHEET",
        &fname,
        &ct,
        data,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /parts/{part_id}/cnc-pair` —— 一次提交 G_CODE + SETUP_SHEET。
pub async fn upload_cnc_pair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    mut multipart: Multipart,
) -> Result<Json<R<crate::modules::cnc_program::dto::CncPairOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
    let mut g: Option<(Vec<u8>, String, String)> = None;
    let mut s: Option<(Vec<u8>, String, String)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().unwrap_or("upload.bin").to_string();
        let ct = field
            .content_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let bytes = field
            .bytes()
            .await
            .map_err(|e| AppError::validation(format!("{name} 读取失败: {e}")))?
            .to_vec();
        match name.as_str() {
            "g_code" => g = Some((bytes, fname, ct)),
            "setup_sheet" => s = Some((bytes, fname, ct)),
            _ => {
                return Err(AppError::validation(format!(
                    "multipart 未知字段: '{name}'（仅接受 'g_code' / 'setup_sheet'）"
                )))
            }
        }
    }
    let (g_bytes, g_name, g_ct) = g.ok_or_else(|| AppError::validation("缺少 g_code 字段"))?;
    let (s_bytes, s_name, s_ct) = s.ok_or_else(|| AppError::validation("缺少 setup_sheet 字段"))?;
    let mut tx = state.pool.begin().await?;
    let out = CncProgramService::upload_cnc_pair(
        &mut tx,
        &state.snowflake,
        state.cos.clone(),
        part_id,
        g_bytes,
        &g_name,
        &g_ct,
        s_bytes,
        &s_name,
        &s_ct,
        &current,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /parts/{part_id}/files` —— 列出 part 全部文件（kind 可选过滤）。
pub async fn list_part_files_for_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
    Query(query): Query<PartFilesQuery>,
) -> Result<Json<R<PartFileListOut>>, AppError> {
    current.require_any_role(&[
        Role::Manager,
        Role::Clerk,
        Role::Inspector,
        Role::CncProgrammer,
    ])?;
    let q = PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: query.kind,
        limit: Some(500),
        offset: Some(0),
    };
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::list_files(&mut tx, &q, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /parts/{part_id}/cnc-programs` —— 列出 part 下 G_CODE 文件。
pub async fn list_part_cnc_programs(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<PartFileListOut>>, AppError> {
    let q = PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: Some("G_CODE".into()),
        limit: Some(500),
        offset: Some(0),
    };
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::list_files(&mut tx, &q, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /parts/{part_id}/setup-sheets` —— 列出 part 下 SETUP_SHEET 文件。
pub async fn list_part_setup_sheets(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<PartFileListOut>>, AppError> {
    let q = PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: Some("SETUP_SHEET".into()),
        limit: Some(500),
        offset: Some(0),
    };
    let mut tx = state.pool.begin().await?;
    let out = PartFileService::list_files(&mut tx, &q, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// part 维度文件路由：被 `part::router()` 与 `part_file::router()` 各 nest 一次，
/// 路径前缀分别为 `/parts/{part_id}` 与 `/part-files/parts/{part_id}`。
pub fn part_nested_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/cad-files", post(upload_cad_files))
        .route(
            "/cnc-programs",
            post(upload_cnc_program).get(list_part_cnc_programs),
        )
        .route(
            "/setup-sheets",
            post(upload_setup_sheet).get(list_part_setup_sheets),
        )
        .route("/cnc-pair", post(upload_cnc_pair))
        .route("/files", get(list_part_files_for_part))
}