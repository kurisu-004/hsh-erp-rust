//! shelf 域数据访问
//!
//! 对应 Python myERP/repository/shelf_repository.py。函数签名接收
//! `impl PgExecutor<'_>`（除非要复用同一事务内的多次调用 —— 那种情况收
//! `&mut PgConnection`），兼容 `&PgPool` / `&mut PgConnection` /
//! `&mut Transaction`。
//!
//! ## Phase P3+ shelf CRUD 暴露给 service 的能力
//! - 读：`get_active_by_id` / `get_by_id` / `get_by_id_zone`
//!   / `list_with_filters` / `count_with_filters` / `list_active_production_ordered`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder）
//! - 写：`create` / `update` / `soft_delete`（同时 `is_active = false`）
//! - 引用计数：`count_in_use_parts`（deactivate 前查 t_part.current_holder_id）
//!
//! ## 约定
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//! - `list_active_production_ordered` 通过 LEFT JOIN `t_part_batch` 聚合 current_load

use sqlx::{PgConnection, PgExecutor, QueryBuilder};

use super::model::TShelf;

pub struct ShelfRepo;

impl ShelfRepo {
    /// 按 id 查 active 货架（is_active=true, deleted_at IS NULL）。用于 INSPECTION/PRODUCTION 区校验。
    pub async fn get_active_by_id(
        conn: &mut PgConnection,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND is_active = true AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 按 id 查（不强制 is_active；用于 service 层区分 20501 NOT_FOUND vs 20512 INACTIVE）。
    pub async fn get_by_id(
        conn: &mut PgConnection,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 按 id + zone 双键查（worker-pool 投放 / 看板用）。
    /// 命中失败 → worker 把 batch 投到不属于自己的区，service 层应拒绝。
    pub async fn get_by_id_zone(
        conn: &mut PgConnection,
        id: i64,
        zone: &str,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND zone = $2 AND is_active = true AND deleted_at IS NULL
            "#,
            id,
            zone,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 `code_like` / `zone` 二态过滤。
    /// 一次往返即可拿全表行，避免 N+1。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
        zone: Option<&str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TShelf>, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, code, name, zone, location, is_active, display_order, version, \
             created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_shelf WHERE deleted_at IS NULL",
        );
        if let Some(z) = zone {
            let trimmed = z.trim();
            if !trimmed.is_empty() {
                qb.push(" AND zone = ").push_bind(trimmed.to_string());
            }
        }
        if let Some(active) = is_active {
            qb.push(" AND is_active = ").push_bind(active);
        }
        if let Some(needle) = code_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND code ILIKE ").push_bind(pat);
            }
        }
        qb.push(" ORDER BY display_order ASC, id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);

        qb.build_query_as::<TShelf>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
        zone: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_shelf WHERE deleted_at IS NULL",
        );
        if let Some(z) = zone {
            let trimmed = z.trim();
            if !trimmed.is_empty() {
                qb.push(" AND zone = ").push_bind(trimmed.to_string());
            }
        }
        if let Some(active) = is_active {
            qb.push(" AND is_active = ").push_bind(active);
        }
        if let Some(needle) = code_like {
            let trimmed = needle.trim();
            if !trimmed.is_empty() {
                let pat = format!("%{}%", trimmed);
                qb.push(" AND code ILIKE ").push_bind(pat);
            }
        }

