//! wecom 域 HTTP handler：6 个端点 + 路由表
//!
//! 端点（挂在 `/api/v2/cron/wecom`，由 [`router`] 桥接；**全部 Manager 独占**）：
//! - `GET  /webhooks`                —— webhook 列表（**绝不返回密文**，只返 `url_masked`）
//! - `POST /webhooks`                —— 新建（地址加密入库；不设默认）
//! - `POST /webhooks/{id}`           —— 全量覆写（body 必含 `version`；空 URL = 沿用旧密文）
//! - `POST /webhooks/{id}/delete`    —— 软删（被每日总结配置引用 ⇒ `21808`）
//! - `POST /webhooks/{id}/set-default` —— 置默认（同事务先降后升）
//! - `POST /webhooks/{id}/test`      —— 测试连通（**body 为空对象 `{}`**，⚠️ **不含 `version`**）
//!
//! ## 只用 GET / POST
//!
//! 本仓 HTTP 路由**零** PUT/PATCH/DELETE（`mail_digest` 域 2026-10-13 实测核实：
//! 全仓 `.put(` / `.patch(` / `.delete(` 的命中只有 opendal / redis 客户端调用）。
//! 故「改」走 `POST /webhooks/{id}`、「删」走 `POST /webhooks/{id}/delete` —— 与既有
//! `POST /prod/batches/{id}/receive-from-outsource` 同款约定。回归用例
//! `tests/wecom/accounts.rs::put_patch_delete_verbs_are_not_routed` 钉住这条。
//!
//! ## 角色守卫
//!
//! 6 个 handler 的**第一行**逐字 `current.require_role(Role::Manager)?`（沿
//! `scheduled_task` / `statistics` / `mail_digest` 域范式）。理由：本域管的是「能对真实
//! 外部地址发消息的凭据」，属配置型 + 有外发副作用的操作，只归管理员。
//!
//! ## 事务边界
//!
//! 常规端点 `pool.acquire()` → service 借 `&mut *conn` → drop conn（不开事务，各只做
//! 单行写）；**`set-default` 的事务在 service 内**（`service.rs::set_default_webhook`），
//! 因为「先降后升」的有序性必须由数据层保证。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde_json::Value;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::wecom::dto::{
    CreateWebhookRequest, DeleteWebhookRequest, SetDefaultWebhookRequest, UpdateWebhookRequest,
    WebhookListOut, WebhookOut,
};
use crate::modules::wecom::service;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 本域 router 注册的全部路由（`METHOD /path`，相对 `/api/v2/cron/wecom`）。
///
/// 与 [`router`] 的源码由 `mod tests::routes_declared_in_router` 逐条比对（范式同
/// `scheduled_task::handler::ROUTES` / `mail_digest::handler::ROUTES`）—— 加了
/// `.route()` 忘了登记就在 lib 单测里当场红，而不是等到前端调 404 才发现。
pub const ROUTES: &[&str] = &[
    "GET /webhooks",
    "POST /webhooks",
    "POST /webhooks/{id}",
    "POST /webhooks/{id}/delete",
    "POST /webhooks/{id}/set-default",
    "POST /webhooks/{id}/test",
];

