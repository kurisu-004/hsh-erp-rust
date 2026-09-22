//! cnc_program 域 配对端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// CNC 配对单文件引用（G_CODE 或 SETUP_SHEET 中的一个）
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

/// CNC 配对出参（G_CODE + SETUP_SHEET 配对）
#[derive(Debug, Clone, Serialize)]
pub struct CncPairOut {
    pub g_code: CncFileRef,
    pub setup_sheet: CncFileRef,
}

/// CNC 配对列表项
#[derive(Debug, Clone, Serialize)]
pub struct CncPairListItem {
    #[serde(serialize_with = "serialize_i64")]
    pub g_code_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub setup_sheet_id: i64,
    pub g_code_filename: String,
    pub setup_sheet_filename: String,
    pub created_at: NaiveDateTime,
}

/// CNC 配对列表出参
#[derive(Debug, Clone, Serialize)]
pub struct CncPairListOut {
    pub items: Vec<CncPairListItem>,
    pub total: i64,
}
