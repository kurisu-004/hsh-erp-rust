//! auth 域（登录 / 当前用户 / 改密 / refresh / 登出）
//!
//! 对应 Python myERP/api/v1/auth.py + service/auth_service.py。
//! 注意：横切的 JWT 编解码、CurrentUser extractor、RBAC 角色枚举均在顶层
//! `crate::auth`，本模块仅承载业务 auth handler/service。

pub mod dto;
pub mod handler;
pub mod service;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
