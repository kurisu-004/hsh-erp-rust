//! upload_session 域 DTO
//!
//! 2026-09-18 新增。
//!
//! 7 个端点的请求 / 响应 DTO + `SessionFile` / `UploadSession` 数据结构。
//!
//! ## 与 part_file upload-intents 的差异
//! - **老 upload-intents** 是单次「签 STS + 预生成 tmp_key」无状态 RPC，每次都需要重新签；
//!   客户端状态在 UI 层（无 server 端会话）。
//! - **本模块 upload_session** 是有状态 Redis 会话机制：
//!   `upload_session:{user_id}:{scope}` key 存 `UploadSession` JSON（含 `session_id` /
//!   `credentials` / `files` / `expires_in`），TTL 24h 滑动；客户端只需第一次
//!   `get_or_create`，后续 `allocate` / `complete` / `remove` / `consume` /
//!   `discard` / `renew` 全部走同一会话，凭证 <600s 自动 renew。

use serde::{Deserialize, Serialize};

use crate::modules::part_file::policy;
use crate::shared::error::AppError;
use crate::shared::types::deserialize_i64_opt;

// ============================================================
// 内部数据结构（Redis JSON 值 + 部分出参复用）
// ============================================================

/// 单条文件状态（Redis JSON `UploadSession.files` 元素 + 部分端点出参）。
///
/// 字段命名贴近 front-end 期望；`etag` 是 COS HEAD 返回的 hex md5（带引号 `"..."`，
/// 与 `head_object` 返回的 `resp.etag` 一致；complete 时按 COS 文档原样保存）。
///
/// 2026-09-18 新增。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionFile {
    /// 客户端 reference（UUID 或 0-based seq string）；用于跨端点幂等映射。
    pub client_ref: String,
    /// "drawing" / "3d_model"（小写；与 part_file 直传链路统一）。
    pub kind: String,
    pub original_filename: String,
    /// i64 用默认 serde 行为（JSON number ↔ i64）；Redis 内部 JSON 与 API 出参
    /// 都用 number；client 端若需要 string 可在 `Serialize` 时改 custom serializer。
    pub file_size: i64,
    pub content_type: String,
    /// 64 hex chars；用于 tmp_key 派生（前 16 字符作 `sha16_xxx.pdf`）。
    pub content_sha256: String,
    /// COS 临时对象 key（`tmp/sess/<uuid>/<sha16>_<safe>.ext`）。
    pub tmp_key: String,
    /// "pending" / "done" / "error"。
    pub status: String,
    /// COS 返回的 ETag（head_object；带引号，如 `"d41d8cd98f00b204e9800998ecf8427e"`）。
    /// 未完成时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// ISO 8601 UTC 字符串（前端直接展示）；`done` 时填，其余为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_at: Option<String>,
}

/// 共享 STS 凭证子集（`UploadSession.credentials` 元素 + 出参 `credentials` 复用）。
///
/// 与 part_file `CosCredentialsOut` 字段一致但类型 / 序列化策略不同：
/// - 本结构是 Redis 内部 JSON + API 出参；`start_time` / `expired_time` 用 i64 默认
///   JSON number 序列化（前端拿到的是 number，不是 string；本契约不沿用雪花 id 的
///   "string 防 JS 精度截断"策略，因为 unix 秒远小于 2^53）。
///
/// 2026-09-18 新增。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCredentials {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    pub start_time: i64,
    pub expired_time: i64,
}

/// Redis 内存储的整条 upload session。
///
/// JSON 形式存在 `upload_session:{user_id}:{scope}`；TTL 24h 滑动续期。
///
/// `session_id` 与 Redis key 解耦：path 上的 `session_id` 必须等于
/// `UploadSession.session_id`，否则 → 409 `BIZ_UPLOAD_SESSION_MISMATCH`
/// （防止 cross-user / cross-scope 误用）。
///
/// 2026-09-18 新增。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSession {
    pub session_id: String,
    pub user_id: i64,
    pub scope: String,
    /// `tmp/sess/<session_id>/`（含尾斜杠）。由 service 派生。
    pub tmp_prefix: String,
    pub bucket: String,
    pub region: String,
    pub credentials: SessionCredentials,
    /// 凭证有效期（秒，与 `cfg.credentials.expired_time - start_time` 对齐）。
    pub expires_in: i64,
    pub files: Vec<SessionFile>,
    /// 创建时间（unix 秒；audit 用）。
    pub created_at: i64,
    /// 最后一次写时间（unix 秒；audit 用，update 时刷新）。
    pub updated_at: i64,
}

// ============================================================
// 入参 / 出参 DTO（按 7 个端点）
// ============================================================

/// `POST /upload-sessions/get-or-create` 入参。
///
/// scope 首期仅 `"parts_new"`（白名单硬编码，其它 scope 直接 422 拒绝）。
#[derive(Debug, Clone, Deserialize)]
pub struct GetOrCreateIn {
    pub scope: String,
}

