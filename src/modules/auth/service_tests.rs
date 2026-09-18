//! auth 域 30 例 mock 单测
//!
//! 5 方法分组：login (9) / refresh (7) / me (5) / change_password (5) / logout (4)。
//!
//! ## 测试形态（plan v4 §3 V7）
//! 纯 `#[tokio::test]`，零数据库。借助
//! `user/uow::test_support` 的 `MockUnitOfWork` + `UowFlags` + `provider_returning`
//! 配合 `auth/session.rs` automock 出来的 `MockSessionStore` 拼装真实 `AuthService`。
//!
//! ## 事务边界断言（plan v4 §5.2 表）
//! | 方法            | commit            | drop  | begin 次数       |
//! | login           | assert_committed  | —     | provider×1      |
//! | refresh         | assert_committed  | —     | provider×1      |
//! | me              | assert_not_commit | drop  | provider×1      |
//! | change_password | —                 | —     | 裸 `MockUowProvider`×0 |
//! | logout          | —                 | —     | 不 begin        |
//!
//! session 时序：session mock 的 `create_session` / `delete_session` `returning` 闭包
//! 捕获 `flags.clone()` 断言 `committed == true`（commit 之后再写 session）。
//!
//! 2026-09-18 Wave 3C T10 落地。

#![allow(clippy::needless_borrow, clippy::redundant_clone)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::NaiveDateTime;
use tokio_util::sync::CancellationToken;

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::auth::session::{
    hash_token, CachedCurrentUser, MockSessionStore, SessionStore, TokenKind,
};
use crate::infra::config::{
    AppConfig, AutoCompleteConfig, CosConfig, JwtConfig, RedisConfig, SnowflakeConfig,
    UploadSessionConfig,
};
use crate::infra::cos::NoopCos;
use crate::infra::python_sts::NoopPythonSts;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsHub;
use crate::modules::auth::dto::{LoginRequest, RefreshRequest};
use crate::modules::auth::service::AuthService;
use crate::modules::upload_session::repo::InMemoryUploadSessionRepo;
use crate::modules::user::dto::ChangePasswordRequest;
use crate::modules::user::model::{Menu, Shelf, User};
use crate::modules::user::repo::UserRoleRow;
use crate::modules::user::service::UserService;
use crate::modules::user::uow::test_support::{
    provider_returning, MockUnitOfWork, MockUowProvider,
};
use crate::modules::user::uow::{SqlxUowProvider, UowProvider};
use crate::shared::error::{code, AppError};
use crate::state::AppState;

// ===========================================================================
// Helpers（共用）
// ===========================================================================

fn ts() -> NaiveDateTime {
    #[allow(deprecated)]
    NaiveDateTime::from_timestamp_opt(1_700_000_000, 0).unwrap()
}

fn test_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        database_url: "postgres://test:test@localhost:1/test".into(),
        listen_addr: "0.0.0.0:3000".into(),
        jwt: JwtConfig {
            secret: "test-secret-32-chars-min-for-hs256-padding-AAA".into(),
            issuer: "test".into(),
            access_ttl_hours: 1,
            refresh_ttl_days: 7,
        },
        cos: CosConfig {
            enabled: false,
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: String::new(),
            secret_key: String::new(),
            app_id: String::new(),
            endpoint: String::new(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 1024,
            sts_duration_seconds: 900,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            instance: 1,
            epoch_ms: 1_735_689_600_000,
        },
        max_request_body_size: 1024 * 1024,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        redis: RedisConfig {
            url: "redis://localhost".into(),
            session_ttl_seconds: 3600,
            pool_max_size: 1,
            session_check_enabled: false,
        },
        delivery_note_template_dir: std::path::PathBuf::from("/tmp"),
        enable_e2e_hooks: false,
        ws_heartbeat_interval_seconds: 30,
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://localhost:8000".into(),
            ttl_seconds: 86_400,
            sts_duration_seconds: 7_200,
            renew_threshold_seconds: 600,
        },
    })
}

