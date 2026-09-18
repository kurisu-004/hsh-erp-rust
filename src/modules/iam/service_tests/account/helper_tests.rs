//! iam AccountService 助手方法 + 边界单测（13 例 = 3+2+8）
//!
//! 覆盖 `AccountService::menus_for_roles` / `current_user_out` 助手 + 边界用例（list_users 0/负 limit /
//! 全 None query / create_user 大小写归一 / update_user 全 None / 改密 session 失败 /
//! add_role 货架停用）。
//! 共享 helper 见 `super::helpers`。

use std::sync::{Arc, Mutex};

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::MockSessionStore;
use crate::modules::iam::dto::{CurrentUserOut, MenuNodeOut, UserAddRoleRequest, UserCreateRequest, UserListQuery, UserUpdateRequest};
use crate::modules::iam::uow::test_support::{MockIamUnitOfWork, MockIamUowProvider, provider_returning};
use crate::shared::error::{code, AppError};

use super::helpers::{
    make_menu, make_svc, make_session, make_shelf, make_user, make_user_with_hash,
    manager_current, test_db_error, write_svc,
};

// ===========================================================================
// 11. menus_for_roles（3 例，helper）
// ===========================================================================

fn make_svc_for_helper() -> Arc<crate::modules::iam::service::AccountService> {
    make_svc(Arc::new(MockIamUowProvider::new()), make_session())
}

#[tokio::test]
async fn menus_for_roles_returns_tree_for_active_menus() {
    let (mut uow, _flags) = MockIamUnitOfWork::new();
    uow.menu_repo.expect_list_active_for_roles().returning(|_| {
        Ok(vec![
            make_menu(1, None, "ROOT", 0),
            make_menu(2, Some(1), "CHILD-A", 0),
            make_menu(3, Some(1), "CHILD-B", 1),
        ])
    });
    let svc = make_svc_for_helper();
    let tree: Vec<MenuNodeOut> = svc
        .menus_for_roles(&mut uow, &[Role::Manager])
        .await
        .unwrap();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].code, "ROOT");
    assert_eq!(tree[0].children.len(), 2);
}

#[tokio::test]
async fn menus_for_roles_returns_empty_tree_for_empty_role_list() {
    let (mut uow, _flags) = MockIamUnitOfWork::new();
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|roles| {
            assert!(roles.is_empty());
            Ok(vec![])
        });
    let svc = make_svc_for_helper();
    let tree = svc.menus_for_roles(&mut uow, &[]).await.unwrap();
    assert!(tree.is_empty());
}

#[tokio::test]
async fn menus_for_roles_propagates_menu_repo_error() {
    let (mut uow, _flags) = MockIamUnitOfWork::new();
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Err(test_db_error()));
    let svc = make_svc_for_helper();
    let res = svc.menus_for_roles(&mut uow, &[Role::Manager]).await;
    assert!(matches!(res, Err(AppError::Database(_))));
}

// ===========================================================================
// 12. current_user_out（2 例，helper）
// ===========================================================================

#[tokio::test]
async fn current_user_out_returns_user_with_menus() {
    let (mut uow, _flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(|id| Ok(Some(make_user(id))));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![make_menu(1, None, "ROOT", 0)]));
    let svc = make_svc_for_helper();
    let current = manager_current();
    let out: CurrentUserOut = svc.current_user_out(&mut uow, &current).await.unwrap();
    assert_eq!(out.id, current.id);
    assert_eq!(out.username, format!("user{}", current.id));
    assert_eq!(out.roles, vec!["MANAGER".to_string()]);
    assert_eq!(out.menus.len(), 1);
}

