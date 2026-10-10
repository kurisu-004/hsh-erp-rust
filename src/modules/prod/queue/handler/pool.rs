//! prod::queue 的**队列写端点** HTTP handler（refill / move / auto-allocate）
//!
//! 2026-10-08 自原 `handler.rs` 拆出（该文件同时含 3 个读端点，聚合读已迁到
//! `handler/board.rs`）。
//!
//! ## 端点
//! - `POST /api/v2/prod/queue/refill` —— Manager 触发 `refill_for_worker`。
//! - `POST /api/v2/prod/queue/move` —— Manager 通用移动端点（POOL ↔ WORKER +
//!   WORKER ↔ WORKER 三方向，取代旧 `admin_remove` / `admin_assign`）。commit 后
//!   广播 `WORKER_POOL_MOVE_DONE`。
//! - `POST /api/v2/prod/queue/auto-allocate` —— 按 `process_id + shelf_id` 范围
//!   自动为每个匹配 worker 抢批次数 / 累计工时。commit 后广播
//!   `WORKER_POOL_AUTO_ALLOCATE_DONE`。
//!
//! ## 事务 + WS 广播
//! 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! 三个端点都是「pool.begin() → service → commit」，**广播在 commit 之后**。
//!
//! service 公共方法收 `&mut PgConnection`（生产），service 内部 reborrow
//! `&mut *conn` 喂 `QueueRepoTrait`（trait 已直接 `impl for &mut PgConnection`）。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use crate::modules::prod::queue::dto::{AdminRefillRequest, AutoAllocateRequest, MoveRequest};
use crate::modules::prod::queue::service::queue::QueueService;
use crate::modules::prod::queue::vo::worker as queue_vo;
use crate::modules::prod::queue::vo::worker::{AutoAllocateResult, MoveResult, RefillResult};

/// POST /api/v2/prod/queue/refill
///
/// Manager role 守卫。Commit 后：
/// - `taken.len() > 0` → 广播 `WORKER_POOL_REFILL_DONE`
/// - `stopped_reason == Some("POOL_EMPTY")` → 广播 `WORKER_POOL_EMPTY`
///
/// ⚠️ **2026-10-14 有意的协议行为变更：工人满时不再广播。**
/// 改造前的条件是 `else if r.pool_empty`，而 `pool_empty` 对「池空」与「工人已拿满」
/// **两种**成因都为 `true`（`take_one_from_pool` 对两者都返 `None`，SQL 层失败形态
/// 一样）。于是「这个工人已经拿满了 10 件」也会向所有订阅端推一条
/// `WORKER_POOL_EMPTY` —— 该事件名会误导看板把原因归给「池里没货」，而真实原因
/// 是「这个工人满了」。现在改读新增的 `stopped_reason`，只在该确为池空时广播。
///
/// **已决策：不新增 WS event kind**（用户 2026-10-14 拍板）。代价是「工人满」这条
/// 状态在 WS 上**完全没有信号** —— 只有直接看 `POST /refill` 的响应体才知道。
/// 接受这个代价的理由：(a) 「拿满了」本来就是按工人维度的事实，看板每个工人格子
/// 各自刷新自己的 `current_held`，不需要一条全局广播；(b) 新增 kind 会要求前端
/// 同步加分支，成本高于收益；(c) 响应体里的 `stopped_reason` 已足够前端在**触发
/// 这次 refill 的那个视图**里给出即时提示。
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn admin_refill(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AdminRefillRequest>,
) -> Result<Json<R<RefillResult>>, AppError> {
    current.require_role(Role::Manager)?;
    let mut tx = state.pool.begin().await?;
    let r = QueueService::refill_for_worker(
        &mut tx,
        &state.snowflake,
        req.worker_id,
        // 管理员端点**保留**架锚：它是「为某工人在某架上抢料」的显式操作（Manager
        // only、前端零消费方）。worker-scan 走的是另一条路径，传 `None`（跨架取料）。
        Some(req.shelf_id),
        current.id,
        &current,
    )
    .await?;
    tx.commit().await?;
    if !r.taken.is_empty() {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_REFILL_DONE".into(),
            payload: serde_json::to_value(&r).unwrap_or_default(),
        });
    } else if r.stopped_reason.as_deref() == Some(queue_vo::stopped_reason::POOL_EMPTY) {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_EMPTY".into(),
            payload: json!({
                "worker_id": req.worker_id.to_string(),
                "shelf_id": req.shelf_id.to_string(),
                "pool_empty": true,
            }),
        });
    }
    Ok(Json(R::ok(r)))
}

/// POST /api/v2/prod/queue/auto-allocate
///
/// 按 `process_id + shelf_id` 范围自动为每个匹配 worker 抢批次数 / 累计工时。
///
/// Manager 角色守卫下沉到 service（`auto_allocate_for_process` 内部 `require_role`）。
/// Commit 后广播 `WORKER_POOL_AUTO_ALLOCATE_DONE`（payload = `AutoAllocateResult`）。
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn auto_allocate(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<AutoAllocateRequest>,
) -> Result<Json<R<AutoAllocateResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let result =
        QueueService::auto_allocate_for_process(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_AUTO_ALLOCATE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}

/// POST /api/v2/prod/queue/move（2026-09-30 新增）。
///
/// 通用移动端点：覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向。
/// 取代原 `admin_remove`（WORKER→POOL 单边）+ `admin_assign`（POOL→WORKER 单边）。
///
/// Manager 角色守卫下沉到 service（`move_batch` 内部 `require_role`）。
/// Commit 后统一广播 `WORKER_POOL_MOVE_DONE`（payload 含 from/to 让前端推断方向）。
///
/// 纯写端点（① 形态）：`pool.begin() → service → commit`。
pub async fn move_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<MoveRequest>,
) -> Result<Json<R<MoveResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let result = QueueService::move_batch(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "WORKER_POOL_MOVE_DONE".into(),
        payload: serde_json::to_value(&result).unwrap_or_default(),
    });
    Ok(Json(R::ok(result)))
}
