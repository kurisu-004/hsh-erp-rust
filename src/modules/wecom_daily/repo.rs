//! wecom_daily 域 repo 层 —— SQL 真源
//!
//! 分两组，形态一致（静态 SQL + `sqlx::query_as!` 字面量宏 ⇒ **编译期按 `.env` 的
//! DATABASE_URL 连开发库校验列名与类型**，产物进 `.sqlx/`）：
//! 1. **统计查询**（第一节 / 第二节各一至两条，第三节是**清单**链路的 7 条，第四节一条
//!    `GROUP BY`）；
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
//! 第三节逾期 / 未来两条清单的**谓词**逐字照抄 `dashboard::repo::delivery` 的
//! `overdue` / `upcoming` 两个桶（那两条与 `mail_digest::repo` 的同名查询本来就
//! 同源，三处逐字一致），而状态白名单取自共享常量
//! [`crate::shared::delivery_status::RED_TIER_STATUSES`] —— `shared::` 不属于任何
//! 业务域，域隔离判定只认「域根段 + 他域路径」的前缀失配，所以引用它**不构成**跨域
//! 依赖。
//!
//! ## ⚠️ 状态白名单为什么是**品检前 4 态**而不是大屏那份 6 态
//!
//! 日报这一节答的是「**品检还没开始的活**里哪些要催」—— `INSPECTION`（已进品检）
//! 与 `READY_TO_SHIP`（品检完待发货）各有自己的下一动作，混进来会让「该催谁」这个
//! 唯一的问题得到一个答非所问的清单。故本域取 `RED_TIER_STATUSES`（`PENDING` /
//! `PROGRAMMING` / `IN_PROCESS` / `OUTSOURCE`），而大屏交期面板取
//! `DELIVERY_STATUSES`（6 态）——**同一套谓词、不同的状态集**是有意分叉，不是漂移。
//! 改白名单只改共享常量一处（`shared::delivery_status.rs`）。
//!
//! ⚠️ 白名单**必须**共用一份：三域各存一份副本时，「加一个状态」漏改一处不会
//! 编译失败，只会让日报 / 邮件 / 大屏三处的「未交付」判定互相矛盾且无单测变红。
//!
//! ## 取数恒 **O(1)**：语句数与返回行数无关（护栏见
//! `tests/wecom_daily/n_plus_one_guard.rs`）
//!
//! 不变量是「**语句数不随工单数 / 子件数 / 送货单数 / 工人数增长**」：每节都把整组
//! 聚合压进**一条**语句（第一节 1 条、第二节 1 条、第四节 1 个 `GROUP BY`），
//! 第三节的**清单**链路接了 7 条（逾期 / 未来两条主查询 + 装配件子件展开 + 活跃
//! 批次 + 三个 holder 名批量解析，见 `service::collect_order_groups` 的表），关掉的节
//! 一条都不查。逐行查是**功能
//! 完全正确**的改法（渲染逐字一致、集成测试全绿），只有数据量涨上去时语句数才悄悄
//! 线性涨 —— 而日报恰好是「一天一次、量很大」的生产形态。⚠️ **刻意不钉死具体条数**：
//! 这类计数必然过期，而护栏守的「循环体内不许出现取数调用」才是那条不变量的真正
//! 来源。
//!
//! ⚠️ 图纸直链那一环**不在本文件**：它要读 `t_part_file` 与 `t_file_preview`、还要跨
//! 网络轮询 CI —— 那三样知识（SQL / COS / python）全部属于 `files` 域，由它的批量
//! 入口 `files::part_file::preview::service::PreviewService::ensure_ready_for_parts`
//! 承担。本域只负责「第三节清单涉及哪些 owner id」这一个入参（见
//! `service::collect_order_groups`），从而完全不碰 `t_part_file` 的表结构。
//!
//! ⚠️ 第三节**刻意不给清单主查询加 `LIMIT`**：`order_list_limit` 的截断在 Rust 侧
//! 做，而组标题的「N 条」必须是**未截断的匹配总数**（`COUNT(*) OVER ()`）；SQL 侧加
//! `LIMIT` 会让某个日期组的条数与 `total` 一起失守、组标题开始说假话。代价是「逾期
//! 几千单」的现场要拉回几千行 —— 渲染仍然只出 `order_list_limit` 行。

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::wecom_daily::dto::WorkerBarRow;
use crate::modules::wecom_daily::model::{TWeComDailyConfigRow, TWeComDailyLogRow};
use crate::shared::batch::status_machine::part_status_progress;
// 2026-10-23：第三节的状态白名单从「未交付 6 态」改成「**品检前 4 态**」（红档），
// 理由与「大屏为什么仍用 6 态」见文件头「域隔离」一节。
use crate::shared::delivery_status::red_tier_statuses_vec;

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
// 第三节：逾期未交 / 未来 N 天交期（**清单行**，不再是两个计数）
// ============================================================================

