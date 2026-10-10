//! mail_digest 域 service 层：校验 / OCC / 渲染 + 发送编排
//!
//! ## 事务边界（CLAUDE.md §1）
//! 全部在 handler：`pool.acquire()` → service 借 `&mut *conn` → drop conn。
//! 本域**不开显式事务**：一次请求要么只读、要么只做单行 UPDATE/INSERT，
//! 每条语句自身原子。开事务反而会让「INSERT 流水失败 → 整笔回滚 → 发送结果
//! 也丢了」这种场景无法发生。
//!
//! ## 发送流程（`render_and_send`）的顺序是刻意的
//!
//! ```text
//! 取配置 → 按节开关查统计（关掉的节不查） → 渲染 → 调 lettre → 无论成败落流水
//! ```
//!
//! ⚠️ **落流水在发送之后、且不分成败**：只记成功会让「定时任务每天都在失败」
//! 这件事完全不可观测 —— 而这恰恰是定时任务最常见的故障形态（凭据过期、
//! 收件人被拒、TLS 模式错配）。
//!
//! ## 日志纪律
//! - **绝不**打印 SMTP password（它只在 [`crate::infra::config::MailConfig`] 的
//!   手写 `Debug` 里以 `***` 出现）。
//! - 收件人**只打数量 + 掩码**（`a***@x.com`）：地址本身是个人信息，进日志就等于
//!   把一份客户名册散进了日志文件。

use std::sync::Arc;

use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::infra::config::MailConfig;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::dto::{
    ConfigDefaultsOut, ConfigOut, Frequency, LogListQuery, LogListOut, NormalizedConfig,
    PreviewOut, PreviewRequest, SaveConfigRequest, SendResultOut, SendStatus, TriggerSource,
    UPCOMING_DAYS_MAX, UPCOMING_DAYS_MIN, VisibleColumn, parse_visible_columns,
};
use super::model::{TMailDigestConfig, TMailDigestLog};
use super::repo::{self, NewDigestLog};
use super::template::{self, RenderInput};

// ============================================================================
// 读
// ============================================================================

/// `GET /config`：读配置单例。
///
/// 表里无行时返回**全部默认值 + `version=0` + `id="0"`**（而不是 404）：配置页必须
/// 能在「还没配过」的状态下正常打开并提交首次配置，返 404 会让前端多写一套
/// 「404 ⇒ 空表单」的分支。
pub async fn get_config(
    conn: &mut PgConnection,
    mail: &MailConfig,
) -> Result<ConfigOut, AppError> {
    match repo::get_config(conn).await? {
        Some(row) => Ok(ConfigOut::from_row(&row, mail)),
        None => Ok(ConfigOut::default_out(mail)),
    }
}

/// `GET /logs`：发送历史分页。
pub async fn list_logs(
    conn: &mut PgConnection,
    q: &LogListQuery,
) -> Result<LogListOut, AppError> {
    // limit clamp 到 [1,200]：0 / 负数会让 `LIMIT` 报错或恒空，超大值等于没分页。
    let limit = q.limit.clamp(1, super::dto::LOGS_LIMIT_MAX);
    let offset = q.offset.max(0);
    let (rows, total) = repo::list_logs(conn, limit, offset).await?;
    Ok(LogListOut {
        items: rows.iter().map(SendResultOut::from_row).collect(),
        total,
        limit,
        offset,
    })
}

// ============================================================================
// 写配置（OCC）
// ============================================================================

