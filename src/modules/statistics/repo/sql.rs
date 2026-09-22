//! statistics 域数据访问 — SQL 真源（2026-09-23 PR8 重构）
//!
//! 对应 Python myERP/repository/statistics.py。所有方法返回聚合行，
//! service 层做零填充 / 拼装。
//!
//! ## 全走运行时 `sqlx::query_as` / `sqlx::query_scalar`
//! 与 part_file 域一致：复杂聚合 SQL 改动频繁，不依赖离线 query! 宏。
//! 类型列名通过 SELECT 显式 AS 起别名，服务端手工映射到结构体。
//!
//! ## PR8 拆分（2026-09-23）
//! 原 `repo.rs` 单文件 ZST struct + 16 个固有静态方法 → 本文件 16 个 free fn +
//! 4 个原始行 dataclass-like 结构。trait impl 统一收在 `super::mod.rs`（Rust
//! coherence 规则：同 crate 内同一 trait 对同一类型至多一个 impl 块）。
//! SQL 字符串与原 `repo.rs` byte-identical（PR8 硬约束 零 SQL 文本变化）。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgExecutor, Row};

// ============================================================
// tab1：基础计数
// ============================================================

/// 期内新建工单数：t_part.created_at ∈ [date_from, date_to+1)。
pub async fn count_created(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt \
         FROM t_part \
         WHERE deleted_at IS NULL \
           AND created_at >= $1::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

/// 期内完成工单数：event_type=COMPLETED 且 batch_id IS NULL，distinct part_id。
pub async fn count_completed(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(DISTINCT part_id)::bigint AS cnt \
         FROM t_part_event \
         WHERE event_type = 'COMPLETED' \
           AND batch_id IS NULL \
           AND created_at >= $1::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

/// 期末在制：date_to 当天 24:00 前已创建、且截至查询时刻**未** COMPLETED/CANCELLED
/// 的工单数。
///
/// 2026-09-15 review 修：原 NOT EXISTS 子查询带 `e.created_at < date_to+1` 过滤，
/// 导致 `date_to` 之后才 COMPLETED/CANCELLED 的工单在 `date_to` 统计里仍被算
/// 在制——期末口径偏差。改为「任何时刻存在 COMPLETED/CANCELLED 即不算在制」，
/// 并加 `p.status NOT IN (...)` 双保险（防止历史脏数据缺事件）。
pub async fn count_in_process_at(
    conn: &mut PgConnection,
    date_to: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt \
         FROM t_part p \
         WHERE p.deleted_at IS NULL \
           AND p.created_at < ($1::date + INTERVAL '1 day')::timestamp \
           AND p.status NOT IN ('COMPLETED', 'CANCELLED') \
           AND NOT EXISTS ( \
             SELECT 1 FROM t_part_event e \
             WHERE e.part_id = p.id \
               AND e.batch_id IS NULL \
               AND e.event_type IN ('COMPLETED', 'CANCELLED') \
           )",
    )
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

/// 期内交付集合：count / sum(total_price) / orange / red（单条 SQL 条件聚合）。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `actual_delivery_date` 列；
/// 「实际交付日期」改由 t_part_event 的 DELIVERED 事件派生 —— LATERAL 子查询
/// 取该 part 任一活跃批次（`deleted_at IS NULL`）的最近一条 DELIVERED 事件
/// 时间戳。无事件 → 视为未交付。分类口径不变：orange = 晚于 planned 且
/// 不晚于 system；red = 晚于 system。
pub async fn delivered_stats(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<(i64, Decimal, i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "WITH delivered AS ( \
             SELECT p.id, p.total_price, p.planned_delivery_date, p.system_delivery_date, \
                    (SELECT MAX(e.created_at)::date \
                     FROM t_part_event e \
                     JOIN t_part_batch b ON b.id = e.batch_id \
                     WHERE b.part_id = p.id \
                       AND b.deleted_at IS NULL \
                       AND e.event_type = 'DELIVERED') AS actual_date \
             FROM t_part p \
             WHERE p.deleted_at IS NULL \
         ) \
         SELECT \
             COUNT(*)::bigint AS cnt, \
             COALESCE(SUM(total_price), 0)::numeric AS sum_total, \
             COALESCE(SUM(CASE \
                 WHEN actual_date > planned_delivery_date \
                      AND (system_delivery_date IS NULL OR actual_date <= system_delivery_date) \
                 THEN 1 ELSE 0 END), 0)::bigint AS orange, \
             COALESCE(SUM(CASE \
                 WHEN system_delivery_date IS NOT NULL AND actual_date > system_delivery_date \
                 THEN 1 ELSE 0 END), 0)::bigint AS red \
         FROM delivered \
         WHERE actual_date >= $1 \
           AND actual_date <= $2",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    let cnt: i64 = row.get("cnt");
    let sum: Decimal = row
        .try_get::<Decimal, _>("sum_total")
        .unwrap_or(Decimal::ZERO);
    let orange: i64 = row.get("orange");
    let red: i64 = row.get("red");
    Ok((cnt, sum, orange, red))
}

/// 期内每日新建工单数 → `Vec<(NaiveDate, i64)>`（缺日期不出现）。
pub async fn daily_created_counts(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DATE(created_at) AS d, COUNT(*)::bigint AS cnt \
         FROM t_part \
         WHERE deleted_at IS NULL \
           AND created_at >= $1::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp \
         GROUP BY DATE(created_at) \
         ORDER BY DATE(created_at)",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.get::<NaiveDate, _>("d"), r.get::<i64, _>("cnt"))))
        .collect()
}

/// 期内每日 COMPLETED 事件数（按事件聚合，与 daily_created 对齐）。
pub async fn daily_completed_counts(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DATE(created_at) AS d, COUNT(*)::bigint AS cnt \
         FROM t_part_event \
         WHERE event_type = 'COMPLETED' \
           AND batch_id IS NULL \
           AND created_at >= $1::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp \
         GROUP BY DATE(created_at) \
         ORDER BY DATE(created_at)",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.get::<NaiveDate, _>("d"), r.get::<i64, _>("cnt"))))
        .collect()
}

/// 期内返修工单数（event_type=REPAIR_STARTED，distinct part_id）。
pub async fn count_repair_parts(
    conn: &mut PgConnection,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(DISTINCT part_id)::bigint AS cnt \
         FROM t_part_event \
         WHERE event_type = 'REPAIR_STARTED' \
           AND created_at >= $1::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

/// 当前超期未交付工单数（planned < today, 无 DELIVERED 事件, 非终态）。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `actual_delivery_date` 列；
/// 「未交付」判定改 NOT EXISTS DELIVERED 事件（事件存在 ⇒ 已交付；无事件
/// ⇒ 未交付）。多批次场景下任一活跃批次有 DELIVERED 事件即视为已交付。
pub async fn count_overdue_undelivered(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt \
         FROM t_part p \
         WHERE p.deleted_at IS NULL \
           AND p.planned_delivery_date < $1 \
           AND p.status NOT IN ('COMPLETED', 'CANCELLED') \
           AND NOT EXISTS ( \
             SELECT 1 FROM t_part_event e \
             JOIN t_part_batch b ON b.id = e.batch_id \
             WHERE b.part_id = p.id \
               AND b.deleted_at IS NULL \
               AND e.event_type = 'DELIVERED' \
           )",
    )
    .bind(today)
    .fetch_one(conn)
    .await?;
    Ok(row.get("cnt"))
}

/// 当前各 status 工单数 → `(status_value, count)`，包含 CANCELLED 便于前端看分布。
pub async fn status_distribution(
    conn: &mut PgConnection,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT status AS s, COUNT(*)::bigint AS cnt \
         FROM t_part \
         WHERE deleted_at IS NULL \
         GROUP BY status \
         ORDER BY status",
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.get::<String, _>("s"), r.get::<i64, _>("cnt"))))
        .collect()
}

