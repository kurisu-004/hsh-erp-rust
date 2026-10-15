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

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

// ============================================================================
// 统计窗口锚点（窗口的两端由它算出，见 [`report_window`]）
// ============================================================================

/// 统计窗口缺省锚点的**小时**：20。
///
/// ⚠️ 这不是「下班时间」这类软性理由，而是**默认推送时刻**的缺省值 —— 窗口上界恒
/// 等于「本次推送的那一刻」，下界是它**前一天**的同一时刻（跨天窗口）。两者必须同源，
/// 否则管理员改了「推送时刻」而窗口不动，日报会统计一段与推送时间错位的时间。
pub const WINDOW_ANCHOR_DEFAULT_HOUR: i16 = 20;
/// 统计窗口缺省锚点的**分钟**：05。
///
/// ⚠️ 挑 `20:05` 而不是 `20:00`：整点是全厂扫码/交付动作最密集的时刻，窗口上界压在
/// 整点上会让「20:00 整那一秒」的事件落在窗口外（半开区间的右端），而 5 分钟的错开
/// 既避开整点又仍然贴着「下班后汇总」的实际语义。
pub const WINDOW_ANCHOR_DEFAULT_MINUTE: i16 = 5;

/// 统计窗口：`(from_ts, to_ts)` = **前一天 @ 锚点 ~ 当天 @ 锚点**。
///
/// ## 为什么窗口**跨天**
///
/// 日报回答的是「**这一班**干了多少」，而一班横跨两天：下午 4 点下的料、第二天上午
/// 才做完了。按日历日切窗口会让「今天 20:05 才做完的那批」被切进**明天的**那份日报，
/// 于是昨天推送出去的那条里永远缺最后一班的量 —— 而日报的读者（老板 / 车间主管）
/// 恰恰是在当天晚上看昨天那一班的结论。
///
/// ## 锚点从哪来
///
/// `send_hour` / `send_minute`（推送时刻）就是锚点：上界恒等于「本次推送的那一刻」。
/// ⚠️ **两者任一为 `None` 时回退到 [`WINDOW_ANCHOR_DEFAULT_HOUR`] /
///
/// [`WINDOW_ANCHOR_DEFAULT_MINUTE`]**（`MANUAL_ONLY` 之下这两个字段允许为空，且允许
/// 存脏值 —— 写侧对 `MANUAL_ONLY` 刻意不校验时刻范围，理由见
/// `service::validate_time`）。所以这里必须**各自独立**兜底，不能用「`hour` 空则
/// 分钟也空」这种联动兜底。
///
/// ⚠️ **越界值退回缺省锚点，而不是夹逼到边界**：`MANUAL_ONLY` 之下写侧刻意不校验
/// 时刻范围（理由见 `service::validate_time`），库里可能躺着 `99`。把 `99` 夹成
/// `23:59` 会得到一个**看起来合法、实际统计了完全不同一段时间**的窗口；退回缺省
/// 锚点则与「没配过时刻」的表现一致 —— 两种都比产出怪窗口好。
///
/// 纯函数（零 IO）⇒ lib 单测直接可调，窗口的两端不需要连库就能钉住。
pub fn report_window(cfg: &NormalizedConfig, today: NaiveDate) -> (NaiveDateTime, NaiveDateTime) {
    let hour = cfg
        .send_hour
        .filter(|h| (0..=23).contains(h))
        .unwrap_or(WINDOW_ANCHOR_DEFAULT_HOUR) as u32;
    let minute = cfg
        .send_minute
        .filter(|m| (0..=59).contains(m))
        .unwrap_or(WINDOW_ANCHOR_DEFAULT_MINUTE) as u32;
    let to_ts = today.and_hms_opt(hour, minute, 0).unwrap_or_else(|| {
        // 上面已把两个分量过滤到合法域 ⇒ 这里**不可达**；保留它是为了让本函数对未来的
        // 改动仍然「要么给出一个合法时刻、要么给缺省锚点」，而不是退化成 `1970-01-01`
        // 那种看起来能编译、实际是空窗口的值。
        today
            .and_hms_opt(
                WINDOW_ANCHOR_DEFAULT_HOUR as u32,
                WINDOW_ANCHOR_DEFAULT_MINUTE as u32,
                0,
            )
            .expect("缺省锚点恒合法")
    });
    let from_ts = to_ts - chrono::Duration::days(1);
    (from_ts, to_ts)
}

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

