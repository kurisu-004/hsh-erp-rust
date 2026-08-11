//! outsource 域
//!
// 对应 Python myERP：
//! - api/v1/outsource.py
//! - service/outsource_service.py
//! - repository/outsource_repository.py
//! - model/outsource.py
//! - schema/outsource.py
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}
