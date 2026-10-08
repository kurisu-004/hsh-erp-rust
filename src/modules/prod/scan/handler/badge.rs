//! prod::scan 扫工牌 handler

use std::sync::Arc;

use axum::Json;
use axum::extract::State;

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::scan::dto::VerifyBadgeRequest;
use crate::modules::prod::scan::service::ScanService;
use crate::modules::prod::scan::vo::ScanWorkerBrief;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/prod/scan/verify-badge`
///
/// 报工台三页的入口：工人用工牌号开机，返回后续两步要用的 4 个字段
/// （`id` → 拉 held 列表、`work_type_id` → 拉 pickable 列表、`badge_code` /
/// `name` → 顶栏显示）。
///
/// 权限：**任意已登录用户**（含 `SHELF_ACCOUNT`），故本 handler 不设角色闸门；
/// 鉴权由 `CurrentUser` extractor 承担（未登录 → 401）。
///
/// 出参是 `ScanWorkerBrief`（4 字段），不是 `WorkerOut`（12 字段）—— 收敛判据
/// 与证据见 [`ScanWorkerBrief`] 的 doc。
pub async fn verify_badge(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<VerifyBadgeRequest>,
) -> Result<Json<R<ScanWorkerBrief>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = ScanService::verify_badge(&mut tx, &req.badge_code, &current).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}
