//! mail_digest 域 DTO：入参请求体 + 表单选项源枚举
//!
//! 出参 VO 也放在本文件（`ConfigOut` / `PreviewOut` / `SendResultOut` / `LogListOut`
//! / `MetaOut` …）—— 理由见文件末尾「为什么没有单独的 vo.rs」。
//!
//! ## 三态枚举的取值域即 DB 的 varchar 取值域
//! 本仓无 DB ENUM（`varchar` + Rust 枚举校验），所以下面三个枚举的 `as_str()`
//! 与 `from_str` 必须与 `migrations/20261013000000_001_mail_digest.sql` 里
//! `frequency` / `trigger_source` / `status` 三列的注释**逐字对齐**：
//! - `frequency`：`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`
//! - `trigger_source`：`SCHEDULED` / `MANUAL`
//! - `t_mail_digest_log.status`：`SUCCESS` / `FAILED`
//!
//! 解析一律用 `FromStr`（`Err` 带合法值清单），**不用 `serde` 的 `rename_all` 反序列化**
//! ——后者只能给出「unknown variant」这种不带候选项的信息，运维看到无从下手。

use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

/// 未来节窗口天数下限（与 `dashboard::dto::DASHBOARD_MIN_DAYS` 同档）。
pub const UPCOMING_DAYS_MIN: i32 = 1;
/// 未来节窗口天数上限（与 `dashboard::dto::DASHBOARD_MAX_DAYS` 同档）。
pub const UPCOMING_DAYS_MAX: i32 = 60;

/// 未来节窗口天数缺省值。
pub const UPCOMING_DAYS_DEFAULT: i32 = 14;

/// `GET /logs` 单页最大条数（`limit∈[1,200]`）。
pub const LOGS_LIMIT_MAX: i64 = 200;
/// `GET /logs` 缺省单页条数。
pub const LOGS_LIMIT_DEFAULT: i64 = 20;

// ============================================================================
// 频率枚举
// ============================================================================

/// 发送频率（产品决议：只给 4 个预设，**没有**「每隔 N 小时」）。
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
                code::BIZ_MAIL_CONFIG_INVALID,
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
// 状态中文（2026-10-16 新增）
// ============================================================================

/// 订单状态的中文标签（邮件正文「状态」列用）。
///
/// ⚠️ **人工同步关系分两类**（Rust / TS 之间没有任何编译期约束）：① **两份必须逐字
/// 一致** —— 本文件与前端 `src/types/parts.ts::ORDER_STATUS_LABEL`；② **一份刻意不同**
/// —— 前端 `src/constants/partStatus.ts::STATUS_LABEL` 是**统计**语义（另一套文案），
/// **不要**把它当依据、也不要与本表混用。任一处漂了都不会编译失败，只会让同一批件在
/// 不同页面显示两种中文状态。登记见 `docs/api/mail_digest.md` §5 与 §8.3。
///
/// ⚠️ `REPAIRING` 已于 2026-10-01 从状态枚举删除（降级为 `t_part_batch.is_repairing`
/// 布尔列），但 `shared::batch::status_machine` 仍保留一个过渡兼容分支能解析该字面量，
/// 故本表**保留**这一行（存量行仍可能读出这个值）。
pub fn part_status_label(status: &str) -> &str {
    match status {
        "PENDING" => "待生产",
        "PROGRAMMING" => "编程中",
        "IN_PROCESS" => "生产中",
        "INSPECTION" => "待品检",
        "READY_TO_SHIP" => "待送货",
        "DELIVERED" => "已送货",
        "REPAIRING" => "返修中",
        "OUTSOURCE" => "外协中",
        "COMPLETED" => "已完成",
        "CANCELLED" => "已取消",
        // 未知值**原样回显**，不静默变空串：静默空串会让「这批件状态坏了」这件事
        // 在邮件里完全看不出来（收件人只会以为没填状态）。
        other => other,
    }
}

