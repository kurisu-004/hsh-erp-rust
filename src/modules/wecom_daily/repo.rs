//! wecom_daily 域 repo 层 —— SQL 真源
//!
//! 分两组，形态一致（静态 SQL + `sqlx::query_as!` 字面量宏 ⇒ **编译期按 `.env` 的
//! DATABASE_URL 连开发库校验列名与类型**，产物进 `.sqlx/`）：
//! 1. **统计查询**（第一节 / 第二节 / 第三节 / 第四节各一至两条）；
//! 2. **配置 / 流水 CRUD**。
//!
//! ## ⚠️ 四条硬约束（改 SQL 前逐条读）
//!
//! 1. **统计窗口是形参 `(from_ts, to_ts)`，SQL 里绝不写 `CURRENT_DATE`**：DB 会话
//!    时区（测试容器 UTC）与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个
//!    时钟**，Shanghai 00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「本期」的
//!    窗口整体偏移一天，且**不报任何错**。窗口两端由 `dto::report_window` 从配置
//!    算出（**前一天 @ 推送时刻 ~ 当天 @ 推送时刻**）⇒ 跨天，而 `date + INTERVAL`
//!    那种「按日历日切」的做法在这里根本不成立（推送时刻可能落在 20:05，窗口上界
//!    就不在当天零点）。
//! 2. **窗口是半开 `[from_ts, to_ts)`**；第三节的「N 日内交期」按**日历日**半开
//!    `[today, today+N)`（**含今天**：今天到期最该被看见，`days = 3` 即共 3 天），
//!    写 `<= today + N` 会让「3 日内」变成 4 天。⚠️ 第一节 / 第二节 / 第四节走**时间
//!    戳**窗口（硬约束 1），第三节走**日期**窗口（交期列本身就是 `date`）——两套窗口
//!    不要互串。
//! 3. **出货查询不写 status 谓词**：流转是 `DRAFT → SUBMITTED → PICKED_UP →
//!    ARCHIVED`（`com/delivery_note/model.rs`），`picked_up_at` 只在 `PICKED_UP`
//!    迁移时被设置且此后**不清空** ⇒ 写死 `status = 'PICKED_UP'` 会**漏掉已归档的
//!    单**（而已归档的单才是真正「出了货」的那些）。
//! 4. **`SUM(quantity)` 必须 `COALESCE(…, 0)`**：全 NULL 时 PG 返回 NULL 而不是 0，
//!    而「全 NULL」就是「本期没有任何一条报工事件」——日报最常见的形态。
//!
//! ## 域隔离：**零他域符号引用**
//!
//! 本文件**不 import** `statistics` / `mail_digest` / `dashboard` 的任何符号。
//! 第六条（逾期）与第七条（未来）的**谓词**逐字照抄 `mail_digest::repo`，而
//! 「未交付」状态白名单取自共享常量
//! [`crate::shared::delivery_status::DELIVERY_STATUSES`] —— `shared::` 不属于任何
//! 业务域，域隔离判定只认「域根段 + 他域路径」的前缀失配，所以引用它**不构成**跨域
//! 依赖。而白名单**必须**共用一份：三域各存一份副本时，「加一个状态」漏改一处不会
//! 编译失败，只会让日报 / 邮件 / 大屏三处的「未交付」判定互相矛盾且无单测变红。
//!
//! ## 取数恒 **O(1)**：语句数与返回行数无关（护栏见
//! `tests/wecom_daily/n_plus_one_guard.rs`）
//!
//! 不变量是「**语句数不随工单数 / 送货单数 / 工人数增长**」：每节都把整组聚合压进
//! **一条**语句（第一节 1 条、第二节 1 条、第三节各 1 条、第四节 1 个 `GROUP BY`），
//! 关掉的节一条都不查。逐节循环查是**功能完全正确**的改法（数字守恒、渲染逐字一致、
//! 集成测试全绿），只有数据量涨上去时语句数才悄悄线性涨 —— 而日报恰好是「一天一次、
//! 量很大」的生产形态。⚠️ **刻意不钉死具体条数**：这类计数必然过期，而护栏守的
//! 「循环体内不许出现取数调用」才是那条不变量的真正来源。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::wecom_daily::dto::WorkerBarRow;
use crate::modules::wecom_daily::model::{TWeComDailyConfigRow, TWeComDailyLogRow};
use crate::shared::delivery_status::delivery_statuses_vec;

