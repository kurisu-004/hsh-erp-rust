//! mail_digest 域 repo 层 —— SQL 真源
//!
//! 两组 SQL，形态完全不同，分工明确：
//! 1. **统计查询**（[`list_overdue`] / [`list_upcoming`]）：静态 SQL，走
//!    `sqlx::query_as!` 字面量宏 ⇒ **编译期按 `.env` 的 DATABASE_URL 连开发库
//!    校验列名与类型**（改列名编译即失败），产物进 `.sqlx/`。
//! 2. **配置 / 流水 CRUD**：静态 SQL，同样走 `query_as!` / `query!` 宏。
//!
//! ## 统计口径：为什么与 dashboard 的 SQL **刻意分叉**
//!
//! 两条 SQL 的「未交付」谓词与 dashboard 域
//! `repo/delivery.rs::SQL_COUNT_OVERDUE` / `SQL_ORDERS_*` **逐字同源**（状态白名单
//! 取值来自共享的 [`crate::shared::delivery_status::DELIVERY_STATUSES`]，见它的
//! doc；行单位工单级、
//! 装配件行替换子件行、已交量守卫 `status IN ('DELIVERED','COMPLETED')`）。
//! 但**行集合的取法**不一样，这是需求决定的、不是漂移：
//!
//! | | dashboard | mail_digest（本文件） |
//! |---|---|---|
//! | 每桶上限 | `LIMIT 30`（`DELIVERY_BUCKET_LIMIT`） | **无 LIMIT** |
//! | 取哪些列 | 原样 `p.customer_id`（客户名另有批量聚合） | `customer_id` 换成同一条 SQL 里 `LEFT JOIN` 来的 `customer_name`，**另多取** `drawing_no` / `order_no`（邮件要图号 / 订单号） |
//! | 未来节窗口 | `>= today` 无上界（`upcoming` 桶） | `[today, today+N)`，N clamp 1..60 |
//! | 客户名 | 单独批量聚合（防 N+1） | 本 SQL 里 `LEFT JOIN` 一次带出 |
//!
//! dashboard 那份的 `LIMIT 30` 是**大屏可读性**约束（大屏一屏放不下 300 行），
//! 邮件则必须给全量 —— 漏一条交期就是漏一个客户。而「未来 N 天有上界」是本需求
//! 本身（用户原话「未来 14 天」）。所以**不复用** `DeliveryRepo::list_orders_*`：
//! 复用就得先解掉它的 `LIMIT 30`（等于改坏大屏）或在外面再分页（等于让 SQL
//! 口径与 Rust 侧重叠，两边都要维护）。
//!
//! ## 四条硬约束（改 SQL 前逐条读）
//!
//! 1. **工单级行单位**：`t_part` 侧必须带 `AND p.assembly_id IS NULL`（装配件子件
//!    不重复计入），装配件单独查 `t_assembly` 且其行**替换**子件行。与 dashboard
//!    交期面板三桶同一行单位（柱状图 / 抽屉是件级，本域**不**跟它）。
//! 2. **`NOT EXISTS` 必须同时判 `b.deleted_at IS NULL` 且状态集含 `'COMPLETED'`**：
//!    force-complete 端点（`POST /prod/parts/{id}/force-complete` 与
//!    `POST /prod/assemblies/{id}/force-complete`）把批次强推 `COMPLETED`，
//!    守卫漏掉 `COMPLETED` 会让已完工的工单继续出现在「逾期未交」邮件里。
//! 3. **窗口下界来自形参 `today`，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区
//!    （测试容器 UTC）与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，
//!    Shanghai 00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「今天到期」的
//!    工单会落进逾期节、未来节则整体偏移一天。
//!
//! 4. **统计取数恒 ≤ 3 条语句**（2026-10-16）：上界 = 两条主查询（逾期 / 未来）各一条
//!    加 [`list_batches`] 一条；两节都开（缺省）且查到工单时**恰好 3 条**，关掉的节不
//!    查、无工单时批次那条不发。**与工单数、批次数无关** —— 批次是一次性按 id 集合
//!    `= ANY($N)` 拉回 Rust 侧分组，循环里只做内存操作。⚠️ 节开关拦的是「不能同时关」
//!    （`21704`），**不是**「只能开一个」，「恒定 2 条」是错的说法。护栏见
//!    `tests/mail_digest/n_plus_one_guard.rs`。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::mail_digest::model::{TMailDigestConfig, TMailDigestLog};
// 「未交付」6 态白名单取自共享常量（2026-10-23）：本域的统计 SQL 自己写全，
// 但状态判据**不复制** —— 副本漏改一处不会编译失败，只会让邮件与大屏的逾期数互相
// 矛盾且无单测变红。共享模块不属于任何业务域 ⇒ 本域的域隔离判定不受影响。
use crate::shared::delivery_status::delivery_statuses_vec;

