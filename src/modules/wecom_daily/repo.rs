//! wecom_daily 域 repo 层 —— SQL 真源
//!
//! 分两组，形态一致（静态 SQL + `sqlx::query_as!` 字面量宏 ⇒ **编译期按 `.env` 的
//! DATABASE_URL 连开发库校验列名与类型**，产物进 `.sqlx/`）：
//! 1. **统计查询**（第一节 / 第二节 / 第三节 / 第四节各一至两条）；
//! 2. **配置 / 流水 CRUD**。
//!
//! ## ⚠️ 四条硬约束（改 SQL 前逐条读）
//!
//! 1. **`today` 是形参，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区（测试容器 UTC）
//!    与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，Shanghai
//!    00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「今天」的三个窗口整体
//!    偏移一天，且**不报任何错**。
//! 2. **窗口是半开 `[today, today+N)`**；「3 日内交期」**含今天**（今天到期最该被
//!    看见，`days = 3` 即 `[today, today+3)` 共 3 天）。写 `<= today + N` 会让
//!    「3 日内」变成 4 天。
//! 3. **出货查询不写 status 谓词**：流转是 `DRAFT → SUBMITTED → PICKED_UP →
//!    ARCHIVED`（`com/delivery_note/model.rs`），`picked_up_at` 只在 `PICKED_UP`
//!    迁移时被设置且此后**不清空** ⇒ 写死 `status = 'PICKED_UP'` 会**漏掉已归档的
//!    单**（而已归档的单才是真正「今天出了货」的那些）。
//! 4. **`SUM(quantity)` 必须 `COALESCE(…, 0)`**：全 NULL 时 PG 返回 NULL 而不是 0，
//!    而「全 NULL」就是「今天没有任何一条报工事件」——日报最常见的形态。
//!
//! ## 域隔离：**零他域符号引用**
//!
//! 本文件**不 import** `statistics` / `mail_digest` / `dashboard` 的任何符号。
//! 第六条（逾期）与第七条（未来）的**谓词**逐字照抄 `mail_digest::repo`，但
//! 「未交付」状态白名单是**本域自有常量** [`DELIVERY_STATUSES`] ⇒ 它与
//! `mail_digest::repo::DELIVERY_STATUSES` / `dashboard::repo::delivery::DELIVERY_STATUSES`
//! / 前端 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses` 构成**四处人工同步**关系，
//! 改一处必须同步另三处（同步义务见 `docs/api/wecom-daily.md` §4 与 §8.3）。
//!
//! ## 取数恒 ≤ 6 条语句
//!
//! 上界 = 第一节 2 条 + 第二节 1 条 + 第三节 2 条 + 第四节 1 条 = 6 条；**关掉的节
//! 一条都不查**（`service::fetch_rows` 的 `if cfg.include_*`），与工单数 / 送货单
//! 数 / 工人数**无关**。护栏见 `tests/wecom_daily/n_plus_one_guard.rs`。
//! ⚠️ **刻意不写死总数**：这类计数必然过期，错的那个数比没有更坏 —— 上界是推出来的
//! （六条语句各在哪个函数、哪个开关下会发），由 `fetch_rows` 的 doc 与源码护栏守住。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::wecom_daily::dto::WorkerBarRow;
use crate::modules::wecom_daily::model::{TWeComDailyConfigRow, TWeComDailyLogRow};

/// 「未交付」状态白名单（6 态）。取集合用于第三节两条查询的
/// `status = ANY($2::varchar[])`。
///
/// ⚠️ **刻意复制而**不** import `mail_digest::repo::DELIVERY_STATUSES` 或
/// `dashboard::repo::delivery::DELIVERY_STATUSES`**（用户 2026-10-16 定的硬要求：
/// 「在推送通知的域重写 SQL，不要复用其它域的」）。取值逐字相同，**改一处必须同步
/// 另三处**（mail_digest 本域 / dashboard 本域 / 前端 `LAYERS[].statuses`）。
pub const DELIVERY_STATUSES: [&str; 6] = [
    "PENDING",
    "PROGRAMMING",
    "IN_PROCESS",
    "OUTSOURCE",
    "INSPECTION",
    "READY_TO_SHIP",
];