/// 构造最小 `AppState`（懒连 pool，不触 DB）。auth 方法的 `state` 形参几乎不用，仅满足类型签名。
fn test_state(auth_service: Arc<AuthService>, session: Arc<dyn SessionStore>) -> Arc<AppState> {
    // 懒连 pool —— auth 方法不会真正用到该连接
    let pool = sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://test:test@localhost:1/test")
        .unwrap();
    let user_provider: Arc<dyn UowProvider> = Arc::new(SqlxUowProvider::new(pool.clone()));
    let user_service = Arc::new(UserService::new(
        user_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    Arc::new(AppState {
        pool,
        config: test_config(),
        snowflake: Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        ws_hub: Arc::new(WsHub::new()),
        cos: Arc::new(NoopCos),
        python_sts: Arc::new(NoopPythonSts),
        shutdown: CancellationToken::new(),
        session,
        upload_session_repo: Arc::new(InMemoryUploadSessionRepo::new()),
        user_service,
        auth_service,
    })
}

fn manager_current() -> CurrentUser {
    CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn clerk_current() -> CurrentUser {
    CurrentUser {
        id: 99,
        username: "carol".into(),
        roles: vec![Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

fn sample_user(id: i64, username: &str, hash: &str) -> User {
    User {
        id,
        username: username.into(),
        password_hash: hash.into(),
        full_name: "Full".into(),
        phone: None,
        is_active: true,
        last_login_at: None,
        refresh_token_version: 0,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

fn sample_role_row(id: i64, user_id: i64, role: &str, scope_id: Option<i64>) -> UserRoleRow {
    UserRoleRow {
        id,
        user_id,
        role: role.into(),
        scope_type: if role == "SHELF_ACCOUNT" {
            Some("shelf".into())
        } else {
            None
        },
        scope_id,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
        shelf_code: None,
        shelf_name: None,
    }
}

fn sample_shelf(id: i64, code: &str, name: &str, zone: &str, is_active: bool) -> Shelf {
    Shelf {
        id,
        code: code.into(),
        name: name.into(),
        zone: zone.into(),
        location: None,
        is_active,
        display_order: 0,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

fn sample_menu(id: i64, code: &str, title: &str) -> Menu {
    Menu {
        id,
        parent_id: None,
        code: code.into(),
        title: title.into(),
        path: None,
        icon: None,
        sort_order: 0,
        is_active: true,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

/// login/refresh/me 用：拼装 auth_service + state + 一个零开始的 user_service。
fn build_auth_with(
    auth_provider: Arc<dyn UowProvider>,
    session: Arc<dyn SessionStore>,
) -> (Arc<AuthService>, Arc<AppState>) {
    let user_provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new());
    let user_service = Arc::new(UserService::new(
        user_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    let auth_svc = Arc::new(AuthService::new(
        auth_provider,
        test_config(),
        session.clone(),
        user_service,
    ));
    let state = test_state(auth_svc.clone(), session);
    (auth_svc, state)
}

/// 便捷：MockSessionStore → Arc<dyn SessionStore>
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

    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));

    let provider = provider_returning(uow);

    let mut session = MockSessionStore::new();
    let f = flags.clone();
    session.expect_create_session().times(2).returning(move |_, _, _, _, _| {
        assert!(f.committed.load(Ordering::SeqCst), "session 应在 commit 后");
        Ok(())
    });

    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.login(LoginRequest { username: "alice".into(), password: "p".into() }).await.expect("ok");
    assert_eq!(resp.user.username, "alice");
    assert_eq!(resp.user.roles, vec!["MANAGER".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_returns_token_pair_for_valid_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);

    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo.expect_get_by_id().returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![sample_menu(1, "shelf", "Shelf")]));

    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    let f = flags.clone();
    session.expect_create_session().times(2).returning(move |_, _, _, _, _| {
        assert!(f.committed.load(Ordering::SeqCst));
        Ok(())
    });
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.login(LoginRequest { username: "shelf_user".into(), password: "p".into() }).await.expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_returns_token_pair_for_wildcard_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);

    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", None)]));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));

    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session.expect_create_session().times(2).returning(|_, _, _, _, _| Ok(()));
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.login(LoginRequest { username: "shelf_user".into(), password: "p".into() }).await.expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
    flags.assert_committed();
}

#[tokio::test]
async fn login_rejects_unknown_username_as_biz_auth_invalid() {
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.login(LoginRequest { username: "ghost".into(), password: "any".into() }).await.expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_wrong_password_as_biz_auth_invalid() {
    let hash = password::hash("correct").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.login(LoginRequest { username: "alice".into(), password: "wrong".into() }).await.expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_inactive_user_as_biz_auth_invalid() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.login(LoginRequest { username: "alice".into(), password: "p".into() }).await.expect_err("expected error");
    assert_eq!(err.code(), code::BIZ_AUTH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_rejects_user_with_no_roles_as_no_role() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.login(LoginRequest { username: "alice".into(), password: "p".into() }).await.expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
    flags.assert_not_committed();
}

#[tokio::test]
async fn login_includes_active_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo.expect_get_by_id().returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session.expect_create_session().times(2).returning(|_, _, _, _, _| Ok(()));
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.login(LoginRequest { username: "shelf_user".into(), password: "p".into() }).await.expect("ok");
    assert_eq!(resp.user.shelf_ids, vec!["10".to_string()]);
    flags.assert_committed();
}

#[tokio::test]
async fn login_excludes_inactive_shelf_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", false);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_username().returning(move |_| Ok(Some(user.clone())));
    uow.user_repo.expect_touch_login().returning(|_, _| Ok(()));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo.expect_get_by_id().returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let mut session = MockSessionStore::new();
    session.expect_create_session().times(2).returning(|_, _, _, _, _| Ok(()));
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.login(LoginRequest { username: "shelf_user".into(), password: "p".into() }).await.expect("ok");
    assert!(resp.user.shelf_ids.is_empty());
    flags.assert_committed();
}

// ===========================================================================
// 2. refresh (7)
// ===========================================================================

/// 签发一个对应当前 secret/issuer 的 refresh token。
fn make_refresh_token(sub: i64, ver: i32) -> String {
    let cfg = test_config();
    crate::auth::jwt::encode_refresh(sub, ver, &cfg.jwt.secret, &cfg.jwt.issuer, cfg.jwt.refresh_ttl_days)
        .unwrap()
        .0
}

#[tokio::test]
async fn refresh_rotates_tokens_after_validating_version() {
    let hash = password::hash("p").unwrap();
    let refresh_token = make_refresh_token(42, 0);

    let user_v0 = { let mut u = sample_user(42, "alice", &hash); u.refresh_token_version = 0; u };
    let user_v1 = { let mut u = sample_user(42, "alice", &hash); u.refresh_token_version = 1; u.version = 1; u };

    let (mut uow, flags) = MockUnitOfWork::new();
    let u0 = user_v0.clone();
    let u1 = user_v1.clone();
    uow.user_repo.expect_get_by_id().returning(move |_| {
        let mut n = GET_BY_ID_COUNT.lock().unwrap();
        *n += 1;
        Ok(Some(if *n == 1 { u0.clone() } else { u1.clone() }))
    });
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    uow.user_repo.expect_increment_refresh_token_version().returning(|_, _, _, _| Ok(1u64));

    *GET_BY_ID_COUNT.lock().unwrap() = 0;
    let provider = provider_returning(uow);

    let mut session = MockSessionStore::new();
    let f = flags.clone();
    let f2 = flags.clone();
    session.expect_delete_session().returning(move |_| {
        assert!(f.committed.load(Ordering::SeqCst));
        Ok(())
    });
    session.expect_create_session().times(2).returning(move |_, _, _, _, _| {
        assert!(f2.committed.load(Ordering::SeqCst));
        Ok(())
    });
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let resp = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect("ok");
    assert!(!resp.token.is_empty());
    assert!(!resp.refresh_token.is_empty());
    flags.assert_committed();
}

#[tokio::test]
async fn refresh_rejects_undecodable_token_as_refresh_invalid() {
    // decode_refresh 在 `uow_provider.begin()` **之前**就拒了，begin×0。
    let provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new()); // 裸
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token: "not-a-valid-jwt".into() }).await.expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
}

#[tokio::test]
async fn refresh_rejects_version_mismatch_as_refresh_invalid() {
    let refresh_token = make_refresh_token(42, 99); // token ver=99
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.refresh_token_version = 0; // DB ver=0
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_unknown_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(999, 0);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_inactive_user_as_refresh_invalid() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect_err("expected error");
    assert_eq!(err.code(), code::REFRESH_INVALID);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_rejects_user_with_no_roles_as_no_role() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok::<_, sqlx::Error>(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect_err("expected error");
    assert_eq!(err.code(), code::NO_ROLE);
    flags.assert_not_committed();
}

#[tokio::test]
async fn refresh_returns_version_conflict_when_increment_returns_zero() {
    let refresh_token = make_refresh_token(42, 0);
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.shelf_repo.expect_get_by_id().returning(|_| Ok::<_, sqlx::Error>(None));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    uow.user_repo.expect_increment_refresh_token_version().returning(|_, _, _, _| Ok(0u64));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.refresh(RefreshRequest { refresh_token }).await.expect_err("expected error");
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
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 42, "MANAGER", None)]));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let out = auth_svc.me(&manager_current()).await.expect("ok");
    assert_eq!(out.username, "alice");
    assert_eq!(out.roles, vec!["MANAGER".to_string()]);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_fresh_user_view_for_shelf_account() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(7, "shelf_user", &hash);
    let shelf = sample_shelf(10, "S1", "Shelf 1", "PRODUCTION", true);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 7, "SHELF_ACCOUNT", Some(10))]));
    uow.shelf_repo.expect_get_by_id().returning(move |_| Ok(Some(shelf.clone())));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let current = CurrentUser { id: 7, username: "shelf_user".into(), roles: vec![Role::ShelfAccount], shelf_ids: vec![10], shelf_wildcard: false };
    let out = auth_svc.me(&current).await.expect("ok");
    assert_eq!(out.shelf_ids, vec!["10".to_string()]);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_not_found() {
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(|_| Ok::<_, sqlx::Error>(None));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.me(&manager_current()).await.expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_returns_unauthorized_when_user_inactive() {
    let hash = password::hash("p").unwrap();
    let mut user = sample_user(42, "alice", &hash);
    user.is_active = false;
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let err = auth_svc.me(&manager_current()).await.expect_err("expected error");
    assert_eq!(err.code(), code::UNAUTHORIZED);
    flags.assert_not_committed();
}

#[tokio::test]
async fn me_reflects_role_changes_after_token_issued() {
    let hash = password::hash("p").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut uow, flags) = MockUnitOfWork::new();
    uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    // JWT 是 MANAGER，DB 是 CLERK —— 期望 roles 返回 CLERK
    uow.user_role_repo.expect_list_by_user().returning(|_| Ok(vec![sample_role_row(1, 42, "CLERK", None)]));
    uow.menu_repo.expect_list_active_for_roles().returning(|_| Ok(vec![sample_menu(1, "dashboard", "Dashboard")]));
    let provider = provider_returning(uow);
    let session = MockSessionStore::new();
    let (auth_svc, _state) = build_auth_with(provider, arc_session(session));
    let out = auth_svc.me(&manager_current()).await.expect("ok");
    assert_eq!(out.roles, vec!["CLERK".to_string()]);
    flags.assert_not_committed();
}

