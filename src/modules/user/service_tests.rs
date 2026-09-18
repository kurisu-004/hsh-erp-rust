//! user 域 service 66 例 mock 单测（plan v4 §3 V7 + §5.1）
//!
//! 纯 `#[tokio::test]`，零数据库连接（plan §8 #5 硬 gate）。期望设在
//! `MockUnitOfWork` 持有的 4 个 Mock repo 上；写端点 `assert_committed()`，
//! 读端点 / 守卫类 `assert_not_committed()`；session 时序在 mock 闭包内
//! 捕获 `flags` 断言 `committed == true`。
//!
//! 分配（66 = 59 + 7 boundary）：
//! list_users 6 / get_user 4 / create_user 7 / update_user 7 / deactivate_user 4 /
//! change_own_password 6 / admin_reset_password 4 / list_user_roles 4 / add_role 7 /
//! remove_role 5 / menus_for_roles 3 / current_user_out 2 / boundary 7

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::NaiveDateTime;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{MockSessionStore, SessionStore};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{code, AppError};

use super::dto::{CurrentUserOut, MenuNodeOut, UserAddRoleRequest, UserCreateRequest, UserListQuery, UserUpdateRequest};
use super::model::{Menu, Shelf, User, UserRole};
use super::repo::UserRoleRow;
use super::service::UserService;
use super::uow::test_support::{provider_returning, MockUnitOfWork, MockUowProvider, UowFlags};
use super::uow::UowProvider;

// ===========================================================================
// helpers（共用）
// ===========================================================================

fn test_db_error() -> sqlx::Error { sqlx::Error::RowNotFound }

const TEST_NOW: &str = "2026-09-18T12:00:00";
fn now() -> NaiveDateTime { NaiveDateTime::parse_from_str(TEST_NOW, "%Y-%m-%dT%H:%M:%S").unwrap() }

/// update_phone_captured：捕获 (set_phone, phone) 元组的共享句柄
type PhoneCapture = Arc<Mutex<Option<(bool, Option<String>)>>>;

fn manager_current() -> CurrentUser { CurrentUser { id: 1_000_000, username: "manager".into(), roles: vec![Role::Manager], shelf_ids: vec![], shelf_wildcard: false } }
fn clerk_current() -> CurrentUser { CurrentUser { id: 2_000_000, username: "clerk".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false } }

fn make_snowflake() -> Arc<SnowflakeIdGenerator> { Arc::new(SnowflakeIdGenerator::new(1_700_000_000_000, 1)) }
fn make_session() -> Arc<dyn SessionStore> { Arc::new(MockSessionStore::new()) }

/// 走 provider 的 service 装线（写端点 / 读端点共用）
fn make_svc(provider: Arc<dyn UowProvider>, session: Arc<dyn SessionStore>) -> Arc<UserService> {
    Arc::new(UserService::new(provider, make_snowflake(), session))
}

/// 写端点通用 helper：构造 mock uow → 装线 service → 应用 setup → 返回 svc + flags
async fn write_svc<F: FnOnce(&mut MockUnitOfWork)>(setup: F) -> (Arc<UserService>, UowFlags) {
    let (mut uow, flags) = MockUnitOfWork::new();
    setup(&mut uow);
    (make_svc(provider_returning(uow), make_session()), flags)
}

/// 读端点 / 守卫类通用 helper（同一形态，断言不同）
#[allow(dead_code)]
async fn read_svc<F: FnOnce(&mut MockUnitOfWork)>(setup: F) -> (Arc<UserService>, UowFlags) {
    write_svc(setup).await
}

/// 守卫类 helper：provider 零 begin
fn guard_svc() -> Arc<UserService> {
    make_svc(Arc::new(MockUowProvider::new()), make_session())
}

