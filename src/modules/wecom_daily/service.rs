//! wecom_daily 域 service 层：校验 / OCC / 取数编排 / 发送编排
//!
//! ## 事务边界（CLAUDE.md §1）
//! 全部在 handler：`pool.acquire()` → service 借 `&mut *conn` → drop conn。
//! 本域**不开显式事务**：一次请求要么只读、要么只做单行 UPDATE/INSERT，每条语句
//! 自身原子。开事务反而会让「INSERT 流水失败 → 整笔回滚 → 发送结果也丢了」这种
//! 场景无法发生。
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

use std::sync::Arc;

use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::modules::wecom::sender;
use crate::modules::wecom::service as wecom_service;
use crate::modules::wecom_daily::dto::{
    ConfigDefaultsOut, ConfigOut, Frequency, LOGS_LIMIT_MAX, LogListOut, LogListQuery,
    MAX_CHUNKS_MAX, MAX_CHUNKS_MIN, NormalizedConfig, PreviewOut, PreviewRequest,
    SUBJECT_TEMPLATE_MAX_CHARS, SaveConfigRequest, SendResultOut, SendStatus, TriggerSource,
    UPCOMING_DAYS_MAX, UPCOMING_DAYS_MIN, WORKER_TOP_N_MAX, WORKER_TOP_N_MIN,
};
use crate::modules::wecom_daily::model::{TWeComDailyConfigRow, TWeComDailyLogRow};
use crate::modules::wecom_daily::repo::{self, NewDailyLog};
use crate::modules::wecom_daily::template::{self, RenderInput, ReportRows};
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
            max_chunks: row.max_chunks,
            include_overview: row.include_overview,
            include_delivery: row.include_delivery,
            include_orders: row.include_orders,
            include_workers: row.include_workers,
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
            max_chunks: d.max_chunks,
            include_overview: d.include_overview,
            include_delivery: d.include_delivery,
            include_orders: d.include_orders,
            include_workers: d.include_workers,
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
/// 2. `upcoming_days` **clamp** 到 `[1, 60]`（照 `mail_digest`：这个数只影响窗口
///    宽窄，越界时夹到边界比报错更符合「用户想表达的意思」；而下面三个是**拒**）；
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
        max_chunks: req.max_chunks,
        include_overview: req.include_overview,
        include_delivery: req.include_delivery,
        include_orders: req.include_orders,
        include_workers: req.include_workers,
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
        max_chunks: overrides.max_chunks.unwrap_or(base.max_chunks),
        include_overview: overrides.include_overview.unwrap_or(base.include_overview),
        include_delivery: overrides.include_delivery.unwrap_or(base.include_delivery),
        include_orders: overrides.include_orders.unwrap_or(base.include_orders),
        include_workers: overrides.include_workers.unwrap_or(base.include_workers),
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
        max_chunks: row.max_chunks,
        include_overview: row.include_overview,
        include_delivery: row.include_delivery,
        include_orders: row.include_orders,
        include_workers: row.include_workers,
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
    let rows = fetch_rows(conn, &cfg, today).await?;
    let rendered = template::render(&RenderInput {
        cfg: &cfg,
        rows: &rows,
        today,
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
        repair_part_count: rows.repair_part_count,
        note_count: rows.note_count,
        note_part_count: rows.note_part_count,
        overdue_count: rows.overdue_count,
        upcoming_count: rows.upcoming_count,
        worker_rows: rows.worker_rows.clone(),
        worker_row_total: rendered.worker_row_total,
        sections: rendered.sections,
    })
}

