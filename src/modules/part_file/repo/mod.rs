//! part_file 域 repo 层（胖 trait + PG 实现 + 跨域 helper）
//!
//! ## 结构（2026-09-22 重构对齐 iam 范式）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，6 个 pub 固有静态方法 + 运行时 `sqlx::query_as` /
//!   `sqlx::query` / `sqlx::query_scalar`（**不**用 `query!` 宏，本域 SQL 改动频繁），
//!   **内容零 diff**（part_file 本无 `.sqlx/query-*.json` 缓存故无哈希影响）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `PartFileRepoTrait`（12 方法合并单 trait；
//!   含 sql.rs 6 方法 trait 化 + 跨域 owner 校验 2 + 软删 2 + cnc_program 互写
//!   paired_file_id 2），并直接 `impl PartFileRepoTrait for &mut PgConnection` ——
//!   handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `PartFileRepoTrait`（带 `Trait` 后缀）
//! `assembly` / `part` / `cnc_program` 共 6+ 处直接 `PartFileRepo::xxx(&mut *conn, ...)`
//! 走 ZST 静态方法（assembly/service.rs ×3、part/service/batch.rs ×1、part/service/crud.rs ×2、
//! cnc_program/service.rs ×3），属于其他 worktree 范围（Group A/B/C/D/E），本任务**不能**
//! 破坏 `part_file::repo::PartFileRepo` 作为 ZST 的对外身份。故：
//! - `part_file::repo::PartFileRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::PartFileRepo;` 重新导出至本模块），保留 6 个 pub 静态方法签名不变
//!   （cross-module 调用方零修改）。
//! - `part_file::repo::PartFileRepoTrait` —— 本文件新加的胖 trait（12 方法），part_file 域
//!   内部 service 用 `<R: PartFileRepoTrait>` 收。
//!
//! ## 为什么是胖 trait 而不是按表拆
//! 与 iam 范本同形（见 `iam/repo/mod.rs` §「为什么是胖 trait」）：`&mut PgConnection` 同一
//! 作用域只能借给一个 repo 实例；service 同时持有 part_file_repo + 跨域 owner 校验时只能
//! 走单借位。trait `PartFileRepoTrait` 是单借位，service 签名
//! `<R: PartFileRepoTrait>(&self, mut repo: R, ...)` 一次收下。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::PartFileRepo::yyy`。
//! 无需任何 `PgPartFileRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## 跨域 helper（4 个新方法，从 service.rs 迁来）
//! - `part_owner_exists` / `assembly_owner_exists` —— 替代原 service `assert_owner_exists`
//!   内的 inline SQL `SELECT id FROM t_part/t_assembly WHERE id=$1 AND deleted_at IS NULL`
//! - `soft_delete_file` —— 替代原 service `soft_delete_file` 内的 inline SQL
//!   `UPDATE t_part_file SET deleted_at = now(), version = version + 1, ... WHERE id=$1
//!   AND version=$3 AND deleted_at IS NULL`
//! - `soft_delete_active_by_part_kind` —— 替代原 service `bind_uploaded_file` 内的
//!   inline SQL（单文件 kind 的旧活跃行 soft_delete）
//! - `insert_setup_sheet_with_paired` / `set_paired_file_id` —— 给 cnc_program 域
//!   `upload_cnc_pair` 用：原 service 内 inline `INSERT INTO t_part_file ... SETUP_SHEET
//!   ... paired_file_id = $9` 与 `UPDATE t_part_file SET paired_file_id = $2 ...`
//!   SQL 字符串零 diff 迁入 trait impl。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockPartFileRepoTrait`
//! 供 service 单测注入。part_file 域当前无内联 mod tests（service 全部走
//! `tests/part_file_api.rs` + `tests/parts_api.rs` + `tests/assembly_api.rs` 集成测试守护），
//! 故未建 `part_file/service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`create_part_file` 检
//! SQLSTATE 23505 → 21108 BIZ_PART_FILE_DUPLICATE 在 service 层做）。

