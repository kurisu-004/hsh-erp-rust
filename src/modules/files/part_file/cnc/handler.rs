//! `part_file::cnc` 子模块 HTTP handler
//!
//! 对应 Python myERP/api/v1/cnc_program.py。
//!
//! ## 端点（1 条，挂在 `/api/v2/files/cnc`，由 `mod.rs::router()` 桥接）
//! - `POST /pairs` —— 配对上传（multipart：`data` JSON + `g_code` + `setup_sheet` 二进制）
//!
//! 2026-10-13 收编 + 清理：URL 自 `/api/v2/cnc-programs/pairs` 硬切到
//! `/api/v2/files/cnc/pairs`（无 alias，旧前缀 404）；同批下线的 4 条端点
//! （`GET /parts/{part_id}` 与 3 条 alias）见 `mod.rs` 模块 doc 的移除记录。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式，2026-10-13 收编时不变）
//! handler 负责 `pool.begin()` / `tx.commit()` —— service 不知事务：
//! `pool.begin()` → `state.cnc_program_service.upload_cnc_pair(&mut tx, ...)` →
//! `tx.commit()`。
//!
//! service 仅业务逻辑（方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)` ——
//! 2026-10-13 起直接用父模块 `part_file` 的 repo trait，本子模块不再有自己的 repo 层；
//! trait 已对 `&mut PgConnection` 实现），handler 借 `&mut *tx` 喂给 trait。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫，Manager / CncProgrammer）。
//! - WS 广播：本子模块不上报 WS 事件。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Multipart, State},
    http::StatusCode,
    routing::post,
};
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::files::part_file::cnc::vo::CncPairOut;
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

/// `POST /api/v2/files/cnc/pairs`（multipart） → 201 Created
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
            &mut *tx, part_id, g_bytes, &g_name, &g_ct, s_bytes, &s_name, &s_ct, &current,
        )
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// CNC 配对子路由。2026-10-13 起只剩 `/pairs` 一条（原 `/{file_id}/*` 三条 alias 与
/// `/parts/{part_id}` 列表端点随零调用方清理下线，见 `mod.rs` 模块 doc）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/pairs", post(upload_cnc_pair))
}
