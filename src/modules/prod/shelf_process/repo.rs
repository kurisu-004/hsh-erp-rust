//! shelf ↔ process 映射（`t_shelf_process`）SQL 真源 —— 物理在 `prod` 子模块
//!
//! 对应 Python myERP（无单独 shelf_process 仓储；逻辑在 shelf_repository 内部）。
//! 函数签名接收 `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` /
//! `&mut Transaction`。
//!
//! ## 约定
//! - 全部使用 sqlx 编译期宏（`query!` / `query_as!`）或运行时宏（`query_as` +
//!   `bind`），需 `DATABASE_URL` 或 `.sqlx/` 离线元数据
//! - 读查询一律带 `deleted_at IS NULL`
//! - 软删 `deleted_at = now()`，无乐观锁（mapping 由 set_shelf_processes 整组替换）
//!
//! ## 2026-09-22 重构
//! 原 `process_mapping.rs`（平级文件）拆分到 `process_mapping/{mod.rs, sql.rs}`，
//! 本文件 SQL 与方法签名零 diff，`.sqlx/query-*.json` 哈希不变。
//!
//! ## 2026-10-02 域归属反转（shelf 域拆分）
//! 原路径 `src/modules/shelf/process_mapping/sql.rs` —— 货架自身不含工序概念，
//! 按后端域规约搬到 `src/modules/prod/shelf_process/repo.rs`：
//! - 4 个「平移」方法（`list_by_shelf` / `list_all_active_mappings` /
//!   `soft_delete_all_for_shelf` / `bulk_insert`）SQL 与方法签名**零 diff**
//! - 新增 2 个「收口」方法（`find_first_shelf_for_process` /
//!   `exists_for_shelf_process`）供 prod 域内部调用方改调，消灭手写 `t_shelf_process`
//!   SQL（`prod::batch` / `prod::worker_pool` 各 1 处）
//!
//! ## 本仓内保留 inline 的 `t_shelf_process` SQL（2026-10-02 判定，不要硬抽）
//! - `prod::batch::repo::preview_auto_dispatch` —— `LEFT JOIN LATERAL t_shelf_process`
//!   在大复合查询里，拆出来是性能回退
//! - `prod::process::repo::count_process_references` —— 5 张表 sub-select 求和，
//!   拆出来多 5 次往返
//! - part 域 3 处（`worker_scan.rs` / `phase1/mod.rs` /
//!   `pending_programming_sql.rs`）—— 属 part 域，不在本次拆分范围

use sqlx::PgExecutor;

use crate::infra::snowflake::SnowflakeIdGenerator;

/// 新 mapping 行的输入结构（service 层用，喂给 `ShelfProcessRepo::bulk_insert`）。
#[derive(Debug, Clone)]
pub struct NewShelfProcessRow {
    pub shelf_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// ShelfProcessRepo（t_shelf_process，6 方法 = 平移 4 + 新增 2）
// ---------------------------------------------------------------------------

pub struct ShelfProcessRepo;

impl ShelfProcessRepo {
    /// 按 `shelf_id` 取所有 active mapping（按 sort_order ASC, id ASC）。
    pub async fn list_by_shelf<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_id: i64,
    ) -> Result<Vec<(i64, i64, i32, String, String)>, sqlx::Error> {
        // 返回 (shelf_id, process_id, sort_order, shelf_code, process_code) —— 单 JOIN
        sqlx::query_as(
            r#"
            SELECT sp.shelf_id, sp.process_id, sp.sort_order,
                   s.code AS shelf_code, p.code AS process_code
            FROM t_shelf_process sp
            JOIN t_shelf s ON s.id = sp.shelf_id AND s.deleted_at IS NULL
            JOIN t_process p ON p.id = sp.process_id AND p.deleted_at IS NULL
            WHERE sp.shelf_id = $1
              AND sp.deleted_at IS NULL
            ORDER BY sp.sort_order ASC, sp.id ASC
            "#,
        )
        .bind(shelf_id)
        .fetch_all(executor)
        .await
    }

