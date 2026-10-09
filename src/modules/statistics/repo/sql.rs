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
//!
//! ## 2026-10-12：跳序取件（tab4）清空
//! 删 3 个 free fn（`pickup_skip_summary` / `pickup_skip_detail` /
//! `pickup_skip_detail_count`）+ 2 个 row struct（`PickupSkipSummaryRow` /
//! `PickupSkipDetailRow`）⇒ 现存 13 个 free fn + 2 个 row struct。数据源表
//! `t_pickup_skip_event` 由同批 migration 删除（写入点早已随 Python v1 退役消失）。
//! 同时把 `worker_detail_events` 里 `'RETURNED'` 修正为 `'RETURNED_TO_SHELF'`
//! （Rust 侧实际写的事件类型，详见该函数 doc）。
//!
//! ## 2026-10-12：有效工时维度（3 个 free fn + 2 个 row struct）
//! 「工人完成一道工序的预计工时之和」三条聚合共用同一份骨架常量，见
//! [`effective_minutes`] 模块 doc。三条各抄一遍必然漂移，而漂移的表现是
//! **数字静默错**（不报错、不抛异常），故共用。现存 **16** 个 free fn +
//! **4** 个 row struct。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgExecutor, Row};

/// 有效工时三条聚合共用的 SQL 骨架片段。
///
/// 有效工时是「事件过滤 + 按工单锚链取工序预计工时」两层嵌套的复合口径，
/// 三条消费点（按工人 / 单工人总额 / 按工序）必须逐字同源，故拆成常量 +
/// [`build`] 拼装，而不是三条各写一整串字面量。
///
/// ⚠️ **代价：三条聚合都没有编译期列名校验**。它们走运行时 `sqlx::query()` 而非
/// `query!` 宏（`format!` 拼装无法进宏），`.sqlx/` 里**没有**它们的条目 ⇒ 列名 /
/// 表名写错、某列被改名、`GROUP BY` 漏列都只在**运行时**报 PG 错误（且 `GROUP BY`
/// 漏列这类只在特定数据下才暴露的情形，连运行时都不一定立刻报错）。改这三处的
/// SQL 时要么同步改本模块常量，要么自己确认一次 `cargo test` / dev 库实跑。
mod effective_minutes {
    use sqlx::AssertSqlSafe;

    /// 事件谓词 + 「有工人」闸门 + 日期半开区间。
    ///
    /// **事件集合是「工人完成了一道工序」的唯一口径**：
    /// - **计入**：`RETURNED_TO_SHELF`（放回顺链推进）、`SENT_TO_INSPECTION`
    ///   （工人主动送检 = 做完当前工序）。
    /// - **不计入**：`PICKED_UP` / `TAKEN_FROM_POOL` / `MOVED` —— 那三个是
    ///   **开始**；一进一出都计则同一道工序被计两次。
    ///   `INSPECTED` —— 品检员动作不是工人作业（写点在 `prod::batch` 的批次
    ///   扫码与状态流转两处）。`DELIVERED` —— 无工序语义。
    ///
    /// 三个消费点的**参数序号统一为 `$1` = date_from / `$2` = date_to**
    /// （两条单工人查询把 `worker_id` 放 `$3`），故本片段可逐字共用。
    ///
    /// `worker_id IS NOT NULL` 就 `JOIN t_worker w` 而言是冗余的，保留它是为了
    /// 让部分索引 `ix_part_event_worker_type_created (worker_id, event_type,
    /// created_at) WHERE worker_id IS NOT NULL` 可被直接选中。
    pub const WHERE_SQL: &str = "WHERE e.event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION') \
           AND e.worker_id IS NOT NULL \
           AND e.created_at >= $1::timestamp \
           AND e.created_at < ($2::date + INTERVAL '1 day')::timestamp";

