//! `GET /api/v2/prod/scan/locate` 的 handler（2026-10-15 新增，扫码找件 · 诊断）
//!
//! 只读：走 `pool.acquire()` **不开事务**、不发 WS（与同子模块两条 list 端点同款）。
//! 角色闸门在 service 层（复用 `SCAN_LIST_ROLES`），此处不重复校验。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::scan::dto::LocateQuery;
use crate::modules::prod::scan::listing::ScanListingService;
use crate::modules::prod::scan::vo::LocateOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/prod/scan/locate?serial_no=&worker_id=`
///
/// 扫图纸序列号 → 该 part 全部未软删批次（**含终态**）的位置事实；传了
/// `worker_id` 时按该工人给出每批的 `action` / `disabled_reason`。
///
/// - 权限：Manager + Clerk + Inspector + ShelfAccount（与 `/pickable` / `/held` 同）
/// - `serial_no` **必填**；`worker_id` 可选（**JSON 字符串**，发数字 / 漏传
///   `serial_no` → axum `QueryRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**）
/// - 未命中工单 → `20101`（HTTP 404，不生回退 `t_assembly`）；`worker_id` 无效 →
///   `20201`（404）/ `20202`（400）
/// - 出参 `LocateOut { part, batches, worker_processes }`：`worker_processes` 是
///   **顶层**字段（2026-10-16 新增，前端目标工序 picker 的选项集；工序已软删的行
///   不在其中，详见 `listing/repo.rs::fetch_worker_processes`），逐字段契约见
///   `docs/api/scan.md`
pub async fn locate(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<LocateQuery>,
) -> Result<Json<R<LocateOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = ScanListingService::locate(&mut conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}
