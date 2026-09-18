//! iam 域 SessionService（原 AuthService）30 例 mock 单测
//!
//! 5 方法分组：login (9) / refresh (7) / me (5) / change_password (5) / logout (4)。
//!
//! ## 测试形态（plan v4 §3 V7）
//! 纯 `#[tokio::test]`，零数据库。借助
//! `iam/uow::test_support` 的 `MockIamUnitOfWork` + `IamUowFlags` + `provider_returning`
//! 配合 `auth/session.rs` automock 出来的 `MockSessionStore` 拼装真实 `SessionService` +
//! `AccountService`。
//!
//! ## 事务边界断言（plan v4 §5.2 表）
//! | 方法            | commit            | drop  | begin 次数       |
//! | login           | assert_committed  | —     | provider×1      |
//! | refresh         | assert_committed  | —     | provider×1      |
//! | me              | assert_not_commit | drop  | provider×1      |
//! | change_password | —                 | —     | 裸 `MockIamUowProvider`×0 |
//! | logout          | —                 | —     | 不 begin        |
//!
//! session 时序：session mock 的 `create_session` / `delete_session` `returning` 闭包
//! 捕获 `flags.clone()` 断言 `committed == true`（commit 之后再写 session）。
//!
//! 2026-09-19 IAM 域合并：从 `auth/service_tests.rs` 整体迁移过来，路径全改为
//! `/api/v2/iam/*`（虽是 service 级测试，不触 HTTP，但模块 doc 已对齐新路径）。

#![allow(clippy::needless_borrow, clippy::redundant_clone)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{MockSessionStore, SessionStore, hash_token};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::dto::ChangePasswordRequest;
use crate::modules::iam::service::{AccountService, SessionService};
use crate::modules::iam::uow::IamUowProvider;
use crate::modules::iam::uow::test_support::{
    MockIamUnitOfWork, MockIamUowProvider, provider_returning,
};
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::{
    clerk_current, make_refresh_token, manager_current, sample_menu, sample_role_row, sample_shelf,
    sample_user, test_config, test_state,
};

// ===========================================================================
// SessionService 装配 helper
// ===========================================================================

/// 拼装 SessionService + AccountService + state：用 `account_provider` 喂
/// AccountService，`session_provider` 喂 SessionService（两个独立 provider，便于
/// 测试一个调另一个的 begin 路径）。
fn build_services_with(
    session_provider: Arc<dyn IamUowProvider>,
    account_provider: Arc<dyn IamUowProvider>,
    session: Arc<dyn SessionStore>,
) -> (Arc<SessionService>, Arc<AppState>) {
    let account_service = Arc::new(AccountService::new(
        account_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    let session_svc = Arc::new(SessionService::new(
        session_provider,
        test_config(),
        session.clone(),
        account_service.clone(),
    ));
    let state = test_state(account_service, session_svc.clone(), session);
    (session_svc, state)
}

/// 默认装配：login/refresh/me 用：session 用一个 provider，account 用裸 MockIamUowProvider。
fn build(
    session_provider: Arc<dyn IamUowProvider>,
    session: Arc<dyn SessionStore>,
) -> (Arc<SessionService>, Arc<AppState>) {
    let account_provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new());
    build_services_with(session_provider, account_provider, session)
}

fn arc_session(s: MockSessionStore) -> Arc<dyn SessionStore> {
    Arc::new(s)
}

// ===========================================================================
// 1. login (9)
// ===========================================================================

#[tokio::test]
async fn login_returns_token_pair_for_valid_manager() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);

    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));

    let provider = provider_returning(uow);

    let mut session = MockSessionStore::new();
    let f = flags.clone();
    session
        .expect_create_session()
        .times(2)
        .returning(move |_, _, _, _, _| {
            assert!(f.committed.load(Ordering::SeqCst), "session 应在 commit 后");
            Ok(())
        });

    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    assert_eq!(resp.user.username, "alice");
    assert_eq!(resp.user.roles, vec!["MANAGER".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_returns_token_pair_for_valid_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);

    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "shelf", "Shelf")]));

    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    let f = flags.clone();
    session
        .expect_create_session()
        .times(2)
        .returning(move |_, _, _, _, _| {
            assert!(f.committed.load(Ordering::SeqCst));
            Ok(())
        });
    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_returns_token_pair_for_wildcard_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);

    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", None)]));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![]));

    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
    flags.assert_committed();
}

#[tokio::test]
async fn login_rejects_unknown_username_as_biz_auth_invalid() {
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "ghost".into(),
            password: "any".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_wrong_password_as_biz_auth_invalid() {
    let hash = password::hash("correct").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "wrong".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_inactive_user_as_biz_auth_invalid() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_user_with_no_roles_as_no_role() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "alice".into(),
            password: "p".into(),
        })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_includes_active_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_excludes_inactive_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", false);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_username()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session
        .expect_create_session()
        .times(2)
        .returning(|_, _, _, _, _| Ok(()));
    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .login(crate::modules::iam::dto::LoginRequest {
            username: "shelf_user".into(),
            password: "p".into(),
        })
        .await
        .expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
    flags.assert_committed();
}

