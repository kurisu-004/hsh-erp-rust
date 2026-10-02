//! 微信小程序 BFF 模块 SQL 真源（2026-09-28 新增）
//!
//! 全部 SQL 在此文件集中声明（ZST `WxRepo` + 固有静态方法），service / handler
//! 借 `&mut PgConnection` 调用即可。**不**抽 trait — 本模块端点全是「薄」只读
//! 聚合（无业务规则分支、无 OCC、无乐观锁），trait 抽象带来的 mock 收益小于
//! 维护成本（按 `prod/batch/repo/queries.rs` 2026-09-22 PR2 总结的取舍）。
//!
//! ## 2026-09-29 扩展：`t_wx_identity` 写侧
//! 企业微信小程序登录引入**写操作**（admin 绑定 / 解绑）。仍沿用 ZST + 静态方法
//! 形态（本文件是 wx 域唯一 SQL 真源，形状统一比「为 4 个方法单独开 trait」更重要）。
//! DB model（`WxIdentity` / `WxIdentityInsert`）也放本文件而非 `vo.rs`——`vo.rs`
//! 是「HTTP 响应序列化层」，DB model 带 `version` / 审计字段 / 无 `Serialize`，
//! 语义完全不同。
//!
//! ## 命名
//! - 函数名沿用 `count_xxx` / `list_xxx` / `find_xxx` 三段式
//! - 返回类型用 `mod vo { ... }` 内的 DTO + 必需的 FromRow 中间结构（避免污染
//!   `vo.rs` —— 中间结构无 `Serialize`）

use chrono::NaiveDateTime;
use sqlx::{PgConnection, PgExecutor};

use super::vo::{BatchCounts, CountsByStatus, MonthlyStats, WxBatchSummary, WxPartSummary};

// =============================================================================
// Part 域聚合（counts / list / by-serial）
// =============================================================================

/// 工单计数 ZST（与 `prod/batch/repo/queries.rs::PartBatchRepo` 同形：SQL 真源 + 静态方法）。
pub struct PartCounts;

impl PartCounts {
    /// mini-program `GET /wx/parts/counts` 与 `GET /wx/dashboard/home` 共用聚合：
    /// 一次性 `GROUP BY status` 拉全部状态计数，由 caller 字段映射到 4 个 tab。
    ///
    /// 返回 `Vec<(status, count)>`：每个非 0 状态一行；caller 用 HashMap 解构填
    /// `CountsByStatus { all, pending_production, ... }`（缺位补 0）。
    pub async fn by_status<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT status AS "status!", COUNT(*) AS "cnt!"
            FROM t_part
            WHERE deleted_at IS NULL
            GROUP BY status
            "#,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows.into_iter().map(|r| (r.status, r.cnt)).collect())
    }
}

/// 把 `Vec<(status, count)>` 映射到 `CountsByStatus`（与 4 个 tab 对齐）。
///
/// 设计见 `vo::CountsByStatus` 字段级 doc。
pub fn map_counts_by_status(rows: Vec<(String, i64)>) -> CountsByStatus {
    let mut out = CountsByStatus {
        all: 0,
        pending_production: 0,
        in_production: 0,
        pending_inspection: 0,
        delivered: 0,
    };
    for (s, c) in rows {
        out.all += c;
        match s.as_str() {
            "PENDING" => out.pending_production += c,
            "IN_PROCESS" => out.in_production += c,
            "INSPECTION" => out.pending_inspection += c,
            "READY_TO_SHIP" | "DELIVERED" => out.delivered += c,
            // PROGRAMMING / OUTSOURCE / COMPLETED / CANCELLED 仅计入 all
            //
            // 2026-10-01：删掉 `REPAIRING`。REPAIRING 已从 `PartStatus` 降级为
            // `t_part_batch.is_repairing` 标记列（migration 005/006），
            // `t_part.status` 里不再出现该值（rollup 输出恒为 `IN_PROCESS`）。
            // 返修中的工单**自动**由上面 `"IN_PROCESS"` 臂计入
            // `in_production` —— 这正是期望口径：返修仍在生产中，与
            // 「生产中」tab 同属用户视角下的在厂货，拆出去单列 tab 反而会
            // 让「生产中」少算正在返修的量。
            _ => {}
        }
    }
    out
}

