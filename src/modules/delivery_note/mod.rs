//! delivery_note 域
//!
//! 对应 Python myERP：
//! - api/v1/delivery_note.py        → `handler.rs`
//! - service/delivery_note.py       → `service.rs`
//! - repository/delivery_note.py    → `repo.rs`
//! - model/delivery_note.py         → `model.rs`
//! - schema/delivery_note.py        → `dto.rs`
//! - statemachines/delivery_note.py → `statemachine.rs`
//! - service/delivery_note_print.py → `print.rs`（P4 新增；umya-spreadsheet 实现）
//! - patch helper                   → `print_xml_patch.rs`（P4 新增；绕 umya 缺口）
//!
//! Phase P1 实装「送货分组」；P2 送货单生命周期；P3 扫码入单；P4 打印移植。
//!
//! 路由挂载：`handler::router()` 暴露 `/delivery-groups/*` 与 `/delivery-notes/*`。
////
pub mod dto;
pub mod handler;
pub mod model;
pub mod print;
pub mod print_xml_patch;
pub mod repo;
pub mod service;
pub mod statemachine;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}