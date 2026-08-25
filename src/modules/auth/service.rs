//! auth 域业务逻辑
//!
//! 对应 Python myERP/service/auth_service.py。
//! - login：username 归一化、bcrypt 校验、角色/shelf 范围解析、签发双 token、`last_login_at` 戳更新
//! - refresh：decode refresh → 校验版本 → 轮转 `refresh_token_version` → 重签双 token
//! - me：从 DB 重读当前用户 + 角色 + shelf 范围 + 菜单，返回最新视图
//! - change_password：自助改密复用 user 域的 `change_own_password`
//!
//! 实施约定：
//! - login / refresh / change_password 接收 `&mut PgConnection`（写事务），由 handler 开 tx 并 commit
//! - me 因为只读，可走 pool-acquired conn；为统一签名也接收 `&mut PgConnection`

use std::sync::Arc;

use sqlx::PgConnection;

use crate::auth::jwt::{decode_refresh, issue_token_pair};
use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{hash_token, CachedCurrentUser, TokenKind};
use crate::infra::clock::now_naive;
use crate::modules::user::dto::{ChangePasswordRequest, CurrentUserOut};
use crate::modules::user::repo::{ShelfRepo, UserRepo, UserRoleRepo, UserRoleRow};
use crate::modules::user::service::{role_as_str, UserService};
use crate::shared::error::{code, AppError};
use crate::state::AppState;

use super::dto::{LoginRequest, LoginResponse, RefreshRequest};

/// SHELF_ACCOUNT 角色唯一合法的 scope_type
const SCOPE_TYPE_SHELF: &str = "shelf";

/// 可绑定 SHELF_ACCOUNT 的货架分区白名单（与 user 域 `validate_role_scope` 对齐）
const ALLOWED_SHELF_ZONES: [&str; 2] = ["PRODUCTION", "INSPECTION"];