/// [`DELIVERY_STATUSES`] 的 `Vec<String>` 形态（`LazyLock` 只算一次）。
///
/// 理由与 `mail_digest::repo` 同款：本域走 `sqlx::query_as!` **宏**，宏要求参数是
/// 具体可绑的 Rust 类型，`&[&str; 6]` 绑不上 `$2::varchar[]`。
fn delivery_statuses_vec() -> &'static [String] {
    static VEC: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| DELIVERY_STATUSES.iter().map(|s| s.to_string()).collect());
    &VEC
}

// ============================================================================
// 第一节：全厂报工总览
// ============================================================================

/// 第一节的四个数字（**一条语句四个条件聚合**）。
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct OverviewEvents {
    /// 报工**批次数** = `COUNT(DISTINCT batch_id)`。
    pub batch_count: i64,
    /// 送检 + 放回的**件数** = `COALESCE(SUM(quantity), 0)`。
    pub quantity: i64,
    /// 今日完工单数（`COMPLETED` 且 `batch_id IS NULL` ⇒ 工单级事件）。
    pub completed: i64,
    /// 今日返修单数（`REPAIR_STARTED`，distinct part_id）。
    pub repair: i64,
}

/// 第一节的四个条件聚合（一条语句）。
///
/// ## 报工 = **送检 + 放回**，不是「领取」
///
/// 事件集 `event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')`。
/// ⚠️ **刻意不含 `PICKED_UP`**：那是「从货架**领取**」—— 工人把件拿走的那一刻，
/// 工序**还没开始**。日报问的是「今天干了多少活」，答领取数等于把「拿起来」当成
/// 「做完了」。同一条判据在 `statistics` 域的「有效工时」维度上是**逐字同源**的
/// （`RETURNED_TO_SHELF` + `SENT_TO_INSPECTION`，理由见其 SQL 常量的 doc）。
///
/// ## ⚠️ **四条口径的理由，逐条都要读完再改**
///
/// 1. **`COUNT(DISTINCT batch_id)`（批次数）忽略 NULL** ⇒ 件级事件（`batch_id IS
///    NULL`）不计入。这正是「报工批次数」的正解：同一批次分两次放回会折叠成 1 批
///    （日报要的是「动了多少批」，不是「扫码按了多少次」）。
/// 2. **`SUM(quantity)`（送件数）**：⚠️ **全仓从未算过这个口径** ——
///    `statistics` 域没有（它算的是工时 / 领取数），`wx` 域有一个近似但事件集是
///    `PICKED_UP + RETURNED_TO_SHELF` **且排除 `SENT_TO_INSPECTION`**（那边是为了
///    算「经手件数」去重），口径不同、不能用。
/// 3. **`completed` / `repair` 走同一条语句 ⇒ 受同一条 `JOIN t_worker` 影响**
///    （见下条），这是与 `statistics::repo::count_completed` / `count_repair_parts`
///    的**刻意差异**，登记在 `docs/api/wecom-daily.md` §8.3 第 3 条。
/// 4. **`JOIN t_worker w ON w.id = e.worker_id` 是 INNER**（照 `statistics` 的
///    「有效工时」维度同款）：`worker_id` 指向一个已消失的工人时，那条事件不该进
///    任何工人维度的统计。
///    ⚠️ **连带的已知后果**：`worker_id` 在 2026-10-12 之前**从未被写入过**
///    （`shared/part_event/mod.rs` + 迁移 `20261011120000`），故该日期之前的
///    历史事件既进不了 `completed` / `repair`（INNER JOIN 落空），`quantity` /
///    `batch_count` 也是 0。**这不是 bug**，是数据事实，登记在
///    `docs/api/wecom-daily.md` §4「`worker_id` 的历史空洞」。
///
/// ## 窗口
/// 半开 `[today, today+1)`（硬约束 1 + 2）。
pub async fn overview_events(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<OverviewEvents, sqlx::Error> {
    sqlx::query_as!(
        OverviewEvents,
        r#"
        SELECT
            COUNT(DISTINCT CASE
                WHEN e.event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')
                THEN e.batch_id END)::bigint AS "batch_count!",
            COALESCE(SUM(CASE
                WHEN e.event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')
                THEN e.quantity END), 0)::bigint AS "quantity!",
            COUNT(DISTINCT CASE
                WHEN e.event_type = 'COMPLETED' AND e.batch_id IS NULL
                THEN e.part_id END)::bigint AS "completed!",
            COUNT(DISTINCT CASE
                WHEN e.event_type = 'REPAIR_STARTED'
                THEN e.part_id END)::bigint AS "repair!"
        FROM t_part_event e
        JOIN t_worker w ON w.id = e.worker_id
        WHERE e.created_at >= $1::date::timestamp
          AND e.created_at < ($1::date + INTERVAL '1 day')::timestamp
        "#,
        today,
    )
    .fetch_one(&mut *conn)
    .await
}

/// 「当前在制」工单数（**当前快照、非日窗**）。
///
/// ## 二选一：照抄 `statistics` 的那条（含 `NOT EXISTS`）
///
/// `statistics::repo::count_in_process_at` 多一个 `NOT EXISTS（件级 COMPLETED /
///
/// CANCELLED 事件）` 子查询来抵消「历史脏数据里 `t_part.status` 忘了翻」的偏差。
/// 照抄它 ⇒ 本域这个数字能与统计页**逐个对账**；不抄 ⇒ 少一次全表扫，但遇到脏数据
/// 会与统计页不一致。**选「照抄」**：日报的首要属性是可对账，而那一次子查询的代价是
/// 一个 `t_part_event` 上的索引扫描（`e.part_id` 有索引），在日报这个日频场景下可忽略。
pub async fn in_process_count(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt \
         FROM t_part p \
         WHERE p.deleted_at IS NULL \
           AND p.status NOT IN ('COMPLETED', 'CANCELLED') \
           AND NOT EXISTS ( \
             SELECT 1 FROM t_part_event e \
             WHERE e.part_id = p.id \
               AND e.batch_id IS NULL \
               AND e.event_type IN ('COMPLETED', 'CANCELLED') \
           )",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

// ============================================================================
// 第二节：今日出货
// ============================================================================

/// 第二节的两个数字（一条语句两个聚合）。
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct DeliveryNoteSummary {
    /// 今日已取货的送货单数。
    pub note_count: i64,
    /// 那些单名下的**未删批次数**之和（= 前端送货单详情页那个 `part_count`）。
    pub part_count: i64,
}

/// 今日出货：单数 + 件数（**一条语句两个聚合**）。
///
/// ## 为什么必须 `LEFT JOIN LATERAL` 而不能逐单查
///
/// ⚠️ **`part_count` 不是 `t_delivery_note` 的列**：送货单详情页那个 `part_count` 的
/// 口径是「该单名下**未软删批次**的条数」（照 `com/delivery_note/service/inner.rs`
/// 的 `PartBatchRepo::list_by_delivery_note`）⇒ 必须去 `t_part_batch` 数。
/// 逐单循环查是**功能完全正确**的改法，但出货单量随业务涨而语句数线性涨 ⇒
/// `LATERAL` 把「每单数一次」压进**同一条语句**。
///
/// ## 硬约束 3：**不写 status 谓词**
///
/// `picked_up_at` 只在 `PICKED_UP` 迁移时被设置且此后不清空，而单子随后会流转到
/// `ARCHIVED` ⇒ 写死 `status = 'PICKED_UP'` 会漏掉**已归档的单**，而那些恰恰是
/// 「今天真正出了货」的单。
pub async fn delivery_note_summary(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<DeliveryNoteSummary, sqlx::Error> {
    sqlx::query_as!(
        DeliveryNoteSummary,
        r#"
        SELECT
            COUNT(*)::bigint AS "note_count!",
            COALESCE(SUM(pc.part_count), 0)::bigint AS "part_count!"
        FROM t_delivery_note n
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS part_count
            FROM t_part_batch b
            WHERE b.delivery_note_id = n.id AND b.deleted_at IS NULL
        ) pc ON TRUE
        WHERE n.deleted_at IS NULL
          AND n.picked_up_at >= $1::date::timestamp
          AND n.picked_up_at < ($1::date + INTERVAL '1 day')::timestamp
        "#,
        today,
    )
    .fetch_one(&mut *conn)
    .await
}

// ============================================================================
// 第三节：逾期未交 / 未来 N 天交期
// ============================================================================

/// 逾期未交工单数（**工单级**）。
///
/// ⚠️ **谓词逐字照抄 `mail_digest::repo::list_overdue`**，只把投影收窄成 `COUNT`
/// （日报只要一个数，不列行）。照抄的理由：那份 SQL 是「未交付」的**既定契约**，
/// 自己另写一份等于给同一件事发明第二个定义（改一处漏一处，报出去的数字对不上却
/// 谁都说不清）。
///
/// 保留的四个要点：
/// - 工单级行单位（`t_part` 侧带 `assembly_id IS NULL`，装配件单独一支 `UNION ALL`）；
/// - `system_delivery_date < $1`（**含今天之前、不含今天** —— 今天的交期属未来节）；
/// - `NOT EXISTS（任一批次 `DELIVERED` / `COMPLETED`）` 已交量守卫，**含
///   `COMPLETED`**（force-complete 端点强推它，漏掉会让已完工的工单继续被算逾期）；
/// - 装配件那一支经子件两跳（批次挂在子件上）。
pub async fn count_overdue(conn: &mut PgConnection, today: NaiveDate) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt FROM ( \
             SELECT p.id \
             FROM t_part p \
             WHERE p.deleted_at IS NULL \
               AND p.assembly_id IS NULL \
               AND p.system_delivery_date IS NOT NULL \
               AND p.system_delivery_date < $1 \
               AND p.status = ANY($2::varchar[]) \
               AND NOT EXISTS (SELECT 1 FROM t_part_batch b \
                                WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                                  AND b.status IN ('DELIVERED', 'COMPLETED')) \
             UNION ALL \
             SELECT a.id \
             FROM t_assembly a \
             WHERE a.deleted_at IS NULL \
               AND a.system_delivery_date IS NOT NULL \
               AND a.system_delivery_date < $1 \
               AND a.status = ANY($2::varchar[]) \
               AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                               WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                                 AND b.deleted_at IS NULL \
                                 AND b.status IN ('DELIVERED', 'COMPLETED')) \
         ) u",
    )
    .bind(today)
    .bind(delivery_statuses_vec())
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

