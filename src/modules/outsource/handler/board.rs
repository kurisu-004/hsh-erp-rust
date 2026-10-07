//! outsource 外协看板聚合端点 HTTP handler（2026-10-09 新增）
//!
//! - `GET /api/v2/outsource-queue/snapshot` —— 工序序列板
//! - `GET /api/v2/outsource-queue/processes/{process_id}` —— 单工序板
//!
//! 两个都是**纯读**：handler `pool.acquire()` 不开事务，不发 WS 广播。
//!
//! ## 角色守卫
//! 下沉到 service（`OutsourceQueueService::build_*` 入口 `require_any_role`），handler
//! 不重复校验 —— 与 work_type / assembly 域惯例一致。两个端点都是 Manager + Clerk +
//! Inspector（旧 `/outsource-pool/state` 曾经是 Manager + Clerk 的更窄口径：它吐
//! `unit_price` 与客户 / 申请人名，是外协域的敏感读面；看板把单价内联进公司列后，
//! 两个端点的暴露面相同，取宽口径并保持一致更安全）。
//!
//! ## 取代关系（**无 alias**，旧路径 404）
//! - `GET /outsource-pool/counts` → `/outsource-queue/snapshot`
//! - `GET /outsource-pool/{process_id}` → `/outsource-queue/processes/{process_id}`
//!   （右列从「只给 `held_count`」变成「内联全部在途批次卡片」）
//! - `GET /outsource-pool/state?company_id=&process_id=` → 被上一条的
//!   `companies[].held_batches` 取代，**无需再单独请求**

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::CurrentUser;
use crate::modules::outsource::board::OutsourceQueueService;
use crate::modules::outsource::vo::{OutsourceQueueProcessDetail, OutsourceQueueSnapshot};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/outsource-queue/snapshot`
///
/// 取代原 `GET /outsource-pool/counts`。角色：Manager + Clerk + Inspector（service 内
/// 守卫）。**固定 3 条 SQL**，与工序数无关（见 `board/repo.rs` 的 `snapshot` doc）。
pub async fn snapshot(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<OutsourceQueueSnapshot>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = OutsourceQueueService::build_snapshot(&mut conn, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/outsource-queue/processes/{process_id}`
///
/// 取代原 `GET /outsource-pool/{process_id}` + 逐公司的 `GET /outsource-pool/state`。
/// 角色：Manager + Clerk + Inspector（service 内守卫）。**固定 4 条 SQL**，与公司数 /
/// 批次数无关（见 `board/repo.rs::process_detail` 的 doc）。
///
/// ⚠️ `Path<i64>` 抽不出数字时走 axum 的 `PathRejection`，返 **HTTP 400 纯文本**，
/// 不进 `R<T>` 信封（与本仓其它带 `Path<i64>` 的端点同款，是全仓行为而非本端点的
/// 特例）。
///
/// 工序不存在 / 已软删 → `20801 BIZ_PROCESS_NOT_FOUND`（HTTP 404）。
pub async fn process_detail(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(process_id): Path<i64>,
) -> Result<Json<R<OutsourceQueueProcessDetail>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = OutsourceQueueService::build_process_detail(&mut conn, &current, process_id).await?;
    Ok(Json(R::ok(out)))
}
