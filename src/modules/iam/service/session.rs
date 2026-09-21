//! iam 域会话 / 登录 / refresh / 改密 service
//!
//! 对应 Python myERP/service/auth_service.py。
//! - login：username 归一化、bcrypt 校验、角色/shelf 范围解析、签发双 token、`last_login_at` 戳更新
//! - refresh：decode refresh → 校验版本 → 轮转 `refresh_token_version` → 重签双 token
//! - me：从 DB 重读当前用户 + 角色 + shelf 范围 + 菜单，返回最新视图
//! - change_password：自助改密复用 `AccountService::change_own_password`
//!
//! ## 实施约定（2026-09-21 事务分层重构）
//! - `SessionService` 持 `Arc<AppConfig>` + `Arc<dyn SessionStore>` + `Arc<AccountService>`
//!   （`uow_provider` 字段移除）；所有方法 `&self`。
//! - 事务移交 handler：`login` / `refresh` 拆两阶段——
//!   1. `login<R>(&self, repo: &mut R, req) -> LoginPending`：业务逻辑 + DB 写，
//!      handler 拿到结果后 commit。
//!   2. `complete_login(&self, pending: LoginPending) -> LoginResponse`：写 Redis session +
//!      组装响应，commit 之后做（plan v4 §3 V6 约定）。
//!
//!   refresh 同。
//! - 读端点（me）：方法签名 `me<R>(&self, repo: &mut R, current)`；handler 仍 `pool.acquire()`
//!   不开事务，service 内 `repo.xxx()` 在一次性连接上执行，读完 drop 即可。
//! - 不 begin 端点：
//!   - change_password：纯委托给 `self.account_service.change_own_password(repo, ...)`。
//!   - logout：无 DB 操作，只清 Redis session。
//! - helper `resolve_roles_and_scope` 收 `&mut R: IamRepo`，与 `IamRepo` trait 方法一致。
//!
//! 2026-09-19 IAM 域合并：`AuthService` → `SessionService`，改密用 `AccountService`
//! 取代原 `UserService`。

use std::sync::Arc;

use crate::auth::jwt::{TokenPair, decode_refresh, issue_token_pair};
use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{CachedCurrentUser, SessionStore, TokenKind, hash_token};
use crate::infra::clock::now_naive;
use crate::infra::config::AppConfig;
use crate::modules::iam::dto::{ChangePasswordRequest, CurrentUserOut, LoginResponse, MenuNodeOut};
use crate::modules::iam::model::User;
use crate::modules::iam::repo::{IamRepo, UserRoleRow};
use crate::modules::iam::service::account::{AccountService, role_as_str};
use crate::shared::error::{AppError, code};

use super::super::dto::{LoginRequest, RefreshRequest};

/// SHELF_ACCOUNT 角色唯一合法的 scope_type
const SCOPE_TYPE_SHELF: &str = "shelf";

/// 可绑定 SHELF_ACCOUNT 的货架分区白名单（与 account 域 `validate_role_scope` 对齐）
const ALLOWED_SHELF_ZONES: [&str; 2] = ["PRODUCTION", "INSPECTION"];

/// login 第一阶段产出：DB 操作结果 + 待签 token + 用户视图素材。
/// handler 拿到后 commit，然后调 `complete_login` 写 Redis + 组装响应。
#[derive(Debug)]
pub struct LoginPending {
    pub pair: TokenPair,
    pub user: User,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
    pub menus: Vec<MenuNodeOut>,
}

/// refresh 第一阶段产出：DB 操作结果 + 待签 token + 用户视图素材 + 旧 refresh hash。
/// handler 拿到后 commit，然后调 `complete_refresh` 删旧 session + 写新 + 组装响应。
#[derive(Debug)]
pub struct RefreshPending {
    pub pair: TokenPair,
    pub user: User,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
    pub menus: Vec<MenuNodeOut>,
    pub old_refresh_hash: String,
}

