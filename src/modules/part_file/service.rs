//! part_file 域业务逻辑（2026-09-14 Phase 3）
//!
//! 对应 Python myERP/service/part_file_service.py + service/_file_kind_policy.py。
//!
//! ## 上传核心逻辑（`upload_file_for_owner`）
//! 1. 取 `owner_kind`（PART / ASSEMBLY），校验存在性（21105 OWNER_NOT_FOUND）
//! 2. 校验 `kind` 在白名单内（policy::allowed_exts）
//! 3. 校验扩展名（policy::ext_of）—— 无扩展名 / 不在白名单 → 21102 BAD_TYPE
//! 4. 校验 content_type（policy::expected_content_types_for_ext）—— 不匹配 → 21102
//! 5. SHA-256 算 hash → 同 owner + kind + sha 撞唯一索引 → 21108 DUPLICATE（CAS 去重）
//!    注意：CAS 命中时**跳过 COS PUT**，直接复用已有 object_key（节省 COS 流量）
//! 6. 上传 COS（infra/cos.rs::put_object）
//! 7. INSERT t_part_file
//! 8. 返回 PartFileOut
//!
//! ## multipart 上传契约
//! 客户端发 multipart form-data：
//!   - `data` JSON 字段：`{"owner_kind":"PART","owner_id":"1001","kind":"DRAWING"}`
//!   - `file` 二进制字段：PDF / STEP / STL 等
//!
//! `content_type` 取客户端声明（multipart 头），扩展名从 `filename` 提取。
//!
//! ## 与 part 域的耦合
//! 装配体 PDF（kind='ASSEMBLY_MASTER'）走相同上传通道，owner_kind='ASSEMBLY'，
//! 由 assembly 域在创建流程或单独的 `POST /assemblies/{id}/files` 端点调用。

use std::sync::Arc;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part_file::dto::{PartFileListOut, PartFileListQuery, PartFileOut, PartFileWithUrlOut};
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::policy;
use crate::modules::part_file::repo::{hash_bytes, NewPartFile, PartFileRepo};
use crate::shared::error::{code, AppError};

pub struct PartFileService;

