//! iam AccountService CRUD 单测（27 例 = 6+4+7+7+4）
//!
//! 覆盖 `AccountService::list_users` / `get_user` / `create_user` / `update_user` /
//! `deactivate_user` 五方法。共享 helper 见 `super::helpers`。
//!
//! 2026-09-21 事务分层重构后：`write_svc(|uow| { ... })` 改为
//! `make_repo_with(|repo| { ... })`，service 调用从 `svc.xxx(...)` 改为
//! `svc.xxx(&mut repo, ...)`，事务 commit/rollback 时序断言整体删除。

use std::sync::{Arc, Mutex};

use crate::modules::iam::dto::{UserCreateRequest, UserListQuery, UserUpdateRequest};
use crate::shared::error::{AppError, code};

use super::helpers::{
    clerk_current, guard_repo, make_account_service, make_repo_with, make_user,
    make_user_with_username, manager_current,
};

// ===========================================================================
// 1. list_users（6 例）
// ===========================================================================

#[tokio::test]
async fn list_users_returns_paginated_list() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|_, _, _, _| Ok(vec![make_user(101), make_user(102)]));
        r.expect_count_with_filters().returning(|_, _| Ok(42));
        r.expect_list_by_user()
            .returning(|id| Ok(vec![super::helpers::make_user_role_row(1, id, "MANAGER")]));
    });
    let out = svc
        .list_users(
            &mut repo,
            &UserListQuery {
                username_like: None,
                is_active: None,
                limit: Some(20),
                offset: Some(0),
            },
            &manager_current(),
        )
        .await
        .unwrap();
    assert_eq!(out.total, 42);
    assert_eq!(out.items.len(), 2);
    assert_eq!(out.limit, 20);
    assert_eq!(out.offset, 0);
}

#[tokio::test]
async fn list_users_requires_manager_role() {
    let (svc, mut repo) = guard_repo();
    let res = svc
        .list_users(
            &mut repo,
            &UserListQuery {
                username_like: None,
                is_active: None,
                limit: None,
                offset: None,
            },
            &clerk_current(),
        )
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn list_users_clamps_limit_to_max_when_exceeded() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_with_filters().returning(|_ul, _ia, limit, _o| {
            assert_eq!(limit, 500);
            Ok(vec![])
        });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    let out = svc
        .list_users(
            &mut repo,
            &UserListQuery {
                username_like: None,
                is_active: None,
                limit: Some(10_000),
                offset: Some(0),
            },
            &manager_current(),
        )
        .await
        .unwrap();
    assert_eq!(out.limit, 500);
}

#[tokio::test]
async fn list_users_returns_empty_when_no_results() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|_, _, _, _| Ok(vec![]));
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    let out = svc
        .list_users(
            &mut repo,
            &UserListQuery {
                username_like: Some("nope".into()),
                is_active: None,
                limit: None,
                offset: None,
            },
            &manager_current(),
        )
        .await
        .unwrap();
    assert!(out.items.is_empty());
    assert_eq!(out.total, 0);
    assert_eq!(out.limit, 50);
    assert_eq!(out.offset, 0);
}

#[tokio::test]
async fn list_users_filters_by_is_active() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|_ul, is_active, _l, _o| {
                assert_eq!(is_active, Some(false));
                Ok(vec![])
            });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    svc.list_users(
        &mut repo,
        &UserListQuery {
            username_like: None,
            is_active: Some(false),
            limit: None,
            offset: None,
        },
        &manager_current(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn list_users_normalizes_blank_username_like_to_none() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_list_with_filters()
            .returning(|username_like, _ia, _l, _o| {
                assert!(username_like.is_none());
                Ok(vec![])
            });
        r.expect_count_with_filters().returning(|_, _| Ok(0));
    });
    svc.list_users(
        &mut repo,
        &UserListQuery {
            username_like: Some("   ".into()),
            is_active: None,
            limit: None,
            offset: None,
        },
        &manager_current(),
    )
    .await
    .unwrap();
}

// ===========================================================================
// 2. get_user（4 例）
// ===========================================================================

