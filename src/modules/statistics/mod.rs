//! statistics 域（生产统计，MANAGER-only）
//!
//! 对应 Python myERP：
//! - api/v1/statistics.py
//! - service/statistics_service.py
//! - repository/statistics_repository.py
//! - schema/statistics.py
//!
//! 2026-09-15 takeover-fill：完整化 5 端点（overview / workers / workers/{id} /
//! pickup-skips / pickup-skips/{id}），权限统一 MANAGER-only。

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}