        qb.build_query_scalar::<i64>().fetch_one(executor).await
    }

    /// PRODUCTION 区活跃货架列表，按 current_load 升序（同 load 时按 display_order ASC）。
    ///
    /// 用途：`list_for_return` picker —— worker 把成品零件送回时找空货架。
    /// `current_load` 用 `LEFT JOIN t_part_batch` 聚合（status IN ('PENDING',
    /// 'IN_PROCESS', 'INSPECTION', 'REPAIRING', 'OUTSOURCE')）的批次总
    /// quantity；LEFT JOIN 保留 0-负载货架（current_load = 0）。
    pub async fn list_active_production_ordered<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<TShelfWithLoad>, sqlx::Error> {
        sqlx::query_as!(
            TShelfWithLoad,
            r#"
            SELECT s.id, s.code, s.name, s.zone, s.location, s.is_active, s.display_order,
                   s.version, s.created_at, s.created_by, s.updated_at, s.updated_by, s.deleted_at,
                   COALESCE(load.cnt, 0)::bigint AS "current_load!"
            FROM t_shelf s
            LEFT JOIN (
                SELECT current_holder_id AS shelf_id,
                       SUM(quantity)::bigint AS cnt
                FROM t_part_batch
                WHERE status IN ('PENDING', 'IN_PROCESS', 'INSPECTION', 'REPAIRING', 'OUTSOURCE')
                GROUP BY current_holder_id
            ) load ON load.shelf_id = s.id
            WHERE s.zone = 'PRODUCTION'
              AND s.is_active = true
              AND s.deleted_at IS NULL
            ORDER BY load.cnt ASC NULLS FIRST, s.display_order ASC, s.id ASC
            "#,
        )
        .fetch_all(executor)
        .await
    }

    /// 插入新货架。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    #[allow(clippy::too_many_arguments)]
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        code: &str,
        name: &str,
        zone: &str,
        location: Option<&str>,
        display_order: i32,
        created_by: i64,
    ) -> Result<TShelf, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            INSERT INTO t_shelf (id, code, name, zone, location, is_active, display_order,
                                 created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, true, $6, $7, $7)
            RETURNING id, code, name, zone, location, is_active, display_order,
                      version, created_at, created_by, updated_at, updated_by, deleted_at
            "#,
            snowflake_id,
            code,
            name,
            zone,
            location,
            display_order,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。
    ///
    /// `location` / `display_order` 三态编码（与 process.update 同形）：
    /// - `None` ⇒ 字段缺省，不修改
    /// - `Some(None)` ⇒ 显式清空（SET NULL）
    /// - `Some(Some(v))` ⇒ 改值
    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        location: Option<Option<&str>>,
        display_order: Option<i32>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_location = location.is_some();
        let new_location = location.flatten();
        sqlx::query!(
            r#"
            UPDATE t_shelf
            SET name          = COALESCE($3::varchar, name),
                location      = CASE WHEN $4::bool THEN $5::varchar ELSE location END,
                display_order = COALESCE($6::integer, display_order),
                version       = version + 1,
                updated_at    = now(),
                updated_by    = $7
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            set_location,
            new_location,
            display_order,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 软删除 + 停用：`deleted_at = now()` + `is_active = false` 同时置位（Python pattern）。
    /// 带乐观锁 `WHERE id = $1 AND version = $2 AND deleted_at IS NULL`。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_shelf
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

    /// 软删前查引用：单条 `UNION ALL` 统计 `t_part.current_holder_id = shelf_id` 且
    /// `status IN ('IN_PROCESS', 'INSPECTION', 'REPAIRING')` 的非软删零件数。
    ///
    /// 任一分支 > 0 ⇒ 20503 BIZ_SHELF_IN_USE。
    pub async fn count_in_use_parts<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_id: i64,
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
                )::bigint AS total
            "#,
        )
        .bind(shelf_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }

    /// 计算一组 shelf 各自的 SHELF_ACCOUNT 角色数（GROUP BY）。单条 SQL 批量算。
    ///
    /// 用法：`ShelfService::list_shelves` 把 items 的 id 一次性查 account_count，
    /// 防 N+1。
    pub async fn count_accounts_by_shelf<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_ids: &[i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        if shelf_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            r#"
            SELECT scope_id, COUNT(*)::bigint
            FROM t_user_role
            WHERE scope_type = 'shelf'
              AND scope_id = ANY($1)
              AND deleted_at IS NULL
            GROUP BY scope_id
            "#,
        )
        .bind(shelf_ids)
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }
}

/// `TShelf` + 聚合 `current_load`（来自 t_part_batch LEFT JOIN）。
///
/// 用于 `list_active_production_ordered`（picker for-return）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TShelfWithLoad {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    pub display_order: i32,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    pub current_load: i64,
}