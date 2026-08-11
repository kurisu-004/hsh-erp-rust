//! user 域业务逻辑
//!
//! 对应 Python myERP/service/user.py + service/menu.py。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## 与 Python 的错误码映射
//! Python 的 `BIZ_INVALID_VALUE = 20104` / `BIZ_SHELF_NOT_FOUND = 20501` 尚未进入
//! `shared::error::code`（本任务不改 error.rs），故：
//! - scope 用法错误（SHELF_ACCOUNT 缺 scope / 非货架角色带 scope）→ `VALIDATION_ERROR`(40001)
//! - 货架不存在 / zone 非法 / 已停用 → `NOT_FOUND`(40400)
//!   （Python 对这三种情况复用同一个 20501，仅 HTTP 状态码不同）
//!
//! 待 205xx / 201xx 段补齐后可无损替换。

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{code, AppError};

use super::dto::{
    CurrentUserOut, MenuNodeOut, UserAddRoleRequest, UserCreateRequest, UserListOut, UserListQuery,
    UserOut, UserRoleOut, UserUpdateRequest,
};
use super::model::{Menu, User};
use super::repo::{
    MenuRepo, ShelfRepo, UserInsert, UserRepo, UserRoleInsert, UserRoleRepo, UserRoleRow,
};

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
    AppError::biz(
        code::USER_NOT_FOUND,
        format!("user {user_id} not found"),
    )
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

pub struct UserService;