/// 第三节交期清单「每组最多列出多少工单」的下限。
///
/// ⚠️ **下界是 `0` 而不是 1**：`0` = **不限**（与 [`WORKER_TOP_N_MIN`] 同一语义约定，
/// 渲染层 `template::build_blocks` 里 `if cfg.worker_top_n > 0 { … } else { 全列 }`
/// 是同款形态）。存量管理员手上可能有「清单要全量」的诉求，让它必须绕道写 999
/// 才能表达「不限」没有意义。
pub const ORDER_LIST_LIMIT_MIN: i32 = 0;
/// 第三节交期清单「每组最多列出多少工单」的上限。
///
/// ⚠️ **200 是「让管理员在保存时就知道自己填了多大的量」的护栏**，不是分片装填的
/// 硬边界（真撞上限时渲染层会以 `21804` 失败，绝不静默截断）。
pub const ORDER_LIST_LIMIT_MAX: i32 = 200;
/// 第三节交期清单「每组最多列出多少工单」的缺省值。
pub const ORDER_LIST_LIMIT_DEFAULT: i32 = 30;

/// 单次触发分片数上限的下限。
pub const MAX_CHUNKS_MIN: i16 = 1;
/// ⚠️ **20 是企业微信官方限频**（每个消息推送 ≤ 20 条/分钟），不是随手取的整数。
/// `POST /config` 在写侧就拒 `> 20`（`21804`），见 [`MAX_CHUNKS_MAX`] 的 doc。
pub const MAX_CHUNKS_MAX: i16 = 20;
/// 分片数上限缺省值。
///
/// ⚠️ **缺省从 3 提到 6 的理由**（2026-10-23，第三节改清单后重算）：清单形态下每行
/// ≈ **135 字节**（其中一条图纸预览直链就 ≈ 95 字节），4 组（逾期 / 今日 / 明日 /
/// 更远）× [`ORDER_LIST_LIMIT_DEFAULT`] 30 条 ≈ **16.4 KB ≈ 5 片** —— 缺省 3 片会让
/// 一份**完全按缺省配置**生成的日报直接 `21804`。6 片是在「装得下缺省配置」与
/// 「不发 20 条刷屏」之间取的中间值。
pub const MAX_CHUNKS_DEFAULT: i16 = 6;

/// `GET /logs` 单页最大条数（`limit ∈ [1,200]`）。
pub const LOGS_LIMIT_MAX: i64 = 200;
/// `GET /logs` 缺省单页条数。
pub const LOGS_LIMIT_DEFAULT: i64 = 20;

/// `subject_template` 长度上限（与列 `varchar(200)` 对齐）。
pub const SUBJECT_TEMPLATE_MAX_CHARS: usize = 200;

// ============================================================================
// 图纸预览链路的预算
// ============================================================================