/// 未来 `days` 天内交期到期且一件没交过的工单数。
///
/// 窗口是**半开** `[today, today + days)`（硬约束 2）：`days = 3` 时含今天与今天
/// 之后 2 天，共 3 天。SQL 里用 `$1::date + $3::int`（PG 的 `date + int` 本身返回
/// `date`，不需要绕 `interval`）。
///
/// ⚠️ 谓词其余部分逐字照 `mail_digest::repo::list_upcoming`（见 [`count_overdue`]
/// 的四个要点）。
pub async fn count_upcoming(
    conn: &mut PgConnection,
    today: NaiveDate,
    days: i32,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS cnt FROM ( \
             SELECT p.id \
             FROM t_part p \
             WHERE p.deleted_at IS NULL \
               AND p.assembly_id IS NULL \
               AND p.system_delivery_date IS NOT NULL \
               AND p.system_delivery_date >= $1::date \
               AND p.system_delivery_date < $1::date + $3::int \
               AND p.status = ANY($2::varchar[]) \
               AND NOT EXISTS (SELECT 1 FROM t_part_batch b \
                                WHERE b.part_id = p.id AND b.deleted_at IS NULL \
                                  AND b.status IN ('DELIVERED', 'COMPLETED')) \
             UNION ALL \
             SELECT a.id \
             FROM t_assembly a \
             WHERE a.deleted_at IS NULL \
               AND a.system_delivery_date IS NOT NULL \
               AND a.system_delivery_date >= $1::date \
               AND a.system_delivery_date < $1::date + $3::int \
               AND a.status = ANY($2::varchar[]) \
               AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id \
                               WHERE c.assembly_id = a.id AND c.deleted_at IS NULL \
                                 AND b.deleted_at IS NULL \
                                 AND b.status IN ('DELIVERED', 'COMPLETED')) \
         ) u",
    )
    .bind(today)
    .bind(delivery_statuses_vec())
    .bind(days)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.get::<i64, _>("cnt"))
}

