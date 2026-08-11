//! statistics 域（生产统计）
//!
// 对应 Python myERP：
//! - api/v1/statistics.py
//! - service/statistics_service.py
//! - repository/statistics_repository.py
//! - schema/statistics.py

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}
