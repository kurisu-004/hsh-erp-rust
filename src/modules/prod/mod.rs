//! prod 域（生产调度：工人 / 工种 / 工序 / 工艺链 / 工人池 / 待下发批次）
//!
//! 2026-09-19 prod 模块聚合：把 worker + work_type + process + process_chain +
//! worker_pool 五个支撑域平移至 `prod` 下，URL 一并迁移到 `/api/v2/prod/*`。
//!
//! 路由风格保持 com 容器模式：各子域各自定义独立 `router()`，prod 模块做 nest。
//!
//! 2026-09-29 新增 `prod::batch` 子模块（车间 PENDING 批次列表 + 一次 / 多次 / 自动
//! 下发 4 端点，URL 挂 `/api/v2/prod/batches/*`），复用既有 `t_shelf_process` 解析货架，
//! 零 schema 变更。
//!
//! 2026-09-30 prod 域 9 端点重构（worker-pool + batches 合并）：
//! - worker-pool → pool 路径收敛：原 `/worker-pool` + `/admin/worker-pool` 双 nest
//!   合并为单一 `/pool` nest，5 个端点全部挂 `/api/v2/prod/pool/*`（`/state`、
//!   `/counts`、`/{process_id}`、`/refill`、`/move`、`/auto-allocate`）。
//! - 旧 `/admin/worker-pool/{remove,assign}` 路径 404（前端调用统一走 `/pool/move`）。
//! - batches 端点合并：原 `/batches/dispatch`（单条）+ `/batches/bulk-dispatch`（批量）
//!   合并为单一 bulk-only `/batches/dispatch`，原 `/bulk-dispatch` 路径 404。
//!
//! 2026-10-01 新增 `prod::programming` 子模块（待编程一览，1 端点，URL 挂
//! `/api/v2/prod/programming/pending`）：前端「待编程一览」页从 part 域
//! `GET /api/v2/parts/pending-programming` 切过来。谓词按三规则并集（part 级去重）：
//! ① `p.status = 'PROGRAMMING'` 兼容旧筛选 ② 工单工艺链含 `is_cnc` 工序
//! ③ 批次 `current_process_id` 指向 `is_cnc` 工序（migration 004 确立的唯一
//! 权威列）。part 域旧端点**保留兼容、一行未改**，仅追加弃用文档说明。
//!
//! part / assembly 是全仓库核心实体（生产只是其生命周期一段），不进 prod。
//! 报工端点（worker-scan / pick-up / to-* / complete）留在 part 域；
//! 文档层「生产全流程端点地图」在 `docs/api/production/index.md` 兜底串联。
//!
//! URL 硬切换（无 alias）：前端配套 PR 锁步迁移。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod batch;
pub mod process;
pub mod process_chain;
pub mod programming;
pub mod work_type;
pub mod worker;
pub mod worker_pool;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/workers", worker::router())
        .nest("/work-types", work_type::router())
        .nest("/processes", process::router())
        .nest("/process-chains", process_chain::router())
        // 2026-09-30 重构：worker-pool → pool 路径收敛，原双 nest（/worker-pool +
        // /admin/worker-pool）合并为单一 /pool nest。
        .nest("/pool", worker_pool::router())
        // 2026-09-29 新增：prod::batch（PENDING 批次 + 下发）
        .nest("/batches", batch::router())
        // 2026-10-01 新增：prod::programming（待编程一览，三规则并集口径）
        .nest("/programming", programming::router())
}
