//! _e2e 测试 seed hook —— 2026-09-14 新增
//!
//! 提供 `/api/v2/_e2e/*` 路由供 e2e 子模块（Playwright spec）匿名灌入 seed 数据。
//!
//! ## 安全约束
//! - **完全不走 CurrentUser extractor**：handler 签名不取 `current: CurrentUser`，
//!   从而 axum 不会触发 JWT 校验，spec 端 `request.newContext({ baseURL })` 即可调。
//! - **每个 handler 开头**调用 `e2e_guard(&state)?` 二次校验：
//!   `state.config.enable_e2e_hooks == true` 才放行；否则 404（不泄漏端点存在性）。
//! - **生产硬关**：完全靠 env `E2E_HOOKS_ENABLED` 控制（单一控制点）。
//!   docker compose / dev `cargo run` 走默认（true）；prod / staging 必须显式
//!   `E2E_HOOKS_ENABLED=false`（ops 责任，不靠编译期二分）。
//!   2026-09-14 修复 Bug #1：移除 main.rs 原 release profile 二次硬关。
//!
//! ## 路由表
//! 全部挂在 `/api/v2/_e2e`（见 `modules::v2_router().nest("/_e2e", _e2e::router())`）：
//! - POST   `/probe`             探测端点存在
//! - POST   `/reset`             清所有 t_e2e_seeded 标记行（不动 alembic seed）
//! - POST   `/seed/customer`     入参 { name, parent_id?, serial_prefix? }
//! - POST   `/seed/applicant`    入参 { name, customer_id }
//! - POST   `/seed/worker`       入参 { name, work_type_code }
//! - POST   `/seed/part`         入参 { serial, customer_id, applicant_name, name?, drawing_no? }
//! - POST   `/seed/outsource_company`  入参 { name }
//! - POST   `/seed/outsource_quote`    入参 { part_id, company_id, process_id, price }
//! - POST   `/seed/delivery_note`      入参 { status?, customer_id }
//! - POST   `/seed/user`               入参 { username, role_codes: string[], phone?, full_name? }
//! - POST   `/revoke-session`          入参 { username }，删该 user 全部 Redis session
//! - DELETE `/hard-delete/outsource_company/{id}`  物理删外协公司 + 清 t_e2e_seeded 元数据
//!   （仅供 e2e 清理，不走业务软删；idempotent）

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, post};

use crate::shared::error::{AppError, code};
use crate::state::AppState;

pub mod dto;
pub mod handler;

/// 软门控：返回 Ok(()) 表示放行；返回 Err(AppError) 让 axum 走 404 / 错误响应。
pub fn e2e_guard(state: &AppState) -> Result<(), AppError> {
    if state.config.enable_e2e_hooks {
        Ok(())
    } else {
        // 用 NOT_FOUND 404 而非 403，避免泄漏端点存在性。
        Err(AppError::biz(code::NOT_FOUND, "endpoint disabled"))
    }
}

/// _e2e 路由表（挂载点 `/api/v2/_e2e`，见 `src/modules/mod.rs::v2_router()`）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/probe", post(handler::probe))
        .route("/reset", post(handler::reset))
        .route("/seed/customer", post(handler::seed_customer))
        .route("/seed/applicant", post(handler::seed_applicant))
        .route("/seed/worker", post(handler::seed_worker))
        .route("/seed/part", post(handler::seed_part))
        .route(
            "/seed/outsource_company",
            post(handler::seed_outsource_company),
        )
        .route("/seed/outsource_quote", post(handler::seed_outsource_quote))
        .route("/seed/delivery_note", post(handler::seed_delivery_note))
        .route("/seed/user", post(handler::seed_user))
        .route("/revoke-session", post(handler::revoke_session))
        // 2026-09-15 新增：物理删外协公司 + 清 t_e2e_seeded 元数据（仅供 e2e 清理）
        .route(
            "/hard-delete/outsource_company/{id}",
            delete(handler::hard_delete_outsource_company),
        )
}