/// iam 域会话 service。构造时注入 `Arc<AppConfig>`（JWT / Redis TTL 配置）+
/// `Arc<dyn SessionStore>`（服务端 session）+ `Arc<AccountService>`（跨域委托）。
///
/// 2026-09-21 重构：移除 `Arc<dyn IamUowProvider>` 字段——事务由 handler 管，service
/// 不知事务边界。
pub struct SessionService {
    config: Arc<AppConfig>,
    session: Arc<dyn SessionStore>,
    account_service: Arc<AccountService>,
}

/// 把 DB 中的 role 字符串转回 `Role` 枚举。
///
/// `UserRoleRow.role` 是数据库返回的 varchar，已由 seed 数据保证只含 5 种已知值；遇到未知值时
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

/// 重复 AccountService::change_own_password 中的乐观锁翻译，service 局部使用。
fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

impl SessionService {
    /// 构造。
    pub fn new(
        config: Arc<AppConfig>,
        session: Arc<dyn SessionStore>,
        account_service: Arc<AccountService>,
    ) -> Self {
        Self {
            config,
            session,
            account_service,
        }
    }

    // =======================================================================
    // login：两阶段（DB 在第一阶段 commit，Redis 写在第二阶段 commit 后）
    // =======================================================================

    /// login 第一阶段：DB 操作（用户查 / 角色 / 菜单 / touch_login），返回待签 token 配对
    /// 与菜单视图。**不** commit、**不**写 Redis——由 handler commit 后调 `complete_login`。
    pub async fn login<R: IamRepo>(
        &self,
        mut repo: R,
        req: LoginRequest,
    ) -> Result<LoginPending, AppError> {
        // 1. username 归一化（对齐 Python `.strip().lower()`）
        let username_lower = req.username.trim().to_lowercase();

        // 2. 查用户（已过滤软删）+ 校验 active
        let u = repo
            .get_by_username(&username_lower)
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
        let role_rows = repo.list_by_user(u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }

        // 5. 解析角色枚举 + shelf 范围；委托 account_service 取菜单
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut repo, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = self.account_service.menus_for_roles(&mut repo, &roles).await?;

        // 6. 签发双 token
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

        // 7. 戳一下 last_login_at（不动 version，避开与并发业务更新冲突）
        repo.touch_login(u.id, now_naive()).await?;

        Ok(LoginPending {
            pair,
            user: u,
            roles,
            shelf_ids,
            shelf_wildcard,
            menus,
        })
    }

