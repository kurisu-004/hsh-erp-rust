//! outsource 域（Phase 2 2026-09-13）
//!
//! 对应 Python myERP：
//! - api/v1/outsource_*.py
//! - service/outsource_*.py
//! - repository/outsource_*.py
//! - model/outsource.py
//! - schema/outsource.py
//! - statemachines/outsource_quote.py

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

/// 公司域路由（挂载点 `/outsource-companies`，见 `modules::v2_router`）。
pub fn company_router() -> Router<Arc<AppState>> {
    handler::company_router()
}

/// 报价域路由（挂载点 `/outsource-quotes`）。
pub fn quote_router() -> Router<Arc<AppState>> {
    handler::quote_router()
}

/// 发货记录域路由（挂载点 `/outsource-shipments`）。
pub fn shipment_router() -> Router<Arc<AppState>> {
    handler::shipment_router()
}
