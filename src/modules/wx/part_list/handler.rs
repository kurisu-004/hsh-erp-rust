//! wx::part_list 子模块 handler 层 —— HTTP 路由
//!
//! 2026-10-11 新增。**两个**只读端点。
//!
//! ## 路由表（⚠️ matchit 注册顺序是硬约束）
//! ```text
//! .route("/page", get(page))   // 静态段，必须**先**注册
//! .route("/",     get(home))   // 兜底，**后**注册
//! ```
//! 静态段 `/page` 若晚于 `/` 注册，会被兜底路由抢走（matchit 先匹配先注册者）。
//! 参考仓库既有写法：`iam::shelf::handler::router` 同样用 `.route("/", …)` +
//! `.route("/{id}", …)`。
//!
//! ## 端点
//! - `GET /api/v2/wx/part-list` —— 首屏聚合（`counts` + `list` + `hasMore`）
//! - `GET /api/v2/wx/part-list/page` —— 上拉增量（`list` + `hasMore`）
//!
//! ## Query 参数（两个端点**完全共用** `dto::PartListQuery`）
//! | 参数 | 缺省 | 非法值行为 |
//! |---|---|---|
//! | `date` | 不加日期谓词（全部 6 状态 × 任意日期） | chrono 解析不出日期 ⇒ **HTTP 400 纯文本**（**不走** `R<T>`；⚠️ `2026-8-4` 这种不零填充形态 chrono **是接受的**） |
//! | `status` | `all`（6 状态白名单） | 白名单外 ⇒ **40001 / HTTP 422**（`R<T>`） |
//! | `page` | 1（`max(1)`） | `?page=abc` ⇒ **HTTP 400 纯文本**（axum `Query` 提取器） |
//! | `size` | 10（`clamp(1, 50)`） | 同上 |
//!
//! ⚠️ **两种 400/422 的分界**在 `date` / `page` / `size`（提取器层 400 纯文本）
//! 与 `status`（service 层 40001 / 422）之间 —— 契约见 `docs/api/wx.md` §4。
//!
//! ## 事务边界
//! 纯读端点：`pool.acquire()` 不开事务（不开 tx、不发 WS 广播），与
//! `prod::process_design` 范式一致。
//!
//! ## ⚠️ 无尾斜杠
//! `/api/v2/wx/part-list`（无尾斜杠）命中；`/api/v2/wx/part-list/` 不命中。
//! 实测形态见 `docs/api/wx.md` §5 与
//! `tests/wx/part_list.rs::trailing_slash_form_is_pinned`。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::PartListQuery;
use super::service::PartListService;
use super::vo::{PartListHomeOut, PartListPageOut};

/// `/api/v2/wx/part-list/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ⚠️ 顺序硬约束：`/page`（静态）必须先于 `/`（兜底）注册
        .route("/page", get(page))
        .route("/", get(home))
}

/// `GET /api/v2/wx/part-list` —— 首屏聚合。
///
/// 角色：登录即可（**无角色闸门**；小程序不做货架/工种隔离）。读端点：
/// `pool.acquire()` 不开事务。
pub async fn home(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<PartListQuery>,
) -> Result<Json<R<PartListHomeOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartListService::home(&mut conn, &q).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/wx/part-list/page` —— 上拉增量（只返 `list` + `hasMore`）。
///
/// 与 [`home`] 共用 service 的同一个列表查询，故同一 `?page=` 下 `list` 逐字相同。
pub async fn page(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
    Query(q): Query<PartListQuery>,
) -> Result<Json<R<PartListPageOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = PartListService::page(&mut conn, &q).await?;
    Ok(Json(R::ok(out)))
}
