//! process 域数据访问（SQL 真源，零 diff 搬迁自 `repo.rs`）
//!
//! 对应 Python myERP/repository/process_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P2 process CRUD 暴露给 service 的能力：
//! - 读：`get_by_id` / `get_by_code`
//! - 过滤+分页+计数：`list_with_filters` / `count_with_filters`（QueryBuilder，防 N+1）
//! - 写：`create` / `update` / `soft_delete`
//! - 引用计数：`count_process_references`（软删前查引用，best-effort）
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! ## 2026-09-22 重构（D-2-simple）
//! 原 `repo.rs` 平移到 `repo/sql.rs`，本文件 SQL 与方法签名零 diff，
//! `.sqlx/query-*.json` 哈希不变；新增的 `ProcessRepoTrait` 胖 trait 在 `repo/mod.rs`。
//! trait 已直接 `impl for &mut PgConnection`（与 iam 2026-09-22 同步，零转发壳）。

use sqlx::{PgExecutor, QueryBuilder};

use super::super::model::TProcess;

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
                   requires_approval, color, is_cnc
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
                   requires_approval, color, is_cnc
            FROM t_process
            WHERE code = $1 AND deleted_at IS NULL
            "#,
            code,
        )
        .fetch_optional(executor)
        .await
    }

    /// 批量按 id 查（活跃行）。空切片短路返回空 Vec。
    ///
    /// 用于 `prod::shelf_process::service::ShelfProcessService::set_shelf_processes`
    /// 校验 items 里的所有 process_id 都存在；走 `WHERE id = ANY($1)` 单次往返
    /// （防 N+1）。2026-10-02 域拆分后调用方从 `shelf::process_mapping` 换到同域
    /// `prod::shelf_process`（shelf 侧的两个反向 helper 已删），本方法零改动。
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
                   requires_approval, color, is_cnc
            FROM t_process
            WHERE id = ANY($1)
              AND deleted_at IS NULL
            ORDER BY id ASC
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
             requires_approval, color, is_cnc \
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
        let mut qb: QueryBuilder<sqlx::Postgres> =
            QueryBuilder::new("SELECT COUNT(*)::bigint FROM t_process WHERE deleted_at IS NULL");
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
        color: Option<&str>,
        is_cnc: bool,
        created_by: i64,
    ) -> Result<TProcess, sqlx::Error> {
        sqlx::query_as!(
            TProcess,
            r#"
            INSERT INTO t_process (id, code, name, category, sort_order, description,
                                   requires_approval, color, is_cnc, created_by, updated_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
            RETURNING id, code, name, category, sort_order, description, version,
                      created_at, created_by, updated_at, updated_by, deleted_at,
                      requires_approval, color, is_cnc
            "#,
            snowflake_id,
            code,
            name,
            category,
            sort_order,
            description,
            requires_approval,
            color,
            is_cnc,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 部分更新（OCC）：带乐观锁。`code` 不允许改（业务唯一键，由 service 层 enforce）。
    ///
    /// 三态编码：
    /// - `name` / `sort_order` / `description` / `color` / `requires_approval`：None ⇒ 不改
    /// - `description` / `color` 三态编码 `Option<Option<&str>>`：
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
        color: Option<Option<&str>>,
        is_cnc: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_description = description.is_some();
        let new_description = description.flatten();
        let set_color = color.is_some();
        let new_color = color.flatten();
        sqlx::query!(
            r#"
            UPDATE t_process
            SET name            = COALESCE($3::varchar, name),
                sort_order      = COALESCE($4::integer, sort_order),
                description     = CASE WHEN $5::bool THEN $6::varchar ELSE description END,
                requires_approval = COALESCE($7::boolean, requires_approval),
                color           = CASE WHEN $8::bool THEN $9::varchar ELSE color END,
                is_cnc          = COALESCE($11::boolean, is_cnc),
                version         = version + 1,
                updated_at      = now(),
                updated_by      = $10
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            sort_order,
            set_description,
            new_description,
            requires_approval,
            set_color,
            new_color,
            updated_by,
            is_cnc,
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
    /// `t_shelf_process` + `t_part.next_process_id` + `t_process_chain_step` 5 张表
    /// 的引用计数总和。
    ///
    /// 前 3 张 junction 表的 sub-select **不过滤** `deleted_at`：它们按整组替换语义
    /// 写入（整组替换 = 软删旧行 + 插新行），引用计数若只算 active 行，用户清空一次
    /// 映射就能绕过 20803 把仍在历史映射里的工序软删掉。`t_part` /
    /// `t_process_chain_step` 两张实体表则带 `deleted_at IS NULL`。
    ///
    /// 2026-09-17 PR-4 守卫修复：补 `t_process_chain_step.process_id`（PR-1 工艺链
    /// FK 翻转 + PR-3 批次 step 化后，part → chain → step 是新的工艺引用通道；
    /// 之前缺这条会漏掉「工艺链 step 仍引用此 process」场景，软删后 step 的
    /// process_id 指向已软删 process 会撞 23503）。`t_part.next_process_id`
    /// 保留作为 rollup 派生缓存（migration 027 不动），继续纳入计数。
    ///
    /// **best-effort**：mapping 表（work_type_process / outsource_company_process /
    /// process_chain_step）目前 Rust 端没有专门的 repo 暴露，本查询用单条
    /// sub-select 加法一次往返；若对应表当前不存在（理论上不应发生，迁移
    /// 003/004/005/017 已建），本函数会被 PostgreSQL 拒绝，service 层把 sqlx
    /// 错误转 `BIZ_PROCESS_IN_USE`（保守：宁可误拒也不放过真引用）。当前阶段所有
    /// 5 张表均已迁移到位，best-effort 注释仅留给后续 junction repo 拆分时回看。
    ///
    /// 2026-10-02 域拆分：原注释「后续 junction repo 拆分时回看」已兑现一处 ——
    /// `t_shelf_process` 的 SQL 真源搬到
    /// `crate::modules::prod::shelf_process::repo::ShelfProcessRepo`（见同目录
    /// `shelf-process-mapping.md`）。但本函数**仍保留 inline `t_shelf_process`
    /// sub-select，不抽出去**：它是 5 张表 sub-select 加法，拆出来要多 5 次往返
    /// （其中 `t_shelf_process` 那次还是纯计数）。剩余
    /// `t_work_type_process`（归 `prod::work_type`）/ `t_outsource_company_process`
    /// （归 `outsource` 域）/ `t_process_chain_step`（归 `prod::process_chain`）3 张
    /// 表同样保持 inline —— 拆 junction repo 的收益是「写路径有单一入口」，而本函数
    /// 是**只读计数**，不存在写路径分叉问题。
    ///
    /// 2026-09-30（review 第 1 轮 M3）两点补充：
    /// - **行为收紧**：`t_part.next_process_id` 的 rollup 派生源已改为直读
    ///   `t_part_batch.current_process_id`（migration 004）。此前无工序链的工单该列
    ///   恒为 NULL（被抹掉），现在会被正常填上真实 process_id → **软删该工序会比
    ///   以前更容易被 `BIZ_PROCESS_IN_USE`（20803）拒**。这是修正（原防线静默失效）。
    /// - **`t_part_batch.current_process_id` 不在本查询的 5 张表里**（它是新增的
    ///   到 `t_process` 的引用通道）。实际风险低：所有写入该列的路径都先过工序
    ///   存在性校验（`assert_shelf_maps_process` / `find_first_shelf_for_process` /
    ///   worker-scan 的 `t_shelf_process` 映射校验），而 `t_shelf_process` 已在计数内
    ///   间接兜住。但这是**隐式依赖而非显式不变量** —— 若将来新增不经货架映射直接写
    ///   该列的路径，本守卫会漏判，届时应在此补一条
    ///   `t_part_batch WHERE current_process_id = $1 AND deleted_at IS NULL`。
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
                    +
                    (SELECT COUNT(*) FROM t_process_chain_step
                     WHERE process_id = $1 AND deleted_at IS NULL)
                )::bigint AS total
            "#,
        )
        .bind(process_id)
        .fetch_one(executor)
        .await?;
        Ok(row.0)
    }
}