/// 邮件正文的一行（`t_part` 一行 UNION ALL `t_assembly` 一行）+ 它下面的全部批次子行。
///
/// ⚠️ **刻意不起 `T*` 名**：`T*` 在本仓是「域 owned 表的全列投影」的约定
/// （见 `model.rs` 顶部 doc），而本结构是**跨多张非 owned 表的读侧视图行**
/// （`t_assembly` 没有 `assembly_id`、`t_part` 没有子件概念，两侧列名与类型
/// 不同，装配件那一支额外靠 `row_type` 字面量区分；`batches` 更完全不属于任何一张
/// 表的投影）。起 `T*` 名会让「这是本域 owned 表」的错误印象盖过「这是拼出来的
/// 视图行」。
#[derive(Debug, Clone)]
pub struct DigestRow {
    /// `t_part.id` 或 `t_assembly.id`。雪花全局唯一，但**跨表**不保证不撞
    /// —— 本域从不把它当主键用，只在调试时人工定位。
    pub id: i64,
    /// `varchar(15)`，可为 NULL（未派发序列号）。
    pub serial_no: Option<String>,
    /// `varchar(100) NOT NULL`。
    pub drawing_no: String,
    /// `varchar(200) NOT NULL`。
    pub name: String,
    /// 装配件的语义是**套数**、散件是**件数**（沿用 dashboard 的行单位约定）。
    pub quantity: i32,
    /// 6 态之一（见 `crate::shared::delivery_status::DELIVERY_STATUSES`）；装配件侧
    /// 只可能命中其中 4 态
    /// （`AssemblyStatus` 无 `PROGRAMMING` / `OUTSOURCE`），无需第二份常量。
    pub status: String,
    /// 系统交期（`date`）；可为 NULL，被窗口谓词自然排除。
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    /// `varchar(30)`，可为 NULL。
    pub order_no: Option<String>,
    /// `LEFT JOIN t_customer` 得到的客户名。
    ///
    /// ⚠️ **刻意不加 `AND c.deleted_at IS NULL`**：`ON` 里只有 `cust.id = p.customer_id`
    /// ⇒ **软删客户的名字照常带出**。闸门放 `ON` 里本来也只会清空名字、**不会**剔除工单行，
    /// 所以「不该把工单从邮件里剔除」不是这里的取舍理由；取舍是「客户名以 `id` 为准、
    /// 不受软删影响」（与 dashboard `list_orders_*` 同款 —— 那边是
    /// `SELECT id, name FROM t_customer WHERE id = ANY(...)`，同样无软删闸门）。
    /// 本列为 `NULL` 的**唯一**成因是客户行不存在（`customer_id` 悬空，指向一个已不存在的
    /// `t_customer.id`）。四条语句（[`list_overdue`] / [`list_upcoming`] 各两个
    /// `UNION ALL` 分支）口径一致。
    pub customer_name: Option<String>,
    /// SQL 里写死的来源字面量：`'PART'` / `'ASSEMBLY'`。
    pub row_type: String,
    /// 该工单名下的**全部**批次（2026-10-16），按 `batch_no ASC, id ASC`。
    ///
    /// ⚠️ **「该工单名下」对装配件是「它全部子件名下」**：批次挂在**子件**
    /// （`t_part_batch.part_id → t_part.id`）上，而 [`DigestBatchRow::batch_no`] 是
    /// **每个 `part_id` 各自从 1 起** ⇒ 一个装配件下多个子件各有一条 `1`，跨子件会撞号。
    ///
    /// 由 [`list_batches`] 一次拉回后按 `(row_type, id)` 在 Rust 侧分组装进来
    /// （见 `service::fetch_rows`）⇒ 工单数、批次数都不会让语句数变多。
    /// ⚠️ **不过滤状态**（用户要的是「对应的**所有**批次」），但这与**工单行**的
    /// 已交量守卫方向相反：守卫是 `NOT EXISTS（任一批次 `status IN
    /// ('DELIVERED','COMPLETED')`）` ⇒ **一件交过，整个工单行就不出现**
    /// ⇒ 工单行一旦出现，它名下**不可能**有 `DELIVERED` / `COMPLETED` 批次，
    /// 「不过滤状态」实际只对 `CANCELLED` 这类**非交割终态**批次有意义（照列）。
    /// ⚠️ 终态批次的位置列恒渲成 `—` 是**另一条**事实（迁移出厂即清
    /// `location` / `current_holder_id`，见 [`DigestBatchRow::holder_name`]），
    /// 不要与上一条混成一句。
    /// 无批次时是空数组（渲染层不渲子行）。回归：
    /// `tests/mail_digest/batches.rs::mixed_delivered_and_open_batches_follow_the_opposite_two_rules`。
    pub batches: Vec<DigestBatchRow>,
}

