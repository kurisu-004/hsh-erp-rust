//! customer 域
//!
//! 对应 Python myERP：
//! - api/v1/customer.py
//! - service/customer_service.py
//! - repository/customer_repository.py
//! - model/customer.py
//! - schema/customer.py
//!
//! 域内含 L1（一级集团，parent_id IS NULL，带 serial_prefix 单大写字母）与
//! L2（叶子客户，parent_id 非 NULL）两层结构。CRUD 端点挂在 `/api/v2/customers`。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}