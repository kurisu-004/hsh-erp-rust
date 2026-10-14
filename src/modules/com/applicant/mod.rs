//! applicant 域
//!
//! 对应 Python myERP/api/v1/applicant.py。
//! 路由前缀 `/api/v2/com/applicants`。本域无状态机。
//!
//! **端点 6 条**（2026-10-17：5 → 6，新增单条幂等 `POST /com/applicants/get-or-create`）：
//! 读 2（list / get）+ 写 4（create / get-or-create / update / soft-delete），
//! 逐条清单见 `handler.rs` 顶部 doc 与 `handler::router()`。
//!
//! 与 Python 侧的两处**有意差异**（都写在这里，别当缺口）：
//! - `/search` **未移植**（无消费方）；
//! - `/bulk-get-or-create` **不做**（批量形态无人调用）—— 用单条
//!   `/get-or-create` 覆盖新增订单页自动登记申请人的场景。
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
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
