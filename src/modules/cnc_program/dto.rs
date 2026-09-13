//! cnc_program 域 DTO（2026-09-14 Phase 3）
//!
//! 对应 Python myERP/schema/cnc_program.py。
//!
//! CNC 程序是「配对上传」：一次提交 G_CODE + SETUP_SHEET 两个文件，
//! 形成一对（`paired_file_id` 互相指向对方）。
//!
//! ## id 序列化约定
//! 雪花 i64 字段用 `serialize_i64`（Global Constraint #3）。

use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ---------- 入参 ----------

/// 配对上传请求（multipart `data` JSON 字段）。
///
/// 上传时同时携带 G_CODE + SETUP_SHEET 两个文件，service 端在事务内写两条
/// `t_part_file` 行，`paired_file_id` 互指。允许多次上传形成多版本对，但
/// 简单版（Phase 3）只写一对。
#[derive(Debug, Clone, Deserialize)]
pub struct CncPairUploadRequest {
    pub part_id: String, // 雪花 id
    pub note: Option<String>,
}

// ---------- 出参 ----------

/// 单条 part_file 出参（cnc_program 域复用 part_file 的存储，复用 TPartFile + PartFileOut）。
///
/// 这里给出领域特定包装：成对返回（G_CODE + SETUP_SHEET）。
#[derive(Debug, Clone, Serialize)]
pub struct CncPairOut {
    pub g_code: CncFileRef,
    pub setup_sheet: CncFileRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct CncFileRef {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub kind: String,
    pub file_type: String,
    pub original_filename: String,
    #[serde(serialize_with = "serialize_i64")]
    pub file_size: i64,
    pub content_type: String,
    pub content_sha256: Option<String>,
    pub download_url: String,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub paired_file_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CncPairListItem {
    #[serde(serialize_with = "serialize_i64")]
    pub g_code_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub setup_sheet_id: i64,
    pub g_code_filename: String,
    pub setup_sheet_filename: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct CncPairListOut {
    pub items: Vec<CncPairListItem>,
    pub total: i64,
}