/// 装配件行的来源字面量（与两条清单 SQL 里写死的 `'ASSEMBLY'::text` 对齐）。
const ROW_TYPE_ASSEMBLY: &str = "ASSEMBLY";

/// 第三节清单的一行（**工单级**：散件工单一条 + 装配件一条）。
///
/// ⚠️ **行单位是工单级而不是件级**：与 `dashboard::repo::delivery` 的 `overdue` /
/// `upcoming` 两个桶逐字一致（装配件算 1 条、其子件不重复计入）。子件仍然会出现，
/// 但它们是**渲染时挂在装配件行下面的子行**（[`OrderRow::is_child`]），不是清单的
/// 行单位 —— 否则一个 30 子件的装配件会把「逾期 12 单」撑成 41 单。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderListRow {
    /// `'PART'`（散件工单）/ `'ASSEMBLY'`（装配件本身），由 SQL 的 `'…'::text`
    /// 字面量带出（判断入口见 [`OrderListRow::is_assembly`]）。
    pub row_type: String,
    /// `t_part.id` 或 `t_assembly.id`。
    pub id: i64,
    /// `varchar(15)`，**可空**（未派发序列号）。
    ///
    /// ⚠️ 可空就是可空：渲染层在它是 `None` 时**留空**，不回退图号、不显示「无」
    /// ——「无」是系统自己编的字，出现在现场会让人以为序列号那一栏坏了。
    pub serial_no: Option<String>,
    /// `varchar(200) NOT NULL`，清单里作为链接文字（`[名称](图纸直链)`）。
    pub name: String,
    /// 装配件的语义是**套数**、散件是**件数**（与 dashboard 的行单位约定同款）。
    pub quantity: i32,
    /// 系统交期；两条谓词都带 `IS NOT NULL`，故这里是**非空** `date`（宏里用
    /// `AS "system_delivery_date!"` 覆盖掉 nullable 推断，省掉一层 `Option` 拆包）。
    ///
    /// ⚠️ 它同时是**分组的键**：逾期组按 `< today` 归组、日期组按它归到
    /// `2026-10-15` / `2026-10-16` 那一组（组标题就是它的 `%Y-%m-%d`）。
    pub system_delivery_date: NaiveDate,
    /// `COUNT(*) OVER ()`：该次查询的**未截断匹配总数**，在同一批返回行上恒相同。
    ///
    /// ⚠️ 私有字段 + [`OrderList::from_rows`] 归并的原因：它**不是**某一行的属性，
    /// 挂在行上只是因为 `COUNT(*) OVER ()` 必须出现在 select 列表里。
    total: i64,
}

impl OrderListRow {
    /// 是否装配件行（决定要不要把它的子件挂进来）。
    pub fn is_assembly(&self) -> bool {
        self.row_type == ROW_TYPE_ASSEMBLY
    }
}

/// 一次清单查询的返回：命中行 + **未截断**的匹配总数。
#[derive(Debug, Clone, Default)]
pub struct OrderList {
    /// 按 `system_delivery_date ASC, id ASC` 排好的命中行。
    pub rows: Vec<OrderListRow>,
    /// 匹配总数（**不受** `order_list_limit` 影响；组标题的「N 条」用它）。
    pub total: i64,
}

impl OrderList {
    /// 从一批行里归并出 `total`。
    ///
    /// ⚠️ **零命中时没有任何行**（窗口函数无从求值）⇒ `total` 退化成 `0`，而不是
    /// 报错或写成一个占位负数；这是 PG 窗口函数的固有形态，`dashboard::repo::delivery`
    /// 的 `read_bucket` 是同款处置。
    fn from_rows(rows: Vec<OrderListRow>) -> Self {
        let total = rows.first().map(|r| r.total).unwrap_or(0);
        Self { rows, total }
    }
}