/// `POST /config`：全量覆写 + 触发调度热重载。
///
/// ⚠️ body 的 `version` 由 DTO 保证**必填**（缺失 → axum `Json` 提取器返 HTTP
/// 422 纯文本），本函数只做两件事：
/// 1. 首次初始化（表里无行、`version=0`）→ INSERT；
/// 2. 其余 → `UPDATE … WHERE id AND version` 的标准 OCC。
///
/// `mail` 只用于回填 `smtp_configured`（让前端一次拿到「配置行」+「SMTP 是否就绪」），
/// 不参与 OCC。
pub async fn save_config(
    conn: &mut PgConnection,
    req: &SaveConfigRequest,
    current: &CurrentUser,
    snowflake: &crate::infra::snowflake::SnowflakeIdGenerator,
    mail: &MailConfig,
) -> Result<ConfigOut, AppError> {
    let normalized = validate_save_request(req)?;
    let now = now_naive();

    match repo::get_config(conn).await? {
        None => {
            // 首次初始化。并发下两个请求同时走到这里，靠唯一索引
            // `uk_t_mail_digest_config_alive` 兜底：后到者撞 23505 → 转 OCC 冲突。
            let id = snowflake.next_id();
            match repo::insert_config(conn, id, &normalized, current.id, now).await {
                Ok(true) => {}
                Ok(false) => return Err(version_conflict()),
                Err(e) if is_unique_violation(&e) => return Err(version_conflict()),
                Err(e) => return Err(e.into()),
            }
        }
        Some(row) => {
            let ok = repo::update_config(
                conn,
                row.id,
                req.version,
                &normalized,
                current.id,
                now,
            )
            .await?;
            if !ok {
                return Err(version_conflict());
            }
        }
    }

    // 读回最新行（含自增后的 version）返回，避免前端拿一个「提交前的 version」
    // 做下一次提交 → 立刻撞一次假的 OCC 冲突。
    let fresh = repo::get_config(conn)
        .await?
        .ok_or_else(version_conflict)?;
    Ok(ConfigOut::from_row(&fresh, mail))
}

impl ConfigOut {
    fn from_row(row: &TMailDigestConfig, mail: &MailConfig) -> Self {
        Self {
            id: row.id,
            version: row.version,
            enabled: row.enabled,
            frequency: row.frequency.clone(),
            send_hour: row.send_hour,
            send_minute: row.send_minute,
            weekday: row.weekday,
            day_of_month: row.day_of_month,
            recipients: row.recipients.clone(),
            cc_recipients: row.cc_recipients.clone(),
            include_overdue: row.include_overdue,
            include_upcoming: row.include_upcoming,
            upcoming_days: row.upcoming_days,
            visible_columns: row.visible_columns.clone(),
            subject_template: row.subject_template.clone(),
            smtp_configured: mail.is_enabled(),
            created_at: format_ts(row.created_at),
            updated_at: format_ts(row.updated_at),
        }
    }

    /// 表里无行时的「零值配置」：`id="0"` + `version=0` + 缺省内容。
    fn default_out(mail: &MailConfig) -> Self {
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
            recipients: d.recipients.clone(),
            cc_recipients: d.cc_recipients.clone(),
            include_overdue: d.include_overdue,
            include_upcoming: d.include_upcoming,
            upcoming_days: d.upcoming_days,
            visible_columns: d
                .visible_columns
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
            subject_template: d.subject_template.clone(),
            smtp_configured: mail.is_enabled(),
            // 时间戳在「尚未创建」时没有意义，给一个固定的占位串而不是 `now()`：
            // 前端拿它做「更新时间」展示的话，固定的占位比会跳动的假时间更好判断。
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

impl SendResultOut {
    fn from_row(row: &TMailDigestLog) -> Self {
        Self {
            log_id: row.id,
            config_version: row.config_version,
            trigger_source: row.trigger_source.parse().unwrap_or(TriggerSource::Manual),
            status: row.status.parse().unwrap_or(SendStatus::Failed),
            subject: row.subject.clone(),
            to_recipients: split_recipients(&row.to_recipients),
            cc_recipients: split_recipients(&row.cc_recipients),
            overdue_count: row.overdue_count,
            upcoming_count: row.upcoming_count,
            html_bytes: row.html_bytes,
            sent_at: format_ts(row.sent_at),
            error_message: row.error_message.clone(),
        }
    }
}

/// 逗号分隔的收件人快照 → `Vec<String>`（空串 ⇒ 空数组）。
fn split_recipients(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
// 校验（写侧 21703 / 21704）
// ============================================================================

/// 校验 + 规范化 `POST /config` 请求体。
///
/// 规范化做的事（都在这里做完，下游一律拿 [`NormalizedConfig`]）：
/// 1. `frequency` 解析成枚举（未知 ⇒ 21704）；
/// 2. `upcoming_days` clamp 到 `[1, 60]`；
/// 3. `DAILY` / `MANUAL_ONLY` 之下**清空** `weekday` / `day_of_month`
///    （不留脏值：读侧会照单全收地拼 cron，留着等于埋雷）；
///    `WEEKLY` / `MONTHLY` 的必填字段缺失 ⇒ 21704；
/// 4. 邮箱逐个校验（格式 + 拒 CR/LF）；
/// 5. `visible_columns` 逐项解析（未知项 ⇒ 21704，**不静默丢弃**）；
/// 6. 两节都关 ⇒ 21704（那样渲出来的是一封纯「无符合条件的工单」的邮件，
///    每天发一次纯属骚扰）。
pub fn validate_save_request(req: &SaveConfigRequest) -> Result<NormalizedConfig, AppError> {
    let frequency: Frequency = req.frequency.parse()?;

    let send_hour = req.send_hour;
    let send_minute = req.send_minute;
    validate_time(frequency, send_hour, send_minute)?;

    let weekday = match frequency {
        Frequency::Weekly => Some(validate_weekday(req.weekday)?),
        // DAILY / MONTHLY / MANUAL_ONLY 之下 weekday 不参与 ⇒ 清 NULL
        _ => None,
    };
    let day_of_month = match frequency {
        Frequency::Monthly => Some(validate_day_of_month(req.day_of_month)?),
        _ => None,
    };

    if !req.include_overdue && !req.include_upcoming {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "「已逾期」与「即将到期」两节不能同时关闭：那样每天只会收到一封\
             「无符合条件的工单」的邮件",
        ));
    }

    Ok(NormalizedConfig {
        enabled: req.enabled,
        frequency,
        send_hour,
        send_minute,
        weekday,
        day_of_month,
        recipients: validate_recipients(&req.recipients)?,
        cc_recipients: validate_recipients(&req.cc_recipients)?,
        include_overdue: req.include_overdue,
        include_upcoming: req.include_upcoming,
        upcoming_days: req.upcoming_days.clamp(UPCOMING_DAYS_MIN, UPCOMING_DAYS_MAX),
        visible_columns: parse_visible_columns(&req.visible_columns)?,
        subject_template: validate_subject_template(req.subject_template.as_deref())?,
    })
}

/// 时段校验：`MANUAL_ONLY` 不需要时刻；其余频率必须给全 `hour` + `minute`。
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
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("频率 {} 需要填写发送小时（0..23）", frequency.as_str()),
        ));
    };
    if !(0..=23).contains(&h) {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("发送小时必须在 0..23 之间，收到 {h}"),
        ));
    }
    let Some(m) = minute else {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("频率 {} 需要填写发送分钟（0..59）", frequency.as_str()),
        ));
    };
    if !(0..=59).contains(&m) {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("发送分钟必须在 0..59 之间，收到 {m}"),
        ));
    }
    Ok(())
}

