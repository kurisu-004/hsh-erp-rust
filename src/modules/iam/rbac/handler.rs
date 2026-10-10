//! RBAC 子模块 HTTP handler（8 条端点）
//!
//! 挂载点：`iam::handler::router()` 的 `.nest("/", …)` 之外的 `rbac_router()`
//! 直接 merge 进 `/api/v2/iam` 根 —— 与 session 端点同层（`/iam/roles` 而非
//! `/iam/rbac/roles`）。见 [`crate::modules::iam::rbac::mod`] 的模块 doc。
//!
//! ## 权限守卫
//! 8 条**全部** `require_role(Role::Manager)`，不走权限点。理由见
//! [`crate::modules::iam::rbac::mod`]：「这个面板自己就是权限真源，用权限点
//! 守它会自锁」—— 管理员一旦把「进入面板」这个权限点从自己身上去掉，就再也
//! 没人能把权限点加回去。ADMIN 用户由 `has_role(Admin) → 放行` 天然通过。
//!
//! 守卫放在 service 层（与 `iam` 域其余端点同款），handler 不重复校验。
//!
//! ## 事务三形态
//! - 读端点（list_roles / get_role_grants / list_permission_groups /
//!   list_menus）：`pool.acquire()` 不开事务；
//! - 写端点：`pool.begin()` → service → `tx.commit()`；
//! - **踢下线一律在 commit 之后**（Redis 不在 DB 事务内，先踢会在事务回滚时
//!   把仍持有旧授权的用户误踢）。这一条对本模块的 5 个写端点都成立。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use crate::auth::rbac::CurrentUser;
use crate::modules::iam::rbac::dto::{
    RoleCreateRequest, RoleDeactivateRequest, RoleGrantsRequest, RoleUpdateRequest,
};
use crate::modules::iam::rbac::service::KickList;
use crate::modules::iam::rbac::vo::{PermissionGroup, RoleDeactivateOut, RoleDef, RoleGrants};
use crate::modules::iam::vo::MenuNodeOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// 读端点（acquire，不开事务）
// ---------------------------------------------------------------------------

/// `GET /api/v2/iam/roles` —— 角色列表（含在册用户数）。
pub async fn list_roles(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<Vec<RoleDef>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state.rbac_service.list_roles(&mut conn, &current).await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/iam/roles/{id}/grants` —— 授权整包。ADMIN 返全量。
pub async fn get_role_grants(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<R<RoleGrants>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .rbac_service
        .get_role_grants(&mut conn, id, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/iam/permissions` —— 权限点清单（按域分组）。
pub async fn list_permissions(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<Vec<PermissionGroup>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state
        .rbac_service
        .list_permission_groups(&mut conn, &current)
        .await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/iam/menus` —— 菜单树，与 `GET /iam/me` 的 `menus` 同形。
pub async fn list_menus(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
) -> Result<Json<R<Vec<MenuNodeOut>>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = state.rbac_service.list_menus(&mut conn, &current).await?;
    Ok(Json(R::ok(out)))
}

// ---------------------------------------------------------------------------
// 写端点（begin → service → commit → 踢下线）
// ---------------------------------------------------------------------------

/// `POST /api/v2/iam/roles` → 201 —— 新建自定义角色，不踢任何人。
pub async fn create_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<RoleCreateRequest>,
) -> Result<(StatusCode, Json<R<RoleDef>>), AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .rbac_service
        .create_role(&mut tx, &req, &current)
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `POST /api/v2/iam/roles/{id}/update` —— 改名 / 改描述，提交后踢该角色持有者。
///
/// 改 code 时同事务级联三张关联表（service 内完成）。
pub async fn update_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<RoleUpdateRequest>,
) -> Result<Json<R<RoleDef>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let (out, kicks) = state
        .rbac_service
        .update_role(&mut tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    kick_sessions(&state, &kicks, "update_role").await;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/iam/roles/{id}/deactivate` —— 停用 / 软删。
///
/// 护栏场景（ADMIN 会导致管理员数归零）在此之前就以 400 返回，
/// `kicks` 为空 ⇒ 不会触发任何 `delete_all_user_sessions`。
pub async fn deactivate_role(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<RoleDeactivateRequest>,
) -> Result<Json<R<RoleDeactivateOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let (out, kicks) = state
        .rbac_service
        .deactivate_role(&mut tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    kick_sessions(&state, &kicks, "deactivate_role").await;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/iam/roles/{id}/grants` —— 授权整包替换，提交后踢全部持有者。
pub async fn set_role_grants(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<RoleGrantsRequest>,
) -> Result<Json<R<RoleGrants>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let (out, kicks) = state
        .rbac_service
        .set_role_grants(&mut tx, id, &req, &current)
        .await?;
    tx.commit().await?;
    kick_sessions(&state, &kicks, "set_role_grants").await;
    Ok(Json(R::ok(out)))
}

/// 踢下线统一出口（best-effort：单个失败只 warn，不回滚已提交的写）。
///
/// ⚠️ **必须在 `tx.commit()` 之后调** —— Redis 不在 DB 事务里。
async fn kick_sessions(state: &Arc<AppState>, kicks: &KickList, ctx: &str) {
    for user_id in kicks {
        if let Err(e) = state.session.delete_all_user_sessions(*user_id).await {
            tracing::warn!(error = %e, user_id, ctx, "RBAC 写后踢下线失败");
        }
    }
}

/// 本模块路由表（8 条），由 `iam::handler::router()` merge 进 `/api/v2/iam`。
///
/// 路由注册顺序：`/{id}` catch-all 排在三条**同段静态路由**之后
/// （axum matchit 0.8 按注册顺序消歧，见 `iam::handler::router` 的说明）。
/// 本表的 8 条里静态段是 `/roles` / `/permissions` / `/menus`（1 段），
/// catch-all 是 `/{id}`（1 段）—— 顺序上**有约束**，故静态三条在前。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/permissions", get(list_permissions))
        .route("/menus", get(list_menus))
        .route("/roles", get(list_roles).post(create_role))
        .route("/roles/{id}/update", post(update_role))
        .route("/roles/{id}/deactivate", post(deactivate_role))
        .route(
            "/roles/{id}/grants",
            get(get_role_grants).post(set_role_grants),
        )
}
