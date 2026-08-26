//! work_type 域
//!
//! 对应 Python myERP：
//! - api/v1/work_type.py
//! - service/work_type_service.py
//! - repository/work_type_repository.py
//! - model/work_type.py
//! - schema/work_type.py
//!
//! ## Phase P5 work_type CRUD：7 端点挂在 `/api/v2/work-types`
//! 读 3：list / get / list_work_type_processes
//! 写 4 (MANAGER)：create / update / soft_delete / set_work_type_processes
//!
//! ## 子模块
//! `process_mapping.rs` 单文件拆出 `t_work_type_process` 的 repo + service，
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