/// 工单列表 / by-serial 行（FromRow 中间结构；不带 Serialize — 不进 vo.rs）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WxPartRow {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub status: String,
    pub is_urgent: bool,
    pub planned_delivery_date: chrono::NaiveDate,
    pub customer_id: i64,
    pub customer_name: Option<String>,
    /// 当前活跃批次（status != COMPLETED && != CANCELLED 的非软删批次；
    /// 多个时取 batch_no ASC 第一个）。用于详情跳转。
    pub current_batch_id: Option<i64>,
    pub current_batch_no: Option<i32>,
    pub current_holder_label: Option<String>,
    pub assembly_id: Option<i64>,
}

pub struct PartList;

impl PartList {
    /// mini-program `GET /wx/parts?status=&page=&size=`：
    ///
    /// - status 过滤：可选 `PENDING` / `PROGRAMMING` / `IN_PROCESS` / `INSPECTION` /
    ///   `READY_TO_SHIP` / `DELIVERED` / `OUTSOURCE` / `COMPLETED` /
    ///   `CANCELLED`；None 或 `"all"` 返回全部非软删件。
    ///   2026-10-01：删掉 `REPAIRING` —— REPAIRING 已降级为
    ///   `t_part_batch.is_repairing` 标记列（migration 005/006），
    ///   `t_part.status` 里不再出现该值；返修中的工单按 `IN_PROCESS` 过滤即可
    ///   （注意：小程序端**没有**按 `is_repairing` 单独过滤的口径，返修态对
    ///   用户可见性等价于生产中）。本 SQL 的过滤是 `$1::text` 直传（无白名单
    ///   校验），传 `REPAIRING` 只会得到空列表而非报错。
    /// - 排序：`is_urgent DESC, planned_delivery_date ASC, id ASC`（紧急 + 交期近
    ///   优先）。注意这是**服务端硬编码**的「紧急件优先」，与 Web 端
    ///   `GET /prod/batches/inspection` 的表头点列排序（`is_urgent` 不参与排序，
    ///   由前端自行标红）口径不同。
    /// - JOIN：`t_customer`（取客户名）+ 层级 LEFT JOIN `t_part_batch` / `t_shelf` /
    ///   `t_worker` / `t_outsource_company` 解析当前 holder 标签。
    /// - 一次聚合（无 N+1）：对每个 part 拿「当前活跃批次」（多批次时取 batch_no ASC
    ///   第一条 active 批次；b.deleted_at IS NULL 且 status NOT IN ('COMPLETED',
    ///   'CANCELLED')）；并 JOIN 解析 holder。
    ///
    /// 已知折中（与 dashboard 域 `COALESCE(s.name, w.name, oc.name)` 同形 bug，
    /// 见 `prod::batch::repo::mod` 模块 doc 的「holder 三表 COALESCE 的多态歧义」
    /// 一节）：holder 多态歧义时优先 shelf.name。mini-program 不强依赖此字段精确性，
    /// 仅作展示。
    #[allow(clippy::too_many_arguments)]
    pub async fn list<'e, E: PgExecutor<'e>>(
        executor: E,
        status_filter: Option<&str>,
        customer_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<WxPartRow>, sqlx::Error> {
        // status_filter 为 None 或 "all" → 视为不过滤
        let effective_status: Option<&str> = match status_filter {
            None => None,
            Some(s) if s.eq_ignore_ascii_case("all") => None,
            Some(s) => Some(s),
        };
        let rows = sqlx::query!(
            r#"
            SELECT
                p.id              AS "id!",
                p.serial_no       AS "serial_no?",
                p.name            AS "name!",
                p.drawing_no      AS "drawing_no!",
                p.quantity        AS "quantity!",
                p.status          AS "status!",
                p.is_urgent       AS "is_urgent!",
                p.planned_delivery_date AS "planned_delivery_date!",
                p.customer_id     AS "customer_id!",
                p.assembly_id     AS "assembly_id?",
                c.name            AS "customer_name?",
                cb.id             AS "cb_id?",
                cb.batch_no       AS "cb_batch_no?",
                COALESCE(sh.code, w.name, oc.name) AS "holder_label?"
            FROM t_part p
            LEFT JOIN t_customer c ON c.id = p.customer_id
            LEFT JOIN LATERAL (
                SELECT id, batch_no, current_holder_id, location
                FROM t_part_batch pb
                WHERE pb.part_id = p.id
                  AND pb.deleted_at IS NULL
                  AND pb.status NOT IN ('COMPLETED', 'CANCELLED')
                ORDER BY pb.batch_no ASC
                LIMIT 1
            ) cb ON TRUE
            LEFT JOIN t_shelf            sh ON sh.id = cb.current_holder_id
            LEFT JOIN t_worker           w  ON w  .id = cb.current_holder_id
            LEFT JOIN t_outsource_company oc ON oc.id = cb.current_holder_id
            WHERE p.deleted_at IS NULL
              AND ($1::text IS NULL OR p.status = $1::text)
              AND ($2::bigint IS NULL OR p.customer_id = $2::bigint)
            ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC
            LIMIT $3 OFFSET $4
            "#,
            effective_status,
            customer_id,
            limit,
            offset,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| WxPartRow {
                id: r.id,
                serial_no: r.serial_no,
                name: r.name,
                drawing_no: r.drawing_no,
                quantity: r.quantity,
                status: r.status,
                is_urgent: r.is_urgent,
                planned_delivery_date: r.planned_delivery_date,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                current_batch_id: r.cb_id,
                current_batch_no: r.cb_batch_no,
                current_holder_label: r.holder_label,
                assembly_id: r.assembly_id,
            })
            .collect())
    }