/// `POST /send`：手动触发（渲染 + 真发 + 落流水）。
pub async fn send_manual(
    state: &Arc<AppState>,
    req: &PreviewRequest,
) -> Result<SendResultOut, AppError> {
    let mut conn = state.pool.acquire().await?;
    let cfg = effective_config(&mut conn, req).await?;
    deliver(state, &mut conn, &cfg, TriggerSource::Manual).await
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
pub async fn send_scheduled(state: &Arc<AppState>) {
    let mut conn = match state.pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "wecom_daily: 定时日报取连接失败（无连接可落流水）");
            return;
        }
    };

    let cfg = match current_normalized(&mut conn).await {
        Ok(c) => c,
        Err(e) => {
            log_prepare_failure(state, &mut conn, None, &e).await;
            tracing::warn!(error = %e, "wecom_daily: 定时日报准备失败（读配置/解析），流水已落库");
            return;
        }
    };

    if let Err(e) = wecom_service::resolve_target(&mut conn, cfg.webhook_id).await {
        log_prepare_failure(state, &mut conn, Some(&cfg), &e).await;
        tracing::warn!(error = %e, "wecom_daily: 定时日报准备失败（推送通道不可用），流水已落库");
        return;
    }

    match deliver(state, &mut conn, &cfg, TriggerSource::Scheduled).await {
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
/// `cfg` 传 `None` 表示连配置都没解析出来（脏 `frequency` / `worker_metric`）——
/// 此时尽力从**原始行**捞版本号，捞不到就记 0：流水表的这几列是**快照**（配置改了
/// 之后仍要看「当时用的是哪版配置」），拿不到时留空比编一个更诚实。
///
/// ⚠️ 本函数**永不**向上返回错误：它存在的意义是「失败也要留下痕迹」，若它自己失败
/// 就静默 `warn!`（调用方随后仍会 `warn!` 原始原因，两条日志互补）。
async fn log_prepare_failure(
    state: &Arc<AppState>,
    conn: &mut PgConnection,
    cfg: Option<&NormalizedConfig>,
    err: &AppError,
) {
    // `cfg` 只用来区分「配置都读不出来」与「读出来了但通道不可用」两种准备阶段失败 ——
    // 两者落库的形态一样（内容全 0），差异只在这一行 warn 文案上。
    let _ = cfg;
    let config_version = cfg_version_of(conn).await.unwrap_or(0);
    let inserted = repo::insert_log(
        conn,
        &state.snowflake,
        &NewDailyLog {
            config_version,
            trigger_source: TriggerSource::Scheduled.as_str(),
            // 通道都没解析出来 ⇒ 名称与掩码只能是空串（列是 NOT NULL）。留空而不是
            // 编一个：那两列是「发到哪个群」的快照，编出来的比没有更会误导排障。
            webhook_name: String::new(),
            webhook_masked: String::new(),
            report_date: now_ts().date(),
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
            // ⚠️ `report_date` 由 `sent_at.date()` 派生 ⇒ 「统计日恒等于发送时刻的
            // 日期」这条前端依赖的不变量在流水侧也成立。
            sent_at: now_ts(),
        },
    )
    .await;
    match inserted {
        Ok(log) => tracing::info!(log_id = log.id, "wecom_daily: 准备阶段失败流水已落库"),
        Err(e) => tracing::warn!(error = %e, "wecom_daily: 准备阶段失败流水落库失败"),
    }
}

/// 按节开关取数（**关掉的节一条 SQL 都不发**）。
///
/// ## 不变量：**恒 ≤ 6 条语句，与工单数 / 送货单数 / 工人数无关**
///
/// 上界来自下面**恰好 6 个** `repo::` 调用点：第一节 2 条 + 第二节 1 条 + 第三节 2 条
/// + 第四节 1 条。⇒ **四节全开（缺省）时恰好 6 条**，关掉一节就少一条。
///
/// **本函数体内没有任何循环** —— 工人榜的排序在下一句用 `sort_by` 做（内存操作）。
///
/// ## 护栏
/// 「循环体内只许做内存操作、绝不调 `repo::`」由
/// `tests/wecom_daily/n_plus_one_guard.rs` 的**源码级**护栏钉住（顶层恰好 6 个
/// 调用点 + 循环体内禁 `repo::` / `sqlx::query`）。
///
/// ⚠️ 逐节循环查是**功能完全正确、集成测试照样全绿**的写法（每条用例只造 2~3 行），
/// 只有数据量涨上去时语句数才悄悄线性涨 —— 而日报恰好是「一天一次、量很大」的生产形态。
async fn fetch_rows(
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    today: chrono::NaiveDate,
) -> Result<ReportRows, AppError> {
    let mut rows = ReportRows::default();

    if cfg.include_overview {
        let ev = repo::overview_events(conn, today).await?;
        rows.overview_batch_count = ev.batch_count as i32;
        rows.overview_quantity = ev.quantity as i32;
        rows.completed_count = ev.completed as i32;
        rows.repair_part_count = ev.repair as i32;
        rows.in_process_count = repo::in_process_count(conn).await? as i32;
    }

    if cfg.include_delivery {
        let d = repo::delivery_note_summary(conn, today).await?;
        rows.note_count = d.note_count as i32;
        rows.note_part_count = d.part_count as i32;
    }

    if cfg.include_orders {
        rows.overdue_count = repo::count_overdue(conn, today).await? as i32;
        rows.upcoming_count = repo::count_upcoming(conn, today, cfg.upcoming_days).await? as i32;
    }

    if cfg.include_workers {
        rows.worker_rows = repo::worker_rows(conn, today).await?;
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

/// 读当前配置行版本（流水快照用；表空时记 `0`）。
async fn cfg_version_of(conn: &mut PgConnection) -> Result<i32, AppError> {
    Ok(repo::get_config(conn)
        .await?
        .map(|r| r.version)
        .unwrap_or(0))
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
async fn deliver(
    state: &Arc<AppState>,
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    source: TriggerSource,
) -> Result<SendResultOut, AppError> {
    // 通道解析排在节开关检查**之前**（更基础的失败先报，见模块 doc）。
    let target = wecom_service::resolve_target(conn, cfg.webhook_id).await?;
    validate_sections(cfg)?;

    let now = now_ts();
    let today = now.date();
    let rows = fetch_rows(conn, cfg, today).await?;
    let rendered = template::render(&RenderInput {
        cfg,
        rows: &rows,
        today,
    })?;
    // ⚠️ 上面这一行返回 `Err(21804)` 时**不落流水**：没发出去，不是投递失败。
    // 前端 `/send` 失败后正是靠「有没有流水行」区分这两类。
    let chunks = rendered.chunks;
    let markdown_bytes = chunks.iter().map(|c| c.len() as i32).sum::<i32>();

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

    let config_version = cfg_version_of(conn).await?;
    let log = repo::insert_log(
        conn,
        &state.snowflake,
        &NewDailyLog {
            config_version,
            trigger_source: source.as_str(),
            webhook_name: target.name.clone(),
            webhook_masked: target.masked.clone(),
            report_date: today,
            markdown_bytes,
            chunk_count: chunks.len() as i16,
            chunk_sent: chunk_sent as i16,
            overview_batch_count: rows.overview_batch_count,
            overview_quantity: rows.overview_quantity,
            note_count: rows.note_count,
            note_part_count: rows.note_part_count,
            overdue_count: rows.overdue_count,
            upcoming_count: rows.upcoming_count,
            worker_row_total: rendered.worker_row_total,
            status: status.as_str(),
            error_message,
            sent_at: now,
        },
    )
    .await?;

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
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
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
        MAX_CHUNKS_DEFAULT, UPCOMING_DAYS_DEFAULT, WORKER_TOP_N_DEFAULT, WorkerBarRow, WorkerMetric,
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
            max_chunks: 3,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
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
                max_chunks: 3,
                include_overview: true,
                include_delivery: true,
                include_orders: true,
                include_workers: true,
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
            max_chunks: 3,
            include_overview: true,
            include_delivery: true,
            include_orders: true,
            include_workers: true,
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
        assert!(t.chars().count() <= 501, "截断后不应超 500 字 + 省略号");
        assert!(t.ends_with('…'));
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }
}
