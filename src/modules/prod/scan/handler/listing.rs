//! prod::scan 报工台两条只读聚合端点的 handler
//!
//! 两个端点都是只读：走 `pool.acquire()` **不开事务**（与 `prod::inspection` /
//! `prod::queue::board` 同款）。角色闸门在 service 层的
//! [`ScanListingService`](crate::modules::prod::scan::listing::ScanListingService)，
//! 此处不重复校验。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::scan::dto::{HeldQuery, PickableQuery};
use crate::modules::prod::scan::listing::ScanListingService;
use crate::modules::prod::scan::vo::ScanListOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/prod/scan/pickable?work_type_id=&limit=&offset=`
///
/// 取件页数据源：PRODUCTION 区活跃架上、当前工序绑了该工种的可领取批次。
///
/// - 权限：Manager + Clerk + Inspector + ShelfAccount
/// - `work_type_id` **必填** query 参数（JSON string 形态，雪花 id 一律 string）
/// - 货架范围按当前账号 scope 收窄（Clerk / Inspector 无 scope ⇒ 空列表，见
///   `listing::service::pickable_shelf_scope` 的 doc）
/// - `limit` 缺省 50 / 上限 200：想要全部必须显式传 `limit=200`
pub async fn list_pickable(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<PickableQuery>,
) -> Result<Json<R<ScanListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out =
        ScanListingService::list_pickable(&mut conn, query.work_type_id, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/prod/scan/held?worker_id=&limit=&offset=`
///
/// 放回 / 送检页的唯一数据源：某工人当前持有的 IN_PROCESS+WORKER 批次，**带
/// 工序链位置**（`chain_state` 三值 + 下一道工序 id / 名字）。
///
/// - 权限：Manager + Clerk + Inspector + ShelfAccount
/// - `worker_id` **必填** query 参数（JSON string 形态）
/// - 不做货架 scope 收窄：行已在工人手上，不是架上的候选池
pub async fn list_held(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<HeldQuery>,
) -> Result<Json<R<ScanListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ScanListingService::list_held(&mut conn, query.worker_id, &query, &current).await?;
    Ok(Json(R::ok(out)))
}
