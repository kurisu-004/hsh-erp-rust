//! process 域
//!
// 对应 Python myERP：
//! - api/v1/process.py
//! - service/process_service.py
//! - repository/process_repository.py
//! - model/process.py
//! - schema/process.py
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
