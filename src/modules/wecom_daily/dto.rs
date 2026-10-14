//! wecom_daily 域 DTO：入参请求体 + 出参 VO + 表单选项源枚举
//!
//! 出参 VO（[`ConfigOut`] / [`MetaOut`] / [`PreviewOut`] / [`SendResultOut`] /
//! [`LogListOut`]）也放在本文件 —— 理由见文件末尾「为什么没有单独的 vo.rs」。
//!
//! ## 枚举取值域即 DB 的 varchar 取值域
//! 本仓无 DB ENUM（`varchar` + Rust 枚举校验），所以下面四个枚举的 `as_str()` 与
//! `from_str` 必须与 `migrations/20261018000000_001_wecom.sql` 里
//! `frequency` / `worker_metric` / `trigger_source` / `status` 四列的注释对齐：
//! - `frequency`：`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`
//! - `worker_metric`：`QUANTITY` / `BATCH`
//! - `trigger_source`：`SCHEDULED` / `MANUAL`
//! - `t_wecom_daily_log.status`：`SUCCESS` / `FAILED`
//!
//! 解析一律用 `FromStr`（`Err` 带合法值清单），**不用** `serde` 的 `rename_all`
//! 反序列化 —— 后者只能给出「unknown variant」这种不带候选项的信息，运维看到无从下手。
//!
//! ## ⚠️ `worker_metric` 的两个值与迁移注释里的字面量**不同**（已知偏差）
//!
//! 迁移 `20261018000000_001_wecom.sql` 的列注释写的是 `QUANTITY / HOURS`，
//! 而本域落地为 `QUANTITY / BATCH`（条形图按**送检 + 放回的批次数**画）。
//! **迁移是 append-only、不得改写**，而取值域的真源是本文件（无 DB ENUM、未知值在
//! 写侧就被 `21804` 拒）⇒ 代码侧逐字生效，**迁移注释那一行是过时的**。
//! 为什么不是 `HOURS`：本域的日报口径里**没有工时**（有效工时要经工序链上配的
//! `estimated_minutes`，而日报要的是一个当场可算、跨域可对账的量；见
//! `docs/api/wecom-daily.md` §4 口径表）。登记见该文档 §8.3 第 1 条。

use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

// ============================================================================
// 取值范围常量（`GET /meta` 下发给前端，前端一律不硬编码）
// ============================================================================

/// 「未来 N 天交期」窗口天数下限。
pub const UPCOMING_DAYS_MIN: i32 = 1;
/// 「未来 N 天交期」窗口天数上限。
pub const UPCOMING_DAYS_MAX: i32 = 60;
/// 「未来 N 天交期」窗口天数缺省值。
pub const UPCOMING_DAYS_DEFAULT: i32 = 3;

/// 工人榜人数上限下限（`0` = 不限，全列）。
pub const WORKER_TOP_N_MIN: i32 = 0;
/// 工人榜人数上限上限。
pub const WORKER_TOP_N_MAX: i32 = 200;
/// 工人榜人数上限缺省值。
pub const WORKER_TOP_N_DEFAULT: i32 = 15;

/// 单次触发分片数上限的下限。
pub const MAX_CHUNKS_MIN: i16 = 1;
/// ⚠️ **20 是企业微信官方限频**（每个消息推送 ≤ 20 条/分钟），不是随手取的整数。
/// `POST /config` 在写侧就拒 `> 20`（`21804`），见 [`MAX_CHUNKS_MAX`] 的 doc。
pub const MAX_CHUNKS_MAX: i16 = 20;
/// 分片数上限缺省值。
pub const MAX_CHUNKS_DEFAULT: i16 = 3;

/// `GET /logs` 单页最大条数（`limit ∈ [1,200]`）。
pub const LOGS_LIMIT_MAX: i64 = 200;
/// `GET /logs` 缺省单页条数。
pub const LOGS_LIMIT_DEFAULT: i64 = 20;

/// `subject_template` 长度上限（与列 `varchar(200)` 对齐）。
pub const SUBJECT_TEMPLATE_MAX_CHARS: usize = 200;

// ============================================================================
// 频率枚举
// ============================================================================

