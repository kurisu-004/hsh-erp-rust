//! worker 域
//!
// 对应 Python myERP：
//! - api/v1/worker.py
//! - service/worker_service.py
//! - repository/worker_repository.py
//! - model/worker.py
//! - schema/worker.py
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;


use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}
