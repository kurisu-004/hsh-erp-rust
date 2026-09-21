//! iam AccountService 改密单测（6 例 = 4+2）
//!
//! 覆盖 `AccountService::change_own_password` / `admin_reset_password` 两方法。
//! 共享 helper 见 `super::helpers`。
//!
//! 2026-09-21 事务分层重构后：
//! - 服务不再 commit/rollback → 删「session delete 在 commit 之后」时序断言。
//! - 服务不再调 `state.session.delete_all_user_sessions(...)` —— 移交 handler。
//!   `change_own_password` / `admin_reset_password` 仅做 DB 业务，session 清理测试
//!   改由 handler 层（`tests/iam_api.rs` HTTP 契约测试）覆盖。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::iam::repo::MockIamRepo;
use crate::shared::error::{AppError, code};

use super::helpers::{
    clerk_current, guard_repo, make_account_service, make_repo_with, make_user,
    make_user_with_hash, manager_current,
};

// ===========================================================================
// 1. change_own_password（4 例）
// ===========================================================================

#[tokio::test]
async fn change_own_password_succeeds_for_self() {
    let real_hash = crate::auth::password::hash("oldpass").unwrap();
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id()
            .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
        r.expect_update_password_and_rotate()
            .returning(|_, _, _, _, _| Ok(1));
    });
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    svc.change_own_password(&mut repo, 42, "oldpass", "newpass", &current)
        .await
        .unwrap();
}

#[tokio::test]
async fn change_own_password_manager_can_reset_other_user() {
    let real_hash = crate::auth::password::hash("oldpass").unwrap();
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id()
            .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
        r.expect_update_password_and_rotate()
            .returning(|_, _, _, _, _| Ok(1));
    });
    svc.change_own_password(&mut repo, 42, "oldpass", "newpass", &manager_current())
        .await
        .unwrap();
}

#[tokio::test]
async fn change_own_password_rejects_non_self_non_manager() {
    let (svc, mut repo) = guard_repo();
    let res = svc
        .change_own_password(&mut repo, 42, "oldpass", "newpass", &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn change_own_password_rejects_wrong_old_password() {
    let real_hash = crate::auth::password::hash("real-old").unwrap();
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id()
            .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    });
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc
        .change_own_password(&mut repo, 42, "wrong-old", "newpass", &current)
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::OLD_PASSWORD_MISMATCH));
}

#[tokio::test]
async fn change_own_password_returns_not_found_for_missing_user() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc
        .change_own_password(&mut repo, 42, "oldpass", "newpass", &current)
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

#[tokio::test]
async fn change_own_password_rejects_empty_new_password() {
    let (svc, mut repo) = guard_repo();
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc.change_own_password(&mut repo, 42, "oldpass", "", &current).await;
    assert!(matches!(res, Err(AppError::Validation(_))));
}

// ===========================================================================
// 2. admin_reset_password（2 例）
// ===========================================================================

#[tokio::test]
async fn admin_reset_password_succeeds() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_password_and_rotate()
            .returning(|_, _, _, _, _| Ok(1));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let out = svc
        .admin_reset_password(&mut repo, 42, &manager_current())
        .await
        .unwrap();
    assert_eq!(out.id, 42);
}

#[tokio::test]
async fn admin_reset_password_requires_manager_role() {
    let (svc, mut repo) = guard_repo();
    let res = svc
        .admin_reset_password(&mut repo, 42, &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn admin_reset_password_returns_version_conflict_when_no_row_affected() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_password_and_rotate()
            .returning(|_, _, _, _, _| Ok(0));
    });
    let res = svc
        .admin_reset_password(&mut repo, 42, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
}

#[tokio::test]
async fn admin_reset_password_returns_not_found_for_missing_user() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc
        .admin_reset_password(&mut repo, 999, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

// 抑制 import 警告
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _ = std::marker::PhantomData::<MockIamRepo>;
}
