//! wecom_daily 域 service 层：校验 / OCC / 取数编排 / 发送编排
//!
//! ## 事务边界（CLAUDE.md §1）
//! 本域**不开显式事务**：一次请求要么只读、要么只做单行 UPDATE/INSERT，每条语句
//! 自身原子。开事务反而会让「INSERT 流水失败 → 整笔回滚 → 发送结果也丢了」这种
//! 场景无法发生。
//!
//! ## ⚠️ 连接纪律：**投递期间绝不持有连接**（2026-10-19）
//!
//! 读 / 配置端点的连接照旧在 handler 借（`pool.acquire()` → 借 `&mut *conn` → 用完
//! 还），但**推送路径（`send_manual` / `send_scheduled` → `deliver`）自己借、且每段
//! 用完即还**，全段只在三个点借连接：
//!
//! ```text
//! 借 1/3 → resolve_target（查 + 解密）→ 还
//! 借 2/3 → fetch_rows（恒 O(1) 条 SQL）→ 还
//! ────── 逐片顺序发送（≈ max_chunks × 5s 的**纯网络等待**，零 DB IO）──────
//! 借 3/3 → cfg_version 快照 + insert_log（合并成**一次**借还）→ 还
//! ```
//!
//! 为什么：**投递期间握着一条 `PoolConnection` 等 ≈100s 的网络**，`/send` 又**无频控**
//! ⇒ 几个管理员同时点就把连接池占满，**读端点跟着排队**（一条只查配置单例的
//! `GET /config` 排在后面拿不到连接，现场看是「整个域卡住」）。`/preview` 不受
//! 影响（只渲染不投递）。
//!
//! ## 域外依赖：**1 个模块**（`wecom`，通道域）
//!
//! [`crate::modules::wecom::service::resolve_target`]（拿明文 webhook URL + 名称 +
//! 掩码）、[`crate::modules::wecom::service::target_is_available`]（调度注册判据）、
//! [`crate::modules::wecom::sender::send_markdown`]（唯一出网点）。方向是
//! **内容域 → 通道域**，反向不存在。
//! ⚠️ 本域**不解密**任何东西：webhook URL 的解密在通道域内完成。
//!
//! ## 发送流程（`deliver`）的顺序是刻意的
//!
//! ```text
//! 解析通道 → 判四节不全关 → 取数（关掉的节不查） → 渲染分片 → 逐片顺序发送
//!         → 无论成败落流水
//! ```
//!
//! ⚠️ **通道解析排在节开关检查之前**：通道不可用是更基础的故障，先报它，免得管理员
//! 在「还没选通道」的现场被「四节全关」带偏方向。
//!
//! ## 两条「不落流水」的失败（必须区分）
//!
//! | 失败 | 落流水？ | 理由 |
//! |---|---|---|
//! | 渲染阶段 `21804`（单块超限 / 超 `max_chunks`） | **否** | **没发出去**，不是投递失败。前端 `/send` 失败后靠「有没有流水行」区分这两类 |
//! | 投递阶段 `21802`（某片推送失败） | **是** | 已发出若干片，流水是唯一的现场 |
//! | `SCHEDULED` 的准备阶段失败 | **是** | 见 [`log_prepare_failure`] |
//! | `MANUAL` 的准备阶段失败 | **否** | 失败有 HTTP 错误码给调用方看（照 `mail_digest` 既有口径） |
//!
//! ## 日志纪律
//! **绝不**打印 webhook 明文 URL（那个 `key=` 就是 bearer 凭证）：通道域的
//! `ResolvedTarget` 里 `url` 只在调用栈活着，本域只把它交给 `send_markdown`。
//! 落库与 `tracing` 一律用 `masked`。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::modules::wecom::sender;
use crate::modules::wecom::service as wecom_service;
use crate::modules::wecom_daily::dto::{
    ConfigDefaultsOut, ConfigOut, Frequency, LOGS_LIMIT_MAX, LogListOut, LogListQuery,
    MAX_CHUNKS_MAX, MAX_CHUNKS_MIN, NormalizedConfig, ORDER_LIST_LIMIT_MAX, ORDER_LIST_LIMIT_MIN,
    PreviewOut, PreviewRequest, SUBJECT_TEMPLATE_MAX_CHARS, SaveConfigRequest, SendResultOut,
    SendStatus, TriggerSource, UPCOMING_DAYS_MAX, UPCOMING_DAYS_MIN, WORKER_TOP_N_MAX,
    WORKER_TOP_N_MIN, report_window,
};
use crate::modules::wecom_daily::model::{TWeComDailyConfigRow, TWeComDailyLogRow};
use crate::modules::wecom_daily::repo::{self, ActiveBatchRow, AssemblyChildRow, NewDailyLog};
use crate::modules::wecom_daily::template::{
    self, OrderGroup, OrderGroupKind, OrderRow, RenderInput, ReportRows,
};
use crate::shared::error::{AppError, code};
use crate::state::AppState;

// ============================================================================
// 读
// ============================================================================

/// `GET /config`：读配置单例。
///
/// 表里无行时返回**全部默认值 + `version=0` + `id="0"`**（而不是 404）：配置页必须
/// 能在「还没配过」的状态下正常打开并提交首次配置，返 404 会让前端多写一套
/// 「404 ⇒ 空表单」的分支。
///
/// `webhook_configured` 是本轮唯一需要额外查一次库的地方：判「选中的那行还在不在」。
pub async fn get_config(conn: &mut PgConnection) -> Result<ConfigOut, AppError> {
    match repo::get_config(conn).await? {
        Some(row) => {
            let configured = webhook_row_exists(conn, row.webhook_id).await;
            Ok(ConfigOut::from_row(&row, configured))
        }
        None => Ok(ConfigOut::default_out(false)),
    }
}

/// `webhook_id` 指向的行**还在不在**（纯 DB 存在性检查）。
///
/// ⚠️ **刻意不用** [`wecom_service::target_is_available`]（它会真解一次密）：把
/// 「运维换了 `MAIL_ACCOUNT_SECRET` 导致存量密文全解不开」显示成「通道没配置」，会让
/// 管理员去建一条新 webhook 而不是找运维换回密钥 —— 那是**方向性错误**的提示。
/// 解密失败那一类只在真推送时以 `21801` 暴露。
async fn webhook_row_exists(conn: &mut PgConnection, webhook_id: Option<i64>) -> bool {
    let Some(id) = webhook_id else { return false };
    matches!(
        crate::modules::wecom::repo::get_webhook(conn, id).await,
        Ok(Some(_))
    )
}

/// `GET /logs`：推送历史分页。
pub async fn list_logs(conn: &mut PgConnection, q: &LogListQuery) -> Result<LogListOut, AppError> {
    // limit clamp 到 [1,200]：0 / 负数会让 `LIMIT` 报错或恒空，超大值等于没分页。
    let limit = q.limit.clamp(1, LOGS_LIMIT_MAX);
    let offset = q.offset.max(0);
    let (rows, total) = repo::list_logs(conn, limit, offset).await?;
    Ok(LogListOut {
        items: rows.iter().map(SendResultOut::from_row).collect(),
        total,
        limit,
        offset,
    })
}

/// `GET /meta` 的 `defaults` 段（纯函数，不查库）。
pub fn config_defaults() -> ConfigDefaultsOut {
    ConfigDefaultsOut::from(&NormalizedConfig::default())
}

// ============================================================================
// 写配置（OCC）
// ============================================================================

/// `POST /config`：全量覆写 + 触发调度热重载。
///
/// ⚠️ body 的 `version` 由 DTO 保证**必填**（缺失 → axum `Json` 提取器返 HTTP 422
/// 纯文本），本函数只做两件事：
/// 1. 首次初始化（表里无行、`version=0`）→ INSERT；
/// 2. 其余 → `UPDATE … WHERE id AND version` 的标准 OCC。
pub async fn save_config(
    conn: &mut PgConnection,
    req: &SaveConfigRequest,
    current: &CurrentUser,
    snowflake: &crate::infra::snowflake::SnowflakeIdGenerator,
) -> Result<ConfigOut, AppError> {
    validate_webhook(conn, req.webhook_id).await?;
    let normalized = validate_save_request(req)?;
    let now = now_naive();

    match repo::get_config(conn).await? {
        None => {
            // 首次初始化。并发下两个请求同时走到这里，靠唯一索引
            // `uk_t_wecom_daily_config_alive` 兜底：后到者撞 23505 → 转 OCC 冲突。
            let id = snowflake.next_id();
            match repo::insert_config(conn, id, &normalized, current.id, now).await {
                Ok(true) => {}
                Ok(false) => return Err(version_conflict()),
                Err(e) if is_unique_violation(&e) => return Err(version_conflict()),
                Err(e) => return Err(e.into()),
            }
        }
        Some(row) => {
            let ok = repo::update_config(conn, row.id, req.version, &normalized, current.id, now)
                .await?;
            if !ok {
                return Err(version_conflict());
            }
        }
    }

    // 读回最新行（含自增后的 version）返回，避免前端拿一个「提交前的 version」做
    // 下一次提交 → 立刻撞一次假的 OCC 冲突。
    let fresh = repo::get_config(conn).await?.ok_or_else(version_conflict)?;
    let configured = webhook_row_exists(conn, fresh.webhook_id).await;
    Ok(ConfigOut::from_row(&fresh, configured))
}

impl ConfigOut {
    fn from_row(row: &TWeComDailyConfigRow, webhook_configured: bool) -> Self {
        Self {
            id: row.id,
            version: row.version,
            enabled: row.enabled,
            frequency: row.frequency.clone(),
            send_hour: row.send_hour,
            send_minute: row.send_minute,
            weekday: row.weekday,
            day_of_month: row.day_of_month,
            webhook_id: row.webhook_id,
            upcoming_days: row.upcoming_days,
            worker_metric: row.worker_metric.clone(),
            worker_top_n: row.worker_top_n,
            order_list_limit: row.order_list_limit,
            max_chunks: row.max_chunks,
            include_overview: row.include_overview,
            include_delivery: row.include_delivery,
            include_orders: row.include_orders,
            include_workers: row.include_workers,
            include_drawings: row.include_drawings,
            subject_template: row.subject_template.clone(),
            webhook_configured,
            created_at: format_ts(row.created_at),
            updated_at: format_ts(row.updated_at),
        }
    }

    /// 表里无行时的「零值配置」：`id="0"` + `version=0` + 缺省内容。
    fn default_out(webhook_configured: bool) -> Self {
        let d = NormalizedConfig::default();
        Self {
            id: 0,
            version: 0,
            enabled: d.enabled,
            frequency: d.frequency.as_str().to_string(),
            send_hour: d.send_hour,
            send_minute: d.send_minute,
            weekday: d.weekday,
            day_of_month: d.day_of_month,
            webhook_id: d.webhook_id,
            upcoming_days: d.upcoming_days,
            worker_metric: d.worker_metric.as_str().to_string(),
            worker_top_n: d.worker_top_n,
            order_list_limit: d.order_list_limit,
            max_chunks: d.max_chunks,
            include_overview: d.include_overview,
            include_delivery: d.include_delivery,
            include_orders: d.include_orders,
            include_workers: d.include_workers,
            include_drawings: d.include_drawings,
            subject_template: d.subject_template.clone(),
            webhook_configured,
            // 时间戳在「尚未创建」时没有意义，给空串而不是 `now()`：前端拿它做
            // 「更新时间」展示的话，空串比会跳动的假时间更好判断。
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

impl SendResultOut {
    fn from_row(row: &TWeComDailyLogRow) -> Self {
        Self {
            log_id: row.id,
            config_version: row.config_version,
            trigger_source: row.trigger_source.parse().unwrap_or(TriggerSource::Manual),
            status: row.status.parse().unwrap_or(SendStatus::Failed),
            webhook_name: row.webhook_name.clone(),
            webhook_masked: row.webhook_masked.clone(),
            report_date: row.report_date.format("%Y-%m-%d").to_string(),
            markdown_bytes: row.markdown_bytes,
            chunk_count: row.chunk_count as i32,
            chunk_sent: row.chunk_sent as i32,
            overview_batch_count: row.overview_batch_count,
            overview_quantity: row.overview_quantity,
            note_count: row.note_count,
            note_part_count: row.note_part_count,
            overdue_count: row.overdue_count,
            upcoming_count: row.upcoming_count,
            worker_row_total: row.worker_row_total,
            error_message: row.error_message.clone(),
            sent_at: format_ts(row.sent_at),
        }
    }
}

fn format_ts(ts: NaiveDateTime) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn version_conflict() -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        "配置已被其他人修改（或刚刚被首次创建），请刷新后重试",
    )
}

/// PG 唯一约束冲突（SQLSTATE 23505）。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")
    )
}

