//! cnc_program 域
//!
//! 对应 Python myERP：
//! - api/v1/cnc_program.py
//! - service/cnc_program_service.py
//! - repository/cnc_program_repository.py
//!
//! 2026-09-14 Phase 3：补齐 cnc-pair 配对上传 + 列表端点。
//! 存储复用 part_file（kind='G_CODE' + kind='SETUP_SHEET'，paired_file_id 互指）。
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