fn make_user(id: i64) -> User {
    let t = now();
    User { id, username: format!("user{id}"), password_hash: "$2b$12$abc".into(), full_name: format!("User {id}"), phone: Some("13800000000".into()), is_active: true, last_login_at: None, refresh_token_version: 0, version: 0, created_at: t, created_by: Some(1_000_000), updated_at: t, updated_by: Some(1_000_000), deleted_at: None }
}

/// 构造指定 username 的 user（create_user 回读测试用）
fn make_user_with_username(id: i64, username: &str) -> User {
    let mut u = make_user(id);
    u.username = username.into();
    u.full_name = "Alice".into();
    u
}

/// 用真实 bcrypt 散列构造 user（change_own_password 走 verify 路径必用）
fn make_user_with_hash(id: i64, hash: String) -> User {
    let mut u = make_user(id);
    u.password_hash = hash;
    u
}
fn make_user_role_row(id: i64, user_id: i64, role: &str) -> UserRoleRow {
    let t = now();
    UserRoleRow { id, user_id, role: role.into(), scope_type: None, scope_id: None, version: 0, created_at: t, created_by: Some(1_000_000), updated_at: t, updated_by: Some(1_000_000), deleted_at: None, shelf_code: None, shelf_name: None }
}
fn make_user_role(id: i64, user_id: i64, role: &str) -> UserRole {
    let t = now();
    UserRole { id, user_id, role: role.into(), scope_type: None, scope_id: None, version: 0, created_at: t, created_by: Some(1_000_000), updated_at: t, updated_by: Some(1_000_000), deleted_at: None }
}
fn make_shelf(id: i64, zone: &str, is_active: bool) -> Shelf {
    let t = now();
    Shelf { id, code: format!("SHELF-{id}"), name: format!("Shelf {id}"), zone: zone.into(), location: None, is_active, display_order: 0, version: 0, created_at: t, created_by: Some(1_000_000), updated_at: t, updated_by: Some(1_000_000), deleted_at: None }
}
fn make_menu(id: i64, parent_id: Option<i64>, code: &str, sort_order: i32) -> Menu {
    let t = now();
    Menu { id, parent_id, code: code.into(), title: format!("Menu {id}"), path: Some(format!("/m/{id}")), icon: None, sort_order, is_active: true, version: 0, created_at: t, created_by: Some(1_000_000), updated_at: t, updated_by: Some(1_000_000), deleted_at: None }
}

// ===========================================================================
// 1. list_users（6 例）
// ===========================================================================

#[tokio::test]
async fn list_users_returns_paginated_list() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_, _, _, _| Ok(vec![make_user(101), make_user(102)]));
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(42));
        uow.user_role_repo.expect_list_by_user().returning(|id| Ok(vec![make_user_role_row(1, id, "MANAGER")]));
    }).await;
    let out = svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: Some(20), offset: Some(0) }, &manager_current()).await.unwrap();
    assert_eq!(out.total, 42); assert_eq!(out.items.len(), 2); assert_eq!(out.limit, 20); assert_eq!(out.offset, 0);
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: None, offset: None }, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn list_users_clamps_limit_to_max_when_exceeded() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_ul, _ia, limit, _o| { assert_eq!(limit, 500); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    let out = svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: Some(10_000), offset: Some(0) }, &manager_current()).await.unwrap();
    assert_eq!(out.limit, 500); flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_returns_empty_when_no_results() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_, _, _, _| Ok(vec![]));
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    let out = svc.list_users(&UserListQuery { username_like: Some("nope".into()), is_active: None, limit: None, offset: None }, &manager_current()).await.unwrap();
    assert!(out.items.is_empty()); assert_eq!(out.total, 0);
    assert_eq!(out.limit, 50); assert_eq!(out.offset, 0);
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_filters_by_is_active() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_ul, is_active, _l, _o| { assert_eq!(is_active, Some(false)); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    svc.list_users(&UserListQuery { username_like: None, is_active: Some(false), limit: None, offset: None }, &manager_current()).await.unwrap();
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_normalizes_blank_username_like_to_none() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|username_like, _ia, _l, _o| { assert!(username_like.is_none()); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    svc.list_users(&UserListQuery { username_like: Some("   ".into()), is_active: None, limit: None, offset: None }, &manager_current()).await.unwrap();
    flags.assert_not_committed();
}

// ===========================================================================
// 2. get_user（4 例）
// ===========================================================================

#[tokio::test]
async fn get_user_returns_user_with_roles() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|id| Ok(vec![make_user_role_row(1, id, "MANAGER")]));
    }).await;
    let out = svc.get_user(101, &manager_current()).await.unwrap();
    assert_eq!(out.id, 101); assert_eq!(out.roles.len(), 1); assert_eq!(out.roles[0].role, "MANAGER");
    flags.assert_not_committed();
}