pub struct AuthService;

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
    pub async fn login(
        conn: &mut PgConnection,
        req: LoginRequest,
        state: &Arc<AppState>,
    ) -> Result<LoginResponse, AppError> {
        // 1. username 归一化（对齐 Python `.strip().lower()`）
        let username_lower = req.username.trim().to_lowercase();

        // 2. 查用户（已过滤软删）+ 校验 active
        let u = UserRepo::get_by_username(&mut *conn, &username_lower)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"))?;
        if !u.is_active {
            return Err(AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"));
        }

        // 3. bcrypt 校验
        if !password::verify(&req.password, &u.password_hash)? {
            return Err(AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误"));
        }

        // 4. 角色列表（为空 → 403 NO_ROLE，统一对外不区分原因）
        let role_rows = UserRoleRepo::list_by_user(&mut *conn, u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }

        // 5. 解析角色枚举 + shelf 范围；调用用户域 helper 拼菜单
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut *conn, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = UserService::menus_for_roles(&mut *conn, &roles).await?;

        // 6. 签发双 token
        let pair = issue_token_pair(
            u.id,
            &u.username,
            &roles,
            &shelf_ids,
            shelf_wildcard,
            u.refresh_token_version,
            &state.config.jwt.secret,
            &state.config.jwt.issuer,
            state.config.jwt.access_ttl_hours,
            state.config.jwt.refresh_ttl_days,
        )?;

        // 7. 戳一下 last_login_at（不动 version，避开与并发业务更新冲突）
        UserRepo::touch_login(&mut *conn, u.id, now_naive()).await?;

        // 8. 服务端 session 入库（Redis）：
        //    tx 已 commit（handler 端），此处为 tx 外。失败应直接抛错让登录失败，
        //    否则用户拿 token 但下次 `/me` 立刻 40105 —— 比显式报错更迷惑。
        let cached = CachedCurrentUser {
            id: u.id,
            username: u.username.clone(),
            roles: roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
            shelf_ids: shelf_ids.clone(),
            shelf_wildcard,
        };
        let ttl = state.config.redis.session_ttl_seconds;
        state
            .session
            .create_session(
                &hash_token(&pair.access_token),
                u.id,
                TokenKind::Access,
                ttl,
                &cached,
            )
            .await?;
        state
            .session
            .create_session(
                &hash_token(&pair.refresh_token),
                u.id,
                TokenKind::Refresh,
                ttl,
                &cached,
            )
            .await?;

        // 9. 组装 CurrentUserOut（直接拼，不绕 user helper，避免 jwt 里 stale 数据回流到 /me）
        let user_out = build_current_user_out(&u, &roles, &shelf_ids, menus);

        Ok(LoginResponse {
            token: pair.access_token,
            refresh_token: pair.refresh_token,
            user: user_out,
        })
    }

    pub async fn refresh(
        conn: &mut PgConnection,
        req: RefreshRequest,
        state: &Arc<AppState>,
    ) -> Result<LoginResponse, AppError> {
        // 1. 解码 refresh token，取 sub + ver
        let (sub, ver) = decode_refresh(
            &req.refresh_token,
            &state.config.jwt.secret,
            &state.config.jwt.issuer,
        )
        .map_err(|_| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;

        // 2. 查用户 + 校验 active + 校验版本号匹配
        let u = UserRepo::get_by_id(&mut *conn, sub)
            .await?
            .ok_or_else(|| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;
        if !u.is_active {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }
        if u.refresh_token_version != ver {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }

        // 3. 取角色 + shelf 范围 + 菜单（与 login 同样的解析）
        let role_rows = UserRoleRepo::list_by_user(&mut *conn, u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut *conn, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = UserService::menus_for_roles(&mut *conn, &roles).await?;

        // 4. 轮转 refresh_token_version（带乐观锁；0 行 → 409）
        let user_id = u.id;
        let user_version = u.version;
        let affected = UserRepo::increment_refresh_token_version(
            &mut *conn,
            user_id,
            user_version,
            now_naive(),
            Some(user_id),
        )
        .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 5. 拿轮转后的 ver 重新签发（重读 DB 取 +1 后的新版本）
        let u = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| AppError::biz(code::REFRESH_INVALID, "user disappeared"))?;

        let pair = issue_token_pair(
            u.id,
            &u.username,
            &roles,
            &shelf_ids,
            shelf_wildcard,
            u.refresh_token_version,
            &state.config.jwt.secret,
            &state.config.jwt.issuer,
            state.config.jwt.access_ttl_hours,
            state.config.jwt.refresh_ttl_days,
        )?;

        // 6. 旧 refresh 的 Redis session 删除（best-effort）+ 新一对 token 写 session
        let old_refresh_hash = hash_token(&req.refresh_token);
        if let Err(e) = state.session.delete_session(&old_refresh_hash).await {
            tracing::warn!(error = %e, user_id = u.id, "refresh: 删旧 refresh session 失败");
        }
        let cached = CachedCurrentUser {
            id: u.id,
            username: u.username.clone(),
            roles: roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
            shelf_ids: shelf_ids.clone(),
            shelf_wildcard,
        };
        let ttl = state.config.redis.session_ttl_seconds;
        state
            .session
            .create_session(
                &hash_token(&pair.access_token),
                u.id,
                TokenKind::Access,
                ttl,
                &cached,
            )
            .await?;
        state
            .session
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
        conn: &mut PgConnection,
        current: &CurrentUser,
        _state: &Arc<AppState>,
    ) -> Result<CurrentUserOut, AppError> {
        // 1. 重读用户（handle 被外部停用/软删的极端情况）→ 不存在/已删 → UNAUTHORIZED
        let u = UserRepo::get_by_id(&mut *conn, current.id)
            .await?
            .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"))?;
        if !u.is_active {
            return Err(AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"));
        }

        // 2. 重查角色 + shelf 范围 + 菜单（不走 JWT 里的 stale 数据）
        let role_rows = UserRoleRepo::list_by_user(&mut *conn, u.id).await?;
        let (roles, shelf_ids, _wildcard) =
            resolve_roles_and_scope(&mut *conn, &role_rows).await?;
        let menus = UserService::menus_for_roles(&mut *conn, &roles).await?;

        Ok(build_current_user_out(&u, &roles, &shelf_ids, menus))
    }

    pub async fn change_password(
        conn: &mut PgConnection,
        user_id: i64,
        req: ChangePasswordRequest,
        current: &CurrentUser,
        state: &Arc<AppState>,
    ) -> Result<(), AppError> {
        // 权限：本人或 MANAGER；与 user_service.change_own_password 内部校验重复，
        // 显式提一处以便在 service 入口给出明确语义。
        if user_id != current.id && !current.has_role(Role::Manager) {
            return Err(AppError::biz(code::FORBIDDEN, "只能修改本人密码"));
        }
        UserService::change_own_password(
            conn,
            user_id,
            &req.old_password,
            &req.new_password,
            current,
            state,
        )
        .await
    }

    /// 登出当前 token：删 Redis session 条目，使后续 `/me` 立即返回 40105。
    /// 注意 — 此函数不依赖 tx，service 入口在 handler 处负责提交（此处没有 DB 写）。
    pub async fn logout(state: &Arc<AppState>, token_hash: &str) -> Result<(), AppError> {
        state.session.delete_session(token_hash).await
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
    conn: &mut PgConnection,
    rows: &[UserRoleRow],
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
                    let shelf = ShelfRepo::get_by_id(&mut *conn, sid).await?;
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