    /// 批量取所有 active shelves 的 mapping：单条 JOIN 返回所有 active shelf
    /// ↔ process 行（防 N+1）。
    pub async fn list_all_active_mappings<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<(i64, i64, String, String)>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT sp.shelf_id, sp.process_id,
                   s.code AS shelf_code, p.code AS process_code
            FROM t_shelf_process sp
            JOIN t_shelf s ON s.id = sp.shelf_id AND s.deleted_at IS NULL
            JOIN t_process p ON p.id = sp.process_id AND p.deleted_at IS NULL
            WHERE sp.deleted_at IS NULL
              AND s.is_active = true
            ORDER BY sp.shelf_id ASC, sp.sort_order ASC
            "#,
        )
        .fetch_all(executor)
        .await
    }

    /// 软删一个 shelf 的全部 active mapping（同事务内与 INSERT 配对）。
    pub async fn soft_delete_all_for_shelf<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_id: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE t_shelf_process
            SET deleted_at = now()
            WHERE shelf_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(shelf_id)
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 批量插入新 mapping：单条 INSERT ... VALUES (...), (...), (...)。
    ///
    /// 空切片短路返回 0 行（与 Python `set_shelf_processes` 「传空数组 = 清空」
    /// 语义对齐；service 层若要清空映射仍应走 set_shelf_processes + 空 items）。
    pub async fn bulk_insert<'e, E: PgExecutor<'e>>(
        executor: E,
        rows: &[NewShelfProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }

        // sqlx::QueryBuilder 拼 INSERT ... VALUES (...), (...), ...；
        // 单条往返即可写入全部行（防 N+1）。
        use sqlx::QueryBuilder;
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, created_by, updated_by) ",
        );
        qb.push_values(rows.iter(), |mut b, row| {
            let id = snowflake.next_id();
            b.push_bind(id)
                .push_bind(row.shelf_id)
                .push_bind(row.process_id)
                .push_bind(row.sort_order)
                .push_bind(created_by)
                .push_bind(created_by);
        });
        qb.build()
            .execute(executor)
            .await
            .map(|r| r.rows_affected())
    }

    /// 按 `process_id` 取首条 active 货架映射（多结果取 sort_order 最小者）。
    ///
    /// 2026-10-02 新增：原为 `prod::batch::repo::find_first_shelf_for_process`
    /// （`src/modules/prod/batch/repo.rs`）的手写 SQL，随 shelf↔process 映射搬到本
    /// 文件作为 SQL 真源，调用方 `prod::batch::service::dispatch_single` 改调本方法。
    /// SQL 逐字保留，0 结果 → `Ok(None)`（由 service 层映射 `BIZ_SHELF_PROCESS_NOT_FOUND`）。
    ///
    /// 不带 `is_active` 守卫（车间 active 货架默认软删）；后续如需守卫再加。
    pub async fn find_first_shelf_for_process<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT shelf_id
            FROM t_shelf_process
            WHERE process_id = $1 AND deleted_at IS NULL
            ORDER BY sort_order ASC, id ASC
            LIMIT 1
            "#,
        )
        .bind(process_id)
        .fetch_optional(executor)
        .await?;
        Ok(row)
    }

    /// 存在性检查：该 shelf 是否映射了该 process。
    ///
    /// 2026-10-02 新增：原为 `prod::worker_pool::service::move_batch` WORKER→POOL
    /// 分支里的内联 SQL
    /// `SELECT shelf_id FROM t_shelf_process WHERE shelf_id=$1 AND process_id=$2
    ///  AND deleted_at IS NULL ORDER BY sort_order ASC, id ASC LIMIT 1` +
    /// `mapped.is_none()` 判定 —— 该写法是**恒真式**（只 SELECT 一列后判空，实际只判
    /// 「是否存在」，拿到的 `shelf_id` 恒等于入参 `$1`，`ORDER BY … LIMIT 1` 也是
    /// 冗余）。本方法改用 `SELECT EXISTS(…)` 把「存在性」语义显式化，与原逻辑
    /// **语义等价**（同一组 WHERE 谓词 + 同一 `deleted_at IS NULL` 守卫），20507
    /// `BIZ_SHELF_PROCESS_NOT_MAPPED` 的触发条件与文案保持不变。
    pub async fn exists_for_shelf_process<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_id: i64,
        process_id: i64,
    ) -> Result<bool, sqlx::Error> {
        let exists: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM t_shelf_process
                WHERE shelf_id = $1 AND process_id = $2 AND deleted_at IS NULL
            )
            "#,
        )
        .bind(shelf_id)
        .bind(process_id)
        .fetch_one(executor)
        .await?;
        Ok(exists)
    }
}
