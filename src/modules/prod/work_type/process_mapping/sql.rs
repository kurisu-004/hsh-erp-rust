//! work_type ↔ process 映射（`t_work_type_process`）SQL 真源
//!
//! 对应 Python myERP（无单独 work_type_process 仓储；逻辑在 work_type_repository 内部）。
//! 函数签名接收 `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` /
//! `&mut Transaction`。
//!
//! ## 约定
//! - 全部使用 sqlx 编译期宏（`query!` / `query_as!`）或运行时宏（`query_as` +
//!   `bind`），需 `DATABASE_URL` 或 `.sqlx/` 离线元数据
//! - 读查询一律带 `deleted_at IS NULL`
//! - 软删 `deleted_at = now()`，无乐观锁（mapping 由 set_work_type_processes 整组替换）
//!
//! ## 2026-09-22 D-2-simple 重构
//! 原 `process_mapping.rs`（平级文件）拆分到 `process_mapping/{mod.rs, sql.rs}`，
//! 本文件 SQL 与方法签名零 diff，`.sqlx/query-*.json` 哈希不变。
//!
//! ## 与 `work_type/repo` 的协作
//! `t_work_type_process` 的方法**也**在胖 trait `WorkTypeRepoTrait`
//! （`work_type/repo/mod.rs`）里重新声明——handler/service 借 `&mut *tx` /
//! `&mut *conn` 直接调胖 trait，trait impl 一行委托到本文件的 `WorkTypeProcessRepo`
//! 静态方法。决策方案 A（推荐）—— 单一胖 trait，单 service 签名
//! `<R: WorkTypeRepoTrait>`，单 mock。

use sqlx::PgExecutor;

use crate::infra::snowflake::SnowflakeIdGenerator;

/// 新 mapping 行的输入结构（service 层用，喂给 `WorkTypeProcessRepo::bulk_insert`）。
#[derive(Debug, Clone)]
pub struct NewWorkTypeProcessRow {
    pub work_type_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// WorkTypeProcessRepo（t_work_type_process，4 方法）
// ---------------------------------------------------------------------------

pub struct WorkTypeProcessRepo;

impl WorkTypeProcessRepo {
    /// 按 `work_type_id` 取所有 active mapping（按 sort_order ASC）。
    /// `t_work_type_process` 无业务软删（mapping 表通常保留历史），不筛 `deleted_at`。
    pub async fn list_by_work_type<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<Vec<(i64, i32, String)>, sqlx::Error> {
        // 返回 (process_id, sort_order, process_code) —— 单 JOIN
        sqlx::query_as(
            r#"
            SELECT wtp.process_id, wtp.sort_order, p.code AS process_code
            FROM t_work_type_process wtp
            JOIN t_process p ON p.id = wtp.process_id AND p.deleted_at IS NULL
            WHERE wtp.work_type_id = $1
            ORDER BY wtp.sort_order ASC, wtp.id ASC
            "#,
        )
        .bind(work_type_id)
        .fetch_all(executor)
        .await
    }

    /// 批量取一组工种的全部 process_id 列表（防 N+1）：
    /// 单条 SQL 返回 `[work_type_id, process_id, process_code, sort_order]`
    /// 用于 `WorkTypeService::list_work_types` 一次性补齐 `process_ids` 字段。
    pub async fn list_by_work_types_batch<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_ids: &[i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        if work_type_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            r#"
            SELECT work_type_id, process_id
            FROM t_work_type_process
            WHERE work_type_id = ANY($1)
            ORDER BY work_type_id, sort_order, id
            "#,
        )
        .bind(work_type_ids)
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }

    /// 软删一个 work_type 的全部 active mapping（同事务内与 INSERT 配对）。
    pub async fn soft_delete_all_for_work_type<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE t_work_type_process
            SET deleted_at = now()
            WHERE work_type_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(work_type_id)
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 批量插入新 mapping：单条 INSERT ... VALUES (...), (...), (...)。
    ///
    /// 空切片短路返回 0 行；service 层若要清空映射仍应走 set_work_type_processes + 空 items。
    #[allow(clippy::too_many_arguments)]
    pub async fn bulk_insert<'e, E: PgExecutor<'e>>(
        executor: E,
        rows: &[NewWorkTypeProcessRow],
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
            "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, created_by, updated_by) ",
        );
        qb.push_values(rows.iter(), |mut b, row| {
            let id = snowflake.next_id();
            b.push_bind(id)
                .push_bind(row.work_type_id)
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
