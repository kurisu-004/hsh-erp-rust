//! 微信小程序 BFF 模块 SQL 真源 —— **B3 过渡期：只剩 batch / worker**
//!
//! 2026-10-11 重构：本文件原先持有 4 组结构（`PartCounts` / `PartList` /
//! `BatchCountsAgg` / `BatchList` / `DailyEventCounts` / `WorkerStats`），其中
//! **part 相关的 4 组已随 `wx::part_list` 子模块搬走**：
//!
//! | 已搬走 | 新位置 |
//! |---|---|
//! | `PartCounts` | [`super::part_list::repo::PartListRepo::counts_by_status`] |
//! | `PartList`（`list` / `count` / `by_serial`） | [`super::part_list::repo::PartListRepo::list_parts`] |
//! | `WxPartRow` | [`super::part_list::model::PartListRow`] |
//! | `map_counts_by_status` / `row_to_wx_part` | [`super::part_list::service`]（私有） |
//! | `DailyEventCounts` | **删除**（唯一消费者是已整域删除的 `/wx/dashboard/home`） |
//!
//! ⚠️ **B3（2026-10-11 之后接手）会把本文件剩余内容整体搬进
//! `wx::production::repo`**（配合 `/wx/batches/*` + `/wx/worker/*` →
//! `/wx/production/*` 的 URL 硬切）。届时本文件连同 `vo.rs` 的 batch/worker 部分
//! 一并删除，`wx/mod.rs` 里的两个 `nest` 换成
//! `.nest("/production", production::router())`。
//!
//! ## 2026-10-10 移出：`t_wx_identity`
//! 该表的 SQL 真源已搬到 `modules::iam::repo::sql::wx_identity` —— 它存的是
//! 「企业微信 userid ↔ 本系统 `t_user.id`」的**账号映射**，属 iam 域的数据；wx 域
//! 只是消费方（登录时反查绑定），通过 `AccountService::resolve_wx_login_user`
//! 开口，故 wx 域现在对 `t_wx_identity` 零 SQL。
//!
//! ## 命名
//! - 函数名沿用 `count_xxx` / `list_xxx` / `find_xxx` 三段式
//! - 返回类型用 [`super::vo`] 内的 DTO + 必需的 `FromRow` 中间结构

use sqlx::{PgConnection, PgExecutor};

use super::vo::{BatchCounts, MonthlyStats, WxBatchSummary};

// =============================================================================
// Batch 域聚合（counts / list）
// =============================================================================

pub struct BatchCountsAgg;

impl BatchCountsAgg {
    /// mini-program `GET /wx/batches/counts?period=YYYY-MM`：
    ///
    /// 一次性返回 `(in_progress, done)` 两个计数（当前月）。
    ///
    /// `finished_date` 派生口径：
    /// - in_progress = `status='IN_PROCESS'` AND `updated_at::text LIKE 'YYYY-MM%'`
    ///   （batch 最近一次 update 进入 IN_PROCESS 状态在当月）
    /// - done = `status IN ('DELIVERED','COMPLETED')` AND 存在 batch 对应的
    ///   `t_part_event` 中 `event_type='DELIVERED'` AND 该事件 `created_at::text LIKE 'YYYY-MM%'`
    ///   （批次在当月完成"实际送车"事件；与 dashboard / statistics 域对齐）
    ///
    /// 收 `&mut PgConnection`（不走 `E: PgExecutor`）以支持两次查询复用同一连接
    /// —— 与 `prod/batch/repo/queries.rs::_split_batch_inner` 同形。
    pub async fn by_period(
        conn: &mut PgConnection,
        period: &str, // YYYY-MM
    ) -> Result<BatchCounts, sqlx::Error> {
        // LIKE 通配符：YYYY-MM → "YYYY-MM%"
        let pattern = format!("{period}%");

        let in_progress: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch
            WHERE status = 'IN_PROCESS'
              AND deleted_at IS NULL
              AND updated_at::text LIKE $1
            "#,
            &pattern,
        )
        .fetch_one(&mut *conn)
        .await?;

        let done: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch b
            WHERE b.status IN ('DELIVERED', 'COMPLETED')
              AND b.deleted_at IS NULL
              AND EXISTS (
                  SELECT 1 FROM t_part_event e
                  WHERE e.batch_id = b.id
                    AND e.event_type = 'DELIVERED'
                    AND e.created_at::text LIKE $1
              )
            "#,
            &pattern,
        )
        .fetch_one(&mut *conn)
        .await?;

        Ok(BatchCounts { in_progress, done })
    }
}