use async_trait::async_trait;
use sqlx::PgConnection;

use super::model::TPartFile;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型 / NewPartFile，让上层继续用
// `super::repo::{PartFileRepo, NewPartFile, hash_bytes}` 这种路径不破。
pub use sql::{NewPartFile, PartFileRepo};

/// SHA-256 hex 编码（小写 64 字符）。
///
/// service 层在上传完成后用其计算 `content_sha256`，写入 `t_part_file`；
/// `uk_t_part_file_part_kind_sha` 唯一索引据此去重。
pub fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// part_file 域数据访问 trait（12 方法 = sql.rs 6 + 跨域 owner 校验 2 + 软删 2 + cnc_pair 2）。
///
/// 单 trait 而非每表一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参 + 跨域 helper（trait impl 一行委托
/// 到 `sql::PartFileRepo::yyy` 或 inline SQL）。`<'a>` 显式生命周期是 mockall 0.15
/// automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PartFileRepoTrait: Send {
    // ── sql.rs 6 方法 trait 化 ──
    async fn create_part_file<'a>(
        &mut self,
        nf: NewPartFile<'a>,
    ) -> Result<i64, sqlx::Error>;
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    async fn get_by_part_kind<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    async fn get_by_owner_kind_sha<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
        sha: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        owner_kind: &'a str,
        owner_id: Option<i64>,
        kind: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TPartFile>, i64), sqlx::Error>;
    async fn list_by_owner<'a>(
        &mut self,
        owner_kind: &'a str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error>;

    // ── 跨域 owner 校验（2）── 从原 service `assert_owner_exists` 迁来
    /// `SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL`。
    async fn part_owner_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error>;
    /// `SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL`。
    async fn assembly_owner_exists(
        &mut self,
        assembly_id: i64,
    ) -> Result<bool, sqlx::Error>;

    // ── 软删 helper（2）── 从原 service `soft_delete_file` / `bind_uploaded_file` 迁来
    /// 单条软删（带版本号 OCC）：原 `soft_delete_file` 的 UPDATE 语句。
    /// 返回 `rows_affected`：0 ⇒ version 冲突。
    async fn soft_delete_file(
        &mut self,
        file_id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    /// 按 owner + kind 软删旧活跃行：原 `bind_uploaded_file` 的 UPDATE 语句。
    /// `uk_t_part_file_single` 部分唯一约束的支撑方法。
    async fn soft_delete_active_by_part_kind<'a>(
        &mut self,
        part_id: i64,
        updated_by: i64,
        kind: &'a str,
    ) -> Result<u64, sqlx::Error>;

    // ── cnc_pair 写入 helper（2）── 从 cnc_program::service 迁来（SQL 字符串零 diff）
    /// `INSERT INTO t_part_file ... SETUP_SHEET ... paired_file_id = $9, ...`。
    /// cnc_program::upload_cnc_pair 写 SETUP_SHEET 行专用。
    #[allow(clippy::too_many_arguments)]
    async fn insert_setup_sheet_with_paired<'a>(
        &mut self,
        id: i64,
        part_id: i64,
        file_type: &'a str,
        object_key: &'a str,
        original_filename: &'a str,
        file_size: i64,
        content_type: &'a str,
        content_sha256: Option<&'a str>,
        paired_file_id: i64,
        created_by: i64,
    ) -> Result<(), sqlx::Error>;
    /// `UPDATE t_part_file SET paired_file_id = $2, updated_at = now(), updated_by = $3
    /// WHERE id = $1 AND deleted_at IS NULL` —— cnc_program::upload_cnc_pair 互写
    /// paired_file_id 专用。
    async fn set_paired_file_id(
        &mut self,
        g_code_id: i64,
        setup_sheet_id: i64,
        updated_by: i64,
    ) -> Result<(), sqlx::Error>;
}