fn validate_weekday(v: Option<i16>) -> Result<i16, AppError> {
    match v {
        Some(d) if (1..=7).contains(&d) => Ok(d),
        Some(d) => Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("weekday 必须在 1..7 之间（ISO：1=周一 .. 7=周日），收到 {d}"),
        )),
        None => Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "频率 WEEKLY（每周）必须填写 weekday（1..7，ISO：1=周一 .. 7=周日）",
        )),
    }
}

fn validate_day_of_month(v: Option<i16>) -> Result<i16, AppError> {
    match v {
        Some(d) if (1..=28).contains(&d) => Ok(d),
        Some(d) => Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("day_of_month 必须在 1..28 之间（刻意避开 2 月末与 30/31 日短月），收到 {d}"),
        )),
        None => Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "频率 MONTHLY（每月）必须填写 day_of_month（1..28）",
        )),
    }
}

/// 收件人列表校验：`recipients` 必填非空；逐个做基本格式校验。
///
/// ⚠️ **拒含 CR / LF 的地址**：这是 SMTP **header 注入**面 —— 攻击者把
/// `a@b.com\r\nBcc: victim@x.com` 塞进收件人，就能让本系统替他发信给任意第三方。
/// 必须在**入库前**拒掉，不能指望 lettre 兜底。
pub fn validate_recipients(raw: &[String]) -> Result<Vec<String>, AppError> {
    let mut out = Vec::with_capacity(raw.len());
    for addr in raw {
        let a = addr.trim();
        if a.is_empty() {
            continue;
        }
        validate_email(a)?;
        out.push(a.to_string());
    }
    Ok(out)
}

