//! part 域
//!
// 对应 Python myERP：
//! - api/v1/part.py
//! - service/part_service.py
//! - repository/part_repository.py
//! - model/part.py
//! - schema/part.py
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