/// 把 `PartFileRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::PartFileRepo::yyy`，零转发壳（与 iam
/// 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::PartFileRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl PartFileRepoTrait for &mut PgConnection {
    // ── sql.rs 6 方法一行委托 ──
    async fn create_part_file<'b>(
        &mut self,
        nf: NewPartFile<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartFileRepo::create_part_file(&mut **self, nf).await
    }

    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn get_by_part_kind<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_part_kind(&mut **self, owner_id, kind).await
    }

    async fn get_by_owner_kind_sha<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
        sha: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_owner_kind_sha(&mut **self, owner_id, kind, sha).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        owner_kind: &'b str,
        owner_id: Option<i64>,
        kind: Option<&'b str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TPartFile>, i64), sqlx::Error> {
        PartFileRepo::list_with_filters(&mut **self, owner_kind, owner_id, kind, limit, offset).await
    }

    async fn list_by_owner<'b>(
        &mut self,
        owner_kind: &'b str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error> {
        PartFileRepo::list_by_owner(&mut **self, owner_kind, owner_id).await
    }

    // ── 跨域 owner 校验（2）── inline SQL（迁自原 service `assert_owner_exists`）
    async fn part_owner_exists(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error> {
        let exists: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(exists.is_some())
    }

    async fn assembly_owner_exists(
        &mut self,
        assembly_id: i64,
    ) -> Result<bool, sqlx::Error> {
        let exists: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(assembly_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(exists.is_some())
    }

    // ── 软删 helper（2）── inline SQL（迁自原 service）
    async fn soft_delete_file(
        &mut self,
        file_id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), \
                 version    = version + 1, \
                 updated_at = now(), \
                 updated_by = $2 \
             WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
        )
        .bind(file_id)
        .bind(updated_by)
        .bind(version)
        .execute(&mut **self)
        .await?
        .rows_affected();
        Ok(rows_affected)
    }

    async fn soft_delete_active_by_part_kind<'b>(
        &mut self,
        part_id: i64,
        updated_by: i64,
        kind: &'b str,
    ) -> Result<u64, sqlx::Error> {
        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), version = version + 1, updated_at = now(), updated_by = $2 \
             WHERE part_id = $1 AND kind = $3 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .bind(updated_by)
        .bind(kind)
        .execute(&mut **self)
        .await?
        .rows_affected();
        Ok(rows_affected)
    }

    // ── cnc_pair 写入 helper（2）── inline SQL（迁自 cnc_program::service，零 diff）
    #[allow(clippy::too_many_arguments)]
    async fn insert_setup_sheet_with_paired<'b>(
        &mut self,
        id: i64,
        part_id: i64,
        file_type: &'b str,
        object_key: &'b str,
        original_filename: &'b str,
        file_size: i64,
        content_type: &'b str,
        content_sha256: Option<&'b str>,
        paired_file_id: i64,
        created_by: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO t_part_file \
               (id, part_id, kind, file_type, object_key, original_filename, \
                file_size, content_type, upload_status, content_sha256, \
                paired_file_id, \
                created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, 'SETUP_SHEET', $3, $4, $5, $6, $7, 'READY', $8, \
                     $9, now(), $10, now(), $10)",
        )
        .bind(id)
        .bind(part_id)
        .bind(file_type)
        .bind(object_key)
        .bind(original_filename)
        .bind(file_size)
        .bind(content_type)
        .bind(content_sha256)
        .bind(paired_file_id)
        .bind(created_by)
        .execute(&mut **self)
        .await?;
        Ok(())
    }

    async fn set_paired_file_id(
        &mut self,
        g_code_id: i64,
        setup_sheet_id: i64,
        updated_by: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE t_part_file SET paired_file_id = $2, \
                                      updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(g_code_id)
        .bind(setup_sheet_id)
        .bind(updated_by)
        .execute(&mut **self)
        .await?;
        Ok(())
    }
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
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}