/// 批次行的状态标签。
///
/// ⚠️ **与工单行不对称**：`t_part_batch.is_repairing` 这个布尔列只存在于批次表，
/// `t_part` / `t_assembly` 上没有对应列 ⇒ 批次行能显示「返修中」，工单行**不能**
/// （只能查 `status` 表）。这是两个表结构的事实差异，不是漏实现。
/// 也就是说：一行返修中的件，工单行显示「生产中」而它下面的批次子行显示「返修中」。
pub fn batch_status_label(status: &str, is_repairing: bool) -> &str {
    if is_repairing {
        "返修中"
    } else {
        part_status_label(status)
    }
}

/// 批次位置的展示文案，**逐字对齐前端**
/// `src/views/parts/list/partsListColumnDefs.ts` 的位置列渲染：
/// `货架 {code}` / `品检 {code}` / `{工人姓名}` / `外协 {公司名}` / 解析不出时 `—`。
///
/// ⚠️ `WORKER` **不加前缀**（工人姓名本身自解释），与其余三档不同，这是前端的既有
/// 口径、不是遗漏。
pub fn batch_location_label(location: Option<&str>, holder_name: Option<&str>) -> String {
    let Some(holder) = holder_name else {
        return "—".to_string();
    };
    match location {
        Some("PRODUCTION_SHELF") => format!("货架 {holder}"),
        Some("INSPECTION_SHELF") => format!("品检 {holder}"),
        Some("WORKER") => holder.to_string(),
        Some("OUTSOURCE_COMPANY") => format!("外协 {holder}"),
        // `OFFICE` / 未知 location / holder 已被软删（名字解析不出来）⇒ 同样给 `—`
        _ => "—".to_string(),
    }
}

// ============================================================================
// 列白名单枚举
// ============================================================================

/// 正文表格可选列（内容可选 ②）。
///
/// ⚠️ **这是封闭枚举**：新增列必须同时改①本枚举 ②`template.rs` 的取值函数
/// ③`GET /meta` 的输出（后两者都由本枚举派生，改一处即可）。未知列一律
/// `21704 BIZ_MAIL_CONFIG_INVALID`，**不静默丢弃** —— 配置错误应当当场暴露，
/// 静默丢弃会让人以为列已经生效。真正的解析入口是本文件的 [`parse_visible_columns`]
/// （写侧 `service::validate_save_request` 与读侧 `service::normalize_row` 都走它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleColumn {
    /// 序列号。工单行取工单自己的；**批次子行取子件的**（装配件支）或留空
    /// （散件支，见 `repo::DigestBatchRow::part_serial_no`）。2026-10-16 起子行也填这一列。
    SerialNo,
    DrawingNo,
    Name,
    /// 批次号（**批次子行**才有值，工单行留空）。2026-10-16 新增。
    BatchNo,
    Quantity,
    CustomerName,
    Status,
    /// 批次所在位置（**批次子行**才有值，工单行留空）。2026-10-16 新增。
    Location,
    SystemDeliveryDate,
    RemainingDays,
    RowType,
    IsUrgent,
    OrderNo,
}