/// naive timestamp 的展示格式（与全仓 `created_at` 出参一致）。
pub(crate) fn now_ts() -> NaiveDateTime {
    now_naive()
}

// ============================================================================
// 校验（写侧 21804）
// ============================================================================

/// 校验 + 规范化 `POST /config` 请求体。
///
/// 规范化做的事（都在这里做完，下游一律拿 [`NormalizedConfig`]）：
/// 1. `frequency` / `worker_metric` 解析成枚举（未知 ⇒ `21804`）；
/// 2. `upcoming_days` / `order_list_limit` **clamp** 到各自的区间（照 `mail_digest`：
///    这两个数只影响窗口宽窄 / 清单长度，越界时夹到边界比报错更符合「用户想表达
///    的意思」；而下面三个是**拒**）；
/// 3. `DAILY` / `MANUAL_ONLY` 之下**清空** `weekday` / `day_of_month`
///    （不留脏值：读侧会照单全收地拼 cron，留着等于埋雷）；
///    `WEEKLY` / `MONTHLY` 的必填字段缺失 ⇒ `21804`；
/// 4. `worker_top_n` 必须 ∈ `[0, 200]`、`max_chunks` 必须 ∈ `[1, 20]` ⇒ `21804`；
/// 5. 四节**不能同时关** ⇒ `21804`；
/// 6. `subject_template` ≤ 200 **字** + 拒 CR/LF ⇒ `21804`。
///
/// ⚠️ **本函数刻意保持「纯函数、零 DB IO」**：`webhook_id` 的存在性校验被拆到
/// [`validate_webhook`]（async、查一次库）。拆开而不是塞进来，是为了让本函数仍可被
/// lib 单测直接调用，也让「哪条规则要查库」在函数签名上就看得见。
pub fn validate_save_request(req: &SaveConfigRequest) -> Result<NormalizedConfig, AppError> {
    let normalized = NormalizedConfig {
        enabled: req.enabled,
        frequency: req.frequency.parse()?,
        send_hour: req.send_hour,
        send_minute: req.send_minute,
        weekday: None,
        day_of_month: None,
        webhook_id: req.webhook_id,
        upcoming_days: req
            .upcoming_days
            .clamp(UPCOMING_DAYS_MIN, UPCOMING_DAYS_MAX),
        worker_metric: req.worker_metric.parse()?,
        worker_top_n: req.worker_top_n,
        order_list_limit: req
            .order_list_limit
            .clamp(ORDER_LIST_LIMIT_MIN, ORDER_LIST_LIMIT_MAX),
        max_chunks: req.max_chunks,
        include_overview: req.include_overview,
        include_delivery: req.include_delivery,
        include_orders: req.include_orders,
        include_workers: req.include_workers,
        include_drawings: req.include_drawings,
        subject_template: validate_subject_template(req.subject_template.as_deref())?,
    };
    let weekday = match normalized.frequency {
        Frequency::Weekly => Some(validate_weekday(req.weekday)?),
        _ => None,
    };
    let day_of_month = match normalized.frequency {
        Frequency::Monthly => Some(validate_day_of_month(req.day_of_month)?),
        _ => None,
    };
    validate_normalized(&NormalizedConfig {
        weekday,
        day_of_month,
        ..normalized
    })
}

/// `webhook_id` 的存在性校验（**唯一**要查库的校验规则）。
///
/// `NULL` 是**合法值**（未选通道 —— 新装的库在管理员第一次打开配置页之前就是它）；
/// 指向不存在**或已软删**的 webhook ⇒ `21806 BIZ_WECOM_WEBHOOK_NOT_FOUND`。
///
/// 为什么在这里查而不在推送时才发现：配置页若接受一个失效的 id，页面上看着一切正常
/// （甚至 `webhook_configured` 也可能在删之前是 `true`），直到定时任务真推送才以
/// `21801` 暴露 —— 那时现场只剩一句 503，没人知道配置早就坏了。
pub async fn validate_webhook(
    conn: &mut PgConnection,
    webhook_id: Option<i64>,
) -> Result<(), AppError> {
    let Some(id) = webhook_id else {
        return Ok(());
    };
    if webhook_row_exists(conn, Some(id)).await {
        return Ok(());
    }
    Err(AppError::biz(
        code::BIZ_WECOM_WEBHOOK_NOT_FOUND,
        format!(
            "推送通道（id={id}）不存在或已被删除：请先在「定时任务 → 企业微信 → webhook 配置」\
             里添加，或改选另一个通道"
        ),
    ))
}

/// 时段校验：`MANUAL_ONLY` 不需要时刻；其余频率必须给全 `hour` + `minute`。
///
/// ⚠️ **`MANUAL_ONLY` 直接早退是有意的**：这两个字段在 `MANUAL_ONLY` 之下**永不被读**
/// （不产生 cron、不注册 job），为它们加范围校验只会给「切到仅手动」这个最高频操作
/// 平白加一条报错路径。代价是能存进 `99`。
fn validate_time(
    frequency: Frequency,
    hour: Option<i16>,
    minute: Option<i16>,
) -> Result<(), AppError> {
    if !frequency.needs_time() {
        return Ok(());
    }
    let Some(h) = hour else {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("频率 {} 需要填写推送小时（0..23）", frequency.as_str()),
        ));
    };
    if !(0..=23).contains(&h) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("推送小时必须在 0..23 之间，收到 {h}"),
        ));
    }
    let Some(m) = minute else {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("频率 {} 需要填写推送分钟（0..59）", frequency.as_str()),
        ));
    };
    if !(0..=59).contains(&m) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("推送分钟必须在 0..59 之间，收到 {m}"),
        ));
    }
    Ok(())
}

fn validate_weekday(v: Option<i16>) -> Result<i16, AppError> {
    match v {
        Some(d) if (1..=7).contains(&d) => Ok(d),
        Some(d) => Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("weekday 必须在 1..7 之间（ISO：1=周一 .. 7=周日），收到 {d}"),
        )),
        None => Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            "频率 WEEKLY（每周）必须填写 weekday（1..7，ISO：1=周一 .. 7=周日）",
        )),
    }
}

fn validate_day_of_month(v: Option<i16>) -> Result<i16, AppError> {
    match v {
        Some(d) if (1..=28).contains(&d) => Ok(d),
        Some(d) => Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("day_of_month 必须在 1..28 之间（刻意避开 2 月末与 30/31 日短月），收到 {d}"),
        )),
        None => Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            "频率 MONTHLY（每月）必须填写 day_of_month（1..28）",
        )),
    }
}

/// 标题模板校验：≤ 200 **字**（与列 `varchar(200)` 对齐）+ 拒 CR/LF。
///
/// ⚠️ `.chars().count()` 而不是 `.len()`：后者是**字节数**，中文标题会被算成 3 倍，
/// 一个 80 字的标题（240 字节）会被误判为超长。
pub fn validate_subject_template(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(t) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if t.contains('\r') || t.contains('\n') {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            "消息标题模板含 CR/LF（已拒绝）",
        ));
    }
    let n = t.chars().count();
    if n > SUBJECT_TEMPLATE_MAX_CHARS {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("消息标题模板超长：{n} 字（上限 {SUBJECT_TEMPLATE_MAX_CHARS}）"),
        ));
    }
    Ok(Some(t.to_string()))
}

/// 对已合并好的 [`NormalizedConfig`] 做完整校验（预览 / 发送路径用）。
///
/// ⚠️ 与 `validate_save_request` 的两处**刻意不对称**：
/// - `upcoming_days` 走**拒**（而不是 clamp）：覆盖值是用户在预览框里现填的，夹到
///   边界会让预览结果与他的输入不符（他填 -5，看到的却是「未来 1 天」）；
/// - `weekday` / `day_of_month` 只在**需要**的那个频率下校验（合并进来的可能是
///   上一档频率残留的脏值，不该为它报错）。
fn validate_normalized(cfg: &NormalizedConfig) -> Result<NormalizedConfig, AppError> {
    validate_time(cfg.frequency, cfg.send_hour, cfg.send_minute)?;
    if cfg.frequency.needs_weekday() {
        validate_weekday(cfg.weekday)?;
    }
    if cfg.frequency.needs_day_of_month() {
        validate_day_of_month(cfg.day_of_month)?;
    }
    validate_subject_template(cfg.subject_template.as_deref())?;
    if !(UPCOMING_DAYS_MIN..=UPCOMING_DAYS_MAX).contains(&cfg.upcoming_days) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!(
                "未来天数必须在 {UPCOMING_DAYS_MIN}..{UPCOMING_DAYS_MAX} 之间，收到 {}",
                cfg.upcoming_days
            ),
        ));
    }
    validate_worker_top_n(cfg.worker_top_n)?;
    validate_order_list_limit(cfg.order_list_limit)?;
    validate_max_chunks(cfg.max_chunks)?;
    validate_sections(cfg)?;
    Ok(cfg.clone())
}

fn validate_worker_top_n(v: i32) -> Result<(), AppError> {
    if !(WORKER_TOP_N_MIN..=WORKER_TOP_N_MAX).contains(&v) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!(
                "「最多列出工人」必须在 {WORKER_TOP_N_MIN}..{WORKER_TOP_N_MAX} 之间（0 = 不限），收到 {v}"
            ),
        ));
    }
    Ok(())
}

/// 第三节清单每组行数校验（**与 `worker_top_n` 同款**：下界 `0` = 不限）。
///
/// ⚠️ 保存路径是 **clamp**（见 `validate_save_request`），只有**合并覆盖**路径
/// （预览 / 发送的临时覆盖）才拒 —— 与 `upcoming_days` 的分界同理：覆盖值是用户在
/// 预览框里现填的，夹到边界会让预览结果与他的输入不符。
fn validate_order_list_limit(v: i32) -> Result<(), AppError> {
    if !(ORDER_LIST_LIMIT_MIN..=ORDER_LIST_LIMIT_MAX).contains(&v) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!(
                "「交期清单每组最多列出」必须在 {ORDER_LIST_LIMIT_MIN}..{ORDER_LIST_LIMIT_MAX} 之间（0 = 不限），收到 {v}"
            ),
        ));
    }
    Ok(())
}

/// ⚠️ **上界 20 不是随手取的整数**：企业微信官方限频是「每个消息推送 ≤ 20 条/分钟」
/// （`wecom::sender` 模块 doc 里登记了同一条）。本域**在保存配置时**就拒它，而不是
/// 让用户在真推送时被企业微信限频。
fn validate_max_chunks(v: i16) -> Result<(), AppError> {
    if !(MAX_CHUNKS_MIN..=MAX_CHUNKS_MAX).contains(&v) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!(
                "「最多拆几条」必须在 {MAX_CHUNKS_MIN}..{MAX_CHUNKS_MAX} 之间\
                 （企业微信官方限频：每个消息推送 ≤ 20 条/分钟），收到 {v}"
            ),
        ));
    }
    Ok(())
}

/// 四节不能同时关（那是一条空消息，渲染出来没有任何信息量）。
///
/// ⚠️ 拦的是「**全**关」，**不是**「只能开一个」—— 四节都开是缺省形态。
fn validate_sections(cfg: &NormalizedConfig) -> Result<(), AppError> {
    if !cfg.include_overview && !cfg.include_delivery && !cfg.include_orders && !cfg.include_workers
    {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            "四节内容至少要开启一节，否则推送出去是一条空消息",
        ));
    }
    Ok(())
}

// ============================================================================
// 预览 / 发送
// ============================================================================

