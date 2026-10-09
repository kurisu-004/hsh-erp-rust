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
//! 2026-10-12：pickup-skips 两端点随 `t_pickup_skip_event` 删除下线，剩 3 端点。

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