// ============================================================
// tab2：工人 pickup 聚合（原始行）
// ============================================================

/// 期内每个工人的 PICKED_UP 聚合：
/// `(worker_id, work_type_id, pickup_count, pickup_quantity, participated_part_count)`。
pub async fn worker_pickup_rows<'e, E: PgExecutor<'e>>(
    executor: E,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<WorkerPickupRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT e.worker_id        AS worker_id, \
                w.work_type_id     AS work_type_id, \
                COUNT(e.id)::bigint           AS pickup_count, \
                COALESCE(SUM(e.quantity), 0)::bigint AS pickup_quantity, \
                COUNT(DISTINCT e.part_id)::bigint   AS participated_part_count \
         FROM t_part_event e \
         JOIN t_worker w ON w.id = e.worker_id \
         WHERE e.event_type = 'PICKED_UP' \
           AND e.worker_id IS NOT NULL \
           AND e.created_at >= $1::timestamp \
           AND e.created_at < ($2::date + INTERVAL '1 day')::timestamp \
         GROUP BY e.worker_id, w.work_type_id",
    )
    .bind(date_from)
    .bind(date_to)
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(WorkerPickupRow {
                worker_id: r.get::<i64, _>("worker_id"),
                work_type_id: r.try_get::<i64, _>("work_type_id").ok(),
                pickup_count: r.get::<i64, _>("pickup_count"),
                pickup_quantity: r.get::<i64, _>("pickup_quantity"),
                participated_part_count: r.get::<i64, _>("participated_part_count"),
            })
        })
        .collect()
}