/// 逾期未交清单（`system_delivery_date < today`，**无上界**）。
///
/// ## ⚠️ 谓词的四个要点，逐条照抄 `dashboard::repo::delivery::SQL_ORDERS_OVERDUE`
///
/// 1. **行单位工单级**：`t_part` 侧带 `assembly_id IS NULL`（排除装配件子件），
///    装配件单独一支 `UNION ALL`。自己另写一套行单位等于让「日报逾期 N 条」与「大屏
///    逾期 N 条」在同一个库上给出两个数，而两边的人都不知道该信哪个。
/// 2. **窗口是开在 `today` 左侧的无界区间**：今天到期的属未来组（那是**最该被看见**
///    的一批，混进「逾期」等于把还没到期的东西说成过期）。
/// 3. **已交量守卫 `NOT EXISTS（任一批次 `DELIVERED` / `COMPLETED`）`**：用户口径是
///    「部分已交的不显示」——只要交过一部分（哪怕 1 件 / 100 件），整张工单就不再是
///    「未交」。守卫**含 `COMPLETED`**（force-complete 端点强推它，漏掉会让已完工的
///    工单继续被算逾期）。
/// 4. **装配件那一支经子件两跳**（批次挂在子件上），所以它的 `NOT EXISTS` 是
///    `t_part c JOIN t_part_batch b ON b.part_id = c.id WHERE c.assembly_id = a.id`。
///
/// 唯一**有意**的差别是状态白名单：本条取**品检前 4 态**（见文件头「域隔离」），
/// 大屏那两条取未交付 6 态。
pub async fn list_overdue(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<OrderList, sqlx::Error> {
    let rows = sqlx::query_as!(
        OrderListRow,
        r#"
        SELECT
            u.row_type             AS "row_type!",
            u.id                   AS "id!",
            u.serial_no            AS "serial_no?",
            u.name                 AS "name!",
            u.quantity             AS "quantity!",
            u.system_delivery_date AS "system_delivery_date!",
            COUNT(*) OVER ()       AS "total!"
        FROM (
            SELECT p.id, p.serial_no, p.name, p.quantity, p.system_delivery_date,
                   'PART'::text AS row_type
            FROM t_part p
            WHERE p.deleted_at IS NULL
              AND p.assembly_id IS NULL
              AND p.system_delivery_date IS NOT NULL
              AND p.system_delivery_date < $1::date
              AND p.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part_batch b
                               WHERE b.part_id = p.id AND b.deleted_at IS NULL
                                 AND b.status IN ('DELIVERED', 'COMPLETED'))
            UNION ALL
            SELECT a.id, a.serial_no, a.name, a.quantity, a.system_delivery_date,
                   'ASSEMBLY'::text
            FROM t_assembly a
            WHERE a.deleted_at IS NULL
              AND a.system_delivery_date IS NOT NULL
              AND a.system_delivery_date < $1::date
              AND a.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id
                              WHERE c.assembly_id = a.id AND c.deleted_at IS NULL
                                AND b.deleted_at IS NULL
                                AND b.status IN ('DELIVERED', 'COMPLETED'))
        ) u
        ORDER BY u.system_delivery_date ASC, u.id ASC
        "#,
        today,
        red_tier_statuses_vec(),
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(OrderList::from_rows(rows))
}

