//! work_type ↔ process 映射（`t_work_type_process`）子模块
//!
//! 7 个 work_type 端点中，mapping 相关 2 个端点（`GET /work-types/{id}/processes` /
//! `POST /work-types/{id}/processes`）的本域职责抽到本文件，避免 `service.rs` 超过 1000 行
//! 硬上限（conventions.md §2）。
//!
//! ## 接口
//! - `WorkTypeProcessRepo` —— 4 个 SQL 函数
//! - `WorkTypeProcessService` —— 2 个 service 方法（set / list）

use sqlx::{PgConnection, PgExecutor};

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::process::repo::ProcessRepo;
use crate::shared::error::{code, AppError};

use super::dto::{
    SetWorkTypeProcessesItem, WorkTypeProcessMappingItem, WorkTypeProcessMappingOut,
};

// ---------------------------------------------------------------------------
// Repo
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
        qb.build().execute(executor).await.map(|r| r.rows_affected())
    }
}

/// 新 mapping 行的输入结构（service 层用）。
#[derive(Debug, Clone)]
pub struct NewWorkTypeProcessRow {
    pub work_type_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct WorkTypeProcessService;

impl WorkTypeProcessService {
    /// 设置指定 work_type 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 work_type 存在
    /// 2. 校验 items 内的所有 process_id 存在（一次 `ProcessRepo::list_by_ids` 批量）
    /// 3. 软删该 work_type 的全部旧 mapping
    /// 4. INSERT 新 mapping（按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20901 `BIZ_WORK_TYPE_NOT_FOUND`
    /// - 20801 `BIZ_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    #[allow(clippy::too_many_arguments)]
    pub async fn set_work_type_processes(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        work_type_id: i64,
        items: &[SetWorkTypeProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. work_type 存在性 + 软删校验（已软删 → 404）
        let wt = super::repo::WorkTypeRepo::get_by_id(&mut *conn, work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
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
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        WorkTypeProcessRepo::soft_delete_all_for_work_type(&mut *conn, work_type_id).await?;

        // 4. 批量 INSERT 新 mapping（空 items = 清空映射；无行写）
        let new_rows: Vec<NewWorkTypeProcessRow> = items
            .iter()
            .zip(process_ids.iter())
            .map(|(it, &pid)| NewWorkTypeProcessRow {
                work_type_id: wt.id,
                process_id: pid,
                sort_order: it.sort_order,
            })
            .collect();
        WorkTypeProcessRepo::bulk_insert(&mut *conn, &new_rows, snowflake, current.id).await?;

        Ok(())
    }

    /// 列出指定 work_type 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_work_type_processes(
        conn: &mut PgConnection,
        work_type_id: i64,
        current: &CurrentUser,
    ) -> Result<WorkTypeProcessMappingOut, AppError> {
        // 权限：与 list_work_types 一致（任意已登录）
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        // work_type 存在性校验
        let wt = super::repo::WorkTypeRepo::get_by_id(&mut *conn, work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;

        let rows = WorkTypeProcessRepo::list_by_work_type(&mut *conn, wt.id).await?;
        let items = rows
            .into_iter()
            .map(
                |(pid, sort_order, process_code)| WorkTypeProcessMappingItem {
                    work_type_id: wt.id,
                    process_id: pid,
                    process_code,
                    sort_order,
                },
            )
            .collect();
        Ok(WorkTypeProcessMappingOut { items })
    }
}