impl VisibleColumn {
    /// 全量白名单，顺序 = `GET /meta` 的展示顺序 = 缺省配置顺序。
    pub const ALL: &'static [VisibleColumn] = &[
        VisibleColumn::SerialNo,
        VisibleColumn::DrawingNo,
        VisibleColumn::Name,
        VisibleColumn::BatchNo,
        VisibleColumn::Quantity,
        VisibleColumn::CustomerName,
        VisibleColumn::Status,
        VisibleColumn::Location,
        VisibleColumn::SystemDeliveryDate,
        VisibleColumn::RemainingDays,
        VisibleColumn::RowType,
        VisibleColumn::IsUrgent,
        VisibleColumn::OrderNo,
    ];

    /// 缺省配置使用的列（= `ALL` 中**除 `row_type` / `is_urgent` / `order_no` 之外的
    /// 全部**）。
    ///
    /// ⚠️ **刻意按名描述、不写「`ALL` 的前 N 个」**：`DEFAULT_SET` 是显式列表、不依赖枚举
    /// 顺序，往 `ALL` 中段插一个新变体只会让那种措辞变陈旧、不会改变缺省集本身。个数另有
    /// 机器钉住（`tests/mail_digest/preview.rs` 的 `Some(10)`、`template.rs` 的
    /// `ALL.len() == 13`），完整四份副本清单与各自同步方式见
    /// `docs/api/mail_digest.md` §2.3。
    ///
    /// ⚠️ **改这一处必须新开一条迁移**同时改列 DEFAULT 与**回填存量行**
    /// （2026-10-16 已按此办理，迁移见
    /// `migrations/20261016000001_001_mail_digest_default_columns.sql`）：迁移是
    /// append-only、不会被改写，已经建过表的环境不会跟着变 —— 只改 Rust 常量的话，
    /// 新库与老库的缺省列会静默分叉。回填会**改掉管理员已存的勾选**（刻意决定，
    /// 理由与后果见 `docs/api/mail_digest.md` §8.3）。
    pub const DEFAULT_SET: &'static [VisibleColumn] = &[
        VisibleColumn::SerialNo,
        VisibleColumn::DrawingNo,
        VisibleColumn::Name,
        VisibleColumn::BatchNo,
        VisibleColumn::Quantity,
        VisibleColumn::CustomerName,
        VisibleColumn::Status,
        VisibleColumn::Location,
        VisibleColumn::SystemDeliveryDate,
        VisibleColumn::RemainingDays,
    ];

    /// DB / JSON 的字面量（与 migration 的 `visible_columns` 列取值域一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SerialNo => "serial_no",
            Self::DrawingNo => "drawing_no",
            Self::Name => "name",
            Self::BatchNo => "batch_no",
            Self::Quantity => "quantity",
            Self::CustomerName => "customer_name",
            Self::Status => "status",
            Self::Location => "location",
            Self::SystemDeliveryDate => "system_delivery_date",
            Self::RemainingDays => "remaining_days",
            Self::RowType => "row_type",
            Self::IsUrgent => "is_urgent",
            Self::OrderNo => "order_no",
        }
    }

    /// 中文表头（邮件正文与前端预览共用，前端不另存一份映射）。
    pub fn label(self) -> &'static str {
        match self {
            Self::SerialNo => "序列号",
            Self::DrawingNo => "图号",
            Self::Name => "名称",
            Self::BatchNo => "批次",
            Self::Quantity => "数量",
            Self::CustomerName => "客户",
            Self::Status => "状态",
            Self::Location => "位置",
            Self::SystemDeliveryDate => "系统交期",
            Self::RemainingDays => "剩余天数",
            Self::RowType => "工单类型",
            Self::IsUrgent => "是否紧急",
            Self::OrderNo => "订单号",
        }
    }

    /// 该列的**列宽权重**（2026-10-16 新增，需求「表格列要对齐」）。
    ///
    /// ⚠️ 是**权重**不是百分比：实际百分比由 [`crate::modules::mail_digest::template`]
    /// 按「当次实际渲染的列集合」归一化后取整，最后一列吸收余数保证合计恒 100。
    /// 语义分配的理由（窄列 = 定长码 / 计数；宽列 = 自由文本）见 template.rs 的
    /// 「列宽表」注释。
    pub fn width_weight(self) -> u32 {
        match self {
            Self::SerialNo => 7,
            Self::DrawingNo => 10,
            Self::Name => 14,
            Self::BatchNo => 5,
            Self::Quantity => 5,
            Self::CustomerName => 10,
            Self::Status => 6,
            Self::Location => 11,
            Self::SystemDeliveryDate => 9,
            Self::RemainingDays => 6,
            Self::RowType => 6,
            Self::IsUrgent => 5,
            Self::OrderNo => 6,
        }
    }

    /// 由字面量解析。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == raw)
    }
}

/// 由字符串切片解析成列白名单；任一项未知即返 `21704`（含候选项清单）。
pub fn parse_visible_columns(raw: &[String]) -> Result<Vec<VisibleColumn>, AppError> {
    let mut out = Vec::with_capacity(raw.len());
    for key in raw {
        match VisibleColumn::parse(key) {
            Some(col) => out.push(col),
            None => {
                let legal: Vec<&str> = VisibleColumn::ALL.iter().map(|c| c.as_str()).collect();
                return Err(AppError::biz(
                    code::BIZ_MAIL_CONFIG_INVALID,
                    format!("未知的表格列 `{key}`；合法列名只有 {}", legal.join(" / ")),
                ));
            }
        }
    }
    Ok(out)
}

