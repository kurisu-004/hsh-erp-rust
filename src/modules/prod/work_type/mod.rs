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
//! 2026-09-22 PR6：原 `process_mapping/{mod.rs, sql.rs}` 子目录已合并入本域
//! `service.rs`（`WorkTypeProcessRepo` + `WorkTypeProcessService`）；本目录不再
//! 持有 `process_mapping` 子模块，service.rs 仍控制在 1000 行内（合并后约 660 行）。
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
