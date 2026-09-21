//! iam AccountService 单测共享 helpers（不计入 service_tests 行数额度）
//!
//! 2026-09-21 事务分层重构后：service 签名 `<R: IamRepo>(&self, repo: &mut R, ...)`，
//! 单测用 `MockIamRepo` 直接注入；事务边界不再在 service 层，故无 commit/rollback 断言。
//! 写端点原本的 `assert_committed()` / `assert_not_committed()` 整体删除——服务不知事务。

use std::sync::{Arc, Mutex};

use chrono::NaiveDateTime;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::model::{Menu, Shelf, User, UserRole};
use crate::modules::iam::repo::{MockIamRepo, UserRoleRow};
use crate::modules::iam::service::AccountService;

/// 模拟 `sqlx::Error::RowNotFound`（与原 user/service_tests.rs 同形）
pub(crate) fn test_db_error() -> sqlx::Error {
    sqlx::Error::RowNotFound
}

pub(crate) const TEST_NOW: &str = "2026-09-18T12:00:00";
pub(crate) fn now() -> NaiveDateTime {
    NaiveDateTime::parse_from_str(TEST_NOW, "%Y-%m-%dT%H:%M:%S").unwrap()
}

/// update_phone_captured：捕获 (set_phone, phone) 元组的共享句柄
pub(crate) type PhoneCapture = Arc<Mutex<Option<(bool, Option<String>)>>>;

pub(crate) fn manager_current() -> CurrentUser {
    CurrentUser {
        id: 1_000_000,
        username: "manager".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}
pub(crate) fn clerk_current() -> CurrentUser {
    CurrentUser {
        id: 2_000_000,
        username: "clerk".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

pub(crate) fn make_snowflake() -> Arc<SnowflakeIdGenerator> {
    Arc::new(SnowflakeIdGenerator::new(1_700_000_000_000, 1))
}

/// 直接构造 `AccountService`（2026-09-21 后仅需 snowflake）。
pub(crate) fn make_account_service() -> Arc<AccountService> {
    Arc::new(AccountService::new(make_snowflake()))
}

/// 构造一个按 `setup` 配置好的 `MockIamRepo`。用例调用 `make_repo_with(|r| { ... })`。
///
/// 不再返回 `flags` —— service 不再 commit/rollback，时序断言整体删除（plan v4 §5.2
/// 表的「service 不再管事务」改造）。
pub(crate) fn make_repo_with<F: FnOnce(&mut MockIamRepo)>(setup: F) -> MockIamRepo {
    let mut repo = MockIamRepo::new();
    setup(&mut repo);
    repo
}

/// 守卫类 helper：service 构造 + 空 MockIamRepo（用于「权限拒」类用例，无 repo 期望）
pub(crate) fn guard_repo() -> (Arc<AccountService>, MockIamRepo) {
    (make_account_service(), MockIamRepo::new())
}

pub(crate) fn make_user(id: i64) -> User {
    let t = now();
    User {
        id,
        username: format!("user{id}"),
        password_hash: "$2b$12$abc".into(),
        full_name: format!("User {id}"),
        phone: Some("13800000000".into()),
        is_active: true,
        last_login_at: None,
        refresh_token_version: 0,
        version: 0,
        created_at: t,
        created_by: Some(1_000_000),
        updated_at: t,
        updated_by: Some(1_000_000),
        deleted_at: None,
    }
}

/// 构造指定 username 的 user（create_user 回读测试用）
pub(crate) fn make_user_with_username(id: i64, username: &str) -> User {
    let mut u = make_user(id);
    u.username = username.into();
    u.full_name = "Alice".into();
    u
}

/// 用真实 bcrypt 散列构造 user（change_own_password 走 verify 路径必用）
pub(crate) fn make_user_with_hash(id: i64, hash: String) -> User {
    let mut u = make_user(id);
    u.password_hash = hash;
    u
}
pub(crate) fn make_user_role_row(id: i64, user_id: i64, role: &str) -> UserRoleRow {
    let t = now();
    UserRoleRow {
        id,
        user_id,
        role: role.into(),
        scope_type: None,
        scope_id: None,
        version: 0,
        created_at: t,
        created_by: Some(1_000_000),
        updated_at: t,
        updated_by: Some(1_000_000),
        deleted_at: None,
        shelf_code: None,
        shelf_name: None,
    }
}
pub(crate) fn make_user_role(id: i64, user_id: i64, role: &str) -> UserRole {
    let t = now();
    UserRole {
        id,
        user_id,
        role: role.into(),
        scope_type: None,
        scope_id: None,
        version: 0,
        created_at: t,
        created_by: Some(1_000_000),
        updated_at: t,
        updated_by: Some(1_000_000),
        deleted_at: None,
    }
}
pub(crate) fn make_shelf(id: i64, zone: &str, is_active: bool) -> Shelf {
    let t = now();
    Shelf {
        id,
        code: format!("SHELF-{id}"),
        name: format!("Shelf {id}"),
        zone: zone.into(),
        location: None,
        is_active,
        display_order: 0,
        version: 0,
        created_at: t,
        created_by: Some(1_000_000),
        updated_at: t,
        updated_by: Some(1_000_000),
        deleted_at: None,
    }
}
pub(crate) fn make_menu(id: i64, parent_id: Option<i64>, code: &str, sort_order: i32) -> Menu {
    let t = now();
    Menu {
        id,
        parent_id,
        code: code.into(),
        title: format!("Menu {id}"),
        path: Some(format!("/m/{id}")),
        icon: None,
        sort_order,
        is_active: true,
        version: 0,
        created_at: t,
        created_by: Some(1_000_000),
        updated_at: t,
        updated_by: Some(1_000_000),
        deleted_at: None,
    }
}