// ============================================================================
// 触发来源 / 发送结果
// ============================================================================

/// 触发来源（`t_mail_digest_log.trigger_source`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSource {
    /// cron 定时任务（`task::mail_digest` 的 job）。
    Scheduled,
    /// 人工点「立即发送」（`POST /api/v2/mail-digest/send`）。
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
                code::BIZ_MAIL_SEND_FAILED,
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

/// 发送结果（`t_mail_digest_log.status`）。**成败都落库**，故这是流水的主状态。
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
                code::BIZ_MAIL_SEND_FAILED,
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

/// `POST /api/v2/mail-digest/config` 请求体：**全量覆写**。
///
/// ⚠️ `version` **必填、无 `#[serde(default)]`**（CLAUDE.md §8 硬约定）：
/// 缺失时 axum `Json` 提取器直接返 **HTTP 422 纯文本**，不是业务信封。
/// 「读-再-比」式隐式 OCC 被明令禁止：前端表单可能开着 30s，隐式 OCC 下
/// 「你看到的配置已被别人改过」会静默成功。
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
    pub recipients: Vec<String>,
    #[serde(default)]
    pub cc_recipients: Vec<String>,
    #[serde(default = "default_true")]
    pub include_overdue: bool,
    #[serde(default = "default_true")]
    pub include_upcoming: bool,
    #[serde(default = "default_upcoming_days")]
    pub upcoming_days: i32,
    pub visible_columns: Vec<String>,
    #[serde(default)]
    pub subject_template: Option<String>,
    /// 2026-10-16 新增：发信邮箱（`t_mail_account.id`）。
    ///
    /// - **JSON string**（与出参 `ConfigOut.sender_account_id` 对称，雪花 ID 防
    ///   JS 精度截断）：`deserialize_i64_opt` 只收字符串或 `null`。
    /// - **`null` / 缺省 = 不选邮箱**：新装的库在管理员第一次打开配置页之前就是
    ///   这个状态。此时定时任务**不注册 cron**、`POST /send` 返 `21701`
    ///   （与旧 env 方案「没配 SMTP 就跳过」的降级语义逐条对齐，判据换成 DB）。
    /// - 指向不存在**或已软删**的邮箱 ⇒ `21706 BIZ_MAIL_ACCOUNT_NOT_FOUND`
    ///   （写侧校验，不等到发信时才发现）。
    #[serde(
        default,
        deserialize_with = "crate::shared::types::deserialize_i64_opt"
    )]
    pub sender_account_id: Option<i64>,
}

fn default_true() -> bool {
    true
}

fn default_upcoming_days() -> i32 {
    UPCOMING_DAYS_DEFAULT
}

/// `POST /preview` / `POST /send` 请求体：**配置的部分覆盖**。
///
/// - **不含 `version`**：这两个端点**不改配置**，只是拿「当前配置 + 临时覆盖」
///   渲一封信出来试排版 / 真发一封，OCC 对它们没有意义（覆盖值不落库）。
/// - 全部字段 `#[serde(default)]`：空体 `{}` = 完全用当前配置，语义最自然。
/// - ⚠️ **刻意不含 `sender_account_id`**（2026-10-16）：`/preview` 不发信，
///   发信通道与它无关；`/send` 确实发信，但「用哪个邮箱发」是一条**配置**决策，
///   不该被「预览一次」这种临时覆盖顺带改掉（否则 /send 的覆盖就成了绕过配置
///   页直接换发件人的后门）。要换邮箱请走 `POST /config`。
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
    pub recipients: Option<Vec<String>>,
    #[serde(default)]
    pub cc_recipients: Option<Vec<String>>,
    #[serde(default)]
    pub include_overdue: Option<bool>,
    #[serde(default)]
    pub include_upcoming: Option<bool>,
    #[serde(default)]
    pub upcoming_days: Option<i32>,
    #[serde(default)]
    pub visible_columns: Option<Vec<String>>,
    #[serde(default)]
    pub subject_template: Option<String>,
}

