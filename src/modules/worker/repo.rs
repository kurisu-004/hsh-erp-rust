//! worker 域数据访问
//!
//! 对应 Python myERP/repository/worker_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P4 worker CRUD 暴露给 service 的能力：
//! - 读：`get_by_id` / `get_by_badge_code` / `list_with_filters` /
//!   `count_with_filters` / `count_in_use_parts`
//! - 写：`create` / `update` / `deactivate` / `reactivate`
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409

use sqlx::{PgExecutor, QueryBuilder};

use super::model::TWorker;

pub struct WorkerRepo;

impl WorkerRepo {
    /// 按 id 查（`include_deleted=true` 用于 reactivate 时回读历史行）
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        sqlx::query_as!(
            TWorker,
            r#"
            SELECT id, badge_code, name, id_card_no, phone, is_active,
                   work_type_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_worker
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 badge_code 查（扫码台 / 唯一性校验用）。
    /// `include_deleted=true` 用于管理后台 / reactivate 时回读。
    pub async fn get_by_badge_code<'e, E: PgExecutor<'e>>(
        executor: E,
        badge_code: &str,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        sqlx::query_as!(
            TWorker,
            r#"
            SELECT id, badge_code, name, id_card_no, phone, is_active,
                   work_type_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_worker
            WHERE badge_code = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            badge_code,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 `name_like` / `is_active` 二态过滤。
    /// 一次往返即可拿全表行，避免 N+1。
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TWorker>, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, badge_code, name, id_card_no, phone, is_active, \
             work_type_id, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_worker WHERE deleted_at IS NULL",
        );
        if let Some(active) = is_active {
            qb.push(" AND is_active = ").push_bind(active);
        }
        if let Some(needle) = name_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND name ILIKE ").push_bind(pat);
            }
        }
        qb.push(" ORDER BY id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);

        qb.build_query_as::<TWorker>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_worker WHERE deleted_at IS NULL",
        );
        if let Some(active) = is_active {
            qb.push(" AND is_active = ").push_bind(active);
        }
        if let Some(needle) = name_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND name ILIKE ").push_bind(pat);
            }
        }

        qb.build_query_scalar::<i64>().fetch_one(executor).await
    }

    /// 插入新工人。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    #[allow(clippy::too_many_arguments)]
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        badge_code: &str,
        name: &str,
        id_card_no: Option<&str>,
        phone: Option<&str>,
        work_type_id: Option<i64>,
        created_by: i64,
    ) -> Result<TWorker, sqlx::Error> {
        sqlx::query_as!(
            TWorker,
            r#"
            INSERT INTO t_worker (id, badge_code, name, id_card_no, phone, is_active,
                                   work_type_id, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, true, $6, $7, $7)
            RETURNING id, badge_code, name, id_card_no, phone, is_active,
                      work_type_id, version,
                      created_at, created_by, updated_at, updated_by, deleted_at
            "#,
            snowflake_id,
            badge_code,
            name,
            id_card_no,
            phone,
            work_type_id,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。
    ///
    /// 三态编码：
    /// - `name` / `badge_code`：二态 `Option<&str>`，None ⇒ 不修改（`COALESCE` 短路），Some ⇒ 改值
    /// - `id_card_no` / `phone` / `work_type_id`：三态 `Option<Option<T>>`：
    ///   - `None` ⇒ 字段缺省，不修改
    ///   - `Some(None)` ⇒ 显式清空（SET NULL）
    ///   - `Some(Some(v))` ⇒ 改值
    ///
    /// 注：`badge_code` 为二态（无显式清空语义）；service 层校验是否撞唯一索引
    /// （`uk_t_worker_badge_code`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        badge_code: Option<&str>,
        id_card_no: Option<Option<&str>>,
        phone: Option<Option<&str>>,
        work_type_id: Option<Option<i64>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_id_card_no = id_card_no.is_some();
        let new_id_card_no = id_card_no.flatten();
        let set_phone = phone.is_some();
        let new_phone = phone.flatten();
        let set_work_type_id = work_type_id.is_some();
        let new_work_type_id = work_type_id.flatten();
        sqlx::query!(
            r#"
            UPDATE t_worker
            SET name         = COALESCE($3::varchar, name),
                badge_code   = COALESCE($4::varchar, badge_code),
                id_card_no   = CASE WHEN $5::bool THEN $6::varchar ELSE id_card_no END,
                phone        = CASE WHEN $7::bool THEN $8::varchar ELSE phone END,
                work_type_id = CASE WHEN $9::bool THEN $10::bigint ELSE work_type_id END,
                version      = version + 1,
                updated_at   = now(),
                updated_by   = $11
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            badge_code,
            set_id_card_no,
            new_id_card_no,
            set_phone,
            new_phone,
            set_work_type_id,
            new_work_type_id,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 停用：`is_active = false` 同时 `deleted_at = now()`（Python pattern）。
    /// 带乐观锁 `WHERE id = $1 AND version = $2 AND deleted_at IS NULL`。
    pub async fn deactivate<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_worker
            SET deleted_at = now(),
                is_active  = false,
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

    /// 重新激活：`is_active = true` 同时 `deleted_at = NULL`（undelete + 启用）。
    /// 带乐观锁 `WHERE id = $1 AND version = $2`，**不**限制 `deleted_at IS NULL`
    /// —— service 端用 `include_deleted=true` 拿到 row 才能 reactivate。
    pub async fn reactivate<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_worker
            SET deleted_at = NULL,
                is_active  = true,
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND version = $2 AND deleted_at IS NOT NULL
            "#,
            id,
            version,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 停用前查引用：单条 `UNION ALL` 统计 `t_part.current_holder_id = worker_id` 且
    /// `status IN ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')` 的非软删零件数。
    ///
    /// 任一分支 > 0 ⇒ 20203 `BIZ_WORKER_IN_USE`。
    ///
    /// 注：brief 说 "active 持有 (holder=WORKER)"，Python `_assert_not_holding_parts`
    /// 同时过滤 `location='WORKER'`。本实现按 brief "simpler interpretation matching
    /// Python" 接受 `current_holder_id = worker_id` 单条件 —— 在 Rust rollup 模型下
    /// `current_holder_id` 已足够定位「该 worker 持有」的 part，不依赖 location
    /// 多态列（location 主要给 Python ORM 多态鉴别用）。
    pub async fn count_in_use_parts<'e, E: PgExecutor<'e>>(
        executor: E,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT
                (
                    (SELECT COUNT(*) FROM t_part
                     WHERE current_holder_id = $1
                       AND status = 'IN_PROCESS'
                       AND deleted_at IS NULL)
                    +
                    (SELECT COUNT(*) FROM t_part
                     WHERE current_holder_id = $1
                       AND status = 'INSPECTION'
                       AND deleted_at IS NULL)
                    +
                    (SELECT COUNT(*) FROM t_part
                     WHERE current_holder_id = $1
                       AND status = 'REPAIRING'
                       AND deleted_at IS NULL)
                    +
                    (SELECT COUNT(*) FROM t_part
                     WHERE current_holder_id = $1
                       AND status = 'RETURNED'
                       AND deleted_at IS NULL)
                )::bigint AS total
            "#,
        )
        .bind(worker_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}
