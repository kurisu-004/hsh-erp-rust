//! `POST /api/v2/prod/scan/take-over` 的 handler（2026-10-15 新增，扫码接管 S2）
//!
//! ## 事务边界 + WS 广播
//! 事务边界在 handler（`pool.begin()` → service → `tx.commit()`，错误路径 tx drop
//! 隐式回滚）；WS 广播在 commit **之后**（对齐 Python 延迟广播模式）。
//!
//! commit 后两条（顺序即先业务后派生）：
//! 1. `PART_TRANSFERRED` —— 无条件。payload 四键**全部 JSON 字符串**
//!    （雪花 id 精度约定）：`{ part_id, batch_id, from_worker_id, to_worker_id }`。
//!    ⚠️ 本事件是**新面**（2026-10-15 首次出现），消费方为 dashboard 与队列看板；
//!    改名会静默断链 —— 与同域其它事件同一纪律（事件名是裸 `String`，无枚举保护）。
//! 2. `ASSEMBLY_UPDATED` —— 仅当 `sync_from_batch_change` 返回 `Changed(aid)`
//!    （父装配件真变了），与 `handler/transition.rs::worker_scan` 同款模式。
//!
//! 本端点**没有** refill 编排：接管只换持有人与工序，不动工人池（refill 是
//! worker-scan 放回流的语义）。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::prod::scan::dto::TakeOverRequest;
use crate::modules::prod::scan::service::ScanService;
use crate::modules::prod::scan::vo::TakeOverOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/prod/scan/take-over`
///
/// 扫码接管：批次挂在别人手上（`IN_PROCESS + location='WORKER'` + holder≠我），
/// 扫工牌后 holder 换成我、顺手推进到我工种下的下一道工序。
///
/// - 权限：`Manager` 或 `ShelfAccount`（与 `worker-scan` 对齐）
/// - 入参：`TakeOverRequest { batch_id, badge_code, version, next_process_id?,
///   note? }`；`next_process_id` 仅链位置推导不出下一道时必填
/// - 错误码与判据顺序见 `service/transfer.rs` 的模块 doc（`20201` / `20202` /
///   `20206` → `20109` / `20101` → `40901` → `20103` → `20104` → `40001` /
///   `20702` → `20104` → `20904` / `20204` → `40901`）
/// - WS 广播：commit 后 `PART_TRANSFERRED`（payload 四键全为字符串）+ 派生
///   `ASSEMBLY_UPDATED`
pub async fn take_over(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<TakeOverRequest>,
) -> Result<Json<R<TakeOverOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
    let mut tx = state.pool.begin().await?;
    let out = ScanService::take_over(&mut *tx, &state.snowflake, req, &current).await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）。
    //
    // ⚠️ payload 四键**全部 JSON 字符串**（雪花 id 精度约定，与同域其它事件同款）；
    // 这是本事件名的第一个、也是当前唯一产出点，键集合即契约。
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_TRANSFERRED".into(),
        payload: json!({
            "part_id": out.part_id.to_string(),
            "batch_id": out.batch_id.to_string(),
            "from_worker_id": out.from_worker_id.to_string(),
            "to_worker_id": out.to_worker_id.to_string(),
        }),
    });
    // 父装配件真变了才补发（`SyncOutcome::Changed`），与 worker-scan 同款判据。
    if let Some(aid) = out.synced_assembly_id {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "ASSEMBLY_UPDATED".into(),
            payload: json!({ "assembly_id": aid.to_string() }),
        });
    }
    Ok(Json(R::ok(out)))
}
