//! part_file 域 DTO（2026-09-16 M2-B → 2026-09-18 上传会话拆分 + 2026-09-22 PR4 拆分）
//!
//! 对应 Python myERP/schema/part_file.py。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 本文件仅含入参（Deserialize）；出参结构（`PartFileOut` / `PartFileWithUrlOut` /
//! `PartFileListOut`）已迁移至 `super::vo`。

use serde::Deserialize;

use crate::modules::part_file::policy;
use crate::shared::error::{AppError, code};
use crate::shared::types::deserialize_i64;

// ===== 2026-09-16 M2-B 业务层：ConfirmFileIn（保留） =====
//
// 2026-09-18 注：原 `UploadIntentsIn` / `UploadIntentsOut` / `UploadIntentItemIn` /
// `UploadIntentItemOut` / `CosCredentialsOut` 等 DTO 已删除，迁移至相关 STS 会话域。
// 2026-09-28 备注：相关 STS 会话域已下线，前端改为单 uploader 触发时单 HTTP 调用
// python `sts-tmp-keys` 数组入参直签。本文件不再涉及任何 STS / 会话相关 DTO。
//
// confirm 端点（`POST /api/v2/parts/{id}/files/confirm`）仍保留，其入参：
///
/// 2026-09-29 扁平化：新增 `ext` 字段（client 声明）。CAS key 模板五段→两段后，
/// key 已不再含 owner_kind / owner_id / KIND 段，kind 信息需 client 在 confirm
/// 时显式回传（其实 kind 已在 ConfirmFileIn 里，但 ext 是新模板隐式依赖的
/// 字段——同一 owner + sha + 不同 ext 对应不同 file_type，所以 ext 必须随
/// confirm 上行，便于 service 校验 file_type 与 ext 一致）。
#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmFileIn {
    pub kind: String,
    pub tmp_key: String,
    pub content_sha256: String,
    pub original_filename: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    pub content_type: String,
    /// 2026-09-29 新增：扩展名（小写、不含点）。由 client 从 `original_filename`
    /// 提取后随 confirm 提交，避免 server 端 `policy::ext_of` 在中文 / 多段扩展
    /// 边界（`.tar.gz`）上与 client 不一致。
    #[serde(default)]
    pub ext: Option<String>,
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

    /// 2026-09-29 新增：客户端声明的 `ext` 与服务端推导（policy::ext_of）一致。
    ///
    /// 用途：confirm 端点 client 提交 `ext` 字段（CAS key 模板五段→两段后，
    /// ext 需作为 file_type 推导源随 confirm 上行，避免 server 端从
    /// original_filename 二次解析与 client 不一致）。允许 ext 为空（None /
    /// `""`）走兼容回退，由 service 走 policy::ext_of。
    pub fn check_ext(client_ext: Option<&str>, filename: &str) -> Result<String, AppError> {
        let server_ext = policy::ext_of(filename)
            .ok_or_else(|| AppError::validation(format!("filename {filename:?} 缺少扩展名")))?;
        if let Some(ce) = client_ext
            && !ce.is_empty()
            && !ce.eq_ignore_ascii_case(&server_ext)
        {
            return Err(AppError::validation(format!(
                "client ext {ce:?} 与 server 推导 ext {server_ext:?} 不一致"
            )));
        }
        Ok(server_ext)
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
            ext: None, // 2026-09-29 新增字段（unit test 兼容回退 None）
        };
        assert!(validate::check_confirm_file_in(&req, max_file_size()).is_ok());
    }
}
