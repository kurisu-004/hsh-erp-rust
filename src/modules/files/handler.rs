//! files 域 HTTP handler（路由表 + STS 转发端点）
//!
//! ## 路由表（2026-10-13 起本文件是 files 域的**唯一**路由声明处）
//!
//! ```text
//! /sts-tmp-keys   → forward_sts_tmp_keys      （2026-09-28 起，逐字未变）
//! /objects/*      → part_file::router()       （嵌套子模块 part_file）
//! /cnc/*          → part_file::cnc::router()  （嵌套子模块 part_file::cnc）
//! ```
//!
//! 完整路径（叠加 `modules::v2_router()` 的 `.nest("/files", …)`）：
//! `/api/v2/files/sts-tmp-keys` / `/api/v2/files/objects/*` / `/api/v2/files/cnc/*`。
//!
//! ⚠️ **为什么 nest 段写 `/objects` 而不是模块名 `part_file`**：URL 对外语义是
//! 「文件对象」而非「零件文件」—— `t_part_file` 的 owner 本来就是多态的
//! （`owner_kind ∈ {PART, ASSEMBLY}`），装配件文件也走这里。模块归 `part_file`
//! （它 own 这张表）、URL 说对象，两者有意不同。
//!
//! ⚠️ **matchit 注册顺序**：1 段静态 `/sts-tmp-keys` vs 2~3 段的两个 nest，
//! 段数不同天然无冲突，**不需要**「静态段先于 `Path<i64>` catch-all」的处理。
//!
//! ## 端点 `POST /api/v2/files/sts-tmp-keys`
//!
//! 2026-09-28 新增：强制 JWT 鉴权 + Role 检查 + 透明转发到 python
//! `/api/v1/files/sts-tmp-keys`。
//!
//! 2026-09-29 追加：rust 端在转发前注入 `X-Forwarded-User-Id: <CurrentUser.id>`
//! 头（clone 原 HeaderMap 后再插入，不污染原 extractor 形参）。python 端 STS
//! service 信任此 header 取得真实 user_id（替代此前「依赖 rust JWT 二次验签」的
//! 反向泄露风险）。
//!
//! 详见 [`mod.rs`](mod.rs) 模块 doc。
//!
//! ## 事务边界
//! 纯转发，无 DB 操作 —— `handler` 不开事务（与「非持久化写」端点同形）。
//!
//! ## 约束
//! - **强制鉴权**：走 `v2_router` 全局 `authenticate_middleware`，未带 token
//!   → 40100 UNAUTHORIZED（与其它受保护端点同形）。
//! - **Role 检查**：`Manager / Clerk / CncProgrammer / Inspector` 4 角色之一；
//!   其余角色（含未登录 SHELF_ACCOUNT）→ 40300 FORBIDDEN。
//! - **透传语义**：body（`Json<Value>` 原样转发）+ 响应（status/headers/body
//!   三元组原样拼装 Response），python 信封 `{code, message, data}` 与 rust 信封
//!   同形，前端 `envelopeResponseInterceptor` 自动解。
//! - **身份注入**：rust 端把 `CurrentUser.id`（已 JWT 验签 + Redis session 校验）
//!   以 `X-Forwarded-User-Id` 头透传给 python；python 端 STS service 直接信任。
//!   `filter_request_headers` 的 SKIP 列表不含 `x-forwarded-user-id`，不会被
//!   `HttpPyBackend` 过滤掉。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::files::part_file;
use crate::shared::error::AppError;
use crate::state::AppState;

/// files 域路由表。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/sts-tmp-keys", post(forward_sts_tmp_keys))
        .nest("/objects", part_file::router())
        .nest("/cnc", part_file::cnc::router())
}

/// `POST /api/v2/files/sts-tmp-keys` —— 鉴权 + 转发
///
/// 1. middleware 强制 JWT 鉴权（未带 token → 40100）；
/// 2. `require_any_role` 限制 4 角色（其它 → 40300）；
/// 3. 鉴权通过后，clone `headers` 后插入 `X-Forwarded-User-Id: current.id`，
///    再调 `state.py_backend.forward_sts_tmp_keys(body, headers)` 透明转发到
///    python；返回 `(status, headers, body)` 三元组原样拼 Response。
///
/// ## 2026-09-29 新增：身份透传 header
/// 原 `headers` 形参由 axum extractor 提供，**不可**直接 mutate（会污染
/// 其它 extractor / middleware）。故先 `HeaderMap::clone()` 得一份副本，
/// 副本上插入 `X-Forwarded-User-Id`，再交付给 py_backend。`HttpPyBackend`
/// 内的 `filter_request_headers` SKIP 列表不含此 header（见
/// [`crate::infra::py_backend`]），不会被二次过滤。
pub async fn forward_sts_tmp_keys(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    current.require_any_role(&[
        Role::Manager,
        Role::Clerk,
        Role::CncProgrammer,
        Role::Inspector,
    ])?;
    // 2026-09-29 新增：clone 一份 HeaderMap 后注入 X-Forwarded-User-Id；
    // 不直接 mutate 原 headers 形参（axum extractor 多次复用同一 HeaderMap）。
    let mut fwd_headers = headers.clone();
    if let Ok(value) = current.id.to_string().parse() {
        fwd_headers.insert("x-forwarded-user-id", value);
    }
    let resp = state
        .py_backend
        .forward_sts_tmp_keys(body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}
