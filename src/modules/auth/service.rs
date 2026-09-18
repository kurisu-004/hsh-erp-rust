//! auth 域业务逻辑
//!
//! 对应 Python myERP/service/auth_service.py。
//! - login：username 归一化、bcrypt 校验、角色/shelf 范围解析、签发双 token、`last_login_at` 戳更新
//! - refresh：decode refresh → 校验版本 → 轮转 `refresh_token_version` → 重签双 token
//! - me：从 DB 重读当前用户 + 角色 + shelf 范围 + 菜单，返回最新视图
//! - change_password：自助改密复用 user 域的 `change_own_password`
//!
//! ## 实施约定（2026-09-18 auth-di 重构 Wave 2B）
//! - AuthService 持 `Arc<dyn UowProvider>` + `Arc<AppConfig>` + `Arc<dyn SessionStore>` +
//!   `Arc<UserService>`；所有方法 `&self`。
//! - 写端点（login / refresh）：内部 `self.uow_provider.begin().await?` → 业务操作 →
//!   `uow.commit().await?` → session 写/清**在 commit 之后**（plan v4 §3 V6 约定）。
//! - 读端点（me）：内部 begin 后 drop（不 commit，隐式回滚）。
//! - 不 begin 端点：
//!   - change_password：纯委托给 `self.user_service.change_own_password(...)`，
//!     user_service 内部自管 begin/commit。
//!   - logout：无 DB 操作，只清 Redis session。
//! - helper `resolve_roles_and_scope` 收 `&mut dyn UnitOfWork`（需 `shelf_repo().get_by_id(...)`），
//!   保持与 auth 在自己 begin 的 uow 同 tx。

use std::sync::Arc;

use crate::auth::jwt::{decode_refresh, issue_token_pair};
use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{hash_token, CachedCurrentUser, SessionStore, TokenKind};
use crate::infra::clock::now_naive;
use crate::infra::config::AppConfig;
use crate::modules::user::dto::{ChangePasswordRequest, CurrentUserOut};
use crate::modules::user::service::{role_as_str, UserService};
use crate::modules::user::uow::UnitOfWork;
use crate::modules::user::uow::UowProvider;
use crate::shared::error::{code, AppError};

use super::dto::{LoginRequest, LoginResponse, RefreshRequest};

/// SHELF_ACCOUNT 角色唯一合法的 scope_type
const SCOPE_TYPE_SHELF: &str = "shelf";

/// 可绑定 SHELF_ACCOUNT 的货架分区白名单（与 user 域 `validate_role_scope` 对齐）
const ALLOWED_SHELF_ZONES: [&str; 2] = ["PRODUCTION", "INSPECTION"];

/// auth 域服务。构造时注入 `Arc<dyn UowProvider>`（DB 事务来源）+ `Arc<AppConfig>`
/// （JWT / Redis TTL 配置）+ `Arc<dyn SessionStore>`（服务端 session）+ `Arc<UserService>`
/// （跨域委托：menus / change_password）。
pub struct AuthService {
    uow_provider: Arc<dyn UowProvider>,
    config: Arc<AppConfig>,
    session: Arc<dyn SessionStore>,
    user_service: Arc<UserService>,
}

/// 把 DB 中的 role 字符串转回 `Role` 枚举。
///
/// UserRoleRow.role 是数据库返回的 varchar，已由 seed 数据保证只含 5 种已知值；遇到未知值时
/// 记 warn 并跳过——宁可不识别也不 panic。
fn parse_role(s: &str) -> Option<Role> {
    Some(match s {
        "MANAGER" => Role::Manager,
        "CLERK" => Role::Clerk,
        "INSPECTOR" => Role::Inspector,
        "CNC_PROGRAMMER" => Role::CncProgrammer,
        "SHELF_ACCOUNT" => Role::ShelfAccount,
        _ => return None,
    })
}

/// 重复 UserService::change_own_password 中的乐观锁翻译，service 局部使用。
fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

impl AuthService {
    /// 构造。
    pub fn new(
        uow_provider: Arc<dyn UowProvider>,
        config: Arc<AppConfig>,
        session: Arc<dyn SessionStore>,
        user_service: Arc<UserService>,
    ) -> Self {
        Self {
            uow_provider,
            config,
            session,
            user_service,
        }
    }