/// 推送频率（产品决议：只给 4 个预设，**没有**「每隔 N 小时」）。
///
/// ⚠️ **刻意不 import `mail_digest::dto::Frequency`**：两个域各自演进的枚举，
/// 字面量逐字相同但**配套字段的语义由各自的 `GET /meta` 下发**，共用一个常量会在
/// 后端改一侧语义时让另一侧静默漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    /// 每天一次。
    Daily,
    /// 每周一次（`weekday` 生效，ISO 1=周一 .. 7=周日）。
    Weekly,
    /// 每月一次（`day_of_month` 生效，1..28）。
    Monthly,
    /// 仅手动（`POST /send`）；定时任务**不注册** job。
    ManualOnly,
}

impl Frequency {
    /// 全部取值的字面量（`GET /meta` 的 `frequencies[]` 与写侧校验共用这一份）。
    pub const ALL: &'static [Frequency] = &[
        Frequency::Daily,
        Frequency::Weekly,
        Frequency::Monthly,
        Frequency::ManualOnly,
    ];

    /// DB / JSON 的字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "DAILY",
            Self::Weekly => "WEEKLY",
            Self::Monthly => "MONTHLY",
            Self::ManualOnly => "MANUAL_ONLY",
        }
    }

    /// 中文标签（`GET /meta` 的 `label`，前端直接渲染，不在前端另存一份映射）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Daily => "每天",
            Self::Weekly => "每周",
            Self::Monthly => "每月",
            Self::ManualOnly => "仅手动",
        }
    }

    /// 是否需要用户填「几点几分」（MANUAL_ONLY 不需要）。
    pub fn needs_time(self) -> bool {
        !matches!(self, Self::ManualOnly)
    }

    /// 是否需要填「星期几」（仅 WEEKLY）。
    pub fn needs_weekday(self) -> bool {
        matches!(self, Self::Weekly)
    }

    /// 是否需要填「几号」（仅 MONTHLY）。
    pub fn needs_day_of_month(self) -> bool {
        matches!(self, Self::Monthly)
    }
}

impl std::str::FromStr for Frequency {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "DAILY" => Ok(Self::Daily),
            "WEEKLY" => Ok(Self::Weekly),
            "MONTHLY" => Ok(Self::Monthly),
            "MANUAL_ONLY" => Ok(Self::ManualOnly),
            other => Err(AppError::biz(
                code::BIZ_WECOM_CONFIG_INVALID,
                format!("频率取值非法：{other}；合法值只有 DAILY / WEEKLY / MONTHLY / MANUAL_ONLY"),
            )),
        }
    }
}

impl Serialize for Frequency {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

// ============================================================================
// 工人榜指标枚举
// ============================================================================

/// 第四节条形图的度量口径。
///
/// ⚠️ 两个值都**不是工时**（理由见文件头「`worker_metric` 的两个值与迁移注释不同」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerMetric {
    /// 按**送检 + 放回的件数**画条（`SUM(quantity)`）。
    Quantity,
    /// 按**报工批次数**画条（`COUNT(DISTINCT batch_id)`）。
    Batch,
}

impl WorkerMetric {
    /// 全部取值的字面量（`GET /meta` 的 `worker_metrics[]` 与写侧校验共用）。
    pub const ALL: &'static [WorkerMetric] = &[WorkerMetric::Quantity, WorkerMetric::Batch];

    /// DB / JSON 的字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quantity => "QUANTITY",
            Self::Batch => "BATCH",
        }
    }

    /// 中文标签（`GET /meta` 的 `label`；前端直接渲染）。
    ///
    /// ⚠️ 前端有一份**兜底**标签（「按件数」/「按批次数」，见
    /// `DEFAULT_WECOM_DAILY_WORKER_METRIC_OPTIONS`），那份只在 `meta` 还没到达时用；
    /// 真源是本字段。登记见 `docs/api/wecom-daily.md` §8.3 第 2 条。
    pub fn label(self) -> &'static str {
        match self {
            Self::Quantity => "送件数",
            Self::Batch => "报工批次数",
        }
    }

    /// 第四节标题里的口径后缀（「工人报工（送件数）」）。
    pub fn heading_suffix(self) -> &'static str {
        match self {
            Self::Quantity => "送件数",
            Self::Batch => "报工批次数",
        }
    }

    /// 行内数值取该指标对应的量。
    pub fn value_of(self, row: &crate::modules::wecom_daily::dto::WorkerBarRow) -> i32 {
        match self {
            Self::Quantity => row.quantity,
            Self::Batch => row.batch_count,
        }
    }
}

