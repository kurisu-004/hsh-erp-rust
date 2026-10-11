//! `POST /api/v2/prod/scan/outsource-receive` 的 handler（2026-10-15 新增 S3）
//!
//! ## 事务边界 + WS 广播
//! 事务边界在 handler（`pool.begin()` → service → `tx.commit()`，错误路径 tx drop
//! 隐式回滚）；WS 广播在 commit **之后**（对齐 Python 延迟广播模式）。
//!
//! commit 后广播 **`PART_PICKED_UP`**，payload 与同域 `pick-up` 端点
//! （`handler/transition.rs::pick_up`）**逐字同形**：四键
//! `{ part_id, worker_id, batch_id, quantity }`（雪花 id 全为 JSON string，
//! `quantity` 裸 number）。
//!
//! ⚠️ **本端点刻意不广播 `OUTSOURCE_MOVE_DONE`**：那个事件的 payload 是
//! `OutsourceMoveResult` 的**全量序列化**（含 `from_kind` / `to_kind` /
//! `shipment_id` / `new_process_id`），消费方按 `from_kind` / `to_kind` 推断方向。
//! 本端点没有 `OutsourceMoveResult` 语义（既要伪造 `shipment_id` 又要伪造
//! `to_kind` 的落点），伪造它会破坏 `outsource` 域的消费方契约。
//! 「收回」这一步对看板的效果与「批次被工人领走」等价（holder 是工人、
//! `IN_PROCESS + WORKER`），故走 `PART_PICKED_UP` 这一条既有通路。
//!
//! ## 权限
//! Manager 或 ShelfAccount（与 `worker-scan` / `take-over` 对齐，service 双层守卫）。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::prod::scan::dto::OutsourceReceiveRequest;
use crate::modules::prod::scan::service::ScanService;
use crate::modules::prod::scan::vo::OutsourceReceiveOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/prod/scan/outsource-receive`
///
/// 外协确认收回：件实际已回厂，扫码一步「收回落自动选架 + 立刻交到该工人手上」。
///
/// - 权限：`Manager` 或 `ShelfAccount`
/// - 入参：`OutsourceReceiveRequest { batch_id, badge_code, version,
///   next_process_id? }`；`next_process_id` 缺省时按链推导（**无链必填**，`40001`）
/// - 判据顺序与错误码见 `service/outsource_receive.rs` 的模块 doc
///   （`20201` / `20202` / `20206` → `20109` / `20101` → `40901` → `20103` /
///   `20122` → `40001` / `20706` → `20104` → `20904` / `20204` → `20508`）
/// - WS 广播：commit 后 `PART_PICKED_UP`（payload 与 `pick-up` 逐字同形）
pub async fn outsource_receive(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceReceiveRequest>,
) -> Result<Json<R<OutsourceReceiveOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
    let mut tx = state.pool.begin().await?;
    let out = ScanService::outsource_receive(&mut tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）。
    //
    // ⚠️ payload 与 `handler/transition.rs::pick_up` 的 `PART_PICKED_UP` **逐字同形**
    // （同一个事件名 + 同一组 4 个键）：消费方（dashboard / 队列看板的工人持有格子）
    // 无需为「外协收回」新写分支 —— 对看板而言结果就是「这一批现在在工人手上了」。
    // 改键集合时必须**两处同时改**。
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_PICKED_UP".into(),
        payload: json!({
            "part_id": out.part_id.to_string(),
            "worker_id": out.worker_id.to_string(),
            "batch_id": out.batch_id.to_string(),
            "quantity": out.picked_quantity,
        }),
    });
    Ok(Json(R::ok(out)))
}
