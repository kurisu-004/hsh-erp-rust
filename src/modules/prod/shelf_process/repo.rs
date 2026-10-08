//! shelf ↔ process 映射（`t_shelf_process`）SQL 真源 —— 物理在 `prod` 子模块
//!
//! 对应 Python myERP（无单独 shelf_process 仓储；逻辑在 shelf_repository 内部）。
//! 函数签名接收 `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` /
//! `&mut Transaction`。
//!
//! ## 约定
//! - 全部使用 sqlx 编译期宏（`query!` / `query_as!`）或运行时宏（`query_as` +
//!   `bind`），需 `DATABASE_URL` 或 `.sqlx/` 离线元数据
//! - 读查询一律带 `deleted_at IS NULL`
//! - 软删 `deleted_at = now()`，无乐观锁（mapping 由 set_shelf_processes 整组替换）
//!
//! ## 2026-10-02 域归属反转（shelf 域拆分）
//! 自 `src/modules/shelf/process_mapping/sql.rs` **整文件平移**到本文件（本仓内
//! 平级单文件形态，不是 `sql.rs`/`mod.rs` 目录拆分的继任者）：
//! - 4 个「平移」方法（`list_by_shelf` / `list_all_active_mappings` /
//!   `soft_delete_all_for_shelf` / `bulk_insert`）SQL 与方法签名**零 diff**
//! - 新增「收口」方法 `find_first_shelf_for_process` 供 prod 域内部调用方改调，消灭手写
//!   `t_shelf_process` SQL（`prod::batch` / `prod::queue` 各 1 处）。
//!   ⚠️ 2026-10-10：另一个收口方法 `exists_for_shelf_process` 已删除 —— 它唯一的服务
//!   对象（`move_batch` WORKER→POOL 的映射校验）随目标货架自动选架一并退场，方法零
//!   调用方。**不要**恢复它：要判「架是否映射该工序」就在选架 SQL 的候选集里用
//!   `EXISTS`（见 `shared::shelf::select`），让谓词只留一处。
//!
//! ## 本仓内保留 inline 的 `t_shelf_process` SQL（2026-10-02 判定，不要硬抽）
//! - `prod::batch::repo::preview_auto_dispatch` —— `LEFT JOIN LATERAL t_shelf_process`
//!   在大复合查询里，拆出来是性能回退
//! - `prod::process::repo::count_process_references` —— 5 张表 sub-select 求和，
//!   拆出来多 5 次往返
//! - `prod::batch::service` 2 处（`guard.rs` 的 `assert_shelf_maps_process` /
//!   `worker_scan.rs` 的 RETURNED 分支）—— 都要连 `t_part_batch` 一起判，放一起
//!   省一次往返；拆开反而把「校验 + 写」割成两个事务上下文

use sqlx::PgExecutor;

use crate::infra::snowflake::SnowflakeIdGenerator;