impl PartFileService {
    /// 单文件上传（multipart `file` 字段 + JSON `data` 字段已解析）。
    ///
    /// `original_filename` 是客户端 multipart 的 filename；`content_type` 是
    /// 客户端声明的 MIME；`bytes` 是文件二进制。
    ///
    /// 返回新建的 `PartFileOut`（含 id）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_for_owner(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        owner_kind: &str,
        owner_id: i64,
        kind: &str,
        original_filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
        current: &CurrentUser,
    ) -> Result<PartFileOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::CncProgrammer])?;

        // 1. owner 存在性校验
        Self::assert_owner_exists(conn, owner_kind, owner_id).await?;

        // 2. kind 白名单（policy::allowed_exts 自动挡掉未知 kind）
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("文件缺少扩展名: {original_filename:?}")))?;
        let allowed_exts = policy::allowed_exts(kind);
        if !allowed_exts.contains(&ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("kind={kind:?} 不接受扩展名 {ext:?}"),
            ));
        }
        // 3. content_type 校验
        let expected = policy::expected_content_types_for_ext(&ext);
        if !expected.iter().any(|c| c.eq_ignore_ascii_case(content_type)) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!(
                    "kind={kind:?}, 扩展名={ext:?}: content_type {content_type:?} 不在白名单 {expected:?}"
                ),
            ));
        }
        // 4. file_type 推导
        let file_type = policy::file_type_for_ext(&ext)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("扩展名 {ext:?} 无对应 file_type")))?;

        // 5. SHA-256 → CAS 去重（撞唯一索引 → 21108 DUPLICATE）
        let sha = hash_bytes(&bytes);
        if let Some(existing) = PartFileRepo::get_by_owner_kind_sha(&mut *conn, owner_id, kind, &sha).await? {
            // CAS 命中：跳过 COS PUT，直接复用已有记录（返回 id / object_key）
            return Ok(Self::render_out(&existing, owner_kind));
        }

        // 6. 上传 COS（key 模板：`{owner_kind.to_lowercase()}/{owner_id}/{kind}/{sha}_{safe_filename}`）
        let safe_filename = sanitize_filename(original_filename);
        let object_key = format!(
            "{}/{}/{}/{}_{}",
            owner_kind.to_lowercase(),
            owner_id,
            kind,
            &sha[..16],
            safe_filename,
        );
        cos.put_object(&object_key, bytes.clone(), content_type)
            .await?;

        // 7. INSERT t_part_file
        let nf = NewPartFile {
            id: snowflake.next_id(),
            part_id: owner_id,
            owner_kind,
            kind,
            file_type,
            object_key: &object_key,
            original_filename,
            file_size: bytes.len() as i64,
            content_type,
            upload_status: "READY",
            content_sha256: Some(&sha),
            created_by: current.id,
        };
        let id = PartFileRepo::create_part_file(&mut *conn, nf).await.map_err(|e| match e {
            sqlx::Error::Database(db) => {
                if db.code().as_deref() == Some("23505") {
                    AppError::biz(
                        code::BIZ_PART_FILE_DUPLICATE,
                        format!("owner {owner_id} / {kind} / sha={} 撞唯一索引", &sha[..16]),
                    )
                } else {
                    AppError::from(sqlx::Error::Database(db))
                }
            }
            other => AppError::from(other),
        })?;

        // 8. 读回（include_deleted=true 兜底 INSERT 可见性）
        let row = PartFileRepo::get_by_id(&mut *conn, id, true)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_NOT_FOUND, "刚 INSERT 却查不到"))?;
        Ok(Self::render_out(&row, owner_kind))
    }

    /// 列表：按 owner_kind + owner_id + kind 过滤 + 分页。
    pub async fn list_files(
        conn: &mut PgConnection,
        query: &PartFileListQuery,
        current: &CurrentUser,
    ) -> Result<PartFileListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let owner_kind = query.owner_kind.as_deref().unwrap_or("PART");
        let owner_id_opt: Option<i64> = query
            .owner_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, format!("owner_id 非法: {s:?}"))
                })
            })
            .transpose()?;
        let kind_opt = query.kind.as_deref();

        let (rows, total) = PartFileRepo::list_with_filters(
            &mut *conn,
            owner_kind,
            owner_id_opt,
            kind_opt,
            limit,
            offset,
        )
        .await?;
        let items = rows
            .into_iter()
            .map(|r| Self::render_out(&r, owner_kind))
            .collect();
        Ok(PartFileListOut { items, total })
    }

    /// 单条详情 + 预签下载 URL。
    pub async fn get_file_with_url(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileWithUrlOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::CncProgrammer])?;
        let row = PartFileRepo::get_by_id(&mut *conn, file_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_NOT_FOUND, format!("part_file {file_id} 不存在")))?;
        let url = cos.presigned_get_url(&row.object_key, 3600).await?;
        Ok(PartFileWithUrlOut {
            id: row.id.to_string(),
            kind: row.kind,
            file_type: row.file_type,
            original_filename: row.original_filename,
            file_size: row.file_size,
            content_type: row.content_type,
            content_sha256: row.content_sha256,
            upload_status: row.upload_status,
            download_url: url,
            url_expires_in_seconds: 3600,
        })
    }

    /// 校验 owner 存在性（polymorphic）。
    async fn assert_owner_exists(
        conn: &mut PgConnection,
        owner_kind: &str,
        owner_id: i64,
    ) -> Result<(), AppError> {
        match owner_kind {
            "PART" => {
                let exists: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(owner_id)
                .fetch_optional(&mut *conn)
                .await?;
                if exists.is_none() {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!("part {owner_id} 不存在"),
                    ));
                }
            }
            "ASSEMBLY" => {
                let exists: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(owner_id)
                .fetch_optional(&mut *conn)
                .await?;
                if exists.is_none() {
                    return Err(AppError::biz(
                        code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                        format!("assembly {owner_id} 不存在"),
                    ));
                }
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("owner_kind {other:?} 不支持（仅 PART / ASSEMBLY）"),
                ));
            }
        }
        Ok(())
    }

    /// `TPartFile` → `PartFileOut`（同时注入 owner_kind）。
    fn render_out(row: &TPartFile, owner_kind: &str) -> PartFileOut {
        PartFileOut {
            id: row.id,
            owner_id: row.part_id,
            owner_kind: owner_kind.to_string(),
            kind: row.kind.clone(),
            file_type: row.file_type.clone(),
            object_key: row.object_key.clone(),
            original_filename: row.original_filename.clone(),
            file_size: row.file_size,
            content_type: row.content_type.clone(),
            upload_status: row.upload_status.clone(),
            content_sha256: row.content_sha256.clone(),
            version: row.version,
            created_at: Some(row.created_at),
            created_by: row.created_by,
        }
    }
}