// ============================================================================
// 第四节：工人今日报工（条形图的数据源）
// ============================================================================

/// 第四节的工人行（**一个 `GROUP BY` 一次拉回**，一个工人一条）。
///
/// ⚠️ 刻意**不起 `T*` 名**：它是跨 `t_part_event` + `t_worker` 两表聚合出来的读侧
/// 行，不是任何一张表的全列投影（起 `T*` 名会让「这是本域 owned 表」的错误印象
/// 盖过「这是聚合出来的」）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkerRowRaw {
    pub worker_name: String,
    pub badge_code: String,
    pub batch_count: i64,
    pub quantity: i64,
}

/// 第四节的工人行（`GROUP BY e.worker_id`）。
///
/// ## 事件集与第一节逐字同源
/// `RETURNED_TO_SHELF` + `SENT_TO_INSPECTION`（理由见 [`overview_events`]）。
///
/// ## 为什么 `HAVING` 挡掉全零工人
///
/// 统计页会为零报工工人**补零行**（那一页要的是「谁今天没干活」），日报只画条形图 ——
/// 让零值行进来等于把条形图灌满空条，而日报**只**服务于「今天干了多少」这个问题。
///
/// ## `t_worker` 软删闸门
///
/// ⚠️ 这里带 `w.deleted_at IS NULL` 而第一节的 `JOIN t_worker` **不带** ⇒ 两条
/// SQL 在「工人被软删且当天有事件」时**会不一致**（第一节算进去、这里不算）。
/// 处置：如实登记在 `docs/api/wecom-daily.md` §8.3 第 4 条 + `service.rs` 的一致性
/// 校验注释，不偷偷对齐（对齐哪一侧都会让另一侧与它的源域脱节）。
pub async fn worker_rows(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<Vec<WorkerBarRow>, sqlx::Error> {
    let rows = sqlx::query_as!(
        WorkerRowRaw,
        r#"
        SELECT
            w.name        AS "worker_name!",
            w.badge_code  AS "badge_code!",
            COUNT(DISTINCT e.batch_id)::bigint AS "batch_count!",
            COALESCE(SUM(e.quantity), 0)::bigint AS "quantity!"
        FROM t_part_event e
        JOIN t_worker w ON w.id = e.worker_id AND w.deleted_at IS NULL
        WHERE e.event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')
          AND e.created_at >= $1::date::timestamp
          AND e.created_at < ($1::date + INTERVAL '1 day')::timestamp
        GROUP BY e.worker_id, w.name, w.badge_code
        HAVING COUNT(DISTINCT e.batch_id) > 0 OR COALESCE(SUM(e.quantity), 0) > 0
        "#,
        today,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| WorkerBarRow {
            worker_name: r.worker_name,
            badge_code: r.badge_code,
            batch_count: r.batch_count as i32,
            quantity: r.quantity as i32,
        })
        .collect())
}

