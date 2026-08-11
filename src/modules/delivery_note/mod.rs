//! delivery_note 域
//!
// 对应 Python myERP：
//! - api/v1/delivery_note.py
//! - service/delivery_note_service.py
//! - repository/delivery_note_repository.py
//! - model/delivery_note.py
//! - schema/delivery_note.py
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
