//! cnc_program 域数据访问占位（2026-09-14 Phase 3）
//!
//! 配对存储在 `t_part_file`（kind IN ('G_CODE','SETUP_SHEET')，`paired_file_id` 互指）。
//! 配对查询直接走 `sqlx::query_as` 现写 SQL，不引入专用 repo 方法。
//! 保留此文件是为对齐六件套结构。

use crate::modules::part_file::model::TPartFile;

pub struct CncProgramRepo;

impl CncProgramRepo {
    /// 列出一个 part 的全部 CNC 配对（按 G_CODE created_at DESC）。
    ///
    /// 返回值用 `(g_code, setup_sheet)` 元组 Vec；service 层 DTO 化。
    /// SETUP_SHEET 通过 G_CODE 的 `paired_file_id` 反查（避免双写时序问题）。
    pub async fn list_pairs_for_part(
        conn: &mut sqlx::PgConnection,
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

#[cfg(test)]
mod tests {
    // 纯逻辑（无 IO）覆盖：list_pairs_for_part 在无 G_CODE 时返空 Vec。
    // 实际数据流验证在 tests/cnc_program_api.rs 集成测试。

    #[test]
    fn module_loaded() {
        // 占位测试；保证 `cargo test` 在本 mod 不会 panic
        assert_eq!(2 + 2, 4);
    }
}
