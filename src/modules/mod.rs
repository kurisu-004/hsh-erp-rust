//! 业务域聚合：按 REST/mcp/ws 三种入口组装 Router
//!
//! 重构版业务 REST 接口统一挂在 `/api/v2`；AI 只读入口 `/api/mcp` 保持非版本化；
//! WebSocket 大屏挂在 `/ws/dashboard`。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

pub mod _e2e;
pub mod applicant;
pub mod assembly;
pub mod cnc_program;
pub mod customer;
pub mod dashboard;
pub mod delivery_note;
// 2026-09-19 IAM 域合并（PR-1）：合并 `auth` + `user` 为单一 `iam` 业务域；
// handler 内 14 端点 + 3 router 工厂函数（`router` 新 + `auth_router` + `users_router` 旧 alias）。
// auth / user 目录已删除。
pub mod iam;
pub mod outsource;
pub mod part;
pub mod part_batch;
pub mod part_file;
pub mod process;
pub mod process_chain;
pub mod shelf;
pub mod statistics;
pub mod upload_session; // 2026-09-18 新增：Redis 共享 STS 凭证会话机制
pub mod work_type;
pub mod worker;
pub mod worker_pool;

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        service: "hsh-erp-api",
        version: "v2",
    })
}

/// `/api/v2/*` 业务路由聚合（重构版统一版本前缀）
///
/// 2026-09-19 IAM 域合并（PR-1）：新增 `/iam` 新路径；保留 `/auth` + `/users` 旧 alias
/// 路由至 PR-4（兼容期，便于前端 / 第三方客户端迁移）。三路由全部指向
/// `iam::handler::{router, auth_router, users_router}`。
pub fn v2_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        // 2026-09-19 IAM 域合并：原 auth::router() / user::router() 指向新的 iam 域
        .nest("/auth", iam::auth_router())
        .nest("/users", iam::users_router())
        // 2026-09-19 IAM 域合并：新路径 `/iam` 14 端点
        .nest("/iam", iam::router())
        .nest("/customers", customer::router())
        .nest("/applicants", applicant::router())
        .nest("/workers", worker::router())
        .nest("/work-types", work_type::router())
        .nest("/processes", process::router())
        .nest("/shelves", shelf::router())
        .nest("/parts", part::router())
        .nest("/assemblies", assembly::router())
        .nest("/cnc-programs", cnc_program::router())
        .nest("/part-files", part_file::router())
        // 2026-09-18 新增：上传会话域（7 个 POST 端点，挂在 /api/v2/upload-sessions）
        .nest("/upload-sessions", upload_session::router())
        .nest("/outsource-companies", outsource::company_router())
        .nest("/outsource-quotes", outsource::quote_router())
        .nest("/outsource-shipments", outsource::shipment_router())
        .nest("/delivery-notes", delivery_note::router())
        .nest("/delivery-groups", p1_router())
        .nest("/statistics", statistics::router())
        .nest("/worker-pool", worker_pool::router())
        .nest("/admin/worker-pool", worker_pool::admin_router())
        .nest("/process-chains", process_chain::router())
        // 2026-09-14 新增：e2e 测试 seed hook（dev/test 默认启用，release profile 硬关）
        .nest("/_e2e", _e2e::router())
}

/// `/ws/*` WebSocket 入口（当前仅 dashboard 大屏）
pub fn ws_router() -> Router<Arc<AppState>> {
    dashboard::router()
}

/// P1 送货分组 router re-export（供 `/api/v2/delivery-groups` nest 使用）
pub fn p1_router() -> Router<Arc<AppState>> {
    delivery_note::handler::p1_router()
}
