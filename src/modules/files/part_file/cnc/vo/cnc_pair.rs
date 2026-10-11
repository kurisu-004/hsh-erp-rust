//! `part_file::cnc` 子模块配对端点响应 VO（2026-09-22 PR4 重构，2026-10-13 收编）
//!
//! 2026-10-13：`CncPairListItem` / `CncPairListOut` 随 `GET /cnc-programs/parts/{id}`
//! 配对列表端点下线一并删除（零前端调用方）。配对列表改由 `parent` 的对象列表端点
//! `GET /files/objects?...&kind=G_CODE` 承担，其列表项 `PartFileOut` 已投影
//! `paired_file_id`。

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