/// `POST /upload-sessions/get-or-create` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct GetOrCreateOut {
    pub session_id: String,
    pub scope: String,
    pub tmp_prefix: String,
    pub bucket: String,
    pub region: String,
    pub credentials: SessionCredentialsOut,
    pub expires_in: i64,
    pub files: Vec<SessionFile>,
}

/// `POST /upload-sessions/{session_id}/files:allocate` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct AllocateFilesIn {
    pub scope: String,
    pub files: Vec<AllocateFileItemIn>,
}

/// allocate 单条入参。
///
/// 字段与 SessionFile 几乎对齐，缺 `tmp_key` / `status` / `etag` / `uploaded_at`（由 service 派生）。
#[derive(Debug, Clone, Deserialize)]
pub struct AllocateFileItemIn {
    pub client_ref: String,
    pub kind: String,
    pub original_filename: String,
    /// i64 默认 JSON number ↔ i64；客户端若需 string 可在 out 端改 custom serializer。
    /// 本契约不沿用雪花 id "string 防 JS 精度截断"策略（file_size < 2^53 安全）。
    pub file_size: i64,
    pub content_type: String,
    pub content_sha256: String,
}

/// `POST /upload-sessions/{session_id}/files:allocate` 出参。
///
/// `client_ref` 已存在 → 幂等返回原 `tmp_key`；同 sha 不同 client_ref 仍分配新 tmp_key
/// （防止误用别人 client_ref 锁死自己的 key 分配）。
#[derive(Debug, Clone, Serialize)]
pub struct AllocateFilesOut {
    pub items: Vec<AllocateFileItemOut>,
}

/// allocate 单条出参。
#[derive(Debug, Clone, Serialize)]
pub struct AllocateFileItemOut {
    pub client_ref: String,
    pub tmp_key: String,
}

/// `POST /upload-sessions/{session_id}/files/{client_ref}/complete` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct CompleteFileIn {
    pub scope: String,
    /// 可选：客户端声明的 etag（与 COS head 返回不一致时仍以 COS 为准，本字段仅做 audit 参考）。
    #[serde(default)]
    pub etag: Option<String>,
    /// 可选：客户端声明的 file_size；服务端 head 时交叉校验（若有则校验，无则跳过）。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub file_size: Option<i64>,
}

/// `POST /upload-sessions/{session_id}/files/{client_ref}/complete` 出参（更新后的 SessionFile）。
pub type CompleteFileOut = SessionFile;

/// `POST /upload-sessions/{session_id}/files:remove` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveFilesIn {
    pub scope: String,
    pub client_refs: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/files:remove` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct RemoveFilesOut {
    /// 已从 session JSON 移除的 client_ref 列表（顺序按入参，**仅含真实移除的**）。
    /// 不存在的 client_ref 不计入。
    pub removed: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/renew` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct RenewIn {
    pub scope: String,
}

/// `POST /upload-sessions/{session_id}/renew` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct RenewOut {
    pub credentials: SessionCredentialsOut,
    pub expires_in: i64,
}

