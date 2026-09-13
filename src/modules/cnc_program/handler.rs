//! cnc_program 域 HTTP handler（2026-09-14 Phase 3）
//!
//! 对应 Python myERP/api/v1/cnc_program.py。
//!
//! ## 端点（挂在 `/api/v2/cnc-programs`，由 `mod.rs::router()` 桥接）
//! - `POST /pairs`             —— 配对上传（multipart：`data` JSON + `g_code` + `setup_sheet` 二进制）
//! - `GET  /parts/{part_id}`   —— 列出 part 全部 CNC 配对
//!
//! ## 约束
//! - 事务边界在 handler
//! - 统一响应信封
//! - 权限在 service 层
//! - WS 广播：本域不上报 WS 事件

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::cnc_program::dto::{CncPairListOut, CncPairOut};
use crate::modules::cnc_program::service::CncProgramService;
use crate::shared::error::{code, AppError};
use crate::shared::response::R;
use crate::state::AppState;

/// 配对上传 multipart `data` JSON 字段。
#[derive(Debug, Deserialize)]
pub struct PairUploadData {
    pub part_id: String, // 雪花 id
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v2/cnc-programs/pairs`（multipart） → 201 Created
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

    let data_json = data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let data: PairUploadData = serde_json::from_str(&data_json).map_err(|e| {
        AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}"))
    })?;
    let part_id: i64 = data.part_id.parse().map_err(|_| {
        AppError::biz(code::BIZ_INVALID_VALUE, format!("part_id 非法: {}", data.part_id))
    })?;
    let g_bytes = g_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 g_code 字段"))?;
    let g_name = g_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 g_code 文件名"))?;
    let g_ct = g_ct.unwrap_or_else(|| "application/octet-stream".to_string());
    let s_bytes = s_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 setup_sheet 字段"))?;
    let s_name = s_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 setup_sheet 文件名"))?;
    let s_ct = s_ct.unwrap_or_else(|| "application/pdf".to_string());

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
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `GET /api/v2/cnc-programs/parts/{part_id}` → 200 OK
pub async fn list_pairs_for_part(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(part_id): Path<i64>,
) -> Result<Json<R<CncPairListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = CncProgramService::list_pairs_for_part(&mut tx, state.cos.clone(), part_id, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pairs", post(upload_cnc_pair))
        .route("/parts/{part_id}", get(list_pairs_for_part))
}