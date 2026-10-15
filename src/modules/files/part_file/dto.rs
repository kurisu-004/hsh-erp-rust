//! `files::part_file` 子模块 DTO（2026-09-16 M2-B → 2026-09-18 上传会话拆分
//! + 2026-09-22 PR4 拆分 + 2026-10-13 收编进 files 域）
//!
//! 对应 Python myERP/schema/part_file.py。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 本文件仅含入参（Deserialize）；出参结构（`PartFileOut` / `PartFileWithUrlOut` /
//! `PartFileListOut`）在 `super::vo`。
//!
//! 2026-10-13：`DeletePartFileRequest` 自 `parent::handler` 移入本文件（与
//! `ConfirmFileIn` 做邻居）。原因是它原先被 `cnc` 子模块的 handler import ——
//! 全仓唯一一处 handler → handler 跨模块 import；子模块化之后那条依赖线读起来像
//! 「cnc 依赖 part_file 的 handler 层」，而 DTO 归 dto.rs 才是本仓既有约定
//! （见上文 DTO/VO 边界）。

use serde::Deserialize;

use crate::modules::files::part_file::policy;
use crate::shared::error::{AppError, code};
use crate::shared::types::deserialize_i64;

/// `POST /api/v2/files/objects/{file_id}/delete` 入参：版本号 OCC。
#[derive(Debug, Deserialize)]
pub struct DeletePartFileRequest {
    pub version: i32,
}

/// `POST /api/v2/files/objects` multipart 的 `data` 文本字段（JSON 序列化本结构）。
///
/// 2026-10-13 PR1b **复活**：本结构在 PR1a 随 `POST /api/v2/part-files/` 端点下线
/// 一并删除，本轮四合一上传端点（`POST /files/objects`）把它原样接回来 —— 复用旧
/// 契约，不是新设计。四个旧入口（`upload-drawing` / `upload-3d-model` /
/// `assemblies/{id}/files` / `POST /part-files/`）现在共用这一个形态。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadFileData {
    pub owner_kind: String, // PART / ASSEMBLY
    /// 雪花 id 的**字符串**形式（`shared::types` 的 serde helper 约定：i64 主键
    /// 一律以 string 出入参，防 JS 精度截断）。handler 侧 parse 成 i64 后交给
    /// service。
    pub owner_id: String,
    pub kind: String, // DRAWING / 3D_MODEL / ASSEMBLY_MASTER / CAD_2D / …
}

// ===== 2026-10-15 上传链路统一：upload-intents（签发预签 PUT URL） =====
//
// 全仓唯一的收文件链路：业务提交成功 → `POST /files/objects/upload-intents` 拿
// 预签 PUT URL → 前端 XHR 直传 COS → `POST /files/objects/confirm` 登记。

/// `POST /api/v2/files/objects/upload-intents` 入参（**批量**，1..=200 项）。
///
/// `owner_kind` / `owner_id` 一次声明、本组 `items` 共用 —— 前端一次选中多个文件
/// 只发一个请求，不必逐文件往返。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadIntentsIn {
    /// `PART` / `ASSEMBLY` / `CUSTOMER`
    pub owner_kind: String,
    /// 雪花 id 的**字符串**形式（`shared::types` 的 serde helper 约定，防 JS 精度截断）。
    pub owner_id: String,
    pub items: Vec<UploadIntentItemIn>,
}

/// 单个待签发项：描述「这个文件是什么」，不含字节。
///
/// ⚠️ `content_sha256` 由**前端**算（上传前），服务端只拿它派生 CAS key —— 见
/// `POST /files/objects/confirm` 的模块 doc 里的信任取舍（预签 PUT 用
/// UNSIGNED-PAYLOAD，服务端无法把 sha 绑进签名）。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadIntentItemIn {
    /// 前端生成的 UUID，原样回显，用来把响应项对齐回本地文件列表。
    pub client_ref: String,
    /// `DRAWING` / `3D_MODEL` / `ASSEMBLY_MASTER` / `DELIVERY_TEMPLATE` …
    pub kind: String,
    pub original_filename: String,
    pub content_type: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    /// 64 位小写 hex
    pub content_sha256: String,
}

