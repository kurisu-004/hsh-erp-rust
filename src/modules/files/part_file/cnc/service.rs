//! `part_file::cnc` 子模块业务逻辑（2026-09-14 Phase 3 + 2026-09-15 takeover-fill
//! + 2026-09-22 对齐 iam 范式 + 2026-10-13 收编进 files 域）
//!
//! 对应 Python myERP/service/cnc_program_service.py。
//!
//! ## 配对上传（cnc-pair）
//! 一次提交 G_CODE + SETUP_SHEET 两个文件：
//! 1. 校验 part 存在（`repo.part_owner_exists`）
//! 2. 计算两份 sha256 → 各自走 CAS 去重（命中则复用 object_key）
//! 3. 上传 COS（按 sha + owner_key 模板）
//! 4. 插入两条 `t_part_file` 行，`paired_file_id` 互相指向
//!
//! ## 2026-10-13：repo 层取消，本子模块直接用父模块 repo
//!
//! 原独立的 `CncProgramRepoTrait`（6 方法）整目录删除，泛型参数换成本模块的父模块
//! trait `parent::repo::PartFileRepoTrait`。方法对照：
//!
//! | 旧 `CncProgramRepoTrait` 方法 | 新对应物 |
//! |---|---|
//! | `part_exists` | `PartFileRepoTrait::part_owner_exists`（SQL 逐字相同） |
//! | `part_file_get_by_owner_kind_sha` | `PartFileRepoTrait::get_by_owner_kind_sha` |
//! | `part_file_create_part_file` | `PartFileRepoTrait::create_part_file` |
//! | `insert_setup_sheet_with_paired` | `PartFileRepoTrait::insert_setup_sheet_with_paired` |
//! | `set_paired_file_id` | `PartFileRepoTrait::set_paired_file_id` |
//! | `list_pairs_for_part` | **删除**（端点已下线，见 `mod.rs` 移除记录） |
//!
//! ⚠️ 后两条的 SQL 字符串原本在 `cnc/repo/mod.rs` 与 `parent::repo/mod.rs` **两个
//! trait impl 内逐字重复**（2026-09-22 重构时的一致性约定，本就是同一份 SQL 写两遍）。
//! 删掉一侧后只剩 `parent::repo` 一份真源。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式，2026-10-13 不变）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`（by-value；
//! `R: PartFileRepoTrait`）参数传入——handler/service 借 `&mut *tx` / `&mut *conn`
//! 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。service 不知事务——
//! handler `pool.begin()` + `tx.commit()` 包外。
//!
//! `CncProgramService` 字段仅 `snowflake` + `cos`（事务已移交 handler）；实例为轻壳，
//! 可直接 `Arc<CncProgramService>` 存 `AppState`；方法签名
//! `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::files::part_file::cnc::vo::{CncFileRef, CncPairOut};
use crate::modules::files::part_file::policy;
use crate::modules::files::part_file::repo::{NewPartFile, PartFileRepoTrait, hash_bytes};
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
    pub async fn upload_cnc_pair<R: PartFileRepoTrait>(
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
        if !repo.part_owner_exists(part_id).await? {
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
            .get_by_owner_kind_sha(part_id, "G_CODE", &g_sha)
            .await?;
        let s_existing = repo
            .get_by_owner_kind_sha(part_id, "SETUP_SHEET", &s_sha)
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
                repo.create_part_file(nf_g).await?;
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
                repo.set_paired_file_id(g_id_new, s_id_new, current.id)
                    .await?;
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
}

// 2026-10-13：`list_pairs_for_part`（CNC 配对列表）随 `GET /cnc-programs/parts/{part_id}`
// 端点下线一并删除 —— 零前端调用方，前端配对分组改由
// `GET /files/objects?owner_kind=PART&owner_id=…&kind=G_CODE` 驱动（列表项带
// `paired_file_id`）。本方法原是本子模块唯一用到 `self.cos` 之外的路径，删除后
// `CncProgramService` 的 `cos` 字段仍被 `upload_cnc_pair` 的 `put_object` /
// `presigned_get_url` 使用。

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
