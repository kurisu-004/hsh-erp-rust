//! applicant 域数据访问
//!
//! 对应 Python myERP/repository/applicant_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` / `query_scalar!` 编译期宏
//!   （需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! 暴露给 service 的能力（applicant CRUD）：
//! - 读：`get_by_id` / `find_by_name_and_customer` / `customer_name`
//! - 校验：`l1_customer_exists` / `count_parts_using_applicant_name`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder 防 N+1）
//! - 写：`create` / `update` / `soft_delete`

use sqlx::{PgExecutor, QueryBuilder, Postgres};

use super::model::TApplicant;

pub struct ApplicantRepo;

impl ApplicantRepo {
    /// 按 id 取单条记录；`include_deleted=true` 时返回含已软删行。
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error> {
        sqlx::query_as!(
            TApplicant,
            r#"
            SELECT id, name, customer_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_applicant
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 `(name, customer_id)` 查单条：用于创建 / 修改时的重名校验。
    pub async fn find_by_name_and_customer<'e, E: PgExecutor<'e>>(
        executor: E,
        name: &str,
        customer_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TApplicant>, sqlx::Error> {
        sqlx::query_as!(
            TApplicant,
            r#"
            SELECT id, name, customer_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_applicant
            WHERE name = $1 AND customer_id = $2
              AND ($3::bool OR deleted_at IS NULL)
            "#,
            name,
            customer_id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 取指定客户的展示名（`deleted_at IS NULL` 过滤）。
    /// service 拼 `ApplicantOut.customer_name` 用。
    pub async fn customer_name<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_id: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL"#,
            customer_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 `customer_id` / `name_like` 过滤。
    /// 一次往返即可拿一页，避免 N+1。
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_id: Option<i64>,
        name_like: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TApplicant>, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT id, name, customer_id, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_applicant WHERE deleted_at IS NULL",
        );
        if let Some(cid) = customer_id {
            qb.push(" AND customer_id = ").push_bind(cid);
        }
        if let Some(needle) = name_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND name ILIKE ").push_bind(pat);
            }
        }
        qb.push(" ORDER BY id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);

        qb.build_query_as::<TApplicant>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)，返回 `i64`。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_id: Option<i64>,
        name_like: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT COUNT(*)::bigint FROM t_applicant WHERE deleted_at IS NULL");
        if let Some(cid) = customer_id {
            qb.push(" AND customer_id = ").push_bind(cid);
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

    /// 校验 `customer_id` 是否指向 L1 客户（`parent_id IS NULL`）。
    /// 返回 `true` ⇒ 是 L1；`false` ⇒ 不存在或不是 L1。
    pub async fn l1_customer_exists<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_id: i64,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT EXISTS(
                 SELECT 1 FROM t_customer
                 WHERE id = $1 AND parent_id IS NULL AND deleted_at IS NULL
               ) AS "exists!: bool""#,
            customer_id,
        )
        .fetch_one(executor)
        .await
    }

    /// 检查 `t_part` 是否有未软删零件引用此 `applicant_name`（属于该 customer）。
    /// `>0` ⇒ 拒软删（避免遗留孤悬引用）。
    pub async fn count_parts_using_applicant_name<'e, E: PgExecutor<'e>>(
        executor: E,
        name: &str,
        customer_id: i64,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
               FROM t_part
               WHERE applicant_name = $1
                 AND customer_id = $2
                 AND deleted_at IS NULL"#,
            name,
            customer_id,
        )
        .fetch_one(executor)
        .await
    }

    /// 插入新 applicant。雪花 id 由调用方（service）生成；
    /// `created_by` / `updated_by` 共用同一值，后续 UPDATE 才更新 `updated_by`。
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        name: &str,
        customer_id: i64,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"INSERT INTO t_applicant
                 (id, name, customer_id, version, created_at, created_by, updated_at, updated_by)
               VALUES ($1, $2, $3, 0, now(), $4, now(), $4)"#,
            snowflake_id,
            name,
            customer_id,
            created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 部分更新（OCC）：带乐观锁。
    ///
    /// `name` / `customer_id` 任一为 `Some` 则更新对应列；均为 `None` 时只 `version + 1`。
    /// 0 行影响由 service 转 409 (`VERSION_CONFLICT`)。
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        customer_id: Option<i64>,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE t_applicant
               SET name         = COALESCE($3, name),
                   customer_id  = COALESCE($4, customer_id),
                   version      = version + 1,
                   updated_at   = now(),
                   updated_by   = $5
               WHERE id = $1 AND version = $2 AND deleted_at IS NULL"#,
            id,
            version,
            name,
            customer_id,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }

    /// 软删除：置 `deleted_at = now()` + `version + 1`，带乐观锁。
    /// 0 行影响由 service 转 409 (`VERSION_CONFLICT`)。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE t_applicant
               SET deleted_at = now(),
                   updated_at = now(),
                   updated_by = $3,
                   version    = version + 1
               WHERE id = $1 AND version = $2 AND deleted_at IS NULL"#,
            id,
            version,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(result.rows_affected())
    }
}