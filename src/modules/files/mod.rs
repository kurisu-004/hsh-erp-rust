//! files 域（BFF 聚合：转发 STS 凭证签发到 python 后端）
//!
//! 2026-09-28 新增：薄壳鉴权转发端点 `POST /api/v2/files/sts-tmp-keys`。
//!
//! ## 背景
//! 上轮"移除 STS session 设施"完成后，前端 `grantStsTmpKey` / `grantStsTmpKeyFiles`
//! 直接打到 python `POST /api/v1/files/sts-tmp-keys` → nginx → python `backend:8000`，
//! **未经过 backend-rust 鉴权**。原因：删 `infra::python_sts::HttpPythonSts` 时
//! 连带把"rust 鉴权后再转发"的薄壳端点也删了，但没补回。
//!
//! ## 修复
//! 本模块挂 `POST /api/v2/files/sts-tmp-keys`：
//! - `CurrentUser` extractor + `require_any_role([Manager/Clerk/CncProgrammer/Inspector])`
//!   强制 JWT 鉴权；
//! - 鉴权通过后透明转发 body 到 python `/api/v1/files/sts-tmp-keys`；
//! - 把 python 响应（status + headers + body）原样透传回前端；
//! - python 端**继续裸开** by design + 部署层隔离（运维层加固后续单独 PR）。

mod handler;

use std::sync::Arc;

use axum::{Router, routing::post};

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/sts-tmp-keys", post(handler::forward_sts_tmp_keys))
}