impl std::str::FromStr for WorkerMetric {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "QUANTITY" => Ok(Self::Quantity),
            "BATCH" => Ok(Self::Batch),
            other => Err(AppError::biz(
                code::BIZ_WECOM_CONFIG_INVALID,
                format!("工人榜指标取值非法：{other}；合法值只有 QUANTITY / BATCH"),
            )),
        }
    }
}

impl Serialize for WorkerMetric {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

// ============================================================================
// 触发来源 / 发送结果
// ============================================================================

/// 触发来源（`t_wecom_daily_log.trigger_source`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSource {
    /// cron 定时任务（`task::wecom_daily` 的 job）。
    Scheduled,
    /// 人工点「立即发送」（`POST /api/v2/cron/wecom-daily/send`）。
    Manual,
}

impl TriggerSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "SCHEDULED",
            Self::Manual => "MANUAL",
        }
    }
}

impl std::str::FromStr for TriggerSource {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SCHEDULED" => Ok(Self::Scheduled),
            "MANUAL" => Ok(Self::Manual),
            other => Err(AppError::biz(
                code::BIZ_WECOM_SEND_FAILED,
                format!("触发来源取值非法：{other}；合法值只有 SCHEDULED / MANUAL"),
            )),
        }
    }
}

impl Serialize for TriggerSource {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// 发送结果（`t_wecom_daily_log.status`）。**成败都落库**，故这是流水的主状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    Success,
    Failed,
}

impl SendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failed => "FAILED",
        }
    }
}

impl std::str::FromStr for SendStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SUCCESS" => Ok(Self::Success),
            "FAILED" => Ok(Self::Failed),
            other => Err(AppError::biz(
                code::BIZ_WECOM_SEND_FAILED,
                format!("发送状态取值非法：{other}；合法值只有 SUCCESS / FAILED"),
            )),
        }
    }
}

impl Serialize for SendStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

// ============================================================================
// 请求体
// ============================================================================

/// `POST /api/v2/cron/wecom-daily/config` 请求体：**全量覆写**。
///
/// ⚠️ `version` **必填、无 `#[serde(default)]`**（CLAUDE.md §8 硬约定）：缺失时
/// axum `Json` 提取器直接返 **HTTP 422 纯文本**，不是业务信封。「读-再-比」式隐式
/// OCC 被明令禁止：配置页可能开着很久，隐式 OCC 下「你看到的配置已被别人改过」会
/// 静默成功。
#[derive(Debug, Clone, Deserialize)]
pub struct SaveConfigRequest {
    /// 乐观锁锚点。首次初始化传 `0`（表里无行）。
    pub version: i32,
    #[serde(default)]
    pub enabled: bool,
    pub frequency: String,
    #[serde(default)]
    pub send_hour: Option<i16>,
    #[serde(default)]
    pub send_minute: Option<i16>,
    #[serde(default)]
    pub weekday: Option<i16>,
    #[serde(default)]
    pub day_of_month: Option<i16>,
    /// 推送通道（`t_wecom_webhook.id`，**无物理外键**）。`null` = 未选通道。
    ///
    /// - **JSON string**（与出参 `ConfigOut.webhook_id` 对称，雪花 ID 防 JS 精度截断）。
    /// - **`null` / 缺省 = 未选通道**：新装的库在管理员第一次打开配置页之前就是这个
    ///   状态。此时定时任务**不注册 cron**、`POST /send` 返 `21801`。
    /// - 指向不存在**或已软删**的 webhook ⇒ `21806 BIZ_WECOM_WEBHOOK_NOT_FOUND`
    ///   （写侧校验，不等到推送时才发现）。
    ///
    /// ⚠️ **全仓没有任何「回落到 `is_default`」的分支**：没选通道就是没选，前端的
    /// 选择器必须让用户显式选一条。
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_opt"
    )]
    pub webhook_id: Option<i64>,
    #[serde(default = "default_upcoming_days")]
    pub upcoming_days: i32,
    #[serde(default = "default_worker_metric")]
    pub worker_metric: String,
    #[serde(default = "default_worker_top_n")]
    pub worker_top_n: i32,
    #[serde(default = "default_max_chunks")]
    pub max_chunks: i16,
    #[serde(default = "default_true")]
    pub include_overview: bool,
    #[serde(default = "default_true")]
    pub include_delivery: bool,
    #[serde(default = "default_true")]
    pub include_orders: bool,
    #[serde(default = "default_true")]
    pub include_workers: bool,
    #[serde(default)]
    pub subject_template: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_upcoming_days() -> i32 {
    UPCOMING_DAYS_DEFAULT
}

