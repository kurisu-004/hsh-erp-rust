//! files 域 HTTP handler
//!
//! 2026-09-28 新增：`POST /api/v2/files/sts-tmp-keys` —— 强制 JWT 鉴权 +
//! Role 检查 + 透明转发到 python `/api/v1/files/sts-tmp-keys`。
//!
//! 2026-09-29 追加：rust 端在转发前注入 `X-Forwarded-User-Id: <CurrentUser.id>`
//! 头（clone 原 HeaderMap 后再插入，不污染原 extractor 形参）。python 端 STS
//! service 信任此 header 取得真实 user_id（替代此前「依赖 rust JWT 二
//! 次验签」的反向泄露风险）。
//!
//! 详见 [`mod.rs`](mod.rs) 模块 doc 与 plan `/Users/ren/.claude/plans/sts-session-uploader-sts-sts-sequential-globe.md`。
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
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::auth::rbac::{CurrentUser, Role};
use crate::shared::error::AppError;
use crate::state::AppState;

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
    let resp = state.py_backend.forward_sts_tmp_keys(body, fwd_headers).await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}
