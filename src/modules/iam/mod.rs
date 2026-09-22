//! iam 域（认证 + 账号合并）
pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}