#[tokio::test]
async fn get_user_returns_not_found_when_user_missing() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.get_user(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn get_user_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.get_user(101, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn get_user_returns_not_found_for_deactivated_user() {
    // 软删后 get_by_id 返回 None（deleted_at IS NULL 过滤）→ service 翻 404
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.get_user(42, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

// ===========================================================================
// 3. create_user（7 例）
// ===========================================================================

fn make_create_req() -> UserCreateRequest { UserCreateRequest { username: "alice".into(), password: "secret123".into(), full_name: "Alice".into(), phone: None } }

#[tokio::test]
async fn create_user_succeeds_with_valid_inputs() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_username().returning(|_| Ok(None));
        uow.user_repo.expect_create().returning(|_| Ok(()));
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user_with_username(id, "alice"))));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let out = svc.create_user(&make_create_req(), &manager_current()).await.unwrap();
    assert_eq!(out.username, "alice"); assert!(out.is_active);
    flags.assert_committed();
}

#[tokio::test]
async fn create_user_rejects_empty_username_after_trim() {
    let (svc, flags) = write_svc(|_| {}).await;
    let mut req = make_create_req(); req.username = "   ".into();
    assert!(matches!(svc.create_user(&req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_rejects_empty_password() {
    let (svc, flags) = write_svc(|_| {}).await;
    let mut req = make_create_req(); req.password = "".into();
    assert!(matches!(svc.create_user(&req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_rejects_empty_full_name_after_trim() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_username().returning(|_| Ok(None)); }).await;
    let mut req = make_create_req(); req.full_name = "   ".into();
    assert!(matches!(svc.create_user(&req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_rejects_duplicate_username_as_409() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_username().returning(|_| Ok(Some(make_user(42)))); }).await;
    let res = svc.create_user(&make_create_req(), &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::DUPLICATE_USERNAME));
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_injects_snowflake_id_into_insert() {
    let captured: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_username().returning(|_| Ok(None));
        uow.user_repo.expect_create().returning(move |user| { *captured_c.lock().unwrap() = Some(user.id); Ok(()) });
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let out = svc.create_user(&make_create_req(), &manager_current()).await.unwrap();
    assert_eq!(*captured.lock().unwrap(), Some(out.id)); // service 内 next_id 一次，create 拿到 == out.id
    flags.assert_committed();
}

#[tokio::test]
async fn create_user_hashes_password_before_insert() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_username().returning(|_| Ok(None));
        uow.user_repo.expect_create().returning(move |user| { *captured_c.lock().unwrap() = Some(user.password_hash.clone()); Ok(()) });
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    svc.create_user(&make_create_req(), &manager_current()).await.unwrap();
    let hash = captured.lock().unwrap().clone().unwrap();
    assert!(hash.starts_with("$2")); assert_ne!(hash, "secret123");
    flags.assert_committed();
}

// ===========================================================================
// 4. update_user（7 例）
// ===========================================================================

fn make_update_req() -> UserUpdateRequest { UserUpdateRequest { full_name: Some("Updated".into()), phone: None, password: None, is_active: None } }

#[tokio::test]
async fn update_user_succeeds_with_valid_inputs() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_partial().returning(|_id, _v, _fn, _sp, _p, _ph, _ia, _w, _ub| Ok(1));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let out = svc.update_user(101, &make_update_req(), &manager_current()).await.unwrap();
    assert_eq!(out.id, 101); flags.assert_committed();
}

#[tokio::test]
async fn update_user_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.update_user(101, &make_update_req(), &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn update_user_rejects_empty_full_name_after_trim() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id)))); }).await;
    let mut req = make_update_req(); req.full_name = Some("   ".into());
    assert!(matches!(svc.update_user(101, &req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn update_user_phone_explicit_empty_clears_phone() {
    let captured: PhoneCapture = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_partial().returning(move |_id, _v, _fn, set_phone, phone, _ph, _ia, _w, _ub| { *captured_c.lock().unwrap() = Some((set_phone, phone.map(String::from))); Ok(1) });
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let mut req = make_update_req(); req.phone = Some("".into());
    svc.update_user(101, &req, &manager_current()).await.unwrap();
    let (sp, ph) = captured.lock().unwrap().as_ref().unwrap().clone();
    assert!(sp); assert!(ph.is_none()); flags.assert_committed();
}

#[tokio::test]
async fn update_user_returns_version_conflict_when_no_row_affected() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_partial().returning(|_id, _v, _fn, _sp, _p, _ph, _ia, _w, _ub| Ok(0));
    }).await;
    let res = svc.update_user(101, &make_update_req(), &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}

#[tokio::test]
async fn update_user_rejects_empty_password() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id)))); }).await;
    let mut req = make_update_req(); req.password = Some("".into());
    assert!(matches!(svc.update_user(101, &req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn update_user_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.update_user(999, &make_update_req(), &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

// ===========================================================================
// 5. deactivate_user（4 例）
// ===========================================================================

#[tokio::test]
async fn deactivate_user_soft_deletes_and_returns_inactive_out() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_soft_delete().returning(|_, _, _, _| Ok(1));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let out = svc.deactivate_user(101, &manager_current()).await.unwrap();
    assert!(!out.is_active); assert_eq!(out.version, 1); assert!(out.updated_at >= now());
    flags.assert_committed();
}

#[tokio::test]
async fn deactivate_user_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.deactivate_user(101, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn deactivate_user_returns_version_conflict_when_no_row_affected() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_soft_delete().returning(|_, _, _, _| Ok(0));
    }).await;
    let res = svc.deactivate_user(101, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}

#[tokio::test]
async fn deactivate_user_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.deactivate_user(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

// ===========================================================================
// 6. change_own_password（6 例）
// ===========================================================================

#[tokio::test]
async fn change_own_password_succeeds_and_clears_session_after_commit() {
    let real_hash = password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    let called = Arc::new(AtomicBool::new(false));
    let cc = called.clone();
    session.expect_delete_all_user_sessions().times(1).returning(move |uid| {
        assert!(fc.committed.load(Ordering::SeqCst), "session delete 应在 commit 后");
        assert_eq!(uid, 42);
        cc.store(true, Ordering::SeqCst); Ok(())
    });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    let current = CurrentUser { id: 42, username: "alice".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false };
    svc.change_own_password(42, "oldpass", "newpass", &current).await.unwrap();
    assert!(called.load(Ordering::SeqCst)); flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_manager_can_reset_other_user() {
    let real_hash = password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    session.expect_delete_all_user_sessions().times(1).returning(move |_| { assert!(fc.committed.load(Ordering::SeqCst)); Ok(()) });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    svc.change_own_password(42, "oldpass", "newpass", &manager_current()).await.unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_rejects_non_self_non_manager() {
    let svc = guard_svc();
    let res = svc.change_own_password(42, "oldpass", "newpass", &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn change_own_password_rejects_wrong_old_password() {
    let real_hash = password::hash("real-old").unwrap();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    }).await;
    let current = CurrentUser { id: 42, username: "alice".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false };
    let res = svc.change_own_password(42, "wrong-old", "newpass", &current).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::OLD_PASSWORD_MISMATCH));
    flags.assert_not_committed();
}

#[tokio::test]
async fn change_own_password_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let current = CurrentUser { id: 42, username: "alice".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false };
    let res = svc.change_own_password(42, "oldpass", "newpass", &current).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn change_own_password_rejects_empty_new_password() {
    // 守卫类：empty password 在 `uow_provider.begin()` **之前**就拒了，begin×0。
    let svc = guard_svc();
    let current = CurrentUser { id: 42, username: "alice".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false };
    let res = svc.change_own_password(42, "oldpass", "", &current).await;
    assert!(matches!(res, Err(AppError::Validation(_))));
}

// ===========================================================================
// 7. admin_reset_password（4 例）
// ===========================================================================

#[tokio::test]
async fn admin_reset_password_succeeds_and_clears_session_after_commit() {
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    session.expect_delete_all_user_sessions().times(1).returning(move |_| { assert!(fc.committed.load(Ordering::SeqCst)); Ok(()) });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    let out = svc.admin_reset_password(42, &manager_current()).await.unwrap();
    assert_eq!(out.id, 42); flags.assert_committed();
}

#[tokio::test]
async fn admin_reset_password_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.admin_reset_password(42, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn admin_reset_password_returns_version_conflict_when_no_row_affected() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(0));
    }).await;
    let res = svc.admin_reset_password(42, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}

#[tokio::test]
async fn admin_reset_password_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.admin_reset_password(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

// ===========================================================================
// 8. list_user_roles（4 例）
// ===========================================================================

#[tokio::test]
async fn list_user_roles_returns_roles_for_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|id| Ok(vec![make_user_role_row(1, id, "MANAGER")]));
    }).await;
    let roles = svc.list_user_roles(42, &manager_current()).await.unwrap();
    assert_eq!(roles.len(), 1); assert_eq!(roles[0].role, "MANAGER");
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_user_roles_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.list_user_roles(42, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn list_user_roles_returns_not_found_when_user_missing() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|_| Ok(None)); }).await;
    let res = svc.list_user_roles(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_user_roles_returns_empty_list_when_no_roles() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let roles = svc.list_user_roles(42, &manager_current()).await.unwrap();
    assert!(roles.is_empty()); flags.assert_not_committed();
}