#[tokio::test]
async fn current_user_out_returns_not_found_when_user_missing() {
    let (mut uow, _flags) = MockIamUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|_| Ok(None));
    let svc = make_svc_for_helper();
    let res = svc.current_user_out(&mut uow, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

// ===========================================================================
// 边界 8 例
// ===========================================================================

#[tokio::test]
async fn list_users_with_zero_limit_clamps_to_one() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_list_with_filters()
            .returning(|_ul, _ia, limit, _o| {
                assert_eq!(limit, 1);
                Ok(vec![])
            });
        uow.user_repo
            .expect_count_with_filters()
            .returning(|_, _| Ok(0));
    })
    .await;
    let out = svc
        .list_users(
            &UserListQuery {
                username_like: None,
                is_active: None,
                limit: Some(0),
                offset: Some(0),
            },
            &manager_current(),
        )
        .await
        .unwrap();
    assert_eq!(out.limit, 1);
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_with_negative_offset_clamps_to_zero() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_list_with_filters()
            .returning(|_ul, _ia, _l, offset| {
                assert_eq!(offset, 0);
                Ok(vec![])
            });
        uow.user_repo
            .expect_count_with_filters()
            .returning(|_, _| Ok(0));
    })
    .await;
    let out = svc
        .list_users(
            &UserListQuery {
                username_like: None,
                is_active: None,
                limit: Some(10),
                offset: Some(-5),
            },
            &manager_current(),
        )
        .await
        .unwrap();
    assert_eq!(out.offset, 0);
    flags.assert_not_committed();
}

#[tokio::test]
async fn list_users_with_all_none_query_uses_defaults() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_list_with_filters()
            .returning(|ul, ia, lim, off| {
                assert!(ul.is_none());
                assert!(ia.is_none());
                assert_eq!(lim, 50);
                assert_eq!(off, 0);
                Ok(vec![])
            });
        uow.user_repo
            .expect_count_with_filters()
            .returning(|_, _| Ok(0));
    })
    .await;
    svc.list_users(
        &UserListQuery {
            username_like: None,
            is_active: None,
            limit: None,
            offset: None,
        },
        &manager_current(),
    )
    .await
    .unwrap();
    flags.assert_not_committed();
}

#[tokio::test]
async fn create_user_trims_and_lowercases_username_and_full_name() {
    let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_username()
            .returning(|_| Ok(None));
        uow.user_repo.expect_create().returning(move |user| {
            *captured_c.lock().unwrap() = Some((user.username.clone(), user.full_name.clone()));
            Ok(())
        });
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_role_repo
            .expect_list_by_user()
            .returning(|_| Ok(vec![]));
    })
    .await;
    let req = UserCreateRequest {
        username: "  Alice@EXAMPLE.COM  ".into(),
        password: "secret123".into(),
        full_name: "  Alice Wonderland  ".into(),
        phone: None,
    };
    svc.create_user(&req, &manager_current()).await.unwrap();
    let (u, f) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(u, "alice@example.com");
    assert_eq!(f, "Alice Wonderland");
    flags.assert_committed();
}

#[tokio::test]
async fn update_user_with_all_none_fields_commits_without_changes() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.user_repo.expect_update_partial().returning(
            |_id, _v, fn_, sp, _p, _ph, _ia, _w, _ub| {
                assert!(fn_.is_none());
                assert!(!sp);
                Ok(1)
            },
        );
        uow.user_role_repo
            .expect_list_by_user()
            .returning(|_| Ok(vec![]));
    })
    .await;
    let req = UserUpdateRequest {
        full_name: None,
        phone: None,
        password: None,
        is_active: None,
    };
    svc.update_user(101, &req, &manager_current())
        .await
        .unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn change_own_password_session_failure_is_swallowed() {
    let real_hash = password::hash("oldpass").unwrap();
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |id| Ok(Some(make_user_with_hash(id, real_hash.clone()))));
    uow.user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    let mut session = MockSessionStore::new();
    session
        .expect_delete_all_user_sessions()
        .times(1)
        .returning(|_| Err(AppError::internal("redis down")));
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
    flags.assert_committed();
}

#[tokio::test]
async fn admin_reset_password_session_failure_is_swallowed() {
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
    session
        .expect_delete_all_user_sessions()
        .times(1)
        .returning(|_| Err(AppError::internal("redis down")));
    let svc = make_svc(provider_returning(uow), Arc::new(session));
    svc.admin_reset_password(42, &manager_current())
        .await
        .unwrap();
    flags.assert_committed();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_shelf_inactive() {
    let (svc, flags) = write_svc(|uow| {
        uow.user_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        uow.shelf_repo
            .expect_get_by_id()
            .returning(|id| Ok(Some(make_shelf(id, "PRODUCTION", false))));
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

// 抑制 import 警告：IamUowProvider trait 在本文件只有 provider_returning 隐式使用。
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _ = std::marker::PhantomData::<MockIamUnitOfWork>;
}