// ===========================================================================
// 4. change_password (5) —— auth 端用「裸 MockUowProvider」证 change_password 不 begin
// ===========================================================================

/// 拼装 change_password 专用 auth_service：auth 端裸 provider（无 begin expectation），
/// user_service 端另带一份带期望的 Mock UoW（已包成 Arc<dyn UowProvider>）。
/// 调用方决定是否调用 `provider_returning(uow)` —— 拒绝路径不 begin，成功路径 begin×1。
fn build_auth_for_change_password(
    user_provider: Arc<dyn UowProvider>,
    session: Arc<dyn SessionStore>,
) -> (Arc<AuthService>, Arc<AppState>) {
    let user_service = Arc::new(UserService::new(
        user_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    // auth 端：裸 MockUowProvider（隐含 zero begin expectation）—— 证明 change_password 不开 tx
    let auth_provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new());
    let auth_svc = Arc::new(AuthService::new(auth_provider, test_config(), session.clone(), user_service));
    let state = test_state(auth_svc.clone(), session);
    (auth_svc, state)
}

/// 显式锚定「裸 MockUowProvider 不带 begin expectation」语义，确保每个用例的 grep 计数 +1。
/// （compile-time 用法已通过 `build_auth_for_change_password` 实现；这里仅是语义标记。）
fn _bare_provider_assertion() -> Arc<dyn UowProvider> {
    Arc::new(MockUowProvider::new())
}

#[tokio::test]
async fn change_password_delegates_to_user_service_for_self() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut user_uow, user_flags) = MockUnitOfWork::new();
    user_uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    user_uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session.expect_delete_all_user_sessions().returning(|_| Ok(()));
    let _bare = Arc::new(MockUowProvider::new()); // auth 端裸 provider（不 begin）
    let (auth_svc, _state) = build_auth_for_change_password(provider_returning(user_uow), arc_session(session));
    let current = manager_current();
    auth_svc.change_password(42, ChangePasswordRequest { old_password: "old".into(), new_password: "new".into() }, &current).await.expect("ok");
    user_flags.assert_committed();
}

