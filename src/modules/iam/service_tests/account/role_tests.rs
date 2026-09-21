//! iam AccountService 角色管理单测（15 例 = 4+6+5）
//!
//! 覆盖 `AccountService::list_user_roles` / `add_role` / `remove_role` 三方法。
//! 共享 helper 见 `super::helpers`。
//!
//! 2026-09-21 事务分层重构后：service 不再 commit/rollback，时序断言删除。

use std::sync::{Arc, Mutex};

use crate::auth::rbac::Role;
use crate::modules::iam::dto::UserAddRoleRequest;
use crate::shared::error::{AppError, code};

use super::helpers::{
    clerk_current, guard_repo, make_account_service, make_repo_with, make_shelf, make_user,
    make_user_role, make_user_role_row, manager_current,
};

// ===========================================================================
// 1. list_user_roles（4 例）
// ===========================================================================

#[tokio::test]
async fn list_user_roles_returns_roles_for_user() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user()
            .returning(|id| Ok(vec![make_user_role_row(1, id, "MANAGER")]));
    });
    let roles = svc
        .list_user_roles(repo, 42, &manager_current())
        .await
        .unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].role, "MANAGER");
}

#[tokio::test]
async fn list_user_roles_requires_manager_role() {
    let (svc, repo) = guard_repo();
    let res = svc
        .list_user_roles(repo, 42, &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn list_user_roles_returns_not_found_when_user_missing() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc
        .list_user_roles(repo, 999, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

#[tokio::test]
async fn list_user_roles_returns_empty_list_when_no_roles() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let roles = svc
        .list_user_roles(repo, 42, &manager_current())
        .await
        .unwrap();
    assert!(roles.is_empty());
}

// ===========================================================================
// 2. add_role（6 例）
// ===========================================================================

#[tokio::test]
async fn add_role_succeeds_for_non_shelf_role() {
    // capture insert.id via Arc<Mutex<Option<i64>>> 供 list_by_user 回写
    let captured_id: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let cap2 = captured_id.clone();
    let svc = make_account_service();
    let repo = make_repo_with(move |r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_exists_same_scope().returning(|_, _, _, _| Ok(false));
        r.expect_role_create().returning(move |ins| {
            *cap2.lock().unwrap() = Some(ins.id);
            Ok(())
        });
        r.expect_list_by_user().returning(move |uid| {
            let id = captured_id.lock().unwrap().unwrap_or(0);
            Ok(vec![make_user_role_row(id, uid, "CLERK")])
        });
    });
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
    let out = svc
        .add_role(repo, 42, &req, &manager_current())
        .await
        .unwrap();
    assert_eq!(out.role, "CLERK");
}

#[tokio::test]
async fn add_role_requires_manager_role() {
    let (svc, repo) = guard_repo();
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
    let res = svc.add_role(repo, 42, &req, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn add_role_shelf_account_without_scope_fails_validation() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    });
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: None,
        scope_id: None,
    };
    assert!(matches!(
        svc.add_role(repo, 42, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn add_role_non_shelf_role_with_scope_fails_validation() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    });
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: Some("shelf".into()),
        scope_id: Some(7),
    };
    assert!(matches!(
        svc.add_role(repo, 42, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_shelf_missing() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_shelf_get_by_id().returning(|_| Ok(None));
    });
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: Some("shelf".into()),
        scope_id: Some(999),
    };
    let res = svc
        .add_role(repo, 42, &req, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_zone_invalid() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_shelf_get_by_id()
            .returning(|id| Ok(Some(make_shelf(id, "STORAGE", true))));
    });
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: Some("shelf".into()),
        scope_id: Some(7),
    };
    let res = svc
        .add_role(repo, 42, &req, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
}

#[tokio::test]
async fn add_role_returns_duplicate_role_409() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_exists_same_scope().returning(|_, _, _, _| Ok(true));
    });
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
    let res = svc
        .add_role(repo, 42, &req, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_DUPLICATE));
}

// ===========================================================================
// 3. remove_role（5 例）
// ===========================================================================

#[tokio::test]
async fn remove_role_succeeds() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_role_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        r.expect_role_soft_delete().returning(|_, _, _, _| Ok(1));
    });
    svc.remove_role(repo, 42, 99, &manager_current())
        .await
        .unwrap();
}

#[tokio::test]
async fn remove_role_requires_manager_role() {
    let (svc, repo) = guard_repo();
    let res = svc
        .remove_role(repo, 42, 99, &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn remove_role_returns_not_found_when_role_missing() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_role_get_by_id().returning(|_| Ok(None));
    });
    let res = svc
        .remove_role(repo, 42, 999, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
}

#[tokio::test]
async fn remove_role_returns_not_found_when_role_belongs_to_other_user() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_role_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 99, "MANAGER"))));
    });
    let res = svc
        .remove_role(repo, 42, 100, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
}

#[tokio::test]
async fn remove_role_returns_version_conflict_when_no_row_affected() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_role_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        r.expect_role_soft_delete().returning(|_, _, _, _| Ok(0));
    });
    let res = svc
        .remove_role(repo, 42, 99, &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
}