// ============================================================================
// 配置单例 CRUD
// ============================================================================

/// 读配置单例。`None` = 表里还没有未软删行（`GET /config` 据此返回默认值 +
/// `version=0` + `id="0"`，见 `service::get_config`）。
///
/// `LIMIT 1` 而非不加：唯一索引 `uk_t_wecom_daily_config_alive` 保证至多一行，
/// 但加上它能让「唯一索引被人手工 drop 掉」这种脏库的异常表现为「取第一行」而不是
/// 「取到多行直接报错」—— 配置是单例，取第一行继续跑比整个页面 500 好。
pub async fn get_config(
    conn: &mut PgConnection,
) -> Result<Option<TWeComDailyConfigRow>, sqlx::Error> {
    sqlx::query_as!(
        TWeComDailyConfigRow,
        r#"
        SELECT id, version, enabled, frequency, send_hour, send_minute, weekday,
               day_of_month, webhook_id, upcoming_days, worker_metric, worker_top_n,
               max_chunks, include_overview, include_delivery, include_orders,
               include_workers, subject_template, created_at, created_by, updated_at,
               updated_by, deleted_at
        FROM t_wecom_daily_config
        WHERE deleted_at IS NULL
        ORDER BY id ASC
        LIMIT 1
        "#
    )
    .fetch_optional(&mut *conn)
    .await
}