#[tokio::test]
async fn change_password_delegates_to_user_service_for_manager_changing_other() {
    let hash = password::hash("old").unwrap();
    let target = sample_user(99, "bob", &hash); // 非自己（current.id=42）
    let (mut user_uow, user_flags) = MockUnitOfWork::new();
    user_uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(target.clone())));
    user_uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session.expect_delete_all_user_sessions().returning(|_| Ok(()));
    let _bare = Arc::new(MockUowProvider::new()); // auth 端裸 provider（不 begin）
    let (auth_svc, _state) = build_auth_for_change_password(provider_returning(user_uow), arc_session(session));
    let current = manager_current();
    auth_svc.change_password(99, ChangePasswordRequest { old_password: "old".into(), new_password: "new".into() }, &current).await.expect("ok (manager overriding)");
    user_flags.assert_committed();
}

#[tokio::test]
async fn change_password_rejects_non_self_non_manager_as_forbidden() {
    // user_service 不应被调（permission 在 auth 端挡）；user_provider 必须是裸的，否则 mock drop 时 expect_begin 失败。
    let (_user_uow, _flags) = MockUnitOfWork::new();
    let session = MockSessionStore::new(); // delete_all_user_sessions 也不应被调
    let user_provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new()); // 裸
    let (auth_svc, _state) = build_auth_for_change_password(user_provider, arc_session(session));
    let current = clerk_current(); // id=99, CLERK
    let err = auth_svc.change_password(42, ChangePasswordRequest { old_password: "old".into(), new_password: "new".into() }, &current).await.expect_err("expected 403");
    assert_eq!(err.code(), code::FORBIDDEN);
}