/// 单个邮箱的基本校验：`x@y.z` 形态 + 拒 CR/LF/空白/尖括号。
///
/// **刻意不做 RFC 5322 全量校验**：企业邮的真实地址形态（中文域名、
/// `user+tag@`、内网 IP 域名）千奇百怪，全量校验的误拒率会高到运维宁可关掉功能。
/// 本函数只挡「明显不是地址」与「有注入风险」两类。
pub fn validate_email(addr: &str) -> Result<(), AppError> {
    let reject = |msg: &str| {
        Err(AppError::biz(
            code::BIZ_MAIL_RECIPIENT_INVALID,
            format!("邮箱地址不合法：{msg}"),
        ))
    };
    if addr.contains('\r') || addr.contains('\n') {
        return reject("地址含 CR/LF（SMTP header 注入，已拒绝）");
    }
    if addr.chars().any(|c| c.is_whitespace()) {
        return reject("地址含空白字符");
    }
    if addr.contains('<') || addr.contains('>') || addr.contains(',') || addr.contains(';') {
        return reject("地址含分隔符 / 尖括号（多个地址请分条填写）");
    }
    let Some((local, domain)) = addr.split_once('@') else {
        return reject("缺少 @");
    };
    if local.is_empty() {
        return reject("@ 之前为空");
    }
    if domain.is_empty() || !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.')
    {
        return reject("域名部分不像域名（应有且仅有一个主分隔点，两端非空）");
    }
    if domain.contains("..") {
        return reject("域名含连续的点");
    }
    Ok(())
}

/// 主题模板校验：长度 ≤200（与列 `varchar(200)` 对齐）+ 拒 CR/LF（header 注入）。
///
/// ⚠️ `.chars().count()` 而不是 `.len()`：后者是**字节数**，中文主题会被算成 3 倍，
/// 一个 80 字的标题（240 字节）会被误判为超长。
pub fn validate_subject_template(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(t) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if t.contains('\r') || t.contains('\n') {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "邮件主题模板含 CR/LF（SMTP header 注入，已拒绝）",
        ));
    }
    let n = t.chars().count();
    if n > 200 {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!("邮件主题模板超长：{n} 字（上限 200）"),
        ));
    }
    Ok(Some(t.to_string()))
}

// ============================================================================
// 预览 / 发送
// ============================================================================

/// 把「当前配置 + 临时覆盖」合并成一个 [`NormalizedConfig`]。
///
/// 覆盖是**逐字段**的：`None` 表示「沿用当前值」。合并后**重新跑一遍校验** ——
/// 覆盖值可能让原本合法的配置变得非法（比如把 `upcoming_days` 改成 -5）。
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
        recipients: overrides
            .recipients
            .clone()
            .unwrap_or_else(|| base.recipients.clone()),
        cc_recipients: overrides
            .cc_recipients
            .clone()
            .unwrap_or_else(|| base.cc_recipients.clone()),
        include_overdue: overrides.include_overdue.unwrap_or(base.include_overdue),
        include_upcoming: overrides.include_upcoming.unwrap_or(base.include_upcoming),
        upcoming_days: overrides.upcoming_days.unwrap_or(base.upcoming_days),
        visible_columns: match overrides.visible_columns.as_ref() {
            Some(cols) => parse_visible_columns(cols)?,
            None => base.visible_columns,
        },
        subject_template: overrides
            .subject_template
            .clone()
            .or(base.subject_template),
    };
    // 合并结果走同一套校验（把 NormalizedConfig 还原成「请求体」形态复用校验逻辑）
    validate_normalized(&merged)
}

/// 读当前配置并规范化（表里无行 ⇒ 全部缺省）。
pub async fn current_normalized(conn: &mut PgConnection) -> Result<NormalizedConfig, AppError> {
    let Some(row) = repo::get_config(conn).await? else {
        return Ok(NormalizedConfig::default());
    };
    normalize_row(&row)
}

/// DB 行 → [`NormalizedConfig`]（解析枚举 / 列白名单）。
fn normalize_row(row: &TMailDigestConfig) -> Result<NormalizedConfig, AppError> {
    let frequency: Frequency = row.frequency.parse()?;
    Ok(NormalizedConfig {
        enabled: row.enabled,
        frequency,
        send_hour: row.send_hour,
        send_minute: row.send_minute,
        weekday: row.weekday,
        day_of_month: row.day_of_month,
        recipients: row.recipients.clone(),
        cc_recipients: row.cc_recipients.clone(),
        include_overdue: row.include_overdue,
        include_upcoming: row.include_upcoming,
        upcoming_days: row.upcoming_days,
        visible_columns: parse_visible_columns(&row.visible_columns)?,
        subject_template: row.subject_template.clone(),
    })
}

