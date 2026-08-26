//! process 域数据访问
//!
//! 对应 Python myERP/repository/process_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P2 process CRUD 暴露给 service 的能力：
//! - 读：`get_by_id` / `get_by_code` / `list_by_ids`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder，防 N+1）
//! - 写：`create` / `update` / `soft_delete`
//! - 引用计数：`count_process_references`（软删前查引用，best-effort）
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409

use sqlx::{PgExecutor, QueryBuilder};

use super::model::TProcess;

pub struct ProcessRepo;

impl ProcessRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        sqlx::query_as!(
            TProcess,
            r#"
            SELECT id, code, name, category, sort_order, description, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   requires_approval
            FROM t_process
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 code 精确查找（活跃行）。重复 code 校验用；inhouse/outsource 类别不在这里约束。
    pub async fn get_by_code<'e, E: PgExecutor<'e>>(
        executor: E,
        code: &str,
    ) -> Result<Option<TProcess>, sqlx::Error> {
        sqlx::query_as!(
            TProcess,
            r#"
            SELECT id, code, name, category, sort_order, description, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   requires_approval
            FROM t_process
            WHERE code = $1 AND deleted_at IS NULL
            "#,
            code,
        )
        .fetch_optional(executor)
        .await
    }

    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TProcess,
            r#"
            SELECT id, code, name, category, sort_order, description, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   requires_approval
            FROM t_process
            WHERE id = ANY($1) AND deleted_at IS NULL
            "#,
            ids,
        )
        .fetch_all(executor)
        .await
    }

    /// 过滤+分页：用 `QueryBuilder` 动态拼 `code_like` / `category` 二态过滤。
    /// 一次往返即可拿全表行，避免 N+1。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
        category: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT id, code, name, category, sort_order, description, version, \
             created_at, created_by, updated_at, updated_by, deleted_at, \
             requires_approval \
             FROM t_process WHERE deleted_at IS NULL",
        );
        if let Some(cat) = category {
            let trimmed = cat.trim();
            if !trimmed.is_empty() {
                qb.push(" AND category = ").push_bind(trimmed.to_string());
            }
        }
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

        qb.build_query_as::<TProcess>().fetch_all(executor).await
    }

    /// 同 `list_with_filters` 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        code_like: Option<&str>,
        category: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_process WHERE deleted_at IS NULL",
        );
        if let Some(cat) = category {
            let trimmed = cat.trim();
            if !trimmed.is_empty() {
                qb.push(" AND category = ").push_bind(trimmed.to_string());
            }
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

    /// 插入新工序。雪花 id 由调用方（service）生成；created_by / updated_by
    /// 共用 `created_by`，后续 UPDATE 才更新 updated_by。
    #[allow(clippy::too_many_arguments)]
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        snowflake_id: i64,
        code: &str,
        name: &str,
        category: &str,
        sort_order: i32,
        description: Option<&str>,
        requires_approval: bool,
        created_by: i64,
    ) -> Result<TProcess, sqlx::Error> {
        sqlx::query_as!(
            TProcess,
            r#"
            INSERT INTO t_process (id, code, name, category, sort_order, description,
                                   requires_approval, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            RETURNING id, code, name, category, sort_order, description, version,
                      created_at, created_by, updated_at, updated_by, deleted_at,
                      requires_approval
            "#,
            snowflake_id,
            code,
            name,
            category,
            sort_order,
            description,
            requires_approval,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。`code` 不允许改（业务唯一键，由 service 层 enforce）。
    ///
    /// 三态编码：
    /// - `name` / `sort_order` / `description` / `requires_approval`：None ⇒ 不改
    /// - `description` 三态编码 `Option<Option<&str>>`：
    ///   - `None` ⇒ 字段缺省，不修改
    ///   - `Some(None)` ⇒ 显式清空（SET NULL）
    ///   - `Some(Some(v))` ⇒ 改值
    #[allow(clippy::too_many_arguments)]
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: Option<&str>,
        sort_order: Option<i32>,
        description: Option<Option<&str>>,
        requires_approval: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_description = description.is_some();
        let new_description = description.flatten();
        sqlx::query!(
            r#"
            UPDATE t_process
            SET name            = COALESCE($3::varchar, name),
                sort_order      = COALESCE($4::integer, sort_order),
                description     = CASE WHEN $5::bool THEN $6::varchar ELSE description END,
                requires_approval = COALESCE($7::boolean, requires_approval),
                version         = version + 1,
                updated_at      = now(),
                updated_by      = $8
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            sort_order,
            set_description,
            new_description,
            requires_approval,
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
            UPDATE t_process
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

    /// 软删前查引用：跨 `t_work_type_process` + `t_outsource_company_process` +
    /// `t_shelf_process` + `t_part.next_process_id` 4 张表的引用计数总和。
    ///
    /// **best-effort**：mapping 表（work_type_process / outsource_company_process /
    /// shelf_process）目前 Rust 端没有专门的 repo 暴露，本查询用单条 `UNION ALL` 一次往返；
    /// 若对应表当前不存在（理论上不应发生，迁移 003/004/005 已建），本函数会被 PostgreSQL
    /// 拒绝，service 层把 sqlx 错误转 `BIZ_PROCESS_IN_USE`（保守：宁可误拒也不放过真引用）。
    /// 当前阶段（Phase P2）所有 4 张表均已迁移到位，best-effort 注释仅留给后续 junction
    /// repo 拆分时回看。
    pub async fn count_process_references<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT
                (
                    (SELECT COUNT(*) FROM t_work_type_process
                     WHERE process_id = $1)
                    +
                    (SELECT COUNT(*) FROM t_outsource_company_process
                     WHERE process_id = $1)
                    +
                    (SELECT COUNT(*) FROM t_shelf_process
                     WHERE process_id = $1)
                    +
                    (SELECT COUNT(*) FROM t_part
                     WHERE next_process_id = $1 AND deleted_at IS NULL)
                )::bigint AS total
            "#,
        )
        .bind(process_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}