/// 未来 `days` 天内交期到期且一件没交过的工单清单。
///
/// ## ⚠️ 窗口必须是**半开日历日** `[today, today + days)`（硬约束 2）
///
/// **含今天**（今天到期最该被看见，`days = 3` 即共 3 天）；写成 `<= today + N` 会让
/// 「3 日内」悄悄变成 4 天，而这种偏差**不报任何错**，只是每天多列一批根本不该在
/// 「3 日内」里的工单。SQL 里用 `$1::date + $3::int`（PG 的 `date + int` 本身返回
/// `date`，不必绕 `interval`）。
///
/// ⚠️ 谓词其余部分与 [`list_overdue`] **逐字相同**（只把 `< $1` 换成半开的
/// `>= $1 AND < $1 + $3`）——两条谓词同源是「逾期组与日期组不重不漏」的前提：
/// `sdd == today` 只进日期组、`sdd == today + days` 两组都不进。
///
/// ⚠️ 这里**没有** `IS NOT NULL`：`sdd >= $1` 对 NULL 求值为 NULL（不成立）已经把它
/// 排除掉了，与 `dashboard` 的 `upcoming` 桶逐字同款。
pub async fn list_upcoming(
    conn: &mut PgConnection,
    today: NaiveDate,
    days: i32,
) -> Result<OrderList, sqlx::Error> {
    let rows = sqlx::query_as!(
        OrderListRow,
        r#"
        SELECT
            u.row_type             AS "row_type!",
            u.id                   AS "id!",
            u.serial_no            AS "serial_no?",
            u.name                 AS "name!",
            u.quantity             AS "quantity!",
            u.system_delivery_date AS "system_delivery_date!",
            COUNT(*) OVER ()       AS "total!"
        FROM (
            SELECT p.id, p.serial_no, p.name, p.quantity, p.system_delivery_date,
                   'PART'::text AS row_type
            FROM t_part p
            WHERE p.deleted_at IS NULL
              AND p.assembly_id IS NULL
              AND p.system_delivery_date >= $1::date
              AND p.system_delivery_date < $1::date + $3::int
              AND p.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part_batch b
                               WHERE b.part_id = p.id AND b.deleted_at IS NULL
                                 AND b.status IN ('DELIVERED', 'COMPLETED'))
            UNION ALL
            SELECT a.id, a.serial_no, a.name, a.quantity, a.system_delivery_date,
                   'ASSEMBLY'::text
            FROM t_assembly a
            WHERE a.deleted_at IS NULL
              AND a.system_delivery_date >= $1::date
              AND a.system_delivery_date < $1::date + $3::int
              AND a.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id
                              WHERE c.assembly_id = a.id AND c.deleted_at IS NULL
                                AND b.deleted_at IS NULL
                                AND b.status IN ('DELIVERED', 'COMPLETED'))
        ) u
        ORDER BY u.system_delivery_date ASC, u.id ASC
        "#,
        today,
        red_tier_statuses_vec(),
        days,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(OrderList::from_rows(rows))
}

/// 装配件的**子件行**（`t_part WHERE assembly_id = ANY(...)`）。
///
/// ⚠️ **不过滤状态、不过滤交期**：子件的交期由父装配件那一行代表（子件自己的
/// `system_delivery_date` 在业务上不独立生效），把它单列成组只会让同一件事在清单里
/// 出现两遍。
///
/// ⚠️ 也**不需要**在这里补一道「已交量守卫」：装配件那一支的 `NOT EXISTS` 已经经
/// `c.assembly_id = a.id` 扫过它全部子件的批次 —— 装配件能进清单，就意味着**它的
/// 子件没有一件被交过**（见 [`list_overdue`] 的要点 4）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AssemblyChildRow {
    /// `t_part.id`（子件自己也是一张工单，有自己的序列号与批次）。
    pub id: i64,
    /// 父装配件 `t_assembly.id`（Rust 侧按它分桶挂回装配件行下面）。
    pub assembly_id: i64,
    /// 见 [`OrderListRow::serial_no`]：可空，渲染时留空。
    pub serial_no: Option<String>,
    pub name: String,
    pub quantity: i32,
}

/// 一次拉回清单里**全部装配件**的子件（`assembly_id = ANY($1)`）。
///
/// ⚠️ `ORDER BY assembly_id ASC, id ASC` 里的 `id ASC` 是**tiebreaker**，不是业务
/// 顺序：PG 对并列行不保证稳定排序，而子件要逐行渲染 —— 不补它，同一个库两次预览会
/// 给出不同的子件次序。它等价于「同父装配件取建单最早的子件」。
///
/// 入参为空集时**不发 SQL**（省一条语句；`assembly_id = ANY('{}')` 本来就恒空，但
/// 「空集不发」是这个域的通用纪律，见 `worker_rows` 之外的批量解析族）。
pub async fn list_assembly_children(
    conn: &mut PgConnection,
    assembly_ids: &[i64],
) -> Result<Vec<AssemblyChildRow>, sqlx::Error> {
    if assembly_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        AssemblyChildRow,
        r#"
        SELECT id, assembly_id AS "assembly_id!", serial_no, name, quantity
        FROM t_part
        WHERE assembly_id = ANY($1) AND deleted_at IS NULL
        ORDER BY assembly_id ASC, id ASC
        "#,
        assembly_ids,
    )
    .fetch_all(&mut *conn)
    .await
}