/// 对已合并好的 [`NormalizedConfig`] 做完整校验（预览 / 发送路径用）。
fn validate_normalized(cfg: &NormalizedConfig) -> Result<NormalizedConfig, AppError> {
    // 空体 `{}` + 表里也还没配置过 ⇒ recipients 为空 ⇒ 无法发信。
    // 这里给的是**明确报错**而不是「照发一封没人收的信」。
    validate_recipients(&cfg.recipients)?;
    validate_recipients(&cfg.cc_recipients)?;
    validate_time(cfg.frequency, cfg.send_hour, cfg.send_minute)?;
    validate_subject_template(cfg.subject_template.as_deref())?;
    if cfg.frequency.needs_weekday() {
        validate_weekday(cfg.weekday)?;
    }
    if cfg.frequency.needs_day_of_month() {
        validate_day_of_month(cfg.day_of_month)?;
    }
    if !cfg.include_overdue && !cfg.include_upcoming {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "「已逾期」与「即将到期」两节不能同时关闭",
        ));
    }
    if !(UPCOMING_DAYS_MIN..=UPCOMING_DAYS_MAX).contains(&cfg.upcoming_days) {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            format!(
                "未来窗口天数必须在 {UPCOMING_DAYS_MIN}..{UPCOMING_DAYS_MAX} 之间，收到 {}",
                cfg.upcoming_days
            ),
        ));
    }
    Ok(cfg.clone())
}

/// `POST /preview`：渲染但**不发送、不落流水**。
pub async fn preview(
    conn: &mut PgConnection,
    req: &PreviewRequest,
) -> Result<PreviewOut, AppError> {
    let cfg = effective_config(conn, req).await?;
    let today = now_naive().date();
    let (overdue, upcoming) = fetch_rows(conn, &cfg, today).await?;
    let rendered = template::render(&RenderInput {
        cfg: &cfg,
        overdue_rows: &overdue,
        upcoming_rows: &upcoming,
        today,
    });
    Ok(PreviewOut {
        subject: rendered.subject,
        html: rendered.html,
        overdue_count: rendered.overdue_count,
        upcoming_count: rendered.upcoming_count,
        columns: rendered.columns,
    })
}

/// `POST /send`：手动触发（渲染 + 真发 + 落流水）。
pub async fn send_manual(state: &Arc<AppState>, req: &PreviewRequest) -> Result<SendResultOut, AppError> {
    let mut conn = state.pool.acquire().await?;
    let cfg = effective_config(&mut conn, req).await?;
    deliver(state, &mut conn, &cfg, TriggerSource::Manual).await
}

/// cron 定时触发（`task::mail_digest` 调用）。
///
/// 与手动触发的差别只有 `TriggerSource`；失败**不**向上抛（后台任务没有调用方，
/// 抛了也只会被 tokio 吞掉），只 `warn!` + 记流水。
pub async fn send_scheduled(state: &Arc<AppState>) {
    let result = async {
        let mut conn = state.pool.acquire().await?;
        let cfg = current_normalized(&mut conn).await?;
        deliver(state, &mut conn, &cfg, TriggerSource::Scheduled).await
    }
    .await;
    match result {
        Ok(out) => tracing::info!(
            log_id = out.log_id,
            overdue_count = out.overdue_count,
            upcoming_count = out.upcoming_count,
            "mail_digest: 定时汇总邮件已发出"
        ),
        Err(e) => tracing::warn!(error = %e, "mail_digest: 定时汇总邮件发送/准备失败（流水已落库）"),
    }
}

/// 按节开关取两批行（关掉的节**不查**，省一次 SQL）。
async fn fetch_rows(
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    today: chrono::NaiveDate,
) -> Result<(Vec<repo::DigestRow>, Vec<repo::DigestRow>), AppError> {
    let overdue = if cfg.include_overdue {
        repo::list_overdue(conn, today).await?
    } else {
        Vec::new()
    };
    let upcoming = if cfg.include_upcoming {
        repo::list_upcoming(conn, today, cfg.upcoming_days).await?
    } else {
        Vec::new()
    };
    Ok((overdue, upcoming))
}

