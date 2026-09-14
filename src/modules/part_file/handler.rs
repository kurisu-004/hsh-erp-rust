//! part_file 域 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill）
//!
//! 对应 Python myERP/api/v1/part_file.py。
//!
//! ## 端点（挂在 `/api/v2/part-files`，由 `mod.rs::router()` 桥接）
//! - `POST /`                            —— 单文件上传（multipart：`data` JSON + `file` 二进制）
//! - `GET  /`                            —— 列表查询 + 分页（`owner_kind` / `owner_id` / `kind` 过滤）
//! - `GET  /{file_id}/url`               —— 单条详情 + COS 预签下载 URL
//! - `GET  /{file_id}/content`           —— 后端代理文件内容（Phase 3 补齐）
//! - `POST /{file_id}/delete`            —— 软删 + COS 异步清理（Phase 3 补齐）
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

use crate::auth::rbac::CurrentUser;
use crate::modules::part_file::dto::{PartFileListQuery, PartFileListOut, PartFileOut, PartFileWithUrlOut};
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
        .route("/{file_id}/url", get(get_part_file_url))
        .route("/{file_id}/content", get(get_part_file_content))
        .route("/{file_id}/delete", post(soft_delete_part_file))
}