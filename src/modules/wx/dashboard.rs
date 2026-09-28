//! 微信小程序 BFF / dashboard 域 handler（2026-09-28 新增）
//!
//! 当前唯一端点：`GET /api/v2/wx/dashboard/home` —— mini-program 首页聚合
//! （一次 HTTP 拉全部卡片 + 计数 + 当前用户视图）。
//!
//! ## 鉴权
//! - Bearer JWT + Redis session 校验：走 `v2_router` 末尾的 `authenticate_middleware`
//! - handler 用 `Extension<CurrentUser>` 占位（任意已登录；与 `dashboard::get_snapshot` 范式一致）
//!
//! ## 事务分层
//! handler 三形态 ①：`pool.acquire()` 不开事务，5 次 `repo.xxx(&mut *conn, ...)`
//! 只读聚合。read-only 端点无需 tx 边界。

use std::sync::Arc;

use axum::Json as AxumJson;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::auth::rbac::CurrentUser;
use crate::modules::iam::vo::CurrentUserOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::repo::{BatchCountsAgg, DailyEventCounts, PartCounts, map_counts_by_status};
use super::vo::HomeDashboard;

/// `/api/v2/wx/dashboard/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/home", get(home))
}

/// `GET /api/v2/wx/dashboard/home` —— mini-program 首页聚合。
///
/// 返回 6 个 BFF 维度数据：
/// 1. `me`：当前用户视图（与 `/api/v2/iam/me` 同源；本端点用 `CurrentUser`
///    内字段直填，未触发 DB 二次读——因为 `current.id` / `username` / `roles`
///    已在 JWT middleware 阶段从 Redis session 拿齐，`full_name` 因 mini-program
///    不展示故保留为 `""`，避免一次额外 DB round-trip）
/// 2. `part_counts`：工单 4 个 tab 计数（与 `/wx/parts/counts` 同 SQL）
/// 3. `batch_counts`：当月批次计数（与 `/wx/batches/counts?period=YYYY-MM` 同 SQL，
///    period 走「当前年月」——`chrono::Local::now()` 派生的 `YYYY-MM`）
/// 4. `today_picked` / `today_delivered`：今日事件计数
///
/// 错误码：DB 失败 → `AppError::Database`（50001）。
pub async fn home(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<AxumJson<R<HomeDashboard>>, AppError> {
    let mut conn = state.pool.acquire().await?;

    // 1. part_counts
    let raw_counts = PartCounts::by_status(&mut *conn).await?;
    let part_counts = map_counts_by_status(raw_counts);

    // 2. batch_counts（默认按当前年月）
    let period = chrono::Local::now().format("%Y-%m").to_string();
    let batch_counts = BatchCountsAgg::by_period(&mut conn, &period).await?;

    // 3. today_picked + today_delivered（一次 SQL）
    let (today_picked, today_delivered) = DailyEventCounts::today(&mut *conn).await?;

    // 4. me 视图（mini-program 不需要菜单树 / 货架范围——只暴露用户名 / 角色）
    let roles_str: Vec<String> = current
        .roles
        .iter()
        .map(|r| match r {
            crate::auth::rbac::Role::Manager => "MANAGER".into(),
            crate::auth::rbac::Role::Clerk => "CLERK".into(),
            crate::auth::rbac::Role::Inspector => "INSPECTOR".into(),
            crate::auth::rbac::Role::CncProgrammer => "CNC_PROGRAMMER".into(),
            crate::auth::rbac::Role::ShelfAccount => "SHELF_ACCOUNT".into(),
        })
        .collect();
    let shelf_ids_str: Vec<String> = current.shelf_ids.iter().map(|v| v.to_string()).collect();
    let me = CurrentUserOut {
        id: current.id,
        username: current.username.clone(),
        // 2026-09-28 决策：mini-program 暂不展示 full_name（首屏只显示用户名 + 角色
        // 标签）。后续如前端需展示，再开 `/iam/me` 二次拉。
        full_name: String::new(),
        is_active: true,
        roles: roles_str,
        shelf_ids: shelf_ids_str,
        menus: Vec::new(),
    };

    let resp = HomeDashboard {
        me,
        part_counts,
        batch_counts,
        today_picked,
        today_delivered,
    };
    Ok(AxumJson(R::ok(resp)))
}
