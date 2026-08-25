//! part_file 域数据访问（Phase PR-CRUD）
//!
//! `PartFileRepo` 提供 `t_part_file` 的最小写入 + 查最近一条；`hash_bytes`
//! 为 `sha2` 工具函数（service 层在上传完成后用其生成 `content_sha256`，
//! 走 `uk_t_part_file_part_kind_sha` 部分唯一索引去重）。

use sqlx::PgExecutor;
use sha2::{Digest, Sha256};

use super::model::TPartFile;

pub struct PartFileRepo;

/// `create_part_file` 输入：service 层用 builder 模式注入。
///
/// `id` 由 caller 预生成雪花；`version` 走 DB 默认（0）；`created_at` /
/// `updated_at` 走 DB 默认（now）；`deleted_at` 默认 NULL。
pub struct NewPartFile<'a> {
    pub id: i64,
    pub part_id: i64,
    pub kind: &'a str,
    pub file_type: &'a str,
    pub object_key: &'a str,
    pub original_filename: &'a str,
    pub file_size: i64,
    pub content_type: &'a str,
    pub upload_status: &'a str,
    pub content_sha256: Option<&'a str>,
    pub created_by: i64,
}

impl PartFileRepo {
    /// INSERT `t_part_file`：返回写入行的雪花 `id`。
    pub async fn create_part_file<'e, E: PgExecutor<'e>>(
        executor: E,
        nf: NewPartFile<'_>,
    ) -> Result<i64, sqlx::Error> {
        let id: i64 = sqlx::query_scalar!(
            r#"
            INSERT INTO t_part_file (
                id, part_id, kind, file_type, object_key,
                original_filename, file_size, content_type,
                upload_status, content_sha256,
                created_at, created_by, updated_at, updated_by
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8,
                $9, $10,
                now(), $11, now(), $11
            )
            RETURNING id AS "id!"
            "#,
            nf.id, nf.part_id, nf.kind, nf.file_type, nf.object_key,
            nf.original_filename, nf.file_size, nf.content_type,
            nf.upload_status, nf.content_sha256, nf.created_by,
        )
        .fetch_one(executor)
        .await?;
        Ok(id)
    }

    /// 取 part + kind 下最近一条非软删的 part_file。
    ///
    /// service 层在上传同名 kind 前可先调本方法做幂等检查；或在下载时
    /// 取最新版本。多版本场景留待后续 PR。
    pub async fn get_by_part_kind<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        kind: &str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        sqlx::query_as!(
            TPartFile,
            r#"
            SELECT id, part_id, kind, file_type, object_key, original_filename,
                   file_size, content_type, upload_status, content_sha256,
                   version, created_at, created_by, updated_at, updated_by,
                   deleted_at, paired_file_id
            FROM t_part_file
            WHERE part_id = $1 AND kind = $2 AND deleted_at IS NULL
            ORDER BY created_at DESC
            LIMIT 1
            "#,
            part_id, kind,
        )
        .fetch_optional(executor).await
    }
}

/// SHA-256 hex 编码（小写 64 字符）。
///
/// service 层在上传完成后用其计算 `content_sha256`，写入 `t_part_file`；
/// `uk_t_part_file_part_kind_sha` 唯一索引据此去重。
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}