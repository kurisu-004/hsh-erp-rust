//! `prod::scan` HTTP handler 汇总 + 报工台路由的注册表。
//!
//! ## 子文件
//! - `badge.rs` —— 扫工牌（`POST /scan/verify-badge`）
//! - `listing.rs` —— 两条只读聚合端点（`GET /scan/pickable` / `GET /scan/held`）
//! - `transition.rs` —— 放回 / 送检（`POST /scan/worker-scan`）+ 手动 pick-up
//!   （`POST /scan/batches/{batch_id}/pick-up`）
//!
//! ## 事务边界
//! 写端点统一在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! 两条 list 端点是只读的，走 `pool.acquire()` 不开事务。
//! 统一响应信封：`Result<Json<R<T>>, AppError>`。权限在 handler 与 service 双层守卫
//! （`verify-badge` 例外：任意已登录用户可调，只有 `CurrentUser` extractor 一层）。

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

pub mod badge;
pub mod listing;
pub mod transition;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ---- 1 段静态（无 Path extractor）----
        .route("/verify-badge", post(badge::verify_badge))
        .route("/worker-scan", post(transition::worker_scan))
        .route("/pickable", get(listing::list_pickable))
        .route("/held", get(listing::list_held))
        // ---- 2 段：首段静态 `batches` + 动态 `{batch_id}` ----
        // ⚠️ 两组段数相同，靠 matchit 的静态段优先规则消解（静态注册在前即可，
        // 实测 `POST /scan/batches/pick-up` 不会被 `{batch_id}` 吞掉）。
        // 切勿把 `/batches/{batch_id}/…` 移到静态组之前。
        .route("/batches/{batch_id}/pick-up", post(transition::pick_up))
}
