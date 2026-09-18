//! iam 域账号管理 service（CRUD + 角色 + 改密）
//!
//! 对应 Python myERP/service/user.py。
//!
//! ## 事务边界（2026-09-18 重构 + 2026-09-19 IAM 合并）
//! Service 持 `Arc<dyn IamUowProvider>`，所有数据访问经 `uow.user_repo().xxx()` /
//! `uow.user_role_repo().xxx()` / `uow.menu_repo().xxx()` / `uow.shelf_repo().xxx()`
//! 访问器；写端点最后 `uow.commit().await?`（消费 Box），读端点直接 drop（隐式回滚）。
//! Handler 不再开 tx，完全薄壳化（见 `handler.rs`）。
//!
//! ## Session 清理
//! `change_own_password` / `admin_reset_password` 在 `uow.commit().await?` **之后**
//! 调用 `self.session.delete_all_user_sessions(user_id)`，best-effort 失败只打 warn；
//! DB 的 `refresh_token_version` 轮转是兜底。
//!
//! ## 与 Python 的错误码映射
//! Python 的 `BIZ_INVALID_VALUE = 20104` / `BIZ_SHELF_NOT_FOUND = 20501` 尚未进入
//! `shared::error::code`（本任务不改 error.rs），故：
//! - scope 用法错误（SHELF_ACCOUNT 缺 scope / 非货架角色带 scope）→ `VALIDATION_ERROR`(40001)
//! - 货架不存在 / zone 非法 / 已停用 → `NOT_FOUND`(40400)
//!   （Python 对这三种情况复用同一个 20501，仅 HTTP 状态码不同）
//!
//! 待 205xx / 201xx 段补齐后可无损替换。
//!
//! 2026-09-19 IAM 域合并：`UserService` → `AccountService`，方法签名 + 业务逻辑零 diff，
//! 仅路径变更。`menus_for_roles` / `current_user_out` 仍保留在本服务，作为
//! `SessionService`（原 AuthService）跨域委托的目标。

use std::sync::Arc;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::SessionStore;
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{AppError, code};

use super::menu::build_menu_tree;
// `iam/service/` 子目录中 dto / model / repo / uow 是 sibling 的兄弟模块 —— 用 `super::super::` 跨级
use super::super::dto::{
    CurrentUserOut, MenuNodeOut, UserAddRoleRequest, UserCreateRequest, UserListOut, UserListQuery,
    UserOut, UserRoleOut, UserUpdateRequest,
};
use super::super::model::User;
// struct 来自 repo.rs（uow.rs 只 re-export trait，不 re-export struct）
use super::super::repo::{UserInsert, UserRoleInsert, UserRoleRow};
use super::super::uow::{IamUnitOfWork, IamUowProvider};

/// 管理员重置密码时写入的默认口令（对齐 Python `DEFAULT_RESET_PASSWORD`）
pub const DEFAULT_RESET_PASSWORD: &str = "changeme";

/// SHELF_ACCOUNT 角色唯一合法的 scope_type
const SCOPE_TYPE_SHELF: &str = "shelf";

/// 可绑定 SHELF_ACCOUNT 的货架分区白名单（对齐 Python `ShelfZone`）
const ALLOWED_SHELF_ZONES: [&str; 2] = ["PRODUCTION", "INSPECTION"];

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// `Role` → DB / JSON 中的大写字符串（与 `auth::rbac::Role` 的 serde rename 保持一致）
pub fn role_as_str(role: Role) -> &'static str {
    match role {
        Role::Manager => "MANAGER",
        Role::Clerk => "CLERK",
        Role::Inspector => "INSPECTOR",
        Role::CncProgrammer => "CNC_PROGRAMMER",
        Role::ShelfAccount => "SHELF_ACCOUNT",
    }
}

fn user_not_found(user_id: i64) -> AppError {
    AppError::biz(code::USER_NOT_FOUND, format!("user {user_id} not found"))
}

