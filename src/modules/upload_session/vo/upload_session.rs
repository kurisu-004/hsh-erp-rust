//! upload_session 域响应 VO（HTTP 出参隔离层）
//!
//! 2026-09-22 PR4：从 `dto.rs` 拆出全部 Out 类型（Serialize-only）。
//!
//! - `SessionCredentialsOut` / `SessionFileOut`：分别镜像内部结构
//!   `dto::SessionCredentials` / `dto::SessionFile`（Redis JSON 值），仅 Serialize。
//! - `GetOrCreateOut` / `AllocateFilesOut` / `AllocateFileItemOut` /
//!   `CompleteFileOut` / `RemoveFilesOut` / `RenewOut` / `ConsumeFilesOut` /
//!   `DiscardOut`：7 个端点出参，按端点命名一一对应。
//!
//! ## i64 ID 序列化策略
//! 本域**无雪花 ID** 出参——所有 `i64` 字段均为业务 unix 秒（`start_time` /
//! `expired_time` / `expires_in` / `file_size`），量级 ≤ 2^53，按默认 serde 行为
//! 走 JSON number，不强制 `serialize_i64` 字符串化。

use serde::Serialize;

use super::super::dto::{SessionCredentials, SessionFile};

/// STS 凭证出参（`UploadSession.credentials` 元素 + `get_or_create` / `renew`
/// 响应中 `credentials` 复用）。
///
/// 与 `part_file::CosCredentialsOut` 字段一致但类型不同：`start_time` /
/// `expired_time` 走 i64 默认 JSON number（unix 秒远小于 2^53，不沿用雪花 ID
/// "string 防 JS 精度截断"策略）。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionCredentialsOut {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    pub start_time: i64,
    pub expired_time: i64,
}

impl From<&SessionCredentials> for SessionCredentialsOut {
    fn from(c: &SessionCredentials) -> Self {
        Self {
            tmp_secret_id: c.tmp_secret_id.clone(),
            tmp_secret_key: c.tmp_secret_key.clone(),
            session_token: c.session_token.clone(),
            start_time: c.start_time,
            expired_time: c.expired_time,
        }
    }
}

/// 单条文件状态出参（`GetOrCreateOut.files` 元素 + `complete_file` 返回值）。
///
/// 字段对齐 `dto::SessionFile`（内部 Redis JSON 结构），仅 Serialize。
/// `etag` 是 COS HEAD 返回的 hex md5（带引号）；`uploaded_at` 是 ISO 8601 UTC 字符串。
///
/// 2026-09-22 PR4 新增（之前 `GetOrCreateOut.files` 直接复用内部 `SessionFile`，
/// 内部结构被迫双向 Serialize；本次拆出后内部结构可仅保持 Deserialize）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionFileOut {
    pub client_ref: String,
    /// "drawing" / "3d_model"（小写）。
    pub kind: String,
    pub original_filename: String,
    pub file_size: i64,
    pub content_type: String,
    /// 64 hex chars。
    pub content_sha256: String,
    pub tmp_key: String,
    /// "pending" / "done" / "error"。
    pub status: String,
    /// COS HEAD 返回的 ETag；未完成时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// ISO 8601 UTC 字符串（前端直接展示）；`done` 时填，其余为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_at: Option<String>,
}

impl From<&SessionFile> for SessionFileOut {
    fn from(f: &SessionFile) -> Self {
        Self {
            client_ref: f.client_ref.clone(),
            kind: f.kind.clone(),
            original_filename: f.original_filename.clone(),
            file_size: f.file_size,
            content_type: f.content_type.clone(),
            content_sha256: f.content_sha256.clone(),
            tmp_key: f.tmp_key.clone(),
            status: f.status.clone(),
            etag: f.etag.clone(),
            uploaded_at: f.uploaded_at.clone(),
        }
    }
}

impl From<SessionFile> for SessionFileOut {
    fn from(f: SessionFile) -> Self {
        Self {
            client_ref: f.client_ref,
            kind: f.kind,
            original_filename: f.original_filename,
            file_size: f.file_size,
            content_type: f.content_type,
            content_sha256: f.content_sha256,
            tmp_key: f.tmp_key,
            status: f.status,
            etag: f.etag,
            uploaded_at: f.uploaded_at,
        }
    }
}

/// `POST /upload-sessions/get-or-create` 出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出，`files` 改为
/// `Vec<SessionFileOut>`（之前直接用内部 `Vec<SessionFile>`）。
#[derive(Debug, Clone, Serialize)]
pub struct GetOrCreateOut {
    pub session_id: String,
    pub scope: String,
    pub tmp_prefix: String,
    pub bucket: String,
    pub region: String,
    pub credentials: SessionCredentialsOut,
    pub expires_in: i64,
    pub files: Vec<SessionFileOut>,
}

/// `POST /upload-sessions/{session_id}/files:allocate` 出参。
///
/// `client_ref` 已存在 → 幂等返回原 `tmp_key`；同 sha 不同 client_ref 仍分配新 tmp_key
/// （防止误用别人 client_ref 锁死自己的 key 分配）。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct AllocateFilesOut {
    pub items: Vec<AllocateFileItemOut>,
}

/// allocate 单条出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct AllocateFileItemOut {
    pub client_ref: String,
    pub tmp_key: String,
}

/// `POST /upload-sessions/{session_id}/files/{client_ref}/complete` 出参
/// （更新后的 SessionFile）。
///
/// 与 `SessionFileOut` 同形（字段完全一致）；定义为独立类型别名以保持端点
/// 命名清晰——`complete_file` 端点的响应语义是"complete 后状态"，与
/// `get_or_create` 中的 "files 列表元素" 同形但语义侧重不同，演化方向
/// 可能分歧（例：complete 可能加 `server_received_at` 字段）。
///
/// 2026-09-22 PR4：原 `pub type CompleteFileOut = SessionFile;`（dto.rs）改为
/// `pub type CompleteFileOut = SessionFileOut;`（vo/）——内部 `SessionFile` 不再
/// 暴露给 handler。
pub type CompleteFileOut = SessionFileOut;

/// `POST /upload-sessions/{session_id}/files:remove` 出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct RemoveFilesOut {
    /// 已从 session JSON 移除的 client_ref 列表（顺序按入参，**仅含真实移除的**）。
    /// 不存在的 client_ref 不计入。
    pub removed: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/renew` 出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct RenewOut {
    pub credentials: SessionCredentialsOut,
    pub expires_in: i64,
}

/// `POST /upload-sessions/{session_id}/consume` 出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct ConsumeFilesOut {
    pub consumed: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/discard` 出参。
///
/// 2026-09-18 新增；2026-09-22 PR4：自 dto.rs 迁出。
#[derive(Debug, Clone, Serialize)]
pub struct DiscardOut {
    pub session_id: String,
}