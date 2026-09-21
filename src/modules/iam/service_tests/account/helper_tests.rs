//! iam AccountService 助手方法 + 边界单测（8 例 = 3+5）
//!
//! 覆盖 `AccountService::menus_for_roles` 助手 + 边界用例
//! （list_users 0/负 limit、全 None query、create_user 大小写归一、update_user 全 None、
//! add_role 货架停用）。
//! 共享 helper 见 `super::helpers`。
//!
//! 2026-09-21 事务分层重构后：service 不再 commit/rollback；helpers 改为收 `&mut R: IamRepo`，
//! `menus_for_roles` 不再要求 `&mut dyn IamUnitOfWork`。
//! 2026-09-22 删 `PgIamRepo` 转发壳后：以上描述专指 helpers（私有 helper 仍借 `&mut R` 以便
//! 多次调 trait 方法）；service 公开方法形参已改为 by-value `mut repo: R`，不可与 helpers 混用。
//! 2026-09-22 删 `AccountService::current_user_out`（死方法，零生产调用方），对应 2 例
//! 删除。

use std::sync::{Arc, Mutex};

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::iam::dto::{
    MenuNodeOut, UserAddRoleRequest, UserCreateRequest, UserListQuery, UserUpdateRequest,
};
use crate::shared::error::{AppError, code};

use super::helpers::{
    make_account_service, make_menu, make_repo_with, make_shelf, make_user, manager_current,
    test_db_error,
};

// ===========================================================================
// 1. menus_for_roles（3 例，helper）
// ===========================================================================

#[tokio::test]
async fn menus_for_roles_returns_tree_for_active_menus() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_active_for_roles().returning(|_| {
            Ok(vec![
                make_menu(1, None, "ROOT", 0),
                make_menu(2, Some(1), "CHILD-A", 0),
                make_menu(3, Some(1), "CHILD-B", 1),
            ])
        });
    });
    let tree: Vec<MenuNodeOut> = svc
        .menus_for_roles(&mut repo, &[Role::Manager])
        .await
        .unwrap();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].code, "ROOT");
    assert_eq!(tree[0].children.len(), 2);
}

#[tokio::test]
async fn menus_for_roles_returns_empty_tree_for_empty_role_list() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_active_for_roles().returning(|roles| {
            assert!(roles.is_empty());
            Ok(vec![])
        });
    });
    let tree = svc.menus_for_roles(&mut repo, &[]).await.unwrap();
    assert!(tree.is_empty());
}

#[tokio::test]
async fn menus_for_roles_propagates_menu_repo_error() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_active_for_roles()
            .returning(|_| Err(test_db_error()));
    });
    let res = svc.menus_for_roles(&mut repo, &[Role::Manager]).await;
    assert!(matches!(res, Err(AppError::Database(_))));
}

// ===========================================================================
// 边界 5 例
// ===========================================================================

#[tokio::test]
async fn list_users_with_zero_limit_clamps_to_one() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|_ul, _ia, limit, _o| {
                assert_eq!(limit, 1);
                Ok(vec![])
            });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    let out = svc
        .list_users(
            repo,
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
}

#[tokio::test]
async fn list_users_with_negative_offset_clamps_to_zero() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|_ul, _ia, _l, offset| {
                assert_eq!(offset, 0);
                Ok(vec![])
            });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    let out = svc
        .list_users(
            repo,
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
}

#[tokio::test]
async fn list_users_with_all_none_query_uses_defaults() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_list_with_filters().returning(|ul, ia, lim, off| {
            assert!(ul.is_none());
            assert!(ia.is_none());
            assert_eq!(lim, 50);
            assert_eq!(off, 0);
            Ok(vec![])
        });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    svc.list_users(
        repo,
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
}

#[tokio::test]
async fn create_user_trims_and_lowercases_username_and_full_name() {
    let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let svc = make_account_service();
    let repo = make_repo_with(move |r| {
        r.expect_get_by_username().returning(|_| Ok(None));
        r.expect_create().returning(move |user| {
            *captured_c.lock().unwrap() = Some((user.username.clone(), user.full_name.clone()));
            Ok(())
        });
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let req = UserCreateRequest {
        username: "  Alice@EXAMPLE.COM  ".into(),
        password: "secret123".into(),
        full_name: "  Alice Wonderland  ".into(),
        phone: None,
    };
    svc.create_user(repo, &req, &manager_current())
        .await
        .unwrap();
    let (u, f) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(u, "alice@example.com");
    assert_eq!(f, "Alice Wonderland");
}

#[tokio::test]
async fn update_user_with_all_none_fields_commits_without_changes() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_partial().returning(
            |_id, _v, fn_, sp, _p, _ph, _ia, _w, _ub| {
                assert!(fn_.is_none());
                assert!(!sp);
                Ok(1)
            },
        );
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let req = UserUpdateRequest {
        full_name: None,
        phone: None,
        password: None,
        is_active: None,
    };
    svc.update_user(repo, 101, &req, &manager_current())
        .await
        .unwrap();
}

#[tokio::test]
async fn add_role_shelf_account_returns_not_found_when_shelf_inactive() {
    let svc = make_account_service();
    let repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_shelf_get_by_id()
            .returning(|id| Ok(Some(make_shelf(id, "PRODUCTION", false))));
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

// 抑制 import 警告：password 在 make_user_with_hash 间接使用
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _ = password::hash;
    let _ = CurrentUser {
        id: 0,
        username: String::new(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
}