/// 一条**活跃批次**（`t_part_batch` 的部分列投影 —— 只为算「位置」）。
///
/// ⚠️ 刻意只投影 6 列而不是 `TPartBatch` 全列：日报对批次只关心「它在哪儿、归谁」，
/// 多拉的列是纯带宽（一条装配件 30 个子件 × 多批次时这个差额会被放大几十倍）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActiveBatchRow {
    pub part_id: i64,
    /// **每个 `part_id` 各自从 1 起**（唯一键 `uq_t_part_batch_part_no`），故它只是
    /// tiebreaker 的一部分，不是「装配件内的第几批」。
    pub batch_no: i32,
    pub status: String,
    /// `PRODUCTION_SHELF` / `INSPECTION_SHELF` / `WORKER` / `OUTSOURCE_COMPANY` /
    /// `OFFICE` / `None`。
    pub location: Option<String>,
    /// 指向 `t_shelf` / `t_worker` / `t_outsource_company` 之一，**取决于
    /// `location`**（无物理外键，见 `t_part_batch.current_holder_id` 的列注释）。
    pub current_holder_id: Option<i64>,
}

/// 一次拉回清单里全部 `t_part` 侧工单（含装配件的子件）的**活跃批次**。
///
/// ⚠️ `WHERE part_id = ANY($1) AND deleted_at IS NULL` 与
/// `prod::batch::repo::PartBatchRepo::list_active_by_part_ids` **逐字一致**，
/// `ORDER BY part_id ASC, batch_no ASC` 也是 —— 「活跃批次」的**定义**（软删闸门 +
/// 排序）只有一份含义，本域照抄而不是自己再定一遍，否则同一个工单在工单列表页和在
/// 日报里会显示两个不同的位置。
pub async fn list_active_batches(
    conn: &mut PgConnection,
    part_ids: &[i64],
) -> Result<Vec<ActiveBatchRow>, sqlx::Error> {
    if part_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        ActiveBatchRow,
        r#"
        SELECT part_id, batch_no, status, location, current_holder_id
        FROM t_part_batch
        WHERE part_id = ANY($1) AND deleted_at IS NULL
        ORDER BY part_id ASC, batch_no ASC
        "#,
        part_ids,
    )
    .fetch_all(&mut *conn)
    .await
}

/// min-progress 的**目标活跃批次**（判据逐字照抄 `part::service` 的两处实现）。
///
/// ## ⚠️ 为什么必须照抄而不能自己发明一套
///
/// 「这个工单现在在哪儿」这个问题，工单列表页已经有一个答案了
/// （`part::service::list_enrichment::enrich_part_list_with_location_and_holder`，
/// 与 `shared::batch::status_machine::compute_part_target` 同款判据）。日报如果自己
/// 选一套（比如「取最新批次」），同一个工单在两个页面上会显示两个位置，而两边的
/// 人都会认为**另一个页面错了**。所以本函数逐字复制那三步：
///
/// 1. 排除 `CANCELLED`；若一条都不剩 ⇒ 候选退回到**全部**批次（全取消的工单照样要
///    显示一个位置，否则清单里凭空少一行）；
/// 2. 非空时再排除 `COMPLETED`；若一条都不剩 ⇒ 候选退回第 1 步的结果；
/// 3. 取 [`part_status_progress`] 最小的那批；**并列时取首条**（`min_by_key` 的语义）
///    —— 首条由 SQL 的 `ORDER BY part_id, batch_no` 决定，即「并列取 `batch_no`
///    最小的」。
///
/// progress 表在 `shared::batch::status_machine`（本仓唯一的 progress 真源），本域
/// 引它而不复制：复制一份就会在「加一个批次状态」时静默漂移。
///
/// `None` = 无活跃批次 ⇒ 调用方**整项不显示**「位置」（不留悬空分隔符）。
pub fn active_batch_target(batches: &[ActiveBatchRow]) -> Option<&ActiveBatchRow> {
    if batches.is_empty() {
        return None;
    }
    let non_cancelled: Vec<&ActiveBatchRow> =
        batches.iter().filter(|b| b.status != "CANCELLED").collect();
    if non_cancelled.is_empty() {
        // 全是 CANCELLED ⇒ 退回全部批次（见 doc 的第 1 步）。
        return min_progress(batches.iter().collect());
    }
    let non_terminal: Vec<&ActiveBatchRow> = non_cancelled
        .iter()
        .copied()
        .filter(|b| b.status != "COMPLETED")
        .collect();
    if non_terminal.is_empty() {
        // 非 CANCELLED 全是 COMPLETED ⇒ 退回非取消集（见 doc 的第 2 步）。
        return min_progress(non_cancelled);
    }
    min_progress(non_terminal)
}

