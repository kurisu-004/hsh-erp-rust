//! part_file 域 DTO
//!
//! 对应 Python myERP/schema/part_file.py。
//!
//! ## id 序列化约定
//! 雪花 i64 字段用 `serialize_i64`（Global Constraint #3）。

use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ---------- 出参 ----------

/// 单条 part_file 出参（`TPartFile` 完整投影）。
///
/// `content_sha256` / `paired_file_id` 可空；owner 是 polymorphic part/assembly。
/// 2026-09-14 新增。
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
    pub version: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::NaiveDateTime>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
}

/// 单条 part_file 详情 + COS 预签下载 URL。
///
/// 2026-09-14 新增：服务端即时拼 URL（默认 1h 有效期），前端无需关心签名逻辑。
#[derive(Debug, Clone, Serialize)]
pub struct PartFileWithUrlOut {
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

// ---------- 入参 ----------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PartFileListQuery {
    #[serde(default)]
    pub owner_kind: Option<String>, // PART / ASSEMBLY
    #[serde(default)]
    pub owner_id: Option<String>,   // 雪花 id（String 形式；service 层 parse i64）
    #[serde(default)]
    pub kind: Option<String>,       // DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}