fn default_worker_metric() -> String {
    WorkerMetric::Quantity.as_str().to_string()
}

fn default_worker_top_n() -> i32 {
    WORKER_TOP_N_DEFAULT
}

fn default_max_chunks() -> i16 {
    MAX_CHUNKS_DEFAULT
}

/// `POST /preview` / `POST /send` 请求体：**配置的部分覆盖**。
///
/// - **不含 `version`**：这两个端点**不改配置**（覆盖值不落库），OCC 对它们没有意义。
/// - 全部字段 `#[serde(default)]`：空体 `{}` = 完全用当前配置，语义最自然。
/// - ⚠️ **刻意不含 `webhook_id`**（照 `mail_digest::dto::PreviewRequest` 同一理由）：
///   `/preview` 不推送，发信通道与渲染无关；`/send` 确实推送，但「推到哪个群」是一条
///   **配置**决策，不该被「预览一次」这种临时覆盖顺带改掉（否则 `/send` 的覆盖就成了
///   绕过配置页直接换通道的后门）。要换通道请走 `POST /config`。
///   serde 默认**忽略**未知键 ⇒ 前端带上它也无妨。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreviewRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub frequency: Option<String>,
    #[serde(default)]
    pub send_hour: Option<i16>,
    #[serde(default)]
    pub send_minute: Option<i16>,
    #[serde(default)]
    pub weekday: Option<i16>,
    #[serde(default)]
    pub day_of_month: Option<i16>,
    #[serde(default)]
    pub upcoming_days: Option<i32>,
    #[serde(default)]
    pub worker_metric: Option<String>,
    #[serde(default)]
    pub worker_top_n: Option<i32>,
    #[serde(default)]
    pub max_chunks: Option<i16>,
    #[serde(default)]
    pub include_overview: Option<bool>,
    #[serde(default)]
    pub include_delivery: Option<bool>,
    #[serde(default)]
    pub include_orders: Option<bool>,
    #[serde(default)]
    pub include_workers: Option<bool>,
    #[serde(default)]
    pub subject_template: Option<String>,
}

/// `GET /logs` query：`limit ∈ [1,200]` + `offset ≥ 0`。
#[derive(Debug, Clone, Deserialize)]
pub struct LogListQuery {
    #[serde(default = "default_logs_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_logs_limit() -> i64 {
    LOGS_LIMIT_DEFAULT
}

// ============================================================================
// 归一形状（读侧 / 写侧 / 模板 / 调度共用的唯一形态）
// ============================================================================

/// 配置的**规范化形态**：读侧（DB 行）与写侧（请求体）共用的中间表示
/// （已过校验、已清无用字段）。
///
/// 有了它，`template::render` / `schedule::to_cron` / `service::fetch_rows` 三处
/// 拿到的永远是同一份形状，DB 行 / 请求体 / 覆盖后配置这三种来源不会在下游
/// 反复互转。
///
/// ⚠️ **刻意不含 `webhook_configured`**：那是「DB 里那条行还在不在」的**读侧派生**，
/// 归 [`ConfigOut`] 在读的时候算；放进归一形状会让「临时覆盖 + 派生」两件事混在
/// 一起，且 [`Default`]（`GET /meta` 的 `defaults` 段用它）会凭空带出一个恒 false
/// 的布尔。
#[derive(Debug, Clone)]
pub struct NormalizedConfig {
    pub enabled: bool,
    pub frequency: Frequency,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
    /// 推送通道 id；`None` = 未选通道（定时任务不注册 job、`/send` 返 `21801`）。
    pub webhook_id: Option<i64>,
    pub upcoming_days: i32,
    pub worker_metric: WorkerMetric,
    pub worker_top_n: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    pub subject_template: Option<String>,
}