/// 把「当前配置 + 临时覆盖」合并成一个 [`NormalizedConfig`]。
///
/// 覆盖是**逐字段**的：`None` 表示「沿用当前值」。合并后**重新跑一遍校验** ——
/// 覆盖值可能让原本合法的配置变得非法（比如把 `max_chunks` 改成 99）。
///
/// ⚠️ `webhook_id` **不在覆盖范围内**（[`PreviewRequest`] 的 doc）：推到哪个群是
/// 一条**配置**决策，不该被「预览一次」顺带改掉。
pub async fn effective_config(
    conn: &mut PgConnection,
    overrides: &PreviewRequest,
) -> Result<NormalizedConfig, AppError> {
    let base = current_normalized(conn).await?;
    let merged = NormalizedConfig {
        enabled: overrides.enabled.unwrap_or(base.enabled),
        frequency: match overrides.frequency.as_deref() {
            Some(f) => f.parse()?,
            None => base.frequency,
        },
        send_hour: overrides.send_hour.or(base.send_hour),
        send_minute: overrides.send_minute.or(base.send_minute),
        weekday: overrides.weekday.or(base.weekday),
        day_of_month: overrides.day_of_month.or(base.day_of_month),
        webhook_id: base.webhook_id,
        upcoming_days: overrides.upcoming_days.unwrap_or(base.upcoming_days),
        worker_metric: match overrides.worker_metric.as_deref() {
            Some(m) => m.parse()?,
            None => base.worker_metric,
        },
        worker_top_n: overrides.worker_top_n.unwrap_or(base.worker_top_n),
        order_list_limit: base.order_list_limit,
        max_chunks: overrides.max_chunks.unwrap_or(base.max_chunks),
        include_overview: overrides.include_overview.unwrap_or(base.include_overview),
        include_delivery: overrides.include_delivery.unwrap_or(base.include_delivery),
        include_orders: overrides.include_orders.unwrap_or(base.include_orders),
        include_workers: overrides.include_workers.unwrap_or(base.include_workers),
        include_drawings: base.include_drawings,
        subject_template: overrides.subject_template.clone().or(base.subject_template),
    };
    validate_normalized(&merged)
}

/// 读当前配置并规范化（表里无行 ⇒ 全部缺省）。
pub async fn current_normalized(conn: &mut PgConnection) -> Result<NormalizedConfig, AppError> {
    let Some(row) = repo::get_config(conn).await? else {
        return Ok(NormalizedConfig::default());
    };
    normalize_row(&row)
}

/// DB 行 → [`NormalizedConfig`]（解析枚举）。
fn normalize_row(row: &TWeComDailyConfigRow) -> Result<NormalizedConfig, AppError> {
    Ok(NormalizedConfig {
        enabled: row.enabled,
        frequency: row.frequency.parse()?,
        send_hour: row.send_hour,
        send_minute: row.send_minute,
        weekday: row.weekday,
        day_of_month: row.day_of_month,
        webhook_id: row.webhook_id,
        upcoming_days: row.upcoming_days,
        worker_metric: row.worker_metric.parse()?,
        worker_top_n: row.worker_top_n,
        order_list_limit: row.order_list_limit,
        max_chunks: row.max_chunks,
        include_overview: row.include_overview,
        include_delivery: row.include_delivery,
        include_orders: row.include_orders,
        include_workers: row.include_workers,
        include_drawings: row.include_drawings,
        subject_template: row.subject_template.clone(),
    })
}

/// `POST /preview`：渲染但**不发送、不落流水**。
pub async fn preview(
    conn: &mut PgConnection,
    req: &PreviewRequest,
) -> Result<PreviewOut, AppError> {
    let cfg = effective_config(conn, req).await?;
    // 预览也要拦「四节全关」：否则渲出来是一条只有标题的空消息，管理员看不出是自己
    // 关错了还是后端坏了。
    validate_sections(&cfg)?;
    let today = now_naive().date();
    let window = report_window(&cfg, today);
    let rows = fetch_rows(conn, &cfg, window.0, window.1).await?;
    let rendered = template::render(&RenderInput {
        cfg: &cfg,
        rows: &rows,
        today,
        window,
    })?;
    let chunk_count = rendered.chunks.len() as i32;
    Ok(PreviewOut {
        subject: rendered.subject,
        chunks: rendered.chunks,
        chunk_count,
        overview_batch_count: rows.overview_batch_count,
        overview_quantity: rows.overview_quantity,
        completed_count: rows.completed_count,
        in_process_count: rows.in_process_count,
        // ⚠️ 第二节改单值口径后不再统计「送货单张数」⇒ `note_count` 恒 0（列保留是
        // 为了不动流水表结构与前端 `.strict()` 契约）。它不是「今天没出货」。
        note_count: 0,
        note_part_count: rows.note_part_count,
        overdue_count: rows.overdue_count,
        upcoming_count: rows.upcoming_count,
        worker_rows: rows.worker_rows.clone(),
        worker_row_total: rendered.worker_row_total,
        sections: rendered.sections,
    })
}

/// `POST /send`：手动触发（渲染 + 真发 + 落流水）。
///
/// ⚠️ 合并配置的连接**用完即还**（作用域块），`deliver` 内部自己按「借 3 次」的
/// 纪律借连接 —— 全程没有任何一段持有连接跨越网络等待（见模块 doc「连接纪律」）。
pub async fn send_manual(
    state: &Arc<AppState>,
    req: &PreviewRequest,
) -> Result<SendResultOut, AppError> {
    let cfg = {
        let mut conn = state.pool.acquire().await?;
        effective_config(&mut conn, req).await?
    };
    deliver(state, &cfg, TriggerSource::Manual).await
}

/// cron 定时触发（`task::wecom_daily` 调用）。
///
/// 与手动触发的差别只有 `TriggerSource`；失败**不**向上抛（后台任务没有调用方，抛了
/// 也只会被 tokio 吞掉），只 `warn!` + 记流水。
///
/// ## ⚠️ 「准备阶段」的失败**也**落流水
///
/// 准备段 = 取连接 → 读配置并规范化 → 解析推送通道。原本它失败就 `return`，
/// `deliver` 根本没被调用 ⇒ `t_wecom_daily_log` 里查不到任何东西，于是「定时任务每天
/// 都在失败」这件事**只剩一行会被轮转掉的 `tracing::warn!`**。可观测性的价值几乎全在
/// 流水表上（只增、带 `config_version` 与通道快照的审计表），所以准备段失败也补一行
/// `status='FAILED'`（`chunk_count=0`、`markdown_bytes=0`、各 count 为 0）。
///
/// 唯一落不了的失败是 `pool.acquire()` 本身 —— 没有连接就没有地方写。见
/// [`log_prepare_failure`]。
///
/// ⚠️ **每个准备段各自借一次连接、用完即还**（块作用域）；失败分支里的
/// [`log_prepare_failure`] 也**自己借**一条 —— 必须先还再借，否则池被本任务自己占满时
/// 落流水会二次 `acquire` 失败（读端点随即排队）。
pub async fn send_scheduled(state: &Arc<AppState>) {
    // ── 准备段 1/2：读配置并规范化 ──
    let normalized = {
        let mut conn = match state.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "wecom_daily: 定时日报读配置段取连接失败（无连接可落流水）");
                return;
            }
        };
        current_normalized(&mut conn).await
    };
    let cfg = match normalized {
        Ok(c) => c,
        Err(e) => {
            log_prepare_failure(state, &e).await;
            tracing::warn!(error = %e, "wecom_daily: 定时日报准备失败（读配置/解析），流水已落库");
            return;
        }
    };

    // ── 准备段 2/2：通道是否可用（与推送路径同一条 `resolve_target`，故判据一致）──
    let target_check = {
        let mut conn = match state.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "wecom_daily: 定时日报通道探测段取连接失败（无连接可落流水）");
                return;
            }
        };
        wecom_service::resolve_target(&mut conn, cfg.webhook_id).await
    };
    if let Err(e) = target_check {
        log_prepare_failure(state, &e).await;
        tracing::warn!(error = %e, "wecom_daily: 定时日报准备失败（推送通道不可用），流水已落库");
        return;
    }

    match deliver(state, &cfg, TriggerSource::Scheduled).await {
        Ok(out) => {
            tracing::info!(
                log_id = out.log_id,
                chunk_count = out.chunk_count,
                overview_batch_count = out.overview_batch_count,
                overdue_count = out.overdue_count,
                "wecom_daily: 定时日报已发出"
            );
        }
        Err(e) => tracing::warn!(error = %e, "wecom_daily: 定时日报推送失败（流水已落库）"),
    }
}

/// 「准备阶段」失败的流水：**没有**渲染产物（压根没分片）。
///
/// ⚠️ **入参刻意只有 `err`**（2026-10-19 删掉了原先的 `cfg: Option<&NormalizedConfig>`）：
/// 那份配置只被调用方拿去做 warn 文案区分（`config_version` 其实是本函数自己从**原始行**
/// 现捞的，压根不用它），落库的形态两种失败**完全一样**（内容全 0 + 通道名/掩码空串）⇒
/// 形参是纯噪音，留着只会让人以为「落库形态依赖 cfg」。两种失败的区分由**调用方的
/// `warn!` 文案**承担（「读配置/解析」vs「推送通道不可用」）。
///
/// `config_version` 尽力从**原始行**捞（表空 / **读取失败** ⇒ 记 0）：流水表的这几列是
/// **快照**（配置改了之后仍要看「当时用的是哪版配置」），拿不到时留 0 比编一个更诚实。
///
/// ⚠️ **刻意不走共用的 [`insert_log`]，自己读版本号再直调 [`repo::insert_log`]**：
/// 共用那条对版本号快照是**严格**的（读失败 ⇒ 整行不落），而本函数存在的意义恰恰是
/// 「失败也要留下痕迹」⇒ 这里的读失败必须降级成 `0`、INSERT 照发。两条路径对**同一个**
/// 失败取相反策略，理由各自不同（`deliver` 那条见 [`insert_log`] 的 doc），所以不给共用
/// helper 加「模式」形参：那样只会在严格那条调用点多一个开关，而严格性恰恰是不能由调用点
/// 随手决定的东西。⚠️ 仍然是**同一条借来的连接**上「读版本号 + INSERT」，不额外借还。
/// 源码级护栏见 `tests` 里 `log_prepare_failure_keeps_its_best_effort_version_snapshot`。
///
/// ⚠️ **自己借连接**（不接 `&mut PgConnection`）：调用方已经把连接还回池里了，
/// 带着形参会逼它在「准备失败 → 要落流水」这条路径上继续占着一条连接。
///
/// ⚠️ 本函数**永不**向上返回错误：它存在的意义是「失败也要留下痕迹」，若它自己失败
/// （**包括 `acquire` 本身失败**）就静默 `warn!`（调用方随后仍会 `warn!` 原始原因，两条
/// 日志互补）。
async fn log_prepare_failure(state: &Arc<AppState>, err: &AppError) {
    let mut conn = match state.pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "wecom_daily: 准备阶段失败流水取连接失败（无连接可落流水）");
            return;
        }
    };
    let now = now_ts();
    let log = NewDailyLog {
        trigger_source: TriggerSource::Scheduled.as_str(),
        // 通道都没解析出来 ⇒ 名称与掩码只能是空串（列是 NOT NULL）。留空而不是
        // 编一个：那两列是「发到哪个群」的快照，编出来的比没有更会误导排障。
        webhook_name: String::new(),
        webhook_masked: String::new(),
        markdown_bytes: 0,
        chunk_count: 0,
        chunk_sent: 0,
        overview_batch_count: 0,
        overview_quantity: 0,
        note_count: 0,
        note_part_count: 0,
        overdue_count: 0,
        upcoming_count: 0,
        worker_row_total: 0,
        status: SendStatus::Failed.as_str(),
        error_message: Some(truncate_chars(&err.to_string(), 500)),
        // ⚠️ `report_date` 与 `sent_at` **取自同一次 `now_ts()`**：两次独立取时钟
        // 会跨零点产出 `report_date != sent_at.date()` 的行，直接违反 `repo.rs`
        // 写死的那条不变量（前端「不建统计日列」的前提）。
        report_date: now.date(),
        sent_at: now,
    };
    let config_version = cfg_version_best_effort(cfg_version_of(&mut conn).await);
    let inserted = repo::insert_log(&mut conn, &state.snowflake, config_version, &log).await;
    match inserted {
        Ok(row) => tracing::info!(log_id = row.id, "wecom_daily: 准备阶段失败流水已落库"),
        Err(e) => tracing::warn!(error = %e, "wecom_daily: 准备阶段失败流水落库失败"),
    }
}

