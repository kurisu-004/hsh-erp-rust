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
//!
//! ## 路由归属
//! - 域本体 10 条（CRUD / 状态机 / 文件 / 子件）挂 `/api/v2/assemblies/*`
//!   （`modules::v2_router` 的顶层 nest，见本文件 [`router`]）。
//! - 2026-10-11 新增 1 条生产链路逃生端点
//!   `POST /api/v2/prod/assemblies/{assembly_id}/force-complete` 挂 `/api/v2/prod/*`
//!   下（见 [`force_complete_router`]，由 `prod::router()` nest）。前缀并存是刻意的：
//!   域本体不进 `/prod`，只有这条收口端点归生产链路。
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

/// 装配件域的**生产链路逃生端点**子路由（不含公共前缀；由 `prod::router()` 桥接到
/// `/prod/assemblies`）。
///
/// 2026-10-11 新增，当前只含 1 条：
/// - `POST /{assembly_id}/force-complete` —— MANAGER 单角色强推装配件 + 全部子件 +
///   全部非 CANCELLED 批次为 `COMPLETED`（绕状态机）。
///
/// 为什么单开一个工厂而不并进上面的 [`router`]：装配件域**本体**是核心实体，路由仍
/// 挂在 `/api/v2/assemblies/*`（`modules::v2_router` 的顶层 nest，10 条一行未动）；
/// 只有这条生产链路收口的逃生端点挂在 `/api/v2/prod/assemblies/*` 下 —— 与批次 /
/// 队列动作同属「生产执行」语义。两个前缀并存是**刻意**的分歧，不是遗漏。
pub fn force_complete_router() -> Router<Arc<AppState>> {
    handler::force_complete_router()
}
