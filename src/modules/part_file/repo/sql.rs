//! part_file 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/part_file_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! ## 约定
//! - 全部使用运行时 `sqlx::query_as` / `sqlx::query` / `sqlx::query_scalar`
//!   （**不**用 `query!` 宏：本域运行时 SQL 改动频繁，不强依赖 `.sqlx/` 离线缓存）。
//! - 读查询一律带 `deleted_at IS NULL`（软删）
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! ## 2026-09-22 重构：从 `repo.rs` 平移到 `repo/sql.rs`
//! 本文件 SQL 与方法签名零 diff，`.sqlx/` 哈希不变（part_file 本来就不用 `query!` 宏，
//! 无 `.sqlx/query-*.json` 影响）。新增的胖 trait `PartFileRepoTrait` 在
//! `repo/mod.rs`——trait 既含本文件 ZST 6 个方法的 trait 化版本，也含跨域 owner 校验
//! helper（`part_owner_exists` / `assembly_owner_exists`）和软删 helper（`soft_delete_file`
//! / `soft_delete_active_by_part_kind`），这些 helper 的 SQL 是从原 service.rs 迁来的，
//! 字符串不变（见 `repo/mod.rs` 注释）。
//!
//! ## 为什么保留 `PartFileRepo` ZST 名
//! `assembly` / `part` / `cnc_program` 共 6+ 处直接 `PartFileRepo::xxx(&mut *conn, ...)`
//! 走 ZST 静态方法（assembly/service.rs ×3、part/service/batch.rs ×1、part/service/crud.rs ×2、
//! cnc_program/service.rs ×3），属于其他 worktree 范围（Group A/B/C/D/E），本任务**不能**
//! 破坏 `part_file::repo::PartFileRepo` 作为 ZST 的对外身份。故：
//! - `part_file::repo::PartFileRepo` —— ZST struct，保留 6 个 pub 静态方法签名不变
//!   （cross-module 调用方零修改）。
//! - `part_file::repo::PartFileRepoTrait` —— `mod.rs` 新加的胖 trait（含跨域 helper），
//!   part_file 域内部 service 用 `<R: PartFileRepoTrait>` 收。

use sqlx::PgExecutor;

use crate::modules::part_file::model::TPartFile;

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

pub struct PartFileRepo;

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
        conn: &mut sqlx::PgConnection,
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