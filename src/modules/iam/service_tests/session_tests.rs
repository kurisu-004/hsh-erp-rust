//! iam 域 SessionService（原 AuthService）mock 单测
//!
//! 5 方法分组：login (9) / refresh (7) / me (5) / change_password (5) / logout (4)。
//!
//! ## 测试形态（2026-09-21 事务分层重构后）
//! 纯 `#[tokio::test]`，零数据库。service 方法签名 `<R: IamRepo>(&self, repo: &mut R, ...)`，
//! 单测用 `MockIamRepo` 直接注入；事务不在 service 层（handler 管），故无 commit 时序断言。
//!
//! ## 形态变化
//! - `login` / `refresh` 拆两阶段：第一阶段 `svc.login(repo, req) -> LoginPending`
//!   / `svc.refresh(repo, req) -> RefreshPending` 跑 DB；第二阶段
//!   `svc.complete_login(pending) -> LoginResponse` / `svc.complete_refresh(pending) -> LoginResponse`
//!   写 Redis session。service 层两阶段独立可测。
//! - `change_password` 直接 `svc.change_password(repo, user_id, req, &current)`，
//!   内部委托给 `account_service.change_own_password(repo, ...)`。
//!
//! 2026-09-19 IAM 域合并：从 `auth/service_tests.rs` 整体迁移过来，路径全改为
//! `/api/v2/iam/*`（虽是 service 级测试，不触 HTTP，但模块 doc 已对齐新路径）。
//!
//! 2026-09-21 事务分层重构：删 commit 时序断言；login / refresh 拆两阶段测试。

#![allow(clippy::needless_borrow, clippy::redundant_clone)]

use std::sync::Arc;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{MockSessionStore, SessionStore, hash_token};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::dto::ChangePasswordRequest;
use crate::modules::iam::repo::MockIamRepo;
use crate::modules::iam::service::AccountService;
use crate::modules::iam::service::SessionService;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::{
    clerk_current, make_refresh_token, make_session_service, manager_current, sample_menu,
    sample_role_row, sample_shelf, sample_user, test_state,
};

// ===========================================================================
// SessionService 装配 helper（2026-09-21 事务分层重构后：单 session_provider，
// account 共享同一 snowflake generator）
// ===========================================================================

/// 拼装 SessionService + AccountService + state：service 借注入 repo 跑 DB，
/// `complete_*` 阶段调 session store 写 Redis。
fn build(
    account: Arc<AccountService>,
    session: Arc<dyn SessionStore>,
) -> (Arc<SessionService>, Arc<AppState>) {
    let session_svc = make_session_service(session.clone(), account.clone());
    let state = test_state(account, session_svc.clone(), session);
    (session_svc, state)
}

fn make_account() -> Arc<AccountService> {
    Arc::new(AccountService::new(Arc::new(SnowflakeIdGenerator::new(
        1_735_689_600_000,
        1,
    ))))
}

fn arc_session(s: MockSessionStore) -> Arc<dyn SessionStore> {
    Arc::new(s)
}

// ===========================================================================
// 1. login (9) —— 拆两阶段测试：第一阶段 DB（LoginPending），第二阶段 Redis（LoginResponse）
// ===========================================================================

#[tokio::test]
async fn login_returns_token_pair_for_valid_manager() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);

    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_touch_login().returning(|_, _| Ok(()));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    repo.expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));

    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));

    let (svc, _state) = build(account, arc_session(session));
    let resp = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_login(resp).await.expect("ok");
    assert_eq!(resp.user.username, "alice");
    assert_eq!(resp.user.roles, vec!["MANAGER".to_string()]);
}

#[tokio::test]
async fn login_returns_token_pair_for_valid_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);

    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_touch_login().returning(|_, _| Ok(()));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    repo.expect_shelf_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    repo.expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "shelf", "Shelf")]));

    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(account, arc_session(session));
    let pending = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_login(pending).await.expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
}

#[tokio::test]
async fn login_returns_token_pair_for_wildcard_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);

    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_touch_login().returning(|_, _| Ok(()));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", None)]));
    repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));

    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(account, arc_session(session));
    let pending = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_login(pending).await.expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
}

#[tokio::test]
async fn login_rejects_unknown_username_as_biz_auth_invalid() {
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "ghost".into(),
            password: "any".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
}

#[tokio::test]
async fn login_rejects_wrong_password_as_biz_auth_invalid() {
    let hash = password::hash("correct").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "wrong".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
}

#[tokio::test]
async fn login_rejects_inactive_user_as_biz_auth_invalid() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
}

#[tokio::test]
async fn login_rejects_user_with_no_roles_as_no_role() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_list_by_user()
        .returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
}

#[tokio::test]
async fn login_includes_active_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_touch_login().returning(|_, _| Ok(()));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    repo.expect_shelf_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(account, arc_session(session));
    let pending = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_login(pending).await.expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
}