impl Default for NormalizedConfig {
    /// 缺省配置（`GET /config` 在表空时返的、`GET /meta` 的 `defaults` 段、
    /// `POST /config` 首次初始化前的基准，三者共用这一份）。
    ///
    /// ⚠️ `webhook_id` 刻意是 **`None`** 而不是「默认 webhook」：全仓没有回落到
    /// `is_default` 的分支，而缺省就替用户选一条会让「还没配通道」在配置页上看不出来
    /// （页面一切正常，只有真推送时才炸）。显式选一次反而是更早暴露问题的形态。
    /// `enabled` 缺省 `false`（少发一条永远好过多发一条）。
    fn default() -> Self {
        Self {
            enabled: false,
            frequency: Frequency::Daily,
            send_hour: Some(8),
            send_minute: Some(0),
            weekday: None,
            day_of_month: None,
            webhook_id: None,
            upcoming_days: UPCOMING_DAYS_DEFAULT,
            worker_metric: WorkerMetric::Quantity,
            worker_top_n: WORKER_TOP_N_DEFAULT,
            max_chunks: MAX_CHUNKS_DEFAULT,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
            subject_template: None,
        }
    }
}

// ============================================================================
// 出参
// ============================================================================

/// `GET /config` / `POST /config` 响应体（**21 个字段**，前端 `.strict()` + 键数断言）。
///
/// `id` 是 JSON **string**（雪花 ID 防 JS 精度截断）。
#[derive(Debug, Clone, Serialize)]
pub struct ConfigOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub enabled: bool,
    pub frequency: String,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub webhook_id: Option<i64>,
    pub upcoming_days: i32,
    pub worker_metric: String,
    pub worker_top_n: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    pub subject_template: Option<String>,
    /// `webhook_id` 指向的行**还在不在**（纯 DB 存在性检查）。
    ///
    /// ⚠️ **刻意不解密**（与 `wecom::service::target_is_available` 不同，后者会真的
    /// 解一次）：把「主密钥被换过导致存量密文全解不开」显示成「通道没配置」，会让
    /// 管理员去建一条新 webhook 而不是找运维换回密钥 —— 那是**方向性错误**的提示。
    /// 解密失败那一类只在真推送时以 `21801` 暴露。
    pub webhook_configured: bool,
    /// naive timestamp（Asia/Shanghai），格式 `YYYY-MM-DD HH:MM:SS`。
    pub created_at: String,
    pub updated_at: String,
}

/// `GET /meta` 的 `defaults` 段：**不含** `id` / `version` / 两个时间戳 /
/// `webhook_configured`；`webhook_id` 恒为 `null`。
#[derive(Debug, Clone, Serialize)]
pub struct ConfigDefaultsOut {
    pub enabled: bool,
    pub frequency: String,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub webhook_id: Option<i64>,
    pub upcoming_days: i32,
    pub worker_metric: String,
    pub worker_top_n: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    pub subject_template: Option<String>,
}

impl From<&NormalizedConfig> for ConfigDefaultsOut {
    fn from(c: &NormalizedConfig) -> Self {
        Self {
            enabled: c.enabled,
            frequency: c.frequency.as_str().to_string(),
            send_hour: c.send_hour,
            send_minute: c.send_minute,
            weekday: c.weekday,
            day_of_month: c.day_of_month,
            webhook_id: c.webhook_id,
            upcoming_days: c.upcoming_days,
            worker_metric: c.worker_metric.as_str().to_string(),
            worker_top_n: c.worker_top_n,
            max_chunks: c.max_chunks,
            include_overview: c.include_overview,
            include_delivery: c.include_delivery,
            include_orders: c.include_orders,
            include_workers: c.include_workers,
            subject_template: c.subject_template.clone(),
        }
    }
}

/// `GET /meta` 的频率选项。
#[derive(Debug, Clone, Serialize)]
pub struct FrequencyOption {
    pub value: String,
    pub label: String,
    pub needs_time: bool,
    pub needs_weekday: bool,
    pub needs_day_of_month: bool,
}

/// `GET /meta` 的工人榜指标选项。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerMetricOption {
    pub value: String,
    pub label: String,
}

/// `GET /meta` 的取值区间（前端输入项的 `:min/:max` 与表单越界校验全部读它）。
#[derive(Debug, Clone, Serialize)]
pub struct RangeOut {
    pub min: i32,
    pub max: i32,
}

/// `GET /meta` 响应体（**6 个键**：内容与调度的全部表单判据都在这一个端点里）。
///
/// ⚠️ **三个 range 与四个 `needs_*` 全部由本端下发，前端不许硬编码**。
#[derive(Debug, Clone, Serialize)]
pub struct MetaOut {
    pub frequencies: Vec<FrequencyOption>,
    pub worker_metrics: Vec<WorkerMetricOption>,
    pub defaults: ConfigDefaultsOut,
    pub upcoming_days_range: RangeOut,
    pub worker_top_n_range: RangeOut,
    pub max_chunks_range: RangeOut,
}

