//! iam 域账号管理 service —— `AccountService` 本体 + 跨子模块共享的常量 / helper
//!
//! `AccountService` 是同一个 struct，方法按 `impl` 块分散在 3 个子模块里：
//!
//! | 子模块 | 装什么 |
//! |---|---|
//! | [`mod.rs`]（本文件）| `AccountService` 结构体 + `new()` + **全部常量** + 三块业务都用的共享 helper（出参组装 / 唯一索引错误翻译 / 文本归一 / 乐观锁 409）+ `menus_for_roles`（`SessionService` 复用）+ `resolve_wx_login_user`（wx 域开口） |
//! | [`user`] | `list_users` / `get_user` / `create_user` / `update_user` / `deactivate_user` / `change_own_password` / `admin_reset_password` —— `t_user` 的 CRUD 与密码 |
//! | [`role`] | `list_user_roles` / `add_role` / `remove_role` / `validate_role_scope` —— `t_user_role` 的增删查 + SHELF_ACCOUNT 的 scope 校验 |
//! | [`wx`] | `bind_wx_identity` / `unbind_wx_identity` / `get_wx_identity` —— `t_wx_identity` 的管理端读写 |
//!
//! 为什么 `menus_for_roles` 留本文件：它服务的是 `SessionService`（`/iam/login` /
//! `/iam/me` / `/iam/refresh` 的菜单组装），不属于上面三块中的任何一块。

use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::rbac::Role;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{AppError, code};

// `iam/service/` 子目录中 dto / vo / repo 是 sibling 的兄弟模块 —— 用 `super::super::` 跨级
use super::super::repo::model::{User, WxIdentity};
use super::super::repo::{IamRepoTrait, UserRoleRow};
use super::super::vo::{MenuNodeOut, UserOut, UserRoleOut, WxIdentityOut};
use super::menu::build_menu_tree;

mod role;
mod user;
mod wx;

// ===========================================================================
// 常量
// ===========================================================================

/// 管理员重置密码时写入的默认口令（对齐 Python `DEFAULT_RESET_PASSWORD`）
pub const DEFAULT_RESET_PASSWORD: &str = "changeme";

/// `t_wx_identity.corp_id` 列宽（DB 侧 varchar(64)，应用层同步守卫）
pub(crate) const MAX_CORP_ID_LEN: usize = 64;
/// `t_wx_identity.wx_user_id` 列宽（DB 侧 varchar(64)，应用层同步守卫）
pub(crate) const MAX_WX_USER_ID_LEN: usize = 64;

/// SHELF_ACCOUNT 角色唯一合法的 scope_type
pub const SCOPE_TYPE_SHELF: &str = "shelf";

/// 可绑定 SHELF_ACCOUNT 的货架分区白名单（对齐 Python `ShelfZone`）
///
/// 两处消费方（写入侧 `validate_role_scope`、登录侧 `resolve_roles_and_scope`）共用
/// 这一个常量 —— 曾经 iam 域内各有一份副本，改白名单时漏改一处会让「管理端能绑、
/// 登录时被静默过滤掉」的不一致现象出现。
pub const ALLOWED_SHELF_ZONES: [&str; 2] = ["PRODUCTION", "INSPECTION"];

pub(crate) const DEFAULT_LIMIT: i64 = 50;
pub(crate) const MAX_LIMIT: i64 = 500;

// ===========================================================================
// 结构体
// ===========================================================================

/// iam 域账号管理 service
///
/// 字段仅 `snowflake`（事务已移交 handler；session 清理也移交 handler）。实例为轻壳，
/// 可直接 `Arc<AccountService>` 存 `AppState`；方法签名收 `mut repo: R`（by-value；
/// 生产 `R = &mut PgConnection`，单测 `R = MockIamRepoTrait`），单测用 `MockIamRepoTrait` 直接注入。
pub struct AccountService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl AccountService {
    /// 构造：仅需雪花 ID 生成器。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    // =======================================================================
    // 菜单（helper：对 SessionService 的 `/me` 与登录响应开放）
    // =======================================================================

    /// 取角色可见菜单并组树（供 SessionService 复用）。调用方传 `repo`，helper 不自管事务。
    pub async fn menus_for_roles<R: IamRepoTrait>(
        &self,
        repo: &mut R,
        roles: &[Role],
    ) -> Result<Vec<MenuNodeOut>, AppError> {
        let role_strs: Vec<String> = roles.iter().map(|r| r.as_str().to_string()).collect();
        let menus = repo.list_active_menus_by_roles(&role_strs).await?;
        Ok(build_menu_tree(menus))
    }

    // =======================================================================
    // 企业微信登录的身份解析（wx 域开口）
    // =======================================================================

    /// 把「企业微信 `(corp_id, wx_user_id)`」解析成系统账号，供 wx 域的
    /// `POST /api/v2/wx/iam/wx-login` 调（该 handler 随后把 `User` 交给
    /// `SessionService::login_by_user_id`）。
    ///
    /// ## 错误码（对外契约，勿改文案）
    /// - `40107 BIZ_WX_NOT_BOUND` —— 该 userid 没有活跃绑定（**仅预绑定，不自动开户**）
    /// - `40101 BIZ_AUTH_INVALID` —— 绑定指向的账号不存在或已软删。对外只说
    ///   「用户名或密码错误」：**不向调用方泄露「绑定存在但账号已删」**这一额外信息，
    ///   该情形只进 `tracing::warn!`。
    pub async fn resolve_wx_login_user<R: IamRepoTrait>(
        &self,
        mut repo: R,
        corp_id: &str,
        wx_user_id: &str,
    ) -> Result<User, AppError> {
        let Some(identity) = repo
            .get_wx_identity_by_corp_and_user(corp_id, wx_user_id)
            .await?
        else {
            return Err(AppError::biz(
                code::BIZ_WX_NOT_BOUND,
                "该企业微信账号未绑定系统账号，请联系管理员",
            ));
        };

        let user_id = identity.user_id;
        repo.get_user_by_id(user_id).await?.ok_or_else(|| {
            tracing::warn!(user_id, "wx-login: 绑定指向的用户不存在或已软删");
            AppError::biz(code::BIZ_AUTH_INVALID, "用户名或密码错误")
        })
    }
}