/// 首次初始化：INSERT 配置单例行（`version = 0`）。
///
/// `Ok(false)` 表示**并发下已有别的请求插进来了**（撞唯一索引
/// `uk_t_wecom_daily_config_alive`，SQLSTATE 23505），此时调用方应把 `23505`
/// **转成 OCC 冲突**而不是当成数据库故障（符合「40901 只表达 OCC」）。
pub async fn insert_config(
    conn: &mut PgConnection,
    id: i64,
    cfg: &crate::modules::wecom_daily::dto::NormalizedConfig,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        INSERT INTO t_wecom_daily_config (
            id, version, enabled, frequency, send_hour, send_minute, weekday,
            day_of_month, webhook_id, upcoming_days, worker_metric, worker_top_n,
            max_chunks, include_overview, include_delivery, include_orders,
            include_workers, subject_template, created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $18, $19)
        "#,
    )
    .bind(id)
    .bind(cfg.enabled)
    .bind(cfg.frequency.as_str())
    .bind(cfg.send_hour)
    .bind(cfg.send_minute)
    .bind(cfg.weekday)
    .bind(cfg.day_of_month)
    .bind(cfg.webhook_id)
    .bind(cfg.upcoming_days)
    .bind(cfg.worker_metric.as_str())
    .bind(cfg.worker_top_n)
    .bind(cfg.max_chunks)
    .bind(cfg.include_overview)
    .bind(cfg.include_delivery)
    .bind(cfg.include_orders)
    .bind(cfg.include_workers)
    .bind(cfg.subject_template.as_deref())
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;

    Ok(res.rows_affected() == 1)
}

/// 全量覆写（OCC）。
///
/// `WHERE id=$1 AND version=$2` 是本仓 OCC 的标准形态；`rows_affected == 0` 表示
/// 「版本对不上」⇒ 调用方返 `40901 VERSION_CONFLICT`。
///
/// ⚠️ 用 autocommit（不开事务），与其它写端点一致；真正值得注意的是它**没有**的事：
/// 全量覆写**不**级联写任何其它表（不碰 `t_wecom_daily_log`）⇒ 不存在「必须同事务」
/// 的第二个写。
pub async fn update_config(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    cfg: &crate::modules::wecom_daily::dto::NormalizedConfig,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_wecom_daily_config
        SET enabled          = $3,
            frequency        = $4,
            send_hour        = $5,
            send_minute      = $6,
            weekday          = $7,
            day_of_month     = $8,
            webhook_id       = $9,
            upcoming_days    = $10,
            worker_metric    = $11,
            worker_top_n     = $12,
            max_chunks       = $13,
            include_overview = $14,
            include_delivery = $15,
            include_orders   = $16,
            include_workers  = $17,
            subject_template = $18,
            version          = version + 1,
            updated_at       = $19,
            updated_by       = $20
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(id)
    .bind(expected_version)
    .bind(cfg.enabled)
    .bind(cfg.frequency.as_str())
    .bind(cfg.send_hour)
    .bind(cfg.send_minute)
    .bind(cfg.weekday)
    .bind(cfg.day_of_month)
    .bind(cfg.webhook_id)
    .bind(cfg.upcoming_days)
    .bind(cfg.worker_metric.as_str())
    .bind(cfg.worker_top_n)
    .bind(cfg.max_chunks)
    .bind(cfg.include_overview)
    .bind(cfg.include_delivery)
    .bind(cfg.include_orders)
    .bind(cfg.include_workers)
    .bind(cfg.subject_template.as_deref())
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;

    Ok(res.rows_affected() == 1)
}

// ============================================================================
// 推送流水
// ============================================================================

