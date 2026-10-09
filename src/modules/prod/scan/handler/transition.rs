//! prod::scan 报工台的写路径 handler —— 放回 / 送检（worker-scan）与手动 pick-up
//!
//! ## 端点
//! - `POST /api/v2/prod/scan/worker-scan`
//! - `POST /api/v2/prod/scan/batches/{batch_id}/pick-up`
//!
//! 2026-10-10 自 `prod::batch::handler::{transition,lifecycle}` 搬来（硬切无
//! alias）：两条端点的唯一消费方都是报工台 / 队列看板，按「目标域按前端消费方
//! 判定」的规约归 `prod::scan`。行为与 WS 事件名**一字不改**。
//!
//! ## 事务边界 + WS 广播
//! 事务边界在 handler（`pool.begin()` → service → `tx.commit()`）；WS 广播在
//! commit 之后（对齐 Python 延迟广播模式）。事件名逐字不变 —— dashboard 与队列
//! 页都在监听，改名会静默断链。唯一新增的是 `PART_BATCH_SPLIT`：2026-10-11
//! worker-scan 支持部分数量后与 pick-up 共用它，payload 字段名逐字同形。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::json;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::ws_hub::WsEvent;
use crate::modules::part::vo::PartOut;
use crate::modules::prod::queue::service::QueueService;
use crate::modules::prod::scan::dto::{PickUpRequest, WorkerScanRequest};
use crate::modules::prod::scan::service::ScanService;
use crate::modules::prod::scan::vo::WorkerScanOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 通用 WS 广播 helper：单 kind + 单 payload 字段。
#[inline]
fn ws_broadcast(state: &AppState, kind: &str, payload: serde_json::Value) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: kind.into(),
        payload,
    });
}

/// 父装配件自动同步 WS 广播事件（commit 后调用；送检流触发父 status 翻转时
/// 推送给 dashboard）。
fn ws_broadcast_assembly_updated(state: &AppState, assembly_id: i64) {
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
}

/// `POST /api/v2/prod/scan/worker-scan`
///
/// 工人扫码台主入口：RETURNED / INSPECTED 二合一。**同事务**调 scan → refill
/// （scan 与 refill 必须原子，否则扫描放回 → refill 抢批中间会被并发抢走同批）。
///
/// 行为：
/// - 权限：`Manager` 或 `ShelfAccount`（不是 Inspector——工人持有件自有工人操作）
/// - 入参：`WorkerScanRequest { serial_no, badge_code, event_type, next_process_id?,
///   batch_id?, quantity? }`
///   主键是 `serial_no`，`batch_id` 仅用于多批次消歧。**没有任何货架字段**
///   （2026-10-10 起两个货架字段都被删除，目标架由服务端按负载自动选）。
///   `quantity`（2026-10-11 新增）是 **JSON 字符串**，缺省 = 整批
/// - 业务流转：
///   - `RETURNED`：worker 把 IN_PROCESS+WORKER 批次放回**服务端选出的**生产架
///     （`next_process_id` 仅非顺应工序时必填）；**批次在链尾时改走送检**（自动送检）
///   - `INSPECTED`：worker 把持有件直接送检（品检架服务端自动选）
///   - 任一成功后同事务 `QueueService::refill_for_worker_with_work_type`
///     （**跨全部映射架取料**，无架锚）
/// - 部分数量（`0 < quantity < batch.quantity`）：service 先拆批，本次流转作用在
///   **拆出来的那一批**上 ⇒ 响应 `scan.batch_id` 是新批次；余量继承
///   `current_holder_id` 留在工人手上，仍出现在 `GET /scan/held` 列表里
/// - WS 广播：commit 后
///   - `PART_BATCH_SPLIT`（部分数量拆批时；payload 与 pick-up 那条**同形**）
///   - `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 **`scan_out.event_type`**，
///     即**响应**里那个值而非请求里的值 —— 链尾自动送检会让「请求 `RETURNED` /
///     响应 `WORKER_SCAN_INSPECTED`」成立，而 dashboard 两条事件都监听，故广播
///     链路无需改动即成立）；
///   - `WORKER_POOL_REFILL_DONE`（refill 抢到一批）或
///   - `WORKER_POOL_EMPTY`（refill 池空）。
///
/// 本端点一笔事务改 2 个批次（扫的那个 + 同事务从工人池补的），是全仓唯一的
/// 跨 part 批次写点；部分数量场景下拆批本身再加 1 笔 `t_part_batch` 写入。
/// 动作语义仍是「以批次为对象的工人报工」。
pub async fn worker_scan(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<WorkerScanRequest>,
) -> Result<Json<R<WorkerScanOut>>, AppError> {
    current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
    // 2026-10-10：`shelf_id` / `target_inspection_shelf_id` 两个入参删除 ⇒ handler
    // 侧那两道 `can_access_shelf` 防御检查也一并消失。货架范围收敛改由
    // `shared::shelf::select::shelf_scope_for` 在**选架那一步**统一承担
    // （它同时覆盖 SHELF_ACCOUNT 的手填白名单与 Manager 的 wildcard）。
    let mut tx = state.pool.begin().await?;
    // scan（状态翻转 + 写事件日志）
    let scan_out =
        ScanService::worker_scan_event(&mut *tx, &state.snowflake, req.clone(), &current).await?;
    // refill（同事务；`refill_for_worker_with_work_type` 内部对 work_type /
    // process 映射校验失败会抛业务错——事务自动回滚 scan 写入，保持原子语义）。
    // 复用 worker_scan_event 已经 fetch 过的 work_type_id + badge_code，
    // 跳过 queue service 内的 WorkerRepo::get_by_id 重复查询。
    let refill_out = QueueService::refill_for_worker_with_work_type(
        &mut tx,
        &state.snowflake,
        scan_out.worker_id,
        scan_out.work_type_id,
        // 2026-10-10：worker-scan 的 refill **不再有架锚** —— 跨全部映射该工种工序的
        // 活跃生产架取料。负载均衡的整体职责已在「放回时 `pick_least_loaded` 选架」
        // 一侧完成，继续按架过滤会在「放回到 A 架 → 随即从 A 架补料」这个闭环里查空池
        // （尤其是链尾自动送检：批次根本没落任何生产架）。
        None,
        &scan_out.badge_code,
        current.id,
        &current,
    )
    .await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）
    if let Some(aid) = scan_out.synced_assembly_id {
        ws_broadcast_assembly_updated(&state, aid);
    }
    // 2026-10-11：部分数量拆批 → 补发 PART_BATCH_SPLIT，payload 与 pick-up 那条
    // **逐字同形**（同一个事件名 + 同一组字段名），两处共用消费方。
    // 必须发：拆批把源批次的 quantity 静默扣减、并新建了一个批次行，源批次在
    // worker-scan 场景下**仍留在工人手上**（不像 pick-up 那样留在架上），其它端的
    // 批次视图不收到这条事件就永远看不到「持有件变多了 / 数量变了」。
    if let Some(split) = scan_out.split.as_ref() {
        ws_broadcast(
            &state,
            "PART_BATCH_SPLIT",
            json!({
                "part_id": scan_out.part_id.to_string(),
                "new_batch_id": split.new_batch_id.to_string(),
                "source_batch_id": split.source_batch_id.to_string(),
                "quantity": split.quantity,
            }),
        );
    }
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: scan_out.event_type.clone(),
        payload: serde_json::to_value(&scan_out).unwrap_or_default(),
    });
    if !refill_out.taken.is_empty() {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_REFILL_DONE".into(),
            payload: serde_json::to_value(&refill_out).unwrap_or_default(),
        });
    } else if refill_out.pool_empty {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "WORKER_POOL_EMPTY".into(),
            // 2026-10-10：`shelf_id` 键删除（refill 已无架锚，worker-scan 也不再收它）。
            // WS payload 与 HTTP 响应的 `refill.shelf_id` 同步为 `null`。
            payload: json!({
                "worker_id": scan_out.worker_id.to_string(),
                "shelf_id": serde_json::Value::Null,
                "pool_empty": true,
            }),
        });
    }
    Ok(Json(R::ok(WorkerScanOut {
        scan: scan_out,
        refill: refill_out,
    })))
}

