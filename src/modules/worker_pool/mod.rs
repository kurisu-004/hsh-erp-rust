//! worker_pool 域
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::{routing::{get, post}, Router};
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/state", get(handler::state))
        .route("/{process_id}", get(handler::pool_by_process))
}

pub fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/refill", post(handler::admin_refill))
        .route("/remove", post(handler::admin_remove))
}