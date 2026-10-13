//! mail_digest 域表行模型（`T*` = 域 owned 表的**全列** `FromRow` 投影）
//!
//! 仓内惯例：`pub struct T*` 全部 derive `sqlx::FromRow`、全列投影、零例外。
//! 本文件两个 `T*` 分别是本域 owned 的两张表的全列投影：
//! - [`TMailDigestConfig`] ← `t_mail_digest_config`
//! - [`TMailDigestLog`] ← `t_mail_digest_log`
//!
//! ## 反例：统计查询的行**不是** `T*`
//! 邮件正文的每一行是「`t_part` 一行 UNION ALL `t_assembly` 一行」的**读侧自定义列集**
//! （相对 dashboard 侧那份**多取 `drawing_no` / `order_no`**，并把 `customer_id` 换成
//! `LEFT JOIN` 来的 `customer_name`；两张表的列名与类型都不同、装配件额外标
//! `row_type`），本域不拥有
//! `t_part` / `t_assembly`，故那种行结构叫 [`DigestRow`]、**刻意不起 `T*` 名**，
//! 定义在 `repo.rs`（与 `prod::inspection::model` 的窄投影范式一致）。
//! 起 `T*` 名会让「哪一列是端点要的」这件事被「这是本域 owned 表的全列」这个
//! 错误印象掩盖。

use chrono::NaiveDateTime;

/// `t_mail_digest_config` 全列投影（配置单例；按业务只有一行未软删）。
///
/// 数组列（`recipients` / `cc_recipients` / `visible_columns`）是 PG 的
/// `text[]` / `varchar[]`，sqlx 原生支持 `Vec<String>` ↔ 数组，无需自定义类型。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TMailDigestConfig {
    pub id: i64,
    pub version: i32,
    pub enabled: bool,
    /// `varchar(20)`：`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`。
    /// 刻意**存字符串不存枚举**：DB 无 ENUM，写侧的 `Frequency` 枚举由
    /// `service` 层解析（脏值会在那里变成 `21704`，不会静默取默认值）。
    pub frequency: String,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    /// ISO 1=周一 .. 7=周日。
    pub weekday: Option<i16>,
    /// 1..28（避开短月）。
    pub day_of_month: Option<i16>,
    pub recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub include_overdue: bool,
    pub include_upcoming: bool,
    pub upcoming_days: i32,
    pub visible_columns: Vec<String>,
    pub subject_template: Option<String>,
    /// 2026-10-16 新增：发信邮箱（`t_mail_account.id`）；`None` = 未选邮箱。
    ///
    /// ⚠️ **无物理外键**（CLAUDE.md「DB 约定」）。指向已软删邮箱是可能发生的
    /// （`scheduled_task` 域会拦删除，但那一次「查引用 → 删」之间有极窄窗口），
    /// 读侧按 `None` 处理并以 `21701` 暴露。
    pub sender_account_id: Option<i64>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_mail_digest_log` 全列投影（发送流水；**成败都记**、不软删）。
///
/// ⚠️ 本表刻意**没有** `version` / `deleted_at`：它是只增的审计流水，
/// 只会被读（`GET /logs`），永远不被 UPDATE / DELETE —— 加 OCC 列会让
/// 「谁改了这一行」这个问题永远无解，而流水被改本身就是事故。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TMailDigestLog {
    pub id: i64,
    pub config_version: i32,
    /// `SCHEDULED` / `MANUAL`。
    pub trigger_source: String,
    /// 逗号分隔的收件人**快照**（不 join 配置表：配置改了也要看得见当时发给谁）。
    pub to_recipients: String,
    pub cc_recipients: String,
    pub subject: String,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    /// 正文字节数（UTF-8）；正文本体不入库，复现走 `POST /preview`。
    pub html_bytes: i32,
    /// `SUCCESS` / `FAILED`。
    pub status: String,
    pub error_message: Option<String>,
    pub sent_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
}