/// 按节开关取数（**关掉的节一条 SQL 都不发**）。
///
/// ## 不变量：**语句数与返回行数无关（O(1)）**
///
/// 每节都把整组聚合压进**一条**语句：第一节 1 条（报工 / 送检 / 在制在同一句里
/// 五个聚合）、第二节 1 条、第四节 1 个 `GROUP BY`；第三节是**清单**链路，固定
/// **7 条**（见 [`collect_order_groups`] 的表）。**本函数体内没有任何循环** ——
/// 工人榜的排序在下一句用 `sort_by` 做（内存操作）。
///
/// ⚠️ **不钉死具体条数**：这类计数必然过期，而它守的那条东西（循环体内不许取数）
/// 才是 O(1) 的真正来源。
///
/// ## 护栏
/// 「循环体内只许做内存操作、绝不调 `repo::` / `sqlx::query*`」由
/// `tests/wecom_daily/n_plus_one_guard.rs` 的**源码级**护栏钉住。
///
/// ⚠️ 逐节循环查是**功能完全正确、集成测试照样全绿**的写法（每条用例只造 2~3 行），
/// 只有数据量涨上去时语句数才悄悄线性涨 —— 而日报恰好是「一天一次、量很大」的生产形态。
///
/// ## 窗口
///
/// `(from_ts, to_ts)` = **跨天**窗口（`dto::report_window` 算，理由见那处）。
/// 第三节那两条交期查询仍吃**日历日** `today`（交期列本身是 `date`），这里由
/// `to_ts.date()` 派生 —— 与窗口上界同一个值 ⇒ 「发送日」不会在一次推送里出现两个。
async fn fetch_rows(
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    from_ts: NaiveDateTime,
    to_ts: NaiveDateTime,
) -> Result<ReportRows, AppError> {
    let mut rows = ReportRows::default();

    if cfg.include_overview {
        let ev = repo::overview_events(conn, from_ts, to_ts).await?;
        rows.overview_batch_count = ev.batch_count as i32;
        rows.overview_quantity = ev.quantity as i32;
        rows.completed_count = ev.completed as i32;
        rows.sent_batch_count = ev.sent as i32;
        rows.in_process_count = ev.wip as i32;
    }

    if cfg.include_delivery {
        let d = repo::delivery_note_summary(conn, from_ts, to_ts).await?;
        rows.note_part_count = d.part_count as i32;
    }

    if cfg.include_orders {
        let today = to_ts.date();
        rows.order_groups = collect_order_groups(conn, cfg, today).await?;
        // ⚠️ 两个计数字段取**未截断的匹配工单数**（`OrderGroup::total` 之和），与
        // 标题占位符 `{overdue_count}` 和流水快照列同源 —— 它们报的是「有几单要催」，
        // 不是「这条消息里写了几行」。清单一行的清单不在这两个字段里。
        rows.overdue_count = rows
            .order_groups
            .iter()
            .filter(|g| g.kind == OrderGroupKind::Overdue)
            .map(|g| g.total)
            .sum();
        rows.upcoming_count = rows
            .order_groups
            .iter()
            .filter(|g| matches!(g.kind, OrderGroupKind::OnDate(_)))
            .map(|g| g.total)
            .sum();
    }

    if cfg.include_workers {
        rows.worker_rows = repo::worker_rows(conn, from_ts, to_ts).await?;
        // ⚠️ 排序在**内存**里做（SQL 不认识「按配置选指标」这件事）。三条排序键
        // 逐条有理由：指标降序（条形图要「最多的排前面」）/ 姓名升序（同指标时的稳定
        // 次序，免得两次推送的行序随机跳）/ 工号升序（同名兜底，保证确定性）。
        let metric = cfg.worker_metric;
        rows.worker_rows.sort_by(|a, b| {
            metric
                .value_of(b)
                .cmp(&metric.value_of(a))
                .then_with(|| a.worker_name.cmp(&b.worker_name))
                .then_with(|| a.badge_code.cmp(&b.badge_code))
        });
    }

    Ok(rows)
}

// ============================================================================
// 第三节：交期清单的取数编排（**恒 7 条语句**）
// ============================================================================

/// 第三节的取数编排 + 分组 + 截断。
///
/// ## 语句清单（**恒 7 条**，与工单数 / 子件数 / 装配件数 / 货架数无关）
///
/// | # | 语句 | 入参形状 | 入参为空时 |
/// |---|---|---|---|
/// | 1 | [`repo::list_overdue`] | `today` | 照发（它自己出 `total = 0`） |
/// | 2 | [`repo::list_upcoming`] | `today` + `upcoming_days` | 照发 |
/// | 3 | [`repo::list_assembly_children`] | 清单里 `row_type='ASSEMBLY'` 的 id | **跳过** |
/// | 4 | [`repo::list_active_batches`] | 散件工单 id + 全部子件 id | **跳过** |
/// | 5 | [`repo::shelf_codes`] | 目标批次里 `PRODUCTION_SHELF` / `INSPECTION_SHELF` 的 holder id | **跳过** |
/// | 6 | [`repo::worker_names`] | 目标批次里 `WORKER` 的 holder id | **跳过** |
/// | 7 | [`repo::outsource_names`] | 目标批次里 `OUTSOURCE_COMPANY` 的 holder id | **跳过** |
///
/// ⚠️ 图纸直链那一环（[`repo::list_drawing_files`]）**本轮不接**：直链由 `files` 域的
/// 预览入口生成，接上之后它就是第 8 条，接缝是 [`ReportRows::drawing_links`]。
///
/// ## ⚠️ **位置只对 `t_part` 侧的 id 解析，装配件行恒不显示位置**
///
/// `t_part_batch.part_id` 指向的是 **`t_part`**（批次挂在零件上），而装配件在
/// `t_assembly` 里、`t_part.assembly_id` 指的是它的子件 ⇒ **装配件自己没有批次**。
/// 「这套装配件现在在哪儿」这件事由**子件行逐条**回答（这正是清单要展开子件的原因），
/// 所以第 4 条语句的入参是「散件工单 id + 全部子件 id」，不含装配件 id。
///
/// ## ⚠️ 为什么子件**不**再过一遍状态 / 交期 / 已交量守卫
///
/// 装配件那一支的 `NOT EXISTS` 已经经 `c.assembly_id = a.id` 扫过它**全部**子件的
/// 批次（见 `repo::list_overdue` 的要点 4）⇒ **装配件能进清单，就意味着它的子件一件
/// 都没被交过**。再加一道子件级守卫既重复，又会在守卫口径漂移时给出「装配件在、子件
/// 不在」这种自相矛盾的清单。
async fn collect_order_groups(
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    today: NaiveDate,
) -> Result<Vec<OrderGroup>, AppError> {
    let overdue = repo::list_overdue(conn, today).await?;
    let upcoming = repo::list_upcoming(conn, today, cfg.upcoming_days).await?;

    // 装配件 id（两条查询的并集）—— 子件展开的入参。
    let asm_ids: Vec<i64> = std::iter::once(&overdue)
        .chain(std::iter::once(&upcoming))
        .flat_map(|l| l.rows.iter())
        .filter(|r| r.is_assembly())
        .map(|r| r.id)
        .collect();
    let children = repo::list_assembly_children(conn, &asm_ids).await?;
    let mut children_by_asm: HashMap<i64, Vec<AssemblyChildRow>> = HashMap::new();
    for c in &children {
        children_by_asm
            .entry(c.assembly_id)
            .or_default()
            .push(c.clone());
    }

    // 活跃批次 → min-progress 目标批次 → `part_id → (location, holder_id)`。
    // ⚠️ 入参**不含装配件 id**（见本函数 doc 的「位置只对 t_part 侧解析」）。
    let part_ids: Vec<i64> = std::iter::once(&overdue)
        .chain(std::iter::once(&upcoming))
        .flat_map(|l| l.rows.iter())
        .filter(|r| !r.is_assembly())
        .map(|r| r.id)
        .chain(children.iter().map(|c| c.id))
        .collect();
    let batches = repo::list_active_batches(conn, &part_ids).await?;
    let mut batches_by_part: HashMap<i64, Vec<ActiveBatchRow>> = HashMap::new();
    for b in &batches {
        batches_by_part
            .entry(b.part_id)
            .or_default()
            .push(b.clone());
    }
    let targets: HashMap<i64, (Option<String>, Option<i64>)> = batches_by_part
        .iter()
        .filter_map(|(part_id, bs)| {
            repo::active_batch_target(bs)
                .map(|b| (*part_id, (b.location.clone(), b.current_holder_id)))
        })
        .collect();

    // 三个 holder 桶各查一次（`BTreeSet` 只为让入参顺序确定，便于复现与排障）。
    let mut shelf_ids = BTreeSet::new();
    let mut worker_ids = BTreeSet::new();
    let mut outsource_ids = BTreeSet::new();
    for (location, holder_id) in targets.values() {
        let Some(hid) = holder_id else { continue };
        match location.as_deref() {
            Some("PRODUCTION_SHELF") | Some("INSPECTION_SHELF") => {
                shelf_ids.insert(*hid);
            }
            Some("WORKER") => {
                worker_ids.insert(*hid);
            }
            Some("OUTSOURCE_COMPANY") => {
                outsource_ids.insert(*hid);
            }
            _ => {}
        }
    }
    let shelf = repo::shelf_codes(conn, &shelf_ids.into_iter().collect::<Vec<_>>()).await?;
    let worker = repo::worker_names(conn, &worker_ids.into_iter().collect::<Vec<_>>()).await?;
    let outsource =
        repo::outsource_names(conn, &outsource_ids.into_iter().collect::<Vec<_>>()).await?;

    // 摊平成「(归组键, 行)」。⚠️ 图纸直链恒缺省（接缝见 `ReportRows::drawing_links`）：
    // `drawing_links` 为空 ⇒ 每行的名称都是纯文字，其余形态一字不变。
    let ctx = RowContext {
        targets: &targets,
        shelf: &shelf,
        worker: &worker,
        outsource: &outsource,
        links: &rows_drawing_links(),
    };
    let mut flat: Vec<(OrderGroupKind, OrderRow)> = Vec::new();
    flatten_orders(
        &mut flat,
        &overdue,
        |_| OrderGroupKind::Overdue,
        &mut children_by_asm,
        &ctx,
    );
    flatten_orders(
        &mut flat,
        &upcoming,
        OrderGroupKind::OnDate,
        &mut children_by_asm,
        &ctx,
    );

    Ok(group_and_cap(
        flat,
        overdue.total as i32,
        cfg.order_list_limit,
    ))
}

/// 图纸直链映射的**接缝取值**（本轮恒空）。
///
/// 抽成一个函数而不是直接写 `&HashMap::new()`：接链那一轮只改这一个函数的实现
/// （并在其前面加上取数与填充），调用点与渲染层**一字不改**。
fn rows_drawing_links() -> HashMap<i64, String> {
    // ⚠️ 2026-10-23：直链由 `files` 域的预览入口生成（`t_part_file` → `t_file_preview`
    // → 直链），那一轮接上后本函数改为「查 → 填」。在它接上之前，第三节只出文字行。
    HashMap::new()
}

/// 渲染一行的**解析器**：位置（`part_id → min-progress 目标批次的 holder 名`）与图纸
/// 直链（`part_id → URL`）。
///
/// ⚠️ 打成结构体是因为 [`flatten_orders`] 要把同一套解析器递给**每一次**调用：拆成 5 个
/// 形参会在两个调用点上重复 5 遍，而「位置按哪张表解析」这件事只该有一处判据。
struct RowContext<'a> {
    /// `part_id → (location, holder_id)`，来自 min-progress 的目标活跃批次。
    targets: &'a HashMap<i64, (Option<String>, Option<i64>)>,
    shelf: &'a HashMap<i64, String>,
    worker: &'a HashMap<i64, String>,
    outsource: &'a HashMap<i64, String>,
    /// `part_id → 图纸预览直链`（接缝，见 [`rows_drawing_links`]）。
    links: &'a HashMap<i64, String>,
}

impl RowContext<'_> {
    /// 位置的中文展示值；**取不到就是 `None`**（渲染层据此整项不显示）。
    fn location(&self, part_id: i64) -> Option<String> {
        let (location, holder) = self.targets.get(&part_id)?;
        holder_name(
            location.as_deref(),
            *holder,
            self.shelf,
            self.worker,
            self.outsource,
        )
    }

    /// 图纸直链；**键缺失就是 `None`**（名称渲成纯文字）。
    fn link(&self, id: i64) -> Option<String> {
        self.links.get(&id).cloned()
    }
}

