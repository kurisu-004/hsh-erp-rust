//! mail_digest 域 HTTP handler：6 个端点 + 路由表
//!
//! 端点（挂在 `/api/v2/mail-digest`，由 [`router`] 桥接；**全部 Manager 独占**）：
//! - `GET  /config` —— 读单例配置（表空 ⇒ 全部默认值 + `version=0` + `id="0"`）
//! - `POST /config` —— 全量覆写（body **必含** `version`）+ 触发调度热重载
//! - `GET  /meta`   —— 表单选项源（频率 / 列白名单 / 取值范围 / SMTP 公开信息）
//! - `POST /preview`—— 按「当前配置 + 临时覆盖」渲染 HTML（**不发送、不落流水**）
//! - `POST /send`   —— 手动触发（渲染 + 真发 + 落流水）
//! - `GET  /logs`   —— 发送历史分页（`limit∈[1,200]` + `offset`）
//!
//! ## 事务边界
//! 统一在 handler：`state.pool.acquire()` → 借 `&mut *conn` 给 service → drop conn。
//! 本域**不开显式事务**（读端点无写；写端点各只做单行 INSERT / 单行 UPDATE，
//! 语句自身原子）。理由见 `service` 模块 doc。
//!
//! ## 角色守卫
//! 6 个 handler 的**第一行**逐字 `current.require_role(Role::Manager)?`
//! （沿 `statistics` 域范式）。本域 6 条端点全 Manager，无第二个角色。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::mail_digest::dto::{
    ColumnOption, ConfigOut, FrequencyOption, LogListOut, MetaOut, PreviewOut, PreviewRequest,
    SaveConfigRequest, SendResultOut, SmtpPublicOut,
};
use crate::modules::mail_digest::service;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 本域 router 注册的全部路由（`METHOD /path`，相对 `/api/v2/mail-digest`）。
///
/// 与 [`router`] 的源码由 `mod tests::routes_declared_in_router` 逐条比对
/// （范式同 `prod::batch::handler::ROUTES`）—— 加了 `.route()` 忘了登记就在
/// lib 单测里当场红，而不是等到前端调 404 才发现。
pub const ROUTES: &[&str] = &[
    "GET /config",
    "POST /config",
    "GET /meta",
    "POST /preview",
    "POST /send",
    "GET /logs",
];

