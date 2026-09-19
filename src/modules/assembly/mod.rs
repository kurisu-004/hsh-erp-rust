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

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