    /// 公共 FROM + JOIN + LATERAL。
    ///
    /// ⚠️ **三处口径，逐条都是硬约束**：
    ///
    /// 1. **必须包在 `LEFT JOIN LATERAL (…) cs ON TRUE` 里，不能把
    ///    `t_process_chain_step` 直接摆进 FROM 的 JOIN 列表**：该表**没有**
    ///    `(chain_id, process_id)` 唯一约束（只有 `uq_chain_step_chain_order
    ///    (chain_id, sort_order) WHERE deleted_at IS NULL`），写侧
    ///    `prod::process_chain::service::upsert_chain` 也只校验链内 `sort_order`
    ///    互不重复、**不校验 `process_id` 重复** ⇒ 同名工序在一条链里出现多次
    ///    是后端照收的合法形态。直接 JOIN 会让这类行按命中条数**翻倍计入工时**
    ///    且**不报错**。`ORDER BY sort_order ASC, id ASC LIMIT 1` 取唯一一行。
    ///    （与 `shared::batch::chain` 把 `hit_count > 1` 判成 `chain_state =
    ///    'NONE'` 是同一事实的两种处置：那边「下一道是谁」有歧义必须拒答，
    ///    这边「这一步的预计工时」取链上第一次出现的那一步即可。）
    /// 2. **锚链取 `p.process_chain_id` 单来源**，**不写**
    ///    `COALESCE(p.process_chain_id, s.chain_id)`：那套 COALESCE 是
    ///    `shared::batch::chain` 的**运行时链定位**纪律（要兼容批次 step 指针
    ///    指向的旧链）。统计侧不需要兼容老数据，且子查询的 `chain_id` 本就取自
    ///    `p.process_chain_id`、无从回退，写 COALESCE 只会让「无链工单」被误判成
    ///    「有链」。
    /// 3. **`JOIN t_worker w` 是 INNER**（同 `worker_pickup_rows`）：本维度只
    ///    统计真有工人身份的作业事件，`worker_id` 指向已消失的工人时该行不该进
    ///    任何统计。**不要**改成 LEFT。
    pub const FROM_SQL: &str = "FROM t_part_event e \
         JOIN t_worker w ON w.id = e.worker_id \
         JOIN t_part p ON p.id = e.part_id AND p.deleted_at IS NULL \
         LEFT JOIN LATERAL ( \
             SELECT s.estimated_minutes \
               FROM t_process_chain_step s \
              WHERE s.chain_id = p.process_chain_id \
                AND s.process_id = e.process_id \
                AND s.deleted_at IS NULL \
              ORDER BY s.sort_order ASC, s.id ASC \
              LIMIT 1 \
         ) cs ON TRUE";

    /// `LEFT JOIN t_process pr`：按工序聚合的**显示名**来源。
    ///
    /// ⚠️ **ON 上刻意不带 `pr.deleted_at IS NULL`**（与仓内**筛候选**的那批带这道
    /// 闸门的 SQL 相反，见下面理由 3）：被聚合的行是 `t_part_event` —— 一张**没有
    /// `deleted_at` 列**的不可变审计流水，而统计窗口又是**已过去的闭区间**。事后软删
    /// 一道工序是对**当前工序字典**的行政操作，不该追溯抹掉历史报表里的名字，否则
    /// 一次软删就让整段历史行集体退化成 `(已删除)`。
    ///
    /// 三条理由，缺一条都可能被下一个人「顺手对齐」回去：
    ///
    /// 1. **同一条派生在 `part::service::phase1::events` 已是这个口径** —— 那里对
    ///    完全相同的 `t_part_event.process_id → t_process.name` 显式不过滤
    ///    `deleted_at`（「事件行是当时的快照事实」）。同一份 diff 里对同一派生给两种
    ///    相反口径，是漂移的起点。
    /// 2. **分组键是 `e.process_id`（事件侧），不是字典侧** ⇒ 加不加这道闸门都不改
    ///    分组结果，只改显示名。`t_process.id` 是主键，LEFT JOIN 至多命中一行，
    ///    也不会因此放大行数。
    /// 3. **仓内「LEFT JOIN 取显示名」一律不过滤**（`prod::inspection::repo` /
    ///    `outsource::repo::sql` / `shared::shelf::pool_priority` 同款）；带
    ///    `AND pr.deleted_at IS NULL` 的是**INNER JOIN 筛候选**的场景
    ///    （`prod::programming` / `prod::work_type` / `prod::shelf_process`）。
    ///
    /// ⇒ 落到 `process_name` 为 NULL 的只剩「`process_id` 本身是 NULL」与
    /// 「工序行物理缺失」两种，service 统一退 `(已删除)`。
    pub const PROCESS_JOIN_SQL: &str = "LEFT JOIN t_process pr ON pr.id = e.process_id";

    /// `COALESCE(SUM(cs.estimated_minutes), 0)` 的取数表达式。
    ///
    /// LATERAL 无行（`process_id IS NULL` / `process_chain_id IS NULL` /
    /// 链内找不到该工序的 step）⇒ `cs.estimated_minutes` 为 SQL NULL ⇒ 该事件
    /// **计 0 分钟但不被丢弃**：它照常计入 `worked_process_count` /
    /// `worked_batch_count`，只是工时为 0。⚠️ 写 `SUM(...)` 不带 `COALESCE`
    /// 会让「该工人全部事件都在链外」这一整组产出 NULL 而不是 0。
    pub const EFFECTIVE_MINUTES_SQL: &str = "COALESCE(SUM(cs.estimated_minutes), 0)";