// ===========================================================================
// 共享 helper
// ===========================================================================

/// `Role` → DB / JSON 中的大写字符串。
///
/// 薄委托到 `auth::rbac::Role::as_str`（唯一真源）。保留本函数是因为
/// `iam::service::role_as_str` 是模块对外导出项，调用点（`session.rs` 的
/// Redis profile / `CurrentUserOut` 组装）不必改成 `r.as_str()` 也能保持单一真源。
pub fn role_as_str(role: Role) -> &'static str {
    role.as_str()
}

pub(crate) fn user_not_found(user_id: i64) -> AppError {
    AppError::biz(code::USER_NOT_FOUND, format!("user {user_id} not found"))
}

/// 乐观锁写入返回 0 行 → 409。
///
/// 0 行的含义取决于 `version` 的来源，本仓两种都有：
/// - **客户端 body 传入**（`update_user` / `deactivate_user` / `remove_role` /
///   `unbind_wx_identity` 的首行）⇒ 只能是并发改动；
/// - **service 从 DB 读到**（`admin_reset_password` 这条 OCC 豁免的幂等端点、
///   `change_own_password`）⇒ 也可能是读到的 version 在本事务内已过期。
pub(crate) fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// `full_name` 等文本字段：trim 后空串归一为 `None`（对齐 Python `.strip() or None`）
pub(crate) fn trimmed_or_none(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// `UserRoleRow` → `UserRoleOut`（`scope_id` 与 id 一样按 i64→string 序列化）
pub(crate) fn to_role_out(r: &UserRoleRow) -> UserRoleOut {
    UserRoleOut {
        id: r.id,
        version: r.version,
        role: r.role.clone(),
        scope_type: r.scope_type.clone(),
        scope_id: r.scope_id.map(|v| v.to_string()),
        shelf_code: r.shelf_code.clone(),
        shelf_name: r.shelf_name.clone(),
    }
}

/// `t_wx_identity` 行 → `WxIdentityOut`
pub(crate) fn to_wx_identity_out(r: WxIdentity) -> WxIdentityOut {
    WxIdentityOut {
        id: r.id,
        corp_id: r.corp_id,
        wx_user_id: r.wx_user_id,
        user_id: r.user_id,
        version: r.version,
        created_at: r.created_at,
    }
}

/// `t_user` 行 + 它的角色行 → `UserOut`
pub(crate) fn assemble_user_out(u: &User, roles: &[UserRoleRow]) -> UserOut {
    UserOut {
        id: u.id,
        version: u.version,
        username: u.username.clone(),
        full_name: u.full_name.clone(),
        phone: u.phone.clone(),
        is_active: u.is_active,
        last_login_at: u.last_login_at,
        created_at: u.created_at,
        updated_at: u.updated_at,
        roles: roles.iter().map(to_role_out).collect(),
    }
}

/// `list_users` 用：把批量查回的角色行按 `user_id` 分桶，供逐行 `assemble_user_out`
/// O(1) 取用（消解 N+1）。返回的 map 直接交给 `roles_by_user.get(&u.id)`。
pub(crate) fn bucket_roles_by_user(rows: Vec<UserRoleRow>) -> HashMap<i64, Vec<UserRoleRow>> {
    let mut map: HashMap<i64, Vec<UserRoleRow>> = HashMap::new();
    for r in rows {
        map.entry(r.user_id).or_default().push(r);
    }
    map
}

/// 唯一索引兜底：并发插入撞上 `uk_t_user_username` 时翻成 409 而非 500
pub(crate) fn map_duplicate_username(e: sqlx::Error) -> AppError {
    if is_unique_violation(&e) {
        AppError::biz(code::DUPLICATE_USERNAME, "username already exists")
    } else {
        AppError::from(e)
    }
}

/// 唯一索引兜底：并发插入撞上 `uk_t_user_role_user_role_scope` 时翻成 409 而非 500
pub(crate) fn map_duplicate_role(e: sqlx::Error) -> AppError {
    if is_unique_violation(&e) {
        AppError::biz(code::ROLE_DUPLICATE, "role already assigned to this user")
    } else {
        AppError::from(e)
    }
}

/// 唯一索引兜底：并发插入撞上 `uk_wx_identity_corp_user` 时翻成 40108 而非 500
/// （DB 兜底与应用层预检同码 40108）
pub(crate) fn map_duplicate_wx_identity(e: sqlx::Error) -> AppError {
    if is_unique_violation(&e) {
        AppError::biz(
            code::BIZ_WX_BINDING_DUPLICATE,
            "该企业微信账号已绑定到其他系统账号",
        )
    } else {
        AppError::from(e)
    }
}

/// PostgreSQL SQLSTATE 23505 = unique_violation
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
