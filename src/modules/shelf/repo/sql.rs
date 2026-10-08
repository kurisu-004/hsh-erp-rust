//! shelf 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/shelf_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! ## 约定
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）—— **含 LEFT JOIN 的聚合子查询**（外层带了
//!   不算数，子查询自己也得带，否则软删行的量会被永久计入）
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! ## Phase P3+ shelf CRUD 暴露给 service 的能力
//! - 读：`get_active_by_id` / `get_by_id` / `get_by_id_zone`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder）
//! - 写：`create` / `update` / `soft_delete`（同时 `is_active = false`）
//! - 引用计数：`count_in_use_parts`（deactivate 前查 t_part_batch.current_holder_id
//!   + location + status 三维核对，PR-2 真相源迁移后已不再读 t_part）
//!
//! ## 2026-10-10：两个聚合列表方法删除
//! `list_active_production_ordered` / `list_active_inspection_with_load` 是 picker
//! 两条端点的专供查询，随端点下线一并删除。它们各自内联了一份**逐字重复**的
//! `t_part_batch` 负载聚合子查询；口径的**唯一**真源现在在
//! [`crate::shared::shelf::load::LOAD_AGGREGATE_SQL`]，由 `ShelfRepoTrait::load_by_ids`
//! 与 `shared::shelf::select::pick_least_loaded` 共用 —— 本文件不再有任何负载聚合。
//!
//! 2026-09-22 重构：从 `repo.rs` 平移到 `repo/sql.rs`，本文件 SQL 与方法签名零 diff，
//! `.sqlx/query-*.json` 哈希不变；新增的 `ShelfRepoTrait` 胖 trait 在 `repo/mod.rs`。
//!
//! 2026-10-02 域拆分：`t_shelf_process` 的 4 个方法与账号计数
//! `count_accounts_by_shelf` 一并移出本文件 —— 前者搬到
//! `crate::modules::prod::shelf_process::repo::ShelfProcessRepo`（工序映射归 prod
//! 域），后者随 `ShelfOut.account_count` 出参取消而删除（账号绑定真源在 iam 域）。
//! 本文件现在只负责 `t_shelf` 单表。

use sqlx::{PgExecutor, QueryBuilder};

use crate::modules::shelf::model::TShelf;

// ---------------------------------------------------------------------------
// ShelfRepo（t_shelf，9 方法）
// ---------------------------------------------------------------------------

pub struct ShelfRepo;

impl ShelfRepo {
    /// 按 id 查 active 货架（is_active=true, deleted_at IS NULL）。用于 INSPECTION/PRODUCTION 区校验。
    pub async fn get_active_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   capacity, version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND is_active = true AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 id 查（不强制 is_active；用于 service 层区分 20501 NOT_FOUND vs 20512 INACTIVE）。
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   capacity, version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 id + zone 双键查（worker-pool 投放 / 看板用）。
    /// 命中失败 → worker 把 batch 投到不属于自己的区，service 层应拒绝。
    pub async fn get_by_id_zone<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        zone: &str,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, location, is_active, display_order,
                   capacity, version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND zone = $2 AND is_active = true AND deleted_at IS NULL
            "#,
            id,
            zone,
        )
        .fetch_optional(executor)
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
            "SELECT id, code, name, zone, location, is_active, display_order, capacity, \
             version, created_at, created_by, updated_at, updated_by, deleted_at \
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
        let mut qb: QueryBuilder<sqlx::Postgres> =
            QueryBuilder::new("SELECT COUNT(*)::bigint FROM t_shelf WHERE deleted_at IS NULL");
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

    /// 插入新货架。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    ///
    /// `capacity` 2026-10-10 起可传（件数上限）；`None` → 落 NULL = 不限。
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
        capacity: Option<i32>,
    ) -> Result<TShelf, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            INSERT INTO t_shelf (id, code, name, zone, location, is_active, display_order,
                                 capacity, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, true, $6, $7, $8, $8)
            RETURNING id, code, name, zone, location, is_active, display_order,
                      capacity, version, created_at, created_by, updated_at, updated_by, deleted_at
            "#,
            snowflake_id,
            code,
            name,
            zone,
            location,
            display_order,
            capacity,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。
    ///
    /// `location` / `capacity` 三态编码（与 process.update 同形）：
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
        capacity: Option<Option<i32>>,
    ) -> Result<u64, sqlx::Error> {
        let set_location = location.is_some();
        let new_location = location.flatten();
        let set_capacity = capacity.is_some();
        let new_capacity = capacity.flatten();
        sqlx::query!(
            r#"
            UPDATE t_shelf
            SET name          = COALESCE($3::varchar, name),
                location      = CASE WHEN $4::bool THEN $5::varchar ELSE location END,
                display_order = COALESCE($6::integer, display_order),
                capacity      = CASE WHEN $8::bool THEN $9::integer ELSE capacity END,
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
            set_capacity,
            new_capacity,
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

    /// 软删前查引用：单条 `UNION ALL` 统计 `t_part_batch.current_holder_id = shelf_id` 且
    /// `status IN ('IN_PROCESS', 'INSPECTION')` 的非软删批次数。
    ///
    /// 任一分支 > 0 ⇒ 20503 BIZ_SHELF_IN_USE。
    ///
    /// 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `current_holder_id` 列；
    /// 「该 shelf 持有」改查 t_part_batch 真相源（status + holder + location
    /// 三维核对：活跃 + 持有人为该 shelf + location='PRODUCTION_SHELF' 或
    /// 'INSPECTION_SHELF'）。
    ///
    /// 2026-10-01：删掉第 3 个（REPAIRING）子查询。REPAIRING 降级为
    /// `t_part_batch.is_repairing` 标记列后，返修中批次的 `status` 就是
    /// `'IN_PROCESS'`，与第 1 个子查询**同一行**命中 —— 守卫强度不变（返修
    /// 批次仍被算作「在用」，货架仍不可软删），只是少扫一次表。
    pub async fn count_in_use_parts<'e, E: PgExecutor<'e>>(
        executor: E,
        shelf_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT
                (
                    (SELECT COUNT(*) FROM t_part_batch
                     WHERE current_holder_id = $1
                       AND location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')
                       AND status = 'IN_PROCESS'
                       AND deleted_at IS NULL)
                    +
                    (SELECT COUNT(*) FROM t_part_batch
                     WHERE current_holder_id = $1
                       AND location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')
                       AND status = 'INSPECTION'
                       AND deleted_at IS NULL)
                )::bigint AS total
            "#,
        )
        .bind(shelf_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}
