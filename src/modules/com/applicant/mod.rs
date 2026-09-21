//! applicant 域
//!
//! 对应 Python myERP/api/v1/applicant.py。
//! 路由前缀 `/api/v2/com/applicants`。本域无状态机。
//!
//! 2026-09-22 重构对齐 iam 范本：`repo.rs` → `repo/{mod, sql}.rs`（胖 trait
//! `ApplicantRepo` + `impl for &mut PgConnection`），`service.rs` →
//! `service/{mod, crud}.rs`（`ApplicantService` 仅持 `Arc<SnowflakeIdGenerator>`）。
//! 跨域 inline t_customer SQL（批量 `lookup_names`）收敛到 `CustomerRepo::lookup_names`；
//! service 收两个 trait 形参 `<R: ApplicantRepo, R3: CustomerRepo>`，handler 借
//! `&mut *tx` 喂两次 reborrow（见 handler.rs）。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