/// 落一行推送流水（**成败都落**，见 `t_wecom_daily_log` 的表注释）。
///
/// ⚠️ `error_message` 由调用方**截断到 500 字符**再传（列是 `varchar(500)`）：企业
/// 微信的失败原因可能带整段 errmsg，很容易超长；让 PG 报错会把「推送失败」这件事
/// 本身变成「落库也失败」，反而丢掉诊断信息。
pub async fn insert_log(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    log: &NewDailyLog,
) -> Result<TWeComDailyLogRow, sqlx::Error> {
    let id = snowflake.next_id();
    sqlx::query_as!(
        TWeComDailyLogRow,
        r#"
        INSERT INTO t_wecom_daily_log (
            id, config_version, trigger_source, webhook_name, webhook_masked,
            report_date, markdown_bytes, chunk_count, chunk_sent,
            overview_batch_count, overview_quantity, note_count, note_part_count,
            overdue_count, upcoming_count, worker_row_total, status, error_message,
            sent_at, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $19)
        RETURNING id, config_version, trigger_source, webhook_name, webhook_masked,
                  report_date, markdown_bytes, chunk_count, chunk_sent,
                  overview_batch_count, overview_quantity, note_count, note_part_count,
                  overdue_count, upcoming_count, worker_row_total, status, error_message,
                  sent_at, created_at
        "#,
        id,
        log.config_version,
        log.trigger_source,
        log.webhook_name,
        log.webhook_masked,
        log.report_date,
        log.markdown_bytes,
        log.chunk_count,
        log.chunk_sent,
        log.overview_batch_count,
        log.overview_quantity,
        log.note_count,
        log.note_part_count,
        log.overdue_count,
        log.upcoming_count,
        log.worker_row_total,
        log.status,
        log.error_message,
        log.sent_at,
    )
    .fetch_one(&mut *conn)
    .await
}

/// 推送流水分页读（`sent_at DESC`，同刻按 `id DESC` 兜底排序确定性）。
pub async fn list_logs(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<TWeComDailyLogRow>, i64), sqlx::Error> {
    let items = sqlx::query_as!(
        TWeComDailyLogRow,
        r#"
        SELECT id, config_version, trigger_source, webhook_name, webhook_masked,
               report_date, markdown_bytes, chunk_count, chunk_sent,
               overview_batch_count, overview_quantity, note_count, note_part_count,
               overdue_count, upcoming_count, worker_row_total, status, error_message,
               sent_at, created_at
        FROM t_wecom_daily_log
        ORDER BY sent_at DESC, id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(&mut *conn)
    .await?;

    let total: i64 = sqlx::query("SELECT COUNT(*) FROM t_wecom_daily_log")
        .fetch_one(&mut *conn)
        .await?
        .get(0);

    Ok((items, total))
}

/// 写一行推送流水的入参（`id` / `created_at` 由 [`insert_log`] 生成）。
#[derive(Debug, Clone)]
pub struct NewDailyLog {
    pub config_version: i32,
    /// `SCHEDULED` / `MANUAL`。
    pub trigger_source: &'static str,
    /// webhook 名称快照。
    pub webhook_name: String,
    /// webhook 地址掩码快照。
    pub webhook_masked: String,
    /// 统计基准日。
    pub report_date: NaiveDate,
    /// 全部片的字节数之和。
    pub markdown_bytes: i32,
    /// 渲染出的片数（`0` = 准备阶段就失败，合法值）。
    pub chunk_count: i16,
    /// 实发片数。
    pub chunk_sent: i16,
    pub overview_batch_count: i32,
    pub overview_quantity: i32,
    pub note_count: i32,
    pub note_part_count: i32,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub worker_row_total: i32,
    /// `SUCCESS` / `FAILED`。
    pub status: &'static str,
    pub error_message: Option<String>,
    /// ⚠️ `report_date` 必须由它 `.date()` 派生（同一个 `now_naive()` 调用取值）——
    /// 「统计日恒等于发送时刻的日期」是前端**不建「统计日」列**的前提。
    pub sent_at: NaiveDateTime,
}
