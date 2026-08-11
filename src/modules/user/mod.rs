//! user 域
//!
//! 对应 Python myERP：
//! - api/v1/user.py       → `handler.rs`
//! - service/user.py      → `service.rs`（菜单树来自 service/menu.py）
//! - repository/user.py   → `repo.rs`
//! - model/user.py        → `model.rs`
//! - schema/user.py       → `dto.rs`
//!
//! 路由挂载点 `/api/v2/users`（见 `crate::modules::v2_router`）。
//! 全部端点要求 MANAGER 角色，权限守卫在 service 层（见 `handler.rs` 头注释）。

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
