//! wx::login 子模块 handler 层 —— HTTP 路由 + 事务边界
//!
//! 2026-10-11 自旧 `src/modules/wx/auth.rs` 平铺实现迁入。
//!
//! ## 端点
//! - `POST /api/v2/wx/login/wecom` —— **公开**企业微信小程序登录
//!
//! ## ⚠️ 本端点是全域唯一**有事务**的 handler
//! 事务必须开在 handler（service 不知事务），且分三段：
//! 1. 事务**外**调企业微信外部 HTTP（`service::wecom_login` 的前半段）
//! 2. 事务**内**解析绑定 + 签 token
//! 3. commit **后**写 Redis session
//!
//! 这条边界写在 [`super`] 模块 doc「事务分层」段，后人不要为了「service 一把梭」
//! 把 `pool.begin()` 挪进 service。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;

use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::WxLoginRequest;
use super::service::WxLoginService;
use super::vo::WxLoginOut;

/// `/api/v2/wx/login/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/wecom", post(wecom_login))
}

/// `POST /api/v2/wx/login/wecom` —— 企业微信小程序登录。
///
/// 流程（⚠️ 三段式，理由见模块 doc）：
/// 1. **事务外** `code_to_session` 换 `WeComSession { corp_id, user_id }` +
///    corpid 比对（防跨企业串号）+ userid 归一化（trim + 转小写）
/// 2. **事务内** `resolve_wx_login_user`（查 `t_wx_identity` 预绑定）→
///    `login_by_user_id`（复用 iam 登录流水线：is_active → 角色 → 签双 token）→
///    commit
/// 3. **commit 后** `complete_login`（写 Redis session）→ 投影成 `WxLoginOut`
///
/// 错误码：
/// - 40001 VALIDATION —— `code` 为空 / 超长（`WxLoginRequest::validate`）
/// - 40106 BIZ_WX_LOGIN_FAILED —— 企微 `40029` / token 失效重取后仍失败；
///   或企微返回的 userid 为空
/// - 40107 BIZ_WX_NOT_BOUND —— corpid 不符，或 userid 未预绑定
/// - 40109 BIZ_WX_NOT_CONFIGURED —— 后端未配置 `WECOM_CORPID` / `WECOM_CORPSECRET`
/// - 40101 BIZ_AUTH_INVALID —— 绑定指向的用户已软删 / 已停用
/// - 20606 NO_ROLE —— 绑定用户未分配任何角色
///
/// 本端点在 `auth::middleware::is_public_path` 与
/// `middleware::idempotency::is_public_idempotency_path` 两处白名单里（⚠️ 加公开
/// 路径必须两处同步改）。
pub async fn wecom_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WxLoginRequest>,
) -> Result<Json<R<WxLoginOut>>, AppError> {
    // ---- 1. 事务外：入参校验 + 企微外部 HTTP + corpid 比对 + userid 归一化 -----
    //    绝不能把这一步挪进事务：持着 PG 连接等企微接口（最长 5s 超时）会迅速
    //    耗尽连接池。
    let ident = WxLoginService::exchange_wecom_identity(&state, &req).await?;

    // ---- 2. 事务内：查预绑定 → 取系统账号 → 签双 token ---------------------
    let mut tx = state.pool.begin().await?;

    // 2a+2b. 查预绑定 + 取系统账号（两步合进 iam 域的 `resolve_wx_login_user`：
    //       绑定表的 SQL 真源属 iam 域，wx 域对 `t_wx_identity` 零 SQL）。
    //       错误码语义与拆开写时逐字相同：未绑定 → 40107；账号已软删/不存在 → 40101。
    let user = state
        .account_service
        .resolve_wx_login_user(&mut *tx, &ident.corp_id, &ident.wx_user_id)
        .await?;

    // 2c. 复用 iam 登录流水线（is_active → 角色 → shelf 范围 → 菜单 → 签双 token）
    let pending = state
        .session_service
        .login_by_user_id(&mut *tx, user)
        .await?;
    tx.commit().await?;

    // ---- 3. commit 后：写 Redis session（access_jti + refresh_jti）----------
    // ⚠️ 这一步不能省：只签 JWT 不写 session，第一个鉴权请求就会 40105。
    let resp = state.session_service.complete_login(pending).await?;

    // 4. 投影成 wx 域自己的 VO（不带 is_active / shelf_ids / menus）
    let out = WxLoginService::project_login(resp);

    tracing::info!(
        user_id = out.user.id,
        "wx-login 成功（企业微信 userid 预绑定登录）"
    );
    Ok(Json(R::ok(out)))
}