/// `POST /api/v2/prod/scan/batches/{batch_id}/pick-up`
///
/// 手动 pick-up（B 方案）：PENDING / IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER。
/// Manager / Clerk / ShelfAccount 三角色可触发；worker 必须 active 且绑定 work_type。
///
/// `quantity` 支持部分领取（service 自动拆批）。**响应体形状不变**（仍 `R<PartOut>`），
/// 拆批信息只走 WS。
pub async fn pick_up(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(batch_id): Path<i64>,
    Json(req): Json<PickUpRequest>,
) -> Result<Json<R<PartOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    // `worker_id` 在 `req` 被 service 消费前先取出：它只进 WS payload，而
    // `outcome.part.id` 是 part id，与 `worker_id` 字段名不符。
    let worker_id = req.worker_id;
    let outcome = ScanService::pick_up(&mut *tx, &state.snowflake, batch_id, req, &current).await?;
    tx.commit().await?;
    // 部分领取发生了拆批 → 补发 PART_BATCH_SPLIT。
    // 必须发：拆批把源批次的 quantity 静默扣减、并新建了一个批次行，其它端的
    // 批次视图不收到这条事件就永远看不到「源批次余量变了 / 多了一个批次」。
    //
    // ⚠️ 本事件**不是**「与拆批端点同形」。两处共用 `part_id` / `new_batch_id`
    // 两个字段名（消费方唯一可无条件依赖的部分），后两个是本处的增量字段。
    // 同一事件名两种 payload 的完整对照见 `prod::batch::handler::lifecycle` 的
    // `split_batch_by_body`。
    if let Some(split) = outcome.split.as_ref() {
        ws_broadcast(
            &state,
            "PART_BATCH_SPLIT",
            json!({
                "part_id": split.part_id.to_string(),
                "new_batch_id": split.new_batch_id.to_string(),
                "source_batch_id": batch_id.to_string(),
                "quantity": split.quantity,
            }),
        );
    }
    // 补 batch_id + quantity（整批路径 = 源批次 / 整批量；部分路径 = 拆出来的
    // 新批次 / 拆走量），消费方据此知道工人领走了哪一批。
    ws_broadcast(
        &state,
        "PART_PICKED_UP",
        json!({
            "part_id": outcome.part.id.to_string(),
            "worker_id": worker_id.to_string(),
            "batch_id": outcome.picked_batch_id.to_string(),
            "quantity": outcome.picked_quantity,
        }),
    );
    Ok(Json(R::ok(outcome.part)))
}
