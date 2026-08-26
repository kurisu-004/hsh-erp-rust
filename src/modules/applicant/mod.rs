//! applicant 域
//!
//! 对应 Python myERP/api/v1/applicant.py。
//! 路由前缀 `/api/v2/applicants`。本域无状态机。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