    /// 配套 COUNT（与 `list` 同 WHERE）。
    pub async fn count<'e, E: PgExecutor<'e>>(
        executor: E,
        status_filter: Option<&str>,
        customer_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        let effective_status: Option<&str> = match status_filter {
            None => None,
            Some(s) if s.eq_ignore_ascii_case("all") => None,
            Some(s) => Some(s),
        };
        let n: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part p
            WHERE p.deleted_at IS NULL
              AND ($1::text IS NULL OR p.status = $1::text)
              AND ($2::bigint IS NULL OR p.customer_id = $2::bigint)
            "#,
            effective_status,
            customer_id,
        )
        .fetch_one(executor)
        .await?;
        Ok(n)
    }

    /// mini-program `GET /wx/parts/by-serial/{serial_no}`：扫码定位。
    ///
    /// 与 `part::repo::sql::part_sql.rs::get_by_serial` 同投影（完整 25 列 TPart +
    /// 客户名 + 当前批次 id），但为 mini-program 收窄到 WxPartSummary 字段集。
    /// 0 行 → `Ok(None)`，由 handler 转 `40400 NOT_FOUND`。
    pub async fn by_serial<'e, E: PgExecutor<'e>>(
        executor: E,
        serial_no: &str,
    ) -> Result<Option<WxPartRow>, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT
                p.id              AS "id!",
                p.serial_no       AS "serial_no?",
                p.name            AS "name!",
                p.drawing_no      AS "drawing_no!",
                p.quantity        AS "quantity!",
                p.status          AS "status!",
                p.is_urgent       AS "is_urgent!",
                p.planned_delivery_date AS "planned_delivery_date!",
                p.customer_id     AS "customer_id!",
                p.assembly_id     AS "assembly_id?",
                c.name            AS "customer_name?",
                cb.id             AS "cb_id?",
                cb.batch_no       AS "cb_batch_no?",
                COALESCE(sh.code, w.name, oc.name) AS "holder_label?"
            FROM t_part p
            LEFT JOIN t_customer c ON c.id = p.customer_id
            LEFT JOIN LATERAL (
                SELECT id, batch_no, current_holder_id, location
                FROM t_part_batch pb
                WHERE pb.part_id = p.id
                  AND pb.deleted_at IS NULL
                  AND pb.status NOT IN ('COMPLETED', 'CANCELLED')
                ORDER BY pb.batch_no ASC
                LIMIT 1
            ) cb ON TRUE
            LEFT JOIN t_shelf            sh ON sh.id = cb.current_holder_id
            LEFT JOIN t_worker           w  ON w  .id = cb.current_holder_id
            LEFT JOIN t_outsource_company oc ON oc.id = cb.current_holder_id
            WHERE p.serial_no = $1 AND p.deleted_at IS NULL
            "#,
            serial_no,
        )
        .fetch_optional(executor)
        .await?;
        Ok(row.map(|r| WxPartRow {
            id: r.id,
            serial_no: r.serial_no,
            name: r.name,
            drawing_no: r.drawing_no,
            quantity: r.quantity,
            status: r.status,
            is_urgent: r.is_urgent,
            planned_delivery_date: r.planned_delivery_date,
            customer_id: r.customer_id,
            customer_name: r.customer_name,
            current_batch_id: r.cb_id,
            current_batch_no: r.cb_batch_no,
            current_holder_label: r.holder_label,
            assembly_id: r.assembly_id,
        }))
    }
}

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
// 当日 PICKED / DELIVERED 计数（首页聚合）
// =============================================================================