#[tokio::test]
async fn login_excludes_inactive_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", false);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_touch_login().returning(|_, _| Ok(()));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    repo.expect_shelf_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(account, arc_session(session));
    let pending = svc
        .login(repo, crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_login(pending).await.expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
}

// ===========================================================================
// 2. refresh (7) —— 拆两阶段测试
// ===========================================================================

#[tokio::test]
async fn refresh_rotates_tokens_after_validating_version() {
    let hash = password::hash("p").unwrap();
    let refresh_token = make_refresh_token(42, 0);

    let user_v0 = {
        let mut u = sample_user(42, "alice", &hash);
        u.refresh_token_version = 0;
        u
    };
    let user_v1 = {
        let mut u = sample_user(42, "alice", &hash);
        u.refresh_token_version = 1;
        u.version = 1;
        u
    };

    let account = make_account();
    let mut repo = MockIamRepo::new();
    let u0 = user_v0.clone();
    let u1 = user_v1.clone();
    repo.expect_get_by_id().returning(move |_| {
        let mut n = GET_BY_ID_COUNT.lock().unwrap();
        *n += 1;
        Ok(Some(if *n == 1 { u0.clone() } else { u1.clone() }))
    });
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    repo.expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    repo.expect_increment_refresh_token_version()
        .returning(|_, _, _, _| Ok(1u64));

    *GET_BY_ID_COUNT.lock().unwrap() = 0;

    let mut session = MockSessionStore::new();
    session.expect_delete_session().returning(|_| Ok(()));
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(account, arc_session(session));
    let pending = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest {
            refresh_token: refresh_token.clone(),
        })
        .await
        .expect("ok");
    let resp = svc.complete_refresh(pending).await.expect("ok");
    assert!(!resp.token.is_empty());
    assert!(!resp.refresh_token.is_empty());
}

#[tokio::test]
async fn refresh_rejects_undecodable_token_as_refresh_invalid() {
    // decode_refresh 在 `repo` 借进来**之前**就拒了（service 拿不到 repo）。
    let account = make_account();
    let repo = MockIamRepo::new();
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest {
            refresh_token: "not-a-valid-jwt".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
}

#[tokio::test]
async fn refresh_rejects_version_mismatch_as_refresh_invalid() {
    let refresh_token = make_refresh_token(42, 99); // token ver=99
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.refresh_token_version = 0; // DB ver=0
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
}

#[tokio::test]
async fn refresh_rejects_unknown_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(999, 0);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
}

#[tokio::test]
async fn refresh_rejects_inactive_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
}

#[tokio::test]
async fn refresh_rejects_user_with_no_roles_as_no_role() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_list_by_user()
        .returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
}

#[tokio::test]
async fn refresh_returns_version_conflict_when_increment_returns_zero() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    repo.expect_shelf_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    repo.expect_increment_refresh_token_version()
        .returning(|_, _, _, _| Ok(0u64));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .refresh(repo, crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::VERSION_CONFLICT);
}

// ===========================================================================
// 3. me (5) —— 单 repo 直接借
// ===========================================================================

#[tokio::test]
async fn me_returns_fresh_user_view_for_manager() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    repo.expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let out = svc.me(repo, &manager_current()).await.expect("ok");
    assert_eq!(out.username, "alice");
    assert_eq!(out.roles, vec!["MANAGER".to_string()]);
}

#[tokio::test]
async fn me_returns_fresh_user_view_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    repo.expect_shelf_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let current = CurrentUser {
        id: 7,
        username: "shelf_user".into(),
        roles: vec![Role::ShelfAccount],
        shelf_ids: vec![10],
        shelf_wildcard: false,
    };
    let out = svc.me(repo, &current).await.expect("ok");
    assert_eq!(out.shelf_ids, vec!["10".to_string()]);
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_not_found() {
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .me(repo, &manager_current())
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_inactive() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let err = svc
        .me(repo, &manager_current())
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
}

#[tokio::test]
async fn me_reflects_role_changes_after_token_issued() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut repo = MockIamRepo::new();
    repo.expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    // JWT 是 MANAGER，DB 是 CLERK —— 期望 roles 返回 CLERK
    repo.expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "CLERK", None)]));
    repo.expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let session = MockSessionStore::new();
    let (svc, _state) = build(account, arc_session(session));
    let out = svc.me(repo, &manager_current()).await.expect("ok");
    assert_eq!(out.roles, vec!["CLERK".to_string()]);
}

// ===========================================================================
// 4. change_password (5) —— service 直接借 repo 调 account_service.change_own_password
// ===========================================================================

#[tokio::test]
async fn change_password_delegates_to_account_service_for_self() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut account_repo = MockIamRepo::new();
    account_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    account_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1u64));

    let session = MockSessionStore::new(); // session.delete_all_user_sessions 不应被调
    let (svc, _state) = build(account.clone(), arc_session(session));
    let current = manager_current();
    svc.change_password(
        account_repo,
        42,
        ChangePasswordRequest {
            old_password: "old".into(),
            new_password: "new".into(),
        },
        &current,
    )
    .await
    .expect("ok");
}

