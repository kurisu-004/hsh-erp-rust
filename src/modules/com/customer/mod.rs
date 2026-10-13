//! customer 域
//!
//! 对应 Python myERP：
//! - api/v1/customer.py
//! - service/customer_service.py
//! - repository/customer_repository.py
//! - model/customer.py
//! - schema/customer.py
//!
//! 域内是**任意深度**的客户树（2026-10-17 由固定两层放开，对齐部门架构：
//! 集团 / 分厂 / 车间 / 班组…）：根节点（`parent_id IS NULL`，带 `serial_prefix`
//! 单大写字母）+ 任意层级的非根节点（`parent_id` 非 NULL）。
//! 跨域的客户树遍历设施在 `shared::customer`（子树展开 / 上溯根 / 路径批量取）。
//! CRUD 端点挂在 `/api/v2/com/customers`。
//!
//! 2026-09-22 重构对齐 iam 范本：`repo.rs` → `repo/{mod, sql}.rs`（胖 trait
//! `CustomerRepo` + `impl for &mut PgConnection`），`service.rs` →
//! `service/{mod, crud}.rs`（`CustomerService` 仅持 `Arc<SnowflakeIdGenerator>`）。
//! 跨域 inline SQL（`t_part` / `t_assembly` 引用计数 + applicant 用的 `lookup_names`）
//! 收敛到 `CustomerRepo::count_*_using_customer` / `lookup_names` 三个 helper。

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