/// `GET /api/v2/mail-digest/config`
///
/// Manager 独占。表里无行时返回 `id="0"` / `version=0` + 全部默认值，
/// 前端据此渲染空表单并以 `version=0` 提交首次配置。
pub async fn get_config(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<ConfigOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::get_config(&mut conn, &state.config.mail).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/mail-digest/config`
///
/// Manager 独占。**全量覆写** + 标准 OCC（`version` 必填，缺失 → axum `Json`
/// 提取器返 **HTTP 422 纯文本**，不是业务信封）。
///
/// 成功后由 handler 调 [`crate::task::mail_digest::reschedule`] 热重载 cron ——
// 改完配置立刻生效，不必重启进程。热重载失败**不回滚配置**：配置已经落库是对的，
/// 只是这一次的调度没换上；日志里 warn 并让下一次重启 / 下一次改配置再试。
pub async fn save_config(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    req: axum::extract::Json<SaveConfigRequest>,
) -> Result<Json<R<ConfigOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::save_config(
        &mut conn,
        &req,
        &current,
        &state.snowflake,
        &state.config.mail,
    )
    .await?;
    drop(conn);
    crate::task::mail_digest::reschedule(&state).await;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/mail-digest/meta`
///
/// Manager 独占。**表单的全部选项源都在这一个端点**（频率 / 列白名单 / 取值范围 /
/// SMTP 公开信息 / 缺省值），前端不需要在任何地方硬编码一份枚举映射。
///
/// ⚠️ `smtp` 段**绝不含 username / password**（本段原样进前端页面）；
/// 含 `tls_mode` 是因为「收不到信」时第一件事就是看 TLS 模式配对没配对。
pub async fn meta(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<MetaOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mail = &state.config.mail;
    let out = MetaOut {
        frequencies: crate::modules::mail_digest::dto::Frequency::ALL
            .iter()
            .map(|f| FrequencyOption {
                value: f.as_str().to_string(),
                label: f.label().to_string(),
                needs_time: f.needs_time(),
                needs_weekday: f.needs_weekday(),
                needs_day_of_month: f.needs_day_of_month(),
            })
            .collect(),
        columns: crate::modules::mail_digest::dto::VisibleColumn::ALL
            .iter()
            .map(|c| ColumnOption {
                key: c.as_str().to_string(),
                label: c.label().to_string(),
            })
            .collect(),
        defaults: service::config_defaults(),
        upcoming_days_range: crate::modules::mail_digest::dto::RangeOut {
            min: crate::modules::mail_digest::dto::UPCOMING_DAYS_MIN,
            max: crate::modules::mail_digest::dto::UPCOMING_DAYS_MAX,
        },
        smtp: SmtpPublicOut {
            configured: mail.is_enabled(),
            host: mail.host.clone(),
            port: mail.port,
            tls_mode: mail.tls_mode.to_string(),
            from_address: mail.from_address.clone(),
            from_name: mail.from_name.clone(),
        },
    };
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/mail-digest/preview`
///
/// Manager 独占。按「当前配置 + 临时覆盖」渲染 HTML，**不发送、不落流水**。
/// 用途：前端配置页的「预览邮件效果」。空体 `{}` = 完全用当前配置。
pub async fn preview(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<R<PreviewOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::preview(&mut conn, &req).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/mail-digest/send`
///
/// Manager 独占。手动触发：渲染 + 真发 + 落流水（**成败都落**）。
///
/// ⚠️ 本端点**不做鉴权之外的限流**：它会对真实收件人发信。限流 / 频率限制
/// 登记为后续增强（当前 `enabled` 闸 + MANAGER 角色是唯一的护栏）。
pub async fn send(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<R<SendResultOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let out = service::send_manual(&state, &req).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/mail-digest/logs`
///
/// Manager 独占。发送历史（`sent_at DESC`），`limit` clamp 到 `[1,200]`、
/// `offset` clamp 到 `>=0`。`total` 不受 `limit` 截断影响。
pub async fn logs(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<crate::modules::mail_digest::dto::LogListQuery>,
) -> Result<Json<R<LogListOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::list_logs(&mut conn, &q).await?;
    Ok(Json(R::ok(out)))
}

/// `/api/v2/mail-digest/*` 路由表。
///
/// ⚠️ **matchit 注册顺序**：本域 6 条路由**全部是静态段**（无 `Path` 参数），
/// 故不存在「静态段必须先于 catch-all」的排序约束 —— 这一点在 `mod.rs` 的
/// 模块 doc 里也登记了，避免后人按老习惯瞎加 `/{id}` 时踩坑。
///
/// 计数口径（与 CLAUDE.md「端点求和」一节同款）：一条 `.route(path, m1)`
/// 记 1 条、`.route(path, get(h).post(h))` 记 2 条。本域 6 条全是单方法。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/config", get(get_config).post(save_config))
        .route("/meta", get(meta))
        .route("/preview", post(preview))
        .route("/send", post(send))
        .route("/logs", get(logs))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;

    use super::{ROUTES, router};
    use crate::state::AppState;

    /// 从 `router()` 源码里抠出全部 `METHOD /path`。
    ///
    /// 解析规则（对本文件的书写方式足够，且不引入构建期依赖）：
    /// - 以 `.route(` 为锚点，跳过空白后读一个 `"…"` 字面量作 path；
    /// - 从该字面量末尾到**下一个** `.route(`（或函数末）之间出现 `post(` 记 POST、
    ///   出现 `get(` 记 GET，两者都出现则**各记一条**（本域的 `/config` 正是
    ///   `.route("/config", get(..).post(..))`，一路由两方法）。
    ///
    /// ⚠️ 与 `prod::batch` 版的差别：本域有「一路由两方法」，所以这里**不能**
    /// 沿用那边「两者都出现即 panic」的写法，否则 `/config` 会把测试自己搞红。
    fn routes_in_router_source() -> Vec<String> {
        let src = include_str!("handler.rs");
        let body = src
            .split_once("pub fn router()")
            .expect("本文件必须有 pub fn router()")
            .1;
        let end = body.find("\n}\n").expect("router() 必须有收尾大括号");
        let body = &body[..end];
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            let after_ws = rest.trim_start();
            assert!(
                after_ws.starts_with('"'),
                ".route( 后必须紧跟字符串字面量 path；实测：{after_ws:.40}"
            );
            let after_quote = &after_ws[1..];
            let close = after_quote.find('"').expect("path 字面量必须有闭合引号");
            let path = &after_quote[..close];
            let tail = &after_quote[close..];
            let seg_end = tail.find(".route(").unwrap_or(tail.len());
            let seg = &tail[..seg_end];
            // 顺序敏感：源码里 `get(...)` 写在 `post(...)` 前面，故先记 GET。
            // ROUTES 也按这个顺序登记 —— 改任一侧都要同步改另一侧（测试会红）。
            let mut hit = false;
            if seg.contains("get(") {
                out.push(format!("GET {path}"));
                hit = true;
            }
            if seg.contains("post(") {
                out.push(format!("POST {path}"));
                hit = true;
            }
            assert!(hit, "`.route(\"{path}\")` 后既无 get( 也无 post(：{seg:.60}");
            rest = &tail[seg_end..];
        }
        out
    }

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// 这是登记表的牙齿：往 `router()` 加一条 `.route(...)` 而忘了登记
    /// `ROUTES` 时，这里立刻红 —— 否则要到前端调 404 那天才暴露。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source();
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(got, want, "`router()` 与 ROUTES 的条目不一致");
        }
    }

    /// 6 条路由无重复，且 `/config` 的 GET / POST 各占一条（前端契约里它们是
    /// 两个语义完全不同的动作：读 vs 覆写）。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        assert!(ROUTES.contains(&"GET /config"));
        assert!(ROUTES.contains(&"POST /config"));
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }
}