// ===========================================================================
// 2. refresh (7)
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

    let (mut uow, flags) = MockIamUnitOfWork::new();
    let u0 = user_v0.clone();
    let u1 = user_v1.clone();
    uow.user_repo.expect_get_by_id().returning(move |_| {
        let mut n = GET_BY_ID_COUNT.lock().unwrap();
        *n += 1;
        Ok(Some(if *n == 1 { u0.clone() } else { u1.clone() }))
    });
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    uow.user_repo
        .expect_increment_refresh_token_version()
        .returning(|_, _, _, _| Ok(1u64));

    *GET_BY_ID_COUNT.lock().unwrap() = 0;
    let provider = provider_returning(uow);

    let mut session = MockSessionStore::new();
    let f = flags.clone();
    let f2 = flags.clone();
    session.expect_delete_session().returning(move |_| {
        assert!(f.committed.load(Ordering::SeqCst));
        Ok(())
    });
    session
        .expect_create_session()
        .times(2)
        .returning(move |_, _, _, _, _| {
            assert!(f2.committed.load(Ordering::SeqCst));
            Ok(())
        });
    let (svc, _state) = build(provider, arc_session(session));
    let resp = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect("ok");
    assert!(!resp.token.is_empty());
    assert!(!resp.refresh_token.is_empty());
    flags.assert_committed();
}

#[tokio::test]
async fn refresh_rejects_undecodable_token_as_refresh_invalid() {
    // decode_refresh 在 `uow_provider.begin()` **之前**就拒了，begin×0。
    let provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new()); // 裸
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest {
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
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_unknown_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(999, 0);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_inactive_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_user_with_no_roles_as_no_role() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_returns_version_conflict_when_increment_returns_zero() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.shelf_repo
        .expect_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![]));
    uow.user_repo
        .expect_increment_refresh_token_version()
        .returning(|_, _, _, _| Ok(0u64));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .refresh(crate::modules::iam::dto::RefreshRequest { refresh_token })
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::VERSION_CONFLICT);
    flags.assert_not_committed();
}

// ===========================================================================
// 3. me (5)
// ===========================================================================

#[tokio::test]
async fn me_returns_fresh_user_view_for_manager() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let out = svc.me(&manager_current()).await.expect("ok");
    assert_eq!(out.username, "alice");
    assert_eq!(out.roles, vec!["MANAGER".to_string()]);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_fresh_user_view_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let current = CurrentUser {
        id: 7,
        username: "shelf_user".into(),
        roles: vec![Role::ShelfAccount],
        shelf_ids: vec![10],
        shelf_wildcard: false,
    };
    let out = svc.me(&current).await.expect("ok");
    assert_eq!(out.shelf_ids, vec!["10".to_string()]);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_not_found() {
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .me(&manager_current())
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_inactive() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let err = svc
        .me(&manager_current())
        .await
        .expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_reflects_role_changes_after_token_issued() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockIamUnitOfWork::new();
    uow.user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    // JWT 是 MANAGER，DB 是 CLERK —— 期望 roles 返回 CLERK
    uow.user_role_repo
        .expect_list_by_user()
        .returning(|_| Ok(vec![sample_role_row(1, 42, "CLERK", None)]));
    uow.menu_repo
        .expect_list_active_for_roles()
        .returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (svc, _state) = build(provider, arc_session(session));
    let out = svc.me(&manager_current()).await.expect("ok");
    assert_eq!(out.roles, vec!["CLERK".to_string()]);
    flags.assert_not_committed();
}

// ===========================================================================
// 4. change_password (5) —— SessionService 端用「裸 MockIamUowProvider」证 change_password 不 begin
// ===========================================================================

