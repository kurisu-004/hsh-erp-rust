//! files 域 HTTP handler
//!
//! 2026-09-28 新增：`POST /api/v2/files/sts-tmp-keys` —— 强制 JWT 鉴权 +
//! Role 检查 + 透明转发到 python `/api/v1/files/sts-tmp-keys`。
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
/// 3. 鉴权通过后调 `state.py_backend.forward_sts_tmp_keys(body, headers)`
///    透明转发到 python；返回 `(status, headers, body)` 三元组原样拼 Response。
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
    let resp = state.py_backend.forward_sts_tmp_keys(body, headers).await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}