// ===== 2026-09-16 M2-B 业务层：ConfirmFileIn（保留） =====
//
// 2026-09-18 注：原 `UploadIntentsIn` / `UploadIntentsOut` / `UploadIntentItemIn` /
// `UploadIntentItemOut` / `CosCredentialsOut` 等 DTO 已删除，迁移至相关 STS 会话域。
// 2026-09-28 备注：相关 STS 会话域已下线，前端改为单 uploader 触发时单 HTTP 调用
// python `sts-tmp-keys` 数组入参直签。本文件不再涉及任何 STS / 会话相关 DTO。
//
// confirm 端点仍保留，其入参：
//
// 2026-10-13 PR1b：confirm 端点自 part 域 `POST /api/v2/parts/{id}/files/confirm`
// 搬进本子模块，硬切为 `POST /api/v2/files/objects/confirm`（**无 alias，旧路径 404**）；
// path 段的 `part_id` 拆成 body 的 `owner_kind` + `owner_id` 两字段。
/// 2026-09-29 扁平化：新增 `ext` 字段（client 声明）。CAS key 模板五段→两段后，
/// key 已不再含 owner_kind / owner_id / KIND 段，kind 信息需 client 在 confirm
/// 时显式回传（其实 kind 已在 ConfirmFileIn 里，但 ext 是新模板隐式依赖的
/// 字段——同一 owner + sha + 不同 ext 对应不同 file_type，所以 ext 必须随
/// confirm 上行，便于 service 校验 file_type 与 ext 一致）。
#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmFileIn {
    /// 2026-10-13 PR1b 新增：owner 类别（`PART` / `ASSEMBLY` / `CUSTOMER`）。
    ///
    /// ⚠️ 旧端点 `POST /api/v2/parts/{part_id}/files/confirm` 的 owner 是 path 段
    /// 隐含的（恒 PART），搬家到 `POST /api/v2/files/objects/confirm` 后 path 段消失，
    /// owner 改由 body 显式携带 —— 与同端点群 `POST /files/objects` 的 multipart
    /// `data.owner_kind` **逐字对称**（同名字段、同取值域、同序列化形态）。
    pub owner_kind: String,
    /// 2026-10-13 PR1b 新增：owner 雪花 id 字符串（同上，替代旧 path 段的 `part_id`）。
    pub owner_id: String,
    pub kind: String,
    /// 2026-10-15 **硬切**：原 `tmp_key` 字段改名并改语义 ——
    /// 值必须是 `POST /files/objects/upload-intents` 响应里回的那个**正式 CAS key**，
    /// 服务端会用 `content_sha256` + `original_filename` 重新派生一遍并比对，
    /// 不一致即 21114（防前端拿任意 object_key 登记）。**无兼容层**：旧字段名不再被
    /// 接受，serde 反序列化阶段即失败。
    pub object_key: String,
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
    /// 2026-10-15 新增：CNC 配对目标（另一条已登记的 `t_part_file` 行 id，字符串
    /// 形式的雪花 id）。给了就把本行与那一行**双向**互写 `paired_file_id`
    /// （G_CODE ↔ SETUP_SHEET）；不给则不配对。
    #[serde(default)]
    pub pair_with_file_id: Option<String>,
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
            owner_kind: "PART".into(), // 2026-10-13 PR1b 新增字段
            owner_id: "1001".into(),   // 2026-10-13 PR1b 新增字段
            kind: "3D_MODEL".into(),
            object_key: "obj/bbbbbbbbbbbbbbbb_model.step".into(),
            content_sha256: "b".repeat(64),
            original_filename: "model.step".into(),
            file_size: 2048,
            content_type: "application/step".into(),
            ext: None,               // 2026-09-29 新增字段（unit test 兼容回退 None）
            pair_with_file_id: None, // 2026-10-15 新增（CNC 配对，可选）
        };
        assert!(validate::check_confirm_file_in(&req, max_file_size()).is_ok());
    }

    /// 2026-10-15 硬切：旧的 `tmp_key` 字段名**不再被接受**（无兼容层），
    /// 缺 `object_key` 直接反序列化失败（⇒ axum Json 提取器 422）。
    #[test]
    fn confirm_file_in_rejects_legacy_tmp_key_name() {
        let raw = r#"{
            "owner_kind": "PART",
            "owner_id": "1001",
            "kind": "3D_MODEL",
            "tmp_key": "tmp/x.stp",
            "content_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "original_filename": "model.step",
            "file_size": 2048,
            "content_type": "application/step"
        }"#;
        assert!(
            serde_json::from_str::<ConfirmFileIn>(raw).is_err(),
            "旧 `tmp_key` 字段名必须被拒（硬切，无兼容层）"
        );
    }
}