// ============================================================
// tab3：单工人详情
// ============================================================

/// 单工人 `(pickup_count, pickup_quantity, return_count)`（CASE 聚合）。
pub async fn worker_detail_events(
    conn: &mut PgConnection,
    worker_id: i64,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<(i64, i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "SELECT \
             COALESCE(SUM(CASE WHEN event_type = 'PICKED_UP' THEN 1 ELSE 0 END), 0)::bigint AS pickup_cnt, \
             COALESCE(SUM(CASE WHEN event_type = 'PICKED_UP' THEN quantity ELSE NULL END), 0)::bigint AS pickup_qty, \
             COALESCE(SUM(CASE WHEN event_type = 'RETURNED' THEN 1 ELSE 0 END), 0)::bigint AS return_cnt \
         FROM t_part_event \
         WHERE worker_id = $1 \
           AND event_type IN ('PICKED_UP', 'RETURNED') \
           AND created_at >= $2::timestamp \
           AND created_at < ($3::date + INTERVAL '1 day')::timestamp",
    )
    .bind(worker_id)
    .bind(date_from)
    .bind(date_to)
    .fetch_one(conn)
    .await?;
    Ok((
        row.get::<i64, _>("pickup_cnt"),
        row.get::<i64, _>("pickup_qty"),
        row.get::<i64, _>("return_cnt"),
    ))
}

/// 单工人每日 PICKED_UP 次数 → `Vec<(NaiveDate, i64)>`。
pub async fn worker_daily_pickups(
    conn: &mut PgConnection,
    worker_id: i64,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<(NaiveDate, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DATE(created_at) AS d, COUNT(*)::bigint AS cnt \
         FROM t_part_event \
         WHERE worker_id = $1 \
           AND event_type = 'PICKED_UP' \
           AND created_at >= $2::timestamp \
           AND created_at < ($3::date + INTERVAL '1 day')::timestamp \
         GROUP BY DATE(created_at) \
         ORDER BY DATE(created_at)",
    )
    .bind(worker_id)
    .bind(date_from)
    .bind(date_to)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.get::<NaiveDate, _>("d"), r.get::<i64, _>("cnt"))))
        .collect()
}

/// 单工人期内参与工单一览（含 last_pickup_at / 该工人对该工单领取次数）。
/// 子查询聚合 per-part pickup_count + last_pickup_at，再连 `t_part` 取业务字段。
pub async fn worker_parts(
    conn: &mut PgConnection,
    worker_id: i64,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<WorkerPartRow>, sqlx::Error> {
    let rows = sqlx::query(
        "WITH per_part AS ( \
            SELECT part_id, \
                   COUNT(*)::bigint AS per_part_pickup_count, \
                   MAX(created_at)  AS last_pickup_at \
            FROM t_part_event \
            WHERE worker_id = $1 \
              AND event_type = 'PICKED_UP' \
              AND created_at >= $2::timestamp \
              AND created_at < ($3::date + INTERVAL '1 day')::timestamp \
            GROUP BY part_id \
         ) \
         SELECT p.id         AS part_id, \
                p.serial_no  AS serial_no, \
                p.name       AS name, \
                p.drawing_no AS drawing_no, \
                p.status     AS status, \
                pp.per_part_pickup_count AS pickup_count, \
                pp.last_pickup_at        AS last_pickup_at \
         FROM per_part pp \
         JOIN t_part p ON p.id = pp.part_id \
         WHERE p.deleted_at IS NULL \
         ORDER BY pp.last_pickup_at DESC, p.id DESC",
    )
    .bind(worker_id)
    .bind(date_from)
    .bind(date_to)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(WorkerPartRow {
                part_id: r.get::<i64, _>("part_id"),
                serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
                name: r.try_get::<String, _>("name").unwrap_or_default(),
                drawing_no: r.try_get::<String, _>("drawing_no").unwrap_or_default(),
                status: r.try_get::<String, _>("status").unwrap_or_default(),
                pickup_count: r.get::<i64, _>("pickup_count"),
                last_pickup_at: r.get::<NaiveDateTime, _>("last_pickup_at"),
            })
        })
        .collect()
}

// ============================================================
// tab4：跳序取件
// ============================================================

