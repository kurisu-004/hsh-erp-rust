//! assembly 域
//!
// 对应 Python myERP：
//! - api/v1/assembly.py
//! - service/assembly_service.py
//! - repository/assembly_repository.py
//! - model/assembly.py
//! - schema/assembly.py
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
