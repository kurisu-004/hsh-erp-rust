//! prod::batch 子模块 —— `t_part_batch` 域（生产执行单元 = 批次）
//!
//! 2026-10-02 域迁移：`t_part_batch` 的 repo / model / 状态机写入口、**全部以批次
//! 为对象的服务用例**与批次路由（URL 锚由 `part_id` 改为 `batch_id`）整体
//! 从 part 域搬入本模块，路由从 `POST /api/v2/parts/…` 硬切到
//! `POST /api/v2/prod/batches/…`（**无 alias**，前端配套 PR 锁步迁移）。现共
//! **27 条**路由（24 条批次路由 + 本域原有 3 条下发端点），逐条见下方路由表。
//!
//! part 域自此只保留「多批次动作 + part 级动作」：`/{part_id}/cancel`（BATCH-N）、
//! `/{part_id}/force-complete`（BATCH-N）、`/{part_id}/soft-delete`、
//! `GET /{part_id}/batches` 与全部 CRUD / 文件 / Excel 工具 / 各类 list 端点。
//!
//! 依赖方向：prod → part 单向（本模块引用 `PartOut` / `PartRepoTrait` /
//! `part::statemachine` 等 part 域实体）；但 `prod::batch::service` 的批次方法经
//! part 域 `PartRepoTrait` 的默认体回调本模块 `PartBatchRepo` + `status_gate`，
//! **构成反向依赖，尚未单向**。part → prod 方向另有 `status_gate` +
//! `PartBatchRepo` 两处数据依赖。过渡期成因与收敛步骤见
//! [`service`] 模块 doc。
//!
//! ## 模块结构
//! - `model.rs` —— `TPartBatch` / `RecentBatchRow` / `PartBatchScanRow` 行结构
//!   + `current_process_id` / `current_process_step_id` 的**读取方分工**清单
//! - `status_gate.rs` —— **全仓唯一** `t_part_batch.status` 写入口（写 + batch →
//!   part → assembly 派生焊在一个函数里）
//! - `repo/queries.rs` —— ZST `PartBatchRepo` + 通用 SQL 静态方法
//! - `repo/sql.rs` —— inspection / lifecycle 流转的定位 + 写点
//! - `repo/trait.rs` —— 胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`
//! - `repo/mod.rs` —— ZST `BatchRepo`：「PENDING 批次下发给车间」专用查询
//! - `service/` —— 全部业务用例（`impl BatchService`，按流拆文件，见该目录 mod doc）
//! - `dto.rs` —— 全部入参（`Deserialize`）
//! - `vo.rs` —— 全部出参（`Serialize`）
//! - `handler/{dispatch,transition,lifecycle}.rs` —— HTTP 路由 + 角色守卫 + WS 广播
//!
//! ## 2026-10-07 迁出：待品检队列读
//! `GET /api/v2/prod/batches/inspection`（+ 它的 `dto` / `vo` / `model` 行结构 /
//! `repo/list.rs` / `service/list.rs`）整体迁往 `prod::inspection`，新路径
//! `GET /api/v2/prod/inspection/queue`，**无 alias**。理由：该页面的两个数据源
//! （队列列表 + 扫码树）本就同属一个页面，迁后 `prod::inspection` 零跨域依赖、
//! 可被域隔离护栏完整覆盖。**本域不再持有任何集合读 SQL**：`/repair` /
//! `/repairing` 两条集合读在 service 层直接构造 VO。
//!
//! ## 路由表（24 条 + 本域原有 3 条）
//!
//! 静态 1 段（原 `/api/v2/parts/…`，**无 Path extractor**）：
//! - `POST   /to-ship`          ← `/api/v2/prod/batches/to-ship`
//! - `POST   /to-inspection`    ← `/api/v2/prod/batches/to-inspection`
//! - `POST   /worker-scan`      ← `/api/v2/prod/batches/worker-scan`
//! - `GET    /repair`           ← `/api/v2/prod/batches/repair`
//! - `GET    /repairing`        ← `/api/v2/prod/batches/repairing`
//! - `GET    /pending` / `POST /dispatch` / `POST /auto-dispatch`（本域原有，不动）
//!
//! 静态 2 段：
//! - `POST   /scan/deliver`     ← `/api/v2/prod/batches/scan/deliver`
//!
//! 动态 2 段 `/{batch_id}/…`（原 `/api/v2/parts/{part_id}/…`，锚改批次）：
//! - `to-inspection` / `to-ship` / `to-process` / `scan-inspect`
//! - `deliver` / `complete` / `start-repair`
//! - `place-on-shelf` / `recall-to-pending` / `release-from-programming`
//! - `send-to-outsource` / `receive-from-outsource` /
//!   `receive-from-outsource-to-inspection`
//! - `complete-repair` / `repair-dispatch`
//! - `split` / `cancel` / `pick-up`
//!
//! ## DTO 契约
//! 子资源 18 条的 `batch_id` 自**请求体删除**（它是 URL 路径参数），其余字段不变；
//! 静态 3 条 body 完全不变。错误码语义随之变化：批次 id 全局唯一即锚点，不存在
//! 「跨 part 批次」，20109 `BIZ_PART_BATCH_NOT_FOUND` 退化为「批次不存在 / 已软删 /
//! 状态不是流转起点」；20101 `BIZ_PART_NOT_FOUND` 现在只能经由「批次的 part 已软删」
//! 触发，仍可达。
//!
//! ## 事务 / WS 广播
//! - 写端点：handler `state.pool.begin()` → service → `tx.commit()` → WS 广播
//! - 读端点（pending / repair / repairing）：handler `pool.acquire()` 不开事务
//! - 只读端点（auto-dispatch）：`pool.acquire()` 不开事务，**不发** WS 广播
//!
//! ## 角色守卫
//! - GET pending: Manager + Clerk + Inspector
//! - GET repair / repairing: Manager + Inspector
//! - POST dispatch / auto-dispatch: Manager + Clerk
//! - to-XXX 三流 + 批量两流: Manager + Inspector
//! - worker-scan: Manager + ShelfAccount

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod status_gate;
pub mod vo;

// 重导出 model 与 repo 的公开符号，保持外部 callers 用 `prod::batch::*` 一层路径。
pub use model::{PartBatchScanRow, RecentBatchRow, TPartBatch};
pub use repo::{NewInitialBatch, PartBatchRepo, PartBatchRepoTrait};
pub use service::BatchService;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
