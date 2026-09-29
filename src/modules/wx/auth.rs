//! 微信小程序 BFF / auth 域：`POST /api/v2/wx/iam/wx-login`（2026-09-29 重写）
//!
//! > 2026-09-29 重写说明：本文件原是 2026-09-28 的**微信 openid 占位**文档
//! > （内容已过时——写的是微信 `api.weixin.qq.com/sns/jscode2session` + openid +
//! > 未来新增 `wx_openid` 列）。实际落地是企业微信方案，以下为正典。
//!
//! ## 身份源：企业微信 userid（不是微信 openid）
//! 小程序**只会在企业微信客户端内打开**，因此不需要处理 openid / 微信端分支。
//! 换取身份走企业微信的 `GET /cgi-bin/miniprogram/jscode2session`
//! （**不是**微信的 `api.weixin.qq.com/sns/jscode2session`）：
//!
//! ```text
//! GET /cgi-bin/miniprogram/jscode2session
//!        ?access_token={AT}&js_code={CODE}&grant_type=authorization_code
//!   → { "corpid":"...", "userid":"...", "session_key":"...", "errcode":0 }
//! ```
//!
//! ## 应用类型：自建应用
//! 自建应用的 `jscode2session` 返回**明文 userid**。若换成第三方应用，返回的
//! 是加密 userid（需 `suite_access_token` + `auth/getuserinfo3rd` 二次解密），
//! 本方案**不适用**——配置时必须用自建应用的 Secret。
//!
//! ## 仅预绑定，不自动开户
//! userid 必须在 `t_wx_identity`（`migrations/20260929000000_001_add_wx_identity.sql`）
//! 中已由管理员预绑定到某个 `t_user.id`，否则直接拒绝（`40107 BIZ_WX_NOT_BOUND`）。
//! **不**自动创建账号——企业微信通讯录与本系统账号并非一一对应，自动开户会让
//! 离职/外部协作人员凭一个 userid 拿到系统权限。
//!
//! ## `session_key` 拿到即丢
//! 本方案只用 userid 做身份映射，不解密任何业务数据（无手机号 / 头像昵称），
//! 因此 `session_key` 在 `HttpWeComClient` 反序列化瞬间即被丢弃，**不落库**、
//! 不进日志、不进本 handler 的作用域（见 `wecom_client.rs` 模块 doc 的安全硬约束段）。
//!
//! ## 鉴权白名单（⚠️ 两处，漏一处即安全漏洞）
//! 本端点是**公开路径**（调用方是小程序，还没有 token）：
//! - `src/auth/middleware.rs::is_public_path` —— 免 Bearer 校验
//! - `src/middleware/idempotency.rs::is_public_idempotency_path` —— 不缓存响应
//!
//! 第二处尤其关键：登录响应含 JWT，若被 idempotency 缓存，攻击者复用同一个
//! `Idempotency-Key` 即可劫持他人 session。
//!
//! ## 事务分层
//! 外部 HTTP 调用**必须在事务外**——持着 PG 连接等企微接口（最长 5s 超时）会
//! 迅速耗尽连接池。严格两段：
//! 1. 事务外：`code_to_session` 换 userid + corpid 比对 + 查绑定 + 查 user
//! 2. 事务内：`login_by_user_id`（DB 写 `last_login_at`）→ commit
//! 3. commit 后：`complete_login` 写 2 条 Redis session（access_jti + refresh_jti）
//!
//! 响应直接复用 `iam::vo::LoginResponse`（`{token, refresh_token, user}`），
//! 与账号密码登录逐字同构——小程序前端零改动即可复用既有登录态处理逻辑。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;

use crate::modules::iam::repo::sql::user::get_user_by_id;
use crate::modules::iam::vo::LoginResponse;
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::WxLoginRequest;
use super::repo::{WxIdentityRepo, WxIdentity};

/// `/api/v2/wx/iam/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/wx-login", post(wx_login))
}

