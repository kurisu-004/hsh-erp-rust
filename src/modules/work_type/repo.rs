//! work_type 域数据访问
//!
//! 对应 Python myERP/repository/work_type_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P5 work_type CRUD 暴露给 service 的能力：
//! - 读：`get_by_id` / `get_by_code` / `list_with_filters` / `count_with_filters` / `list_by_ids`
//! - 写：`create` / `update` / `soft_delete`
//! - 引用计数：`count_work_type_references`（软删前查 `t_worker.work_type_id` +
//!   `t_work_type_process` 引用，best-effort）
//! - mapping 子模块 `process_mapping.rs` 维护 `t_work_type_process`
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409

use sqlx::{PgExecutor, QueryBuilder};

use super::model::TWorkType;

pub struct WorkTypeRepo;

impl WorkTypeRepo {
    /// 按 id 查活跃行（`deleted_at IS NULL`）。
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        sqlx::query_as!(
            TWorkType,
            r#"
            SELECT id, code, name, description, sort_order, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   max_held_batches
            FROM t_work_type
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 code 精确查活跃行（`uk_t_work_type_code` 唯一索引覆盖）。
    pub async fn get_by_code<'e, E: PgExecutor<'e>>(
        executor: E,
        code: &str,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        sqlx::query_as!(
            TWorkType,
            r#"
            SELECT id, code, name, description, sort_order, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   max_held_batches
            FROM t_work_type
            WHERE code = $1 AND deleted_at IS NULL
            "#,
            code,
        )
        .fetch_optional(executor)
        .await
    }

    /// 工种可执行的工序 id 列表（worker-pool 校验 worker 操作的 process 是否属于其工种）。
    /// `t_work_type_process` 无业务软删（mapping 表通常保留历史），不筛 `deleted_at`。
    pub async fn list_process_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<i64> = sqlx::query_scalar!(
            r#"
            SELECT process_id AS "process_id!"
            FROM t_work_type_process
            WHERE work_type_id = $1
            "#,
            work_type_id,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }

    /// 批量按 id 查（活跃行）。空切片短路返回空 Vec。
    ///
    /// 用途：`WorkerService::list_workers` 取所有 worker 的 work_type_id 后，
    /// 一次性 `list_by_ids` 拿齐 `work_type.name`，防 N+1。
    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
    ) -> Result<Vec<TWorkType>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TWorkType,
            r#"
            SELECT id, code, name, description, sort_order, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   max_held_batches
            FROM t_work_type
            WHERE id = ANY($1) AND deleted_at IS NULL
            ORDER BY id ASC
            "#,
            ids,
        )
        .fetch_all(executor)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 `code_like` 二态过滤。
    /// 一次往返即可拿全表行，避免 N+1。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorkType>, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, code, name, description, sort_order, version, \
             created_at, created_by, updated_at, updated_by, deleted_at, \
             max_held_batches \
             FROM t_work_type WHERE deleted_at IS NULL",
        );
        if let Some(needle) = code_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND code ILIKE ").push_bind(pat);
            }
        }
        qb.push(" ORDER BY sort_order ASC, id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);

        qb.build_query_as::<TWorkType>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_work_type WHERE deleted_at IS NULL",
        );
        if let Some(needle) = code_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND code ILIKE ").push_bind(pat);
            }
        }

        qb.build_query_scalar::<i64>().fetch_one(executor).await
    }

    /// 插入新工种。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    #[allow(clippy::too_many_arguments)]
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        code: &str,
        name: &str,
        description: Option<&str>,
        sort_order: i32,
        max_held_batches: Option<i32>,
        created_by: i64,
    ) -> Result<TWorkType, sqlx::Error> {
        sqlx::query_as!(
            TWorkType,
            r#"
            INSERT INTO t_work_type (id, code, name, description, sort_order,
                                     max_held_batches, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
            RETURNING id, code, name, description, sort_order, version,
                      created_at, created_by, updated_at, updated_by, deleted_at,
                      max_held_batches
            "#,
            snowflake_id,
            code,
            name,
            description,
            sort_order,
            max_held_batches,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。`code` 不允许改（业务唯一键，由 service 层 enforce）。
    ///
    /// 三态编码：
    /// - `name` / `sort_order`：二态 `Option<&str / Option<i32>`，None ⇒ 不修改
    /// - `description` / `max_held_batches`：三态 `Option<Option<T>>`：
    ///   - `None` ⇒ 字段缺省，不修改
    ///   - `Some(None)` ⇒ 显式清空（SET NULL）
    ///   - `Some(Some(v))` ⇒ 改值
    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        description: Option<Option<&str>>,
        sort_order: Option<i32>,
        max_held_batches: Option<Option<i32>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_description = description.is_some();
        let new_description = description.flatten();
        let set_max_held_batches = max_held_batches.is_some();
        let new_max_held_batches = max_held_batches.flatten();
        sqlx::query!(
            r#"
            UPDATE t_work_type
            SET name            = COALESCE($3::varchar, name),
                description     = CASE WHEN $4::bool THEN $5::varchar ELSE description END,
                sort_order      = COALESCE($6::integer, sort_order),
                max_held_batches = CASE WHEN $7::bool THEN $8::integer ELSE max_held_batches END,
                version         = version + 1,
                updated_at      = now(),
                updated_by      = $9
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            set_description,
            new_description,
            sort_order,
            set_max_held_batches,
            new_max_held_batches,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 软删除：置 `deleted_at = now()` + `version + 1`，带乐观锁。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_work_type
            SET deleted_at = now(),
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 软删前查引用：单条 `UNION ALL` 统计 `t_worker.work_type_id = $1` 的活跃行数 +
    /// `t_work_type_process.work_type_id = $1` 的行数。
    ///
    /// 任一分支 > 0 ⇒ 20903 `BIZ_WORK_TYPE_IN_USE`。
    ///
    /// 注：`t_work_type_process` 无业务软删（mapping 表保留历史），不筛 `deleted_at`；
    /// 这与 `ProcessRepo::count_process_references` 的 junction 处理一致。
    pub async fn count_work_type_references<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT
                (
                    (SELECT COUNT(*) FROM t_worker
                     WHERE work_type_id = $1 AND deleted_at IS NULL)
                    +
                    (SELECT COUNT(*) FROM t_work_type_process
                     WHERE work_type_id = $1)
                )::bigint AS total
            "#,
        )
        .bind(work_type_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}