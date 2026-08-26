//! shelf 域
//!
//! 对应 Python myERP：
//! - api/v1/shelf.py
//! - service/shelf_service.py
//! - repository/shelf_repository.py
//! - model/shelf.py
//! - schema/shelf.py
//!
//! ## Phase P3+ shelf CRUD：11 端点挂在 `/api/v2/shelves`
//! 读 3：list / get / list_shelf_processes
//! picker 3：list_for_return / list_for_inspection / list_all_process_mappings
//! 写 5 (MANAGER)：create / update / deactivate / set_shelf_processes
//!
//! ## 子模块
//! `process_mapping.rs` 单文件拆出 `t_shelf_process` 的 repo + service，
//! 让主 `service.rs` 控制在 1000 行内（conventions.md §2）。
pub mod dto;
pub mod handler;
pub mod model;
pub mod process_mapping;
pub mod repo;
pub mod service;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}