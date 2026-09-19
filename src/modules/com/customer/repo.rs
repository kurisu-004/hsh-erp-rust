//! customer 域数据访问
//!
//! 对应 Python myERP/repository/customer.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! 暴露给 service 的能力（Phase P1+ customer CRUD）：
//! - 读：`get_by_id` / `list_by_ids` / `list_children` / `list_roots` / `list_all`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder，防 N+1）
//! - 写：`create` / `update` / `soft_delete`

use sqlx::{PgExecutor, QueryBuilder};

use super::model::TCustomer;

pub struct CustomerRepo;

impl CustomerRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE id = ANY($1)
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            ids,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 取 `parent_id = $1` 的全部 L2 子客户（用于 L1 分组列表计算组外 L2）。
    /// 空 parent_id 时返回空 Vec，由 caller 自行短路。
    pub async fn list_children<'e, E: PgExecutor<'e>>(
        executor: E,
        parent_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE parent_id = $1
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            parent_id,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 取全部一级客户（`parent_id IS NULL`），可选包含已软删。
    pub async fn list_roots<'e, E: PgExecutor<'e>>(
        executor: E,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE parent_id IS NULL
              AND ($1::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 取全部客户（L1+L2），可选包含已软删，按 L1 优先 + id 升序。
    pub async fn list_all<'e, E: PgExecutor<'e>>(
        executor: E,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE ($1::bool OR deleted_at IS NULL)
            ORDER BY parent_id NULLS FIRST, id ASC
            "#,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 name_like / parent_id / is_root 三态过滤。
    /// 一次往返即可拿全表行，避免 N+1。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, name, parent_id, version, \
             created_at, created_by, updated_at, updated_by, deleted_at, \
             serial_prefix \
             FROM t_customer WHERE deleted_at IS NULL",
        );
        if let Some(p) = parent_id {
            qb.push(" AND parent_id = ").push_bind(p);
        } else if matches!(is_root, Some(true)) {
            // is_root = Some(true) ⇒ parent_id IS NULL
            qb.push(" AND parent_id IS NULL");
        } else if matches!(is_root, Some(false)) {
            // is_root = Some(false) ⇒ parent_id IS NOT NULL
            qb.push(" AND parent_id IS NOT NULL");
        }
        // None = 不按 is_root 过滤
        if let Some(needle) = name_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND name ILIKE ").push_bind(pat);
            }
        }
        qb.push(" ORDER BY parent_id NULLS FIRST, id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);

        qb.build_query_as::<TCustomer>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        name_like: Option<&str>,
        parent_id: Option<i64>,
        is_root: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_customer WHERE deleted_at IS NULL",
        );
        if let Some(p) = parent_id {
            qb.push(" AND parent_id = ").push_bind(p);
        } else if matches!(is_root, Some(true)) {
            qb.push(" AND parent_id IS NULL");
        } else if matches!(is_root, Some(false)) {
            qb.push(" AND parent_id IS NOT NULL");
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

    /// 插入新客户。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        name: &str,
        parent_id: Option<i64>,
        serial_prefix: Option<&str>,
        created_by: i64,
    ) -> Result<TCustomer, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            INSERT INTO t_customer (id, name, parent_id, serial_prefix, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, $5)
            RETURNING id, name, parent_id, version,
                      created_at, created_by, updated_at, updated_by, deleted_at,
                      serial_prefix
            "#,
            snowflake_id,
            name,
            parent_id,
            serial_prefix,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。
    ///
    /// `serial_prefix` 三态编码：
    /// - `None` ⇒ 字段缺省，不修改
    /// - `Some(None)` ⇒ 显式清空（SET NULL）
    /// - `Some(Some(v))` ⇒ 改值
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        serial_prefix: Option<Option<&str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_prefix = serial_prefix.is_some();
        let new_prefix = serial_prefix.flatten();
        sqlx::query!(
            r#"
            UPDATE t_customer
            SET name         = COALESCE($3::varchar, name),
                serial_prefix = CASE WHEN $4::bool THEN $5::varchar ELSE serial_prefix END,
                version       = version + 1,
                updated_at    = now(),
                updated_by    = $6
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            set_prefix,
            new_prefix,
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
            UPDATE t_customer
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
}