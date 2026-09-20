//! prod 域（生产调度：工人 / 工种 / 工序 / 工艺链 / 工人池）
//!
//! 2026-09-19 prod 模块聚合：把 worker + work_type + process + process_chain +
//! worker_pool 五个支撑域平移至 `prod` 下，URL 一并迁移到 `/api/v2/prod/*`。
//!
//! 路由风格保持 com 容器模式：5 个子域各自定义独立 `router()`，prod 模块做 nest。
//!
//! part / assembly 是全仓库核心实体（生产只是其生命周期一段），不进 prod。
//! 报工端点（worker-scan / pick-up / to-* / complete）留在 part 域；
//! 文档层「生产全流程端点地图」在 `docs/api/production/index.md` 兜底串联。
//!
//! URL 硬切换（无 alias）：前端配套 PR 锁步迁移。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod process;
pub mod process_chain;
pub mod work_type;
pub mod worker;
pub mod worker_pool;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/workers", worker::router())
        .nest("/work-types", work_type::router())
        .nest("/processes", process::router())
        .nest("/process-chains", process_chain::router())
        .nest("/worker-pool", worker_pool::router())
        .nest("/admin/worker-pool", worker_pool::admin_router())
}