    /// 把 [`FROM_SQL`] / [`WHERE_SQL`] 与各消费点自己的 SELECT / JOIN / 条件
    /// / GROUP BY 拼成一条完整语句。
    ///
    /// 用 `format!` 而非 `QueryBuilder`：骨架里的 `{}` 只有 `select_list` /
    /// `process_join` / `extra_predicate` / `group_by` 四处，全是**本模块私有
    /// 常量或调用点字面量**，不含任何外部输入 ⇒ 不存在注入面，故返回值可直接
    /// 进 [`AssertSqlSafe`]（口径同 `shared::batch::chain::resolve_chain_position`
    /// 与 `outsource::board::repo::sendable_dedup_sql`）。
    ///
    /// ⚠️ **调用点纪律**：传给本函数的四个片段只能是编译期常量或本文件里的
    /// 字面量。**任何**来自函数入参 / `format!` 外部的值都不许拼进 SQL —— 用户
    /// 输入一律走 `bind`。
    pub(super) fn build(
        select_list: &str,
        process_join: &str,
        extra_predicate: &str,
        group_by: &str,
    ) -> AssertSqlSafe<String> {
        AssertSqlSafe(format!(
            "{select_list} {from} {process_join} {where} {extra_predicate} {group_by}",
            from = FROM_SQL,
            where = WHERE_SQL,
        ))
    }
}

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

/// 期内每个工人的**有效工时**聚合：
/// `(worker_id, effective_minutes, worked_process_count, worked_batch_count)`。
///
/// 口径细节（事件集合 / LATERAL 去重 / 锚链来源 / 链外工序计 0 / INNER JOIN
/// `t_worker` 的理由）逐条写在 [`effective_minutes`] 的三个骨架常量 doc 上，
/// 本函数只负责「按工人分组」这一层。
///
/// ⚠️ **数据覆盖受工序链配置率限制，不是 bug**：工时取自
/// `t_process_chain_step.estimated_minutes`，未配链的工单、以及链内没有该
/// `process_id` 的工序，一律计 **0 分钟**。工序链配得越少，这个数字越接近 0
/// —— 是口径的必然结果，读数时先看链配置率。
pub async fn worker_effective_minutes_rows<'e, E: PgExecutor<'e>>(
    executor: E,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<WorkerEffectiveMinutesRow>, sqlx::Error> {
    let sql = effective_minutes::build(
        &format!(
            "SELECT e.worker_id AS worker_id, \
             {}::bigint AS effective_minutes, \
             COUNT(DISTINCT e.process_id)::bigint AS worked_process_count, \
             COUNT(DISTINCT e.batch_id)::bigint   AS worked_batch_count",
            effective_minutes::EFFECTIVE_MINUTES_SQL,
        ),
        "",
        "",
        "GROUP BY e.worker_id",
    );
    let rows = sqlx::query(sql)
        .bind(date_from)
        .bind(date_to)
        .fetch_all(executor)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(WorkerEffectiveMinutesRow {
                worker_id: r.get::<i64, _>("worker_id"),
                effective_minutes: r.get::<i64, _>("effective_minutes"),
                worked_process_count: r.get::<i64, _>("worked_process_count"),
                worked_batch_count: r.get::<i64, _>("worked_batch_count"),
            })
        })
        .collect()
}

// ============================================================
// tab3：单工人详情
// ============================================================

/// 单工人 `(pickup_count, pickup_quantity, return_count)`（CASE 聚合）。
///
/// ⚠️ 2026-10-12：放回口径的字面量由 `'RETURNED'` 修正为 `'RETURNED_TO_SHELF'`。
/// Rust 侧写侧落的事件类型是 `RETURNED_TO_SHELF`（`prod::scan::service::worker_scan`
/// 写事件处），`'RETURNED'` 是 Python v1 时代的字面量、Rust 从不写 ⇒ 修正前
/// `return_cnt` 与下方 `event_type IN (...)` 的过滤**恒为 0**（不是「没发生过
/// 放回」，是「字面量对不上」）。已写入的历史放回事件也全部记在
/// `RETURNED_TO_SHELF` 名下，修正后一并被统计到。
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
             COALESCE(SUM(CASE WHEN event_type = 'RETURNED_TO_SHELF' THEN 1 ELSE 0 END), 0)::bigint AS return_cnt \
         FROM t_part_event \
         WHERE worker_id = $1 \
           AND event_type IN ('PICKED_UP', 'RETURNED_TO_SHELF') \
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

