//! cnc_program 域
//!
// 对应 Python myERP：
//! - api/v1/cnc_program.py
//! - service/cnc_program_service.py
//! - repository/cnc_program_repository.py
//! - model/cnc_program.py
//! - schema/cnc_program.py
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
