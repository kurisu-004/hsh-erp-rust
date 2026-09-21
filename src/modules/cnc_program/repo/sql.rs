//! cnc_program 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 配对存储在 `t_part_file`（kind IN ('G_CODE','SETUP_SHEET')，`paired_file_id` 互指）。
//! 配对查询直接走运行时 `sqlx::query_as` 现写 SQL，不引入专用 repo 方法。
//!
//! ## 约定
//! - 全部使用运行时 `sqlx::query_as` / `sqlx::query`（**不**用 `query!` 宏：本域
//!   SQL 改动频繁）。
//! - 读查询一律带 `deleted_at IS NULL`（软删）。
//!
//! ## 2026-09-22 重构：从 `repo.rs` 平移到 `repo/sql.rs`
//! 本文件 SQL 与方法签名零 diff，`.sqlx/` 哈希不变（本域无 `.sqlx/query-*.json`
//! 影响）。新增的胖 trait `CncProgramRepoTrait` 在 `repo/mod.rs`——trait 既含本文件
//! `list_pairs_for_part` 的 trait 化版本，也含跨域 helper（`part_exists` +
//! `part_file_get_by_owner_kind_sha` + `part_file_create_part_file` +
//! `insert_setup_sheet_with_paired` + `set_paired_file_id`），这些 helper 的 SQL
//! 是从原 `cnc_program::service` 迁来的，字符串不变（见 `repo/mod.rs` 注释）。

use sqlx::PgConnection;

use crate::modules::part_file::model::TPartFile;

pub struct CncProgramRepo;

impl CncProgramRepo {
    /// 列出一个 part 的全部 CNC 配对（按 G_CODE created_at DESC）。
    ///
    /// 返回值用 `(g_code, setup_sheet)` 元组 Vec；service 层 DTO 化。
    /// SETUP_SHEET 通过 G_CODE 的 `paired_file_id` 反查（避免双写时序问题）。
    pub async fn list_pairs_for_part(
        conn: &mut PgConnection,
        part_id: i64,
    ) -> Result<Vec<(TPartFile, Option<TPartFile>)>, sqlx::Error> {
        let g_codes = sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE part_id = $1 AND kind = 'G_CODE' AND deleted_at IS NULL \
             ORDER BY created_at DESC",
        )
        .bind(part_id)
        .fetch_all(&mut *conn)
        .await?;
        let mut out = Vec::with_capacity(g_codes.len());
        for g in g_codes {
            let setup = if let Some(paired_id) = g.paired_file_id {
                sqlx::query_as::<_, TPartFile>(
                    "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                            file_size, content_type, upload_status, content_sha256, \
                            version, created_at, created_by, updated_at, updated_by, \
                            deleted_at, paired_file_id \
                     FROM t_part_file WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(paired_id)
                .fetch_optional(&mut *conn)
                .await?
            } else {
                None
            };
            out.push((g, setup));
        }
        Ok(out)
    }
}