    /// login 第二阶段（commit 之后）：写 Redis session + 组装响应。
    pub async fn complete_login(&self, pending: LoginPending) -> Result<LoginResponse, AppError> {
        let LoginPending {
            pair,
            user: u,
            roles,
            shelf_ids,
            shelf_wildcard,
            menus,
        } = pending;

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

    // =======================================================================
    // refresh：两阶段
    // =======================================================================

    /// refresh 第一阶段：DB 操作 + 轮转 refresh_token_version + 签发新 token，
    /// 返回 `RefreshPending`（含旧 refresh hash）。**不** commit、**不**删旧 session、
    /// **不**写新 Redis session——由 handler commit 后调 `complete_refresh`。
    pub async fn refresh<R: IamRepo>(
        &self,
        mut repo: R,
        req: RefreshRequest,
    ) -> Result<RefreshPending, AppError> {
        // 1. 解码 refresh token，取 sub + ver（在 open tx 前即可拒）
        let (sub, ver) = decode_refresh(
            &req.refresh_token,
            &self.config.jwt.secret,
            &self.config.jwt.issuer,
        )
        .map_err(|_| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;

        // 2. 查用户 + 校验 active + 校验版本号匹配
        let u = repo
            .get_by_id(sub)
            .await?
            .ok_or_else(|| AppError::biz(code::REFRESH_INVALID, "refresh token 失效"))?;
        if !u.is_active {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }
        if u.refresh_token_version != ver {
            return Err(AppError::biz(code::REFRESH_INVALID, "refresh token 失效"));
        }

        // 3. 取角色 + shelf 范围 + 菜单（与 login 同样的解析）
        let role_rows = repo.list_by_user(u.id).await?;
        if role_rows.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let (roles, shelf_ids, shelf_wildcard) =
            resolve_roles_and_scope(&mut repo, &role_rows).await?;
        if roles.is_empty() {
            return Err(AppError::biz(code::NO_ROLE, "账号未分配角色"));
        }
        let menus = self.account_service.menus_for_roles(&mut repo, &roles).await?;

        // 4. 轮转 refresh_token_version（带乐观锁；0 行 → 409）
        let user_id = u.id;
        let user_version = u.version;
        let affected = repo
            .increment_refresh_token_version(user_id, user_version, now_naive(), Some(user_id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 5. 拿轮转后的 ver 重新签发（重读 DB 取 +1 后的新版本）
        let u = repo
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

        Ok(RefreshPending {
            pair,
            user: u,
            roles,
            shelf_ids,
            shelf_wildcard,
            menus,
            old_refresh_hash: hash_token(&req.refresh_token),
        })
    }

    /// refresh 第二阶段（commit 之后）：删旧 refresh session + 写新 access/refresh session
    /// + 组装响应。
    pub async fn complete_refresh(
        &self,
        pending: RefreshPending,
    ) -> Result<LoginResponse, AppError> {
        let RefreshPending {
            pair,
            user: u,
            roles,
            shelf_ids,
            shelf_wildcard,
            menus,
            old_refresh_hash,
        } = pending;

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

    // =======================================================================
    // me（读端点）
    // =======================================================================

    /// `/iam/me`：从 DB 重读当前用户 + 角色 + shelf 范围 + 菜单。
    /// 读端点，handler `pool.acquire()` 不开事务，service 借到的 `repo` 用完即 drop。
    pub async fn me<R: IamRepo>(
        &self,
        mut repo: R,
        current: &CurrentUser,
    ) -> Result<CurrentUserOut, AppError> {
        // 1. 重读用户（handle 被外部停用/软删的极端情况）→ 不存在/已删 → UNAUTHORIZED
        let u = repo
            .get_by_id(current.id)
            .await?
            .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"))?;
        if !u.is_active {
            return Err(AppError::biz(code::UNAUTHORIZED, "用户不存在或已停用"));
        }

        // 2. 重查角色 + shelf 范围 + 菜单（不走 JWT 里的 stale 数据）
        let role_rows = repo.list_by_user(u.id).await?;
        let (roles, shelf_ids, _wildcard) =
            resolve_roles_and_scope(&mut repo, &role_rows).await?;
        let menus = self.account_service.menus_for_roles(&mut repo, &roles).await?;

        Ok(build_current_user_out(&u, &roles, &shelf_ids, menus))
    }

    // =======================================================================
    // change_password / logout（DB 委派 / 无 DB）
    // =======================================================================

    /// 自助改密：纯委托给 account_service（account_service 借传入的 repo 跑业务）。
    /// 本服务入口处的权限校验与 account_service 内部重复，显式提一处以便在 service
    /// 入口给出明确语义。
    pub async fn change_password<R: IamRepo>(
        &self,
        repo: R,
        user_id: i64,
        req: ChangePasswordRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        if user_id != current.id && !current.has_role(Role::Manager) {
            return Err(AppError::biz(code::FORBIDDEN, "只能修改本人密码"));
        }
        self.account_service
            .change_own_password(repo, user_id, &req.old_password, &req.new_password, current)
            .await
    }

    /// 登出当前 token：删 Redis session 条目，使后续 `/iam/me` 立即返回 40105。
    /// 无 DB 操作，本服务不 begin。
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
/// 规则与 account_service.validate_role_scope / shelf_repo.get_by_id 一脉相承。
async fn resolve_roles_and_scope<R: IamRepo>(
    repo: &mut R,
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
                    let shelf = repo.shelf_get_by_id(sid).await?;
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

/// 直接从 DB User + 解析后的角色/shelf 拼 `CurrentUserOut`，避免绕路 account helper（该 helper
/// 依赖 `CurrentUser` 形参的角色/shelf，会把 stale 数据带进出参）。
fn build_current_user_out(
    u: &User,
    roles: &[Role],
    shelf_ids: &[i64],
    menus: Vec<MenuNodeOut>,
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