/// 批次列表行（FromRow 中间结构）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WxBatchRow {
    pub id: i64,
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub assigned_to: Option<String>,
    pub work_hours: Option<f64>,
    pub finished_date: Option<chrono::NaiveDate>,
    pub due_date: Option<chrono::NaiveDate>,
    pub drawing_url: Option<String>,
}

pub struct BatchList;

impl BatchList {
    /// mini-program `GET /wx/batches?tab=in_progress|done&period=&page=&size=`：
    ///
    /// - `tab` 必填；映射到状态集：
    ///   - `in_progress` → `status='IN_PROCESS'`
    ///   - `done` → `status IN ('DELIVERED','COMPLETED')`
    /// - `period` 必填（YYYY-MM）；`finished_date` 派生口径与
    ///   `BatchCountsAgg::by_period` 一致。
    /// - 排序：`updated_at DESC, id DESC`（最近变更优先）。
    /// - 派生：`assigned_to` = `t_worker.name`（按 batch.current_holder_id +
    ///   batch.location='WORKER'）；`work_hours` = 该批次 PICKED_UP +
    ///   RETURNED 事件的 SUM(quantity)（mini-program 用作工作量估算）；
    ///   `drawing_url` 留空（当前未拉图，handler 层后续按 t_part_file::url
    ///   注入；本 PR 不实现，避免 SQL JOIN 跨 5 表）。
    #[allow(clippy::too_many_arguments)]
    pub async fn list<'e, E: PgExecutor<'e>>(
        executor: E,
        tab: &str, // "in_progress" | "done"
        period: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<WxBatchRow>, sqlx::Error> {
        let pattern = format!("{period}%");
        // SQL 用 (tab, pattern) 二分派，避免两段 SQL 重复（已用 ANY($statuses) 兼容）
        let statuses: Vec<String> = match tab {
            "in_progress" => vec!["IN_PROCESS".to_string()],
            "done" => vec!["DELIVERED".to_string(), "COMPLETED".to_string()],
            // caller 已用 try_into 守好；此处双保险
            _ => return Ok(Vec::new()),
        };

        // period 过滤：in_progress 走 batch.updated_at，done 走 DELIVERED event.created_at
        // —— 与 BatchCountsAgg::by_period 派生口径完全一致。
        let rows = sqlx::query!(
            r#"
            SELECT
                b.id              AS "id!",
                b.part_id         AS "part_id!",
                p.serial_no       AS "serial_no?",
                p.name            AS "name!",
                p.drawing_no      AS "drawing_no!",
                b.batch_no        AS "batch_no!",
                b.quantity        AS "quantity!",
                b.status          AS "status!",
                b.updated_at      AS "updated_at!",
                w.name            AS "assigned_to?",
                p.planned_delivery_date AS "due_date?",
                (
                    SELECT e_finished.created_at::date
                    FROM t_part_event e_finished
                    WHERE e_finished.batch_id = b.id
                      AND e_finished.event_type = 'DELIVERED'
                    ORDER BY e_finished.created_at DESC
                    LIMIT 1
                ) AS "finished_date?",
                (
                    SELECT COALESCE(SUM(quantity), 0)::float8
                    FROM t_part_event e_qty
                    WHERE e_qty.batch_id = b.id
                      AND e_qty.event_type IN ('PICKED_UP', 'RETURNED')
                ) AS "work_hours?"
            FROM t_part_batch b
            JOIN t_part p ON p.id = b.part_id
            LEFT JOIN t_worker w ON w.id = b.current_holder_id AND b.location = 'WORKER'
            WHERE b.status = ANY($1)
              AND b.deleted_at IS NULL
              AND p.deleted_at IS NULL
              AND (
                  (b.status = 'IN_PROCESS' AND b.updated_at::text LIKE $2)
                  OR (
                      b.status IN ('DELIVERED', 'COMPLETED')
                      AND EXISTS (
                          SELECT 1 FROM t_part_event e
                          WHERE e.batch_id = b.id
                            AND e.event_type = 'DELIVERED'
                            AND e.created_at::text LIKE $2
                      )
                  )
              )
            ORDER BY b.updated_at DESC, b.id DESC
            LIMIT $3 OFFSET $4
            "#,
            &statuses,
            &pattern,
            limit,
            offset,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| WxBatchRow {
                id: r.id,
                part_id: r.part_id,
                serial_no: r.serial_no,
                name: r.name,
                drawing_no: r.drawing_no,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                assigned_to: r.assigned_to,
                work_hours: r.work_hours,
                finished_date: r.finished_date,
                due_date: r.due_date,
                drawing_url: None, // 本 PR 不拉图，后续单独 PR
            })
            .collect())
    }