// ============================================================================
// 第一节：全厂报工总览
// ============================================================================

/// 第一节的五个数字（**一条语句五个条件聚合**）。
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct OverviewEvents {
    /// 报工**批次数** = `COUNT(DISTINCT batch_id)`。
    pub batch_count: i64,
    /// 送检 + 放回的**件数** = `COALESCE(SUM(quantity), 0)`。
    pub quantity: i64,
    /// 完工单数（`COMPLETED` 且 `batch_id IS NULL` ⇒ 工单级事件）。
    ///
    /// ⚠️ **不进正文**（第一节正文只有报工 / 送检 / 在制三组数字），但仍**保留在出参**
    /// 里：`/preview` 的结构化数字是管理员核对口径的入口，删掉它等于让「今天完工了几单」
    /// 在系统里彻底无处可查。
    pub completed: i64,
    /// 工人主动送检的**批次数** = `COUNT(DISTINCT batch_id)`（事件集只有
    /// `SENT_TO_INSPECTION`）。
    pub sent: i64,
    /// 当前在制工单数（**当前快照、不受窗口约束**）。
    ///
    /// ⚠️ **它只看派生列 `t_part.status = 'IN_PROCESS'`**，且这与统计页
    /// `statistics::repo::count_in_process_at` 的口径**刻意不同**：统计页那条额外带
    /// 一道 `NOT EXISTS（件级 `COMPLETED` / `CANCELLED` 事件）`守卫，用来抵消
    /// 「历史脏数据里 `t_part.status` 忘了翻」的偏差；本域不加那道守卫 ⇒ 两页可能
    /// 对不上，差的就是那些「派生列忘了翻」的脏行。
    ///
    /// 为什么选派生列：日报要的是「**此刻**手上还有多少活」，那是**当前快照**而不是
    /// 本期统计量；判定就该与大屏等其它页面用同一个列 —— 同一个列才不会在三个页面
    /// 给出三个数。代价是 `t_part.status` 是 min-progress **派生缓存**、存在滞后
    /// 窗口（刚完工的件在派生列翻过来之前仍算在制）。
    pub wip: i64,
}

/// 第一节的五个数字（一条语句）。
///
/// ## 报工 = **送检 + 放回**，不是「领取」
///
/// 事件集 `event_type IN ('RETURNED_TO_SHELF', 'SENT_TO_INSPECTION')`。
/// ⚠️ **刻意不含 `PICKED_UP`**：那是「从货架**领取**」—— 工人把件拿走的那一刻，
/// 工序**还没开始**。日报问的是「本期干了多少活」，答领取数等于把「拿起来」当成
/// 「做完了」。同一条判据在 `statistics` 域的「有效工时」维度上是**逐字同源**的
/// （`RETURNED_TO_SHELF` + `SENT_TO_INSPECTION`，理由见其 SQL 常量的 doc）。
///
/// ## ⚠️ **五条口径的理由，逐条都要读完再改**
///
/// 1. **`COUNT(DISTINCT batch_id)`（批次数）忽略 NULL** ⇒ 件级事件（`batch_id IS
///    NULL`）不计入。这正是「报工批次数」的正解：同一批次分两次放回会折叠成 1 批
///    （日报要的是「动了多少批」，不是「扫码按了多少次」）。
/// 2. **`SUM(quantity)`（送件数）**：⚠️ **全仓从未算过这个口径** ——
///    `statistics` 域没有（它算的是工时 / 领取数），`wx` 域有一个近似但事件集是
///    `PICKED_UP + RETURNED_TO_SHELF` **且排除 `SENT_TO_INSPECTION`**（那边是为了
///    算「经手件数」去重），口径不同、不能用。
/// 3. **`sent` 与 `batch_count` 的差别只在事件集**：一个只看 `SENT_TO_INSPECTION`
///    （工人**主动送检**，即「这一批做完了、交给品检了」），另一个是送检 + 放回
///    （放回货架是「做完了但先归架」，不进品检队列）。⚠️ **同样按 `COUNT(DISTINCT
///    batch_id)` 去重**：同一批次送检两次只算 1 批，否则「送检 3 批」在只送过 2 批的
///    现场会变成一个追责用的假数字。
/// 4. **`wip` 只看派生列 `t_part.status`**，见 [`OverviewEvents::wip`] 的口径说明
///    （它带滞后窗口，是这五个数里**唯一**与其它四条不同源的一个）。
/// 5. **`JOIN t_worker w ON w.id = e.worker_id` 是 INNER**（照 `statistics` 的
///    「有效工时」维度同款）：`worker_id` 指向一个已消失的工人时，那条事件不该进
///    任何工人维度的统计。
///    ⚠️ **连带的已知后果**：`worker_id` 在 2026-10-12 之前**从未被写入过**
///    （`shared/part_event/mod.rs` + 迁移 `20261011120000`），故该日期之前的
///    历史事件既进不了 `completed`（INNER JOIN 落空），`quantity` /
///    `batch_count` / `sent` 也是 0。**这不是 bug**，是数据事实，登记在
///    `docs/api/wecom-daily.md` §4「`worker_id` 的历史空洞」。
///    ⚠️ `wip` **不受**这条影响（它根本不 join 工人表）。
///
/// ## 窗口
/// 半开 `[from_ts, to_ts)`（硬约束 1 + 2），两端由 `dto::report_window` 算出。
pub async fn overview_events(
    conn: &mut PgConnection,
    from_ts: NaiveDateTime,
    to_ts: NaiveDateTime,
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
                WHEN e.event_type = 'SENT_TO_INSPECTION'
                THEN e.batch_id END)::bigint AS "sent!",
            (SELECT COUNT(*) FROM t_part p
              WHERE p.deleted_at IS NULL AND p.status = 'IN_PROCESS')::bigint AS "wip!"
        FROM t_part_event e
        JOIN t_worker w ON w.id = e.worker_id
        WHERE e.created_at >= $1
          AND e.created_at < $2
        "#,
        from_ts,
        to_ts,
    )
    .fetch_one(&mut *conn)
    .await
}

