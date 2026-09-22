//! cnc_program 域 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill + 2026-09-22 对齐 iam 范式）
//!
//! 对应 Python myERP/api/v1/cnc_program.py。
//!
//! ## 端点（挂在 `/api/v2/cnc-programs`，由 `mod.rs::router()` 桥接）
//! - `POST /pairs`                          —— 配对上传（multipart：`data` JSON + `g_code` + `setup_sheet` 二进制）
//! - `GET  /parts/{part_id}`                —— 列出 part 全部 CNC 配对
//! - `GET  /{file_id}/download-url`         —— alias → `state.part_file_service.get_file_with_url(&mut tx, ...)`
//! - `GET  /{file_id}/content`              —— alias → `state.part_file_service.get_file_content(&mut tx, ...)`
//! - `POST /{file_id}/delete`               —— alias → `state.part_file_service.soft_delete_file(&mut tx, ...)`
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式）
//! handler 负责 `pool.begin()` / `tx.commit()` —— service 不知事务：
//! - ① **纯写端点**（upload_cnc_pair）：`pool.begin()` →
//!   `state.cnc_program_service.upload_cnc_pair(&mut tx, ...)` → `tx.commit()`。
//! - ② **写 + post-commit 副作用**（delete_cnc_program 形态 ②：alias 转发到
//!   `state.part_file_service.soft_delete_file`，由 part_file handler 模式收尾；
//!   cnc handler 仅透传 + commit + spawn COS delete）。
//! - ③ **读端点**（list_pairs_for_part / get_cnc_program_download_url /
//!   get_cnc_program_content）：`pool.begin()` → service 跑查询 → `tx.commit()`。
//!
//! 3 个 alias 端点（download-url / content / delete）直接转发到
//! `state.part_file_service.method(&mut tx, ...)`——handler 借 `&mut *conn` 喂
//! `PartFileService`，**不**在 `CncProgramService` 上挂 alias 方法（避免 handler 收
//! 两个 service；与 shelf picker 跨域 helper 模式区分，spec §C-1 决策点 1）。
//!
//! service 仅业务逻辑（方法签名 `<R: CncProgramRepoTrait>(&self, mut repo: R, ...)` /
//! `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，trait 已对 `&mut PgConnection`
//! 实现），handler/service 借 `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫）。
//! - WS 广播：本域不上报 WS 事件。

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::cnc_program::vo::{CncPairListOut, CncPairOut};
use crate::modules::part_file::vo::PartFileWithUrlOut;
use crate::modules::part_file::handler::DeletePartFileRequest;
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// 配对上传 multipart `data` JSON 字段。
#[derive(Debug, Deserialize)]
pub struct PairUploadData {
    pub part_id: String, // 雪花 id
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/cnc-programs/pairs`（multipart） → 201 Created（形态 ①）
pub async fn upload_cnc_pair(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<CncPairOut>>), AppError> {
    let mut data_json: Option<String> = None;
    let mut g_bytes: Option<Vec<u8>> = None;
    let mut g_name: Option<String> = None;
    let mut g_ct: Option<String> = None;
    let mut s_bytes: Option<Vec<u8>> = None;
    let mut s_name: Option<String> = None;
    let mut s_ct: Option<String> = None;

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
            "g_code" => {
                g_name = field.file_name().map(|s| s.to_string());
                g_ct = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("g_code 读取失败: {e}")))?;
                g_bytes = Some(bytes.to_vec());
            }
            "setup_sheet" => {
                s_name = field.file_name().map(|s| s.to_string());
                s_ct = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("setup_sheet 读取失败: {e}")))?;
                s_bytes = Some(bytes.to_vec());
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let data_json =
        data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let data: PairUploadData = serde_json::from_str(&data_json)
        .map_err(|e| AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}")))?;
    let part_id: i64 = data.part_id.parse().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("part_id 非法: {}", data.part_id),
        )
    })?;
    let g_bytes =
        g_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 g_code 字段"))?;
    let g_name =
        g_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 g_code 文件名"))?;
    let g_ct = g_ct.unwrap_or_else(|| "application/octet-stream".to_string());
    let s_bytes =
        s_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 setup_sheet 字段"))?;
    let s_name =
        s_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 setup_sheet 文件名"))?;
    let s_ct = s_ct.unwrap_or_else(|| "application/pdf".to_string());

    let mut tx = state.pool.begin().await?;
    let out = state
        .cnc_program_service
        .upload_cnc_pair(
            &mut *tx,
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
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `GET /api/v2/cnc-programs/parts/{part_id}` → 200 OK（形态 ③）
pub async fn list_pairs_for_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<CncPairListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .cnc_program_service
        .list_pairs_for_part(&mut *tx, part_id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/cnc-programs/{file_id}/download-url` —— alias → part_files/{id}/url（形态 ③）
///
/// 2026-09-22 重构：handler 直接转发到 `state.part_file_service`，不经过
/// `CncProgramService`（避免跨域 service 委托；与 shelf picker 跨域 helper 模式区分）。
pub async fn get_cnc_program_download_url(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Json<R<PartFileWithUrlOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .get_file_with_url(&mut *tx, state.cos.clone(), file_id, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/cnc-programs/{file_id}/content` —— alias → part_files/{id}/content（形态 ③）
///
/// 2026-09-22 重构：handler 直接转发到 `state.part_file_service`。
pub async fn get_cnc_program_content(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Response, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .get_file_content(&mut *tx, state.cos.clone(), file_id, &current)
        .await?;
    tx.commit().await?;
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            out.content_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        )
        .header(header::CONTENT_LENGTH, out.bytes.len())
        .body(Body::from(out.bytes))
        .map_err(|e| AppError::internal(format!("response build: {e}")))?;
    Ok(resp)
}

/// `POST /api/v2/cnc-programs/{file_id}/delete` —— alias → part_files/{id}/delete（形态 ②）
///
/// 2026-09-15 review A2 修：commit 在前，spawn COS 在后（与 part_file handler 同 pattern）。
/// 2026-09-22 重构：handler 直接转发到 `state.part_file_service`。
pub async fn delete_cnc_program(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
    Json(req): Json<DeletePartFileRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let object_key = state
        .part_file_service
        .soft_delete_file(&mut *tx, file_id, req.version, &current)
        .await?;
    tx.commit().await?;
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&object_key).await {
            tracing::warn!(
                key = %object_key,
                error = %e,
                "cnc_program COS 异步清理失败（已软删，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok_empty()))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pairs", post(upload_cnc_pair))
        // 静态段在 {file_id} 之前，避免被 catch-all 截胡
        .route("/parts/{part_id}", get(list_pairs_for_part))
        .route("/{file_id}/download-url", get(get_cnc_program_download_url))
        .route("/{file_id}/content", get(get_cnc_program_content))
        .route("/{file_id}/delete", post(delete_cnc_program))
}