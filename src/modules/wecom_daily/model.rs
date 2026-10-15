//! wecom_daily 域表行模型（`T*` = 域 owned 表的**全列** `FromRow` 投影）
//!
//! 仓内惯例：`pub struct T*` 全部 derive `sqlx::FromRow`、全列投影、零例外。
//! 本域 owned 的表两张：
//! - [`TWeComDailyConfigRow`] ← `t_wecom_daily_config`
//! - [`TWeComDailyLogRow`] ← `t_wecom_daily_log`
//!
//! ## 统计查询的行**不是** `T*`
//! 第一节 / 第二节 / 第四节的聚合行是跨表聚合的读侧投影（`t_part_event` 一条语句
//! 四个条件聚合、送货单一条语句两个聚合、工人榜一个 `GROUP BY`），本域**不拥有**
//! 那些表，故那些行结构定义在 `repo.rs`、**刻意不起 `T*` 名**（起 `T*` 名会让
//! 「这是本域 owned 表的全列」的错误印象盖过「这是聚合出来的」）。

use chrono::{NaiveDate, NaiveDateTime};

/// `t_wecom_daily_config` 全列投影（配置**单例**；按业务只有一行未软删）。
///
/// ⚠️ **无物理外键**（CLAUDE.md「DB 约定」）：`webhook_id` 指向
/// `t_wecom_webhook.id` 是裸 bigint，指向已软删 webhook 是可能发生的（那一侧拦
/// 删除，但「查引用 → 删」之间有极窄 TOCTOU 窗口），读侧按 `None` 处理并以
/// `21801` / `21806` 暴露。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWeComDailyConfigRow {
    pub id: i64,
    pub version: i32,
    pub enabled: bool,
    /// `varchar(20)`：`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`。
    /// 刻意**存字符串不存枚举**：DB 无 ENUM，写侧的 `Frequency` 枚举由 `service`
    /// 层解析（脏值会在那里变成 `21804`，不会静默取默认值）。
    pub frequency: String,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    /// ISO 1=周一 .. 7=周日。
    pub weekday: Option<i16>,
    /// 1..28（避开短月）。
    pub day_of_month: Option<i16>,
    /// `t_wecom_webhook.id`（**无物理外键**）；`None` = 未选通道。
    pub webhook_id: Option<i64>,
    pub upcoming_days: i32,
    /// `varchar(10)`：`QUANTITY` / `BATCH`。
    ///
    /// ⚠️ **迁移的列注释写的是 `QUANTITY / HOURS`**，是过时的（见 `dto.rs` 文件头
    /// 「`worker_metric` 的两个值与迁移注释里的字面量不同」）。取值域的真源是
    /// `dto::WorkerMetric`，未知值在写侧就被 `21804` 拒。
    pub worker_metric: String,
    /// `0` = 不限（全列）。
    pub worker_top_n: i32,
    /// 第三节交期清单每组最多列出的工单数；`0` = 不限。
    pub order_list_limit: i32,
    /// 分片数上限；`1..=20`（20 是企业微信官方限频）。
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    /// 第三节清单是否带图纸预览直链。
    pub include_drawings: bool,
    pub subject_template: Option<String>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_wecom_daily_log` 全列投影（推送流水；**成败都记**、不软删）。
///
/// ⚠️ 本表刻意**没有** `version` / `deleted_at`：它是只增的审计流水，只会被读
/// （`GET /logs`），永远不被 UPDATE / DELETE —— 加 OCC 列会让「谁改了这一行」永远
/// 无解，而流水被改本身就是事故。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TWeComDailyLogRow {
    pub id: i64,
    pub config_version: i32,
    /// `SCHEDULED` / `MANUAL`。
    pub trigger_source: String,
    /// webhook **名称**快照（不 join 通道表：配置改了也要看得见当时发到哪个群）。
    pub webhook_name: String,
    /// webhook 地址**掩码**快照（只留可辨识片段，不含 key 全值）。
    pub webhook_masked: String,
    /// 统计基准日。⚠️ 恒等于 `sent_at` 的日期（同一个 `now_naive()` 取值）。
    pub report_date: NaiveDate,
    /// **全部片**的字节数之和。
    pub markdown_bytes: i32,
    /// 渲染出的分片数；`0` = 准备阶段就失败（合法值）。
    pub chunk_count: i16,
    /// 实发分片数。
    pub chunk_sent: i16,
    pub overview_batch_count: i32,
    pub overview_quantity: i32,
    /// 第二节那个数：本期出货的**去重工单数**（`COUNT(DISTINCT part_id)`）。
    ///
    /// ⚠️ **2026-10-23 起语义变更**：此前该列记的是「本期出货的送货单张数」，同日
    /// 的 `note_part_count` 才是去重工单数；第二节改单值口径后 `note_part_count`
    /// 那一列已下线（迁移 `20261023010000_001`），本列承载现在这个数。**历史行按旧
    /// 语义理解**，见该迁移的列注释。
    pub note_count: i32,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    /// 工人榜行数（`worker_top_n` 挡掉尾部**之前**的真实行数）。
    pub worker_row_total: i32,
    /// `SUCCESS` / `FAILED`。
    pub status: String,
    pub error_message: Option<String>,
    pub sent_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
}
