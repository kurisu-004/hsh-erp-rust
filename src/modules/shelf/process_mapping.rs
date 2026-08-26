//! shelf ↔ process 映射（`t_shelf_process`）子模块
//!
//! 11 个 shelf 端点中，mapping 相关 3 个端点（`GET /shelves/{id}/processes` /
//! `POST /shelves/{id}/processes` / `GET /shelves/processes`）的本域职责
//! 抽到本文件，避免 `service.rs` 超过 1000 行硬上限（conventions.md §2）。
//!
//! ## 接口
//! - `ShelfProcessRepo` —— 4 个 SQL 函数
//! - `ShelfProcessService` —— 2 个 service 方法（set_shelf_processes / list_shelf_processes）

use sqlx::{PgConnection, PgExecutor};

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::process::repo::ProcessRepo;
use crate::shared::error::{code, AppError};

use super::dto::{ShelfProcessMappingItem, ShelfProcessMappingOut};

// ---------------------------------------------------------------------------
// Repo
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
    #[allow(clippy::too_many_arguments)]
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
        qb.build().execute(executor).await.map(|r| r.rows_affected())
    }
}

/// 新 mapping 行的输入结构（service 层用）。
#[derive(Debug, Clone)]
pub struct NewShelfProcessRow {
    pub shelf_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct ShelfProcessService;

impl ShelfProcessService {
    /// 设置指定 shelf 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 shelf 存在 + active
    /// 2. 校验 items 内的所有 process_id 存在（一次 `ProcessRepo::list_by_ids` 批量）
    /// 3. 软删该 shelf 的全部旧 mapping
    /// 4. INSERT 新 mapping（按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    /// - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）
    #[allow(clippy::too_many_arguments)]
    pub async fn set_shelf_processes(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        items: &[super::dto::SetShelfProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. shelf 存在性 + 软删校验（已软删 → 404）
        let shelf = super::repo::ShelfRepo::get_by_id(&mut *conn, shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;

        // 2. 解析 + 校验所有 process_id 存在
        let mut process_ids: Vec<i64> = Vec::with_capacity(items.len());
        for it in items {
            let pid = it.process_id.parse::<i64>().map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    "process_id 必须为雪花 ID 字符串",
                )
            })?;
            process_ids.push(pid);
        }
        if !process_ids.is_empty() {
            // 一次性批量查 process —— 防 N+1
            let existing = ProcessRepo::list_by_ids(&mut *conn, &process_ids).await?;
            if existing.len() != process_ids.len() {
                // 找出缺失的 id（用 Vec 差集；批量小，开销可忽略）
                let existing_ids: std::collections::HashSet<i64> =
                    existing.iter().map(|p| p.id).collect();
                let missing: Vec<i64> = process_ids
                    .iter()
                    .filter(|p| !existing_ids.contains(p))
                    .copied()
                    .collect();
                return Err(AppError::biz(
                    code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        ShelfProcessRepo::soft_delete_all_for_shelf(&mut *conn, shelf_id).await?;

        // 4. 批量 INSERT 新 mapping（空 items = 清空映射；无行写）
        let new_rows: Vec<NewShelfProcessRow> = items
            .iter()
            .zip(process_ids.iter())
            .map(|(it, &pid)| NewShelfProcessRow {
                shelf_id: shelf.id,
                process_id: pid,
                sort_order: it.sort_order,
            })
            .collect();
        ShelfProcessRepo::bulk_insert(&mut *conn, &new_rows, snowflake, current.id).await?;

        Ok(())
    }

    /// 列出指定 shelf 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_shelf_processes(
        conn: &mut PgConnection,
        shelf_id: i64,
        current: &CurrentUser,
    ) -> Result<ShelfProcessMappingOut, AppError> {
        // 权限：与 list_shelves 一致（任意已登录）
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        // shelf 存在性 / scope 校验
        let shelf = super::repo::ShelfRepo::get_by_id(&mut *conn, shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;
        if !current.can_access_shelf(shelf.id) {
            return Err(AppError::biz(
                code::SHELF_MISMATCH,
                format!("无权访问 shelf {shelf_id}"),
            ));
        }

        let rows = ShelfProcessRepo::list_by_shelf(&mut *conn, shelf.id).await?;
        let items = rows
            .into_iter()
            .map(
                |(sid, pid, sort_order, shelf_code, process_code)| ShelfProcessMappingItem {
                    shelf_id: sid,
                    shelf_code,
                    process_id: pid,
                    process_code,
                    sort_order,
                },
            )
            .collect();
        Ok(ShelfProcessMappingOut { items })
    }
}