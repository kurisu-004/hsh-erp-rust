//! assembly 域
//!
//! 对应 Python myERP：
//! - api/v1/assembly.py
//! - service/assembly_service.py
//! - repository/assembly_repository.py
//! - model/assembly.py
//! - schema/assembly.py
//!
//! 2026-09-22 Group D-3 重构（对齐 iam 事务分层范式）：
//! - 原 `repo.rs` 拆为 `repo/{mod, sql}.rs`：sql.rs 是 SQL 真源（11 个静态方法 + ZST
//!   `AssemblyRepo`，零 diff），mod.rs 新增胖 trait `AssemblyRepoTrait`（11 方法）并
//!   直接 `impl for &mut PgConnection`（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//! - 原 `service.rs`（1088 行超 1000 行上限）拆为 `service/{mod, crud, lifecycle,
//!   sync_from_part}.rs` 4 子模块。
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo; // 2026-09-22 PR4：响应 VO（仅 Serialize）从 dto/ 抽出到此目录

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