#[tokio::test]
async fn change_password_delegates_to_account_service_for_manager_changing_other() {
    let hash = password::hash("old").unwrap();
    let target = sample_user(99, "bob", &hash); // 非自己（current.id=42）
    let account = make_account();
    let mut account_repo = MockIamRepo::new();
    account_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(target.clone())));
    account_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1u64));

    let session = MockSessionStore::new();
    let (svc, _state) = build(account.clone(), arc_session(session));
    let current = manager_current();
    svc.change_password(
        account_repo,
        99,
        ChangePasswordRequest {
            old_password: "old".into(),
            new_password: "new".into(),
        },
        &current,
    )
    .await
    .expect("ok (manager overriding)");
}

#[tokio::test]
async fn change_password_rejects_non_self_non_manager_as_forbidden() {
    // account_service 不应被调（permission 在 session 端挡）。
    let account = make_account();
    let account_repo = MockIamRepo::new(); // 无 expect → 任何调用都会 panic
    let session = MockSessionStore::new();
    let (svc, _state) = build(account.clone(), arc_session(session));
    let current = clerk_current(); // id=99, CLERK
    let err = svc
        .change_password(
            account_repo,
            42,
            ChangePasswordRequest {
                old_password: "old".into(),
                new_password: "new".into(),
            },
            &current,
        )
        .await
        .expect_err("expected 403");
    assert_eq!(err.code(), code::FORBIDDEN);
}

#[tokio::test]
async fn change_password_propagates_account_service_old_password_mismatch() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let account = make_account();
    let mut account_repo = MockIamRepo::new();
    account_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    // 不设 update_password_and_rotate expectation —— verify 失败前就返回
    let session = MockSessionStore::new();
    let (svc, _state) = build(account.clone(), arc_session(session));
    let current = manager_current();
    let err = svc
        .change_password(
            account_repo,
            42,
            ChangePasswordRequest {
                old_password: "wrong".into(),
                new_password: "new".into(),
            },
            &current,
        )
        .await
        .expect_err("expected OLD_PASSWORD_MISMATCH");
    assert_eq!(err.code(), code::OLD_PASSWORD_MISMATCH);
}

#[tokio::test]
async fn change_password_propagates_account_service_validation_error() {
    let account = make_account();
    let account_repo = MockIamRepo::new();
    let session = MockSessionStore::new();
    let (svc, _state) = build(account.clone(), arc_session(session));
    let current = manager_current();
    let err = svc
        .change_password(
            account_repo,
            42,
            ChangePasswordRequest {
                old_password: "old".into(),
                new_password: "".into(), // 触 account 端 validation
            },
            &current,
        )
        .await
        .expect_err("expected validation error");
    assert!(matches!(err, AppError::Validation(_)));
}

// ===========================================================================
// 5. logout (4) —— 不变
// ===========================================================================

fn build_for_logout(session: Arc<dyn SessionStore>) -> (Arc<SessionService>, Arc<AppState>) {
    let account = make_account();
    build(account, session)
}

#[tokio::test]
async fn logout_deletes_session_successfully() {
    let mut session = MockSessionStore::new();
    session
        .expect_delete_session()
        .times(1)
        .returning(|_| Ok(()));
    let (svc, _state) = build_for_logout(arc_session(session));
    let token_hash = hash_token("dummy-access-token");
    svc.logout(&token_hash).await.expect("logout ok");
}

#[tokio::test]
async fn logout_propagates_session_delete_error() {
    let mut session = MockSessionStore::new();
    session
        .expect_delete_session()
        .returning(|_| Err(AppError::internal("redis: connection refused")));
    let (svc, _state) = build_for_logout(arc_session(session));
    let err = svc.logout("any-hash").await.expect_err("expected error");
    assert_eq!(err.code(), code::INTERNAL);
}

#[tokio::test]
async fn logout_calls_delete_session_with_empty_token_hash() {
    let mut session = MockSessionStore::new();
    session
        .expect_delete_session()
        .times(1)
        .returning(|_| Ok(()));
    let (svc, _state) = build_for_logout(arc_session(session));
    svc.logout("").await.expect("logout ok with empty hash");
}

#[tokio::test]
async fn logout_is_idempotent_on_repeated_calls() {
    let mut session = MockSessionStore::new();
    session
        .expect_delete_session()
        .times(3)
        .returning(|_| Ok(())); // 幂等
    let (svc, _state) = build_for_logout(arc_session(session));
    let hash = "test-hash";
    for _ in 0..3 {
        svc.logout(hash).await.expect("logout ok");
    }
}

// ===========================================================================
// 内部 mock helper
// ===========================================================================

use std::sync::Mutex as StdMutex;

/// refresh happy 用例：get_by_id 需返回两次不同结果（v0 → v1）。单线程测试用全局计数器 toggle。
static GET_BY_ID_COUNT: StdMutex<u32> = StdMutex::new(0);