/// 把一次清单查询的行摊平进 `flat`，**装配件的子件紧随父行**。
///
/// `kind_of` 把交期日映射成归组键：逾期组恒同一个键（闭包忽略入参），日期组用这一行
/// 自己的交期日。子件与父行**同键** —— 一个子件的交期由父装配件代表，两者不分组。
///
/// `children_by_asm` 是**取走式**的（`remove`）：每个装配件的子件只会被挂一次，
/// 留着会让后面重复挂载。
fn flatten_orders(
    flat: &mut Vec<(OrderGroupKind, OrderRow)>,
    list: &repo::OrderList,
    kind_of: impl Fn(NaiveDate) -> OrderGroupKind,
    children_by_asm: &mut HashMap<i64, Vec<AssemblyChildRow>>,
    ctx: &RowContext<'_>,
) {
    for r in &list.rows {
        let kind = kind_of(r.system_delivery_date);
        flat.push((
            kind,
            OrderRow {
                is_child: false,
                serial_no: r.serial_no.clone(),
                name: r.name.clone(),
                quantity: r.quantity,
                link: ctx.link(r.id),
                location: ctx.location(r.id),
            },
        ));
        for c in children_by_asm.remove(&r.id).unwrap_or_default() {
            flat.push((
                kind,
                OrderRow {
                    is_child: true,
                    serial_no: c.serial_no,
                    name: c.name,
                    quantity: c.quantity,
                    link: ctx.link(c.id),
                    location: ctx.location(c.id),
                },
            ));
        }
    }
}

/// 三个 holder 表按 `location` 分桶取名，**取不到就整项不显示**（`None`）。
///
/// ⚠️ 「取不到」有三种成因，都是同一个渲染后果（这一项不出现在行里）：
/// holder id 为 NULL（还没上架）/ 该行已软删（查不到）/ `location` 不在四种之内
/// （`OFFICE` 与 `None`）。
fn holder_name(
    location: Option<&str>,
    holder_id: Option<i64>,
    shelf: &HashMap<i64, String>,
    worker: &HashMap<i64, String>,
    outsource: &HashMap<i64, String>,
) -> Option<String> {
    let hid = holder_id?;
    let name = match location? {
        "PRODUCTION_SHELF" | "INSPECTION_SHELF" => shelf.get(&hid),
        "WORKER" => worker.get(&hid),
        "OUTSOURCE_COMPANY" => outsource.get(&hid),
        _ => None,
    }?;
    location_display(location, Some(name))
}

/// 位置的中文展示值：`生产架:A-03` / `品检架:B-11` / `工人:张三` / `外协:某某`。
///
/// ⚠️ **`OFFICE` / `location IS NULL` 一律 `None`**：办公室不是「这件在哪儿」这个问题的
/// 答案（它回答的是「还没谁来领」，属领料视角），把它渲成位置会让人以为货真在办公室；
/// 而 `OFFICE` 那一档恰好又是「今天有一批件还没人动」的关键信号 —— 它属于提醒，不
/// 属于位置。
fn location_display(location: Option<&str>, holder_name: Option<&str>) -> Option<String> {
    let label = match location? {
        "PRODUCTION_SHELF" => "生产架",
        "INSPECTION_SHELF" => "品检架",
        "WORKER" => "工人",
        "OUTSOURCE_COMPANY" => "外协",
        _ => return None,
    };
    Some(format!("{label}:{}", holder_name?))
}

/// 摊平的行 → 组，并按 `order_list_limit` 扣额度（**纯函数**，不看库、不碰连接）。
///
/// ## 组的顺序（**在这里钉死，不依赖 SQL 的 `ORDER BY`**）
///
/// 逾期组第一，日期组按交期日升序。`sort_by_key` 是**稳定**排序 ⇒ 组内行的相对次序
/// 逐字保留（装配件行永远紧挨着它的子件行，即使输入被打乱过）。
///
/// ## 额度怎么扣
///
/// **工单行与子件行吃同一个额度**，`order_list_limit == 0` 表示不限。
/// ⚠️ 额度扣的是**行**不是工单：一个装配件 + 30 个子件会一次吃掉 31 的额度 —— 这是
/// 有意的（额度是「这条消息最多写多少行」的直译），而子件截断**不单独提示**，只在组末
/// 出**一行**脚注「另有 N 条未列出」（N = 未列出的行数）。
///
/// `overdue_total` 单列传入（而不是从 `flat` 数）：它是 SQL 的
/// `COUNT(*) OVER ()`，即便将来主查询加了 `LIMIT` 也仍然是未截断的匹配总数。
fn group_and_cap(
    flat: Vec<(OrderGroupKind, OrderRow)>,
    overdue_total: i32,
    limit: i32,
) -> Vec<OrderGroup> {
    let mut by_kind: Vec<(OrderGroupKind, Vec<OrderRow>)> = Vec::new();
    for (kind, row) in flat {
        match by_kind.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, rows)) => rows.push(row),
            None => by_kind.push((kind, vec![row])),
        }
    }
    // ⚠️ `NaiveDate::MIN` 只是占位：逾期组的排序键恒小于任何日期组。
    by_kind.sort_by_key(|(kind, _)| match kind {
        OrderGroupKind::Overdue => (0u8, NaiveDate::MIN),
        OrderGroupKind::OnDate(d) => (1u8, *d),
    });

    by_kind
        .into_iter()
        .map(|(kind, rows)| {
            let total = match kind {
                OrderGroupKind::Overdue => overdue_total,
                // 日期组的「N 条」= 该日命中的工单数（子件不算工单）。
                OrderGroupKind::OnDate(_) => rows.iter().filter(|r| !r.is_child).count() as i32,
            };
            let (rows, omitted) = cap_rows(rows, limit);
            OrderGroup {
                kind,
                rows,
                omitted,
                total,
            }
        })
        .collect()
}

/// 扣额度：`(留下的行, 未列出的行数)`。
///
/// ⚠️ `limit <= 0` 是**不限**（配置语义见 `dto::ORDER_LIST_LIMIT_MIN`），不是「一条都
/// 不列」—— 把 0 当成「留 0 条」会让「不限」这个配置项表达出与字面相反的意思。
fn cap_rows(rows: Vec<OrderRow>, limit: i32) -> (Vec<OrderRow>, i64) {
    if limit <= 0 {
        return (rows, 0);
    }
    let cap = limit as usize;
    if rows.len() <= cap {
        return (rows, 0);
    }
    let omitted = (rows.len() - cap) as i64;
    (rows.into_iter().take(cap).collect(), omitted)
}

/// 落一行流水（**版本号快照与 INSERT 合并在同一次借还里**，见模块 doc「连接纪律」）。
///
/// `config_version` 在此**现读**（`repo::get_config` → `.version`，表空 ⇒ `0`）：
/// 它是「落库这一刻」的快照，配置在推送途中被别人改过时流水要记**新**值。本函数**只**
/// 服务 [`deliver`] 一条路径 ⇒ 不存在「调用方塞了个陈旧版本」的路径。
///
/// ⚠️ **版本号读失败在此原样冒泡**（不降级）：`deliver` 里这行就是「推送失败后唯一的
/// 诊断信息载体」，静默吞掉等于把 `21802` 换成一个没人看的结果。
/// ⚠️ 另一条落流水路径（准备阶段失败）**刻意不走本函数**、自己读且降级成 `0` —— 那边
/// 的存在意义是「失败也要留下痕迹」，对同一个失败取相反策略；理由与写法见
/// [`log_prepare_failure`]。
async fn insert_log(
    state: &Arc<AppState>,
    conn: &mut PgConnection,
    log: NewDailyLog,
) -> Result<TWeComDailyLogRow, AppError> {
    let config_version = cfg_version_of(conn).await?;
    Ok(repo::insert_log(conn, &state.snowflake, config_version, &log).await?)
}

/// 读当前配置行版本（流水快照用；表空时记 `0`）。**读失败也返回 `Err`** —— 由调用方决定
/// 拿它怎么办（[`deliver`] 原样冒泡 / [`log_prepare_failure`] 降级，见两处 doc）。
async fn cfg_version_of(conn: &mut PgConnection) -> Result<i32, AppError> {
    Ok(repo::get_config(conn)
        .await?
        .map(|r| r.version)
        .unwrap_or(0))
}

/// 版本号快照的**尽力**读法：读失败降级 `0`（纯函数，吃读出来的 `Result`，不碰连接）。
///
/// ⚠️ **只有 [`log_prepare_failure`] 走它**（`deliver` 走 [`insert_log`] 的严格版）。
/// 降级口径与「表空」同形（都记 `0`）：`config_version` 是审计快照列，宁可少一个版本号，
/// 也不能因为读不到版本号就把**整行**痕迹丢掉 —— 那正是这条路径存在的理由被抽掉时
/// 会出现的那种静默（只多一行会被轮转掉的 `warn!`，`t_wecom_daily_log` 里一行都没有）。
fn cfg_version_best_effort(read: Result<i32, AppError>) -> i32 {
    read.unwrap_or(0)
}

/// 状态映射的**纯函数**（`chunk_sent < chunk_count ⟺ FAILED` 是前端可依赖的不变量）。
///
/// ⚠️ **`chunk_count == 0` 这一档不走本函数**：准备阶段失败（`0 / 0`）也必须是
/// `FAILED`，而按本函数的判据它是 `Success` ⇒ 那一行由 [`log_prepare_failure`] 显式
/// 写 `FAILED`。换句话说本函数只管**渲染成功之后**的状态。
pub fn status_for(chunk_count: i32, chunk_sent: i32) -> SendStatus {
    if chunk_sent < chunk_count {
        SendStatus::Failed
    } else {
        SendStatus::Success
    }
}

/// 渲染 → 逐片顺序推送 → **无论成败都**落流水。
///
/// 返回 `Ok(SendResultOut)` 表示已全部发出；`Err(21802)` 表示某片推送失败（此时
/// `t_wecom_daily_log` 已经有了一行 `status='FAILED'` + `error_message`，**诊断信息
/// 不会因为返错而丢失** —— 这是「失败也落流水」的全部意义）。
///
/// ## 顺序发送、绝不并发、**不重发已成功的片**
///
/// 企业微信**没有批量接口**（一次只能推一条），而群里必须先出现 `1/N` 再出现 `2/N`
/// ⇒ 顺序发。**不重发**已成功的片：重发会在群里出现重复内容，比缺一半更糟。
///
/// ## ⚠️ **连接纪律**：只在三处借、用完即还
///
/// 本函数**不再接收 `&mut PgConnection`**（2026-10-19）：投递期间握着连接等 ≈100s 的
/// 网络会把连接池占满、读端点跟着排队（`/send` 无频控 ⇒ 几个管理员同时点就成立）。
/// 连接纪律见模块 doc，落在下面三段块作用域里：
/// ① 解析通道 ② 取数 ③ 落流水 —— **③ 之后的逐片发送全程零 DB IO、零连接**。
async fn deliver(
    state: &Arc<AppState>,
    cfg: &NormalizedConfig,
    source: TriggerSource,
) -> Result<SendResultOut, AppError> {
    // 通道解析排在节开关检查**之前**（更基础的失败先报，见模块 doc）。
    // 借还 1/3：`ResolvedTarget` 的四个字段全是 `String`/`i64`，还连接之后照样能读。
    let target = {
        let mut conn = state.pool.acquire().await?;
        wecom_service::resolve_target(&mut conn, cfg.webhook_id).await?
    };
    validate_sections(cfg)?;

    let now = now_ts();
    let today = now.date();
    // 统计窗口（跨天）：两端都从**同一份配置**推出来，标题块与 SQL 共用它一个值。
    let window = report_window(cfg, today);
    // 借还 2/3：恒 O(1) 条 SQL（关掉的节一条都不发，见 `fetch_rows`）。
    let rows = {
        let mut conn = state.pool.acquire().await?;
        fetch_rows(&mut conn, cfg, window.0, window.1).await?
    };
    let rendered = template::render(&RenderInput {
        cfg,
        rows: &rows,
        today,
        window,
    })?;
    // ⚠️ 上面这一行返回 `Err(21804)` 时**不落流水**：没发出去，不是投递失败。
    // 前端 `/send` 失败后正是靠「有没有流水行」区分这两类。
    let chunks = rendered.chunks;
    let markdown_bytes = chunks.iter().map(|c| c.len() as i32).sum::<i32>();

    // ── 纯网络等待段（≈ max_chunks × 5s）：**不持有任何连接** ──
    let mut chunk_sent = 0i32;
    let mut failure: Option<(usize, String)> = None;
    for (idx, chunk) in chunks.iter().enumerate() {
        match sender::send_markdown(&target.url, chunk).await {
            sender::SendOutcome::Ok => chunk_sent += 1,
            sender::SendOutcome::Failed(msg) => {
                failure = Some((idx + 1, msg));
                break;
            }
        }
    }

    let status = status_for(chunks.len() as i32, chunk_sent);
    let error_message = failure
        .as_ref()
        .map(|(i, msg)| truncate_chars(&format!("第 {i}/{} 条失败：{msg}", chunks.len()), 500));

    // 借还 3/3：版本号快照 + INSERT（`insert_log` 内部合并成一次借还）。
    let mut conn = state.pool.acquire().await?;
    let log = insert_log(
        state,
        &mut conn,
        NewDailyLog {
            trigger_source: source.as_str(),
            webhook_name: target.name.clone(),
            webhook_masked: target.masked.clone(),
            report_date: today,
            markdown_bytes,
            chunk_count: chunks.len() as i16,
            chunk_sent: chunk_sent as i16,
            overview_batch_count: rows.overview_batch_count,
            overview_quantity: rows.overview_quantity,
            // ⚠️ `note_count` 恒 0：第二节改单值口径后不再统计「送货单张数」，而流水
            // 表那一列是 `NOT NULL`（理由见 `dto::SendResultOut::note_count`）。
            note_count: 0,
            note_part_count: rows.note_part_count,
            overdue_count: rows.overdue_count,
            upcoming_count: rows.upcoming_count,
            worker_row_total: rendered.worker_row_total,
            status: status.as_str(),
            error_message,
            // ⚠️ `report_date` 与 `sent_at` 取自**同一次** `now_ts()` 调用（`today`
            // 就是 `now.date()`）⇒ 「统计日恒等于发送时刻的日期」这条前端依赖的
            // 不变量成立，见 `repo::NewDailyLog::sent_at`。
            sent_at: now,
        },
    )
    .await?;
    drop(conn);

    // 只打名称与掩码，明文 URL 一个字都不进日志（见模块 doc「日志纪律」）。
    tracing::info!(
        log_id = log.id,
        status = log.status,
        webhook_name = %log.webhook_name,
        webhook_masked = %log.webhook_masked,
        chunk_count = log.chunk_count,
        chunk_sent = log.chunk_sent,
        markdown_bytes,
        "wecom_daily: 推送完成"
    );

    match failure {
        None => Ok(SendResultOut::from_row(&log)),
        // 裸文本已经带了失败原因，这里只补「流水已记录 + log_id」这条**排障指针**。
        Some((_, msg)) => Err(AppError::biz(
            code::BIZ_WECOM_SEND_FAILED,
            format!("{msg}（已记入推送流水，log_id={}）", log.id),
        )),
    }
}