/// `POST /api/v2/wx/iam/wx-login` —— 企业微信小程序登录。
///
/// 流程（事务边界见模块 doc「事务分层」段）：
/// 1. **事务外** `code_to_session` 换 `WeComSession { corp_id, user_id }`
/// 2. **事务外** 比对 `corp_id == config.wecom.corpid`（防跨企业串号）
/// 3. 开事务 → 查 `t_wx_identity` 预绑定 → 查 `t_user` → `login_by_user_id`
/// 4. commit → `complete_login`（写 Redis session）→ 返回 `LoginResponse`
///
/// 错误码：
/// - 40001 VALIDATION —— `code` 为空 / 超长（`WxLoginRequest::validate`）
/// - 40106 BIZ_WX_LOGIN_FAILED —— 企微 `40029` / token 失效重取后仍失败
/// - 40107 BIZ_WX_NOT_BOUND —— corpid 不符，或 userid 未预绑定
/// - 40109 BIZ_WX_NOT_CONFIGURED —— 后端未配置 `WECOM_CORPID` / `WECOM_CORPSECRET`
/// - 40101 BIZ_AUTH_INVALID —— 绑定指向的用户已软删
/// - 20606 NO_ROLE —— 绑定用户未分配任何角色
pub async fn wx_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WxLoginRequest>,
) -> Result<Json<R<LoginResponse>>, AppError> {
    // 入参校验（trim + 非空 + ≤512 字节）
    let code = req.validate()?;

    // ---- 1. 事务外：换取企业微信身份 ----------------------------------------
    // `session_key` 在 `HttpWeComClient` 内部即被丢弃，此处拿到的
    // `WeComSession` 只有 corp_id + user_id 两个字段。
    let sess = state.wecom.code_to_session(&code).await?;

    // ---- 2. 事务外：防跨企业串号 --------------------------------------------
    // 配置未启用时 corpid 为空 → 已在第 1 步被 `NoopWeComClient` 拒掉（40109），
    // 到这里 corpid 必非空。
    let expected_corp = state.config.wecom.corpid.trim();
    if expected_corp.is_empty() || sess.corp_id.trim() != expected_corp {
        tracing::warn!(
            returned_corp_len = sess.corp_id.len(),
            "wx-login: 企微返回 corpid 与本地配置不符，拒绝登录"
        );
        return Err(AppError::biz(
            code::BIZ_WX_NOT_BOUND,
            "该企业微信账号未绑定系统账号，请联系管理员",
        ));
    }

    // userid 统一小写（企微 userid 不区分大小写；绑定表存小写）
    let wx_user_id = sess.user_id.trim().to_lowercase();
    if wx_user_id.is_empty() {
        return Err(AppError::biz(
            code::BIZ_WX_LOGIN_FAILED,
            "企业微信登录失败：返回的 userid 为空",
        ));
    }

    // ---- 3. 事务内：查绑定 → 查 user → 签 token ---------------------------
    let mut tx = state.pool.begin().await?;

    // 3a. 预绑定查询（仅预绑定，未绑定即拒）
    let identity: Option<WxIdentity> = WxIdentityRepo::get_by_corp_and_user(
        &mut *tx,
        expected_corp,
        &wx_user_id,
    )
    .await?;
    let Some(identity) = identity else {
        return Err(AppError::biz(
            code::BIZ_WX_NOT_BOUND,
            "该企业微信账号未绑定系统账号，请联系管理员",
        ));
    };

    // 3b. 取系统账号（软删 / 不存在 → 40101；不泄露「绑定记录存在但账号已删」）
    let user = get_user_by_id(&mut *tx, identity.user_id)
        .await?
        .ok_or_else(|| {
            tracing::warn!(
                user_id = identity.user_id,
                "wx-login: 绑定指向的用户不存在或已软删"
            );
            AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误")
        })?;

    // 3c. 复用 iam 登录流水线（is_active → 角色 → shelf 范围 → 菜单 → 签双 token）
    let pending = state.session_service.login_by_user_id(&mut *tx, user).await?;
    tx.commit().await?;

    // ---- 4. commit 后：写 Redis session（access_jti + refresh_jti）--------
    // ⚠️ 这一步不能省：只签 JWT 不写 session，第一个鉴权请求就会 40105。
    let resp = state.session_service.complete_login(pending).await?;

    tracing::info!(
        user_id = resp.user.id,
        "wx-login 成功（企业微信 userid 预绑定登录）"
    );
    Ok(Json(R::ok(resp)))
}
