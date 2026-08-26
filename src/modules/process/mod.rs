//! process 域
//!
//! 对应 Python myERP：
//! - api/v1/process.py
//! - service/process_service.py
//! - repository/process_repository.py
//! - model/process.py
//! - schema/process.py
//!
//! Phase P2 process CRUD：5 端点挂在 `/api/v2/processes`。
//! 业务约束：
//! - INHOUSE 强制 `requires_approval = false`
//! - OUTSOURCE 保留请求值（默认 true）
//! - `code` 业务唯一键，update 不可改
//! - 软删前查 work_type_process + outsource_company_process + shelf_process +
//!   t_part.next_process_id 引用计数（best-effort）
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