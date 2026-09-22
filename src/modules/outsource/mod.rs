//! outsource 域（Phase 2 2026-09-13）
//!
//! 对应 Python myERP：
//! - api/v1/outsource_*.py
//! - service/outsource_*.py
//! - repository/outsource_*.py
//! - model/outsource.py
//! - schema/outsource.py
//! - statemachines/outsource_quote.py
//!
//! 2026-09-22 refactor（对齐 iam 事务分层范式）：
//! - `repo.rs` 拆为 `repo/{mod, sql}.rs`：胖 trait `OutsourceRepoTrait` + 3 ZST struct
//!   （`OutsourceCompanyRepo` / `OutsourceQuoteRepo` / `OutsourceShipmentRepo`）
//!   + 4 `NewXxx` insert builder；trait 直接 `impl for &mut PgConnection`。
//! - `service.rs`（1273 行超 1000 行上限）拆为 `service/{mod, company, quote, shipment}.rs`：
//!   `OutsourceService` 字段仅 `Arc<SnowflakeIdGenerator>`，impl 块分布在 4 子模块。
//! - `handler.rs` 17 端点保持 3 router 工厂（company_router / quote_router /
//!   shipment_router）；handler 三形态严格区分（读走 `pool.acquire()`，
//!   写走 `pool.begin() + tx.commit()`）。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

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
