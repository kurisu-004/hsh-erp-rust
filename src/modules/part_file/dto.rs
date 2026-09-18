//! part_file 域 DTO
//!
//! 对应 Python myERP/schema/part_file.py。
//!
//! ## id 序列化约定
//! 雪花 i64 字段用 `serialize_i64`（Global Constraint #3）。
//!
//! ## 2026-09-16 M2-B → 2026-09-18 上传会话拆分
//! - 原 M2-B 直传 COS 链路 DTO（`UploadIntentsIn` / `UploadIntentsOut` /
//!   `UploadIntentItemIn` / `UploadIntentItemOut` / `UploadIntentsIn` + 上传意图校验）
//!   2026-09-18 已删除：上传意图机制迁移至 `upload_session` 域（共享 STS 凭证 +
//!   Redis 会话），保留 `ConfirmFileIn` 作为 confirm 端点的入参。
//! - `validate` 子模块保留：confirm 端点继续复用 kind / sha / filename / size /
//!   content_type 校验函数。

use serde::{Deserialize, Serialize};

use crate::modules::part_file::policy;
use crate::shared::error::{AppError, code};
use crate::shared::types::{deserialize_i64, serialize_i64, serialize_i64_opt};

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
    /// CNC 配对文件 id（G_CODE <-> SETUP_SHEET 互指）；未配对为 null。
    /// JSON 序列化为 string（雪花 id，防 JS 精度截断）。
    /// 2026-09-16 补投影：v2 切流后前端零件详情 CNC 配对分组依赖本字段，
    /// 此前 DB / model 均有值但 DTO 漏投导致前端配对分组静默失效。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub paired_file_id: Option<i64>,
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

// ===== 2026-09-16 M2-B 业务层：ConfirmFileIn（保留） =====
//
// 2026-09-18 注：原 `UploadIntentsIn` / `UploadIntentsOut` / `UploadIntentItemIn` /
// `UploadIntentItemOut` / `CosCredentialsOut` 等 DTO 已删除，迁移至
// `upload_session` 域（共享 STS 凭证 + Redis 会话机制）。
// - `POST /api/v2/part-files/upload-intents` 端点（删除）
// - 直传意图分配 tmp_key 的 DTO（删除；由 upload_session.allocate 替代）
//
// confirm 端点（`POST /api/v2/parts/{id}/files/confirm`）仍保留，其入参：
#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmFileIn {
    pub kind: String,
    pub tmp_key: String,
    pub content_sha256: String,
    pub original_filename: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    pub content_type: String,
}

// ---------- 入参 ----------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PartFileListQuery {
    #[serde(default)]
    pub owner_kind: Option<String>, // PART / ASSEMBLY
    #[serde(default)]
    pub owner_id: Option<String>, // 雪花 id（String 形式；service 层 parse i64）
    #[serde(default)]
    pub kind: Option<String>, // DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===== 2026-09-16 M2-B 业务层：校验函数集中 =====

/// 入参字段校验（直传 COS 链路专用）。
///
/// 任何字段不合法 → `AppError::Validation(...)`（错误码 40001，HTTP 422）；
/// 不引入业务码，避免与既有 `BIZ_PART_FILE_BAD_TYPE` 21102（multipart 端点）
/// 在语义上重叠——此处校验失败是「客户端没按规范填字段」，与 kind 不匹配
/// 不是一回事。
///
/// 校验范围：
/// - `content_sha256`：64 hex chars（regex `^[0-9a-f]{64}$`）
/// - `filename`：非空、长度 ≤ 255
/// - `file_size`：> 0 且 ≤ `max_file_size`（默认 300MB）
/// - `kind`：必须出现在 `policy::allowed_exts`（非空即合法）
/// - `content_type`：与扩展名匹配（`policy::expected_content_types_for_ext`）
pub mod validate {
    use super::*;

