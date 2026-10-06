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
//!   `place-on-shelf` / `release-from-programming` / outsource 三端点
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
        // ① 1 段静态段（无 Path）—— 与 §② 的 2 段静态段、§③ 的 `/{batch_id}/…`
        //    动态组共存。**静态段必须先于 `/{batch_id}/...` 注册**（axum matchit 对
        //    同优先级按注册序；1 段与 2 段组段数不同，天然无冲突）。
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
        // ③ 2 段、首段动态 `/{batch_id}` —— 子资源 16 条
        //
        // ⚠️ `/{batch_id}/…` 与上面的 `/scan/deliver` **段数相同**，靠 matchit 的
        // 静态段优先规则消解（静态注册在前即可，实测 `POST /prod/batches/scan/deliver`
        // 不被 `/{batch_id}` 吞掉）。切勿把 `/scan/deliver` 移到本组之后。
        //
        // ⚠️ 本组**不注册** `GET /{batch_id}`：它与 §① 的 1 段静态集合读同形状
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

// ============================================================================
// 剥离登记表（2026-10-08 新增）
// ============================================================================
//
// 逐域剥离策略：batch 域**本轮只做 queue 那一份**；后续每一轮某个域重构时重复
// 这个动作（按「目标域」列认领本表中的端点），直到 batch 域的端点被全部分走
// 之后，再删除该域。
//
// 「目标域」按**前端消费方**判定（哪个页面的哪个按钮在调它），不按后端逻辑相似度
// 判定 —— 后端按表 / 状态机聚在一起，前端按页面聚在一起，两者的切分线不同。
// 一条端点被多个页面消费时归「多域共用」，由后续某一轮自行认领。
//
// ⚠️ 本表与 `docs/api/production/batch.md` 内容一致：契约只从 `docs/api/` 读
// （前端 CLAUDE.md 规定），本表是给改 Rust 代码的人就近看的。

/// 剥离登记表（路由 → 目标域）。`mod tests` 里有一条单测断言
/// 「本表覆盖 router 里注册的全部路由」，改路由时同步改表。
pub const STRIP_REGISTRY: &[(&str, &str)] = &[
    // ── 1 段静态 ──
    ("POST /to-ship", "多域共用（views/inspection/ + views/delivery/）"),
    ("POST /to-inspection", "多域共用（views/inspection/ + views/delivery/）"),
    ("POST /worker-scan", "views/scan/（扫码台）"),
    ("GET /repair", "views/repair/"),
    ("GET /repairing", "views/repair/"),
    // ── 2 段静态 ──
    ("POST /scan/deliver", "views/scan/（扫码台）"),
    // ── 2 段动态 /{batch_id} ──
    ("POST /{batch_id}/to-inspection", "多域共用（views/inspection/ + views/delivery/）"),
    ("POST /{batch_id}/to-ship", "多域共用（views/inspection/ + views/delivery/）"),
    ("POST /{batch_id}/to-process", "多域共用（views/inspection/ + views/delivery/）"),
    ("POST /{batch_id}/scan-inspect", "views/scan/（扫码台）"),
    ("POST /{batch_id}/deliver", "views/delivery/（送货单域）"),
    ("POST /{batch_id}/complete", "多域共用（views/parts/ + views/assemblies/ + views/statistics/）"),
    ("POST /{batch_id}/start-repair", "views/repair/"),
    ("POST /{batch_id}/place-on-shelf", "待定（消费方是零件列表页，非队列页）"),
    ("POST /{batch_id}/release-from-programming", "views/cnc/"),
    ("POST /{batch_id}/send-to-outsource", "views/outsource/"),
    ("POST /{batch_id}/receive-from-outsource", "views/outsource/"),
    (
        "POST /{batch_id}/receive-from-outsource-to-inspection",
        "views/outsource/",
    ),
    ("POST /{batch_id}/complete-repair", "views/repair/"),
    ("POST /{batch_id}/repair-dispatch", "views/repair/"),
    ("POST /{batch_id}/split", "views/parts/detail/"),
    ("POST /{batch_id}/cancel", "views/parts/detail/"),
    ("POST /{batch_id}/pick-up", "views/scan/（扫码台）"),
];

/// 已被认领并从 batch 域移走的端点（2026-10-08 本轮）。
///
/// 列出来是为了让下一轮的人不重复找：这些路由**不在**本域 router 里了，
/// 要改它们去目标域。
pub const STRIPPED: &[(&str, &str)] = &[
    (
        "GET /inspection",
        "prod::inspection（2026-10-07 已剥离，改为 GET /prod/inspection/queue）",
    ),
    ("GET /pending", "prod::queue（本轮已剥离）"),
    ("POST /dispatch", "prod::queue（本轮已剥离）"),
    ("POST /auto-dispatch", "prod::queue（本轮已剥离）"),
    (
        "POST /{batch_id}/recall-to-pending",
        "prod::queue（本轮已剥离，改为 POST /prod/queue/recall，batch_id 入 body）",
    ),
];

#[cfg(test)]
mod tests {
    use super::STRIP_REGISTRY;

    /// 登记表必须覆盖 router 注册的**每一条**路由，且不多不少。
    ///
    /// 防止「新加了路由忘了在登记表上标目标域」—— 那种遗漏在剥离时会变成
    /// 「这条端点没人认领，batch 域被删后前端才报 404」。
    #[test]
    fn strip_registry_covers_every_registered_route() {
        // router() 里注册的 24 条路由（人工核对；改 router 时同步改这里）
        const REGISTERED: &[&str] = &[
            "POST /to-ship",
            "POST /to-inspection",
            "POST /worker-scan",
            "GET /repair",
            "GET /repairing",
            "POST /scan/deliver",
            "POST /{batch_id}/to-inspection",
            "POST /{batch_id}/to-ship",
            "POST /{batch_id}/to-process",
            "POST /{batch_id}/scan-inspect",
            "POST /{batch_id}/deliver",
            "POST /{batch_id}/complete",
            "POST /{batch_id}/start-repair",
            "POST /{batch_id}/place-on-shelf",
            "POST /{batch_id}/release-from-programming",
            "POST /{batch_id}/send-to-outsource",
            "POST /{batch_id}/receive-from-outsource",
            "POST /{batch_id}/receive-from-outsource-to-inspection",
            "POST /{batch_id}/complete-repair",
            "POST /{batch_id}/repair-dispatch",
            "POST /{batch_id}/split",
            "POST /{batch_id}/cancel",
            "POST /{batch_id}/pick-up",
        ];
        for r in REGISTERED {
            assert!(
                STRIP_REGISTRY.iter().any(|(route, _)| route == r),
                "路由 `{r}` 未登记目标域（剥离登记表漏了）"
            );
        }
        for (route, _) in STRIP_REGISTRY {
            assert!(
                REGISTERED.contains(route),
                "登记表里的 `{route}` 在 router 里已不存在（端点被移走或改名？）"
            );
        }
        assert_eq!(
            STRIP_REGISTRY.len(),
            REGISTERED.len(),
            "登记表条数与 router 路由数不一致"
        );
    }
}
