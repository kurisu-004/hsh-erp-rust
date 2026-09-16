//! part_file 域 DTO
//!
//! 对应 Python myERP/schema/part_file.py。
//!
//! ## id 序列化约定
//! 雪花 i64 字段用 `serialize_i64`（Global Constraint #3）。
//!
//! ## 2026-09-16 M2-B 业务层
//! 新增直传 COS 链路的 DTO（`UploadIntentsIn` / `UploadIntentsOut` /
//! `ConfirmFileIn` 等）+ `validate` 子模块集中校验函数。

use serde::{Deserialize, Serialize};

use crate::modules::part_file::policy;
use crate::shared::error::{code, AppError};
use crate::shared::types::{
    deserialize_i64, deserialize_i64_opt, serialize_i64, serialize_i64_opt,
};

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

// ===== 2026-09-16 M2-B 业务层：直传 COS 链路 DTO =====

/// `POST /api/v2/part-files/upload-intents` 单文件项。
///
/// 客户端用 `client_ref` 把本条意图回连到 UI 行；服务端按 `(owner_id, kind, sha)`
/// 做 CAS 去重，命中则标记 `dedup_hit=true` 并复用已有 `PartFileOut`，不分配
/// `tmp_key`（前端无需上传即可让前端直接刷列表）。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadIntentItemIn {
    pub kind: String, // "DRAWING" / "3D_MODEL"
    pub filename: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    /// 64 hex chars（客户端声明的 SHA-256；服务端在 confirm 时再算实际值交叉校验）。
    pub content_sha256: String,
    pub content_type: String,
}

/// `POST /api/v2/part-files/upload-intents` 入参。
///
/// - `owner_part_id = Some(_)`：场景 B（已有 part 的补传 / 详情页加文件）；
///   同 `(owner_id, kind, sha)` 已存在 → 标记 `dedup_hit`。
/// - `owner_part_id = None`：场景 A（批量预生成 + part 还未创建）；
///   不做去重（part 还没建，无法查重），分配 batch_uuid 前缀。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadIntentsIn {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub owner_part_id: Option<i64>,
    pub files: Vec<UploadIntentItemIn>,
}

/// 单条上传意图结果。
///
/// - `dedup_hit=true`：`tmp_key` 为空、`existing_file` 填充，前端跳过上传
/// - `dedup_hit=false`：`tmp_key` 分配、`existing_file` 缺省
#[derive(Debug, Clone, Serialize)]
pub struct UploadIntentItemOut {
    /// 客户端 reference，前端用此 key 把上传进度映射回行。
    ///
    /// service 端按入参顺序 1:1 返回 `seq`（0-based 序号转字符串）。
    pub client_ref: String,
    /// COS 临时对象 key（dedup_hit 时为空）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tmp_key: String,
    /// CAS 去重命中（同 owner+kind+sha 已有活跃文件）。
    pub dedup_hit: bool,
    /// 命中时返回已有文件（前端直接刷列表，免上传）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_file: Option<PartFileOut>,
}

/// `POST /api/v2/part-files/upload-intents` 出参。
///
/// 一次性下发 STS 凭证 + 整 batch 的 tmp 前缀 + 每文件的 tmp_key 或 dedup 命中标记。
#[derive(Debug, Clone, Serialize)]
pub struct UploadIntentsOut {
    pub credentials: CosCredentialsOut,
    pub bucket: String,
    pub region: String,
    /// 本次 batch 的 tmp 前缀（场景 A：`{tmp_prefix}{batch_uuid}/`；
    /// 场景 B：`{tmp_prefix}part/{owner_id}/`）。
    pub tmp_prefix: String,
    pub items: Vec<UploadIntentItemOut>,
}

/// 客户端拿到后拼 PUT 请求时用的 STS 临时凭证子集。
///
/// 字段命名贴近 SDK `TemporaryCredentials`；`expired_time` 序列化为 unix 秒字符串。
#[derive(Debug, Clone, Serialize)]
pub struct CosCredentialsOut {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    #[serde(serialize_with = "serialize_i64")]
    pub expired_time: i64,
}

/// `POST /api/v2/parts/{id}/files/confirm` 入参。
///
/// 客户端声明已上传完成的 tmp 对象；服务端 head_object 校验存在 + size 一致后
/// copy 到 CAS key，再 INSERT `t_part_file` 并 spawn 异步 delete tmp 兜底。
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

    /// 一站式校验 `UploadIntentItemIn`（含 max_file_size 上下文）。
    #[allow(clippy::too_many_arguments)]
    pub fn check_upload_intent_item(
        item: &UploadIntentItemIn,
        max_file_size: usize,
    ) -> Result<(), AppError> {
        check_kind(&item.kind)?;
        check_sha256(&item.content_sha256)?;
        check_filename(&item.filename)?;
        check_file_size(item.file_size, max_file_size)?;
        check_content_type(&item.content_type, &item.filename)?;
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
    fn check_upload_intent_item_happy_path() {
        let item = UploadIntentItemIn {
            kind: "DRAWING".into(),
            filename: "drawing.pdf".into(),
            file_size: 1024,
            content_sha256: "a".repeat(64),
            content_type: "application/pdf".into(),
        };
        assert!(validate::check_upload_intent_item(&item, max_file_size()).is_ok());
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