impl DigestRow {
    /// 是否装配件行（决定 `quantity` 的单位文案与「工单类型」列的展示值）。
    pub fn is_assembly(&self) -> bool {
        self.row_type == "ASSEMBLY"
    }
}

/// 主查询的**纯 SQL 投影**（[`DigestRow`] 的原料）。
///
/// ⚠️ 为什么与 [`DigestRow`] 分成两个结构：`sqlx::query_as!` 生成的是
/// **struct literal**（`DigestRow { id: …, … }`），SELECT 里没有的字段**编译不过**。
/// 而 `batches` 不是任何一列的投影（它由第二条语句在 Rust 侧装配）⇒ 只能拆开：
/// 宏落在本结构上，`list_overdue` / `list_upcoming` 再逐字段搬进 [`DigestRow`]。
#[derive(Debug, Clone)]
struct DigestOrderRow {
    id: i64,
    serial_no: Option<String>,
    drawing_no: String,
    name: String,
    quantity: i32,
    status: String,
    system_delivery_date: Option<NaiveDate>,
    is_urgent: bool,
    order_no: Option<String>,
    customer_name: Option<String>,
    row_type: String,
}

impl From<DigestOrderRow> for DigestRow {
    /// 批次留空 —— 装配在 `service::fetch_rows` 里统一做（那里才知道两节的全部
    /// 工单 id），不在单条查询里做。
    fn from(r: DigestOrderRow) -> Self {
        Self {
            id: r.id,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            name: r.name,
            quantity: r.quantity,
            status: r.status,
            system_delivery_date: r.system_delivery_date,
            is_urgent: r.is_urgent,
            order_no: r.order_no,
            customer_name: r.customer_name,
            row_type: r.row_type,
            batches: Vec::new(),
        }
    }
}

/// 邮件正文里一个批次的**子行**（2026-10-16）。
///
/// ⚠️ 同样**刻意不起 `T*` 名**：它是 `t_part_batch` 单表列 + 三个 holder 表解析出
/// 的展示名拼出来的读侧视图行，不是域 owned 表的全列投影。
#[derive(Debug, Clone)]
pub struct DigestBatchRow {
    /// `t_part_batch.id`。
    pub id: i64,
    /// `integer`，**该子件（`t_part`）内**的批次序号（1 起）；渲染成**纯数字**（批次格
    /// 另加一个全角空格缩进，见 `template::batch_row`）。
    ///
    /// ⚠️ **不是「装配件内」的序号、更不是「这封信里」的序号**：唯一键是
    /// `uq_t_part_batch_part_no (part_id, batch_no)`、拆批走
    /// `SELECT COALESCE(MAX(batch_no),0)+1 … WHERE part_id = $1`
    /// （`prod::batch::repo::queries`）⇒ **每个 `part_id` 各自从 1 重新数**。一个装配件下
    /// N 个子件各 1 批 ⇒ **N 行都渲成 `　1`**。子件之间没有先后口径，**子件间的先后顺序
    /// 不是本域的契约**（`ORDER BY` 已补 `id` 兜底，但那只是让结果可复现，不构成语义）。
    pub batch_no: i32,
    /// 批次所属**子件**的序列号（`t_part.serial_no`，即 `t_part_batch.part_id` 指向的
    /// 那一行）；填在子行的「序列号」列上。
    ///
    /// ⚠️ **散件工单的批次恒为 `None`** —— 那一支的 `t_part_batch.part_id` 指向的就是
    /// 工单行自己那个零件，序列号已经写在工单行上，子行再写一遍是纯冗余（每行都重复
    /// 一个已经在上一格出现过的值）。这是刻意选择，不是漏取。
    ///
    /// ⚠️ **装配件追加的子件 `serial_no` 恒为 NULL**（补件语义不重新打开序列号派发
    /// 通道，出处是 `assembly::service::crud::add_assembly_child_inner` 的 doc）⇒ 那一类
    /// 子件的批次子行序列号格**留空**，**不回退到图号**：图号是「图号」列的内容，
    /// 在序列号列重复它会让这一列的语义漂成「零件标识」。回归：
    /// `template.rs::tests::assembly_child_without_serial_no_leaves_cell_blank`。
    pub part_serial_no: Option<String>,
    /// `integer`，本批次数量。**不带**「套」后缀 —— 批次恒是件数，「套」是装配件
    /// 工单行的口径。
    pub quantity: i32,
    /// `PartStatus` 的 9 个字面量之一；渲染前经 `dto::batch_status_label` 转中文。
    pub status: String,
    /// `is_repairing = true` ⇒ 状态列显示「返修中」（该布尔列是 2026-10-01 从
    /// `REPAIRING` 状态码降级来的）。
    pub is_repairing: bool,
    /// `OFFICE` / `PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` /
    /// `OUTSOURCE_COMPANY`，可为 NULL。
    pub location: Option<String>,
    /// 按 `location` 分桶解析出的展示名：货架 `code` / 工人 `name` /
    /// 外协公司 `name`；解析不出（`OFFICE`、`location IS NULL`、holder 行已软删）
    /// 时为 `None`。
    ///
    /// ⚠️ **终态批次这里恒为 `None`**：迁移
    /// `20261013000002_001_clear_terminal_batch_location.sql` 把 `DELIVERED` /
    /// `COMPLETED` / `CANCELLED` 三态的 `location` 与 `current_holder_id` 一起清了
    /// （出厂时工件已经不在任何货架 / 工人 / 外协厂手里）。这不是取数漏了，是数据
    /// 本来就没有；渲染层渲成 `—`。登记见 `docs/api/mail_digest.md` §8.3。
    pub holder_name: Option<String>,
    /// 与工单行的 `row_type` 同源（`'PART'` / `'ASSEMBLY'`），由
    /// [`list_batches`] 的 SQL 原样带回，供 Rust 侧按 `(owner_type, owner_id)` 分组。
    pub owner_type: String,
    /// 所属工单 id（`t_part.id` 或 `t_assembly.id`）。
    pub owner_id: i64,
}