/// `GET /logs` query：`limit∈[1,200]` + `offset≥0`。
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
// 出参
// ============================================================================

/// 配置的**规范化形态**：读侧与写侧共用的中间表示（已过校验、已清无用字段）。
///
/// `service` 内部一律拿它传给 `template::render` / `schedule::to_cron`，
/// 避免「DB 行 / 请求体 / 覆盖后配置」三套形状在渲染层反复转换。
#[derive(Debug, Clone)]
pub struct NormalizedConfig {
    pub enabled: bool,
    pub frequency: Frequency,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
    pub recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub include_overdue: bool,
    pub include_upcoming: bool,
    pub upcoming_days: i32,
    pub visible_columns: Vec<VisibleColumn>,
    pub subject_template: Option<String>,
    /// 2026-10-16 新增：发信邮箱 id（`t_mail_account.id`）；`None` = 未选。
    ///
    /// ⚠️ **不参与渲染**（`template::render` 完全不读它），只参与**发信**。
    pub sender_account_id: Option<i64>,
}

impl Default for NormalizedConfig {
    /// 缺省配置（`GET /config` 在表空时返的、`GET /meta` 的 `defaults` 段、
    /// `POST /config` 首次初始化前的基准，三者共用这一份）。
    ///
    /// ⚠️ `recipients` 刻意**为空**而不是塞一个占位地址：占位地址会让人以为
    /// 「配好了」（然后真发出一封没人收的信，或更糟：被人当成真地址加进客户名册）。
    ///
    /// ⚠️ **空数组在本结构里是合法值**：`POST /config` **允许**存一份空收件人的配置
    /// （编辑中途的正常形态），非空不变量**只**在发送路径上生效
    /// （`service::deliver` / `service::send_scheduled` 的准备段）。
    /// 原注释写的是「空数组会被 `POST /config` 的校验（21704）当场拦下」，**与实现相反**
    /// ——2026-10-13 review 第 1 轮 M6 订正，见
    /// `docs/api/mail_digest.md` §8.3「实现与注释不符」第 1 条与回归用例
    /// `config_accepts_empty_recipients_but_send_rejects_it`。
    /// `enabled` 缺省 `false`，同理（少发一封信永远好过多发一封）。
    fn default() -> Self {
        Self {
            enabled: false,
            frequency: Frequency::Daily,
            send_hour: Some(8),
            send_minute: Some(0),
            weekday: None,
            day_of_month: None,
            recipients: Vec::new(),
            cc_recipients: Vec::new(),
            include_overdue: true,
            include_upcoming: true,
            upcoming_days: UPCOMING_DAYS_DEFAULT,
            visible_columns: VisibleColumn::DEFAULT_SET.to_vec(),
            subject_template: None,
            // 2026-10-16：缺省**不选邮箱**。缺省「选一个自动命中的默认邮箱」会
            // 让「管理员还没配过邮箱」这件事在配置页上看不出来（页面显示正常、
            // 只有真发信时才炸）。显式选一次反而是更早暴露问题的形态。
            sender_account_id: None,
        }
    }
}

/// `GET /config` / `POST /config` 响应体。
///
/// `id` 是 JSON **string**（雪花 ID 防 JS 精度截断，`shared::types::serialize_i64`）。
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
    pub recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub include_overdue: bool,
    pub include_upcoming: bool,
    pub upcoming_days: i32,
    pub visible_columns: Vec<String>,
    pub subject_template: Option<String>,
    /// 2026-10-16 新增：发信邮箱 id（**JSON string** 或 `null`）。
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub sender_account_id: Option<i64>,
    /// 是否**已选定且可用**的发信通道（`sender_account_id` 指向一个未软删邮箱）。
    ///
    /// ⚠️ **2026-10-16 语义改写**：原先是「env 侧是否具备发信能力」
    /// （`MailConfig::is_enabled()`），而 env 那套已整轮删除。现在它是**纯 DB 判定**
    /// （一次 `get_account` 查询），前端「立即发送」按钮的灰态仍读这一位。
    ///
    /// ⚠️ 它**不**校验授权码能否解密（那要真解一次密），所以「主密钥被换过导致
    /// 存量密文全解不开」这一种故障在这里仍显示为 `true` —— 那一类只在真发信时
    /// 才以 `21701` 暴露。把它算进来的代价是每次读配置页都要做一次解密，
    /// 而解密失败的原因对管理员是不可行动的。
    pub smtp_configured: bool,
    /// naive timestamp（Asia/Shanghai），格式 `YYYY-MM-DD HH:MM:SS`。
    pub created_at: String,
    pub updated_at: String,
}