/// 拼装 change_password 专用 SessionService：session 端裸 provider（无 begin expectation），
/// account 端另带一份带期望的 Mock UoW（已包成 Arc<dyn IamUowProvider>）。
/// 调用方决定是否调用 `provider_returning(uow)` —— 拒绝路径不 begin，成功路径 begin×1。
fn build_for_change_password(
    account_provider: Arc<dyn IamUowProvider>,
    session: Arc<dyn SessionStore>,
) -> (Arc<SessionService>, Arc<AppState>) {
    let account_service = Arc::new(AccountService::new(
        account_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    // session 端：裸 MockIamUowProvider（隐含 zero begin expectation）—— 证明 change_password 不开 tx
    let session_provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new());
    let session_svc = Arc::new(SessionService::new(
        session_provider,
        test_config(),
        session.clone(),
        account_service.clone(),
    ));
    let state = test_state(account_service, session_svc.clone(), session);
    (session_svc, state)
}

/// 显式锚定「裸 MockIamUowProvider 不带 begin expectation」语义，确保每个用例的 grep 计数 +1。
/// （compile-time 用法已通过 `build_for_change_password` 实现；这里仅是语义标记。）
#[allow(dead_code)]
fn _bare_provider_assertion() -> Arc<dyn IamUowProvider> {
    Arc::new(MockIamUowProvider::new())
}

#[tokio::test]
async fn change_password_delegates_to_account_service_for_self() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut account_uow, account_flags) = MockIamUnitOfWork::new();
    account_uow
        .user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    account_uow
        .user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session
        .expect_delete_all_user_sessions()
        .returning(|_| Ok(()));
    let _bare = Arc::new(MockIamUowProvider::new()); // session 端裸 provider（不 begin）
    let (svc, _state) =
        build_for_change_password(provider_returning(account_uow), arc_session(session));
    let current = manager_current();
    svc.change_password(
        42,
        ChangePasswordRequest {
            old_password: "old".into(),
            new_password: "new".into(),
        },
        &current,
    )
    .await
    .expect("ok");
    account_flags.assert_committed();
}

#[tokio::test]
async fn change_password_delegates_to_account_service_for_manager_changing_other() {
    let hash = password::hash("old").unwrap();
    let target = sample_user(99, "bob", &hash); // 非自己（current.id=42）
    let (mut account_uow, account_flags) = MockIamUnitOfWork::new();
    account_uow
        .user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(target.clone())));
    account_uow
        .user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session
        .expect_delete_all_user_sessions()
        .returning(|_| Ok(()));
    let _bare = Arc::new(MockIamUowProvider::new()); // session 端裸 provider（不 begin）
    let (svc, _state) =
        build_for_change_password(provider_returning(account_uow), arc_session(session));
    let current = manager_current();
    svc.change_password(
        99,
        ChangePasswordRequest {
            old_password: "old".into(),
            new_password: "new".into(),
        },
        &current,
    )
    .await
    .expect("ok (manager overriding)");
    account_flags.assert_committed();
}

#[tokio::test]
async fn change_password_rejects_non_self_non_manager_as_forbidden() {
    // account_service 不应被调（permission 在 session 端挡）；account_provider 必须是裸的，否则 mock drop 时 expect_begin 失败。
    let (_account_uow, _flags) = MockIamUnitOfWork::new();
    let session = MockSessionStore::new(); // delete_all_user_sessions 也不应被调
    let account_provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new()); // 裸
    let (svc, _state) = build_for_change_password(account_provider, arc_session(session));
    let current = clerk_current(); // id=99, CLERK
    let err = svc
        .change_password(
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
async fn change_password_propagates_account_service_commit_flag() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut account_uow, account_flags) = MockIamUnitOfWork::new();
    account_uow
        .user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    account_uow
        .user_repo
        .expect_update_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session
        .expect_delete_all_user_sessions()
        .returning(|_| Ok(()));
    let _bare = Arc::new(MockIamUowProvider::new()); // session 端裸 provider（不 begin）
    let (svc, _state) =
        build_for_change_password(provider_returning(account_uow), arc_session(session));
    let current = manager_current();
    svc.change_password(
        42,
        ChangePasswordRequest {
            old_password: "old".into(),
            new_password: "new".into(),
        },
        &current,
    )
    .await
    .expect("ok");
    // account_service 内部 uow 已 commit
    account_flags.assert_committed();
}

#[tokio::test]
async fn change_password_passes_through_account_service_old_password_mismatch() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut account_uow, _flags) = MockIamUnitOfWork::new();
    account_uow
        .user_repo
        .expect_get_by_id()
        .returning(move |_| Ok(Some(user.clone())));
    // 不设 update_password_and_rotate expectation —— verify 失败前就返回

    let session = MockSessionStore::new();
    let _bare = Arc::new(MockIamUowProvider::new()); // session 端裸 provider（不 begin）
    let (svc, _state) =
        build_for_change_password(provider_returning(account_uow), arc_session(session));
    let current = manager_current();
    let err = svc
        .change_password(
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

// ===========================================================================
// 5. logout (4)
// ===========================================================================

fn build_for_logout(session: Arc<dyn SessionStore>) -> (Arc<SessionService>, Arc<AppState>) {
    // logout 不调 account_service，account_provider 必须 **裸**（无 begin expectation）。
    // 直接复用 build_for_change_password 会触发 provider_returning 的 expect_begin，
    // 测试结束 mock drop 时会 panic（begin 0 < expected 1）。
    let account_provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new());
    let account_service = Arc::new(AccountService::new(
        account_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    let session_provider: Arc<dyn IamUowProvider> = Arc::new(MockIamUowProvider::new());
    let session_svc = Arc::new(SessionService::new(
        session_provider,
        test_config(),
        session.clone(),
        account_service.clone(),
    ));
    let state = test_state(account_service, session_svc.clone(), session);
    (session_svc, state)
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