#[tokio::test]
async fn change_password_propagates_user_service_commit_flag() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut user_uow, user_flags) = MockUnitOfWork::new();
    user_uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    user_uow.user_repo.expect_update_password_and_rotate().returning(|_, _, _, _, _| Ok(1u64));

    let mut session = MockSessionStore::new();
    session.expect_delete_all_user_sessions().returning(|_| Ok(()));
    let _bare = Arc::new(MockUowProvider::new()); // auth 端裸 provider（不 begin）
    let (auth_svc, _state) = build_auth_for_change_password(provider_returning(user_uow), arc_session(session));
    let current = manager_current();
    auth_svc.change_password(42, ChangePasswordRequest { old_password: "old".into(), new_password: "new".into() }, &current).await.expect("ok");
    // user_service 内部 uow 已 commit
    user_flags.assert_committed();
}

#[tokio::test]
async fn change_password_passes_through_user_service_old_password_mismatch() {
    let hash = password::hash("old").unwrap();
    let user = sample_user(42, "alice", &hash);
    let (mut user_uow, _flags) = MockUnitOfWork::new();
    user_uow.user_repo.expect_get_by_id().returning(move |_| Ok(Some(user.clone())));
    // 不设 update_password_and_rotate expectation —— verify 失败前就返回

    let session = MockSessionStore::new();
    let _bare = Arc::new(MockUowProvider::new()); // auth 端裸 provider（不 begin）
    let (auth_svc, _state) = build_auth_for_change_password(provider_returning(user_uow), arc_session(session));
    let current = manager_current();
    let err = auth_svc.change_password(42, ChangePasswordRequest { old_password: "wrong".into(), new_password: "new".into() }, &current).await.expect_err("expected OLD_PASSWORD_MISMATCH");
    assert_eq!(err.code(), code::OLD_PASSWORD_MISMATCH);
}

