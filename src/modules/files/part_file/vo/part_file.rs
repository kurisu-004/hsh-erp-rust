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

/// `POST /api/v2/files/objects/upload-intents` 出参（逐项对应入参，顺序一致）。
///
/// 2026-10-15 新增：每个 item 有两种互斥形态 ——
///
/// | `already_exists` | 含义 | 前端动作 |
/// |---|---|---|
/// | `true` | 同 owner + kind + 同 sha 已有**活跃行**（秒传） | 跳过 PUT 与 confirm，直接用 `existing_file` |
/// | `false` | 需新传 | PUT 到 `upload_url`（头带 `content_type`），再 confirm |
///
/// 两种形态的字段用 `skip_serializing_if` 互斥省略：秒传项**不会**带
/// `upload_url`（服务端没签、也不该让前端误 PUT），新传项不带 `existing_file`。
#[derive(Debug, Clone, Serialize)]
pub struct UploadIntentsOut {
    pub items: Vec<UploadIntentItemOut>,
}

/// 单个签发结果（见 [`UploadIntentsOut`] 的两态表）。
#[derive(Debug, Clone, Serialize)]
pub struct UploadIntentItemOut {
    /// 入参 `client_ref` 原样回显。
    pub client_ref: String,
    pub already_exists: bool,
    /// 正式 CAS key（`{upload_prefix}{sha16}_{safe_filename}`）；秒传时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_key: Option<String>,
    /// 预签 PUT URL；秒传时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    /// 前端 PUT 时必须原样带上；秒传时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// URL 剩余有效期（秒）；秒传项同样回（值为配置值，前端不用看）。
    pub expires_in: u32,
    /// 秒传命中的既有行（`PartFileOut` 全字段）；需新传时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_file: Option<PartFileOut>,
}