/// 按**字符**截断（不是字节）—— 字节截断可能把一个多字节汉字劈成非法 UTF-8，
/// 那是 PG 层的编码错误，比超长更难查。
///
/// ⚠️ **`max` 是含省略号的**总长上限：`take(max)` 之后再 `push('…')` 会产出
/// `max + 1` 个字符，而两个调用点的 `max` 都是列宽 `varchar(500)` ⇒ 那会撞 PG
/// `22001`（value too long），把本该报 `21802` / 其它码的失败换成一条
/// `50001 DATABASE` 并**丢掉整行流水**（正是 `repo.rs` 那条「`error_message`
/// 必须截到列宽内」的警告形态）。所以先给省略号**留一位**：
/// `take(max - 1)` + `push('…')` = 恰好 `max` 字。
///
/// ⚠️ **`max == 0` 早退成空串**（不是 1 个省略号）：`max.saturating_sub(1)` 在 `max = 0`
/// 时得 0，再 `push('…')` 仍是 1 字 > `max` ⇒ 「输出恒 ≤ `max`」这条不变式会被破坏。
/// 选**运行期早退**而不是 `debug_assert!`：这条不变式是本函数 doc 的**对外承诺**
/// （PG 列宽是硬边界），release 构建里也必须成立；而形如 `varchar(0)` 的列在仓内
/// 不存在，但形参允许 0 ⇒ 早退是「不承诺的事就当场说清」，比让它在测试之外悄悄违反
/// 承诺好。当前两个调用点都硬编码 `500`，这一档**不可达**，是给将来第三个调用点留的。
fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ============================================================================
// 纯函数（一致性校验的纯形态，供 lib 单测直接调）
// ============================================================================

