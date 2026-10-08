//! prod::scan 子模块 —— 报工台（工人扫码台）
//!
//! 2026-10-10 新建：把**报工台的 5 个端点**从它们原先散落在三个域（
//! `prod::worker` / `part` / `prod::batch`）的状态收拢进一域，URL 全部挂在
//! `/api/v2/prod/scan/*`，**硬切、无 alias**。
//!
//! ## 为什么按「报工台」立域而不是按「后端逻辑相似度」归域
//!
//! 本域的判定依据是**前端消费方**：5 条端点的唯一消费方是 `views/scan/` 的三页
//! （取件 / 放回 / 送检）+ 扫工牌弹窗 + 队列看板的一条动作。后端看，这 5 条端点
//! 分属三个域、依赖完全不同的模块；但从工厂现场的视角它们是**一台机器的五个
//! 按钮**。域按消费方聚拢之后，「报工台的契约改动 → 只动一个目录」这条性质才成立。
//!
//! ## 端点表（旧路径 → 新路径，全部无 alias）
//!
//! | 新路径 | 旧路径 | 迁自 |
//! |---|---|---|
//! | `POST /scan/verify-badge` | `POST /prod/workers/verify-badge` | `prod::worker` |
//! | `GET  /scan/pickable?work_type_id=&limit=&offset=` | `GET /parts/pickable-by-work-type/{work_type_id}` | `part` |
//! | `GET  /scan/held?worker_id=&limit=&offset=` | `GET /parts/by-worker/{worker_id}` | `part` |
//! | `POST /scan/worker-scan` | `POST /prod/batches/worker-scan` | `prod::batch` |
//! | `POST /scan/batches/{batch_id}/pick-up` | `POST /prod/batches/{batch_id}/pick-up` | `prod::batch` |
//!
//! 旧路径的实际失效形态（404 / 405，逐条实测）登记在 `docs/api/scan.md` §1.2 ——
//! **5 条里 4 条 404、1 条 405，没有一条是 400**：两条 part 域旧 list 路径是
//! 2 段 path，而 part 域的 `/{part_id}` catch-all 只有 1 段、够不着 ⇒ 干净 404；
//! `/prod/workers/verify-badge` 落进 worker 域的 `/{id}`（那条只注册了 GET）⇒
//! 方法不匹配 405；两条 batch 域旧路径整段路由已不存在 ⇒ 404。⚠️「落进 catch-all
//! ⇒ 400」这个直觉不总成立，段的**数量**是分水岭，详见 §1.2 的「教训登记」。
//!
//! ## 模块结构
//! - `dto.rs` —— 5 条端点的全部入参（`VerifyBadgeRequest` / `PickableQuery` /
//!   `HeldQuery` / `WorkerScanEvent` / `WorkerScanRequest` / `PickUpRequest`）
//! - `vo/` —— 4 组出参（`ScanWorkerBrief` / `ScanListItem` / `WorkerScanOut` …）
//! - `service/` —— 4 条写路径的用例（`impl ScanService`）
//! - `listing/` —— 2 条只读聚合端点，**受域隔离护栏覆盖**（`listing/mod.rs`）
//! - `handler/` —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! ## 依赖方向：本域是**转发型**域
//! `worker_scan` 必然 import `part`（repo / 事件日志 / 状态机）、`assembly`
//! （父件级联）、`prod::queue`（同事务 refill）、`prod::worker`（按工牌反查），
//! 外加两个跨域设施层 `shared::batch::chain`（链位置读写共用的唯一真源）与
//! `shared::shelf`（按负载自动选架）。故 `prod::scan` **整域不适用**
//! `shared::domain_guard`，只有 `listing/` 那两块纯只读聚合单独装护栏 ——
//! 圈出可守的部分比整域不守要强。
//!
//! ## 与 WS 的关系
//! `worker-scan` 在 commit 后广播 `WORKER_SCAN_RETURNED` /
//! `WORKER_SCAN_INSPECTED`（按**响应**的 `event_type`，不是请求的那个）+
//! `WORKER_POOL_REFILL_DONE` / `WORKER_POOL_EMPTY`；`pick-up` 广播
//! `PART_PICKED_UP`（部分领取时另发 `PART_BATCH_SPLIT`）。**事件名一字不改** ——
//! dashboard 与队列页在监听，改名会静默断链。
//!
//! 契约文档见 `docs/api/scan.md`。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod listing;
pub mod service;
pub mod vo;

pub use service::ScanService;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
