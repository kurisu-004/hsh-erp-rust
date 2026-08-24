//! part 域
//!
//! 对应 Python myERP：
//! - api/v1/part.py
//! - service/part_service.py
//! - repository/part_repository.py
//! - model/part.py
//! - schema/part.py
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;
use axum::{routing::post, Router};

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ★ 静态段必须在 /{part_id}/... catch-all 之前注册，
        //   否则 axum 会把 `batch-pass-inspection` 解析成 part_id。
        .route(
            "/batch-pass-inspection",
            post(handler::batch_pass_inspection),
        )
        .route(
            "/{part_id}/pass-inspection",
            post(handler::pass_inspection),
        )
}