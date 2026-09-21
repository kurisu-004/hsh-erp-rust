//! shelf ↔ process 映射（`t_shelf_process`）SQL 真源
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
//! ## 与 `shelf/repo` 的协作
//! `t_shelf_process` 的方法**也**在胖 trait `ShelfRepo`（`shelf/repo/mod.rs`）里
//! 重新声明——handler/service 借 `&mut *tx` / `&mut *conn` 直接调胖 trait，
//! trait impl 一行委托到本文件的 `ShelfProcessRepo` 静态方法。

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
// ShelfProcessRepo（t_shelf_process，4 方法）
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
}