/// 第四节的一个工人（结构化数据，供预览弹窗的数字核对）。
///
/// ⚠️ **条形图不在这一层**：字符条（`█` / `░`）由 [`crate::modules::wecom_daily::template`]
/// 直接渲染进 `chunks[]` 的 markdown 文本，前端只展示文本、不引入任何图表组件。
/// 两条指标（`batch_count` / `quantity`）**都返回** ⇒ 前端展示的那一份不受
/// `worker_metric` 影响。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkerBarRow {
    pub worker_name: String,
    pub badge_code: String,
    pub batch_count: i32,
    pub quantity: i32,
}

/// `POST /preview` 响应体（**15 个字段**）：渲染、不发送、不落 log。
///
/// ⚠️ **没有 `markdown` 单字段**，只有 `chunks: Vec<String>` —— 同时维护两份会让
/// 「要发几条」有两个答案，而那恰恰是管理员唯一需要预判的东西。
#[derive(Debug, Clone, Serialize)]
pub struct PreviewOut {
    pub subject: String,
    /// 将按顺序发送的 N 条，逐条 ≤ 4096 **字节**（不是字符数：中文一条消息按 UTF-8
    /// 占 3 字节，字符数会低估到三分之一）。
    pub chunks: Vec<String>,
    /// 冗余字段，恒等于 `chunks.len()`（后端显式下发，省得前端自己数）。
    pub chunk_count: i32,
    pub overview_batch_count: i32,
    pub overview_quantity: i32,
    pub completed_count: i32,
    pub in_process_count: i32,
    pub repair_part_count: i32,
    pub note_count: i32,
    pub note_part_count: i32,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub worker_rows: Vec<WorkerBarRow>,
    /// 全量工人行数（可能 > `chunks` 里出现的条数：`worker_top_n > 0` 时挡掉了尾部）。
    pub worker_row_total: i32,
    /// 本次实际生效的节（`overview` / `delivery` / `orders` / `workers`）。
    pub sections: Vec<String>,
}

/// `POST /send` 的响应体 = 流水行视图（与 `LogListOut.items[]` 是**同一个结构体**，
/// wire 上不可区分，照 `mail_digest` 的既有做法）。
#[derive(Debug, Clone, Serialize)]
pub struct SendResultOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub log_id: i64,
    pub config_version: i32,
    pub trigger_source: TriggerSource,
    pub status: SendStatus,
    pub webhook_name: String,
    pub webhook_masked: String,
    /// 统计基准日（`YYYY-MM-DD`）。⚠️ **恒等于 `sent_at` 的日期**（同一个
    /// `now_naive()` 调用取值）⇒ 前端**刻意不建「统计日」列**，理由见
    /// `docs/api/wecom-daily.md` §8「与前端的口径分叉登记」。
    pub report_date: String,
    /// **全部片的字节数之和**（不是字符数、不是单片长度）。
    pub markdown_bytes: i32,
    /// 本次渲染出的分片数。`0` 是合法值 = 准备阶段就失败、还没渲染。
    pub chunk_count: i32,
    /// 实际发出去了几条（失败时 < `chunk_count`）。
    pub chunk_sent: i32,
    pub overview_batch_count: i32,
    pub overview_quantity: i32,
    pub note_count: i32,
    pub note_part_count: i32,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub worker_row_total: i32,
    pub error_message: Option<String>,
    pub sent_at: String,
}

/// `GET /logs` 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct LogListOut {
    pub items: Vec<SendResultOut>,
    /// 匹配总数，**不受** `limit` 截断影响。
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 第四节节名（`PreviewOut.sections[]` 的取值；四个开关各自一个）。
pub const SECTION_OVERVIEW: &str = "overview";
/// 第二节节名。
pub const SECTION_DELIVERY: &str = "delivery";
/// 第三节节名。
pub const SECTION_ORDERS: &str = "orders";
/// 第四节节名。
pub const SECTION_WORKERS: &str = "workers";

// ============================================================================
// 为什么没有单独的 vo.rs
//
// 出参 VO 就在本文件：本域只有两张表、五个出参结构，入参四个、两个枚举。拆一个
// `vo.rs` 只会让读代码的人多一次「该 import 哪个路径」的判断（照
// `mail_digest::dto` 与 `wecom::dto` 的同款取舍）。
// ============================================================================