    pub async fn login(
        &self,
        req: LoginRequest,
    ) -> Result<LoginResponse, AppError> {
        // 1. username 归一化（对齐 Python `.strip().lower()`）
        let username_lower = req.username.trim().to_lowercase();

        // 2. 开事务
        let mut uow = self.uow_provider.begin().await?;

        // 3. 查用户（已过滤软删）+ 校验 active
        let u = uow
            .user_repo()
            .get_by_username(&username_lower)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"))?;
        if !u.is_active {
            return Err(AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"));
        }

        // 4. bcrypt 校验
        if !password::verify(&req.password, &u.password_hash)? {
            return Err(AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"));
        }

        // 5. 角色列表（为空 → 403 NO_ROLE，统一对外不区分原因）
        let role_rows = uow.user_role_repo().list_by_user(u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }

        // 6. 解析角色枚举 + shelf 范围；委托 user_service 取菜单
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut *uow, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = self.user_service.menus_for_roles(&mut *uow, &roles).await?;

        // 7. 签发双 token
        let pair = issue_token_pair(
            u.id,
            &u.username,
            &roles,
            &shelf_ids,
            shelf_wildcard,
            u.refresh_token_version,
            &self.config.jwt.secret,
            &self.config.jwt.issuer,
            self.config.jwt.access_ttl_hours,
            self.config.jwt.refresh_ttl_days,
        )?;

        // 8. 戳一下 last_login_at（不动 version，避开与并发业务更新冲突）
        uow.user_repo().touch_login(u.id, now_naive()).await?;

        // 9. 提交事务；session 写在 commit 之后（plan v4 §3 V6 新约定）
        uow.commit().await?;

        let cached = CachedCurrentUser {
            id: u.id,
            username: u.username.clone(),
            roles: roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
            shelf_ids: shelf_ids.clone(),
            shelf_wildcard,
        };
        let ttl = self.config.redis.session_ttl_seconds;
        self.session
            .create_session(
                &hash_token(&pair.access_token),
                u.id,
                TokenKind::Access,
                ttl,
                &cached,
            )
            .await?;
        self.session
            .create_session(
                &hash_token(&pair.refresh_token),
                u.id,
                TokenKind::Refresh,
                ttl,
                &cached,
            )
            .await?;

        // 10. 组装 CurrentUserOut（直接拼，不绕 user helper，避免 jwt 里 stale 数据回流到 /me）
        let user_out = build_current_user_out(&u, &roles, &shelf_ids, menus);

        Ok(LoginResponse {
            token: pair.access_token,
            refresh_token: pair.refresh_token,
            user: user_out,
        })
    }

    pub async fn refresh(
        &self,
        req: RefreshRequest,
    ) -> Result<LoginResponse, AppError> {
        // 1. 解码 refresh token，取 sub + ver
        let (sub, ver) = decode_refresh(
            &req.refresh_token,
            &self.config.jwt.secret,
            &self.config.jwt.issuer,
        )
        .map_err(|_| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;

        // 2. 开事务
        let mut uow = self.uow_provider.begin().await?;

        // 3. 查用户 + 校验 active + 校验版本号匹配
        let u = uow
            .user_repo()
            .get_by_id(sub)
            .await?
            .ok_or_else(|| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;
        if !u.is_active {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }
        if u.refresh_token_version != ver {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }

        // 4. 取角色 + shelf 范围 + 菜单（与 login 同样的解析）
        let role_rows = uow.user_role_repo().list_by_user(u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut *uow, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = self.user_service.menus_for_roles(&mut *uow, &roles).await?;

        // 5. 轮转 refresh_token_version（带乐观锁；0 行 → 409）
        let user_id = u.id;
        let user_version = u.version;
        let affected = uow
            .user_repo()
            .increment_refresh_token_version(
                user_id,
                user_version,
                now_naive(),
                Some(user_id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 6. 拿轮转后的 ver 重新签发（重读 DB 取 +1 后的新版本）
        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::biz(code::REFRESH_INVALID, "user disappeared"))?;

        let pair = issue_token_pair(
            u.id,
            &u.username,
            &roles,
            &shelf_ids,
            shelf_wildcard,
            u.refresh_token_version,
            &self.config.jwt.secret,
            &self.config.jwt.issuer,
            self.config.jwt.access_ttl_hours,
            self.config.jwt.refresh_ttl_days,
        )?;

        // 7. 提交事务；session 删旧 + 写在 commit 之后
        uow.commit().await?;

        let old_refresh_hash = hash_token(&req.refresh_token);
        if let Err(e) = self.session.delete_session(&old_refresh_hash).await {
            tracing::warn!(error = %e, user_id = u.id, "refresh: 删旧 refresh session 失败");
        }
        let cached = CachedCurrentUser {
            id: u.id,
            username: u.username.clone(),
            roles: roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
            shelf_ids: shelf_ids.clone(),
            shelf_wildcard,
        };
        let ttl = self.config.redis.session_ttl_seconds;
        self.session
            .create_session(
                &hash_token(&pair.access_token),
                u.id,
                TokenKind::Access,
                ttl,
                &cached,
            )
            .await?;
        self.session
            .create_session(
                &hash_token(&pair.refresh_token),
                u.id,
                TokenKind::Refresh,
                ttl,
                &cached,
            )
            .await?;

        let user_out = build_current_user_out(&u, &roles, &shelf_ids, menus);

        Ok(LoginResponse {
            token: pair.access_token,
            refresh_token: pair.refresh_token,
            user: user_out,
        })
    }

    pub async fn me(
        &self,
        current: &CurrentUser,
    ) -> Result<CurrentUserOut, AppError> {
        // 读端点：begin 后 drop（隐式回滚，不 commit）
        let mut uow = self.uow_provider.begin().await?;

        // 1. 重读用户（handle 被外部停用/软删的极端情况）→ 不存在/已删 → UNAUTHORIZED
        let u = uow
            .user_repo()
            .get_by_id(current.id)
            .await?
            .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"))?;
        if !u.is_active {
            return Err(AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"));
        }

        // 2. 重查角色 + shelf 范围 + 菜单（不走 JWT 里的 stale 数据）
        let role_rows = uow.user_role_repo().list_by_user(u.id).await?;
        let (roles, shelf_ids, _wildcard) = resolve_roles_and_scope(&mut *uow, &role_rows).await?;
        let menus = self.user_service.menus_for_roles(&mut *uow, &roles).await?;

        Ok(build_current_user_out(&u, &roles, &shelf_ids, menus))
    }

    /// 自助改密：纯委托给 user_service（user_service 自己 begin + commit），
    /// auth 端不 begin、不 commit。入口处的权限校验与 user_service 内部重复，
    /// 显式提一处以便在 service 入口给出明确语义。
    pub async fn change_password(
        &self,
        user_id: i64,
        req: ChangePasswordRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        if user_id != current.id && !current.has_role(Role::Manager) {
            return Err(AppError::biz(code::FORBIDDEN, "只能修改本人密码"));
        }
        self.user_service
            .change_own_password(
                user_id,
                &req.old_password,
                &req.new_password,
                current,
            )
            .await
    }

    /// 登出当前 token：删 Redis session 条目，使后续 `/me` 立即返回 40105。
    /// 无 DB 操作，auth 端不 begin。
    pub async fn logout(&self, token_hash: &str) -> Result<(), AppError> {
        self.session.delete_session(token_hash).await
    }
}

// ===========================================================================
// 内部 helpers
// ===========================================================================

/// 解析 UserRole 行集合为登录态三件套：
/// 1. `Vec<Role>`：转换得到的角色枚举（顺带跳过无法识别的字符串）
/// 2. `Vec<i64>`：可访问的 shelf_id 列表（仅 SHELF_ACCOUNT 角色 + 货架 active + zone∈白名单）
/// 3. `bool`：shelf_wildcard——任意一条 SHELF_ACCOUNT 行的 scope_id 为 NULL 时为 true
///
/// 规则与 user_service.validate_role_scope / shelf_repo.get_by_id 一脉相承。
async fn resolve_roles_and_scope(
    uow: &mut dyn UnitOfWork,
    rows: &[crate::modules::user::repo::UserRoleRow],
) -> Result<(Vec<Role>, Vec<i64>, bool), AppError> {
    let mut roles: Vec<Role> = Vec::with_capacity(rows.len());
    let mut shelf_ids: Vec<i64> = Vec::new();
    let mut shelf_wildcard = false;

    for r in rows {
        let Some(role) = parse_role(&r.role) else {
            tracing::warn!(role = %r.role, user_id = r.user_id, "未知 role 字符串，跳过");
            continue;
        };
        if role == Role::ShelfAccount && r.scope_type.as_deref() == Some(SCOPE_TYPE_SHELF) {
            match r.scope_id {
                None => shelf_wildcard = true,
                Some(sid) => {
                    let shelf = uow.shelf_repo().get_by_id(sid).await?;
                    if let Some(s) = shelf
                        && s.is_active
                        && ALLOWED_SHELF_ZONES.contains(&s.zone.as_str())
                    {
                        shelf_ids.push(sid);
                    }
                }
            }
        }
        roles.push(role);
    }

    Ok((roles, shelf_ids, shelf_wildcard))
}

/// 直接从 DB User + 解析后的角色/shelf 拼 `CurrentUserOut`，避免绕路 user helper（该 helper
/// 依赖 `CurrentUser` 形参的角色/shelf，会把 stale 数据带进出参）。
fn build_current_user_out(
    u: &crate::modules::user::model::User,
    roles: &[Role],
    shelf_ids: &[i64],
    menus: Vec<crate::modules::user::dto::MenuNodeOut>,
) -> CurrentUserOut {
    CurrentUserOut {
        id: u.id,
        username: u.username.clone(),
        full_name: u.full_name.clone(),
        is_active: u.is_active,
        roles: roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
        shelf_ids: shelf_ids.iter().map(|v| v.to_string()).collect(),
        menus,
    }
}

// 2026-09-18 Wave 2 T10：30 例 mock 单测。仅在 `cargo test` 时编译。
#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
