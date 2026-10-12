//! prod::scan 子模块 —— 报工台（工人扫码台）
//!
//! 2026-10-10 新建：把**报工台的 5 个端点**从它们原先散落在三个域（
//! `prod::worker` / `part` / `prod::batch`）的状态收拢进一域，URL 全部挂在
//! `/api/v2/prod/scan/*`，**硬切、无 alias**（2026-10-15 扫码找件 · 自助恢复
//! 三件套落地后共 **8 条**，见下）。
//!
//! ## 为什么按「报工台」立域而不是按「后端逻辑相似度」归域
//!
//! 本域的判定依据是**前端消费方**：8 条端点的唯一消费方是 `views/production/scan/`
//! 的各页（取件 / 放回 / 送检 / 扫码找件）+ 扫工牌弹窗 + 队列看板的一条动作。后端看，
//! 这些端点分属多个域、依赖完全不同的模块；但从工厂现场的视角它们是**同一台机器的
//! 一组按钮**。域按消费方聚拢之后，「报工台的契约改动 → 只动一个目录」这条性质才成立。
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
//! **迁入的 5 条里 4 条 404、1 条 405，没有一条是 400**（2026-10-15 新增的 3 条为
//! 纯新增、无旧路径）：两条 part 域旧 list 路径是
//! 2 段 path，而 part 域的 `/{part_id}` catch-all 只有 1 段、够不着 ⇒ 干净 404；
//! `/prod/workers/verify-badge` 落进 worker 域的 `/{id}`（那条只注册了 GET）⇒
//! 方法不匹配 405；两条 batch 域旧路径整段路由已不存在 ⇒ 404。⚠️「落进 catch-all
//! ⇒ 400」这个直觉不总成立，段的**数量**是分水岭，详见 §1.2 的「教训登记」。
//!
//! ## 2026-10-15：扫码找件 · 自助恢复三件套（第 6 / 7 / 8 条端点，纯新增）
//!
//! | 端点 | 说明 |
//! |---|---|
//! | `GET /scan/locate?serial_no=&worker_id=` | S1：只读诊断（位置事实 + 针对该工人的 `action` / `disabled_reason` + **工种可执行工序 `worker_processes`**，2026-10-16 新增） |
//! | `POST /scan/take-over` | S2：扫码接管（holder worker → worker + 工序顺推的原子写） |
//! | `POST /scan/outsource-receive` | S3：外协确认收回（收回落自动选架 + 立刻交到扫码工人，同一事务） |
//!
//! 三条端点的判据都与对应写侧守卫**同源**（会成功的绝不显示 `NONE`、会拒绝的绝不
//! 显示可点按钮），逐条对照见 `docs/api/scan.md` §3.6。S3 的「收回落架」内核复用
//! `outsource::service::move_svc::receive_from_outsource_inner`（`pub(crate)`）。
//! ⚠️ S3 修正了 S1 的一处**假阻挡**：`locate` 此前对有链但推不出 `NEXT` 的外协
//! 批次给 `NONE`，而写侧支持手填 `next_process_id` ⇒ 该硬阻挡与
//! `LocateDisabledReason::ProcessChainRequired` 变体已一并删除。
//!
//! ## 2026-10-16：接管 / 外协收回的目标工序改由「工人的工种映射」推导
//!
//! 业务口径：报工台这两个端点是「实物已经在本人手上」的现场恢复动作，工人**不该**
//! 被要求在全部工序里挑一道 —— 该做什么由他的**工种**说了算。
//!
//! - `next_process_id` 由「链推不出就必填」变成**三态**（逐条见
//!   `service/transfer.rs` 与 `service/outsource_receive.rs` 的模块 doc）：
//!   链可推导 ⇒ 用链（**优先级最高**，显式值也不覆盖，不破坏工序链进度）；
//!   链推不出 + 显式 ⇒ 允许**工序不在该工单链内**
//!   （`current_process_step_id` 落 `NULL`，`20702` 不再表示「越链」）；
//!   链推不出 + 缺省 ⇒ 工种候选恰好 1 条自动推导 / >1 条 `40001` / 0 条 `20104`。
//!   候选集 = `t_work_type_process` 活跃映射，**不与工序链求交**
//!   （非链推导形态下默认「这张工单的链已不适用」）。
//! - `outsource-receive` 的次序**刻意不同**：显式值覆盖链推导的 2026-10-15 语义
//!   逐字不变，缺省时先信链、推不出才退回工种候选（该端点的链推导口径是 outsource
//!   域的 `sort_order = 当前 + 1`，稀疏链上与读侧不等价，分叉登记见 `docs/api/scan.md` §8）。
//! - 读侧 `GET /scan/locate` 出参新增**顶层** `worker_processes`（工种可执行工序，
//!   供前端把 picker 收窄到这个集合；单候选时直接提交、不弹框）；两个闸门函数各加一条
//!   「**非链推导形态下**工种零候选 ⇒ `PROCESS_NOT_IN_WORK_TYPE`」—— 复用既有
//!   字面量，**不扩 wire 枚举**。
//! - 错误码分段不变：复用 `20104` / `40001` / `20702`，无新增段。
//!
//! ## 模块结构
//! - `dto.rs` —— 8 条端点的全部入参（`VerifyBadgeRequest` / `PickableQuery` /
//!   `HeldQuery` / `LocateQuery` / `WorkerScanEvent` / `WorkerScanRequest` /
//!   `PickUpRequest` / `TakeOverRequest` / `OutsourceReceiveRequest`）
//! - `vo/` —— 6 组出参（`ScanWorkerBrief` / `ScanListItem` / `WorkerScanOut` /
//!   `LocateOut` / `TakeOverOut` / `OutsourceReceiveOut` …）
//! - `service/` —— 5 条写路径的用例（badge / worker_scan / pickup / transfer /
//!   outsource_receive，`impl ScanService`）
//! - `listing/` —— 3 条只读聚合端点（pickable / held / locate），**受域隔离护栏覆盖**（`listing/mod.rs`）
//! - `handler/` —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! ## 依赖方向：本域是**转发型**域
//! `worker_scan` 必然 import `part`（repo / 事件日志 / 状态机）、`assembly`
//! （父件级联）、`prod::queue`（同事务 refill）、`prod::worker`（按工牌反查），
//! 外加两个跨域设施层 `shared::batch::chain`（链位置读写共用的唯一真源）与
//! `shared::shelf`（按负载自动选架）。故 `prod::scan` **整域不适用**
//! `shared::domain_guard`，只有 `listing/` 那三块纯只读聚合单独装护栏 ——
//! 圈出可守的部分比整域不守要强。
//!
//! ## 与 WS 的关系
//! `worker-scan` 在 commit 后广播 `WORKER_SCAN_RETURNED` /
//! `WORKER_SCAN_INSPECTED`（按**响应**的 `event_type`，不是请求的那个）+
//! `WORKER_POOL_REFILL_DONE` / `WORKER_POOL_EMPTY`（**2026-10-14 起后者只在
//! `stopped_reason == "POOL_EMPTY"` 时发**，「工人已拿满」不再冒充池空；判据与
//! 另一产出点 `prod::queue` 的「改一同步二」见
//! `handler/transition.rs::worker_scan` 的 doc），
//! **部分数量拆批时另发
//! `PART_BATCH_SPLIT`**（与 `pick-up` 那条同名同 payload 字段）；`pick-up` 广播
//! `PART_PICKED_UP`（部分领取时另发 `PART_BATCH_SPLIT`）；2026-10-15 三件套起
//! `take-over` 广播 `PART_TRANSFERRED`、`outsource-receive` 广播 `PART_PICKED_UP`
//! （与 `pick-up` 那条逐字同形 —— 同一事件名 + 同一组 4 个键，改一处必须两处同时改）。
//! **事件名一字不改** —— dashboard 与队列页在监听，改名会静默断链。
//!
//! ## 部分数量（2026-10-11 新增）
//! `worker-scan` 的 `RETURNED` / `INSPECTED` 与同域 `pick-up` 一样支持 `quantity`
//! 指定本次操作量：缺省 = 整批，`0 < q < batch.quantity` 自动拆批并作用在拆出来的
//! 那一批上（响应 `scan.batch_id` 返回它），`<= 0` / `>` 返 `20111`。**两处的差别
//! 只有一处**：worker-scan 的**余量继承 `current_holder_id` 留在工人手上**（继续
//! 出现在「已持有」列表），pick-up 的余量留在原处。
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