// ===========================================================================
// 9. add_role（7 例）
// ===========================================================================

#[tokio::test]
async fn add_role_succeeds_for_non_shelf_role() {
    // capture insert.id via Arc<Mutex<Option<i64>>> 供 list_by_user 回写
    let captured_id: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let cap2 = captured_id.clone();
    let (svc, flags) = write_svc(move |uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_exists_same_scope().returning(|_, _, _, _| Ok(false));
        uow.user_role_repo.expect_create().returning(move |ins| { *cap2.lock().unwrap() = Some(ins.id); Ok(()) });
        uow.user_role_repo.expect_list_by_user().returning(move |uid| {
            let id = captured_id.lock().unwrap().unwrap_or(0);
            Ok(vec![make_user_role_row(id, uid, "CLERK")])
        });
    }).await;
    let req = UserAddRoleRequest { role: Role::Clerk, scope_type: None, scope_id: None };
    let out = svc.add_role(42, &req, &manager_current()).await.unwrap();
    assert_eq!(out.role, "CLERK");
    flags.assert_committed();
}

#[tokio::test]
async fn add_role_requires_manager_role() {
    let svc = guard_svc();
    let req = UserAddRoleRequest { role: Role::Clerk, scope_type: None, scope_id: None };
    let res = svc.add_role(42, &req, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn add_role_shelf_account_without_scope_fails_validation() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id)))); }).await;
    let req = UserAddRoleRequest { role: Role::ShelfAccount, scope_type: None, scope_id: None };
    assert!(matches!(svc.add_role(42, &req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_non_shelf_role_with_scope_fails_validation() {
    let (svc, flags) = write_svc(|uow| { uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id)))); }).await;
    let req = UserAddRoleRequest { role: Role::Clerk, scope_type: Some("shelf".into()), scope_id: Some(7) };
    assert!(matches!(svc.add_role(42, &req, &manager_current()).await, Err(AppError::Validation(_))));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_shelf_missing() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.shelf_repo.expect_get_by_id().returning(|_| Ok(None));
    }).await;
    let req = UserAddRoleRequest { role: Role::ShelfAccount, scope_type: Some("shelf".into()), scope_id: Some(999) };
    let res = svc.add_role(42, &req, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_zone_invalid() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.shelf_repo.expect_get_by_id().returning(|id| Ok(Some(make_shelf(id, "STORAGE", true))));
    }).await;
    let req = UserAddRoleRequest { role: Role::ShelfAccount, scope_type: Some("shelf".into()), scope_id: Some(7) };
    let res = svc.add_role(42, &req, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_returns_duplicate_role_409() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_exists_same_scope().returning(|_, _, _, _| Ok(true));
    }).await;
    let req = UserAddRoleRequest { role: Role::Clerk, scope_type: None, scope_id: None };
    let res = svc.add_role(42, &req, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_DUPLICATE));
    flags.assert_not_committed();
}