/// `POST /upload-sessions/{session_id}/consume` 入参（业务消费：从 session 移除条目）。
#[derive(Debug, Clone, Deserialize)]
pub struct ConsumeFilesIn {
    pub scope: String,
    pub client_refs: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/consume` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct ConsumeFilesOut {
    pub consumed: Vec<String>,
}

/// `POST /upload-sessions/{session_id}/discard` 入参。
#[derive(Debug, Clone, Deserialize)]
pub struct DiscardIn {
    pub scope: String,
}

/// `POST /upload-sessions/{session_id}/discard` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct DiscardOut {
    pub session_id: String,
}

// ============================================================
// 出参专用：`SessionCredentialsOut`
// ============================================================

/// 客户端拿到的 STS 凭证出参（与 `part_file::CosCredentialsOut` 字段对齐，
/// 多带 `start_time` 便于前端算剩余有效期；`expired_time` 保持 i64 输出）。
///
/// 2026-09-18 新增。
#[derive(Debug, Clone, Serialize)]
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

// ============================================================
// 校验 helper
// ============================================================

/// scope 白名单硬编码（首期仅 `"parts_new"`）；非白名单 → 422。
///
/// 后续扩容（如新增 `"assemblies_new"`）时改此处 + 文档同步。
pub fn is_valid_scope(scope: &str) -> bool {
    scope == "parts_new"
}

/// 入参 kind 白名单（与 `part_file::policy::allowed_exts` 字段对齐：
/// `"drawing"` / `"3d_model"`；大小写不敏感）。
///
/// 不匹配 → 422 `BIZ_UPLOAD_SESSION_BAD_TYPE`。
///
/// 2026-09-18 review #7 修复：原实现把 "drawing"/"3d_model" 显式映射到
/// "DRAWING"/"3D_MODEL" 查 policy，**回退分支**直接传小写给 policy（policy
/// 是大写 key，等价于永远查不到）。新实现统一 `to_ascii_uppercase` 后查 policy，
/// 删除冗余的 match 回退分支。
pub fn is_valid_kind(kind: &str) -> bool {
    let upper = kind.to_ascii_uppercase();
    !policy::allowed_exts(&upper).is_empty()
}

/// 复用 part_file sanitize_filename 规则（ASCII 字母数字 / `.` / `-` / `_` 保留，
/// 其它替换 `_`，长度 ≤ 80）。本模块独立实现而不直接 pub use `part_file::service::sanitize_filename`，
/// 避免上传会话域反向依赖 part_file service（part_file service 内函数是 private fn）。
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.len() > 80 {
        out.truncate(80);
    }
    if out.is_empty() {
        out.push_str("file");
    }
    out
}

/// sha256 字段一站式校验（64 hex chars，大小写不敏感）。
pub fn check_sha256(sha: &str) -> Result<(), AppError> {
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::validation(format!(
            "content_sha256 必须是 64 个 hex 字符（大小写不敏感），got {len} chars",
            len = sha.len(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_whitelist_accepts_parts_new() {
        assert!(is_valid_scope("parts_new"));
    }

    #[test]
    fn scope_whitelist_rejects_other() {
        assert!(!is_valid_scope("assemblies_new"));
        assert!(!is_valid_scope(""));
        assert!(!is_valid_scope("PARTS_NEW"));
    }

    #[test]
    fn kind_whitelist_accepts_drawing_and_3d_model() {
        assert!(is_valid_kind("drawing"));
        assert!(is_valid_kind("3d_model"));
        // 大小写不敏感
        assert!(is_valid_kind("Drawing"));
    }

    #[test]
    fn kind_whitelist_rejects_other() {
        assert!(!is_valid_kind("g_code"));
        assert!(!is_valid_kind(""));
        assert!(!is_valid_kind("DRAWING_EXTRA"));
    }

    #[test]
    fn sanitize_filename_keeps_safe_chars() {
        assert_eq!(sanitize_filename("drawing.pdf"), "drawing.pdf");
        assert_eq!(sanitize_filename("DRAW-001.PDF"), "DRAW-001.PDF");
    }

    #[test]
    fn sanitize_filename_replaces_unsafe() {
        assert_eq!(sanitize_filename("图纸 v2.pdf"), "___v2.pdf");
        assert_eq!(sanitize_filename("a b/c.pdf"), "a_b_c.pdf");
    }

    #[test]
    fn sanitize_filename_truncates_long_names() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_filename(&long).len(), 80);
    }

    #[test]
    fn sanitize_filename_empty_fallback() {
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("中文"), "__");
    }

    #[test]
    fn check_sha256_accepts_64_hex() {
        assert!(check_sha256(&"a".repeat(64)).is_ok());
        assert!(check_sha256(&"ABCDEF0123456789".repeat(4)).is_ok());
    }

    #[test]
    fn check_sha256_rejects_short_or_non_hex() {
        assert!(check_sha256("abcd").is_err());
        let bad = format!("{}{}", "a".repeat(63), "g");
        assert!(check_sha256(&bad).is_err());
    }

    #[test]
    fn session_credentials_out_conversion_preserves_fields() {
        let cred = SessionCredentials {
            tmp_secret_id: "id".into(),
            tmp_secret_key: "key".into(),
            session_token: "tok".into(),
            start_time: 100,
            expired_time: 3700,
        };
        let out: SessionCredentialsOut = (&cred).into();
        assert_eq!(out.tmp_secret_id, "id");
        assert_eq!(out.start_time, 100);
        assert_eq!(out.expired_time, 3700);
    }

    #[test]
    fn upload_session_roundtrips_json() {
        let s = UploadSession {
            session_id: "abc".into(),
            user_id: 42,
            scope: "parts_new".into(),
            tmp_prefix: "tmp/sess/abc/".into(),
            bucket: "b".into(),
            region: "ap-shanghai".into(),
            credentials: SessionCredentials {
                tmp_secret_id: "i".into(),
                tmp_secret_key: "k".into(),
                session_token: "t".into(),
                start_time: 100,
                expired_time: 200,
            },
            expires_in: 100,
            files: vec![SessionFile {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
                tmp_key: "tmp/sess/abc/aaaa_a.pdf".into(),
                status: "pending".into(),
                etag: None,
                uploaded_at: None,
            }],
            created_at: 1,
            updated_at: 2,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: UploadSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.session_id, "abc");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].client_ref, "r1");
        assert_eq!(back.files[0].file_size, 1024);
    }
}
