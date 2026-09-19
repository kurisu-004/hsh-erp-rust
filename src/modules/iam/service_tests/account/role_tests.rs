//! iam AccountService 角色管理单测（16 例 = 4+7+5）
//!
//! 覆盖 `AccountService::list_user_roles` / `add_role` / `remove_role` 三方法。
//! 共享 helper 见 `super::helpers`。

use std::sync::{Arc, Mutex};

use crate::auth::rbac::Role;
use crate::modules::iam::dto::UserAddRoleRequest;
use crate::shared::error::{AppError, code};

use super::helpers::{
    clerk_current, guard_svc, make_shelf, make_user, make_user_role, make_user_role_row,
    manager_current, write_svc,
};

// ===========================================================================
// 8. list_user_roles（4 例）
// ===========================================================================

#[tokio::test]
async fn list_user_roles_returns_roles_for_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_list_by_user()
            .returning(|id| Ok(vec![make_user_role_row(1, id, "MANAGER")]));
    })
    .await;
    let roles = svc.list_user_roles(42, &manager_current()).await.unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].role, "MANAGER");
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
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo.expect_get_by_id().returning(|_| Ok(None));
    })
    .await;
    let res = svc.list_user_roles(999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_user_roles_returns_empty_list_when_no_roles() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_list_by_user()
            .returning(|_| Ok(vec![]));
    })
    .await;
    let roles = svc.list_user_roles(42, &manager_current()).await.unwrap();
    assert!(roles.is_empty());
    flags.assert_not_committed();
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
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_exists_same_scope()
            .returning(|_, _, _, _| Ok(false));
        uow.user_role_repo.expect_create().returning(move |ins| {
            *cap2.lock().unwrap() = Some(ins.id);
            Ok(())
        });
        uow.user_role_repo
            .expect_list_by_user()
            .returning(move |uid| {
                let id = captured_id.lock().unwrap().unwrap_or(0);
                Ok(vec![make_user_role_row(id, uid, "CLERK")])
            });
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
    let out = svc.add_role(42, &req, &manager_current()).await.unwrap();
    assert_eq!(out.role, "CLERK");
    flags.assert_committed();
}

#[tokio::test]
async fn add_role_requires_manager_role() {
    let svc = guard_svc();
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
    let res = svc.add_role(42, &req, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn add_role_shelf_account_without_scope_fails_validation() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: None,
        scope_id: None,
    };
    assert!(matches!(
        svc.add_role(42, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_non_shelf_role_with_scope_fails_validation() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: Some("shelf".into()),
        scope_id: Some(7),
    };
    assert!(matches!(
        svc.add_role(42, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_shelf_missing() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.shelf_repo.expect_get_by_id().returning(|_| Ok(None));
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: Some("shelf".into()),
        scope_id: Some(999),
    };
    let res = svc.add_role(42, &req, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_zone_invalid() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.shelf_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_shelf(id, "STORAGE", true))));
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::ShelfAccount,
        scope_type: Some("shelf".into()),
        scope_id: Some(7),
    };
    let res = svc.add_role(42, &req, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn add_role_returns_duplicate_role_409() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_exists_same_scope()
            .returning(|_, _, _, _| Ok(true));
    })
    .await;
    let req = UserAddRoleRequest {
        role: Role::Clerk,
        scope_type: None,
        scope_id: None,
    };
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
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        uow.user_role_repo
            .expect_soft_delete()
            .returning(|_, _, _, _| Ok(1));
    })
    .await;
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
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_get_by_id()
            .returning(|_| Ok(None));
    })
    .await;
    let res = svc.remove_role(42, 999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn remove_role_returns_not_found_when_role_belongs_to_other_user() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 99, "MANAGER"))));
    })
    .await;
    let res = svc.remove_role(42, 100, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::ROLE_NOT_FOUND));
    flags.assert_not_committed();
}

#[tokio::test]
async fn remove_role_returns_version_conflict_when_no_row_affected() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user_role(id, 42, "MANAGER"))));
        uow.user_role_repo
            .expect_soft_delete()
            .returning(|_, _, _, _| Ok(0));
    })
    .await;
    let res = svc.remove_role(42, 99, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
    flags.assert_not_committed();
}
