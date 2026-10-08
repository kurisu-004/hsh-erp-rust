//! prod::batch 子模块 —— `t_part_batch` 域（生产执行单元 = 批次）
//!
//! 2026-10-02 域迁移：`t_part_batch` 的 repo / model / 状态机写入口、**全部以批次
//! 为对象的服务用例**与批次路由（URL 锚由 `part_id` 改为 `batch_id`）整体
//! 从 part 域搬入本模块，路由从 `POST /api/v2/parts/…` 硬切到
//! `POST /api/v2/prod/batches/…`（**无 alias**，前端配套 PR 锁步迁移）。本域现注册
//! **19 条**域内路由（权威清单是 `handler/mod.rs::ROUTES`，`mod tests` 断言它与
//! `router()` 源码逐条一致；下方路由表是给读的人看的手写摘要）。
//!
//! part 域自此只保留「多批次动作 + part 级动作」：`/{part_id}/cancel`（BATCH-N）、
//! `/{part_id}/force-complete`（BATCH-N）、`/{part_id}/soft-delete`、
//! `GET /{part_id}/batches` 与全部 CRUD / 文件 / Excel 工具 / 各类 list 端点。
//!
//! 依赖方向：prod → part 单向（本模块引用 `PartOut` / `PartRepoTrait` /
//! `part::statemachine` 等 part 域实体）；但 `prod::batch::service` 的批次方法经
//! part 域 `PartRepoTrait` 的默认体回调本模块 `PartBatchRepo` + `shared::batch::status`，
//! **构成反向依赖，尚未单向**。part → prod 方向另有 `shared::batch::status` +
//! `PartBatchRepo` 两处数据依赖。过渡期成因与收敛步骤见
//! [`service`] 模块 doc。
//!
//! ## 模块结构
//! - `model.rs` —— `RecentBatchRow` / `PartBatchScanRow` 两种窄投影行结构
//!   （`TPartBatch` 全列行已上移 `shared::batch::model`）
//! - `repo/queries.rs` —— ZST `PartBatchRepo` + 通用 SQL 静态方法
//! - `repo/sql.rs` —— inspection / lifecycle 流转的定位 + 写点
//! - `repo/trait.rs` —— 胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`
//! - `repo/mod.rs` —— 3 个子模块的声明与重导出（原 ZST `BatchRepo` 已迁
//!   `prod::queue::repo::dispatch` 并改名 `QueueDispatchRepo`）
//! - `service/` —— 全部业务用例（`impl BatchService`，按流拆文件，见该目录 mod doc）
//! - `dto.rs` —— 全部入参（`Deserialize`）
//! - `vo.rs` —— 全部出参（`Serialize`）
//! - `handler/{transition,lifecycle}.rs` —— HTTP 路由 + 角色守卫 + WS 广播
//!   （`handler/dispatch.rs` 已迁 `prod::queue`）
//!
//! ## 2026-10-08：三处公共设施已上移到 `shared::batch`
//!
//! - `status_gate.rs` → `shared::batch::status`：全仓唯一 `t_part_batch.status`
//!   写入口（写 + batch → part → assembly 派生焊在一个函数里）；
//! - `service/guard.rs` → `shared::batch::guards`：状态机守卫 / OCC / 货架校验；
//! - `model.rs` 的 `TPartBatch` → `shared::batch::model`。
//!
//! 迁移动机：这三者是**所有碰批次的域都要用**的公共设施，与「批次有哪些业务
//! 用例」无关。留在本域意味着每剥离一个新域（queue / scan / inspection /
//! delivery / repair / outsource / cnc）就多一条指向 batch 域的反向依赖。
//! 上移后依赖方向与派生图方向一致（上层域 → shared）。
//! 详细边界记档见 `shared::batch` 模块 doc 与 `src/shared/mod.rs`。
//!
//! ## 2026-10-07 迁出：待品检队列读
//! `GET /api/v2/prod/batches/inspection`（+ 它的 `dto` / `vo` / `model` 行结构 /
//! `repo/list.rs` / `service/list.rs`）整体迁往 `prod::inspection`，新路径
//! `GET /api/v2/prod/inspection/queue`，**无 alias**。理由：该页面的两个数据源
//! （队列列表 + 扫码树）本就同属一个页面，迁后 `prod::inspection` 零跨域依赖、
//! 可被域隔离护栏完整覆盖。**返修两条集合读**（`/repair` / `/repairing`）的 SQL 在
//! service 层自建、不在本域 repo 层。
//!
//! ## 2026-10-10 报工台两条端点迁出
//! `POST /worker-scan` 与 `POST /{batch_id}/pick-up` 连同其 handler / service /
//! DTO / 出参整体迁往 `crate::modules::prod::scan`（硬切无 alias，新路径
//! `POST /api/v2/prod/scan/worker-scan` 与
//! `POST /api/v2/prod/scan/batches/{batch_id}/pick-up`）—— 两条端点的唯一消费方
//! 是报工台三页与队列看板，按「目标域按前端消费方判定」的规约归 `prod::scan`。
//! 见 `handler/mod.rs` 的 `STRIPPED` 表。
//!
//! ## 路由表（摘要，权威清单见 `handler/mod.rs::ROUTES`）
//!
//! 静态 1 段（原 `/api/v2/parts/…`，**无 Path extractor**）：
//! - `POST   /to-ship`          ← `/api/v2/prod/batches/to-ship`
//! - `POST   /to-inspection`    ← `/api/v2/prod/batches/to-inspection`
//! - `GET    /repair`           ← `/api/v2/prod/batches/repair`
//! - `GET    /repairing`        ← `/api/v2/prod/batches/repairing`
//!
//! 静态 2 段：
//! - `POST   /scan/deliver`     ← `/api/v2/prod/batches/scan/deliver`
//!
//! 动态 2 段 `/{batch_id}/…`（原 `/api/v2/parts/{part_id}/…`，锚改批次）：
//! - `to-inspection` / `to-ship` / `to-process` / `scan-inspect`
//! - `deliver` / `complete` / `start-repair`
//! - `place-on-shelf` / `release-from-programming`
//! - `complete-repair` / `repair-dispatch`
//! - `cancel`
//!
//! **本域也不再有报工台端点**：`worker-scan` / `pick-up` 已于 2026-10-10 迁往
//! `prod::scan`。
//!
//! **本域不再有下发流端点**：`pending` / `dispatch` / `auto-dispatch` /
//! `recall-to-pending` 已于 2026-10-08 迁往 `prod::queue`；**也不再有外协端点**：
//! `send-to-outsource` / `receive-from-outsource` /
//! `receive-from-outsource-to-inspection` 已于 2026-10-09 合并为
//! `POST /api/v2/outsource-queue/move`（`outsource` 域）；**拆批也不再是域内路由**：
//! 2026-10-09 由 `POST /api/v2/prod/batches/{batch_id}/split` 提升为顶层共用端点
//! `POST /api/v2/batches/split`（`handler::split_router`，旧路径 404 无 alias）。
//! 三条都见 `handler/mod.rs` 的 `STRIPPED` 表。
//!
//! **本域有 2 处挂载**：本 `router()` → `/api/v2/prod/batches/*`（17 条）与
//! `prod::split_router()` → `/api/v2/batches/*`（1 条拆批）。
//!
//! ## DTO 契约
//! 子资源 13 条的 `batch_id` 自**请求体删除**（它是 URL 路径参数），其余字段不变；
//! 静态 6 条 body 完全不变。错误码语义随之变化：批次 id 全局唯一即锚点，不存在
//! 「跨 part 批次」，20109 `BIZ_PART_BATCH_NOT_FOUND` 退化为「批次不存在 / 已软删 /
//! 状态不是流转起点」；20101 `BIZ_PART_NOT_FOUND` 现在只能经由「批次的 part 已软删」
//! 触发，仍可达。
//!
//! ## 事务 / WS 广播
//! - 写端点：handler `state.pool.begin()` → service → `tx.commit()` → WS 广播
//! - 读端点（`/repair` / `/repairing`）：handler `pool.acquire()` 不开事务
//!
//! ## 角色守卫
//! - GET repair / repairing: Manager + Inspector
//! - to-XXX 三流 + 批量两流: Manager + Inspector
//! - 报工台两条端点（已迁往 `prod::scan`）：worker-scan 是 Manager + ShelfAccount，
//!   pick-up 是 Manager + Clerk + ShelfAccount

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

// 重导出 model 与 repo 的公开符号，保持外部 callers 用 `prod::batch::*` 一层路径。
pub use model::{PartBatchScanRow, RecentBatchRow};
pub use repo::{NewInitialBatch, PartBatchRepo, PartBatchRepoTrait};
pub use service::BatchService;
// 2026-10-08：`TPartBatch` 与批次状态写入口已上移到 `shared::batch`（跨域设施层）。
// 调用方一律直接 `use crate::shared::batch::…`，本模块**不留转发重导出** ——
// 转发壳会让「谁在用这层设施」在代码里看不出来，且本仓已删除过同类
// `PgIamRepo` 转发壳。

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