// ===========================================================================
// 10. remove_role（5 例）
// ===========================================================================

#[tokio::test]
async fn remove_role_succeeds() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_get_by_id().returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        uow.user_role_repo.expect_soft_delete().returning(|_, _, _, _| Ok(1));
    }).await;
    svc.remove_role(42, 99, &manager_current()).await.unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn remove_role_requires_manager_role() {
    let svc = guard_svc();
    let res = svc.remove_role(42, 99, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn remove_role_returns_not_found_when_role_missing() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_get_by_id().returning(|_| Ok(None));
    }).await;
    let res = svc.remove_role(42, 999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn remove_role_returns_not_found_when_role_belongs_to_other_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_get_by_id().returning(|id| Ok(Some(make_user_role(id, 99, "MANAGER"))));
    }).await;
    let res = svc.remove_role(42, 100, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn remove_role_returns_version_conflict_when_no_row_affected() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_get_by_id().returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        uow.user_role_repo.expect_soft_delete().returning(|_, _, _, _| Ok(0));
    }).await;
    let res = svc.remove_role(42, 99, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}

// ===========================================================================
// 11. menus_for_roles（3 例，helper）
// ===========================================================================

fn make_svc_for_helper() -> Arc<UserService> { make_svc(Arc::new(MockUowProvider::new()), make_session()) }

