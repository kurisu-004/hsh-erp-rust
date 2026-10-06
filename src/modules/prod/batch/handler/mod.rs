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
// 剥离登记表
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
// ⚠️ 本表与 `docs/api/batch.md` 内容一致：契约只从 `docs/api/` 读
// （前端 CLAUDE.md 规定），本表是给改 Rust 代码的人就近看的。
//
// **改一同步二的义务**：往上面的 `router()` 加/删一条 `.route(...)` 时，
// `mod tests::routes_declared_in_router` 会立刻红，直到
// `ROUTES` 与 `STRIP_TARGETS` 同步更新为止。

/// 本域 router 注册的全部路由（`METHOD /path`，相对 `/api/v2/prod/batches`）。
///
/// 2026-10-08 起它是**剥离登记表的唯一真源**：`STRIP_TARGETS` 按同序同下标给出
/// 每条路由的目标域，单测据本表与 `router()` 源码比对（见 `mod tests`）。
/// 单独维护两份平行的「路由清单」必然漂移，故只留这一份。
pub const ROUTES: &[&str] = &[
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

/// 剥离登记表（目标域），**与 [`ROUTES`] 同序同长度**：下标 `i` 描述 `ROUTES[i]`。
///
/// 写成两条平行数组而不是 `&[(&str, &str)]`，是为了让「少写一条目标域」在类型上
/// 不可能发生（长度不等时 `mod tests` 直接红），而不是靠运行时比对才发现。
pub const STRIP_TARGETS: &[&str] = &[
    // ── 1 段静态 ──
    "多域共用（views/inspection/ + views/delivery/）",
    "多域共用（views/inspection/ + views/delivery/）",
    "views/scan/（扫码台）",
    "views/repair/",
    "views/repair/",
    // ── 2 段静态 ──
    "views/scan/（扫码台）",
    // ── 2 段动态 /{batch_id} ──
    "多域共用（views/inspection/ + views/delivery/）",
    "多域共用（views/inspection/ + views/delivery/）",
    "多域共用（views/inspection/ + views/delivery/）",
    "views/scan/（扫码台）",
    "views/delivery/（送货单域）",
    "多域共用（views/parts/ + views/assemblies/ + views/statistics/）",
    "views/repair/",
    "待定（消费方是零件列表页，非队列页）",
    "views/cnc/",
    "views/outsource/",
    "views/outsource/",
    "views/outsource/",
    "views/repair/",
    "views/repair/",
    "views/parts/detail/",
    "views/parts/detail/",
    "views/scan/（扫码台）",
];

/// 已被认领并从 batch 域移走的端点。
///
/// 列出来是为了让下一轮的人不重复找：这些路由**不在**本域 router 里了，
/// 要改它们去目标域。
pub const STRIPPED: &[(&str, &str)] = &[
    (
        "GET /inspection",
        "prod::inspection（2026-10-07 剥离，新路径 GET /prod/inspection/queue）",
    ),
    ("GET /pending", "prod::queue（2026-10-08 剥离）"),
    ("POST /dispatch", "prod::queue（2026-10-08 剥离）"),
    ("POST /auto-dispatch", "prod::queue（2026-10-08 剥离）"),
    (
        "POST /{batch_id}/recall-to-pending",
        "prod::queue（2026-10-08 剥离，新路径 POST /prod/queue/recall，batch_id 入 body）",
    ),
];

#[cfg(test)]
mod tests {
    use super::{ROUTES, STRIP_TARGETS};

    /// 从 `router()` 源码里抠出全部 `METHOD /path`。
    ///
    /// 解析规则（对本文件的书写方式足够，且不引入构建期依赖）：
    /// - 以 `.route(` 为锚点，跳过空白后读一个 `"…"` 字面量作 path；
    /// - 从该字面量末尾到**下一个** `.route(`（或函数末）之间出现 `post(` 记 POST、
    ///   出现 `get(` 记 GET，两者都出现即多方法路由（本域当前没有，直接 panic 以免
    ///   误判）。
    fn routes_in_router_source() -> Vec<String> {
        let src = include_str!("mod.rs");
        let body = src
            .split_once("pub fn router()")
            .expect("本文件必须有 pub fn router()")
            .1;
        let body = &body[..body
            .find(
                "
}
",
            )
            .expect("router() 必须有收尾大括号")];
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            let after_ws = rest.trim_start();
            assert!(
                after_ws.starts_with('"'),
                ".route( 后必须紧跟字符串字面量 path；实测：{after_ws:.40}"
            );
            let after_quote = &after_ws[1..];
            let end = after_quote.find('"').expect("path 字面量必须有闭合引号");
            let path = &after_quote[..end];
            let tail = &after_quote[end..];
            let seg_end = tail.find(".route(").unwrap_or(tail.len());
            let seg = &tail[..seg_end];
            let has_get = seg.contains("get(");
            let has_post = seg.contains("post(");
            let method = match (has_get, has_post) {
                (true, false) => "GET",
                (false, true) => "POST",
                (false, false) => panic!("`.route(\"{path}\")` 后既无 get( 也无 post(：{seg:.60}"),
                (true, true) => {
                    panic!("`.route(\"{path}\")` 同时注册了 get 与 post（ROUTES 是一路由一方法）")
                }
            };
            out.push(format!("{method} {path}"));
            rest = &tail[seg_end..];
        }
        out
    }

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// 这是登记表系列测试真正有牙齿的地方：`ROUTES` 与 `STRIP_TARGETS` 都是
    /// 人工 const，往 `router()` 加一条 `.route(...)` 而忘了更新它们时，靠这条
    /// 比对会立刻红 —— 否则「新加了路由忘了标目标域」要到 batch 域被删、
    /// 前端报 404 那天才暴露。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source();
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(
                got,
                want,
                "`router()` 与 ROUTES 的第 {} 条不一致",
                ROUTES.len()
            );
        }
    }

    /// 每条路由都必须有剥离目标域，且无重复。
    #[test]
    fn strip_targets_cover_every_route() {
        assert_eq!(
            ROUTES.len(),
            STRIP_TARGETS.len(),
            "ROUTES {} 条但 STRIP_TARGETS {} 条（少写目标域）",
            ROUTES.len(),
            STRIP_TARGETS.len(),
        );
        let mut seen = std::collections::BTreeSet::new();
        for (route, target) in ROUTES.iter().zip(STRIP_TARGETS.iter()) {
            assert!(!target.is_empty(), "路由 `{route}` 的目标域是空串");
            assert!(seen.insert(*route), "ROUTES 里 `{route}` 重复了");
        }
    }
}