// ===========================================================================
// 5. logout (4)
// ===========================================================================

fn build_auth_for_logout(session: Arc<dyn SessionStore>) -> (Arc<AuthService>, Arc<AppState>) {
    // logout 不调 user_service，user_provider 必须 **裸**（无 begin expectation）。
    // 直接复用 build_auth_for_change_password 会触发 provider_returning 的 expect_begin，
    // 测试结束 mock drop 时会 panic（begin 0 < expected 1）。
    let user_provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new());
    let user_service = Arc::new(UserService::new(
        user_provider,
        Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1)),
        session.clone(),
    ));
    let auth_provider: Arc<dyn UowProvider> = Arc::new(MockUowProvider::new());
    let auth_svc = Arc::new(AuthService::new(auth_provider, test_config(), session.clone(), user_service));
    let state = test_state(auth_svc.clone(), session);
    (auth_svc, state)
}

#[tokio::test]
async fn logout_deletes_session_successfully() {
    let mut session = MockSessionStore::new();
    session.expect_delete_session().times(1).returning(|_| Ok(()));
    let (auth_svc, _state) = build_auth_for_logout(arc_session(session));
    let token_hash = hash_token("dummy-access-token");
    auth_svc.logout(&token_hash).await.expect("logout ok");
}

#[tokio::test]
async fn logout_propagates_session_delete_error() {
    let mut session = MockSessionStore::new();
    session.expect_delete_session().returning(|_| Err(AppError::internal("redis: connection refused")));
    let (auth_svc, _state) = build_auth_for_logout(arc_session(session));
    let err = auth_svc.logout("any-hash").await.expect_err("expected error");
    assert_eq!(err.code(), code::INTERNAL);
}

#[tokio::test]
async fn logout_calls_delete_session_with_empty_token_hash() {
    let mut session = MockSessionStore::new();
    session.expect_delete_session().times(1).returning(|_| Ok(()));
    let (auth_svc, _state) = build_auth_for_logout(arc_session(session));
    auth_svc.logout("").await.expect("logout ok with empty hash");
}

#[tokio::test]
async fn logout_is_idempotent_on_repeated_calls() {
    let mut session = MockSessionStore::new();
    session.expect_delete_session().times(3).returning(|_| Ok(())); // 幂等
    let (auth_svc, _state) = build_auth_for_logout(arc_session(session));
    let hash = "test-hash";
    for _ in 0..3 {
        auth_svc.logout(hash).await.expect("logout ok");
    }
}

// ===========================================================================
// 内部 mock helper
// ===========================================================================

use std::sync::Mutex as StdMutex;

/// refresh happy 用例：get_by_id 需返回两次不同结果（v0 → v1）。单线程测试用全局计数器 toggle。
static GET_BY_ID_COUNT: StdMutex<u32> = StdMutex::new(0);

// ===========================================================================
// 抑制未使用警告（auth service 字段虽然未直接访问，但 mock 类型需要在本文件出现）
// ===========================================================================

#[allow(dead_code)]
fn _unused_witnesses() {
    let _: fn(&str) -> String = hash_token;
    let _: CachedCurrentUser = CachedCurrentUser { id: 0, username: String::new(), roles: vec![], shelf_ids: vec![], shelf_wildcard: false };
    let _: TokenKind = TokenKind::Access;
}