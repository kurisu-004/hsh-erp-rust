//! cnc_program 域
//!
//! 对应 Python myERP：
//! - api/v1/cnc_program.py
//! - service/cnc_program_service.py
//! - repository/cnc_program_repository.py
//!
//! 2026-09-14 Phase 3：补齐 cnc-pair 配对上传 + 列表端点。
//! 存储复用 part_file（kind='G_CODE' + kind='SETUP_SHEET'，paired_file_id 互指）。
//!
//! 2026-09-22 对齐 iam 范式：
//! - `repo.rs` → `repo/{mod, sql}.rs`：ZST `CncProgramRepo` + 胖 trait `CncProgramRepoTrait`。
//!   `CncProgramRepo` 静态方法签名零 diff（`list_pairs_for_part`）；`CncProgramRepoTrait`
//!   含 6 方法（list_pairs_for_part 1 + 跨域 helper 5），对 `&mut PgConnection` 直接实现。
//! - `service.rs`：`CncProgramService` 成为带字段结构（`Arc<SnowflakeIdGenerator>` +
//!   `Arc<dyn CosClient>`），方法签名 `<R: CncProgramRepoTrait>(&self, mut repo: R, ...)`
//!   by-value；handler 借 `&mut *tx` 喂给 trait。3 个 alias 端点（download-url / content
//!   / delete）由 handler 直接转发到 `state.part_file_service`，不在本 service 上挂 alias。
pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}