/// 第三节在**推送**路径上等图纸预览就绪的预算（秒）。
///
/// ## 核算：必须整条推送在长档超时之内跑完
///
/// `/api/v2/cron/wecom-daily/send` 命中 `middleware::timeout.rs` 的**长档**（660s，
/// 白名单第 3 条）。这条请求里串着两段串行等待：
///
/// ```text
/// 480s（CI 就绪预算）+ max_chunks(6) × 5s（逐片顺序发送，单片超时 5s）= 510s < 660s
/// ```
///
/// 余量 150s 留给：取数 SQL、渲染、以及超时层本身的开销。
///
/// ## ⚠️ 为什么是 8 分钟而不是用户最初说的 10 分钟
///
/// 10 分钟 = 600s，加上发送段就是 630s，距 660s 只剩 30s —— 而**分片数是配置项**
/// （`max_chunks` 上限 20）：管理员把它调到 7 就变成 635s，调到 8 就 640s 起步，
/// 再叠加一次慢 SQL 就可能整条请求被超时层掐掉。掐掉的后果不是「重试一次」而是
/// **已经推进群里的前 k 片连同流水一起消失**（超时把 handler 的 future 整个 drop），
/// `GET /logs` 里查不到任何痕迹 —— 这条不变量正是长档白名单存在的理由，不能再让它
/// 被一个可配的分片数顶破。
///
/// 8 分钟给了 150s 余量，且**与分片数无关地安全**：`max_chunks` 调到上限 20 时最坏
/// 仍是 480 + 100 = 580s < 660s。
///
/// ## ⚠️ 预算是**上限**，不是承诺
///
/// 绝大多数工单在几秒内就绪（CI 转 PDF 通常很快）；预算是「最坏情况不许超过」。
/// 预算耗尽 ⇒ 那一行**不带链接**、其余照常挂 —— **绝不因为一张图没转出来就不发日报**
/// （见 `files::part_file::preview::service::PreviewService::ensure_ready_for_parts`）。
///
/// ## ⚠️ 预算约束的是**耗时**，不是**提交量**（2026-10-23）
///
/// CI 提交是**按页计费**的，而名义提交量可以很大：`UPCOMING_DAYS_MAX`(60) ×
/// `ORDER_LIST_LIMIT_MAX`(200) ⇒ 截断后交给图纸链路的入参名义上可到 ~12 000 份。
/// 真正兜住钱的是 `PreviewService::ensure_ready_for_parts` 内部的 deadline 检查 ——
/// 预算耗尽后它不再提交新 job，未就绪的行只是不挂链接。⚠️ 所以「把入参收窄」仍然是
/// 值得做的（少交 pointless 的 job、少花 480s 轮询），但它不是**正确性**依赖：
/// 不收窄不会烧穿预算，只会更慢。
pub const DRAWING_CI_BUDGET_SECS: u64 = 8 * 60;

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
//
// ## ⚠️ 这两个枚举的 `FromStr` 失败**回 `code::INTERNAL`（50000），不是 `21802`**
//
// 这两个是**读侧**枚举（从 `t_wecom_daily_log` 的一行反解），解析失败意味着**库里那行有
// 脏值** —— 那是数据完整性问题（别的进程 / 手改 SQL / 一次没配好的迁移），与「这一次的
// 推送失不成功」毫无关系。返回 `BIZ_WECOM_SEND_FAILED`（21802 / HTTP 502「投递失败」）
// 会把**数据层故障伪装成投递故障**：监控按 21802 告警 ⇒ 值班的人去查企业微信通道，而真因
// 在库里。分层上它属于 `5xxxx` 系统层（`code::INTERNAL`）。
//
// ⚠️ **现状无生产方**：[`SendResultOut::from_row`] 用 `unwrap_or` 兜底（脏值退化成
// `MANUAL` / `FAILED` 展示，不冒泡），所以这一处**不改变任何 wire 行为** —— 是纯粹的
// 分层纠正，留着是为了「万一将来有人改成严格模式」，那时它报出来的码才是对的。
//
// ⚠️ 与**写侧**的分界：用户提交非法 `frequency` / `worker_metric` 仍是 `21804`
// （那是**用户输入**校验），本节两个枚举是**库内数据**的反解。

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
                code::INTERNAL,
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
                code::INTERNAL,
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
    /// 第三节交期清单每组最多列出的工单数；`0` = 不限（与 `worker_top_n` 同语义）。
    #[serde(default = "default_order_list_limit")]
    pub order_list_limit: i32,
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
    /// 第三节清单是否带图纸预览直链（缺省 `true`）。
    #[serde(default = "default_true")]
    pub include_drawings: bool,
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

fn default_order_list_limit() -> i32 {
    ORDER_LIST_LIMIT_DEFAULT
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
    /// 第三节清单每组最多列出的工单数（`0` = 不限）。⚠️ **合并覆盖路径拒越界**
    /// （`validate_order_list_limit`）而不 clamp —— 与 `upcoming_days` 同款理由：
    /// 覆盖值是管理员在预览框里现填的，夹到边界会让预览结果与他的输入不符。
    #[serde(default)]
    pub order_list_limit: Option<i32>,
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
    /// 第三节清单是否带图纸预览直链。
    ///
    /// ⚠️ 可覆盖的**实际意义**是「今天试一下不带链接的清单长什么样」：带链接每行多
    /// ≈95 字节（见 [`MAX_CHUNKS_DEFAULT`] 的核算），而这一项直接决定分片数是否会撞
    /// `max_chunks` ⇒ 管理员必须能在**不动落库配置**的前提下对比两种形态。
    #[serde(default)]
    pub include_drawings: Option<bool>,
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
    /// 第三节交期清单每组最多列出的工单数；`0` = 不限。
    pub order_list_limit: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    /// 第三节清单是否带图纸预览直链。
    pub include_drawings: bool,
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
            // ⚠️ 缺省推送时刻**同时**是统计窗口的锚点（见 `report_window`）⇒ 这里改成
            // 20:05 时，报工 / 出货 / 工人榜三处窗口的上界一律跟着走。
            send_hour: Some(WINDOW_ANCHOR_DEFAULT_HOUR),
            send_minute: Some(WINDOW_ANCHOR_DEFAULT_MINUTE),
            weekday: None,
            day_of_month: None,
            webhook_id: None,
            upcoming_days: UPCOMING_DAYS_DEFAULT,
            worker_metric: WorkerMetric::Quantity,
            worker_top_n: WORKER_TOP_N_DEFAULT,
            order_list_limit: ORDER_LIST_LIMIT_DEFAULT,
            max_chunks: MAX_CHUNKS_DEFAULT,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
            include_drawings: true,
            subject_template: None,
        }
    }
}