/// 按工人聚合跳序次数 + 最近跳序时间。LEFT JOIN 兜底软删工人 → `(已删除)`。
pub async fn pickup_skip_summary(
    conn: &mut PgConnection,
) -> Result<Vec<PickupSkipSummaryRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT p.worker_id      AS worker_id, \
                COALESCE(w.name, '(已删除)') AS worker_name, \
                COALESCE(w.badge_code, '')  AS badge_code, \
                wt.name          AS work_type_name, \
                COUNT(p.id)::bigint         AS skip_count, \
                MAX(p.created_at) AS last_skip_at \
         FROM t_pickup_skip_event p \
         LEFT JOIN t_worker w ON w.id = p.worker_id \
         LEFT JOIN t_work_type wt ON wt.id = p.work_type_id \
         GROUP BY p.worker_id, w.name, w.badge_code, wt.name \
         ORDER BY COUNT(p.id) DESC, MAX(p.created_at) DESC",
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(PickupSkipSummaryRow {
                worker_id: r.get::<i64, _>("worker_id"),
                worker_name: r.get::<String, _>("worker_name"),
                badge_code: r.get::<String, _>("badge_code"),
                work_type_name: r
                    .try_get::<Option<String>, _>("work_type_name")
                    .ok()
                    .flatten(),
                skip_count: r.get::<i64, _>("skip_count"),
                last_skip_at: r
                    .try_get::<Option<NaiveDateTime>, _>("last_skip_at")
                    .ok()
                    .flatten(),
            })
        })
        .collect()
}

/// 单工人跳序事件明细分页（按 created_at desc, id desc）。
pub async fn pickup_skip_detail(
    conn: &mut PgConnection,
    worker_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PickupSkipDetailRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT p.id AS id, \
                p.part_id   AS part_id, \
                p.part_serial_no AS serial_no, \
                COALESCE(pa.name, '(已删除)') AS part_name, \
                p.batch_no  AS batch_no, \
                p.quantity  AS quantity, \
                p.part_planned_delivery_date  AS part_planned_delivery_date, \
                p.skipped_earliest_date       AS skipped_earliest_date, \
                p.created_at AS created_at \
         FROM t_pickup_skip_event p \
         LEFT JOIN t_part pa ON pa.id = p.part_id \
         WHERE p.worker_id = $1 \
         ORDER BY p.created_at DESC, p.id DESC \
         LIMIT $2 OFFSET $3",
    )
    .bind(worker_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(PickupSkipDetailRow {
                id: r.get::<i64, _>("id"),
                part_id: r.get::<i64, _>("part_id"),
                serial_no: r.try_get::<Option<String>, _>("serial_no").ok().flatten(),
                part_name: r.get::<String, _>("part_name"),
                batch_no: r.try_get::<i32, _>("batch_no").unwrap_or(0),
                quantity: r.try_get::<i32, _>("quantity").unwrap_or(0),
                part_planned_delivery_date: r
                    .try_get::<Option<NaiveDate>, _>("part_planned_delivery_date")
                    .ok()
                    .flatten(),
                skipped_earliest_date: r
                    .try_get::<Option<NaiveDate>, _>("skipped_earliest_date")
                    .ok()
                    .flatten(),
                created_at: r.get::<NaiveDateTime, _>("created_at"),
            })
        })
        .collect()
}

/// 单工人跳序事件总数。
pub async fn pickup_skip_detail_count(
    conn: &mut PgConnection,
    worker_id: i64,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt \
         FROM t_pickup_skip_event \
         WHERE worker_id = $1",
    )
    .bind(worker_id)
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

// ============================================================
// 原始行 dataclass-like 结构（service 层消费）
// ============================================================

#[derive(Debug, Clone)]
pub struct WorkerPickupRow {
    pub worker_id: i64,
    pub work_type_id: Option<i64>,
    pub pickup_count: i64,
    pub pickup_quantity: i64,
    pub participated_part_count: i64,
}

#[derive(Debug, Clone)]
pub struct WorkerPartRow {
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub pickup_count: i64,
    pub last_pickup_at: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct PickupSkipSummaryRow {
    pub worker_id: i64,
    pub worker_name: String,
    pub badge_code: String,
    pub work_type_name: Option<String>,
    pub skip_count: i64,
    pub last_skip_at: Option<NaiveDateTime>,
}

#[derive(Debug, Clone)]
pub struct PickupSkipDetailRow {
    pub id: i64,
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub part_name: String,
    pub batch_no: i32,
    pub quantity: i32,
    pub part_planned_delivery_date: Option<NaiveDate>,
    pub skipped_earliest_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
}
