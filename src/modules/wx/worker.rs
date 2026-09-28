//! 微信小程序 BFF / worker 域 handler（2026-09-28 新增）
//!
//! 当前唯一端点：`GET /api/v2/wx/worker/stats?period=YYYY-MM` —— 当月工人
//! 工作量统计（聚合自 t_part_event）。
//!
//! ## 鉴权：Bearer JWT + Redis session（v2_router 中间件统一处理）
//!
//! ## 业务说明
//! `batch_count` = 该工人在该月发生过事件的不同 batch_id 数；
//! `work_hours` = 该工人在该月 PICKED_UP + RETURNED 事件的 SUM(quantity)
//! （DB 无 work_hours 列；以「加工件数」作为工作量估算）。
//!
//! worker_id 来源：当前 `CurrentUser.id`（t_user.id）。注意
//! t_part_event.worker_id 语义上是「t_worker.id」，**目前与 t_user.id 共享
//! 同一雪花 ID 空间**（migration 071 起的统一雪花策略）。如果未来 worker / user
//! ID 空间分裂，本端点需要中间映射。

use std::sync::Arc;

use axum::Json as AxumJson;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::Deserialize;

use crate::auth::rbac::CurrentUser;
use crate::modules::wx::repo::WorkerStats;
use crate::modules::wx::vo::MonthlyStats;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::resolve_period;

/// `/api/v2/wx/worker/*` 入口 router 工厂。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/stats", get(stats))
}

#[derive(Debug, Deserialize)]
pub struct WorkerStatsQuery {
    #[serde(default)]
    pub period: Option<String>,
}

/// `GET /api/v2/wx/worker/stats?period=YYYY-MM`
///
/// period 缺省 → 当前月。
pub async fn stats(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(q): Query<WorkerStatsQuery>,
) -> Result<AxumJson<R<MonthlyStats>>, AppError> {
    // period 校验：缺省 → 当前月；非空 → YYYY-MM 严格格式（复用 batches 的 helper）
    let period = resolve_period(q.period.as_deref())?;

    let mut conn = state.pool.acquire().await?;
    let resp = WorkerStats::by_user_period(&mut *conn, current.id, &period).await?;
    Ok(AxumJson(R::ok(resp)))
}