pub struct DailyEventCounts;

impl DailyEventCounts {
    /// `GET /wx/dashboard/home` 用的今日统计：
    /// - `today_picked` = 今日 `event_type='PICKED_UP'` 事件数（DB 实际词汇；
    ///   spec 简写为 'PICKED'——这里按 DB 真值实现）
    /// - `today_delivered` = 今日 `event_type='DELIVERED'` 事件数
    ///
    /// 一次 SQL 拉两个计数（CTE + FILTER），避免 2 次 round-trip。
    pub async fn today<'e, E: PgExecutor<'e>>(executor: E) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE event_type = 'PICKED_UP') AS "picked!",
                COUNT(*) FILTER (WHERE event_type = 'DELIVERED') AS "delivered!"
            FROM t_part_event
            WHERE created_at::date = CURRENT_DATE
            "#,
        )
        .fetch_one(executor)
        .await?;
        Ok((row.picked, row.delivered))
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

pub(super) fn row_to_wx_part(row: WxPartRow) -> WxPartSummary {
    use crate::modules::part::statemachine::PartStatus;
    use crate::modules::wx::vo::WxPartKind;
    // status string → enum（DB 已校验词表；to_status 失败用 PENDING 兜底以避免 panic）
    let status = PartStatus::from_str(&row.status).unwrap_or(PartStatus::PENDING);
    // kind 推断：assembly_id IS NULL → workOrder；非 NULL → batch
    // （mini-program 视图层分组用；实际 DB 无 kind 列，按 assembly 归属推断）
    let kind = if row.assembly_id.is_some() {
        WxPartKind::Batch
    } else {
        WxPartKind::WorkOrder
    };
    WxPartSummary {
        id: row.id,
        serial_no: row.serial_no,
        name: row.name,
        drawing_no: row.drawing_no,
        quantity: row.quantity,
        status,
        is_urgent: row.is_urgent,
        planned_delivery_date: row.planned_delivery_date,
        customer_name: row.customer_name,
        current_batch_id: row.current_batch_id,
        current_batch_no: row.current_batch_no,
        current_holder_label: row.current_holder_label,
        kind,
    }
}

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