/// 各工人 `quantity` 之和。
///
/// ## ⚠️ 这条等式的**边界**：软删工人
///
/// `overview_quantity` 与「各工人 `quantity` 之和」在**有事件的工人均未被软删**时
/// 恒等 —— 因为 [`repo::overview_events`] 的 `JOIN t_worker` 不带软删闸门，而
/// [`repo::worker_rows`] 带（`w.deleted_at IS NULL`）⇒ 一个「当天有事件、但工人行
/// 已被软删」的行会**只进前者**。
///
/// 处置：**如实登记，不偷偷对齐**（对齐哪一侧都会让那一侧与它的源域
/// `statistics` / `wx` 脱节）。登记见 `docs/api/wecom-daily.md` §8.3 第 4 条。
pub fn worker_quantity_total(rows: &[crate::modules::wecom_daily::dto::WorkerBarRow]) -> i32 {
    rows.iter().map(|r| r.quantity).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::wecom_daily::dto::{
        MAX_CHUNKS_DEFAULT, ORDER_LIST_LIMIT_DEFAULT, UPCOMING_DAYS_DEFAULT, WORKER_TOP_N_DEFAULT,
        WorkerBarRow, WorkerMetric,
    };

    fn row(name: &str, batch: i32, quantity: i32) -> WorkerBarRow {
        WorkerBarRow {
            worker_name: name.to_string(),
            badge_code: format!("W-{name}"),
            batch_count: batch,
            quantity,
        }
    }

    /// ★ 守卫 ⑮：**全厂送件数 == 各工人送件数之和**。
    ///
    /// 用**纯函数**实现（构造两组入参，不连库）：这条等式是「第一节总数」与
    /// 「第四节分项」的对账关系，前端预览弹窗把两个数并排显示，出错时管理员一眼
    /// 看得出问题。
    /// 工时是另一个字段（且本域根本不取工时），不参与这条等式。
    #[test]
    fn factory_quantity_equals_sum_of_worker_quantities() {
        let worker_rows = vec![row("甲", 3, 100), row("乙", 2, 25), row("丙", 1, 7)];
        let total = worker_quantity_total(&worker_rows);
        assert_eq!(total, 132);

        let rows = ReportRows {
            overview_quantity: total,
            worker_rows,
            ..ReportRows::default()
        };
        assert_eq!(rows.worker_quantity_total(), rows.overview_quantity);

        // 逐条数值也要对上（防止「求和函数本身算错」这一种形态）。
        let mut acc = 0;
        for r in &rows.worker_rows {
            assert!(r.quantity <= 100);
            acc += r.quantity;
        }
        assert_eq!(acc, rows.overview_quantity);
    }

    /// 同上，但**构造不一致的入参** ⇒ 必须能算出差值（这条才有诊断价值：
    /// 静默相等说明这条校验是死的）。
    #[test]
    fn worker_quantity_total_detects_soft_deleted_worker_divergence() {
        // 「工人行已被软删、但当天有事件」的那一行：只在第一节计入、不在第四节。
        let rows = ReportRows {
            overview_quantity: 132,
            worker_rows: vec![row("甲", 3, 100), row("乙", 2, 25)],
            ..ReportRows::default()
        };
        assert_eq!(
            rows.overview_quantity - rows.worker_quantity_total(),
            7,
            "软删工人的 7 件会让两条 SQL 不等 —— 已知偏差，见 docs §8.3 第 4 条"
        );
    }

    /// ★ 守卫 ⑯：**`chunk_sent < chunk_count ⟺ status == FAILED`**。
    #[test]
    fn chunk_sent_lt_count_implies_failed() {
        assert_eq!(status_for(1, 1), SendStatus::Success);
        assert_eq!(status_for(3, 3), SendStatus::Success);
        assert_eq!(status_for(3, 0), SendStatus::Failed, "一片都没发出去");
        assert_eq!(status_for(3, 2), SendStatus::Failed, "发到第 2 片失败");
        // ⚠️ `0 / 0` 走**不是**本函数的路径：准备阶段失败那一行由
        // `log_prepare_failure` 显式写 `FAILED`（见 `status_for` 的 doc）。
        assert_eq!(status_for(0, 0), SendStatus::Success);
    }

    /// 校验矩阵：四节全关 ⇒ `21804`（拦的是「全」关，不是「只能开一个」）。
    #[test]
    fn four_sections_all_off_is_21804() {
        let cfg = NormalizedConfig {
            include_overview: false,
            include_delivery: false,
            include_orders: false,
            include_workers: false,
            ..NormalizedConfig::default()
        };
        let e = validate_sections(&cfg).unwrap_err();
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
        assert!(e.to_string().contains("至少要开启一节"));

        // 四节都开是**合法**的（缺省形态）。
        assert!(validate_sections(&NormalizedConfig::default()).is_ok());
        // 任意三个开都合法。
        for i in 0..4 {
            let mut cfg = NormalizedConfig::default();
            match i {
                0 => cfg.include_overview = false,
                1 => cfg.include_delivery = false,
                2 => cfg.include_orders = false,
                _ => cfg.include_workers = false,
            }
            assert!(validate_sections(&cfg).is_ok(), "关掉第 {i} 节不该报错");
        }
    }

    /// 校验矩阵：`max_chunks` 越界与 `> 20` ⇒ `21804`（20 是官方限频）。
    #[test]
    fn max_chunks_bounds_are_21804() {
        assert!(validate_max_chunks(1).is_ok());
        assert!(validate_max_chunks(20).is_ok());
        assert!(validate_max_chunks(MAX_CHUNKS_DEFAULT).is_ok());
        assert!(validate_max_chunks(0).is_err());
        assert!(validate_max_chunks(-1).is_err());
        assert!(validate_max_chunks(21).is_err());
        let e = validate_max_chunks(21).unwrap_err();
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
        assert!(
            e.to_string().contains("20 条/分钟"),
            "要说明为什么是 20：{e}"
        );
    }

    /// 校验矩阵：`worker_top_n` 越界 ⇒ `21804`（`0` = 不限，合法）。
    #[test]
    fn worker_top_n_bounds_are_21804() {
        assert!(validate_worker_top_n(0).is_ok(), "0 = 不限");
        assert!(validate_worker_top_n(200).is_ok());
        assert!(validate_worker_top_n(15).is_ok());
        assert!(validate_worker_top_n(-1).is_err());
        assert!(validate_worker_top_n(201).is_err());
        assert_eq!(
            validate_worker_top_n(201).unwrap_err().code(),
            code::BIZ_WECOM_CONFIG_INVALID
        );
    }

    /// ★ 守卫：`order_list_limit` 与 `worker_top_n` 同款语义 —— `0` = 不限、越界即
    /// `21804`；而**保存路径**是 clamp（夹到边界，不是报错）。
    #[test]
    fn order_list_limit_bounds_are_21804_and_clamped_on_save() {
        assert!(validate_order_list_limit(0).is_ok(), "0 = 不限");
        assert!(validate_order_list_limit(30).is_ok());
        assert!(validate_order_list_limit(200).is_ok());
        let e = validate_order_list_limit(-1).unwrap_err();
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
        assert!(e.to_string().contains("0 = 不限"), "要说明 0 的语义：{e}");
        assert_eq!(
            validate_order_list_limit(201).unwrap_err().code(),
            code::BIZ_WECOM_CONFIG_INVALID
        );

        // 保存路径：越界被夹到边界（照 `upcoming_days` 的分界，见函数 doc）。
        let mk = |v: i32| SaveConfigRequest {
            version: 0,
            enabled: false,
            frequency: "DAILY".into(),
            send_hour: Some(20),
            send_minute: Some(5),
            weekday: None,
            day_of_month: None,
            webhook_id: None,
            upcoming_days: UPCOMING_DAYS_DEFAULT,
            worker_metric: "QUANTITY".into(),
            worker_top_n: WORKER_TOP_N_DEFAULT,
            order_list_limit: v,
            max_chunks: MAX_CHUNKS_DEFAULT,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
            include_drawings: true,
            subject_template: None,
        };
        assert_eq!(validate_save_request(&mk(-5)).unwrap().order_list_limit, 0);
        assert_eq!(
            validate_save_request(&mk(9999)).unwrap().order_list_limit,
            200
        );
    }

    /// 缺省 `max_chunks` 从 3 提到 6：第三节清单形态下每行 ≈135 字节（含一条 ≈95
    /// 字节的图纸预览直链），4 组 × 30 条 ≈ 16.4 KB ≈ 5 片 ⇒ 3 片装不下**完全按缺省
    /// 配置**生成的一份日报。20 是企业微信官方限频，6 是「装得下缺省」与「不刷屏」
    /// 之间的中间值。
    #[test]
    fn default_max_chunks_fits_the_default_order_list() {
        assert_eq!(MAX_CHUNKS_DEFAULT, 6);
        // 4 组 × 缺省 30 条 × 135 字节 ≈ 16.2 KB ⇒ 每片 4048 字节预算下 5 片装得下
        assert!(
            (4 * ORDER_LIST_LIMIT_DEFAULT * 135) as usize / (4096 - 48)
                < MAX_CHUNKS_DEFAULT as usize,
            "缺省配置生成的清单必须装得进缺省片数"
        );
        // ⚠️ 「6 仍在 `1..=20` 之内」这一条由 `max_chunks_bounds_are_21804` 里的
        // `validate_max_chunks(MAX_CHUNKS_DEFAULT).is_ok()` 守着（走函数，clippy 的
        // `assertions_on_constants` 才不会把重复的常量比较判成噪声）。
    }

    /// 校验矩阵：频率与配套字段不匹配 ⇒ `21804`。
    #[test]
    fn frequency_and_time_mismatch_is_21804() {
        // WEEKLY 缺 weekday
        let e = validate_save_request(&SaveConfigRequest {
            version: 0,
            enabled: false,
            frequency: "WEEKLY".into(),
            send_hour: Some(8),
            send_minute: Some(0),
            weekday: None,
            day_of_month: None,
            webhook_id: None,
            upcoming_days: UPCOMING_DAYS_DEFAULT,
            worker_metric: "QUANTITY".into(),
            worker_top_n: WORKER_TOP_N_DEFAULT,
            order_list_limit: ORDER_LIST_LIMIT_DEFAULT,
            max_chunks: 3,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
            include_drawings: true,
            subject_template: None,
        })
        .unwrap_err();
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);

        // MANUAL_ONLY 不需要时刻 ⇒ 合法，且不校验时刻范围。
        assert!(
            validate_save_request(&SaveConfigRequest {
                version: 0,
                enabled: false,
                frequency: "MANUAL_ONLY".into(),
                send_hour: Some(99),
                send_minute: Some(99),
                weekday: None,
                day_of_month: None,
                webhook_id: None,
                upcoming_days: UPCOMING_DAYS_DEFAULT,
                worker_metric: "QUANTITY".into(),
                worker_top_n: WORKER_TOP_N_DEFAULT,
                order_list_limit: ORDER_LIST_LIMIT_DEFAULT,
                max_chunks: 3,
                include_overview: true,
                include_delivery: true,
                include_orders: true,
                include_workers: true,
                include_drawings: true,
                subject_template: None,
            })
            .is_ok(),
            "MANUAL_ONLY 不该被要求填时刻"
        );
    }

    /// 校验矩阵：未知 `worker_metric` ⇒ `21804`（**不静默取默认值**）。
    #[test]
    fn unknown_worker_metric_is_21804() {
        let e = "HOURS"
            .parse::<WorkerMetric>()
            .expect_err("未知指标必须被拒");
        assert_eq!(e.code(), code::BIZ_WECOM_CONFIG_INVALID);
        assert!(e.to_string().contains("QUANTITY"));
        assert!(e.to_string().contains("BATCH"));
        // 两个合法值都要能解析。
        assert_eq!(
            "QUANTITY".parse::<WorkerMetric>().unwrap(),
            WorkerMetric::Quantity
        );
        assert_eq!(
            "BATCH".parse::<WorkerMetric>().unwrap(),
            WorkerMetric::Batch
        );
        assert_eq!(WorkerMetric::ALL.len(), 2);
    }

    /// 标题模板：≤ 200 **字** + 拒 CR/LF + 空串归 `None`。
    #[test]
    fn subject_template_validation() {
        assert_eq!(validate_subject_template(None).unwrap(), None);
        assert_eq!(validate_subject_template(Some("")).unwrap(), None);
        assert_eq!(validate_subject_template(Some("   ")).unwrap(), None);
        assert_eq!(
            validate_subject_template(Some(" # 标题 ")).unwrap(),
            Some("# 标题".to_string())
        );
        // 200 个汉字 = 600 字节，**按字节判会被误拒** ⇒ 这里证明按字判。
        let long = "标".repeat(200);
        assert_eq!(
            validate_subject_template(Some(long.as_str())).unwrap(),
            Some(long)
        );
        let too_long = "标".repeat(201);
        assert_eq!(
            validate_subject_template(Some(too_long.as_str()))
                .unwrap_err()
                .code(),
            code::BIZ_WECOM_CONFIG_INVALID
        );
        assert_eq!(
            validate_subject_template(Some("a\r\nb"))
                .unwrap_err()
                .code(),
            code::BIZ_WECOM_CONFIG_INVALID
        );
    }

    /// `upcoming_days` 在**保存路径**是 clamp（不是拒）。
    #[test]
    fn upcoming_days_is_clamped_on_save() {
        let mk = |days: i32| SaveConfigRequest {
            version: 0,
            enabled: false,
            frequency: "DAILY".into(),
            send_hour: Some(8),
            send_minute: Some(0),
            weekday: None,
            day_of_month: None,
            webhook_id: None,
            upcoming_days: days,
            worker_metric: "QUANTITY".into(),
            worker_top_n: WORKER_TOP_N_DEFAULT,
            order_list_limit: ORDER_LIST_LIMIT_DEFAULT,
            max_chunks: 3,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
            include_drawings: true,
            subject_template: None,
        };
        assert_eq!(validate_save_request(&mk(-5)).unwrap().upcoming_days, 1);
        assert_eq!(validate_save_request(&mk(999)).unwrap().upcoming_days, 60);
        // 但**合并路径**（预览 / 发送的临时覆盖）是拒（用户填的数要如实反馈）。
        let cfg = NormalizedConfig {
            upcoming_days: -5,
            ..NormalizedConfig::default()
        };
        assert_eq!(
            validate_normalized(&cfg).unwrap_err().code(),
            code::BIZ_WECOM_CONFIG_INVALID
        );
    }

    /// `truncate_chars` 按**字符**截断且不劈开多字节汉字。
    #[test]
    fn truncate_chars_never_splits_a_multibyte_char() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        let long = "汉".repeat(600);
        let t = truncate_chars(&long, 500);
        assert!(
            t.chars().count() <= 500,
            "截断后总长不得超过 max（含省略号）"
        );
        assert!(t.ends_with('…'));
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }

    /// ★ 守卫：`truncate_chars` 的**输出长度恒 ≤ `max`**（含省略号）—— 这一条钉的是
    /// `error_message` 的列宽 `varchar(500)`。
    ///
    /// ⚠️ **变异验证**：`take(max)` + `push('…')`（2026-10-19 修掉的那个形态）产出
    /// **501** 字 ⇒ PG `22001` ⇒ `repo::insert_log` 失败会把本该报的 `21802` 换成一条
    /// `50001 DATABASE`，**并且整行审计流水丢失**（正是 `repo.rs` 那条「`error_message`
    /// 必须截到列宽内」的警告形态）。把实现改回 `take(max)` 本用例立刻红。
    #[test]
    fn truncate_chars_never_exceeds_max_including_the_ellipsis() {
        for max in [1usize, 2, 10, 499, 500] {
            for len in [0usize, max - 1, max, max + 1, 600] {
                let src = "汉".repeat(len);
                let out = truncate_chars(&src, max);
                assert!(
                    out.chars().count() <= max,
                    "len={len} max={max} 时输出 {} 字，超了上限：{out:?}",
                    out.chars().count()
                );
                // 超长时必须有省略号，且总长**正好**等于 max（给省略号留了一位）。
                if len > max {
                    assert_eq!(out.chars().count(), max, "len={len} max={max}");
                    assert!(out.ends_with('…'), "len={len} max={max}：{out:?}");
                }
            }
        }
        // **真实调用点**的上限（`error_message` 列宽 `varchar(500)`）逐字钉住。
        let msg = format!("第 {}/{} 条失败：{}", 20, 20, "错因".repeat(400));
        let out = truncate_chars(&msg, 500);
        assert_eq!(out.chars().count(), 500, "必须恰好占满列宽而不是溢出 501");

        // ⚠️ `max == 0` 单独判（上面的循环用 `max - 1` 造入参，0 会下溢）：一个字符都
        // 放不下 ⇒ 空串。`max.saturating_sub(1)` + `push('…')` 那个形态在这里产出
        // **1 个省略号** > `max`，违反本函数 doc 的「输出恒 ≤ max」。
        for len in [0usize, 1, 600] {
            let src = "汉".repeat(len);
            assert_eq!(
                truncate_chars(&src, 0),
                "",
                "len={len} max=0：应为空串而不是 1 个省略号"
            );
        }
    }

    // ==========================================================================
    // 「准备阶段失败」的版本号快照：尽力读 + 绝不改走严格的 `insert_log`
    // ==========================================================================

    /// 把源码里的**注释**空格化（保留换行，便于 `panic!` 里报行号）。
    ///
    /// ⚠️ 只处理 `//` 与 `/* */`，**不**处理字符串字面量 —— 本文件现状零「字面量里含
    /// `//` 或 `/*`」的写法（护栏只扫 [`FN_LOG_PREPARE_FAILURE`] 一个函数体，且下面两条
    /// 断言匹配的是标识符/调用名，不是一般文本）。将来若在那个函数体里加了带 `//` 的
    /// 字面量，本护栏会误判 ⇒ 属**源码形态敏感**的已知局限，与 `tests/wecom_daily/
    /// n_plus_one_guard.rs` 同款口径（那里已把盲区清单逐条登记）。
    fn blank_comments(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                let mut depth = 1usize;
                while i < b.len() {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                    if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
                continue;
            }
            i += 1;
        }
        String::from_utf8(out).expect("注释空格化不改变字节的可解码性")
    }

    /// 被扫描的函数（准备阶段失败落流水，**不走**严格 `insert_log` 的那条）。
    const FN_LOG_PREPARE_FAILURE: &str = "log_prepare_failure";

    /// ★ 守卫：`log_prepare_failure` **不走**严格版 `insert_log`，版本号走尽力读。
    ///
    /// 2026-10-19 review 第 2 轮登记的**上一轮 fixer 自己引入的回归**：那次把
    /// `log_prepare_failure` 里的 `cfg_version_of(conn).await.unwrap_or(0)` 挪进了共用
    /// helper `insert_log`（那里是 `?`）⇒ 版本号读失败会让**整条**落流水提前返回、
    /// `t_wecom_daily_log` 零行 —— 而「失败也要留下痕迹」这条路径上兜底**只有**这一份。
    /// 复现形态（集成测试造不出来，见本用例的「覆盖形态」说明）：准备阶段失败 →
    /// `log_prepare_failure` → `repo::get_config` 读失败 → `Err`。
    ///
    /// ## 覆盖形态与不足（**明写出来，免得日后有人当它是完备证明**）
    ///
    /// 这条护栏是**源码级**的（`include_str!("service.rs")`），与
    /// `tests/wecom_daily/n_plus_one_guard.rs`、本域 `handler.rs` 的路由表护栏同款取舍。
    /// 它守的是**接线形态**（走哪个 helper），守不了运行时：集成测试侧无法让
    /// `repo::get_config` 在一条健康的测试库连接上失败（`REVOKE` / 断网都会让整条用例的
    /// 基建一起挂），所以「读失败仍落库」这个**行为**由 [`cfg_version_best_effort`]
    /// 的纯函数单测守，两条合起来才是「降级口径 + 接线」全覆盖。
    /// ⚠️ 已知局限：注释里的同名字符串会骗过匹配（`blank_comments` 已把注释空格化，
    /// 但**没有**处理字符串字面量）；函数被改名 / 抽出到别的文件时本用例会 panic 而非
    /// 静默通过（panic 同样算红）。
    #[test]
    fn log_prepare_failure_keeps_its_best_effort_version_snapshot() {
        let src = blank_comments(include_str!("service.rs"));
        let (_, rest) = src
            .split_once(&format!("async fn {FN_LOG_PREPARE_FAILURE}"))
            .unwrap_or_else(|| panic!("源码里找不到 `async fn {FN_LOG_PREPARE_FAILURE}`"));
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("`{FN_LOG_PREPARE_FAILURE}` 必须有收尾大括号"));
        let body = &rest[..end];

        assert!(
            body.contains("cfg_version_best_effort("),
            "`{FN_LOG_PREPARE_FAILURE}` 的函数体必须经 `cfg_version_best_effort` 取版本号 \
             （尽力读：失败降级 0，INSERT 照发）"
        );
        // 严格 helper 只服务 `deliver`：本函数体内**任何**独立的 `insert_log` 标识符都
        // 必须带 `repo::` 限定（`repo::insert_log` 是直调 repo，不是共用 helper）。
        let mut from = 0usize;
        while let Some(rel) = body[from..].find("insert_log") {
            let at = from + rel;
            assert!(
                body[..at].ends_with("::"),
                "`{FN_LOG_PREPARE_FAILURE}` 不得调用严格的 `insert_log` helper \
                 （第 {} 行：`{}`）：那里版本号读失败会让整条落流水提前返回、\
                 `t_wecom_daily_log` 零行；本函数要走 `repo::insert_log` + \
                 `cfg_version_best_effort`",
                1 + body[..at].matches('\n').count(),
                body[at..].split(['\n', ',']).next().unwrap_or("")
            );
            from = at + "insert_log".len();
        }
    }

    /// 尽力版的降级口径：读失败 ⇒ `0`，读到 ⇒ 原值。
    ///
    /// ⚠️ 与上一条**成对存在**：那条守「谁在用这个降级」（接线），本条守「降级成什么」
    /// （口径）。缺上一条 ⇒ 把调用改回严格 helper 时本条仍全绿；缺本条 ⇒ 降级写成别的
    /// 值（比如 `1`）时上一条仍全绿。
    #[test]
    fn cfg_version_best_effort_degrades_read_failures_to_zero() {
        assert_eq!(cfg_version_best_effort(Ok(7)), 7, "读得到就原样用");
        // 表空那档在 `cfg_version_of` 里就已经是 `Ok(0)`，落到本函数仍是 0。
        assert_eq!(cfg_version_best_effort(Ok(0)), 0);
        let boom = AppError::Database(sqlx::Error::PoolTimedOut);
        assert_eq!(cfg_version_best_effort(Err(boom)), 0, "读失败必须降级成 0");
    }
    // ========================================================================
    // 第三节：分组 / 截断 / 位置解析（纯函数）
    // ========================================================================

    /// 造一行（测试用：默认有序列号、无位置、无链接）。
    fn mk_row(is_child: bool, serial: &str) -> OrderRow {
        OrderRow {
            is_child,
            serial_no: Some(serial.to_string()),
            name: serial.to_string(),
            quantity: 1,
            link: None,
            location: None,
        }
    }

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 15)
            .expect("固定日期合法")
            .checked_add_signed(chrono::Duration::days(n as i64))
            .expect("日期不越界")
    }

    /// ★ 守卫：分组顺序 = **逾期组第一，日期组按交期日升序**，且**不依赖输入顺序**。
    ///
    /// ⚠️ 入参是**打乱**的：顺序保证写在 [`group_and_cap`] 里而不是「反正 SQL 已经
    /// `ORDER BY` 过了」—— 依赖调用方恰好排好序的那种写法，在多一次查询（比如把日期组
    /// 改成单独查）时就会静默乱序。
    #[test]
    fn order_groups_are_overdue_first_then_dates_ascending() {
        let flat = vec![
            (OrderGroupKind::OnDate(day(1)), mk_row(false, "D1")),
            (OrderGroupKind::Overdue, mk_row(false, "O1")),
            (OrderGroupKind::OnDate(day(0)), mk_row(false, "D0")),
            (OrderGroupKind::OnDate(day(0)), mk_row(false, "D0b")),
            (OrderGroupKind::Overdue, mk_row(false, "O2")),
        ];
        let groups = group_and_cap(flat, 12, 0);
        assert_eq!(groups.len(), 3, "逾期 1 组 + 两个日期各 1 组");
        assert_eq!(groups[0].kind, OrderGroupKind::Overdue);
        assert_eq!(groups[1].kind, OrderGroupKind::OnDate(day(0)));
        assert_eq!(groups[2].kind, OrderGroupKind::OnDate(day(1)));
        // 组内行的相对次序逐字保留（稳定排序）。
        let names = |g: &OrderGroup| g.rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&groups[0]), vec!["O1", "O2"]);
        assert_eq!(names(&groups[1]), vec!["D0", "D0b"]);
    }

    /// ★ 守卫：组标题的「N 条」= **匹配工单数**，子件行不算工单；逾期组用 SQL 给的
    /// 未截断总数。
    #[test]
    fn group_total_counts_orders_not_rows() {
        let flat = vec![
            (OrderGroupKind::OnDate(day(0)), mk_row(false, "A")),
            (OrderGroupKind::OnDate(day(0)), mk_row(true, "a1")),
            (OrderGroupKind::OnDate(day(0)), mk_row(true, "a2")),
            (OrderGroupKind::OnDate(day(1)), mk_row(false, "B")),
        ];
        let groups = group_and_cap(flat, 99, 0);
        assert_eq!(groups[0].total, 1, "3 行（1 工单 + 2 子件）只算 1 单");
        assert_eq!(groups[1].total, 1);
        // 逾期组的 total 取形参（SQL 的 `COUNT(*) OVER ()`），不是数行数。
        let only_overdue =
            group_and_cap(vec![(OrderGroupKind::Overdue, mk_row(false, "O"))], 41, 0);
        assert_eq!(only_overdue[0].total, 41);
    }

    /// ★ 守卫：`order_list_limit == 0` 是**不限**（不是「一条都不列」）。
    #[test]
    fn order_limit_zero_means_unlimited() {
        let flat: Vec<_> = (0..50)
            .map(|i| (OrderGroupKind::Overdue, mk_row(false, &format!("F{i:02}"))))
            .collect();
        let groups = group_and_cap(flat, 50, 0);
        assert_eq!(groups[0].rows.len(), 50, "0 = 全列");
        assert_eq!(groups[0].omitted, 0, "没截断就没有脚注");
    }

    /// ★ 守卫：**工单行与子件行吃同一个额度**，截断只留一行脚注、**不为子件单独提示**。
    ///
    /// 额度是「这条消息最多写多少行」的直译：一个装配件 + 3 个子件在额度 2 时只留下
    /// 装配件行 + 第一个子件，脚注报「另有 3 条未列出」（行数，不是工单数）。
    #[test]
    fn order_limit_is_shared_between_orders_and_children() {
        let flat = vec![
            (OrderGroupKind::Overdue, mk_row(false, "P")),
            (OrderGroupKind::Overdue, mk_row(true, "C1")),
            (OrderGroupKind::Overdue, mk_row(true, "C2")),
            (OrderGroupKind::Overdue, mk_row(true, "C3")),
            (OrderGroupKind::Overdue, mk_row(false, "Q")),
        ];
        let groups = group_and_cap(flat, 2, 2);
        assert_eq!(
            groups[0].rows.len(),
            2,
            "额度 2 ⇒ 只留 2 行（跨工单与子件）"
        );
        assert_eq!(
            groups[0]
                .rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["P", "C1"]
        );
        assert_eq!(groups[0].omitted, 3, "报未列出的**行**数，不报工单数");
        assert_eq!(groups[0].total, 2, "「N 条」是未截断的匹配工单数");
    }

    /// ★ 守卫：额度**按组**各算一份（不是整个第三节共用一份）。
    #[test]
    fn order_limit_applies_per_group() {
        let flat: Vec<_> = (0..4)
            .map(|i| (OrderGroupKind::Overdue, mk_row(false, &format!("O{i}"))))
            .chain((0..4).map(|i| {
                (
                    OrderGroupKind::OnDate(day(0)),
                    mk_row(false, &format!("D{i}")),
                )
            }))
            .collect();
        let groups = group_and_cap(flat, 4, 2);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].rows.len(), 2);
        assert_eq!(groups[1].rows.len(), 2, "日期组有自己的 2 条额度");
        assert_eq!(groups[1].omitted, 2);
    }

    /// ★ 守卫：`cap_rows` 的负数也按「不限」处理（配置层已 clamp，这里是防御）。
    #[test]
    fn cap_rows_treats_non_positive_limit_as_unlimited() {
        let rows = vec![mk_row(false, "A"), mk_row(false, "B")];
        let (kept, omitted) = cap_rows(rows.clone(), 0);
        assert_eq!(kept.len(), 2);
        assert_eq!(omitted, 0);
        let (kept, omitted) = cap_rows(rows, -1);
        assert_eq!(kept.len(), 2, "负数不得被当成「留 0 条」");
        assert_eq!(omitted, 0);
    }

    /// ★ 守卫：位置的中文展示值 = `中文名:实体名`，**四种 location 各一个标签**，
    /// `OFFICE` / `NULL` / 查不到名字一律**整项不显示**。
    #[test]
    fn location_display_labels_the_four_kinds_and_rejects_office() {
        assert_eq!(
            location_display(Some("PRODUCTION_SHELF"), Some("A-03")).as_deref(),
            Some("生产架:A-03")
        );
        assert_eq!(
            location_display(Some("INSPECTION_SHELF"), Some("B-11")).as_deref(),
            Some("品检架:B-11")
        );
        assert_eq!(
            location_display(Some("WORKER"), Some("张三")).as_deref(),
            Some("工人:张三")
        );
        assert_eq!(
            location_display(Some("OUTSOURCE_COMPANY"), Some("宏达")).as_deref(),
            Some("外协:宏达")
        );
        // 三种「拿不到」都归一成 None（渲染层据此整项不显示）。
        assert_eq!(location_display(Some("OFFICE"), Some("张三")), None);
        assert_eq!(location_display(None, Some("张三")), None);
        assert_eq!(location_display(Some("WORKER"), None), None);
    }

    /// ★ 守卫：`holder_name` 按 `location` 分桶取名，取不到（holder 行已软删）时
    /// **回落成 `None`** 而不是「位置: 无」。
    #[test]
    fn holder_name_looks_up_the_right_bucket_and_falls_back_to_none() {
        let shelf = HashMap::from([(10, "A-03".to_string())]);
        let worker = HashMap::from([(20, "张三".to_string())]);
        let outsource = HashMap::from([(30, "宏达".to_string())]);
        assert_eq!(
            holder_name(
                Some("PRODUCTION_SHELF"),
                Some(10),
                &shelf,
                &worker,
                &outsource
            )
            .as_deref(),
            Some("生产架:A-03")
        );
        assert_eq!(
            holder_name(
                Some("INSPECTION_SHELF"),
                Some(10),
                &shelf,
                &worker,
                &outsource
            )
            .as_deref(),
            Some("品检架:A-03"),
            "两种货架位置共用 `t_shelf` 一个桶"
        );
        assert_eq!(
            holder_name(Some("WORKER"), Some(20), &shelf, &worker, &outsource).as_deref(),
            Some("工人:张三")
        );
        assert_eq!(
            holder_name(
                Some("OUTSOURCE_COMPANY"),
                Some(30),
                &shelf,
                &worker,
                &outsource
            )
            .as_deref(),
            Some("外协:宏达")
        );
        // ⚠️ 拿着 worker 的 id 去查货架桶查得到也**不显示**（id 在三类实体间不互斥，
        // 按 location 分桶才是判据）。
        assert_eq!(
            holder_name(
                Some("PRODUCTION_SHELF"),
                Some(20),
                &shelf,
                &worker,
                &outsource
            ),
            None
        );
        // 软删 ⇒ 查不到 ⇒ 整项不显示。
        assert_eq!(
            holder_name(Some("WORKER"), Some(999), &shelf, &worker, &outsource),
            None
        );
        assert_eq!(
            holder_name(Some("OFFICE"), Some(20), &shelf, &worker, &outsource),
            None
        );
        assert_eq!(
            holder_name(Some("WORKER"), None, &shelf, &worker, &outsource),
            None
        );
    }
}
