//! part_file 域数据访问（Phase 3 2026-09-14）
//!
//! `PartFileRepo` 提供 `t_part_file` 的写入 + 查询 + 列表：
//! - `create_part_file`：INSERT 雪花 id 行
//! - `get_by_id` / `get_by_part_kind` / `get_by_owner_kind_sha`：单条查询
//! - `list_with_filters` / `list_by_owner`：列表 + 过滤
//!
//! `hash_bytes` 为 SHA-256 hex 工具函数（service 层在上传完成后算 hash，
//! 走 `uk_t_part_file_part_kind_sha` 部分唯一索引去重）。
//!
//! ## 全部走运行时 `sqlx::query_as` / `sqlx::query` 而非 `query_as!` 宏
//! 编译期宏需要 `.sqlx` 离线缓存（sqlx_prepare.sh 产物）；本域运行时 SQL
//! 改动频繁（Phase 3 集中落地），不强依赖离线缓存。

use sqlx::PgConnection;
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
    /// polymorphic owner_id（part.id 或 assembly.id）。DB 列名是 `part_id`
    /// （沿用历史 schema，不引入新列）。
    pub part_id: i64,
    /// semantic owner 类型（"PART" / "ASSEMBLY"）。当前 schema 不存盘，
    /// 仅用于 service 层校验 + 拼 object_key 模板。
    pub owner_kind: &'a str,
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
        let id: i64 = sqlx::query_scalar(
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
            RETURNING id
            "#,
        )
        .bind(nf.id)
        .bind(nf.part_id)
        .bind(nf.kind)
        .bind(nf.file_type)
        .bind(nf.object_key)
        .bind(nf.original_filename)
        .bind(nf.file_size)
        .bind(nf.content_type)
        .bind(nf.upload_status)
        .bind(nf.content_sha256)
        .bind(nf.created_by)
        .fetch_one(executor)
        .await?;
        Ok(id)
    }

    /// 按 id 查单条（包含 `include_deleted` 旗标，用于"刚 INSERT 后 read-back"）。
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE id = $1 AND ($2::bool OR deleted_at IS NULL)",
        )
        .bind(id)
        .bind(include_deleted)
        .fetch_optional(executor)
        .await
    }

    /// 取 owner + kind 下最近一条非软删的 part_file。
    pub async fn get_by_part_kind<'e, E: PgExecutor<'e>>(
        executor: E,
        owner_id: i64,
        kind: &str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE part_id = $1 AND kind = $2 AND deleted_at IS NULL \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(owner_id)
        .bind(kind)
        .fetch_optional(executor)
        .await
    }

    /// CAS 去重查询：同 owner + kind + sha 是否已有活跃行。
    /// 命中 → service 层直接复用 object_key，跳过 COS PUT。
    pub async fn get_by_owner_kind_sha<'e, E: PgExecutor<'e>>(
        executor: E,
        owner_id: i64,
        kind: &str,
        sha: &str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE part_id = $1 AND kind = $2 AND content_sha256 = $3 AND deleted_at IS NULL \
             LIMIT 1",
        )
        .bind(owner_id)
        .bind(kind)
        .bind(sha)
        .fetch_optional(executor)
        .await
    }

    /// 列表（owner_kind + 可选 owner_id + 可选 kind + limit + offset）。
    /// 返回 (rows, total)。
    ///
    /// 注：`part_id` 列在 schema 里是 polymorphic（part 或 assembly）；
    /// 这里以 `part_id` 当 owner_id，配合 `owner_kind` 字符串区分语义。
    pub async fn list_with_filters(
        conn: &mut PgConnection,
        _owner_kind: &str,
        owner_id: Option<i64>,
        kind: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TPartFile>, i64), sqlx::Error> {
        // 由于 schema 用 part_id 单列（不开 polymorphic 列），这里按 part_id + kind 组合查。
        // owner_kind 仅作语义标签，不参与 SQL（part_file 域当前主要服务 PART）。
        let rows = sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE deleted_at IS NULL \
               AND ($1::bigint IS NULL OR part_id = $1) \
               AND ($2::text IS NULL OR kind = $2) \
             ORDER BY created_at DESC \
             LIMIT $3 OFFSET $4",
        )
        .bind(owner_id)
        .bind(kind)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM t_part_file \
             WHERE deleted_at IS NULL \
               AND ($1::bigint IS NULL OR part_id = $1) \
               AND ($2::text IS NULL OR kind = $2)",
        )
        .bind(owner_id)
        .bind(kind)
        .fetch_one(&mut *conn)
        .await?;
        Ok((rows, total))
    }

    /// 列出 owner 下全部 part_file（不分 kind），按创建时间降序。
    pub async fn list_by_owner<'e, E: PgExecutor<'e>>(
        executor: E,
        _owner_kind: &str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error> {
        sqlx::query_as::<_, TPartFile>(
            "SELECT id, part_id, kind, file_type, object_key, original_filename, \
                    file_size, content_type, upload_status, content_sha256, \
                    version, created_at, created_by, updated_at, updated_by, \
                    deleted_at, paired_file_id \
             FROM t_part_file \
             WHERE part_id = $1 AND deleted_at IS NULL \
             ORDER BY created_at DESC",
        )
        .bind(owner_id)
        .fetch_all(executor)
        .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_known_vector() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hash_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_bytes_output_64_chars_lowercase_hex() {
        let s = hash_bytes(b"test");
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}