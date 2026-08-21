//! delivery_note 域
//!
//! 对应 Python myERP：
//! - api/v1/delivery_note.py        → `handler.rs`
//! - service/delivery_note.py       → `service.rs`
//! - repository/delivery_note.py    → `repo.rs`
//! - model/delivery_note.py         → `model.rs`
//! - schema/delivery_note.py        → `dto.rs`
//! - statemachines/delivery_note.py → `statemachine.rs`
//!
//! Phase P1 实装「送货分组」（model / dto / repo / service / handler / statemachine）；
//! 送货单生命周期 + 扫码入单 + 打印留到 P2–P4。
//!
//! 路由挂载：当前 `handler::router()` 仅暴露 `/delivery-groups/*` 一组端点（设计 §6.1）。
////
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}