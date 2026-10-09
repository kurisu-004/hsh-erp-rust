//! wx::login 子模块 —— 微信小程序企业微信一键登录（2026-10-11 新增）
//!
//! ## 为什么单独切一个 `login/` 子模块
//! 2026-10-11 之前，本域的登录端点叫 `POST /api/v2/wx/iam/wx-login`，实现平铺在
//! `src/modules/wx/auth.rs`（旧文件名 `auth.rs` 与「鉴权中间件」撞名，容易被误读
//! 为「本文件负责鉴权」，实际它只负责**登录**）。BFF 按小程序页面切子模块的重构
//! 把它搬进 `login/`，并把 URL 硬切为 `POST /api/v2/wx/login/wecom`（跟「页面 /
//! 动作名」走，不再复用他域的 `/iam` 前缀）。**旧路径无 alias，硬切即 404。**
//!
//! ## 端点
//! - `POST /api/v2/wx/login/wecom` —— **公开**企业微信小程序登录（唯一端点）
//!
//! ## 身份源：企业微信 userid（不是微信 openid）
//! 小程序**只会在企业微信客户端内打开**，故不需要处理 openid / 微信端分支。身份
//! 走企业微信 `GET /cgi-bin/miniprogram/jscode2session`（**不是**微信的
//! `api.weixin.qq.com/sns/jscode2session`），且必须是**自建应用**（第三方应用
//! 返回的是加密 userid，本方案不适用）。
//!
//! ## 仅预绑定，不自动开户
//! userid 必须在 `t_wx_identity` 中已由管理员预绑定到某个 `t_user.id`，否则直接
//! 40107。**不**自动开户 —— 企业微信通讯录与本系统账号并非一一对应，自动开户会让
//! 离职 / 外部协作人员凭一个 userid 拿到系统权限。
//!
//! 2026-10-10 起绑定表的查询与系统账号解析已合进 iam 域的
//! `AccountService::resolve_wx_login_user`（`t_wx_identity` 的 SQL 真源属 iam 域），
//! 本子模块对该表**零 SQL**。
//!
//! ## `session_key` 拿到即丢
//! 本方案只用 userid 做身份映射，不解密任何业务数据（无手机号 / 头像昵称），因此
//! `session_key` 在 `HttpWeComClient` 反序列化瞬间即被丢弃：**不落库**、不进日志、
//! 不进本子模块任何作用域。`corpsecret` / `access_token` 同理，见
//! [`super::wecom_client`] 模块 doc 的安全硬约束段。
//!
//! ## 鉴权白名单（⚠️ 两处，漏一处即安全漏洞）
//! 本端点是**公开路径**（调用方是小程序，还没有 token）：
//! - `src/auth/middleware.rs::is_public_path` —— 免 Bearer 校验
//! - `src/middleware/idempotency.rs::is_public_idempotency_path` —— 不缓存响应
//!
//! 第二处尤其关键：登录响应含 JWT，若被 idempotency 缓存，攻击者复用同一个
//! `Idempotency-Key` 即可劫持他人 session。
//!
//! ## 事务分层（2026-10-11 重构后仍逐字保留旧 `auth.rs` 语义，只搬位置）
//! 外部 HTTP 调用**必须在事务外** —— 持着 PG 连接等企微接口（最长 5s 超时）会
//! 迅速耗尽连接池。严格两段：
//! 1. **事务外**：`code_to_session` 换 userid + corpid 比对 + 归一化
//! 2. **事务内**：`resolve_wx_login_user` → `login_by_user_id`（DB 写
//!    `last_login_at`）→ commit
//! 3. **commit 后**：`complete_login` 写 2 条 Redis session（access_jti +
//!    refresh_jti）
//!
//! ⚠️ 第 3 步不能省：只签 JWT 不写 Redis session，第一个鉴权请求就会 40105。回归
//! 由 `tests/wecom_login.rs::wx_login_issued_token_works_on_iam_me` 钉死。
//!
//! ## 不复用 `iam::vo::LoginResponse`（2026-10-11 新增决策）
//! 旧实现直接返回 `iam::vo::LoginResponse`（8 字段：`token` / `refresh_token` +
//! `user{id, username, full_name, is_active, roles, shelf_ids, menus}`）。本子模块
//! 改出自己的 [`vo::WxLoginOut`]（6 字段），理由见 `vo.rs` 的模块 doc：前端的
//! `applyLoginResponse`（`wx-app/miniprogram/services/auth.ts:78-87`）只读 6 个
//! 字段，`expires_in` / `is_active` / `shelf_ids` / `menus` 小程序全用不到，而
//! `shelf_ids` / `menus` 是 Web 端货架范围与菜单树的载体，对小程序是**多余负载**。
//! iam 域的结构在 [`service`] 层只作为**中间值**存在，不作为响应类型。
//!
//! ## 模块结构（照 `prod::process_design` / `prod::inspection` 范式）
//! - `dto.rs` —— 入参（仅 `Deserialize`；校验在 service 层显式做，见 `dto.rs` doc）
//! - `vo.rs` —— 出参（仅 `Serialize`）
//! - `service.rs` —— ZST + 静态方法（投影 iam 中间值 → `WxLoginOut`）
//! - `handler.rs` —— HTTP 路由 + **事务边界**（显式 `begin` / `commit`）+ `R::ok`

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod service;
pub mod vo;

/// `/api/v2/wx/login/*` 入口 router 工厂（转发式：`mod.rs` 只放 `pub fn`，
/// 路由表在 [`handler`]）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
