//! wx::production 子模块 handler 层 —— HTTP 路由
//!
//! 2026-10-11 新增。**两个**只读端点（替代旧的 `batches.rs` + `worker.rs` 三个）。
//!
//! ## 路由表（⚠️ matchit 注册顺序是硬约束）
//! ```text
//! .route("/page", get(page))   // 静态段，必须**先**注册
//! .route("/",     get(home))   // 兜底，**后**注册
//! ```
//! 静态段 `/page` 若晚于 `/` 注册，会被兜底路由抢走（matchit 先匹配先注册者）。
//! 与 `wx::part_list::handler` 同构。
//!
//! ## 端点
//! - `GET /api/v2/wx/production` —— 首屏聚合（`worker` + `stats` + `counts` +
//!   `list` + `hasMore`）
//! - `GET /api/v2/wx/production/page` —— 上拉增量（`list` + `hasMore`）
//!
//! ## 事务边界
//! 纯读端点：`pool.acquire()` 不开事务（不开 tx、不发 WS 广播），与
//! `wx::part_list` / `prod::process_design` 范式一致。
//!
//! ## ⚠️ 无尾斜杠
//! `/api/v2/wx/production`（无尾斜杠）命中；`/api/v2/wx/production/` **不命中**，
//! 落到 axum 默认 fallback ⇒ **404 空 body**（不走 `R<T>` 信封）。实测形态登记在
//! `docs/api/wx.md` §5.1，回归 `tests/wx/production.rs::trailing_slash_form_is_pinned`。
//!
//! ⚠️ 小程序侧**旧**代码发的是 `/wx/batches/`（带尾斜杠，见
//! `wx-app/miniprogram/services/production.ts` 的 2026-09-28 注释），本次硬切到
//! `/wx/production` 后**必须同步改前端**，且**不带**尾斜杠。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::ProductionQuery;
use super::service::ProductionService;
use super::vo::{ProductionHomeOut, ProductionPageOut};

/// `/api/v2/wx/production/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ⚠️ 顺序硬约束：`/page`（静态）必须先于 `/`（兜底）注册
        .route("/page", get(page))
        .route("/", get(home))
}

/// `GET /api/v2/wx/production` —— 首屏聚合。
///
/// 角色：登录即可（**无角色闸门**；小程序不做货架/工种隔离）。读端点：
/// `pool.acquire()` 不开事务。
pub async fn home(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<ProductionQuery>,
) -> Result<Json<R<ProductionHomeOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ProductionService::home(&mut conn, current.id, &q).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/wx/production/page` —— 上拉增量（只返 `list` + `hasMore`）。
///
/// 与 [`home`] 共用 service 的同一个列表查询，故同一 `?page=` 下 `list` 逐字相同。
pub async fn page(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<ProductionQuery>,
) -> Result<Json<R<ProductionPageOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ProductionService::page(&mut conn, &q).await?;
    Ok(Json(R::ok(out)))
}