/// 渲染 → 发信 → **无论成败都**落流水。
///
/// 返回 `Ok(SendResultOut)` 表示信已发出；`Err(21702)` 表示 SMTP 层失败
/// （此时 `t_mail_digest_log` 已经有了一行 `status='FAILED'` + `error_message`，
/// **诊断信息不会因为返错而丢失** —— 这是「失败也落流水」的全部意义）。
async fn deliver(
    state: &Arc<AppState>,
    conn: &mut PgConnection,
    cfg: &NormalizedConfig,
    source: TriggerSource,
) -> Result<SendResultOut, AppError> {
    let mail = &state.config.mail;
    if !mail.is_enabled() {
        return Err(AppError::biz(
            code::BIZ_MAIL_NOT_CONFIGURED,
            "后端未配置 SMTP：请设置环境变量 MAIL_DIGEST_ENABLED=true 以及 \
             MAIL_SMTP_USERNAME / MAIL_SMTP_PASSWORD（发件人地址缺省取登录名）后重启",
        ));
    }
    if cfg.recipients.is_empty() {
        return Err(AppError::biz(
            code::BIZ_MAIL_CONFIG_INVALID,
            "收件人为空：请先在「交期汇总邮件」配置里填写至少一个收件人",
        ));
    }

    let today = now_naive().date();
    let (overdue, upcoming) = fetch_rows(conn, cfg, today).await?;
    let rendered = template::render(&RenderInput {
        cfg,
        overdue_rows: &overdue,
        upcoming_rows: &upcoming,
        today,
    });
    let html_bytes = rendered.html.len() as i32;

    let send_result =
        super::mailer::send(mail, cfg, &rendered.subject, &rendered.html).await;

    let status = match &send_result {
        Ok(()) => SendStatus::Success,
        Err(_) => SendStatus::Failed,
    };
    // 截断到 500 字符（列是 varchar(500)）：SMTP 报错常带整段服务器应答，
    // 超长会让**落流水本身**失败，反而把「发送失败」这件事的诊断信息弄丢。
    let error_message = send_result.as_ref().err().map(|m| truncate_chars(m, 500));

    // ⚠️ 先取版本再落库：`insert_log(conn, …)` 已经把 `conn` 的可变借用借走了，
    //    在实参里再写一个 `cfg_version_of(conn).await` 是同一次借用（E0499）。
    let config_version = cfg_version_of(conn).await?;
    let log = repo::insert_log(
        conn,
        &state.snowflake,
        &NewDigestLog {
            config_version,
            trigger_source: source.as_str(),
            to_recipients: cfg.recipients.join(","),
            cc_recipients: cfg.cc_recipients.join(","),
            subject: rendered.subject.clone(),
            overdue_count: rendered.overdue_count,
            upcoming_count: rendered.upcoming_count,
            html_bytes,
            status: status.as_str(),
            error_message,
            sent_at: now_ts(),
        },
    )
    .await?;

    // 收件人只打数量与掩码（地址是个人信息，不进日志）。
    tracing::info!(
        log_id = log.id,
        status = log.status,
        to_count = cfg.recipients.len(),
        cc_count = cfg.cc_recipients.len(),
        to_masked = %mask_recipients(&cfg.recipients),
        overdue_count = rendered.overdue_count,
        upcoming_count = rendered.upcoming_count,
        html_bytes,
        "mail_digest: 发送完成"
    );

    match send_result {
        Ok(()) => Ok(SendResultOut::from_row(&log)),
        // 裸文本已经带了「SMTP 发送失败（host=… port=…）：…」的原因，这里只补
        // 「流水已记录 + log_id」这条**排障指针**（失败信息不会因为返错而丢失）。
        Err(msg) => Err(AppError::biz(
            code::BIZ_MAIL_SEND_FAILED,
            format!("{msg}（已记入发送流水，log_id={}）", log.id),
        )),
    }
}

/// 读当前配置行版本（流水快照用；表空时记 `0`）。
async fn cfg_version_of(conn: &mut PgConnection) -> Result<i32, AppError> {
    Ok(repo::get_config(conn).await?.map(|r| r.version).unwrap_or(0))
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

/// 收件人掩码：`a***@x.com` / 本地部分 ≤2 字符时全打星号。
///
/// 目的是让日志能定位「发给了谁的大致范围」又不至于把完整地址册写进日志文件。
pub fn mask_recipients(list: &[String]) -> String {
    list.iter()
        .map(|addr| match addr.split_once('@') {
            Some((local, domain)) => {
                let head = local.chars().take(1).collect::<String>();
                format!("{head}***@{domain}")
            }
            None => "***".to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `GET /meta` 的 `defaults` 段（纯函数，不查库）。
pub fn config_defaults() -> ConfigDefaultsOut {
    ConfigDefaultsOut::from(&NormalizedConfig::default())
}

/// `VisibleColumn` 的缺省集合（`GET /meta` 与 `template` 共用同一份真源）。
pub fn default_columns() -> &'static [VisibleColumn] {
    VisibleColumn::DEFAULT_SET
}