/// `GET /api/v2/cron/wecom/webhooks`
///
/// Manager 独占。列表**不含** `encrypted_url`，只含 `url_masked`（地址前 4 + `***` + 后 4）。
///
/// ⚠️ 出参排序是 `is_default DESC, id ASC`（默认排最前）。
pub async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<WebhookListOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::list_webhooks(&mut conn).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/wecom/webhooks`
///
/// Manager 独占。新建 webhook。⚠️ **不设默认** —— 置默认是
/// `POST /webhooks/{id}/set-default` 那条独立端点的事。
pub async fn create_webhook(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<Json<R<WebhookOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::create_webhook(&mut conn, &req, &current, &state.snowflake).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/wecom/webhooks/{id}`
///
/// Manager 独占。**全量覆写** + 标准 OCC（`version` 必填，缺失 → axum `Json` 提取器返
/// **HTTP 422 纯文本**，不是业务信封）。
///
/// `webhook_url` 缺省**或**空串 ⇒ 沿用旧密文（管理员改个名称时前端拿不到地址明文，
/// 强制必填等于逼它把「不知道的东西」原样传回来）。
pub async fn update_webhook(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateWebhookRequest>,
) -> Result<Json<R<WebhookOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::update_webhook(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/wecom/webhooks/{id}/delete`
///
/// Manager 独占。软删。被 `t_wecom_daily_config.webhook_id` 引用 ⇒
/// `21808 BIZ_WECOM_WEBHOOK_IN_USE`（HTTP 409）。
///
/// ⚠️ body **必含 `version`**：本表带 `version` 列，按 CLAUDE.md §8「所有对带 `version`
/// 列的表的写端点都要显式收客户端 version 且必填」，软删也不例外。
///
/// ⚠️ **响应是 `R<()>`（`data: null`）**，与 mail 域的 `POST /accounts/{id}/delete` 同款：
/// 前端不 parse 这个响应体、只判 HTTP 状态与 `code`，返回一个 `WebhookOut` 只会白丢。
pub async fn delete_webhook(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<DeleteWebhookRequest>,
) -> Result<Json<R<()>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    service::delete_webhook(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(())))
}

/// `POST /api/v2/cron/wecom/webhooks/{id}/set-default`
///
/// Manager 独占。置默认群机器人。⚠️ 同上，body 必含 `version`（目标行的）。
///
/// 「先降旧默认、再升新默认」两条 UPDATE 在**同一事务内**（service 层开事务）——
/// 唯一索引 `uk_t_wecom_webhook_one_default` 只允许一个未软删的 `is_default = true`。
pub async fn set_default_webhook(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetDefaultWebhookRequest>,
) -> Result<Json<R<WebhookOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::set_default_webhook(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/wecom/webhooks/{id}/test`
///
/// Manager 独占。解密地址并**真发一条 text 探针**。
///
/// ⚠️ **body 是空对象 `{}`，⚠️ 不含 `version`** —— 这条端点**不改那一行**，所以没有
/// 「别人先改过」这回事，也就不该吃 OCC 锚点（前端按 `{}` 写死了）。
/// ⚠️ **不写任何日志表**（`t_wecom_daily_log` 一行都不动）。
///
/// ⚠️ **响应是 `R<()>`（`data: null`）**：前端不 parse 响应体。失败时走 `21802`
/// （HTTP 502）或 `21806`（HTTP 404），message 里**不含地址与 key**（见 `sender.rs`）。
///
/// 入参取 `serde_json::Value` 而不是自定义空结构：本端点**刻意不读 body 的任何字段**，
/// 取 `Value` 只是为了让「空对象 / 其它 JSON」都被接受，解析失败（如完全没 body）仍是
/// axum 默认的 400 纯文本。
pub async fn test_webhook(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(_body): Json<Value>,
) -> Result<Json<R<()>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    service::send_probe(&mut conn, id).await?;
    Ok(Json(R::ok(())))
}

/// `/api/v2/cron/wecom/*` 路由表。
///
/// ⚠️ **matchit 注册顺序无约束**：本域最长段数 3（`/webhooks/{id}/delete`），最短 1
/// （`/webhooks`）；**不存在 2 段的静态路径**（`Path<i64>` 只出现在 `/webhooks/{id}`），
/// 故不存在「静态段被 catch-all 吞成 400」的风险。
/// 与 `/cron/mail` 侧那张表也互不参与（各自独立 `.nest()`，见 `crate::modules::v2_router`
/// 的 `/cron` 前缀族注释）。
///
/// 计数口径（与 CLAUDE.md「端点求和」一节同款）：一条 `.route(path, m1)` 记 1 条、
/// `.route(path, get(h).post(h))` 记 2 条。本域 6 条全是单方法。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route("/webhooks/{id}", post(update_webhook))
        .route("/webhooks/{id}/delete", post(delete_webhook))
        .route("/webhooks/{id}/set-default", post(set_default_webhook))
        .route("/webhooks/{id}/test", post(test_webhook))
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
    ///   出现 `get(` 记 GET，两者都出现则**各记一条**。
    ///
    /// 顺序敏感：源码里 `get(...)` 写在 `post(...)` 前面，故先记 GET。
    /// `ROUTES` 也按这个顺序登记 —— 改任一侧都要同步改另一侧（测试会红）。
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
            let mut hit = false;
            if seg.contains("get(") {
                out.push(format!("GET {path}"));
                hit = true;
            }
            if seg.contains("post(") {
                out.push(format!("POST {path}"));
                hit = true;
            }
            assert!(
                hit,
                "`.route(\"{path}\")` 后既无 get( 也无 post(：{seg:.60}"
            );
            rest = &tail[seg_end..];
        }
        out
    }

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
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

    /// 6 条路由无重复，且 `/webhooks` 的 GET / POST 各占一条（读列表 vs 新建是两个
    /// 语义完全不同的动作）。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        assert!(ROUTES.contains(&"GET /webhooks"));
        assert!(ROUTES.contains(&"POST /webhooks"));
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }
}
