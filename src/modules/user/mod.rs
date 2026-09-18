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
// 2026-09-18 Wave 1A/T2：UoW（repo 访问器模式）— 4 repo trait + UnitOfWork + UowProvider +
// 4 Sqlx*Repo + SqlxUnitOfWork + SqlxUowProvider + 手写 test_support。
pub mod uow;

// 2026-09-18 Wave 2 T9：66 例 mock 单测。单独文件以避开 service.rs 1000 行上限。
// 必须挂在这里（不能用 `#[path]` 挂在 service.rs 里）—— service_tests.rs 内部用
// `super::dto/model/repo/service/uow` 引用同级模块，只有在 user/ 下 super 才指向 user/。
#[cfg(test)]
mod service_tests;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
