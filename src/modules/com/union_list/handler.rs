//! com::union_list 域 HTTP handler（2026-09-29 新增）
//!
//! 对应 plan §4：跨表合并视图端点 `GET /api/v2/com/union-list`。
//!
//! ## 事务边界
//! 单一 GET 端点：不开事务（`pool.acquire()`），service 借 `&mut conn` 执行查
//! 询后归还。
//!
//! ## 权限
//! 4 角色全开放（Manager / Clerk / Inspector / CncProgrammer），与
//! `part/handler/crud.rs::LIST_PART_ROLES` 完全一致；本端点引用同形常量
//! `LIST_UNION_ROLES`（**字面值独立维护**；如 part 域角色表变更需同步）。
//!
//! ## 响应
//! `PartListOut`（与 `/api/v2/parts` 同形 VO；见 `super::vo::PartListOut`），
//! 每行 `row_type` 字段标 `"PART"` / `"ASSEMBLY"`。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::com::union_list::dto::UnionListQuery;
use crate::modules::com::union_list::service::crud::UnionListService;
use crate::modules::com::union_list::vo::PartListOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// 列表端点允许角色：4 角色全开放，与 `part/handler/crud.rs::LIST_PART_ROLES` 同形。
///
/// 字面值独立维护；如 part 域角色表变更需同步。
const LIST_UNION_ROLES: &[Role] = &[
    Role::Manager,
    Role::Clerk,
    Role::Inspector,
    Role::CncProgrammer,
];

/// `GET /api/v2/com/union-list` —— 跨表合并视图（part UNION assembly）。
///
/// 权限：`LIST_UNION_ROLES`（Manager / Clerk / Inspector / CncProgrammer）。
/// query：`UnionListQuery`（含 `row_type=ALL|PART|ASSEMBLY` 必传语义）。
/// 业务流转：纯读；不开事务。
/// 响应：`R<PartListOut>` —— 与 `/api/v2/parts` 同形 VO。
pub async fn list_union_items(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<UnionListQuery>,
) -> Result<Json<R<PartListOut>>, AppError> {
    current.require_any_role(LIST_UNION_ROLES)?;
    let mut conn = state.pool.acquire().await?;
    let out = UnionListService::list_union_items(&mut conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// 本域路由表（挂载点 `/api/v2/com/union-list`，见 `modules::com::router`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/", get(list_union_items))
}