    /// 配套 COUNT（与 `list` 同 WHERE）。
    pub async fn count<'e, E: PgExecutor<'e>>(
        executor: E,
        tab: &str,
        period: &str,
    ) -> Result<i64, sqlx::Error> {
        let pattern = format!("{period}%");
        let statuses: Vec<String> = match tab {
            "in_progress" => vec!["IN_PROCESS".to_string()],
            "done" => vec!["DELIVERED".to_string(), "COMPLETED".to_string()],
            _ => return Ok(0),
        };
        let n: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch b
            JOIN t_part p ON p.id = b.part_id
            WHERE b.status = ANY($1)
              AND b.deleted_at IS NULL
              AND p.deleted_at IS NULL
              AND (
                  (b.status = 'IN_PROCESS' AND b.updated_at::text LIKE $2)
                  OR (
                      b.status IN ('DELIVERED', 'COMPLETED')
                      AND EXISTS (
                          SELECT 1 FROM t_part_event e
                          WHERE e.batch_id = b.id
                            AND e.event_type = 'DELIVERED'
                            AND e.created_at::text LIKE $2
                      )
                  )
              )
            "#,
            &statuses,
            &pattern,
        )
        .fetch_one(executor)
        .await?;
        Ok(n)
    }
}

// =============================================================================
// Worker 月度统计（`GET /wx/worker/stats?period=YYYY-MM`）
// =============================================================================

pub struct WorkerStats;

impl WorkerStats {
    /// `MonthlyStats { batch_count, work_hours }`：
    /// - `batch_count` = 该工人在该月发生过事件的不同 batch_id 数
    /// - `work_hours` = 该工人在该月 `PICKED_UP + RETURNED` 事件的 SUM(quantity)
    ///   （mini-program 用作工作量估算；DB 无 work_hours 列）
    pub async fn by_user_period<'e, E: PgExecutor<'e>>(
        executor: E,
        user_id: i64,
        period: &str,
    ) -> Result<MonthlyStats, sqlx::Error> {
        let pattern = format!("{period}%");
        // 注意：worker_id 字段在 t_part_event 里是 worker 雪花 id；
        // CurrentUser.id 是 t_user.id——二者目前都按雪花 ID 共享 ID 空间
        // （migration 071 起 worker / user 都走统一雪花）。如果发现 worker
        // 表的 worker_id 是 user 表的子集 / 独立空间，这里需要调整。
        let row = sqlx::query!(
            r#"
            SELECT
                COUNT(DISTINCT batch_id) FILTER (WHERE batch_id IS NOT NULL) AS "batch_count!",
                COALESCE(
                    SUM(quantity) FILTER (WHERE event_type IN ('PICKED_UP', 'RETURNED')),
                    0
                )::bigint AS "qty_sum!"
            FROM t_part_event
            WHERE worker_id = $1
              AND created_at::text LIKE $2
            "#,
            user_id,
            &pattern,
        )
        .fetch_one(executor)
        .await?;
        Ok(MonthlyStats {
            batch_count: row.batch_count,
            work_hours: row.qty_sum as f64, // qty 累计作为 work_hours 估算
        })
    }
}

// =============================================================================
// 行 → DTO 转换 helpers（pub(super)，供各 handler 复用）
// =============================================================================

pub(super) fn row_to_wx_batch(row: WxBatchRow) -> WxBatchSummary {
    WxBatchSummary {
        id: row.id,
        part_id: row.part_id,
        serial_no: row.serial_no,
        name: row.name,
        drawing_no: row.drawing_no,
        batch_no: row.batch_no,
        quantity: row.quantity,
        status: row.status,
        assigned_to: row.assigned_to,
        work_hours: row.work_hours,
        finished_date: row.finished_date,
        due_date: row.due_date,
        drawing_url: row.drawing_url,
    }
}