/// 把 client-supplied filename 清洗为 COS object key 安全字符串：
/// - 保留 ASCII 字母 / 数字 / `.` / `-` / `_`
/// - 其它字符（含中文 / 空格）替换为 `_`
/// - 长度上限 80 字符（与 Python `re.sub(r'[^\w.-]', '_', name)[:80]` 对齐）
fn sanitize_filename(name: &str) -> String {
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

// 公开给装配体 upload-files 端点用的辅助：列出某 owner 的 part_file（不限定 kind）。
#[allow(dead_code)]
pub async fn list_part_files_for_owner(
    conn: &mut PgConnection,
    owner_kind: &str,
    owner_id: i64,
) -> Result<Vec<TPartFile>, sqlx::Error> {
    PartFileRepo::list_by_owner(conn, owner_kind, owner_id).await
}

// ===== 2026-09-15 takeover-fill：content / delete（Phase 3 补齐） =====

/// 后端代理文件二进制流：拉 `object_key` → COS `get_object` → 透传 content_type。
///
/// 权限：4 角色任意已登录（与 `get_file_with_url` 一致）。
pub struct PartFileContent {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

impl PartFileService {
    /// `GET /api/v2/part-files/{file_id}/content`。
    pub async fn get_file_content(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileContent, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let row = PartFileRepo::get_by_id(conn, file_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_NOT_FOUND,
                    format!("part_file {file_id} 不存在"),
                )
            })?;
        let bytes = cos.get_object(&row.object_key).await?;
        Ok(PartFileContent {
            bytes,
            content_type: Some(row.content_type),
        })
    }

    /// `POST /api/v2/part-files/{file_id}/delete`。
    ///
    /// 权限：按 `kind` 派生（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；
    /// G_CODE → M+CNC）。
    /// 行为：乐观锁守；UPDATE `deleted_at = now()` + `version = version + 1`；commit 后
    /// 异步调 `cos.delete_object`（失败仅 warn，不阻断）。
    pub async fn soft_delete_file(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        let row = PartFileRepo::get_by_id(&mut *conn, file_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_NOT_FOUND,
                    format!("part_file {file_id} 不存在"),
                )
            })?;
        let kind = row.kind.clone();
        let object_key = row.object_key.clone();
        // 按 kind 派生权限
        match kind.as_str() {
            "DRAWING" | "3D_MODEL" | "CAD_2D" | "SETUP_SHEET" => {
                current.require_any_role(&[Role::Manager, Role::Clerk])?;
            }
            "G_CODE" => {
                current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_PART_FILE_BAD_TYPE,
                    format!("kind={other:?} 不可软删（仅 DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET / G_CODE）"),
                ));
            }
        }

        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), \
                 version    = version + 1, \
                 updated_at = now(), \
                 updated_by = $2 \
             WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
        )
        .bind(file_id)
        .bind(current.id)
        .bind(version)
        .execute(conn)
        .await
        .map_err(AppError::from)?
        .rows_affected();
        if rows_affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part_file {file_id} 版本冲突（version={version}）"),
            ));
        }

        // COS 异步清理（best-effort；handler commit 后台跑）
        let cos = cos.clone();
        let key = object_key.clone();
        tokio::spawn(async move {
            if let Err(e) = cos.delete_object(&key).await {
                tracing::warn!(key = %key, error = %e, "part_file COS 异步清理失败（已软删）");
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_keeps_safe_chars() {
        assert_eq!(sanitize_filename("drawing.pdf"), "drawing.pdf");
        assert_eq!(sanitize_filename("DRAW-001.PDF"), "DRAW-001.PDF");
        assert_eq!(sanitize_filename("my_drawing_v2.pdf"), "my_drawing_v2.pdf");
    }

    #[test]
    fn sanitize_filename_replaces_unsafe() {
        // 空格 / 中文 → _
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
        // 中文字符被替换为 `_`（不是 fallback）：保留断言验证替换逻辑
        assert_eq!(sanitize_filename("中文"), "__");
    }
}