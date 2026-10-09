//! wx::login 子模块出参 VO 层（仅 `Serialize`）
//!
//! 2026-10-11 新增。**不复用 `iam::vo::LoginResponse` / `CurrentUserOut`** —— 这是
//! 本次 wx BFF 重构的既定方向（VO 不复用任何他域结构），本文件是该方向在登录端点
//! 上的第一个落地。
//!
//! ## 为什么砍掉 `LoginResponse` 的 4 个字段
//! 旧实现直接返回 `iam::vo::LoginResponse`，即
//! `{token, refresh_token, user{id, username, full_name, is_active, roles,
//! shelf_ids, menus}}`。小程序侧真源 `wx-app/miniprogram/services/auth.ts` 的
//! `applyLoginResponse` 只读 **6 个**字段：
//!
//! | 字段 | 小程序是否读 | 处置 |
//! |---|---|---|
//! | `token` / `refresh_token` | ✅ 写 token-store | 保留 |
//! | `user.id` / `user.username` / `user.full_name` / `user.roles` | ✅ 写 storage 的 `currentUser` | 保留 |
//! | `user.is_active` | ❌ 停用账号在登录那一刻就被 `login_by_user_id` 40101 拒了，登录成功后该字段恒为 `true` | **删** |
//! | `user.shelf_ids` | ❌ 货架范围是 Web 端权限模型（`SHELF_ACCOUNT` 账号 scope），小程序不做货架作业 | **删** |
//! | `user.menus` | ❌ 小程序导航写死在页面路由里，不吃菜单树 | **删** |
//!
//! 三个删掉的字段恰好是「Web 端权限模型的三件套」，对小程序既无消费方又是**多余负载**
//! （`menus` 是整棵菜单树）。
//!
//! ⚠️ 字段名保持 `snake_case`（`refresh_token` / `full_name` / `roles`），**不**做
//! camelCase 改写 —— 前端 `applyLoginResponse` 逐字读这几个键，改名即打断登录态。
//! （camelCase 逐字对齐只适用于 [`super::super::part_list`] 的卡片 VO，那里前端
//! 侧有独立的映射层。）

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `POST /api/v2/wx/login/wecom` 出参（**不复用** `iam::vo::LoginResponse`）。
#[derive(Debug, Clone, Serialize)]
pub struct WxLoginOut {
    /// access token（JWT，RS256）。客户端放 `Authorization: Bearer`。
    pub token: String,
    /// refresh token（JWT，RS256）。用于 40102 后续期。
    pub refresh_token: String,
    /// 用户视图（4 个字段，见 [`WxLoginUserOut`]）。
    pub user: WxLoginUserOut,
}

/// 登录响应里的用户视图（**不复用** `iam::vo::CurrentUserOut`）。
///
/// ⚠️ `id` 走 `serialize_i64` 序列化为 **JSON string**（雪花 ID 全栈口径，
/// 防 JS 精度截断）；前端 `setStoredUser` 直接存字符串，不做数值转换。
#[derive(Debug, Clone, Serialize)]
pub struct WxLoginUserOut {
    /// `t_user.id`（雪花 ID → JSON string）
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// `t_user.username`
    pub username: String,
    /// `t_user.full_name`（可为**空串**，但不会是 null —— 列 NOT NULL）
    pub full_name: String,
    /// 扁平角色名列表（`MANAGER` / `CLERK` / `INSPECTOR` / `CNC_PROGRAMMER` /
    /// `SHELF_ACCOUNT`）。恒非空：无角色账号在 `login_by_user_id` 就被 20606 拒了。
    pub roles: Vec<String>,
}