/// min-progress 取一条（**并列取首条**，与上两条 fallback 共用）。
fn min_progress(candidates: Vec<&ActiveBatchRow>) -> Option<&ActiveBatchRow> {
    candidates
        .iter()
        .min_by_key(|b| part_status_progress(&b.status))
        .copied()
}

// ---------------------------------------------------------------------------
// 三个 holder 名批量解析（每类 1 条 SQL，入参空集时跳过）
// ---------------------------------------------------------------------------

/// `t_shelf.code` 的批量解析。
///
/// ⚠️ **带软删闸门**：被软删的货架已不在现场清单上，报它的 `code` 等于报一个管理员
/// 已经无法核对的架号。
pub async fn shelf_codes(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        ShelfCodeRow,
        r#"
        SELECT id, code AS "code!"
        FROM t_shelf
        WHERE id = ANY($1) AND deleted_at IS NULL
        "#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.code)).collect())
}

/// `t_worker.name` 的批量解析（软删闸门同上）。
pub async fn worker_names(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        NameRow,
        r#"
        SELECT id, name AS "name!"
        FROM t_worker
        WHERE id = ANY($1) AND deleted_at IS NULL
        "#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name)).collect())
}

/// `t_outsource_company.name` 的批量解析（软删闸门同上）。
pub async fn outsource_names(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        NameRow,
        r#"
        SELECT id, name AS "name!"
        FROM t_outsource_company
        WHERE id = ANY($1) AND deleted_at IS NULL
        "#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.name)).collect())
}

/// `t_shelf` 的两列投影（`code` 在本域只是货架编号，故结构体独立于 `NameRow`）。
#[derive(Debug, Clone, sqlx::FromRow)]
struct ShelfCodeRow {
    id: i64,
    code: String,
}