// ============================================================================
// 第二节：本期出货
// ============================================================================

/// 第二节的**唯一**数字（本期的出货单数）。
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct DeliveryNoteSummary {
    /// 本期出货的**工单数** = `COUNT(DISTINCT b.part_id)`。
    ///
    /// ⚠️ **按 `part_id` 去重**：同一张送货单里同一工单挂了多个批次（一次送货装走
    /// 同一工单的两个批次），日报问的是「今天出了几**单**货」，按批次计会把一张单
    /// 报成两单，管理员按这个数字去核单会对不上。
    pub part_count: i64,
}

/// 本期出货的工单数（**一条语句**）。
///
/// ## 为什么是 JOIN + `COUNT(DISTINCT)` 而不是逐单数
///
/// ⚠️ **工单不是送货单的列**：要数的对象是「本期被取走的送货单名下有哪几个工单」，
/// 而工单挂在 `t_part_batch.delivery_note_id` 上 ⇒ 必须经 `t_part_batch` 才能拿到
/// `part_id`。逐单循环查是**功能完全正确**的改法，但出货单量随业务涨而语句数线性
/// 涨 ⇒ 一条 JOIN + `COUNT(DISTINCT b.part_id)` 把整件事压成**恒 O(1)** 条语句。
/// ⚠️ `COUNT(DISTINCT …)` 同时兜住了「同一工单在同一张单里多批次」与「同一工单
/// 被两张单先后取走」两种重复。
///
/// ## 软删闸门
///
/// `t_part_batch.deleted_at IS NULL`：被软删的批次不算出货（与送货单详情页
/// `PartBatchRepo::list_by_delivery_note` 的口径同源）。
///
/// ## 硬约束 3：**不写 status 谓词**
///
/// `picked_up_at` 只在 `PICKED_UP` 迁移时被设置且此后不清空，而单子随后会流转到
/// `ARCHIVED` ⇒ 写死 `status = 'PICKED_UP'` 会漏掉**已归档的单**，而那些恰恰是
/// 「真正出了货」的单。
pub async fn delivery_note_summary(
    conn: &mut PgConnection,
    from_ts: NaiveDateTime,
    to_ts: NaiveDateTime,
) -> Result<DeliveryNoteSummary, sqlx::Error> {
    sqlx::query_as!(
        DeliveryNoteSummary,
        r#"
        SELECT COUNT(DISTINCT b.part_id)::bigint AS "part_count!"
        FROM t_delivery_note n
        JOIN t_part_batch b
          ON b.delivery_note_id = n.id AND b.deleted_at IS NULL
        WHERE n.deleted_at IS NULL
          AND n.picked_up_at >= $1
          AND n.picked_up_at < $2
        "#,
        from_ts,
        to_ts,
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
// 第四节：工人本期报工（条形图的数据源）
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
/// 让零值行进来等于把条形图灌满空条，而日报**只**服务于「本期干了多少」这个问题。
///
/// ## `t_worker` 软删闸门
///
/// ⚠️ 这里带 `w.deleted_at IS NULL` 而第一节的 `JOIN t_worker` **不带** ⇒ 两条
/// SQL 在「工人被软删且本期有事件」时**会不一致**（第一节算进去、这里不算）。
/// 处置：如实登记在 `docs/api/wecom-daily.md` §8.3 第 4 条 + `service.rs` 的一致性
/// 校验注释，不偷偷对齐（对齐哪一侧都会让另一侧与它的源域脱节）。
pub async fn worker_rows(
    conn: &mut PgConnection,
    from_ts: NaiveDateTime,
    to_ts: NaiveDateTime,
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
          AND e.created_at >= $1
          AND e.created_at < $2
        GROUP BY e.worker_id, w.name, w.badge_code
        HAVING COUNT(DISTINCT e.batch_id) > 0 OR COALESCE(SUM(e.quantity), 0) > 0
        "#,
        from_ts,
        to_ts,
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
               order_list_limit, max_chunks, include_overview, include_delivery,
               include_orders, include_workers, include_drawings, subject_template,
               created_at, created_by, updated_at, updated_by, deleted_at
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
            order_list_limit, max_chunks, include_overview, include_delivery, include_orders,
            include_workers, include_drawings, subject_template, created_at, created_by,
            updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $20, $21)
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
    .bind(cfg.order_list_limit)
    .bind(cfg.max_chunks)
    .bind(cfg.include_overview)
    .bind(cfg.include_delivery)
    .bind(cfg.include_orders)
    .bind(cfg.include_workers)
    .bind(cfg.include_drawings)
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
            order_list_limit = $13,
            max_chunks       = $14,
            include_overview = $15,
            include_delivery = $16,
            include_orders   = $17,
            include_workers  = $18,
            include_drawings = $19,
            subject_template = $20,
            version          = version + 1,
            updated_at       = $21,
            updated_by       = $22
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
    .bind(cfg.order_list_limit)
    .bind(cfg.max_chunks)
    .bind(cfg.include_overview)
    .bind(cfg.include_delivery)
    .bind(cfg.include_orders)
    .bind(cfg.include_workers)
    .bind(cfg.include_drawings)
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
/// ⚠️ `config_version` 是**形参**而不是 [`NewDailyLog`] 的字段：它是「落库这一刻」的
/// 快照，必须由调用方**现读**（[`get_config`] → `.version`，表空 ⇒ `0`）。本仓 repo 的
/// 约定是「一函数一条 SQL」，所以「读版本 + INSERT」由 service 层在**同一条连接**上合并成
/// 一次借还（`service::insert_log` 供投递路径用；准备阶段失败那条自己读、自己降级成 `0`
/// 后直调本函数，理由见 `service::log_prepare_failure` 的 doc）—— 投递期间因此不必持有连接。
///
/// ⚠️ `error_message` 由调用方**截断到 500 字符**再传（列是 `varchar(500)`）：企业
/// 微信的失败原因可能带整段 errmsg，很容易超长；让 PG 报错会把「推送失败」这件事
/// 本身变成「落库也失败」，反而丢掉诊断信息。
pub async fn insert_log(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    config_version: i32,
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
        config_version,
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

/// 写一行推送流水的入参（`id` / `created_at` 由 [`insert_log`] 生成，`config_version`
/// 是它的独立形参）。
///
/// ⚠️ **`config_version` 不在入参里**（是 [`insert_log`] 的独立形参）：它是「落库这一刻」
/// 的版本快照，恒由调用方现读（见 `service::insert_log` —— 两个调用点都在那儿现读，
/// 没有第三条路可以写进一个与库不符的值）。
#[derive(Debug, Clone)]
pub struct NewDailyLog {
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