/// 已逾期且一件没交过的工单（工单级）。
///
/// 参数：
/// - `today`：窗口下界（**形参**，绝不写 `CURRENT_DATE`，理由见模块 doc 硬约束 3）。
///   谓词是 `system_delivery_date < $1`，即**含今天之前、不含今天**（今天的交期
///   属未来节的「今日」组）。
///
/// ⚠️ `system_delivery_date IS NULL` 的工单被 `IS NOT NULL` 显式排除而不是靠
/// `NULL < $1` 返回 NULL 自然落空：显式写法让「没填交期」这个意图在 SQL 里
/// 可见，读者不必反推 NULL 比较语义。
pub async fn list_overdue(
    conn: &mut PgConnection,
    today: NaiveDate,
) -> Result<Vec<DigestRow>, sqlx::Error> {
    sqlx::query_as!(
        DigestOrderRow,
        r#"
        SELECT
            u.id                AS "id!",
            u.serial_no         AS "serial_no?",
            u.drawing_no        AS "drawing_no!",
            u.name              AS "name!",
            u.quantity          AS "quantity!",
            u.status            AS "status!",
            u.system_delivery_date AS "system_delivery_date?",
            u.is_urgent         AS "is_urgent!",
            u.order_no          AS "order_no?",
            u.customer_name     AS "customer_name?",
            u.row_type          AS "row_type!"
        FROM (
            SELECT p.id, p.serial_no, p.drawing_no, p.name, p.quantity, p.status,
                   p.system_delivery_date, p.is_urgent, p.order_no,
                   cust.name AS customer_name, 'PART'::text AS row_type
            FROM t_part p
            LEFT JOIN t_customer cust ON cust.id = p.customer_id
            WHERE p.deleted_at IS NULL
              AND p.assembly_id IS NULL
              AND p.system_delivery_date IS NOT NULL
              AND p.system_delivery_date < $1
              AND p.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part_batch b
                               WHERE b.part_id = p.id AND b.deleted_at IS NULL
                                 AND b.status IN ('DELIVERED', 'COMPLETED'))
            UNION ALL
            SELECT a.id, a.serial_no, a.drawing_no, a.name, a.quantity, a.status,
                   a.system_delivery_date, a.is_urgent, a.order_no,
                   cust.name, 'ASSEMBLY'::text
            FROM t_assembly a
            LEFT JOIN t_customer cust ON cust.id = a.customer_id
            WHERE a.deleted_at IS NULL
              AND a.system_delivery_date IS NOT NULL
              AND a.system_delivery_date < $1
              AND a.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part c JOIN t_part_batch b ON b.part_id = c.id
                              WHERE c.assembly_id = a.id AND c.deleted_at IS NULL
                                AND b.deleted_at IS NULL
                                AND b.status IN ('DELIVERED', 'COMPLETED'))
        ) u
        ORDER BY u.system_delivery_date ASC, u.id ASC
        "#,
        today,
        delivery_statuses_vec(),
    )
    .fetch_all(&mut *conn)
    .await
    .map(|rows| rows.into_iter().map(DigestRow::from).collect())
}

