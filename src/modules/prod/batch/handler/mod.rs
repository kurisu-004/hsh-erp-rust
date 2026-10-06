//! `prod::batch` HTTP handler 汇总 + 批次路由的注册表。
//!
//! 2026-10-08：`dispatch.rs`（`pending` / `dispatch` / `auto-dispatch` 三端点）
//! 与 `lifecycle.rs::recall_to_pending` 搬往 `prod::queue`（下发流与召回的
//! 消费方是队列页，不是批次详情页）。本域剩 `transition.rs` + `lifecycle.rs`。
//!
//! ## 子文件
//! - `transition.rs` —— to-XXX 流（`to-ship` / `to-inspection` / `to-process`）+ 批量
//!   流转 + 扫码快捷入口（`scan-inspect` / `scan/deliver` / `worker-scan`）+ 集合读
//!   （`repair` / `repairing`）
//! - `lifecycle.rs` —— 终态 + 状态机扩展（`deliver` / `complete` / `start-repair` /
//!   `place-on-shelf` / `recall-to-pending` / `release-from-programming` / outsource 三端点
//!   / `complete-repair` / `repair-dispatch` / `split` / `cancel` / `pick-up`）
//!
//! ## 事务边界
//! 统一在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! 统一响应信封：`Result<Json<R<T>>, AppError>`。权限在 handler 与 service 双层守卫。

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

pub mod lifecycle;
pub mod transition;

// ----- transition.rs -----
pub use transition::{
    batch_to_inspection, batch_to_ship, list_repair_batches, list_repairing_batches,
    scan_deliver_part, scan_inspect, to_inspection, to_process, to_ship, worker_scan,
};

// ----- lifecycle.rs -----
pub use lifecycle::{
    cancel_batch, complete, complete_repair, deliver, pick_up, place_on_shelf,
    receive_from_outsource, receive_from_outsource_to_inspection, release_from_programming,
    repair_dispatch, send_to_outsource, split_batch, start_repair,
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ====================================================================
        // ① 1 段静态段（无 Path）—— 与 §2.2 的 3 条批量 / 事件端点、
        //    §2.3 的 2 条集合读同段数。**静态段必须先于 `/{batch_id}/...` 注册**（axum matchit 对
        //    同优先级按注册序；本组与 2 段动态组段数不同，天然无冲突）。
        // ====================================================================
        // ---- 静态批量流转（2 条，无 Path extractor）----
        .route("/to-ship", post(transition::batch_to_ship))
        .route("/to-inspection", post(transition::batch_to_inspection))
        // ---- 工人扫码台主入口（无 Path extractor，主键 serial_no）----
        .route("/worker-scan", post(transition::worker_scan))
        // ---- 集合读 2 条（只读端点，pool.acquire() 不开事务）----
        // 待品检队列读 `/inspection` 已于 2026-10-07 迁往 `prod::inspection`
        // （新路径 `GET /api/v2/prod/inspection/queue`，**无 alias**）
        .route("/repair", get(transition::list_repair_batches))
        .route("/repairing", get(transition::list_repairing_batches))
        // ====================================================================
        // ② 2 段、首段静态（无 Path）—— `scan/deliver`：`serial_no` 反查批次
        // ====================================================================
        .route("/scan/deliver", post(transition::scan_deliver_part))
        // ====================================================================
        // ③ 2 段、首段动态 `/{batch_id}` —— 子资源 18 条
        //
        // ⚠️ `/{batch_id}/…` 与上面的 `/scan/deliver` **段数相同**，靠 matchit 的
        // 静态段优先规则消解（静态注册在前即可，实测 `POST /prod/batches/scan/deliver`
        // 不被 `/{batch_id}` 吞掉）。切勿把 `/scan/deliver` 移到本组之后。
        //
        // ⚠️ 本组**不注册** `GET /{batch_id}`：它与 §③ 的 1 段静态集合读同形状
        // （`GET /prod/batches/repair`），加了会让「单批次详情」与「集合读」
        // 在 matchit 里争同一段位。批次详情请走 part 域
        // `GET /api/v2/parts/{part_id}/batches`。
        // ====================================================================
        // ---- to-XXX 流（品检）----
        .route("/{batch_id}/to-inspection", post(transition::to_inspection))
        .route("/{batch_id}/to-ship", post(transition::to_ship))
        .route("/{batch_id}/to-process", post(transition::to_process))
        .route("/{batch_id}/scan-inspect", post(transition::scan_inspect))
        // ---- 终态 ----
        .route("/{batch_id}/deliver", post(lifecycle::deliver))
        .route("/{batch_id}/complete", post(lifecycle::complete))
        .route("/{batch_id}/start-repair", post(lifecycle::start_repair))
        // ---- 上架 / 编程 ----
        .route(
            "/{batch_id}/place-on-shelf",
            post(lifecycle::place_on_shelf),
        )
        .route(
            "/{batch_id}/release-from-programming",
            post(lifecycle::release_from_programming),
        )
        // ---- 外协 ----
        .route(
            "/{batch_id}/send-to-outsource",
            post(lifecycle::send_to_outsource),
        )
        .route(
            "/{batch_id}/receive-from-outsource",
            post(lifecycle::receive_from_outsource),
        )
        .route(
            "/{batch_id}/receive-from-outsource-to-inspection",
            post(lifecycle::receive_from_outsource_to_inspection),
        )
        // ---- 返修 ----
        .route(
            "/{batch_id}/complete-repair",
            post(lifecycle::complete_repair),
        )
        .route(
            "/{batch_id}/repair-dispatch",
            post(lifecycle::repair_dispatch),
        )
        // ---- 批次操作 ----
        .route("/{batch_id}/split", post(lifecycle::split_batch))
        .route("/{batch_id}/cancel", post(lifecycle::cancel_batch))
        .route("/{batch_id}/pick-up", post(lifecycle::pick_up))
}