/// 新 mapping 行的输入结构（service 层用，喂给 `ShelfProcessRepo::bulk_insert`）。
#[derive(Debug, Clone)]
pub struct NewShelfProcessRow {
    pub shelf_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// ShelfProcessRepo（t_shelf_process，6 方法 = 平移 4 + 新增 2）
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
        qb.build()
            .execute(executor)
            .await
            .map(|r| r.rows_affected())
    }

    /// 按 `process_id` 取首条**可用**货架映射（多结果取 sort_order 最小者）。
    ///
    /// 2026-10-02 新增：原为 `prod::batch::repo::find_first_shelf_for_process`
    /// （`src/modules/prod/batch/repo.rs`）的手写 SQL，随 shelf↔process 映射搬到本
    /// 文件作为 SQL 真源，调用方 `prod::batch::service::dispatch_single` 改调本方法。
    /// 0 结果 → `Ok(None)`（由 service 层映射 `BIZ_SHELF_PROCESS_NOT_FOUND`）。
    ///
    /// ⚠️ **2026-10-10 起本方法零调用方**（dispatch 的目标货架改走
    /// `shared::shelf::select::pick_least_loaded`）。保留不删的理由与后续处置登记在
    /// `docs/api/queue.md` §8.4 —— 新旧两套「第一」的排序口径不同（映射
    /// `sort_order` vs 货架 `display_order`），**不要**在没想清楚之前把它接回选架
    /// 的退化路径。下面几段记的是它被调用时期的设计取舍，供追溯。
    ///
    /// ⚠️ 2026-10-04 加固（`current_holder_id` 写脏缺口）：原 SQL **不 JOIN `t_shelf`**，
    /// 只要 `t_shelf_process` 行未软删就返回，故已停用 / 已软删 / 品检区货架会被
    /// 下发给批次并写进 `t_part_batch.current_holder_id`。后果不是报错而是**静默漏件**：
    /// 报工台取件页数据源（`part::service::phase1::work_type` 的 pickable-by-work-type）
    /// 的取行 SQL 硬限定 `JOIN t_shelf sh ON sh.id = b.current_holder_id
    /// AND sh.is_active = true AND sh.zone = 'PRODUCTION'`，故这种批次永远不会出现在
    /// 工人的可领列表里。历史脏数据**本仓不自动修**，修复走独立的数据修复单；
    /// 排查时按本方法收窄前的三个谓词（`t_shelf_process.deleted_at IS NULL` /
    /// `t_shelf.is_active` / `t_shelf.zone = 'PRODUCTION'`）自行捞行。
    ///
    /// ## 为什么 JOIN 上 3 个谓词（zone 的判断依据）
    /// 当时的唯一调用方 `prod::batch::service::dispatch_single` 把货架写死成
    /// `location='PRODUCTION_SHELF'` + `current_holder_id=shelf_id` + `status='IN_PROCESS'`
    /// （`BatchRepo::update_batch_dispatched`），且只接待下发（`status ∈ {PENDING,
    /// PROGRAMMING}`，2026-10-06 起含已废弃的 PROGRAMMING）的批次
    /// —— 品检流转（`scan_inspect` / `outsource::receive_*`）走的是另一套显式
    /// `target_inspection_shelf_id` + `validate_shelf_zone(.., "INSPECTION")` 路径，
    /// **不经过本方法**。故 `zone='PRODUCTION'` 与写入不变式一致，不会误伤品检。
    /// `deleted_at IS NULL` / `is_active = true` 则与 `validate_shelf_zone` 的
    /// 判序（存在 → 20501 / 停用 → 20512 / zone → 20104）同源，只是这里用 JOIN
    /// 一次判完。
    ///
    /// ## 为什么是「跳过不可用候选」而不是「命中即报错」
    /// 谓词写在 WHERE 上 ⇒ sort_order 最小的**不可用**货架被静默跳过，继续往后找。
    /// 反过来（在 service 层取到首条再 `validate_shelf_zone` 报错）会在「sort_order=1
    /// 的架已停用、sort_order=2 的架是好的」时把整个下发打成失败，把一个本可自动
    /// 恢复的运维事故升级成阻塞。全部候选都不可用时才返回 `None`，由 dispatch 映射
    /// 既有的 `20508 BIZ_SHELF_PROCESS_NOT_FOUND`（不新造错误码）。
    ///
    /// ⚠️ 2026-10-04 加固：`O` 按 `Option<i64>` 收（外层 `Option` 由
    /// `fetch_optional` 表示「有没有行」，不表示列的类型）。`t_shelf_process.shelf_id`
    /// 当前是 `NOT NULL`，故按 `i64` 解码当前安全；但列一旦变可空，同款写法会以
    /// `error occurred while decoding column 0: unexpected null; try decoding as an Option`
    /// 整笔 500。**该次加固零行为变化**（`NOT NULL` 列 `.flatten()` 恒为 `Some(v)`）。
    /// 同款反模式另见 `prod/batch/service/worker_scan.rs::worker_scan_event`
    /// （那里是可空列，已真修过一次 500）。
    pub async fn find_first_shelf_for_process<'e, E: PgExecutor<'e>>(
        executor: E,
        process_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<Option<i64>> = sqlx::query_scalar(
            r#"
            SELECT sp.shelf_id
            FROM t_shelf_process sp
            JOIN t_shelf s ON s.id = sp.shelf_id
            WHERE sp.process_id = $1
              AND sp.deleted_at IS NULL
              AND s.deleted_at IS NULL
              AND s.is_active = true
              AND s.zone = 'PRODUCTION'
            ORDER BY sp.sort_order ASC, sp.id ASC
            LIMIT 1
            "#,
        )
        .bind(process_id)
        .fetch_optional(executor)
        .await?;
        Ok(row.flatten())
    }
}
