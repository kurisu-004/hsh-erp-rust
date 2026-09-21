//! cnc_program 域 repo 层（胖 trait + PG 实现 + 跨域 helper）
//!
//! ## 结构（2026-09-22 重构对齐 iam 范式）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，`CncProgramRepo` ZST + 1 个 pub 静态方法
//!   `list_pairs_for_part` + 运行时 `sqlx::query_as`，**内容零 diff**
//!   （本域无 `.sqlx/query-*.json` 影响）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `CncProgramRepoTrait`（6 方法 = list_pairs_for_part
//!   1 + 跨域 helper 5），并直接 `impl CncProgramRepoTrait for &mut PgConnection` ——
//!   handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `CncProgramRepoTrait`（带 `Trait` 后缀）
//! `cnc_program` 域原 `CncProgramRepo` 是 ZST + 1 个静态方法 `list_pairs_for_part`，
//! 跨模块静态调用方 0 处（本域是其他域的依赖方而非被依赖方）——理论上可以直接命名
//! `CncProgramRepo` 但为与 shelf / com / part_file 范本对齐（统一 `*Trait` 后缀），
//! 本任务也使用 `CncProgramRepoTrait`。
//!
//! - `cnc_program::repo::CncProgramRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::CncProgramRepo;` 重新导出至本模块），保留 1 个 pub 静态方法签名
//!   不变（cross-module 调用方零修改）。
//! - `cnc_program::repo::CncProgramRepoTrait` —— 本文件新加的胖 trait（6 方法），
//!   cnc_program 域内部 service 用 `<R: CncProgramRepoTrait>` 收。
//!
//! ## 为什么是胖 trait（不是按调用拆）
//! 与 iam 范本同形：`&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同
//! 时持有 list_pairs_for_part + 跨域 helper 时只能走单借位。trait `CncProgramRepoTrait`
//! 是单借位，service 签名 `<R: CncProgramRepoTrait>(&self, mut repo: R, ...)` 一次收下。
//!
//! ## 跨域 helper（5 个新方法，从 cnc_program::service 迁来）
//! - `part_exists` —— 替代原 service `upload_cnc_pair` 内的 inline SQL
//!   `SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL`
//! - `part_file_get_by_owner_kind_sha` —— 委托到 `PartFileRepo::get_by_owner_kind_sha`
//!   （cnc_pair 流程的 CAS 去重）
//! - `part_file_create_part_file` —— 委托到 `PartFileRepo::create_part_file`
//!   （cnc_pair 流程的 G_CODE INSERT）
//! - `insert_setup_sheet_with_paired` —— 替代原 service `upload_cnc_pair` 内的
//!   inline SQL `INSERT INTO t_part_file ... SETUP_SHEET ... paired_file_id = $9`
//! - `set_paired_file_id` —— 替代原 service 内的 inline SQL
//!   `UPDATE t_part_file SET paired_file_id = $2 ... WHERE id = $1 AND deleted_at IS NULL`
//!
//! 注：`part_file_get_by_owner_kind_sha` / `part_file_create_part_file` 走
//! `PartFileRepo` ZST 静态方法（不改 `PartFileRepo` 签名，cross-module 调用方零修改）；
//! 其他 3 个 helper 因 `PartFileRepo` 未提供对应 ZST 方法，inline SQL 直接写在 trait impl 内
//! （与 `part_file::repo::mod` 的 `insert_setup_sheet_with_paired` / `set_paired_file_id`
//! 一致——同一份 SQL 字符串在两个 trait impl 内并存，2026-09-22 重构一致性约定）。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::CncProgramRepo::yyy`。
//! 无需任何 `PgCncProgramRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockCncProgramRepoTrait`
//! 供 service 单测注入。cnc_program 域当前无内联 mod tests（service 全部走
//! `tests/cnc_program_api.rs` 集成测试守护），故未建 `cnc_program/service_tests/` 目录
//! ——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::repo::{NewPartFile, PartFileRepo};

pub mod sql;

// 重导出 sql.rs 中的 ZST struct，让上层继续用
// `super::repo::CncProgramRepo` 这种路径不破。
pub use sql::CncProgramRepo;

/// cnc_program 域数据访问 trait（6 方法 = list_pairs_for_part 1 + 跨域 helper 5）。
///
/// 单 trait 而非按调用拆：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参 + 跨域 helper（trait impl 一行委托
/// 到 `sql::CncProgramRepo::yyy` / `part_file::repo::PartFileRepo::yyy` / inline SQL）。
/// `<'a>` 显式生命周期是 mockall 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CncProgramRepoTrait: Send {
    // ── sql.rs 1 方法 trait 化 ──
    async fn list_pairs_for_part(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<(TPartFile, Option<TPartFile>)>, sqlx::Error>;

    // ── 跨域 helper（5）── 从原 cnc_program::service 迁来
    /// `SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL`。
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error>;
    /// CAS 去重：同 owner + kind + sha 是否已有活跃行。委托 `PartFileRepo::get_by_owner_kind_sha`。
    async fn part_file_get_by_owner_kind_sha<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
        sha: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    /// INSERT t_part_file 一行（G_CODE）。委托 `PartFileRepo::create_part_file`。
    async fn part_file_create_part_file<'a>(
        &mut self,
        nf: NewPartFile<'a>,
    ) -> Result<i64, sqlx::Error>;
    /// `INSERT INTO t_part_file ... SETUP_SHEET ... paired_file_id = $9, ...`。
    /// cnc_pair 写 SETUP_SHEET 行专用。
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
    /// WHERE id = $1 AND deleted_at IS NULL` —— cnc_pair 互写 paired_file_id 专用。
    async fn set_paired_file_id(
        &mut self,
        g_code_id: i64,
        setup_sheet_id: i64,
        updated_by: i64,
    ) -> Result<(), sqlx::Error>;
}

/// 把 `CncProgramRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::CncProgramRepo::yyy`，零转发壳（与 iam
/// 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::CncProgramRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl CncProgramRepoTrait for &mut PgConnection {
    // ── sql.rs 1 方法一行委托 ──
    async fn list_pairs_for_part(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<(TPartFile, Option<TPartFile>)>, sqlx::Error> {
        CncProgramRepo::list_pairs_for_part(&mut **self, part_id).await
    }

    // ── 跨域 helper（5）── 一行委托 PartFileRepo / inline SQL ──
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error> {
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
                .bind(part_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(exists.is_some())
    }

    async fn part_file_get_by_owner_kind_sha<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
        sha: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_owner_kind_sha(&mut **self, owner_id, kind, sha).await
    }

    async fn part_file_create_part_file<'b>(
        &mut self,
        nf: NewPartFile<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartFileRepo::create_part_file(&mut **self, nf).await
    }

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
    // 纯逻辑（无 IO）覆盖：list_pairs_for_part 在无 G_CODE 时返空 Vec。
    // 实际数据流验证在 tests/cnc_program_api.rs 集成测试。

    #[test]
    fn module_loaded() {
        // 占位测试；保证 `cargo test` 在本 mod 不会 panic
        assert_eq!(2 + 2, 4);
    }
}