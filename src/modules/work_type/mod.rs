//! work_type 域
//!
// 对应 Python myERP：
//! - api/v1/work_type.py
//! - service/work_type_service.py
//! - repository/work_type_repository.py
//! - model/work_type.py
//! - schema/work_type.py
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
