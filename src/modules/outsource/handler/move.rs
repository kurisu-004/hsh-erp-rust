//! outsource 外协看板移动写端点 HTTP handler（2026-10-09 新增）
//!
//! - `POST /api/v2/outsource-queue/move` —— 三合一移动端点（发送 / 回收生产 / 回收品检）
//!
//! ## 取代关系（**硬切无 alias**，旧路径 404）
//! - `POST /prod/batches/{batch_id}/send-to-outsource` → `/outsource-queue/move`
//!   （`from.kind=PRODUCTION_SHELF` + `to.kind=OUTSOURCE_COMPANY`）
//! - `POST /prod/batches/{batch_id}/receive-from-outsource` → 同端点
//!   （`OUTSOURCE_COMPANY` → `PRODUCTION_SHELF`）
//! - `POST /prod/batches/{batch_id}/receive-from-outsource-to-inspection` → 同端点
//!   （`OUTSOURCE_COMPANY` → `INSPECTION_SHELF`）
//!
//! 三个旧端点的入参变化（**破坏性**，前端必须同步改）：
//! - `batch_id`：从 URL path 段改为 body 字段（三合一后路径退化成静态段 `/move`，
//!   主键进 body，与本域其余写端点一致）；
//! - `outsource_company_id` / `shelf_id`：改为 `to` 对象里的 `company_id` / `shelf_id`；
//! - `process_id`：**删除**（外协工序 = 批次当前所属工序，由后端自推）；
//! - `next_process_id`：改为 `to.next_process_id`，且**可省略**（后端按工序链推导）；
//! - `quantity`：**删除**（整批语义，部分收发先走共用拆批端点
//!   `POST /api/v2/batches/split`，`batch_id` 入 body）；
//! - 出参：`PartOut`（part 级）→ `OutsourceMoveResult`（批次级）。
//!
//! ## 事务边界 + WS 广播
//! 事务边界在 handler：`state.pool.begin()` → service → 显式 `tx.commit()`；提前 return
//! （`?`）时 `Transaction` 的 Drop 自动回滚。**广播在 commit 之后**，而 payload 的
//! 序列化在 commit 之前 —— 序列化的失败路径必须是回滚而不是「广播一个空 payload」。
//!
//! ## 角色守卫
//! 下沉到 service（`OutsourceMoveService::move_batch` 入口 `require_any_role`），handler
//! 不重复校验 —— 与本域其它端点一致。
//!
//! ## WS 事件名 `OUTSOURCE_MOVE_DONE`
//! payload = 整个 `OutsourceMoveResult` 的序列化（照
//! `prod::queue::handler::pool::move_batch` 广播 `WORKER_POOL_MOVE_DONE` 的形态），
//! 前端按 `from_kind` / `to_kind` 自行推断方向。
//!
//! ⚠️ **它不替代 `t_part_event` 的三个审计字面量**（`SENT_TO_OUTSOURCE` /
//! `RECEIVED_FROM_OUTSOURCE` / `RECEIVED_TO_INSPECTION`）：那三个是**业务事实**、
//! 逐字不变；WS 事件名是**传输层的一次移动完成**。旧的三个 WS 事件名
//! （`PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` /
//! `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED`）随本端点下线而删除。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::outsource::dto::OutsourceMoveRequest;
use crate::modules::outsource::service::OutsourceMoveService;
use crate::modules::outsource::vo::OutsourceMoveResult;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/outsource-queue/move`
///
/// 纯写端点：`pool.begin() → service → commit`，**commit 之后**广播
/// `OUTSOURCE_MOVE_DONE`（payload = `OutsourceMoveResult`）。
pub async fn move_batch(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<OutsourceMoveRequest>,
) -> Result<Json<R<OutsourceMoveResult>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let result = OutsourceMoveService::move_batch(&mut tx, &state.snowflake, req, &current).await?;
    // 广播 payload 在 commit **之前**序列化：出参是「标量 + String + Option」的封闭
    // 结构，序列化实际不可能失败，但失败路径不能是「静默降级成 `payload: null`」——
    // 那会让前端 WS 消费者收到一个形状不对的事件却收不到任何报错信号。这里失败即
    // 回滚，让「DB 已提交 / WS 未广播」这个不一致窗口根本不存在。
    let payload = serde_json::to_value(&result)
        .map_err(|e| AppError::internal(format!("序列化 OUTSOURCE_MOVE_DONE payload 失败: {e}")))?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "OUTSOURCE_MOVE_DONE".into(),
        payload,
    });
    Ok(Json(R::ok(result)))
}