/// 乐观锁写入返回 0 行 → 409。已在事务内先 SELECT 过，故 0 行只可能是并发改动。
fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// `full_name` 等文本字段：trim 后空串归一为 `None`（对齐 Python `.strip() or None`）
fn trimmed_or_none(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// iam 域账号管理 service。实例字段 = UoW 来源 + 雪花 ID + session 存储：
/// - `uow_provider` 是唯一事务入口（handler 不再开 tx）
/// - `snowflake` 由 AppState 注入（与 Python `SNOWFLAKE_INSTANCE=0` 区分：rust 用实例号 1）
/// - `session` 在 commit 后用于清该用户的 Redis session（自助改密 / 管理员重置）
pub struct AccountService {
    uow_provider: Arc<dyn IamUowProvider>,
    snowflake: Arc<SnowflakeIdGenerator>,
    session: Arc<dyn SessionStore>,
}

impl AccountService {
    /// 三段装线入口：`AppState::new` 调用，注入 provider + snowflake + session。
    pub fn new(
        uow_provider: Arc<dyn IamUowProvider>,
        snowflake: Arc<SnowflakeIdGenerator>,
        session: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            uow_provider,
            snowflake,
            session,
        }
    }

    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_users(
        &self,
        query: &UserListQuery,
        current: &CurrentUser,
    ) -> Result<UserListOut, AppError> {
        current.require_role(Role::Manager)?;

        let mut uow = self.uow_provider.begin().await?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let like = query
            .username_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let rows = uow
            .user_repo()
            .list_with_filters(like, query.is_active, limit, offset)
            .await?;
        let total = uow
            .user_repo()
            .count_with_filters(like, query.is_active)
            .await?;

        let mut items = Vec::with_capacity(rows.len());
        for u in rows {
            let roles = uow.user_role_repo().list_by_user(u.id).await?;
            items.push(Self::assemble_user_out(u, roles));
        }

        Ok(UserListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_user(&self, user_id: i64, current: &CurrentUser) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = uow.user_role_repo().list_by_user(u.id).await?;
        Ok(Self::assemble_user_out(u, roles))
    }

    // =======================================================================
    // 创建 / 更新 / 停用
    // =======================================================================

    pub async fn create_user(
        &self,
        req: &UserCreateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        let username = req.username.trim().to_lowercase();
        if username.is_empty() {
            return Err(AppError::validation("username 不能为空"));
        }
        if req.password.is_empty() {
            return Err(AppError::validation("password 不能为空"));
        }

        // 显式查重（partial unique 索引仍是最终防线，见下方 INSERT 的错误映射）
        if uow.user_repo().get_by_username(&username).await?.is_some() {
            return Err(AppError::biz(
                code::DUPLICATE_USERNAME,
                format!("username '{username}' already exists"),
            ));
        }

        let full_name = req.full_name.trim();
        if full_name.is_empty() {
            return Err(AppError::validation("full_name 不能为空"));
        }

        let insert = UserInsert {
            id: self.snowflake.next_id(),
            username,
            password_hash: password::hash(&req.password)?,
            full_name: full_name.to_string(),
            phone: req.phone.as_deref().and_then(trimmed_or_none),
            is_active: true,
            created_at: now_naive(),
            created_by: Some(current.id),
        };

        uow.user_repo()
            .create(&insert)
            .await
            .map_err(map_duplicate_username)?;

        let u = uow
            .user_repo()
            .get_by_id(insert.id)
            .await?
            .ok_or_else(|| AppError::internal("创建后回读用户失败"))?;
        let roles = uow.user_role_repo().list_by_user(u.id).await?;
        let out = Self::assemble_user_out(u, roles);

        uow.commit().await?;
        Ok(out)
    }

    pub async fn update_user(
        &self,
        user_id: i64,
        req: &UserUpdateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        // full_name 若提供则必须非空（对齐 Python `min_length=1`）
        let full_name = match req.full_name.as_deref() {
            Some(v) => {
                let t = v.trim();
                if t.is_empty() {
                    return Err(AppError::validation("full_name 不能为空"));
                }
                Some(t.to_string())
            }
            None => None,
        };

        // phone 提供空串 = 显式清空（Python `.strip() or None`）
        let set_phone = req.phone.is_some();
        let phone = req.phone.as_deref().and_then(trimmed_or_none);

        // 管理员在此改密**不**轮转 refresh_token_version（对齐 Python update_user）
        let password_hash = match req.password.as_deref() {
            Some("") => return Err(AppError::validation("password 不能为空")),
            Some(p) => Some(password::hash(p)?),
            None => None,
        };

        let affected = uow
            .user_repo()
            .update_partial(
                u.id,
                u.version,
                full_name.as_deref(),
                set_phone,
                phone.as_deref(),
                password_hash.as_deref(),
                req.is_active,
                now_naive(),
                Some(current.id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = uow.user_role_repo().list_by_user(updated.id).await?;
        let out = Self::assemble_user_out(updated, roles);

        uow.commit().await?;
        Ok(out)
    }

    /// 停用账号 = 软删（置 `deleted_at` + `is_active = false`）
    pub async fn deactivate_user(
        &self,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected = uow
            .user_repo()
            .soft_delete(u.id, u.version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 软删后 get_by_id 会过滤掉该行，故用内存中的行 + 手工推进字段组装出参
        // （对齐 Python `_to_out(u, include_deleted=True)`）。
        let roles = uow.user_role_repo().list_by_user(u.id).await?;
        let now = now_naive();
        let out = Self::assemble_user_out(
            User {
                is_active: false,
                deleted_at: Some(now),
                updated_at: now,
                updated_by: Some(current.id),
                version: u.version + 1,
                ..u
            },
            roles,
        );

        uow.commit().await?;
        Ok(out)
    }

    // =======================================================================
    // 密码
    // =======================================================================

    /// 自助改密：校验旧密码，写新哈希并轮转 refresh token（同一条 UPDATE，原子）。
    ///
    /// 允许本人或 MANAGER 调用。注意与 `update_user` 的区别：这里**会**踢下线。
    /// 流程：begin uow → 校验旧密码 → UPDATE 写新密码 + 轮转 ver → `uow.commit().await?`
    /// → `self.session.delete_all_user_sessions(user_id)`（best-effort）。
    pub async fn change_own_password(
        &self,
        user_id: i64,
        old_password: &str,
        new_password: &str,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        if user_id != current.id && !current.has_role(Role::Manager) {
            return Err(AppError::biz(code::FORBIDDEN, "只能修改本人密码"));
        }
        if new_password.is_empty() {
            return Err(AppError::validation("new_password 不能为空"));
        }

        let mut uow = self.uow_provider.begin().await?;

        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        // get_by_id 已过滤 deleted_at；此处再挡停用账号（对齐 Python 的三重判断）
        if !u.is_active {
            return Err(user_not_found(user_id));
        }

        if !password::verify(old_password, &u.password_hash)? {
            return Err(AppError::biz(code::OLD_PASSWORD_MISMATCH, "旧密码不正确"));
        }

        let affected = uow
            .user_repo()
            .update_password_and_rotate(
                u.id,
                u.version,
                &password::hash(new_password)?,
                now_naive(),
                Some(current.id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        uow.commit().await?;
        // DB 提交后清该用户的 Redis session（best-effort；DB 的 refresh_token_version 轮转是兜底）
        if let Err(e) = self.session.delete_all_user_sessions(user_id).await {
            tracing::warn!(error = %e, user_id, "change_own_password: 清 session 失败");
        }
        Ok(())
    }

    /// 管理员重置密码为默认口令 `changeme`，并轮转 refresh token（踢下线）。
    pub async fn admin_reset_password(
        &self,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        let u = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected = uow
            .user_repo()
            .update_password_and_rotate(
                u.id,
                u.version,
                &password::hash(DEFAULT_RESET_PASSWORD)?,
                now_naive(),
                Some(current.id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = uow
            .user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = uow.user_role_repo().list_by_user(updated.id).await?;
        let out = Self::assemble_user_out(updated, roles);

        uow.commit().await?;
        // DB 提交后清该用户的 Redis session（best-effort）
        if let Err(e) = self.session.delete_all_user_sessions(user_id).await {
            tracing::warn!(error = %e, user_id, "admin_reset_password: 清 session 失败");
        }
        Ok(out)
    }

    // =======================================================================
    // 角色管理
    // =======================================================================

    pub async fn list_user_roles(
        &self,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<UserRoleOut>, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        uow.user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let rows = uow.user_role_repo().list_by_user(user_id).await?;
        Ok(rows.into_iter().map(to_role_out).collect())
    }

    pub async fn add_role(
        &self,
        user_id: i64,
        req: &UserAddRoleRequest,
        current: &CurrentUser,
    ) -> Result<UserRoleOut, AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        uow.user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        Self::validate_role_scope(&mut *uow, req).await?;

        let role_str = role_as_str(req.role);
        let scope_type = req.scope_type.as_deref();

        // 显式查重。Python 依赖唯一索引 + IntegrityError，但 partial unique 索引对
        // (user_id, role, NULL, NULL) 这类含 NULL 的组合不生效（SQL 里 NULL != NULL），
        // 导致非货架角色可以被重复添加。这里用 IS NOT DISTINCT FROM 显式查重堵住该缺口。
        if uow
            .user_role_repo()
            .exists_same_scope(user_id, role_str, scope_type, req.scope_id)
            .await?
        {
            return Err(AppError::biz(
                code::ROLE_DUPLICATE,
                format!(
                    "role {role_str} (scope={}/{}) already assigned to this user",
                    scope_type.unwrap_or("null"),
                    req.scope_id
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                ),
            ));
        }

        let insert = UserRoleInsert {
            id: self.snowflake.next_id(),
            user_id,
            role: role_str.to_string(),
            scope_type: scope_type.map(str::to_string),
            scope_id: req.scope_id,
            created_at: now_naive(),
            created_by: Some(current.id),
        };
        uow.user_role_repo()
            .create(&insert)
            .await
            .map_err(map_duplicate_role)?;

        let rows = uow.user_role_repo().list_by_user(user_id).await?;
        let out = rows
            .into_iter()
            .find(|r| r.id == insert.id)
            .map(to_role_out)
            .ok_or_else(|| AppError::internal("创建后回读角色失败"))?;

        uow.commit().await?;
        Ok(out)
    }

    pub async fn remove_role(
        &self,
        user_id: i64,
        role_id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        let mut uow = self.uow_provider.begin().await?;

        uow.user_repo()
            .get_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let r = uow.user_role_repo().get_by_id(role_id).await?;
        // 角色必须存在且属于该用户，否则一律 404（不泄露他人角色是否存在）
        let r = match r {
            Some(r) if r.user_id == user_id => r,
            _ => {
                return Err(AppError::biz(
                    code::ROLE_NOT_FOUND,
                    format!("role {role_id} not found for user {user_id}"),
                ));
            }
        };

        let affected = uow
            .user_role_repo()
            .soft_delete(r.id, r.version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        uow.commit().await?;
        Ok(())
    }

    // =======================================================================
    // 菜单（helper：对 SessionService（原 AuthService）`/me` 与登录响应开放）
    // =======================================================================

    /// 取角色可见菜单并组树（供 SessionService 复用）。调用方负责 `begin` + `drop` 或
    /// `commit`——helper 不自管事务边界。
    pub async fn menus_for_roles(
        &self,
        uow: &mut dyn IamUnitOfWork,
        roles: &[Role],
    ) -> Result<Vec<MenuNodeOut>, AppError> {
        let role_strs: Vec<String> = roles.iter().map(|r| role_as_str(*r).to_string()).collect();
        let menus = uow.menu_repo().list_active_for_roles(&role_strs).await?;
        Ok(build_menu_tree(menus))
    }

    /// 组装 `/iam/me` 出参（SessionService 复用）。调用方负责 `begin` + `drop` 或
    /// `commit`——helper 不自管事务边界。
    pub async fn current_user_out(
        &self,
        uow: &mut dyn IamUnitOfWork,
        current: &CurrentUser,
    ) -> Result<CurrentUserOut, AppError> {
        let u = uow
            .user_repo()
            .get_by_id(current.id)
            .await?
            .ok_or_else(|| user_not_found(current.id))?;
        let menus = self.menus_for_roles(uow, &current.roles).await?;
        Ok(CurrentUserOut {
            id: u.id,
            username: u.username,
            full_name: u.full_name,
            is_active: u.is_active,
            roles: current
                .roles
                .iter()
                .map(|r| role_as_str(*r).to_string())
                .collect(),
            shelf_ids: current.shelf_ids.iter().map(|v| v.to_string()).collect(),
            menus,
        })
    }

    // =======================================================================
    // 内部
    // =======================================================================

    /// 校验 SHELF_ACCOUNT 角色的 scope 形态、货架存在、zone 白名单、is_active。
    /// 收 `&mut dyn IamUnitOfWork` 而非 `&mut uow`：调用方把 UoW 借进来，helper 只取 shelf_repo。
    async fn validate_role_scope(
        uow: &mut dyn IamUnitOfWork,
        req: &UserAddRoleRequest,
    ) -> Result<(), AppError> {
        if req.role == Role::ShelfAccount {
            // 校验顺序与 Python `_validate_role_scope` 一致：
            // scope 形态 → 货架存在 → zone 白名单 → is_active
            if req.scope_type.as_deref() != Some(SCOPE_TYPE_SHELF) || req.scope_id.is_none() {
                return Err(AppError::validation(
                    "SHELF_ACCOUNT role requires scope_type='shelf' and scope_id",
                ));
            }
            let shelf_id = req.scope_id.expect("上一步已校验非空");
            let shelf = uow.shelf_repo().get_by_id(shelf_id).await?.ok_or_else(|| {
                AppError::biz(code::NOT_FOUND, format!("shelf {shelf_id} not found"))
            })?;
            if !ALLOWED_SHELF_ZONES.contains(&shelf.zone.as_str()) {
                return Err(AppError::biz(
                    code::NOT_FOUND,
                    format!("shelf {shelf_id} has invalid zone '{}'", shelf.zone),
                ));
            }
            if !shelf.is_active {
                return Err(AppError::biz(
                    code::NOT_FOUND,
                    format!("shelf '{}' is inactive; cannot bind", shelf.code),
                ));
            }
        } else if req.scope_type.is_some() || req.scope_id.is_some() {
            // MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER 暂不接 scope
            return Err(AppError::validation(format!(
                "role {} does not accept scope",
                role_as_str(req.role)
            )));
        }
        Ok(())
    }

    fn assemble_user_out(u: User, roles: Vec<UserRoleRow>) -> UserOut {
        UserOut {
            id: u.id,
            version: u.version,
            username: u.username,
            full_name: u.full_name,
            phone: u.phone,
            is_active: u.is_active,
            last_login_at: u.last_login_at,
            created_at: u.created_at,
            updated_at: u.updated_at,
            roles: roles.into_iter().map(to_role_out).collect(),
        }
    }
}

fn to_role_out(r: UserRoleRow) -> UserRoleOut {
    UserRoleOut {
        id: r.id,
        version: r.version,
        role: r.role,
        scope_type: r.scope_type,
        scope_id: r.scope_id.map(|v| v.to_string()),
        shelf_code: r.shelf_code,
        shelf_name: r.shelf_name,
    }
}

/// 唯一索引兜底：并发插入撞上 `uk_t_user_username` 时翻成 409 而非 500
fn map_duplicate_username(e: sqlx::Error) -> AppError {
    if is_unique_violation(&e) {
        AppError::biz(code::DUPLICATE_USERNAME, "username already exists")
    } else {
        AppError::from(e)
    }
}

/// 唯一索引兜底：并发插入撞上 `uk_t_user_role_scope` 时翻成 409 而非 500
fn map_duplicate_role(e: sqlx::Error) -> AppError {
    if is_unique_violation(&e) {
        AppError::biz(code::ROLE_DUPLICATE, "role already assigned to this user")
    } else {
        AppError::from(e)
    }
}

/// PostgreSQL SQLSTATE 23505 = unique_violation
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
