//! iam AccountService 改密单测（10 例 = 6+4）
//!
//! 覆盖 `AccountService::change_own_password` / `admin_reset_password` 两方法。
//! 共享 helper 见 `super::helpers`。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::MockSessionStore;
use crate::modules::iam::uow::test_support::{MockIamUnitOfWork, provider_returning};
use crate::shared::error::{code, AppError};

use super::helpers::{
    clerk_current, guard_svc, make_svc, make_user, make_user_with_hash, manager_current,
    write_svc,
};

// ===========================================================================
// 6. change_own_password（6 例）
// ===========================================================================

#[tokio::test]
async fn change_own_password_succeeds_and_clears_session_after_commit() {
    let real_hash = crate::auth::password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    let called = Arc::new(AtomicBool::new(false));
    let cc = called.clone();
    session
        .expect_delete_all_user_sessions()
        .times(1)
        .returning(move |uid| {
            assert!(
                fc.committed.load(Ordering::SeqCst),
                "session delete 应在 commit 后"
            );
            assert_eq!(uid, 42);
            cc.store(true, Ordering::SeqCst);
            Ok(())
        });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    svc.change_own_password(42, "oldpass", "newpass", &current)
        .await
        .unwrap();
    assert!(called.load(Ordering::SeqCst));
    flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_manager_can_reset_other_user() {
    let real_hash = crate::auth::password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    session
        .expect_delete_all_user_sessions()
        .times(1)
        .returning(move |_| {
            assert!(fc.committed.load(Ordering::SeqCst));
            Ok(())
        });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    svc.change_own_password(42, "oldpass", "newpass", &manager_current())
        .await
        .unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_rejects_non_self_non_manager() {
    let svc = guard_svc();
    let res = svc
        .change_own_password(42, "oldpass", "newpass", &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn change_own_password_rejects_wrong_old_password() {
    let real_hash = crate::auth::password::hash("real-old").unwrap();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    })
    .await;
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc
        .change_own_password(42, "wrong-old", "newpass", &current)
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::OLD_PASSWORD_MISMATCH));
    flags.assert_not_committed();
}

#[tokio::test]
async fn change_own_password_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|_| Ok(None));
    })
    .await;
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc
        .change_own_password(42, "oldpass", "newpass", &current)
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn change_own_password_rejects_empty_new_password() {
    // 守卫类：empty password 在 `uow_provider.begin()` **之前**就拒了，begin×0。
    let svc = guard_svc();
    let current = CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let res = svc
        .change_own_password(42, "oldpass", "", &current)
        .await;
    assert!(matches!(res, Err(AppError::Validation(_))));
}

// ===========================================================================
// 7. admin_reset_password（4 例）
// ===========================================================================

#[tokio::test]
async fn admin_reset_password_succeeds_and_clears_session_after_commit() {
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(|id| Ok(Some(make_user(id))));
    uow.user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![]));
    let mut session = MockSessionStore::new();
    let fc = flags.clone();
    session
        .expect_delete_all_user_sessions()
        .times(1)
        .returning(move |_| {
            assert!(fc.committed.load(Ordering::SeqCst));
            Ok(())
        });
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    let out = svc
        .admin_reset_password(42, &manager_current())
        .await
        .unwrap();
    assert_eq!(out.id, 42);
    flags.assert_committed();
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
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_repo
            .expect_update_password_and_rotate()
            .returning(|_, _, _, _, _| Ok(0));
    })
    .await;
    let res = svc.admin_reset_password(42, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}

#[tokio::test]
async fn admin_reset_password_returns_not_found_for_missing_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|_| Ok(None));
    })
    .await;
    let res = svc.admin_reset_password(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

// 抑制 import 警告
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _ = std::marker::PhantomData::<MockIamUnitOfWork>;
}