// ============================================================================
// 出参
// ============================================================================

/// `GET /config` / `POST /config` 响应体（**23 个字段**，前端 `.strict()` + 键数断言）。
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
    /// 第三节交期清单每组最多列出的工单数；`0` = 不限。
    pub order_list_limit: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    /// 第三节清单是否带图纸预览直链。
    pub include_drawings: bool,
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
    /// 第三节交期清单每组最多列出的工单数；`0` = 不限。
    pub order_list_limit: i32,
    pub max_chunks: i16,
    pub include_overview: bool,
    pub include_delivery: bool,
    pub include_orders: bool,
    pub include_workers: bool,
    /// 第三节清单是否带图纸预览直链。
    pub include_drawings: bool,
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
            order_list_limit: c.order_list_limit,
            max_chunks: c.max_chunks,
            include_overview: c.include_overview,
            include_delivery: c.include_delivery,
            include_orders: c.include_orders,
            include_workers: c.include_workers,
            include_drawings: c.include_drawings,
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

/// `POST /preview` 响应体（**13 个字段**）：渲染、不发送、不落 log。
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
    /// 完工单数。⚠️ **不进正文**（第一节正文只有报工 / 送检 / 在制），但保留在出参里
    /// —— 预览弹窗是管理员核对口径的入口，删掉它等于让这个数在系统里无处可查。
    pub completed_count: i32,
    /// 当前在制单数（口径 = `t_part.status = 'IN_PROCESS'`，见 `repo::OverviewEvents::wip`）。
    pub in_process_count: i32,
    /// 第二节的那个数：本期出货的**去重工单数**（`COUNT(DISTINCT part_id)`）。
    ///
    /// ⚠️ **2026-10-23 起语义变更**：此前本键恒为 `0`、真正的数字在
    /// `note_part_count` 里（那时本键指的是「送货单张数」，而第二节已不再统计它）。
    /// 现在一个键一个含义，**历史流水行按旧语义理解**（见迁移
    /// `20261023010000_001_wecom_daily_note_count_semantics.sql` 的列注释）。
    pub note_count: i32,
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
    /// 第二节的那个数：本期出货的**去重工单数**（`COUNT(DISTINCT part_id)`）。
    /// 与 [`PreviewOut::note_count`] 同源同义，两处同名字段不许出现不同口径。
    pub note_count: i32,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 18).expect("固定测试日期合法")
    }

    /// ★ 守卫：窗口恒**跨天**，两端都等于同一个锚点（推送时刻）。
    ///
    /// ⚠️ 这条钉的是「一天」这个不变量本身：哪天有人把 `from_ts` 写成 `to_ts` 当天的
    /// 零点、或写成 `today - 1 天的**零点**」（那会少统计前一天 00:00~20:05 的量），
    /// 本条立刻红。
    #[test]
    fn report_window_spans_exactly_one_day_back_from_the_anchor() {
        let cfg = NormalizedConfig::default();
        let (from, to) = report_window(&cfg, day());
        let anchor = day()
            .and_hms_opt(
                WINDOW_ANCHOR_DEFAULT_HOUR as u32,
                WINDOW_ANCHOR_DEFAULT_MINUTE as u32,
                0,
            )
            .expect("缺省锚点恒合法");
        assert_eq!(to, anchor, "上界 = 发送当天的推送时刻");
        assert_eq!(
            from,
            anchor - chrono::Duration::days(1),
            "下界 = 前一天的同一时刻"
        );
        assert_eq!((to - from).num_seconds(), 24 * 3600);
        // 半开 `[from, to)`：下界闭合、上界不含（`e.created_at < to` 这一侧的边界）。
        assert!(from < to);
    }

    /// ★ 守卫：锚点缺省 / 脏值都退回缺省锚点，**绝不**产出一个「看起来合法、实际是空
    /// 窗口」的时间对。
    ///
    /// `MANUAL_ONLY` 之下 `send_hour` / `send_minute` 允许为空、且写侧**刻意不校验**
    /// 时刻范围（理由见 `service::validate_time`）⇒ 这两档不是假想。
    #[test]
    fn report_window_falls_back_to_default_anchor() {
        let anchor = day()
            .and_hms_opt(
                WINDOW_ANCHOR_DEFAULT_HOUR as u32,
                WINDOW_ANCHOR_DEFAULT_MINUTE as u32,
                0,
            )
            .expect("缺省锚点恒合法");
        let full = (anchor - chrono::Duration::days(1), anchor);

        // 两个都缺省
        let cfg = NormalizedConfig {
            send_hour: None,
            send_minute: None,
            ..NormalizedConfig::default()
        };
        assert_eq!(report_window(&cfg, day()), full);

        // ⚠️ **各自独立**兜底：只有分钟缺省时，小时仍取配置值（不能连坐成缺省小时）。
        let cfg = NormalizedConfig {
            send_hour: Some(9),
            send_minute: None,
            ..NormalizedConfig::default()
        };
        let (from, to) = report_window(&cfg, day());
        assert_eq!(to, day().and_hms_opt(9, 5, 0).unwrap());
        assert_eq!(from, to - chrono::Duration::days(1));

        // 脏值 99（写侧对 MANUAL_ONLY 放行）⇒ 夹逼后回缺省锚点，而不是 `None` 崩溃
        // 或悄悄变成当天 00:00。
        let cfg = NormalizedConfig {
            send_hour: Some(99),
            send_minute: Some(99),
            ..NormalizedConfig::default()
        };
        assert_eq!(report_window(&cfg, day()), full);
    }

    /// ★ 守卫：读侧两个枚举的**脏值解析失败回 `code::INTERNAL`（系统层），不是 `21802`**
    /// （投递失败）。
    ///
    /// 这两个枚举只用于**从库里反解一行流水** ⇒ 解析失败 = 库里有脏值 = 数据完整性问题。
    /// 报成 `21802`（HTTP 502「投递失败」）会让监控按投递故障告警，而真因在库。
    /// ⚠️ **变异验证**：把两处 `code::INTERNAL` 改回 `code::BIZ_WECOM_SEND_FAILED`
    /// 本用例立刻红。
    #[test]
    fn read_side_enum_parse_failures_are_internal_not_send_failed() {
        let e = "自动".parse::<TriggerSource>().expect_err("脏值必须被拒");
        assert_eq!(e.code(), code::INTERNAL, "不该报成投递失败：{e}");
        assert_ne!(e.code(), code::BIZ_WECOM_SEND_FAILED);
        assert!(e.to_string().contains("SCHEDULED / MANUAL"));

        let e = "PARTIAL".parse::<SendStatus>().expect_err("脏值必须被拒");
        assert_eq!(e.code(), code::INTERNAL, "不该报成投递失败：{e}");
        assert_ne!(e.code(), code::BIZ_WECOM_SEND_FAILED);
        assert!(e.to_string().contains("SUCCESS / FAILED"));

        // 合法值逐条仍能解析（别把分层纠正改成「一律拒」）。
        assert_eq!(
            "SCHEDULED".parse::<TriggerSource>().unwrap(),
            TriggerSource::Scheduled
        );
        assert_eq!(
            "MANUAL".parse::<TriggerSource>().unwrap(),
            TriggerSource::Manual
        );
        assert_eq!(
            "SUCCESS".parse::<SendStatus>().unwrap(),
            SendStatus::Success
        );
        assert_eq!("FAILED".parse::<SendStatus>().unwrap(), SendStatus::Failed);
    }

    /// 对照组：**写侧**枚举（用户输入）仍回 `21804`，别被上面那条带偏。
    #[test]
    fn write_side_enum_parse_failures_stay_21804() {
        let e = "HOURS"
            .parse::<WorkerMetric>()
            .expect_err("非法指标必须被拒");
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
        let e = "HOURLY".parse::<Frequency>().expect_err("非法频率必须被拒");
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
    }
}
