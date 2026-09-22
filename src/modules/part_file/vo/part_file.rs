//! part_file 域 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// 单条 part_file 出参（`TPartFile` 完整投影）。
///
/// `content_sha256` / `paired_file_id` 可空；owner 是 polymorphic part/assembly。
#[derive(Debug, Clone, Serialize)]
pub struct PartFileOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// polymorphic owner id（part.id 或 assembly.id）。
    #[serde(serialize_with = "serialize_i64")]
    pub owner_id: i64,
    pub owner_kind: String, // "PART" / "ASSEMBLY"
    pub kind: String,
    pub file_type: String,
    pub object_key: String,
    pub original_filename: String,
    #[serde(serialize_with = "serialize_i64")]
    pub file_size: i64,
    pub content_type: String,
    pub upload_status: String,
    pub content_sha256: Option<String>,
    /// CNC 配对文件 id（G_CODE <-> SETUP_SHEET 互指）；未配对为 null。
    /// JSON 序列化为 string（雪花 id，防 JS 精度截断）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub paired_file_id: Option<i64>,
    pub version: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<NaiveDateTime>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
}

/// 单条 part_file 详情 + COS 预签下载 URL。
///
/// 服务端即时拼 URL（默认 1h 有效期），前端无需关心签名逻辑。
#[derive(Debug, Clone, Serialize)]
pub struct PartFileWithUrlOut {
    /// 雪花 ID 序列化为字符串防 JS 精度截断（2026-09-22 PR4 重构：从 i64 改 String）
    pub id: String,
    pub kind: String,
    pub file_type: String,
    pub original_filename: String,
    pub file_size: i64,
    pub content_type: String,
    pub content_sha256: Option<String>,
    pub upload_status: String,
    pub download_url: String,
    pub url_expires_in_seconds: u32,
}

/// part_file 列表出参（按 owner + kind 过滤；纯 list）。
#[derive(Debug, Clone, Serialize)]
pub struct PartFileListOut {
    pub items: Vec<PartFileOut>,
    pub total: i64,
}