#[tokio::test]
async fn menus_for_roles_returns_tree_for_active_menus() {
    let (mut uow, _flags) = MockUnitOfWork::new();
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![
        make_menu(1, None, "ROOT", 0), make_menu(2, Some(1), "CHILD-A", 0), make_menu(3, Some(1), "CHILD-B", 1),
    ]));
    let svc = make_svc_for_helper();
    let tree: Vec<MenuNodeOut> = svc.menus_for_roles(&mut uow, &[Role::Manager]).await.unwrap();
    assert_eq!(tree.len(), 1); assert_eq!(tree[0].code, "ROOT"); assert_eq!(tree[0].children.len(), 2);
}

#[tokio::test]
async fn menus_for_roles_returns_empty_tree_for_empty_role_list() {
    let (mut uow, _flags) = MockUnitOfWork::new();
    uow.menu_repo.expect_list_active_for_roles().returning(|roles| { assert!(roles.is_empty()); Ok(vec![]) });
    let svc = make_svc_for_helper();
    let tree = svc.menus_for_roles(&mut uow, &[]).await.unwrap();
    assert!(tree.is_empty());
}

#[tokio::test]
async fn menus_for_roles_propagates_menu_repo_error() {
    let (mut uow, _flags) = MockUnitOfWork::new();
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Err(test_db_error()));
    let svc = make_svc_for_helper();
    let res = svc.menus_for_roles(&mut uow, &[Role::Manager]).await;
    assert!(matches!(res, Err(AppError::Database(_))));
}