#[tokio::test]
async fn get_user_returns_user_with_roles() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user().returning(|id| {
            Ok(vec![super::helpers::make_user_role_row(1, id, "MANAGER")])
        });
    });
    let out = svc.get_user(&mut repo, 101, &manager_current()).await.unwrap();
    assert_eq!(out.id, 101);
    assert_eq!(out.roles.len(), 1);
    assert_eq!(out.roles[0].role, "MANAGER");
}

#[tokio::test]
async fn get_user_returns_not_found_when_user_missing() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc.get_user(&mut repo, 999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

#[tokio::test]
async fn get_user_requires_manager_role() {
    let (svc, mut repo) = guard_repo();
    let res = svc.get_user(&mut repo, 101, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn get_user_returns_not_found_for_deactivated_user() {
    // 软删后 get_by_id 返回 None（deleted_at IS NULL 过滤）→ service 翻 404
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc.get_user(&mut repo, 42, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

// ===========================================================================
// 3. create_user（7 例）
// ===========================================================================

fn make_create_req() -> UserCreateRequest {
    UserCreateRequest {
        username: "alice".into(),
        password: "secret123".into(),
        full_name: "Alice".into(),
        phone: None,
    }
}

#[tokio::test]
async fn create_user_succeeds_with_valid_inputs() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_username().returning(|_| Ok(None));
        r.expect_create().returning(|_| Ok(()));
        r.expect_get_by_id()
            .returning(|id| Ok(Some(make_user_with_username(id, "alice"))));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let out = svc
        .create_user(&mut repo, &make_create_req(), &manager_current())
        .await
        .unwrap();
    assert_eq!(out.username, "alice");
    assert!(out.is_active);
}

#[tokio::test]
async fn create_user_rejects_empty_username_after_trim() {
    let (svc, mut repo) = guard_repo();
    let mut req = make_create_req();
    req.username = "   ".into();
    assert!(matches!(
        svc.create_user(&mut repo, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn create_user_rejects_empty_password() {
    let (svc, mut repo) = guard_repo();
    let mut req = make_create_req();
    req.password = "".into();
    assert!(matches!(
        svc.create_user(&mut repo, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn create_user_rejects_empty_full_name_after_trim() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_username().returning(|_| Ok(None));
    });
    let mut req = make_create_req();
    req.full_name = "   ".into();
    assert!(matches!(
        svc.create_user(&mut repo, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn create_user_rejects_duplicate_username_as_409() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_username()
            .returning(|_| Ok(Some(make_user(42))));
    });
    let res = svc
        .create_user(&mut repo, &make_create_req(), &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::DUPLICATE_USERNAME));
}

#[tokio::test]
async fn create_user_injects_snowflake_id_into_insert() {
    let captured: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let svc = make_account_service();
    let mut repo = make_repo_with(move |r| {
        r.expect_get_by_username().returning(|_| Ok(None));
        r.expect_create().returning(move |user| {
            *captured_c.lock().unwrap() = Some(user.id);
            Ok(())
        });
        r.expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let out = svc
        .create_user(&mut repo, &make_create_req(), &manager_current())
        .await
        .unwrap();
    assert_eq!(*captured.lock().unwrap(), Some(out.id)); // service 内 next_id 一次，create 拿到 == out.id
}

#[tokio::test]
async fn create_user_hashes_password_before_insert() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let svc = make_account_service();
    let mut repo = make_repo_with(move |r| {
        r.expect_get_by_username().returning(|_| Ok(None));
        r.expect_create().returning(move |user| {
            *captured_c.lock().unwrap() = Some(user.password_hash.clone());
            Ok(())
        });
        r.expect_get_by_id()
            .returning(|id| Ok(Some(make_user(id))));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    svc.create_user(&mut repo, &make_create_req(), &manager_current())
        .await
        .unwrap();
    let hash = captured.lock().unwrap().clone().unwrap();
    assert!(hash.starts_with("$2"));
    assert_ne!(hash, "secret123");
}

// ===========================================================================
// 4. update_user（7 例）
// ===========================================================================

fn make_update_req() -> UserUpdateRequest {
    UserUpdateRequest {
        full_name: Some("Updated".into()),
        phone: None,
        password: None,
        is_active: None,
    }
}

#[tokio::test]
async fn update_user_succeeds_with_valid_inputs() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_partial()
            .returning(|_id, _v, _fn, _sp, _p, _ph, _ia, _w, _ub| Ok(1));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let out = svc
        .update_user(&mut repo, 101, &make_update_req(), &manager_current())
        .await
        .unwrap();
    assert_eq!(out.id, 101);
}

#[tokio::test]
async fn update_user_requires_manager_role() {
    let (svc, mut repo) = guard_repo();
    let res = svc
        .update_user(&mut repo, 101, &make_update_req(), &clerk_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn update_user_rejects_empty_full_name_after_trim() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    });
    let mut req = make_update_req();
    req.full_name = Some("   ".into());
    assert!(matches!(
        svc.update_user(&mut repo, 101, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn update_user_phone_explicit_empty_clears_phone() {
    let captured: super::helpers::PhoneCapture = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    let svc = make_account_service();
    let mut repo = make_repo_with(move |r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_partial().returning(
            move |_id, _v, _fn, set_phone, phone, _ph, _ia, _w, _ub| {
                *captured_c.lock().unwrap() = Some((set_phone, phone.map(String::from)));
                Ok(1)
            },
        );
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let mut req = make_update_req();
    req.phone = Some("".into());
    svc.update_user(&mut repo, 101, &req, &manager_current())
        .await
        .unwrap();
    let (sp, ph) = captured.lock().unwrap().as_ref().unwrap().clone();
    assert!(sp);
    assert!(ph.is_none());
}

#[tokio::test]
async fn update_user_returns_version_conflict_when_no_row_affected() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_update_partial()
            .returning(|_id, _v, _fn, _sp, _p, _ph, _ia, _w, _ub| Ok(0));
    });
    let res = svc
        .update_user(&mut repo, 101, &make_update_req(), &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
}

#[tokio::test]
async fn update_user_rejects_empty_password() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
    });
    let mut req = make_update_req();
    req.password = Some("".into());
    assert!(matches!(
        svc.update_user(&mut repo, 101, &req, &manager_current()).await,
        Err(AppError::Validation(_))
    ));
}

#[tokio::test]
async fn update_user_returns_not_found_for_missing_user() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc
        .update_user(&mut repo, 999, &make_update_req(), &manager_current())
        .await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}

// ===========================================================================
// 5. deactivate_user（4 例）
// ===========================================================================

#[tokio::test]
async fn deactivate_user_soft_deletes_and_returns_inactive_out() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_soft_delete().returning(|_, _, _, _| Ok(1));
        r.expect_list_by_user().returning(|_| Ok(vec![]));
    });
    let out = svc
        .deactivate_user(&mut repo, 101, &manager_current())
        .await
        .unwrap();
    assert!(!out.is_active);
    assert_eq!(out.version, 1);
    assert!(out.updated_at >= super::helpers::now());
}

#[tokio::test]
async fn deactivate_user_requires_manager_role() {
    let (svc, mut repo) = guard_repo();
    let res = svc.deactivate_user(&mut repo, 101, &clerk_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::FORBIDDEN));
}

#[tokio::test]
async fn deactivate_user_returns_version_conflict_when_no_row_affected() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|id| Ok(Some(make_user(id))));
        r.expect_soft_delete().returning(|_, _, _, _| Ok(0));
    });
    let res = svc.deactivate_user(&mut repo, 101, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::VERSION_CONFLICT));
}

#[tokio::test]
async fn deactivate_user_returns_not_found_for_missing_user() {
    let svc = make_account_service();
    let mut repo = make_repo_with(|r| {
        r.expect_get_by_id().returning(|_| Ok(None));
    });
    let res = svc.deactivate_user(&mut repo, 999, &manager_current()).await;
    assert!(matches!(res, Err(AppError::Biz { code: c, .. }) if c == code::USER_NOT_FOUND));
}
