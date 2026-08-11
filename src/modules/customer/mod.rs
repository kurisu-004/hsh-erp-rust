//! customer 域
//!
// 对应 Python myERP：
//! - api/v1/customer.py
//! - service/customer_service.py
//! - repository/customer_repository.py
//! - model/customer.py
//! - schema/customer.py
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