/// 单工人有效工时总额（详情页顶部那一个数字）。
///
/// 与 [`worker_effective_minutes_rows`] 的口径**逐字同源**（共用
/// [`effective_minutes`] 的骨架常量），区别只在多一个 `worker_id = $3` 闸门与
/// 不分组 —— 两页对不上数只会是 service 装配错位，不会是口径漂移。
pub async fn worker_effective_minutes(
    conn: &mut PgConnection,
    worker_id: i64,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<i64, sqlx::Error> {
    let sql = effective_minutes::build(
        &format!(
            "SELECT {}::bigint AS effective_minutes",
            effective_minutes::EFFECTIVE_MINUTES_SQL,
        ),
        "",
        "AND e.worker_id = $3",
        "",
    );
    let row = sqlx::query(sql)
        .bind(date_from)
        .bind(date_to)
        .bind(worker_id)
        .fetch_one(conn)
        .await?;
    Ok(row.get::<i64, _>("effective_minutes"))
}

/// 单工人按工序拆分的有效工时（详情页的「按工序」表）。
///
/// 口径与 [`worker_effective_minutes`] 同源，额外 `LEFT JOIN t_process pr` 取
/// 显示名。⚠️ **GROUP BY 含 `pr.name` 而不只是 `e.process_id`**：`t_process.id`
/// 是主键、一个 `process_id` 只对应一行 `t_process`，故加不加 `pr.name` 都不
/// 改变分组结果；加上是为了让 `process_name` 进投影而不必依赖 PG 对主键的函数
/// 依赖推断。
///
/// ⚠️ **`process_id IS NULL` 的事件在这里汇成一个无工序名的组**（对
/// [`worker_effective_minutes`] 的总额贡献恒为 0）：该组不会让总额虚增，但
/// 前端展示时应当把它读成「这批作业没有工序归属、工时 0」，而不是「一道名为
/// `(已删除)` 的工序」。
pub async fn worker_process_breakdown(
    conn: &mut PgConnection,
    worker_id: i64,
    date_from: NaiveDate,
    date_to: NaiveDate,
) -> Result<Vec<WorkerProcessBreakdownRow>, sqlx::Error> {
    let sql = effective_minutes::build(
        &format!(
            "SELECT e.process_id AS process_id, \
             pr.name AS process_name, \
             COUNT(*)::bigint AS step_count, \
             {}::bigint AS effective_minutes",
            effective_minutes::EFFECTIVE_MINUTES_SQL,
        ),
        effective_minutes::PROCESS_JOIN_SQL,
        "AND e.worker_id = $3",
        "GROUP BY e.process_id, pr.name \
         ORDER BY effective_minutes DESC, step_count DESC, e.process_id ASC",
    );
    let rows = sqlx::query(sql)
        .bind(date_from)
        .bind(date_to)
        .bind(worker_id)
        .fetch_all(conn)
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(WorkerProcessBreakdownRow {
                process_id: r.try_get::<Option<i64>, _>("process_id").ok().flatten(),
                process_name: r
                    .try_get::<Option<String>, _>("process_name")
                    .ok()
                    .flatten(),
                step_count: r.get::<i64, _>("step_count"),
                effective_minutes: r.get::<i64, _>("effective_minutes"),
            })
        })
        .collect()
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

/// 一行 = 一个工人在区间内的有效工时汇总（见
/// [`worker_effective_minutes_rows`]）。
#[derive(Debug, Clone)]
pub struct WorkerEffectiveMinutesRow {
    pub worker_id: i64,
    pub effective_minutes: i64,
    pub worked_process_count: i64,
    pub worked_batch_count: i64,
}

/// 一行 = 一个工人在区间内的一道工序（见 [`worker_process_breakdown`]）。
///
/// `process_id` / `process_name` 可空：事件本身没记工序时（写侧只有 `prod::scan` 的
/// 5 个构造点落 `process_id`，其余 `t_part_event` 写点一律 `NULL`）该行代表「无工序归属的
/// 作业」，工时贡献为 0。
#[derive(Debug, Clone)]
pub struct WorkerProcessBreakdownRow {
    pub process_id: Option<i64>,
    pub process_name: Option<String>,
    pub step_count: i64,
    pub effective_minutes: i64,
}
