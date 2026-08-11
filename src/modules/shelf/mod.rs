//! shelf 域
//!
// 对应 Python myERP：
//! - api/v1/shelf.py
//! - service/shelf_service.py
//! - repository/shelf_repository.py
//! - model/shelf.py
//! - schema/shelf.py
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