// =============================================================================
// 企业微信身份映射（t_wx_identity，2026-09-29 新增）
//
// 仅预绑定：未绑定的 userid 由 handler 拒绝（40107），**不自动开户**。
// 写入侧由 iam 域的 admin 绑定端点（`/iam/users/{id}/wx-bind`）调用。
// =============================================================================

/// `t_wx_identity` 行（企业微信 userid → 系统账号 t_user.id 的预绑定）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WxIdentity {
    pub id: i64,
    /// 企业 ID（来自 `WECOM_CORPID`；多企业部署时同一 userid 在不同企业独立）
    pub corp_id: String,
    /// 企业微信 userid（自建应用返回明文；存小写——企微 userid 不区分大小写）
    pub wx_user_id: String,
    /// 对应的系统账号雪花 ID
    pub user_id: i64,
    /// 乐观锁版本（解绑 soft_delete 时带条件）
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_wx_identity` INSERT 入参（id 由调用方用雪花生成，审计字段同批填好）
#[derive(Debug, Clone)]
pub struct WxIdentityInsert {
    pub id: i64,
    pub corp_id: String,
    /// 已 trim + 转小写的 userid
    pub wx_user_id: String,
    pub user_id: i64,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
}

/// `t_wx_identity` SQL 真源（ZST + 静态方法，与本文件其余分组同形）
pub struct WxIdentityRepo;

impl WxIdentityRepo {
    /// 按 `(corp_id, wx_user_id)` 查活跃绑定（wx-login 主路径）。
    /// 0 行 → `Ok(None)`，handler 转 `40107 BIZ_WX_NOT_BOUND`。
    pub async fn get_by_corp_and_user<'e, E: PgExecutor<'e>>(
        executor: E,
        corp_id: &str,
        wx_user_id: &str,
    ) -> Result<Option<WxIdentity>, sqlx::Error> {
        sqlx::query_as!(
            WxIdentity,
            r#"
            SELECT id, corp_id, wx_user_id, user_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_wx_identity
            WHERE corp_id = $1 AND wx_user_id = $2 AND deleted_at IS NULL
            "#,
            corp_id,
            wx_user_id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 查某系统账号的全部活跃绑定（`GET /iam/users/{id}/wx-bind` 读端点 +
    /// admin 界面「该账号绑了谁」展示）。
    pub async fn list_by_user_id<'e, E: PgExecutor<'e>>(
        executor: E,
        user_id: i64,
    ) -> Result<Vec<WxIdentity>, sqlx::Error> {
        sqlx::query_as!(
            WxIdentity,
            r#"
            SELECT id, corp_id, wx_user_id, user_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_wx_identity
            WHERE user_id = $1 AND deleted_at IS NULL
            ORDER BY created_at ASC, id ASC
            "#,
            user_id,
        )
        .fetch_all(executor)
        .await
    }

    /// 新增一条绑定。
    ///
    /// 唯一索引 `uk_wx_identity_corp_user`（partial unique，soft-deleted 行不参与）
    /// 是并发下的最终防线：应用层的「先查后插」存在 TOCTOU 窗口，撞唯一索引时
    /// 由 service 层把 `sqlx::Error::Database(unique_violation)` 翻译成
    /// `40108 BIZ_WX_BINDING_DUPLICATE`。
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        insert: &WxIdentityInsert,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_wx_identity
                (id, corp_id, wx_user_id, user_id, version, created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, $4, 0, $5, $6, $5, $6)
            "#,
            insert.id,
            insert.corp_id,
            insert.wx_user_id,
            insert.user_id,
            insert.created_at,
            insert.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 软删一条绑定（解绑）。带乐观锁：影响 0 行 = 并发已被改 / 已解绑。
    ///
    /// 返回受影响行数供 service 层判 409（`code::VERSION_CONFLICT`）。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_wx_identity
            SET deleted_at = $3, updated_at = $3, updated_by = $4, version = version + 1
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }
}
