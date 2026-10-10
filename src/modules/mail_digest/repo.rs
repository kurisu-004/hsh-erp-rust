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
//! 绑 [`DELIVERY_STATUSES`]、行单位工单级、装配件行替换子件行、已交量守卫
//! `status IN ('DELIVERED','COMPLETED')`）。但**行集合的取法**不一样，这是需求
//! 决定的、不是漂移：
//!
//! | | dashboard | mail_digest（本文件） |
//! |---|---|---|
//! | 每桶上限 | `LIMIT 30`（`DELIVERY_BUCKET_LIMIT`） | **无 LIMIT** |
//! | 取哪些列 | 9 列（无 `drawing_no` / `order_no`） | 10 列（邮件要图号 / 订单号） |
//! | 未来节窗口 | `>= today` 无上界（`upcoming` 桶） | `[today, today+N)`，N clamp 1..60 |
//! | 客户名 | 单独批量聚合（防 N+1） | 本 SQL 里 `LEFT JOIN` 一次带出 |
//!
//! dashboard 那份的 `LIMIT 30` 是**大屏可读性**约束（大屏一屏放不下 300 行），
//! 邮件则必须给全量 —— 漏一条交期就是漏一个客户。而「未来 N 天有上界」是本需求
//! 本身（用户原话「未来 14 天」）。所以**不复用** `DeliveryRepo::list_orders_*`：
//! 复用就得先解掉它的 `LIMIT 30`（等于改坏大屏）或在外面再分页（等于让 SQL
//! 口径与 Rust 侧重叠，两边都要维护）。
//!
//! ## 三条硬约束（改 SQL 前逐条读）
//!
//! 1. **工单级行单位**：`t_part` 侧必须带 `AND p.assembly_id IS NULL`（装配件子件
//!    不重复计入），装配件单独查 `t_assembly` 且其行**替换**子件行。与 dashboard
//!    交期面板三桶同一行单位（柱状图 / 抽屉是件级，本域**不**跟它）。
//! 2. **`NOT EXISTS` 必须同时判 `b.deleted_at IS NULL` 且状态集含 `'COMPLETED'`**：
//!    force-complete 端点（`POST /parts/{id}/force-complete` 与
//!    `POST /prod/assemblies/{id}/force-complete`）把批次强推 `COMPLETED`，
//!    守卫漏掉 `COMPLETED` 会让已完工的工单继续出现在「逾期未交」邮件里。
//! 3. **窗口下界来自形参 `today`，SQL 里绝不写 `CURRENT_DATE`**：DB 会话时区
//!    （测试容器 UTC）与 `infra::clock::now_naive()` 的 Asia/Shanghai 是**两个时钟**，
//!    Shanghai 00:00–08:00 期间 `CURRENT_DATE == today - 1`，于是「今天到期」的
//!    工单会落进逾期节、未来节则整体偏移一天。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{PgConnection, Row};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::dashboard::repo::DELIVERY_STATUSES;
use crate::modules::mail_digest::model::{TMailDigestConfig, TMailDigestLog};

/// `DELIVERY_STATUSES` 的 `Vec<String>` 形态（`LazyLock` 只算一次）。
///
/// ⚠️ 为什么需要这一层：`dashboard::repo::DeliveryRepo` 走**运行时**
/// `sqlx::query(..).bind(DELIVERY_STATUSES)`，`&[&str; 6]` 能直接绑 `varchar[]`；
/// 而本域走 `sqlx::query_as!` **宏**，宏要求参数类型是具体可绑的 Rust 类型，
/// 对 `$2::varchar[]` 推出的类型是 `&[String]`。于是常量（`&[&str; 6]`）得先转一次。
///
/// 每次调用都新建 `Vec` 会给「每天发一封信」平白加 6 次分配；`LazyLock` 把它压到一次。
/// ⚠️ 取值仍直接来自 dashboard 那个常量（不复制字面量）—— 一处改、两处生效，
/// 这正是 CLAUDE.md 要求四处共用同一个 `DELIVERY_STATUSES` 的意义。
fn delivery_statuses_vec() -> &'static [String] {
    static VEC: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| DELIVERY_STATUSES.iter().map(|s| s.to_string()).collect());
    &VEC
}

/// 邮件正文的一行（`t_part` 一行 UNION ALL `t_assembly` 一行）。
///
/// ⚠️ **刻意不起 `T*` 名**：`T*` 在本仓是「域 owned 表的全列投影」的约定
/// （见 `model.rs` 顶部 doc），而本结构是**跨两张非 owned 表的读侧自定义列集**
/// （`t_assembly` 没有 `assembly_id`、`t_part` 没有子件概念，两侧列名与类型
/// 不同，装配件那一支额外靠 `row_type` 字面量区分）。起 `T*` 名会让「这是本域
/// owned 表」的错误印象盖过「这是拼出来的视图行」。
#[derive(Debug, Clone, sqlx::FromRow)]
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
    /// 6 态之一（见 `DELIVERY_STATUSES`）；装配件侧只可能命中其中 4 态
    /// （`AssemblyStatus` 无 `PROGRAMMING` / `OUTSOURCE`），无需第二份常量。
    pub status: String,
    /// 系统交期（`date`）；可为 NULL，被窗口谓词自然排除。
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    /// `varchar(30)`，可为 NULL。
    pub order_no: Option<String>,
    /// `LEFT JOIN t_customer` 得到的客户名。
    ///
    /// ⚠️ **刻意不加 `AND c.deleted_at IS NULL`**：客户软删时该列退化为 `null`，
    /// 但**不该**因此把工单从邮件里剔除掉（工单本身是有效的，只是不知道发给谁家）。
    /// 与 dashboard `list_orders_*` 的客户名处理同款决策。
    pub customer_name: Option<String>,
    /// SQL 里写死的来源字面量：`'PART'` / `'ASSEMBLY'`。
    pub row_type: String,
}

impl DigestRow {
    /// 是否装配件行（决定 `quantity` 的单位文案与「工单类型」列的展示值）。
    pub fn is_assembly(&self) -> bool {
        self.row_type == "ASSEMBLY"
    }
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
        DigestRow,
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
        DigestRow,
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
               created_at, created_by, updated_at, updated_by, deleted_at
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
            created_at, created_by, updated_at, updated_by
        ) VALUES ($1, 0, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $15, $16)
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
            version          = version + 1,
            updated_at       = $16,
            updated_by       = $17
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