// ===========================================================================
// 12. current_user_out（2 例，helper）
// ===========================================================================

#[tokio::test]
async fn current_user_out_returns_user_with_menus() {
    let (mut uow, _flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![make_menu(1, None, "ROOT", 0)]));
    let svc = make_svc_for_helper();
    let current = manager_current();
    let out: CurrentUserOut = svc.current_user_out(&mut uow, &current).await.unwrap();
    assert_eq!(out.id, current.id); assert_eq!(out.username, format!("user{}", current.id));
    assert_eq!(out.roles, vec!["MANAGER".to_string()]); assert_eq!(out.menus.len(), 1);
}

#[tokio::test]
async fn current_user_out_returns_not_found_when_user_missing() {
    let (mut uow, _flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|_| Ok(None));
    let svc = make_svc_for_helper();
    let res = svc.current_user_out(&mut uow, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

// ===========================================================================
// 边界 7 例
// ===========================================================================

#[tokio::test]
async fn list_users_with_zero_limit_clamps_to_one() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_ul, _ia, limit, _o| { assert_eq!(limit, 1); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    let out = svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: Some(0), offset: Some(0) }, &manager_current()).await.unwrap();
    assert_eq!(out.limit, 1); flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_with_negative_offset_clamps_to_zero() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|_ul, _ia, _l, offset| { assert_eq!(offset, 0); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    let out = svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: Some(10), offset: Some(-5) }, &manager_current()).await.unwrap();
    assert_eq!(out.offset, 0); flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_with_all_none_query_uses_defaults() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_list_with_filters().returning(|ul, ia, lim, off| { assert!(ul.is_none()); assert!(ia.is_none()); assert_eq!(lim, 50); assert_eq!(off, 0); Ok(vec![]) });
        uow.user_repo.expect_count_with_filters().returning(|_, _| Ok(0));
    }).await;
    svc.list_users(&UserListQuery { username_like: None, is_active: None, limit: None, offset: None }, &manager_current()).await.unwrap();
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_trims_and_lowercases_username_and_full_name() {
    let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_username().returning(|_| Ok(None));
        uow.user_repo.expect_create().returning(move |user| { *captured_c.lock().unwrap() = Some((user.username.clone(), user.full_name.clone())); Ok(()) });
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let req = UserCreateRequest { username: "  Alice@EXAMPLE.COM  ".into(), password: "secret123".into(), full_name: "  Alice Wonderland  ".into(), phone: None };
    svc.create_user(&req, &manager_current()).await.unwrap();
    let (u, f) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(u, "alice@example.com"); assert_eq!(f, "Alice Wonderland");
    flags.assert_committed();
}

#[tokio::test]
async fn update_user_with_all_none_fields_commits_without_changes() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_partial().returning(|_id, _v, fn_, sp, _p, _ph, _ia, _w, _ub| { assert!(fn_.is_none()); assert!(!sp); Ok(1) });
        uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    }).await;
    let req = UserUpdateRequest { full_name: None, phone: None, password: None, is_active: None };
    svc.update_user(101, &req, &manager_current()).await.unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_session_failure_is_swallowed() {
    let real_hash = password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    session.expect_delete_all_user_sessions().times(1).returning(|_| Err(AppError::internal("redis down")));
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    let current = CurrentUser { id: 42, username: "alice".into(), roles: vec![Role::Clerk], shelf_ids: vec![], shelf_wildcard: false };
    svc.change_own_password(42, "oldpass", "newpass", &current).await.unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn admin_reset_password_session_failure_is_swallowed() {
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![]));
    let mut session = MockSessionStore::new();
    session.expect_delete_all_user_sessions().times(1).returning(|_| Err(AppError::internal("redis down")));
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    svc.admin_reset_password(42, &manager_current()).await.unwrap();
    flags.assert_committed();
}
