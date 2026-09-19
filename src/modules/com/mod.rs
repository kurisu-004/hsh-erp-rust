//! com 域（Customer Order Management）
//!
//! 2026-09-19 新增 com 模块聚合：把 customer + applicant 两个域代码组织上归位到 com 下，
//! URL 一并迁移到 `/api/v2/com/*`。order 域后续再讨论，本任务不涉及。
//!
//! 路由风格保持不变：customer/applicant 各自定义 5 个标准 CRUD 端点。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod applicant;
pub mod customer;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/customers", customer::router())
        .nest("/applicants", applicant::router())
}