/// `t_worker` / `t_outsource_company` 共用的两列投影。
#[derive(Debug, Clone, sqlx::FromRow)]
struct NameRow {
    id: i64,
    name: String,
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
            overview_batch_count, overview_quantity, note_count,
            overdue_count, upcoming_count, worker_row_total, status, error_message,
            sent_at, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $18)
        RETURNING id, config_version, trigger_source, webhook_name, webhook_masked,
                  report_date, markdown_bytes, chunk_count, chunk_sent,
                  overview_batch_count, overview_quantity, note_count,
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
               overview_batch_count, overview_quantity, note_count,
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
    /// 第二节那个数：本期出货的**去重工单数**（2026-10-23 起由 `note_count` 承载，
    /// 见 [`TWeComDailyLogRow::note_count`] 的语义变更说明）。
    pub note_count: i32,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(part_id: i64, batch_no: i32, status: &str) -> ActiveBatchRow {
        ActiveBatchRow {
            part_id,
            batch_no,
            status: status.to_string(),
            location: Some("PRODUCTION_SHELF".to_string()),
            current_holder_id: Some(900 + batch_no as i64),
        }
    }

    /// 取本文件源码里 `[start_marker, end_marker)` 之间的片段（失败即 panic）。
    fn src_between(start: &str, end: &str) -> String {
        let code = include_str!("repo.rs");
        let lo = code
            .find(start)
            .unwrap_or_else(|| panic!("源码里找不到 `{start}`（函数被改名/删除？）"));
        let hi = code[lo + start.len()..]
            .find(end)
            .unwrap_or_else(|| panic!("`{start}` 之后找不到 `{end}`"))
            + lo
            + start.len();
        // ⚠️ **先剥掉注释行**：doc 里正好写着「写成 `<= today + N` 会让……」，留着它
        // 会让下面那条「SQL 里不许出现 `<=`」的断言被自己的注释判红。
        code[lo..hi]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// ★ 守卫：**半开窗口的两个边界**在 SQL 里写对了。
    ///
    /// ⚠️ 为什么是**源码级**断言而不是普通单测：这两个边界是 SQL 文本里的比较运算符，
    /// 纯函数单元测试够不到它；而它是硬约束 2（「写 `<= today + N` 会让『3 日内』变成
    /// 4 天」）的全部内容 —— 写错了**不报任何错**，只是每天多列一批根本不该在「3 日内」
    /// 里的工单。端到端的行为证据在
    /// `tests/wecom_daily/preview.rs::orders_section_grouping_uses_a_half_open_date_window`
    /// （真库、真 SQL）；这条钉的是「改 SQL 的人会先撞到这里」。
    #[test]
    fn list_queries_use_the_half_open_date_window() {
        let overdue = src_between("pub async fn list_overdue", "pub async fn list_upcoming");
        assert!(
            overdue.contains("p.system_delivery_date < $1::date"),
            "逾期组 = `sdd < today`（今天到期属日期组）"
        );
        assert!(!overdue.contains("<="), "逾期组不许出现 `<=`：{overdue}");

        let upcoming = src_between("pub async fn list_upcoming", "/// 装配件的**子件行**");
        // 两支（`t_part` / `t_assembly`）各一对边界，缺一条就会漏掉一类工单。
        assert_eq!(
            upcoming.matches("system_delivery_date >= $1::date").count(),
            2,
            "两支都要 `>= today`（含今天）"
        );
        assert_eq!(
            upcoming
                .matches("system_delivery_date < $1::date + $3::int")
                .count(),
            2,
            "两支都要 `< today + days`（不含第 N 天）"
        );
        assert!(
            !upcoming.contains("<="),
            "写成 `<= today + N` 会让「3 日内」变成 4 天：{upcoming}"
        );
    }

    /// ★ 守卫：`active_batch_target` 的三段候选集与 `part::service` 的既有实现同形。
    ///
    /// 三段分别是：① 排除 `CANCELLED`；② 非空时再排除 `COMPLETED`，非空则用②；
    /// ③ 三者都空时逐级回退。**逐条写死**是为了让「改候选集 = 改日报里工单的位置」
    /// 这件事必须先来这里改（而不是在日报里偷偷换一套选批规则）。
    #[test]
    fn active_batch_target_follows_the_three_step_candidate_set() {
        // ① 全 CANCELLED ⇒ 退回全部批次，取 progress 最小者。
        let all_cancelled = vec![
            batch(1, 1, "CANCELLED"),
            batch(1, 2, "CANCELLED"),
            batch(1, 3, "CANCELLED"),
        ];
        assert_eq!(
            active_batch_target(&all_cancelled).map(|b| b.batch_no),
            Some(1),
            "全取消时取首条（progress 全是兜底档 2）"
        );

        // ② 非 CANCELLED 全 COMPLETED ⇒ 退回非取消集，取其中 progress 最小者。
        let all_completed = vec![
            batch(1, 1, "COMPLETED"),
            batch(1, 2, "PENDING"),
            batch(1, 3, "COMPLETED"),
        ];
        assert_eq!(
            active_batch_target(&all_completed).map(|b| b.status.clone()),
            Some("PENDING".to_string())
        );

        // ③ 正常形态：排除 CANCELLED / COMPLETED 后取 min-progress。
        let mixed = vec![
            batch(1, 1, "INSPECTION"),
            batch(1, 2, "CANCELLED"),
            batch(1, 3, "PENDING"),
            batch(1, 4, "COMPLETED"),
            batch(1, 5, "IN_PROCESS"),
        ];
        assert_eq!(
            active_batch_target(&mixed).map(|b| b.batch_no),
            Some(3),
            "PENDING 的 progress 最小；CANCELLED / COMPLETED 都不参与"
        );

        // 空集 ⇒ None（调用方整项不显示「位置」）。
        assert!(active_batch_target(&[]).is_none());
    }

    /// ★ 守卫：min-progress **并列取首条**，而首条由 SQL 的 `batch_no ASC` 决定。
    #[test]
    fn min_progress_ties_take_the_first_ordered_batch() {
        let tied = vec![batch(7, 1, "IN_PROCESS"), batch(7, 2, "IN_PROCESS")];
        assert_eq!(
            active_batch_target(&tied).map(|b| b.batch_no),
            Some(1),
            "两条 progress 相同时取 `batch_no` 最小的（并列可复现）"
        );
        // 反序输入下仍取首条 —— 判据是「输入顺序里的第一个」，由 SQL 的 ORDER BY 保证。
        let tied_reversed = vec![batch(7, 2, "IN_PROCESS"), batch(7, 1, "IN_PROCESS")];
        assert_eq!(
            active_batch_target(&tied_reversed).map(|b| b.batch_no),
            Some(2)
        );
    }
}
