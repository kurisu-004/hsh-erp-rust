//! iam AccountService 单测共享 helpers（不计入 service_tests 行数额度）

use std::sync::{Arc, Mutex};

use chrono::NaiveDateTime;

use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{MockSessionStore, SessionStore};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::model::{Menu, Shelf, User, UserRole};
use crate::modules::iam::repo::UserRoleRow;
use crate::modules::iam::service::AccountService;
use crate::modules::iam::uow::IamUowProvider;
use crate::modules::iam::uow::test_support::{
    IamUowFlags, MockIamUnitOfWork, MockIamUowProvider, provider_returning,
};

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
pub(crate) fn make_session() -> Arc<dyn SessionStore> {
    Arc::new(MockSessionStore::new())
}

/// 走 provider 的 service 装线（写端点 / 读端点共用）
pub(crate) fn make_svc(
    provider: Arc<dyn IamUowProvider>,
    session: Arc<dyn SessionStore>,
) -> Arc<AccountService> {
    Arc::new(AccountService::new(provider, make_snowflake(), session))
}

/// 写端点通用 helper：构造 mock uow → 装线 service → 应用 setup → 返回 svc + flags
pub(crate) async fn write_svc<F: FnOnce(&mut MockIamUnitOfWork)>(
    setup: F,
) -> (Arc<AccountService>, IamUowFlags) {
    let (mut uow, flags) = MockIamUnitOfWork::new();
    setup(&mut uow);
    (make_svc(provider_returning(uow), make_session()), flags)
}

/// 读端点 / 守卫类通用 helper（同一形态，断言不同）
#[allow(dead_code)]
pub(crate) async fn read_svc<F: FnOnce(&mut MockIamUnitOfWork)>(
    setup: F,
) -> (Arc<AccountService>, IamUowFlags) {
    write_svc(setup).await
}

/// 守卫类 helper：provider 零 begin
pub(crate) fn guard_svc() -> Arc<AccountService> {
    make_svc(Arc::new(MockIamUowProvider::new()), make_session())
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
