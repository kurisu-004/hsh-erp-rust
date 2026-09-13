//! part_file 域
//!
//! 对应 Python myERP：
//! - api/v1/part_file.py
//! - service/part_file_service.py
//! - repository/part_file_repository.py
//! - model/part_file.py
//! - schema/part_file.py
//!
//! 2026-09-14 Phase 3：补齐 service / handler / DTO；polymorphic owner（PART / ASSEMBLY）；
//! SHA-256 CAS 去重 + COS 预签下载 URL。
pub mod dto;
pub mod handler;
pub mod model;
pub mod policy;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}