/// `GET /config` 的默认值形态：**不含** `id` / `version` / 时间戳 / `smtp_configured`
/// （`GET /meta` 的 `defaults` 段用）。
#[derive(Debug, Clone, Serialize)]
pub struct ConfigDefaultsOut {
    pub enabled: bool,
    pub frequency: String,
    pub send_hour: Option<i16>,
    pub send_minute: Option<i16>,
    pub weekday: Option<i16>,
    pub day_of_month: Option<i16>,
    pub recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub include_overdue: bool,
    pub include_upcoming: bool,
    pub upcoming_days: i32,
    pub visible_columns: Vec<String>,
    pub subject_template: Option<String>,
    /// 2026-10-16 新增：缺省**恒为 `null`**（未选邮箱）。序列化同上，JSON string。
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub sender_account_id: Option<i64>,
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
            recipients: c.recipients.clone(),
            cc_recipients: c.cc_recipients.clone(),
            include_overdue: c.include_overdue,
            include_upcoming: c.include_upcoming,
            upcoming_days: c.upcoming_days,
            visible_columns: c
                .visible_columns
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
            subject_template: c.subject_template.clone(),
            sender_account_id: c.sender_account_id,
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

/// `GET /meta` 的列选项。
#[derive(Debug, Clone, Serialize)]
pub struct ColumnOption {
    pub key: String,
    pub label: String,
}

/// `GET /meta` 的取值范围。
#[derive(Debug, Clone, Serialize)]
pub struct RangeOut {
    pub min: i32,
    pub max: i32,
}

/// `GET /meta` 响应体（前端表单的**内容与调度**选项源都在这一个端点里）。
///
/// ⚠️ **2026-10-16 删掉 `smtp` 段**。原先它带的是 env 侧 SMTP 公开信息
/// （`configured` / `host` / `port` / `tls_mode` / `from_address` / `from_name`），
/// 而那些值在「邮箱改为页面内配置」之后**逐条换了归属**：
///
/// | 原 `smtp.*` | 新的去处 |
/// |---|---|
/// | `configured` | `ConfigOut.smtp_configured`（`GET /config`，纯 DB 判定） |
/// | `host` / `port` / `tls_mode` | `GET /api/v2/mail/meta` 的 `presets[]` |
/// | `from_address` | `GET /api/v2/mail/accounts` 的 `account` |
/// | `from_name` | `GET /api/v2/mail/accounts` 的 `name` |
///
/// 保留一个「已经没有任何生产方」的字段只会让前端照着它读一个恒空的对象。
#[derive(Debug, Clone, Serialize)]
pub struct MetaOut {
    pub frequencies: Vec<FrequencyOption>,
    pub columns: Vec<ColumnOption>,
    pub defaults: ConfigDefaultsOut,
    pub upcoming_days_range: RangeOut,
}

/// `POST /preview` 响应体：**不发送、不落 log**。
#[derive(Debug, Clone, Serialize)]
pub struct PreviewOut {
    pub subject: String,
    pub html: String,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub columns: Vec<String>,
}

/// `POST /send` 响应体（= 流水行视图 + 本次计数）。
#[derive(Debug, Clone, Serialize)]
pub struct SendResultOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub log_id: i64,
    pub config_version: i32,
    pub trigger_source: TriggerSource,
    pub status: SendStatus,
    pub subject: String,
    pub to_recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    pub html_bytes: i32,
    pub sent_at: String,
    pub error_message: Option<String>,
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
