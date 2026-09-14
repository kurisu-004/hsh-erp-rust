//! cnc_program 域业务逻辑（2026-09-14 Phase 3）
//!
//! 对应 Python myERP/service/cnc_program_service.py。
//!
//! ## 配对上传（cnc-pair）
//! 一次提交 G_CODE + SETUP_SHEET 两个文件：
//! 1. 校验 part 存在
//! 2. 计算两份 sha256 → 各自走 CAS 去重（命中则复用 object_key）
//! 3. 上传 COS（按 sha + owner_key 模板）
//! 4. 插入两条 `t_part_file` 行，`paired_file_id` 互相指向
//!
//! ## 列表（按 part 过滤）
//! G_CODE created_at DESC + 通过 `paired_file_id` 反查 SETUP_SHEET。

use std::sync::Arc;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::cnc_program::dto::{CncFileRef, CncPairListItem, CncPairListOut, CncPairOut};
use crate::modules::cnc_program::repo::CncProgramRepo;
use crate::modules::part_file::dto::PartFileWithUrlOut;
use crate::modules::part_file::repo::{hash_bytes, NewPartFile, PartFileRepo};
use crate::modules::part_file::service::PartFileContent;
use crate::shared::error::{code, AppError};

pub struct CncProgramService;

impl CncProgramService {
    /// 配对上传：multipart `g_code` + `setup_sheet` 两个二进制字段 + `data` JSON。
    ///
    /// Returns `CncPairOut { g_code, setup_sheet }`。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_cnc_pair(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        part_id: i64,
        g_code_bytes: Vec<u8>,
        g_code_filename: &str,
        g_code_content_type: &str,
        setup_bytes: Vec<u8>,
        setup_filename: &str,
        setup_content_type: &str,
        current: &CurrentUser,
    ) -> Result<CncPairOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;

        // 1. 校验 part 存在
        let exists: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await?;
        if exists.is_none() {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }

        // 2. 校验扩展名 / content_type
        use crate::modules::part_file::policy;
        let g_ext = policy::ext_of(g_code_filename).ok_or_else(|| {
            AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("g_code 缺少扩展名: {g_code_filename:?}"))
        })?;
        if !["tap", "nc", "gcode", "mpf", "cnc"].contains(&g_ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("g_code 扩展名 {g_ext:?} 不在白名单"),
            ));
        }
        let s_ext = policy::ext_of(setup_filename).ok_or_else(|| {
            AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("setup_sheet 缺少扩展名: {setup_filename:?}"))
        })?;
        if !["pdf"].contains(&s_ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("setup_sheet 扩展名 {s_ext:?} 必须为 pdf"),
            ));
        }

        // 3. SHA-256 + CAS
        let g_sha = hash_bytes(&g_code_bytes);
        let s_sha = hash_bytes(&setup_bytes);

        let g_existing = PartFileRepo::get_by_owner_kind_sha(&mut *conn, part_id, "G_CODE", &g_sha).await?;
        let s_existing = PartFileRepo::get_by_owner_kind_sha(&mut *conn, part_id, "SETUP_SHEET", &s_sha).await?;

        // 4. COS 上传（CAS 命中跳过）
        let (g_key, s_key) = match (&g_existing, &s_existing) {
            (Some(g), Some(s)) => (g.object_key.clone(), s.object_key.clone()),
            _ => {
                let mut g_key = String::new();
                let mut s_key = String::new();
                if g_existing.is_none() {
                    g_key = format!("part/{part_id}/G_CODE/{}_{}", &g_sha[..16], sanitize_filename(g_code_filename));
                    cos.put_object(&g_key, g_code_bytes.clone(), g_code_content_type).await?;
                }
                if s_existing.is_none() {
                    s_key = format!("part/{part_id}/SETUP_SHEET/{}_{}", &s_sha[..16], sanitize_filename(setup_filename));
                    cos.put_object(&s_key, setup_bytes.clone(), setup_content_type).await?;
                }
                (g_key, s_key)
            }
        };

        // 5. INSERT t_part_file 两条（paired_file_id 互相指向）
        // CAS 命中 → 跳过 INSERT
        let (g_id, s_id) = match (&g_existing, &s_existing) {
            (Some(g), Some(s)) => (g.id, s.id),
            _ => {
                let g_id_new = snowflake.next_id();
                let s_id_new = snowflake.next_id();
                let nf_g = NewPartFile {
                    id: g_id_new,
                    part_id,
                    owner_kind: "PART",
                    kind: "G_CODE",
                    file_type: "G_CODE",
                    object_key: &g_key,
                    original_filename: g_code_filename,
                    file_size: g_code_bytes.len() as i64,
                    content_type: g_code_content_type,
                    upload_status: "READY",
                    content_sha256: Some(&g_sha),
                    created_by: current.id,
                };
                let nf_s = NewPartFile {
                    id: s_id_new,
                    part_id,
                    owner_kind: "PART",
                    kind: "SETUP_SHEET",
                    file_type: "PDF",
                    object_key: &s_key,
                    original_filename: setup_filename,
                    file_size: setup_bytes.len() as i64,
                    content_type: setup_content_type,
                    upload_status: "READY",
                    content_sha256: Some(&s_sha),
                    created_by: current.id,
                };
                // 先插 G_CODE（拿到 id 写 SETUP_SHEET.paired_file_id）
                PartFileRepo::create_part_file(&mut *conn, nf_g).await?;
                // 再插 SETUP_SHEET，paired_file_id = g_id_new
                sqlx::query(
                    "INSERT INTO t_part_file \
                       (id, part_id, kind, file_type, object_key, original_filename, \
                        file_size, content_type, upload_status, content_sha256, \
                        paired_file_id, \
                        created_at, created_by, updated_at, updated_by) \
                     VALUES ($1, $2, 'SETUP_SHEET', $3, $4, $5, $6, $7, 'READY', $8, \
                             $9, now(), $10, now(), $10)",
                )
                .bind(nf_s.id)
                .bind(nf_s.part_id)
                .bind(nf_s.file_type)
                .bind(nf_s.object_key)
                .bind(nf_s.original_filename)
                .bind(nf_s.file_size)
                .bind(nf_s.content_type)
                .bind(nf_s.content_sha256)
                .bind(g_id_new)
                .bind(current.id)
                .execute(&mut *conn)
                .await?;
                // 互写 paired_file_id
                sqlx::query(
                    "UPDATE t_part_file SET paired_file_id = $2, \
                                              updated_at = now(), updated_by = $3 \
                     WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(g_id_new)
                .bind(s_id_new)
                .bind(current.id)
                .execute(&mut *conn)
                .await?;
                (g_id_new, s_id_new)
            }
        };

        // 6. 生成下载 URL
        let g_url = cos.presigned_get_url(&g_key, 3600).await?;
        let s_url = cos.presigned_get_url(&s_key, 3600).await?;

        Ok(CncPairOut {
            g_code: CncFileRef {
                id: g_id,
                kind: "G_CODE".to_string(),
                file_type: "G_CODE".to_string(),
                original_filename: g_code_filename.to_string(),
                file_size: g_code_bytes.len() as i64,
                content_type: g_code_content_type.to_string(),
                content_sha256: Some(g_sha),
                download_url: g_url,
                paired_file_id: Some(s_id),
            },
            setup_sheet: CncFileRef {
                id: s_id,
                kind: "SETUP_SHEET".to_string(),
                file_type: "PDF".to_string(),
                original_filename: setup_filename.to_string(),
                file_size: setup_bytes.len() as i64,
                content_type: setup_content_type.to_string(),
                content_sha256: Some(s_sha),
                download_url: s_url,
                paired_file_id: Some(g_id),
            },
        })
    }

    /// 列出 part 全部 CNC 配对（G_CODE created_at DESC + 反查 SETUP_SHEET）。
    pub async fn list_pairs_for_part(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<CncPairListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::CncProgrammer, Role::Inspector])?;
        let pairs = CncProgramRepo::list_pairs_for_part(&mut *conn, part_id).await?;
        let mut items = Vec::with_capacity(pairs.len());
        for (g, s_opt) in pairs {
            let (s_id, s_filename) = match s_opt {
                Some(s) => (s.id, s.original_filename),
                None => (0, String::new()),
            };
            items.push(CncPairListItem {
                g_code_id: g.id,
                setup_sheet_id: s_id,
                g_code_filename: g.original_filename,
                setup_sheet_filename: s_filename,
                created_at: g.created_at,
            });
        }
        let total = items.len() as i64;
        // 预签 URL 不入列表（懒加载）；单独端点取详情时再生成
        let _ = cos; // suppress unused
        Ok(CncPairListOut { items, total })
    }
}

