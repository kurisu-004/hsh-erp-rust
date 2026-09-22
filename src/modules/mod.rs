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
pub mod assembly;
pub mod cnc_program;
// 2026-09-19 新增 com 模块聚合：customer + applicant 平移至 `com::customer` / `com::applicant`，
// URL 迁移到 `/api/v2/com/*`（见本文件 `v2_router` 与 `com/mod.rs`）。
pub mod com;
pub mod dashboard;
pub mod delivery_note;
// 2026-09-19 IAM 域合并（PR-1）：合并 `auth` + `user` 为单一 `iam` 业务域；
// handler 内 14 端点 + 1 个 router 工厂函数 `router()`。auth / user 目录已删除。
// 2026-09-19 IAM 域收尾（PR-4）：旧 alias `/auth` + `/users` nest 已下线，
// `/api/v2/iam/*` 成为 IAM 域唯一对外接口。
pub mod iam;
pub mod outsource;
pub mod part;
// 2026-09-22 PR2 合并：原 `part_batch` 域（1866 行 helper，无独立 URL）
// 物理合并入 part 域的 `part::batch` 子模块（src/modules/part/batch/）。
// 这里不再 `pub mod part_batch;`，所有引用改走 `crate::modules::part::batch::*`。
pub mod part_file;
// 2026-09-19 新增 prod 模块聚合：worker + work_type + process + process_chain +
// worker_pool 平移至 `prod::*`，URL 硬切换到 `/api/v2/prod/*`（无 alias，前端锁步）。
// part / assembly 是核心实体未移入；报工端点保留在 part 域。
pub mod prod;
pub mod shelf;
pub mod statistics;
pub mod upload_session; // 2026-09-18 新增：Redis 共享 STS 凭证会话机制

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
/// 2026-09-19 IAM 域收尾（PR-4）：旧 alias `/auth` + `/users` nest 已删除，
/// `/api/v2/iam/*` 成为 IAM 域唯一对外接口。
///
/// 2026-09-19 com 模块聚合：customer + applicant 已平移至 `com` 子模块，
/// nest 路径由 `/customers` + `/applicants` 迁至 `/com/customers` + `/com/applicants`。
///
/// 2026-09-19 prod 模块聚合：worker + work_type + process + process_chain +
/// worker_pool 5 支撑域平移至 `prod`，URL 硬切换到 `/api/v2/prod/*`。
/// 旧 `/workers` + `/work-types` + `/processes` + `/process-chains` + `/worker-pool` +
/// `/admin/worker-pool` 6 个 nest 同步下线，无 alias（前端配套 PR 锁步）。
///
/// 2026-09-20 新增：签名收 `Arc<AppState>`，在 `route_layer` 上挂 `authenticate_middleware`
/// —— Bearer JWT 验签 + Redis session 校验 + 滑动 TTL 集中处理；公开路径
/// （health / login / refresh / `_e2e`）在 middleware 内部白名单放行。
/// `route_layer` 仅作用于已匹配路由，404 不会被强制鉴权（与现状一致）；
/// 边界由 `tests/auth_middleware.rs::nonexistent_route_returns_404_not_40100`
/// 守住不变量——**绝对不能**换成 `.layer()`，否则 404 路径会先过 middleware
/// 拿 40100，掩盖真实路由错误。
/// 需要 state：axum 0.8 的 `from_fn` 不支持 `State` 提取，必须用
/// `from_fn_with_state(state.clone(), ...)`，因此 v2_router 收 state。
pub fn v2_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        // 2026-09-19 IAM 域：新路径 `/iam` 14 端点（PR-1 起开放，PR-4 收尾后唯一）
        .nest("/iam", iam::router())
        // 2026-09-19 com 聚合：customer + applicant 统一挂在 `/com/*` 下
        .nest("/com", com::router())
        // 2026-09-19 prod 聚合：工人 / 工种 / 工序 / 工艺链 / 工人池 5 支撑域统一挂在 `/prod/*` 下
        .nest("/prod", prod::router())
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
        // 2026-09-14 新增：e2e 测试 seed hook（dev/test 默认启用，release profile 硬关）
        .nest("/_e2e", _e2e::router())
        // 2026-09-20 新增：JWT 验证统一走中间件（详见 auth::middleware）
        // 2026-09-22 重构：`auth_middleware` → `authenticate_middleware`（全词化）。
        //
        // 2026-09-23 review #1 修复：route_layer 调用顺序语义是「后调 = 外层 =
        // 请求先经过」。想要 auth 先跑 → auth 必须「后调 = 最后写」。现顺序：
        // 1. idempotency_middleware 先调 = 内层 = handler 之前最后跑（命中即返）
        // 2. authenticate_middleware 后调 = 外层 = handler 之前最先跑（先鉴权）
        // 请求流：auth → idempotency → handler；鉴权失败的 401 不会被 idem 缓存，
        // 公开路径（login / refresh / health / _e2e）也在 auth 白名单直接放行，
        // idem 内置 public path 闸门是双保险（即使顺序错也不缓存登录 JWT）。
        // 顺序不可换：若 auth 在内层，idem 在外层，则 A 带 key K POST 的响应会
        // 被 idem 缓存，B 用同 key POST 命中缓存直接拿到 A 的响应 → 跨用户数据
        // 泄漏 + 公开路径 JWT 缓存劫持 session。
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::idempotency::idempotency_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            crate::auth::middleware::authenticate_middleware,
        ))
}

/// `/ws/*` WebSocket 入口（当前仅 dashboard 大屏）
pub fn ws_router() -> Router<Arc<AppState>> {
    dashboard::router()
}

/// P1 送货分组 router re-export（供 `/api/v2/delivery-groups` nest 使用）
pub fn p1_router() -> Router<Arc<AppState>> {
    delivery_note::handler::p1_router()
}
