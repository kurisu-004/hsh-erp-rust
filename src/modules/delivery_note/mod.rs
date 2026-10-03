//! delivery_note 域
//!
//! 对应 Python myERP：
//! - api/v1/delivery_note.py        → `handler.rs`
//! - service/delivery_note.py       → `service.rs`
//! - repository/delivery_note.py    → `repo.rs`
//! - model/delivery_note.py         → `model.rs`
//! - schema/delivery_note.py        → `dto.rs`
//! - statemachines/delivery_note.py → `statemachine.rs`
//! - service/delivery_note_print.py → 不落地 rust 实现；打印端点在 rust 侧只做
//!   鉴权 + RBAC + 转发，模板渲染由 python 执行（2026-10-03 BFF 转发）
//!
//! Phase P1 实装「送货分组」；P2 送货单生命周期；P3 扫码入单；P4 打印端点转发。
//!
//! 路由挂载：`handler::router()` 暴露 `/delivery-groups/*` 与 `/delivery-notes/*`。
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
