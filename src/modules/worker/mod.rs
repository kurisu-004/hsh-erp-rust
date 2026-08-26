//! worker 域
//!
//! 对应 Python myERP：
//! - api/v1/worker.py
//! - service/worker_service.py
//! - repository/worker_repository.py
//! - model/worker.py
//! - schema/worker.py
//!
//! Phase P4 worker CRUD：7 端点挂在 `/api/v2/workers`
//! 公开（任意已登录）：`POST /workers/verify-badge`
//! MANAGER-only（6）：`GET /workers` / `POST /workers` / `GET /workers/{id}` /
//!                    `POST /workers/{id}/update` / `POST /workers/{id}/deactivate` /
//!                    `POST /workers/{id}/reactivate`

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
