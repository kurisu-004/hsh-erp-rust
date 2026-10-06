pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use axum::routing::get;
use std::sync::Arc;

/// `/ws/*` 入口（WebSocket）：当前唯一端点 `/ws/dashboard`
///
/// 在 `modules::ws_router()` 下挂 `/ws` 前缀（不带 `/api/v2`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(handler::ws_dashboard))
}

/// `/api/v2/dashboard/*` HTTP 端点。
///
/// 三个端点都是只读，任何已登录用户可访问（无角色闸门），故不逐个挂
/// `CurrentUser` 之外的授权层。
pub fn http_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/snapshot", get(handler::get_snapshot))
        .route("/upcoming-delivery", get(handler::get_upcoming_delivery))
        .route("/delivery-orders", get(handler::get_delivery_orders))
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「dashboard 域不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// dashboard 域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state`
    /// 与本域自身；代码区里出现任何其它域的路径即失败。
    #[test]
    fn dashboard_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "dashboard",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/dashboard"),
            "需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里只读聚合\
             （dashboard 的 5 张表见 docs/api/dashboard.md），而不是 import 别人的 service / repo。",
        );
    }
}
