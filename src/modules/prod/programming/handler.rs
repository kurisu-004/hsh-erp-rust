//! prod::programming 子模块 handler 层 —— HTTP 路由
//!
//! 2026-10-01 新增：单只读端点。
//!
//! ## 端点
//! - `GET /api/v2/prod/programming/pending` —— Manager+Clerk+Inspector+CncProgrammer
//!
//! ## 事务边界
//! 读端点：`pool.acquire()` 不开事务，**不发** WS 广播（纯筛选列表，无业务流转）。
//!
//! ## 角色守卫
//! 在 service 第一行下沉（沿 `prod::batch` 范本），handler 不重复校验。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::ProgrammingListQuery;
use super::service::ProgrammingService;
use super::vo::ProgrammingListOut;

/// `GET /api/v2/prod/programming/pending`
///
/// 角色：Manager + Clerk + Inspector + CncProgrammer（service 内守卫）。
/// 读端点：`pool.acquire()` 不开事务。
pub async fn list_pending(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<ProgrammingListQuery>,
) -> Result<Json<R<ProgrammingListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ProgrammingService::list_pending(&mut conn, &current, q).await?;
    Ok(Json(R::ok(out)))
}