// ===== 2026-09-15 takeover-fill：cnc-programs alias 端点（Phase 3 补齐） =====

impl CncProgramService {
    /// `GET /api/v2/cnc-programs/{file_id}/download-url` —— alias。
    ///
    /// 直接复用 `PartFileService::get_file_with_url`，不再重复业务逻辑。
    /// 权限：4 角色任意已登录（与 part_file 一致）。
    pub async fn get_download_url(
        conn: &mut PgConnection,
        cos: Arc<dyn CosClient>,
        file_id: i64,
        current: &CurrentUser,
    ) -> Result<PartFileWithUrlOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        crate::modules::part_file::service::PartFileService::get_file_with_url(
            conn, cos, file_id, current,
        )
        .await
    }

    /// `GET /api/v2/cnc-programs/{file_id}/content` —— alias。
    pub async fn get_content(
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
        crate::modules::part_file::service::PartFileService::get_file_content(
            conn, cos, file_id, current,
        )
        .await
    }

    /// `POST /api/v2/cnc-programs/{file_id}/delete` —— alias。
    ///
    /// 2026-09-15 review A2 修：转发 part_file::soft_delete_file 的 object_key，
    /// 由 handler commit 后再 `tokio::spawn(cos.delete_object(...))`，
    /// 避免 commit 失败却已触发 COS 删除。
    pub async fn delete(
        conn: &mut PgConnection,
        _cos: Arc<dyn CosClient>,
        file_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<String, AppError> {
        crate::modules::part_file::service::PartFileService::soft_delete_file(
            conn, _cos, file_id, version, current,
        )
        .await
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_basic() {
        assert_eq!(sanitize_filename("O0011.tap"), "O0011.tap");
        assert_eq!(sanitize_filename("setup sheet.pdf"), "setup_sheet.pdf");
    }
}