impl UserService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_users(
        conn: &mut PgConnection,
        query: &UserListQuery,
        current: &CurrentUser,
    ) -> Result<UserListOut, AppError> {
        current.require_role(Role::Manager)?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let like = query.username_like.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let rows = UserRepo::list_with_filters(&mut *conn, like, query.is_active, limit, offset).await?;
        let total = UserRepo::count_with_filters(&mut *conn, like, query.is_active).await?;

        let mut items = Vec::with_capacity(rows.len());
        for u in rows {
            items.push(Self::to_user_out(&mut *conn, u).await?);
        }

        Ok(UserListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_user(
        conn: &mut PgConnection,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;
        let u = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        Self::to_user_out(conn, u).await
    }

    // =======================================================================
    // 创建 / 更新 / 停用
    // =======================================================================

    pub async fn create_user(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &UserCreateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let username = req.username.trim().to_lowercase();
        if username.is_empty() {
            return Err(AppError::validation("username 不能为空"));
        }
        if req.password.is_empty() {
            return Err(AppError::validation("password 不能为空"));
        }

        // 显式查重（partial unique 索引仍是最终防线，见下方 INSERT 的错误映射）
        if UserRepo::get_by_username(&mut *conn, &username)
            .await?
            .is_some()
        {
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
            id: snowflake.next_id(),
            username,
            password_hash: password::hash(&req.password)?,
            full_name: full_name.to_string(),
            phone: req.phone.as_deref().and_then(trimmed_or_none),
            is_active: true,
            created_at: now_naive(),
            created_by: Some(current.id),
        };

        UserRepo::create(&mut *conn, &insert)
            .await
            .map_err(map_duplicate_username)?;

        let u = UserRepo::get_by_id(&mut *conn, insert.id)
            .await?
            .ok_or_else(|| AppError::internal("创建后回读用户失败"))?;
        Self::to_user_out(conn, u).await
    }

    pub async fn update_user(
        conn: &mut PgConnection,
        user_id: i64,
        req: &UserUpdateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = UserRepo::get_by_id(&mut *conn, user_id)
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

        let affected = UserRepo::update_partial(
            &mut *conn,
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

        let updated = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        Self::to_user_out(conn, updated).await
    }

    /// 停用账号 = 软删（置 `deleted_at` + `is_active = false`）
    pub async fn deactivate_user(
        conn: &mut PgConnection,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected =
            UserRepo::soft_delete(&mut *conn, u.id, u.version, now_naive(), Some(current.id))
                .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 软删后 get_by_id 会过滤掉该行，故用内存中的行 + 手工推进字段组装出参
        // （对齐 Python `_to_out(u, include_deleted=True)`）。
        let roles = UserRoleRepo::list_by_user(&mut *conn, u.id).await?;
        let now = now_naive();
        Ok(Self::assemble_user_out(
            User {
                is_active: false,
                deleted_at: Some(now),
                updated_at: now,
                updated_by: Some(current.id),
                version: u.version + 1,
                ..u
            },
            roles,
        ))
    }

    // =======================================================================
    // 密码
    // =======================================================================

    /// 自助改密：校验旧密码，写新哈希并轮转 refresh token（同一条 UPDATE，原子）。
    ///
    /// 允许本人或 MANAGER 调用。注意与 `update_user` 的区别：这里**会**踢下线。
    pub async fn change_own_password(
        conn: &mut PgConnection,
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

        let u = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        // get_by_id 已过滤 deleted_at；此处再挡停用账号（对齐 Python 的三重判断）
        if !u.is_active {
            return Err(user_not_found(user_id));
        }

        if !password::verify(old_password, &u.password_hash)? {
            return Err(AppError::biz(
                code::OLD_PASSWORD_MISMATCH,
                "旧密码不正确",
            ));
        }

        let affected = UserRepo::update_password_and_rotate(
            &mut *conn,
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
        Ok(())
    }

    /// 管理员重置密码为默认口令 `changeme`，并轮转 refresh token（踢下线）。
    pub async fn admin_reset_password(
        conn: &mut PgConnection,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected = UserRepo::update_password_and_rotate(
            &mut *conn,
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

        let updated = UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        Self::to_user_out(conn, updated).await
    }

    // =======================================================================
    // 角色管理
    // =======================================================================

    pub async fn list_user_roles(
        conn: &mut PgConnection,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<UserRoleOut>, AppError> {
        current.require_role(Role::Manager)?;

        UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let rows = UserRoleRepo::list_by_user(conn, user_id).await?;
        Ok(rows.into_iter().map(to_role_out).collect())
    }

    pub async fn add_role(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        user_id: i64,
        req: &UserAddRoleRequest,
        current: &CurrentUser,
    ) -> Result<UserRoleOut, AppError> {
        current.require_role(Role::Manager)?;

        UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        Self::validate_role_scope(&mut *conn, req).await?;

        let role_str = role_as_str(req.role);
        let scope_type = req.scope_type.as_deref();

        // 显式查重。Python 依赖唯一索引 + IntegrityError，但 partial unique 索引对
        // (user_id, role, NULL, NULL) 这类含 NULL 的组合不生效（SQL 里 NULL != NULL），
        // 导致非货架角色可以被重复添加。这里用 IS NOT DISTINCT FROM 显式查重堵住该缺口。
        if UserRoleRepo::exists_same_scope(&mut *conn, user_id, role_str, scope_type, req.scope_id)
            .await?
        {
            return Err(AppError::biz(
                code::ROLE_DUPLICATE,
                format!(
                    "role {role_str} (scope={}/{}) already assigned to this user",
                    scope_type.unwrap_or("null"),
                    req.scope_id.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                ),
            ));
        }

        let insert = UserRoleInsert {
            id: snowflake.next_id(),
            user_id,
            role: role_str.to_string(),
            scope_type: scope_type.map(str::to_string),
            scope_id: req.scope_id,
            created_at: now_naive(),
            created_by: Some(current.id),
        };
        UserRoleRepo::create(&mut *conn, &insert)
            .await
            .map_err(map_duplicate_role)?;

        let rows = UserRoleRepo::list_by_user(conn, user_id).await?;
        rows.into_iter()
            .find(|r| r.id == insert.id)
            .map(to_role_out)
            .ok_or_else(|| AppError::internal("创建后回读角色失败"))
    }

    pub async fn remove_role(
        conn: &mut PgConnection,
        user_id: i64,
        role_id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        UserRepo::get_by_id(&mut *conn, user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let r = UserRoleRepo::get_by_id(&mut *conn, role_id).await?;
        // 角色必须存在且属于该用户，否则一律 404（不泄露他人角色是否存在）
        let r = match r {
            Some(r) if r.user_id == user_id => r,
            _ => {
                return Err(AppError::biz(
                    code::ROLE_NOT_FOUND,
                    format!("role {role_id} not found for user {user_id}"),
                ))
            }
        };

        let affected =
            UserRoleRepo::soft_delete(conn, r.id, r.version, now_naive(), Some(current.id)).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    // =======================================================================
    // 菜单
    // =======================================================================

    /// 取角色可见菜单并组树（供 auth 域 `/me` 与登录响应复用）
    pub async fn menus_for_roles(
        conn: &mut PgConnection,
        roles: &[Role],
    ) -> Result<Vec<MenuNodeOut>, AppError> {
        let role_strs: Vec<String> = roles
            .iter()
            .map(|r| role_as_str(*r).to_string())
            .collect();
        let menus = MenuRepo::list_active_for_roles(conn, &role_strs).await?;
        Ok(build_menu_tree(menus))
    }

    /// 组装 `/auth/me` 出参（auth 域复用）
    pub async fn current_user_out(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<CurrentUserOut, AppError> {
        let u = UserRepo::get_by_id(&mut *conn, current.id)
            .await?
            .ok_or_else(|| user_not_found(current.id))?;
        let menus = Self::menus_for_roles(conn, &current.roles).await?;
        Ok(CurrentUserOut {
            id: u.id,
            username: u.username,
            full_name: u.full_name,
            is_active: u.is_active,
            roles: current.roles.iter().map(|r| role_as_str(*r).to_string()).collect(),
            shelf_ids: current.shelf_ids.iter().map(|v| v.to_string()).collect(),
            menus,
        })
    }

    // =======================================================================
    // 内部
    // =======================================================================

    async fn validate_role_scope(
        conn: &mut PgConnection,
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
            let shelf = ShelfRepo::get_by_id(conn, shelf_id).await?.ok_or_else(|| {
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

    /// 读取用户角色并组装 `UserOut`
    async fn to_user_out(conn: &mut PgConnection, u: User) -> Result<UserOut, AppError> {
        let roles = UserRoleRepo::list_by_user(conn, u.id).await?;
        Ok(Self::assemble_user_out(u, roles))
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

// ===========================================================================
// 菜单树组装（纯函数，对齐 Python service/menu.py::_to_tree）
// ===========================================================================

/// 把拍平的菜单行组装成树。
///
/// 规则（与 Python `_to_tree` 逐条对齐）：
/// - 根与每层 children 均按 `(sort_order, code)` 升序
/// - `parent_id` 为 NULL，或指向**不在可见集合内**的父节点（父节点被停用/软删/
///   不属于当前角色）→ 该节点提升为根（孤儿兜底）
/// - 不做深循环检测：DB CHECK `ck_t_menu_no_self_loop` 已挡单行自环，seed 数据可信。
///   但本实现用「从 map 取走节点」的方式递归，即便出现环也只会丢弃成环节点，
///   不会无限递归（比 Python 多一层安全兜底）。
pub fn build_menu_tree(menus: Vec<Menu>) -> Vec<MenuNodeOut> {
    let visible: HashSet<i64> = menus.iter().map(|m| m.id).collect();
    let sort_keys: HashMap<i64, (i32, String)> = menus
        .iter()
        .map(|m| (m.id, (m.sort_order, m.code.clone())))
        .collect();

    let mut nodes: HashMap<i64, MenuNodeOut> = HashMap::with_capacity(menus.len());
    let mut children_of: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut root_ids: Vec<i64> = Vec::new();

    for m in menus {
        let id = m.id;
        let parent_id = m.parent_id;
        nodes.insert(
            id,
            MenuNodeOut {
                id,
                version: m.version,
                parent_id: parent_id.map(|p| p.to_string()),
                code: m.code,
                title: m.title,
                path: m.path,
                icon: m.icon,
                sort_order: m.sort_order,
                children: Vec::new(),
            },
        );
        match parent_id {
            // 父节点可见且非自环 → 挂为子节点
            Some(p) if p != id && visible.contains(&p) => {
                children_of.entry(p).or_default().push(id);
            }
            // parent_id 为 NULL，或父节点不可见 → 升为根（孤儿兜底）
            _ => root_ids.push(id),
        }
    }

    sort_ids(&mut root_ids, &sort_keys);
    for kids in children_of.values_mut() {
        sort_ids(kids, &sort_keys);
    }

    root_ids
        .into_iter()
        .filter_map(|id| assemble_node(id, &mut nodes, &children_of))
        .collect()
}

fn sort_ids(ids: &mut [i64], sort_keys: &HashMap<i64, (i32, String)>) {
    ids.sort_by(|a, b| sort_keys.get(a).cmp(&sort_keys.get(b)));
}

/// 递归组装：从 `nodes` 中「取走」节点，天然防止环导致的无限递归。
fn assemble_node(
    id: i64,
    nodes: &mut HashMap<i64, MenuNodeOut>,
    children_of: &HashMap<i64, Vec<i64>>,
) -> Option<MenuNodeOut> {
    let mut node = nodes.remove(&id)?;
    if let Some(kids) = children_of.get(&id) {
        node.children = kids
            .iter()
            .filter_map(|k| assemble_node(*k, nodes, children_of))
            .collect();
    }
    Some(node)
}