/// 未来 `days` 天内交期到期且一件没交过的工单（工单级）。
///
/// 窗口是**半开区间** `[today, today + days)`：`days = 14` 时含今天与今天之后
/// 13 天，共 14 天（与 dashboard `upcoming-delivery?days=14` 同一口径）。
/// SQL 里用 `($3 || ' days')::interval` 而不是 `$1 + $3`：PG 的 `date + int`
/// 本身就返回 `date`，写成 `$1 + ($3 || ' days')::interval` 会得到
/// `timestamp`，与 `date` 列比较虽能隐式转换，但会让生成的执行计划里多一次 cast。
pub async fn list_upcoming(
    conn: &mut PgConnection,
    today: NaiveDate,
    days: i32,
) -> Result<Vec<DigestRow>, sqlx::Error> {
    sqlx::query_as!(
        DigestOrderRow,
        r#"
        SELECT
            u.id                AS "id!",
            u.serial_no         AS "serial_no?",
            u.drawing_no        AS "drawing_no!",
            u.name              AS "name!",
            u.quantity          AS "quantity!",
            u.status            AS "status!",
            u.system_delivery_date AS "system_delivery_date?",
            u.is_urgent         AS "is_urgent!",
            u.order_no          AS "order_no?",
            u.customer_name     AS "customer_name?",
            u.row_type          AS "row_type!"
        FROM (
            SELECT p.id, p.serial_no, p.drawing_no, p.name, p.quantity, p.status,
                   p.system_delivery_date, p.is_urgent, p.order_no,
                   cust.name AS customer_name, 'PART'::text AS row_type
            FROM t_part p
            LEFT JOIN t_customer cust ON cust.id = p.customer_id
            WHERE p.deleted_at IS NULL
              AND p.assembly_id IS NULL
              AND p.system_delivery_date IS NOT NULL
              AND p.system_delivery_date >= $1::date
              AND p.system_delivery_date < $1::date + $3::int
              AND p.status = ANY($2::varchar[])
              AND NOT EXISTS (SELECT 1 FROM t_part_batch b
                               WHERE b.part_id = p.id AND b.deleted_at IS NULL
                                 AND b.status IN ('DELIVERED', 'COMPLETED'))
            UNION ALL
            SELECT a.id, a.serial_no, a.drawing_no, a.name, a.quantity, a.status,
                   a.system_delivery_date, a.is_urgent, a.order_no,
                   cust.name, 'ASSEMBLY'::text
            FROM t_assembly a
            LEFT JOIN t_customer cust ON cust.id = a.customer_id
            WHERE a.deleted_at IS NULL
              AND a.system_delivery_date IS NOT NULL
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
        delivery_statuses_vec(),
        days,
    )
    .fetch_all(&mut *conn)
    .await
    .map(|rows| rows.into_iter().map(DigestRow::from).collect())
}

/// 一次拉回给定工单的**全部**批次（不按状态过滤），holder 名在**同一条 SQL**里解析完。
///
/// ⚠️ 入参 id 全部来自已带已交量守卫的主查询 ⇒ 名下**不可能**有 `DELIVERED` /
/// `COMPLETED` 批次，「不按状态过滤」实际只对 `CANCELLED` 这类**非交割终态**有意义；
/// 本函数自己**不加**状态谓词（守卫属于主查询，见 [`DigestRow::batches`]）。
///
/// 参数：
/// - `part_ids`：散件工单 id 集合（主查询 `row_type='PART'` 的那些）；
/// - `assembly_ids`：装配件工单 id 集合（主查询 `row_type='ASSEMBLY'` 的那些）。
///
/// ## 为什么是「一条语句、零 N+1」
///
/// 逐工单循环查批次是**功能完全正确**的改法（行数守恒、少批量时更快），但邮件
/// 可能一次带几百个工单 ⇒ 语句数随工单数线性上涨，而这条语句的**总行数**本来就与
/// 工单数成正比（每工单至少 1 个批次）。故改成「按 id 集合 `= ANY` 一次拉回、
/// Rust 侧按 `(owner_type, owner_id)` 分组」，语句数**恒为 1**。
/// `service::fetch_rows` 是唯一调用方，其「循环里不得调 `repo::`」由
/// `tests/mail_digest/n_plus_one_guard.rs` 的源码护栏钉住。
///
/// ## 设计要点
///
/// - **两个 id 集合分开绑**（`$1::bigint[]` / `$2::bigint[]`）：装配件的批次**挂在子件
///   上**（`t_part_batch.part_id → t_part.id`，子件 `t_part.assembly_id` 指向装配件），
///   那一支因此必须经 `t_part c JOIN t_assembly a` 两跳，口径与主查询的已交量守卫
///   **逐字一致**（含 `c.deleted_at IS NULL`）。
/// - **空数组是常态，不是边界情况**：`= ANY('{}')` 恒 false，一侧为空时该侧 UNION
///   分支贡献 0 行、另一半照常工作。仓内绝大多数工单不是装配件。
/// - **三张 holder 表全是 `LEFT JOIN` + `location` 分桶谓词**（不匹配就退化成
///   `holder_name = NULL`，而**不会**丢掉批次行）；`location` 只有 5 个取值、DB
///   侧无枚举约束，将来多一个取值时的表现是「位置列显示 `—`」，不是「批次消失」。
/// - **holder 三表都带 `deleted_at IS NULL`**（在 `ON` 条件里，不是 `WHERE`）：
///   软删的货架 / 工人 / 外协厂不该出现在邮件里；而加了它之后 `LEFT JOIN` 仍保留行，
///   只是名字解析不出来。
/// - **`ORDER BY` 在 SQL 里排**（渲染层不二次排序，与主查询同一约定）。⚠️ **`batch_no`
///   不是全局序号**（见 [`DigestBatchRow::batch_no`]）⇒ 一个装配件下多个子件的批次可以
///   同为 `batch_no = 1`，只按 `(owner_type, owner_id, batch_no)` 排时 PG 的返回顺序
///   **不确定**，两封连续邮件的子件行可能互换。末尾补 `u.id ASC` 兜底：它只保证**结果
///   可复现**（同一份数据每次渲染同序），**不**让子件之间产生先后语义。
/// - **`part_serial_no` 两支取法不同（2026-10-16）**：装配件支取子件的 `c.serial_no`
///   （可空），散件支写 `NULL::varchar` —— 那一支的批次就是工单自己的零件，序列号已在
///   工单行上，子行重复写是冗余。逐字段理由见 [`DigestBatchRow::part_serial_no`]。
pub async fn list_batches(
    conn: &mut PgConnection,
    part_ids: &[i64],
    assembly_ids: &[i64],
) -> Result<Vec<DigestBatchRow>, sqlx::Error> {
    sqlx::query_as!(
        DigestBatchRow,
        r#"
        SELECT
            u.owner_type       AS "owner_type!",
            u.owner_id         AS "owner_id!",
            u.id               AS "id!",
            u.batch_no         AS "batch_no!",
            u.quantity         AS "quantity!",
            u.status           AS "status!",
            u.is_repairing     AS "is_repairing!",
            u.location         AS "location?",
            u.holder_name      AS "holder_name?",
            u.part_serial_no   AS "part_serial_no?"
        FROM (
            SELECT 'PART'::text AS owner_type,
                   p.id AS owner_id,
                   b.id, b.batch_no, b.quantity, b.status, b.is_repairing, b.location,
                   CASE b.location
                       WHEN 'WORKER'            THEN w.name
                       WHEN 'PRODUCTION_SHELF'  THEN s.code
                       WHEN 'INSPECTION_SHELF'  THEN s.code
                       WHEN 'OUTSOURCE_COMPANY' THEN oc.name
                   END AS holder_name,
                   -- 2026-10-16：散件支恒 NULL（子行不重复工单行的序列号，见
                   -- DigestBatchRow::part_serial_no）。显式写 NULL 而不是不选该列，
                   -- 是因为 UNION ALL 两支的列必须逐列对齐、类型也要一致。
                   NULL::varchar AS part_serial_no
            FROM t_part_batch b
            JOIN t_part p ON p.id = b.part_id AND p.deleted_at IS NULL
            LEFT JOIN t_worker w
                   ON b.location = 'WORKER' AND w.id = b.current_holder_id
                  AND w.deleted_at IS NULL
            LEFT JOIN t_shelf s
                   ON b.location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')
                  AND s.id = b.current_holder_id
                  AND s.deleted_at IS NULL
            LEFT JOIN t_outsource_company oc
                   ON b.location = 'OUTSOURCE_COMPANY' AND oc.id = b.current_holder_id
                  AND oc.deleted_at IS NULL
            WHERE b.deleted_at IS NULL AND p.id = ANY($1::bigint[])
            UNION ALL
            SELECT 'ASSEMBLY'::text AS owner_type,
                   a.id AS owner_id,
                   b.id, b.batch_no, b.quantity, b.status, b.is_repairing, b.location,
                   CASE b.location
                       WHEN 'WORKER'            THEN w.name
                       WHEN 'PRODUCTION_SHELF'  THEN s.code
                       WHEN 'INSPECTION_SHELF'  THEN s.code
                       WHEN 'OUTSOURCE_COMPANY' THEN oc.name
                   END AS holder_name,
                   c.serial_no AS part_serial_no
            FROM t_part_batch b
            JOIN t_part c     ON c.id = b.part_id AND c.deleted_at IS NULL
            JOIN t_assembly a ON a.id = c.assembly_id AND a.deleted_at IS NULL
            LEFT JOIN t_worker w
                   ON b.location = 'WORKER' AND w.id = b.current_holder_id
                  AND w.deleted_at IS NULL
            LEFT JOIN t_shelf s
                   ON b.location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')
                  AND s.id = b.current_holder_id
                  AND s.deleted_at IS NULL
            LEFT JOIN t_outsource_company oc
                   ON b.location = 'OUTSOURCE_COMPANY' AND oc.id = b.current_holder_id
                  AND oc.deleted_at IS NULL
            WHERE b.deleted_at IS NULL AND a.id = ANY($2::bigint[])
        ) u
        ORDER BY u.owner_type ASC, u.owner_id ASC, u.batch_no ASC, u.id ASC
        "#,
        part_ids,
        assembly_ids,
    )
    .fetch_all(&mut *conn)
    .await
}

// ============================================================================
// 配置单例 CRUD
// ============================================================================

/// 读配置单例。`None` = 表里还没有未软删行（`GET /config` 据此返回默认值 +
/// `version=0` + `id="0"`，见 `service::get_config`）。
///
/// `LIMIT 1` 而非不加：唯一索引 `uk_t_mail_digest_config_alive` 保证至多一行，
/// 但加上它能让「唯一索引被人手工 drop 掉」这种脏库的异常表现为「取第一行」
/// 而不是「取到多行直接报错」—— 邮件配置是单例，取第一行继续跑比整个页面 500 好。
pub async fn get_config(conn: &mut PgConnection) -> Result<Option<TMailDigestConfig>, sqlx::Error> {
    sqlx::query_as!(
        TMailDigestConfig,
        r#"
        SELECT id, version, enabled, frequency, send_hour, send_minute, weekday,
               day_of_month, recipients, cc_recipients, include_overdue,
               include_upcoming, upcoming_days, visible_columns, subject_template,
               sender_account_id, created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_mail_digest_config
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
/// 返回 `Ok(false)` 表示**并发下已有别的请求插进来了**（撞唯一索引
/// `uk_t_mail_digest_config_alive`，SQLSTATE 23505），此时调用方应把 `23505`
/// **转成 OCC 冲突**而不是当成数据库故障 —— 语义上「别人刚建好了，你刷新一下
/// 再改」就是 OCC 冲突，与 CLAUDE.md §8 的「40901 只表达 OCC」一致。
///
/// ⚠️ 唯一索引是表达式索引 `((true)) WHERE deleted_at IS NULL`，PG 对它报的是
/// 23505 unique_violation，与普通唯一索引同一错误码，故按码判定即可。
pub async fn insert_config(
    conn: &mut PgConnection,
    id: i64,
    cfg: &crate::modules::mail_digest::dto::NormalizedConfig,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        INSERT INTO t_mail_digest_config (
            id, version, enabled, frequency, send_hour, send_minute, weekday,
            day_of_month, recipients, cc_recipients, include_overdue,
            include_upcoming, upcoming_days, visible_columns, subject_template,
            sender_account_id, created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $16, $17)
        "#,
    )
    .bind(id)
    .bind(cfg.enabled)
    .bind(cfg.frequency.as_str())
    .bind(cfg.send_hour)
    .bind(cfg.send_minute)
    .bind(cfg.weekday)
    .bind(cfg.day_of_month)
    .bind(&cfg.recipients)
    .bind(&cfg.cc_recipients)
    .bind(cfg.include_overdue)
    .bind(cfg.include_upcoming)
    .bind(cfg.upcoming_days)
    .bind(cfg.visible_columns.iter().map(|c| c.as_str()).collect::<Vec<&str>>())
    .bind(cfg.subject_template.as_deref())
    .bind(cfg.sender_account_id)
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;

    Ok(res.rows_affected() == 1)
}

/// 全量覆写（OCC）。
///
/// `WHERE id=$1 AND version=$2` 是本仓 OCC 的标准形态；`rows_affected == 0`
/// 表示「版本对不上」⇒ 调用方返 `40901 VERSION_CONFLICT`。
///
/// ⚠️ 用 `&mut *conn` 的 **autocommit**（不开事务），与其它写端点一致。
///
/// **为什么不进 CLAUDE.md §1 的例外清单**（2026-10-13 review 第 1 轮 NIT 补记）：
/// 那份清单里的四条**例外**各自都有「开事务反而更糟」的具体理由（逐行部分成功、
/// fixture 自管、只读端点、逃生通道），本函数**不属于**它们中的任何一类 ——
/// 它就是常规的「单行 UPDATE + 立刻判 `rows_affected`」，与全仓其它 OCC 写端点同形，
/// 本来就不需要进例外清单（进清单的门槛是「与默认约定**不同**且需要理由」）。
/// 真正值得注意的是它**没有**的事：全量覆写**不**级联写任何其它表（不碰
/// `t_part` / `t_part_batch` / `t_mail_digest_log`），所以不存在「必须同事务」的第二个写。
/// ⚠️ 若将来给配置加出「改一次要连带改多张表」的效果，这里必须同时改 handler 的事务
/// 边界（`service::save_config` 现在是「UPDATE → 读回新 version」两段，跨两段时
/// 读回可能读到别人的值）。
pub async fn update_config(
    conn: &mut PgConnection,
    id: i64,
    expected_version: i32,
    cfg: &crate::modules::mail_digest::dto::NormalizedConfig,
    operator: i64,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE t_mail_digest_config
        SET enabled          = $3,
            frequency        = $4,
            send_hour        = $5,
            send_minute      = $6,
            weekday          = $7,
            day_of_month     = $8,
            recipients       = $9,
            cc_recipients    = $10,
            include_overdue  = $11,
            include_upcoming = $12,
            upcoming_days    = $13,
            visible_columns  = $14,
            subject_template = $15,
            sender_account_id = $16,
            version          = version + 1,
            updated_at       = $17,
            updated_by       = $18
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
    .bind(&cfg.recipients)
    .bind(&cfg.cc_recipients)
    .bind(cfg.include_overdue)
    .bind(cfg.include_upcoming)
    .bind(cfg.upcoming_days)
    .bind(
        cfg.visible_columns
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<&str>>(),
    )
    .bind(cfg.subject_template.as_deref())
    .bind(cfg.sender_account_id)
    .bind(now)
    .bind(operator)
    .execute(&mut *conn)
    .await?;

    Ok(res.rows_affected() == 1)
}

// ============================================================================
// 发送流水
// ============================================================================

/// 落一行发送流水（**成败都落**，见 `t_mail_digest_log` 的表注释）。
///
/// ⚠️ `error_message` 由调用方**截断到 500 字符**再传（列是 `varchar(500)`）：
/// SMTP 报错文本常带整段服务器应答，很容易超长；让 PG 报错会把「发送失败」
/// 这件事本身变成「落库也失败」，反而丢掉诊断信息。
pub async fn insert_log(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    log: &NewDigestLog,
) -> Result<TMailDigestLog, sqlx::Error> {
    let id = snowflake.next_id();
    let now = log.sent_at;
    sqlx::query_as!(
        TMailDigestLog,
        r#"
        INSERT INTO t_mail_digest_log (
            id, config_version, trigger_source, to_recipients, cc_recipients,
            subject, overdue_count, upcoming_count, html_bytes, status,
            error_message, sent_at, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12)
        RETURNING id, config_version, trigger_source, to_recipients, cc_recipients,
                  subject, overdue_count, upcoming_count, html_bytes, status,
                  error_message, sent_at, created_at
        "#,
        id,
        log.config_version,
        log.trigger_source,
        log.to_recipients,
        log.cc_recipients,
        log.subject,
        log.overdue_count,
        log.upcoming_count,
        log.html_bytes,
        log.status,
        log.error_message,
        now,
    )
    .fetch_one(&mut *conn)
    .await
}

/// 发送流水分页读（`sent_at DESC`，同刻按 `id DESC` 兜底排序确定性）。
pub async fn list_logs(
    conn: &mut PgConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<TMailDigestLog>, i64), sqlx::Error> {
    let items = sqlx::query_as!(
        TMailDigestLog,
        r#"
        SELECT id, config_version, trigger_source, to_recipients, cc_recipients,
               subject, overdue_count, upcoming_count, html_bytes, status,
               error_message, sent_at, created_at
        FROM t_mail_digest_log
        ORDER BY sent_at DESC, id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(&mut *conn)
    .await?;

    let total: i64 = sqlx::query("SELECT COUNT(*) FROM t_mail_digest_log")
        .fetch_one(&mut *conn)
        .await?
        .get(0);

    Ok((items, total))
}

/// 写一行发送流水的入参（id / created_at 由 [`insert_log`] 生成）。
#[derive(Debug, Clone)]
pub struct NewDigestLog {
    pub config_version: i32,
    /// `SCHEDULED` / `MANUAL`。
    pub trigger_source: &'static str,
    /// 逗号分隔的收件人快照。
    pub to_recipients: String,
    pub cc_recipients: String,
    pub subject: String,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub html_bytes: i32,
    /// `SUCCESS` / `FAILED`。
    pub status: &'static str,
    pub error_message: Option<String>,
    pub sent_at: NaiveDateTime,
}