    /// SHA-256 必须 64 hex chars（regex `^[0-9a-f]{64}$`）。
    pub fn check_sha256(sha: &str) -> Result<(), AppError> {
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            // 区分全大写：M2-A 已约定 hex 必须小写（与 cos-rust-sdk 一致）。
            return Err(AppError::validation(format!(
                "content_sha256 必须是 64 个 hex 字符（大小写不敏感），got {len} chars",
                len = sha.len(),
            )));
        }
        Ok(())
    }

    /// filename 非空、长度 ≤ 255（避免下游路径过长 / DB object_key 字段截断）。
    pub fn check_filename(name: &str) -> Result<(), AppError> {
        if name.is_empty() {
            return Err(AppError::validation("filename 不可为空"));
        }
        if name.len() > 255 {
            return Err(AppError::validation(format!(
                "filename 长度 {len} > 255",
                len = name.len()
            )));
        }
        Ok(())
    }

    /// file_size ∈ (0, max_file_size]。
    ///
    /// `max_file_size` 直接传字节数（默认 300MB = 300 * 1024 * 1024）。
    pub fn check_file_size(size: i64, max_file_size: usize) -> Result<(), AppError> {
        if size <= 0 {
            return Err(AppError::validation(format!(
                "file_size 必须 > 0，got {size}"
            )));
        }
        if (size as usize) > max_file_size {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                format!(
                    "file_size {size} 超过上限 {max_file_size}（{}MB）",
                    max_file_size / 1024 / 1024
                ),
            ));
        }
        Ok(())
    }

    /// kind 白名单（`policy::allowed_exts(kind)` 非空即合法）。
    pub fn check_kind(kind: &str) -> Result<(), AppError> {
        if policy::allowed_exts(kind).is_empty() {
            return Err(AppError::validation(format!(
                "kind {kind:?} 不支持（仅 DRAWING / 3D_MODEL 等白名单）"
            )));
        }
        Ok(())
    }

    /// content_type 与 filename 扩展名匹配。
    pub fn check_content_type(content_type: &str, filename: &str) -> Result<(), AppError> {
        let ext = policy::ext_of(filename)
            .ok_or_else(|| AppError::validation(format!("filename {filename:?} 缺少扩展名")))?;
        let expected = policy::expected_content_types_for_ext(&ext);
        if !expected
            .iter()
            .any(|c| c.eq_ignore_ascii_case(content_type))
        {
            return Err(AppError::validation(format!(
                "content_type {content_type:?} 与扩展名 {ext:?} 不匹配（期望 {expected:?}）"
            )));
        }
        Ok(())
    }

    /// 一站式校验 `ConfirmFileIn`（bind_uploaded_file 用）。
    #[allow(clippy::too_many_arguments)]
    pub fn check_confirm_file_in(
        req: &ConfirmFileIn,
        max_file_size: usize,
    ) -> Result<(), AppError> {
        check_kind(&req.kind)?;
        check_sha256(&req.content_sha256)?;
        check_filename(&req.original_filename)?;
        check_file_size(req.file_size, max_file_size)?;
        check_content_type(&req.content_type, &req.original_filename)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn max_file_size() -> usize {
        300 * 1024 * 1024
    }

    #[test]
    fn check_sha256_accepts_lowercase_64_hex() {
        let sha = "a".repeat(64);
        assert!(validate::check_sha256(&sha).is_ok());
    }

    #[test]
    fn check_sha256_accepts_uppercase_64_hex() {
        let sha = "ABCDEF0123456789".repeat(4);
        assert!(validate::check_sha256(&sha).is_ok());
    }

    #[test]
    fn check_sha256_rejects_short() {
        assert!(validate::check_sha256("abcd").is_err());
    }

    #[test]
    fn check_sha256_rejects_non_hex() {
        // 64 chars but contains 'g'
        let bad = format!("{}{}", "a".repeat(63), "g");
        assert!(validate::check_sha256(&bad).is_err());
    }

    #[test]
    fn check_filename_rejects_empty() {
        assert!(validate::check_filename("").is_err());
    }

    #[test]
    fn check_filename_rejects_too_long() {
        let long = "a".repeat(256);
        assert!(validate::check_filename(&long).is_err());
    }

    #[test]
    fn check_filename_accepts_boundary() {
        assert!(validate::check_filename(&"x".repeat(255)).is_ok());
    }

    #[test]
    fn check_file_size_rejects_zero_and_negative() {
        assert!(validate::check_file_size(0, max_file_size()).is_err());
        assert!(validate::check_file_size(-1, max_file_size()).is_err());
    }

    #[test]
    fn check_file_size_rejects_over_max() {
        let too_big = (max_file_size() as i64) + 1;
        let err = validate::check_file_size(too_big, max_file_size()).expect_err("over-max 应报错");
        match err {
            AppError::Biz { code, .. } => {
                assert_eq!(code, code::BIZ_PART_FILE_TOO_LARGE, "21103");
            }
            other => panic!("期望 AppError::Biz(21103)，got {other:?}"),
        }
    }

    #[test]
    fn check_kind_rejects_unknown() {
        assert!(validate::check_kind("FOO").is_err());
        assert!(validate::check_kind("").is_err());
    }

    #[test]
    fn check_kind_accepts_drawing_and_3d_model() {
        assert!(validate::check_kind("DRAWING").is_ok());
        assert!(validate::check_kind("3D_MODEL").is_ok());
    }

    #[test]
    fn check_content_type_matches_ext() {
        // PDF + application/pdf → ok
        assert!(validate::check_content_type("application/pdf", "drawing.pdf").is_ok());
        // PDF + application/octet-stream → 拒绝（DRAWING 不允许兜底 ct）
        assert!(validate::check_content_type("application/octet-stream", "drawing.pdf").is_err());
    }

    #[test]
    fn check_content_type_rejects_missing_ext() {
        assert!(validate::check_content_type("application/pdf", "noext").is_err());
    }

    #[test]
    fn check_confirm_file_in_happy_path() {
        let req = ConfirmFileIn {
            kind: "3D_MODEL".into(),
            tmp_key: "tmp/batch/seq_step.stp".into(),
            content_sha256: "b".repeat(64),
            original_filename: "model.step".into(),
            file_size: 2048,
            content_type: "application/step".into(),
        };
        assert!(validate::check_confirm_file_in(&req, max_file_size()).is_ok());
    }
}
