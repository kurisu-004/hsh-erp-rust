//! scheduled_task 域 HTTP handler：6 个端点 + 路由表
//!
//! 端点（挂在 `/api/v2/cron/mail`，由 [`router`] 桥接；**全部 Manager 独占**）：
//! - `GET  /meta`                  —— SMTP 预设表（纯函数，不查库）
//! - `GET  /accounts`              —— 邮箱列表（**绝不返回密文**）
//! - `POST /accounts`              —— 新建（授权码加密入库；不设默认）
//! - `POST /accounts/{id}`         —— 全量覆写（body 必含 `version`；空 `secret` = 沿用旧密文）
//! - `POST /accounts/{id}/delete`  —— 软删（被 mail_digest 引用 ⇒ `21708`）
//! - `POST /accounts/{id}/set-default` —— 置默认（同事务先降后升）
//!
//! ## 只用 GET / POST
//!
//! 本仓 HTTP 路由**零** PUT/PATCH/DELETE（`mail_digest` 域 2026-10-13 实测核实：
//! 全仓 `.put(` / `.patch(` / `.delete(` 的命中只有 opendal / redis 客户端调用）。
//! 故「改」走 `POST /accounts/{id}`、「删」走 `POST /accounts/{id}/delete` ——
//! 与既有 `POST /prod/batches/{id}/receive-from-outsource` 同款约定。
//!
//! ## 角色守卫
//!
//! 6 个 handler 的**第一行**逐字 `current.require_role(Role::Manager)?`
//! （沿 `statistics` / `mail_digest` 域范式）。理由：本域管的是「能对真实外部
//! 地址发信的凭据」，属配置型 + 有外发副作用的操作，只归管理员。
//!
//! ## 事务边界
//!
//! 常规端点 `pool.acquire()` → service 借 `&mut *conn` → drop conn（不开事务，
//! 各只做单行写）；**`set-default` 的事务在 service 内**（`service.rs` 的
//! `set_default_account`），因为「先降后升」的有序性必须由数据层保证。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::scheduled_task::dto::{
    CreateAccountRequest, DeleteAccountRequest, MailAccountListOut, MailAccountOut, MailMetaOut,
    SetDefaultAccountRequest, UpdateAccountRequest,
};
use crate::modules::scheduled_task::service;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 本域 router 注册的全部路由（`METHOD /path`，相对 `/api/v2/cron/mail`）。
///
/// 与 [`router`] 的源码由 `mod tests::routes_declared_in_router` 逐条比对
/// （范式同 `mail_digest::handler::ROUTES` / `prod::batch::handler::ROUTES`）
/// —— 加了 `.route()` 忘了登记就在 lib 单测里当场红，而不是等到前端调 404 才发现。
pub const ROUTES: &[&str] = &[
    "GET /meta",
    "GET /accounts",
    "POST /accounts",
    "POST /accounts/{id}",
    "POST /accounts/{id}/delete",
    "POST /accounts/{id}/set-default",
];

/// `GET /api/v2/cron/mail/meta`
///
/// Manager 独占。**纯函数、不开事务、不查库**：预设表是编译期常量，
/// 前端据此渲染「服务商」下拉，不需要在前端另存一份 host/port 映射。
pub async fn meta(current: CurrentUser) -> Result<Json<R<MailMetaOut>>, AppError> {
    current.require_role(Role::Manager)?;
    Ok(Json(R::ok(service::meta())))
}

/// `GET /api/v2/cron/mail/accounts`
///
/// Manager 独占。列表**不含** `encrypted_secret`，也没有掩码字段
/// （见 `dto::MailAccountOut` 的 doc）。
pub async fn list_accounts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<MailAccountListOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::list_accounts(&mut conn).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/mail/accounts`
///
/// Manager 独占。新建邮箱。⚠️ **不设默认** —— 置默认是
/// `POST /accounts/{id}/set-default` 那条独立端点的事。
pub async fn create_account(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CreateAccountRequest>,
) -> Result<Json<R<MailAccountOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::create_account(&mut conn, &req, &current, &state.snowflake).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/mail/accounts/{id}`
///
/// Manager 独占。**全量覆写** + 标准 OCC（`version` 必填，缺失 → axum `Json`
/// 提取器返 **HTTP 422 纯文本**，不是业务信封）。
///
/// `secret` 缺省**或**空串 ⇒ 沿用旧密文（管理员改个名称时前端拿不到明文，
/// 强制必填等于逼它把「不知道的东西」原样传回来）。
pub async fn update_account(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<R<MailAccountOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::update_account(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/cron/mail/accounts/{id}/delete`
///
/// Manager 独占。软删。被 `t_mail_digest_config.sender_account_id` 引用 ⇒
/// `21708 BIZ_MAIL_ACCOUNT_IN_USE`（HTTP 409）。
///
/// ⚠️ body **必含 `version`**：本表带 `version` 列，按 CLAUDE.md §8「所有对带
/// `version` 列的表的写端点都要显式收客户端 version 且必填」，软删也不例外。
pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<DeleteAccountRequest>,
) -> Result<Json<R<()>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    service::delete_account(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(())))
}

/// `POST /api/v2/cron/mail/accounts/{id}/set-default`
///
/// Manager 独占。置默认邮箱。⚠️ 同上，body 必含 `version`（目标行的）。
///
/// 「先降旧默认、再升新默认」两条 UPDATE 在**同一事务内**（service 层开事务）——
/// 唯一索引 `uk_t_mail_account_one_default` 只允许一个未软删的 `is_default = true`。
pub async fn set_default_account(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<SetDefaultAccountRequest>,
) -> Result<Json<R<MailAccountOut>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut conn = state.pool.acquire().await?;
    let out = service::set_default_account(&mut conn, id, &req, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `/api/v2/cron/mail/*` 路由表。
///
/// ⚠️ **matchit 注册顺序无约束**：本域最长段数 3（`/accounts/{id}/delete`），
/// 最短 1（`/meta` / `/accounts`），**不存在同段数下的「静态段 vs catch-all」
/// 争用** —— `Path<i64>` 只出现在第 2 段，而没有任何 2 段的静态路径。
/// 与 `mail_digest` 域那条「6 条全是静态段，故无排序约束」是同一结论的两种形态；
/// 后人往 2 段位上加工具端点时需重新审视这一点。
///
/// 计数口径（与 CLAUDE.md「端点求和」一节同款）：一条 `.route(path, m1)`
/// 记 1 条、`.route(path, get(h).post(h))` 记 2 条。本域 6 条全是单方法。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/meta", get(meta))
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/{id}", post(update_account))
        .route("/accounts/{id}/delete", post(delete_account))
        .route("/accounts/{id}/set-default", post(set_default_account))
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

    /// 6 条路由无重复，且 `/accounts` 的 GET / POST 各占一条（读列表 vs 新建是
    /// 两个语义完全不同的动作）。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        assert!(ROUTES.contains(&"GET /accounts"));
        assert!(ROUTES.contains(&"POST /accounts"));
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }
}
