//! cnc_program 域业务逻辑（2026-09-14 Phase 3 + 2026-09-15 takeover-fill + 2026-09-22 对齐 iam 范式）
//!
//! 对应 Python myERP/service/cnc_program_service.py。
//!
//! ## 配对上传（cnc-pair）
//! 一次提交 G_CODE + SETUP_SHEET 两个文件：
//! 1. 校验 part 存在（`repo.part_exists`）
//! 2. 计算两份 sha256 → 各自走 CAS 去重（命中则复用 object_key，委托 `PartFileRepo`）
//! 3. 上传 COS（按 sha + owner_key 模板）
//! 4. 插入两条 `t_part_file` 行，`paired_file_id` 互相指向
//!
//! ## 列表（按 part 过滤）
//! G_CODE created_at DESC + 通过 `paired_file_id` 反查 SETUP_SHEET。
//!
//! ## 3 个 alias 端点的处理（2026-09-22 重构）
//! 原 `CncProgramService::get_download_url` / `get_content` / `delete` 三个 alias
//! 方法**已删除**——handler 直接调用 `state.part_file_service.method(&mut tx, ...)`，
//! 不再走 `CncProgramService` 转发（spec §C-1 决策点 1：避免 handler 收两个 service；
//! 与 shelf picker 跨域 helper 模式区分）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`（by-value；
//! `R: CncProgramRepoTrait`）参数传入——handler/service 借 `&mut *tx` / `&mut *conn`
//! 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。service 不知事务——
//! handler `pool.begin()` + `tx.commit()` 包外。
//!
//! `CncProgramService` 字段仅 `snowflake` + `cos`（事务已移交 handler）；实例为轻壳，
//! 可直接 `Arc<CncProgramService>` 存 `AppState`；方法签名
//! `<R: CncProgramRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::cnc_program::dto::{CncFileRef, CncPairListItem, CncPairListOut, CncPairOut};
use crate::modules::cnc_program::repo::CncProgramRepoTrait;
use crate::modules::part_file::policy;
use crate::modules::part_file::repo::{NewPartFile, hash_bytes};
use crate::shared::error::{AppError, code};

pub struct CncProgramService {
    snowflake: Arc<SnowflakeIdGenerator>,
    cos: Arc<dyn CosClient>,
}

impl CncProgramService {
    /// 构造：雪花 ID 生成器 + COS 客户端（2026-09-22 重构：cos 从 handler 形参
    /// 收归到 service 字段，service 自管 cos 不变量）。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>, cos: Arc<dyn CosClient>) -> Self {
        Self { snowflake, cos }
    }

    /// 配对上传：multipart `g_code` + `setup_sheet` 两个二进制字段 + `data` JSON。
    ///
    /// Returns `CncPairOut { g_code, setup_sheet }`。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_cnc_pair<R: CncProgramRepoTrait>(
        &self,
        mut repo: R,
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

        // 1. 校验 part 存在（走 trait helper）
        if !repo.part_exists(part_id).await? {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }

        // 2. 校验扩展名 / content_type
        let g_ext = policy::ext_of(g_code_filename).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("g_code 缺少扩展名: {g_code_filename:?}"),
            )
        })?;
        if !["tap", "nc", "gcode", "mpf", "cnc"].contains(&g_ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("g_code 扩展名 {g_ext:?} 不在白名单"),
            ));
        }
        let s_ext = policy::ext_of(setup_filename).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("setup_sheet 缺少扩展名: {setup_filename:?}"),
            )
        })?;
        if !["pdf"].contains(&s_ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("setup_sheet 扩展名 {s_ext:?} 必须为 pdf"),
            ));
        }

        // 3. SHA-256 + CAS（走 trait helper 委托到 PartFileRepo）
        let g_sha = hash_bytes(&g_code_bytes);
        let s_sha = hash_bytes(&setup_bytes);

        let g_existing = repo
            .part_file_get_by_owner_kind_sha(part_id, "G_CODE", &g_sha)
            .await?;
        let s_existing = repo
            .part_file_get_by_owner_kind_sha(part_id, "SETUP_SHEET", &s_sha)
            .await?;

        // 4. COS 上传（CAS 命中跳过）
        let (g_key, s_key) = match (&g_existing, &s_existing) {
            (Some(g), Some(s)) => (g.object_key.clone(), s.object_key.clone()),
            _ => {
                let mut g_key = String::new();
                let mut s_key = String::new();
                if g_existing.is_none() {
                    g_key = format!(
                        "part/{part_id}/G_CODE/{}_{}",
                        &g_sha[..16],
                        sanitize_filename(g_code_filename)
                    );
                    self.cos
                        .put_object(&g_key, g_code_bytes.clone(), g_code_content_type)
                        .await?;
                }
                if s_existing.is_none() {
                    s_key = format!(
                        "part/{part_id}/SETUP_SHEET/{}_{}",
                        &s_sha[..16],
                        sanitize_filename(setup_filename)
                    );
                    self.cos
                        .put_object(&s_key, setup_bytes.clone(), setup_content_type)
                        .await?;
                }
                (g_key, s_key)
            }
        };

        // 5. INSERT t_part_file 两条（paired_file_id 互相指向）
        // CAS 命中 → 跳过 INSERT
        let (g_id, s_id) = match (&g_existing, &s_existing) {
            (Some(g), Some(s)) => (g.id, s.id),
            _ => {
                let g_id_new = self.snowflake.next_id();
                let s_id_new = self.snowflake.next_id();
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
                // 先插 G_CODE（拿到 id 写 SETUP_SHEET.paired_file_id）
                repo.part_file_create_part_file(nf_g).await?;
                // 再插 SETUP_SHEET，paired_file_id = g_id_new（走 trait helper）
                repo.insert_setup_sheet_with_paired(
                    s_id_new,
                    part_id,
                    "PDF",
                    &s_key,
                    setup_filename,
                    setup_bytes.len() as i64,
                    setup_content_type,
                    Some(&s_sha),
                    g_id_new,
                    current.id,
                )
                .await?;
                // 互写 paired_file_id（走 trait helper）
                repo.set_paired_file_id(g_id_new, s_id_new, current.id).await?;
                (g_id_new, s_id_new)
            }
        };

        // 6. 生成下载 URL
        let g_url = self.cos.presigned_get_url(&g_key, 3600).await?;
        let s_url = self.cos.presigned_get_url(&s_key, 3600).await?;

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
    pub async fn list_pairs_for_part<R: CncProgramRepoTrait>(
        &self,
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<CncPairListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let pairs = repo.list_pairs_for_part(part_id).await?;
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
        let _ = &self.cos; // suppress unused
        Ok(CncPairListOut { items, total })
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