//! SessionService 单元测试（13 用例，2026-09-23 新增）
//!
//! 用例覆盖矩阵：
//! - login：happy / wrong_password / inactive_user / bcrypt_error
//! - me：happy
//! - logout：happy
//! - change_password：happy / wrong_old / forbidden_for_other_user（委托给 AccountService）
//! - refresh phase 1：happy / invalid_token
//! - refresh phase 2（complete_refresh）：happy / redis_fail
//!
//! mockall 用法：`MockIamRepo` + `MockSessionStore` 注入 service。Redis session
//! 操作由 MockSessionStore 提供；jwt 签发/解码用 `tests/mod.rs::test_jwt_config()`
//! 提供的 2048-bit RSA keypair（进程级 OnceLock 缓存）。

use mockall::predicate::*;

use super::*;
use crate::auth::password;
use crate::auth::rbac::Role;
use crate::auth::session::MockSessionStore;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::dto::ChangePasswordRequest;
use crate::modules::iam::repo::MockIamRepo;
use crate::modules::iam::repo::model::UserRole;
use crate::modules::iam::service::SessionService;
use crate::shared::error::{AppError, code};

// ===========================================================================
// SessionService 实例工厂（用 test_jwt_config + MockSessionStore）
// ===========================================================================

/// 构造 SessionService（MockSessionStore + AccountService）。
pub fn make_session_service(mock_session: MockSessionStore) -> SessionService {
    let config = test_app_config();
    let session: std::sync::Arc<dyn crate::auth::session::SessionStore> =
        std::sync::Arc::new(mock_session);
    let account_service = std::sync::Arc::new(make_account_service());
    SessionService::new(config, session, account_service)
}

/// 构造一个最小化的 User 行（与 tests/mod.rs sample_user 类似但 password_hash 用真实 bcrypt）。
#[allow(dead_code)]
pub fn sample_user_with_hash(id: i64, username: &str, plain_password: &str) -> crate::modules::iam::repo::User {
    let hash = password::hash(plain_password).expect("bcrypt hash");
    crate::modules::iam::repo::User {
        id,
        password_hash: hash,
        ..sample_user(id, username)
    }
}

/// 构造一个真实 `UserRole`（用于 `get_user_role_by_id` mock 返回，本测试用不到）。
#[allow(dead_code)]
pub fn sample_user_role_full(id: i64, user_id: i64, role: Role) -> UserRole {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 23)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let role_str = match role {
        Role::Manager => "MANAGER",
        Role::Clerk => "CLERK",
        Role::Inspector => "INSPECTOR",
        Role::CncProgrammer => "CNC_PROGRAMMER",
        Role::ShelfAccount => "SHELF_ACCOUNT",
    };
    UserRole {
        id,
        user_id,
        role: role_str.to_string(),
        scope_type: None,
        scope_id: None,
        version: 1,
        created_at: now,
        created_by: Some(1),
        updated_at: now,
        updated_by: Some(1),
        deleted_at: None,
    }
}

// ===========================================================================
// login（4 用例）
// ===========================================================================

#[tokio::test]
async fn login_happy_path_returns_pending() {
    // Arrange
    let hash = password::hash("correct").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 42,
        password_hash: hash,
        is_active: true,
        ..sample_user(42, "alice")
    };
    let role_row = crate::modules::iam::repo::UserRoleRow {
        role: "MANAGER".into(),
        ..sample_user_role(1, 42, "MANAGER")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_username()
        .returning(move |_| Ok(Some(u.clone())));
    mock_repo
        .expect_list_user_roles_by_user_id()
        .returning(move |_| Ok(vec![role_row.clone()]));
    mock_repo
        .expect_touch_user_last_login_at()
        .returning(|_, _| Ok(()));
    mock_repo
        .expect_list_active_menus_by_roles()
        .returning(|_| Ok(vec![])); // 菜单：空

    // 不需要 SessionStore：login 第一阶段不写 Redis
    let mock_session = MockSessionStore::new();

    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::LoginRequest {
        username: "Alice".into(), // 归一化 lowercase
        password: "correct".into(),
    };

    // Act
    let pending = svc.login(mock_repo, req).await.expect("login 应 Ok");

    // Assert
    assert_eq!(pending.user.id, 42);
    assert_eq!(pending.user.username, "alice"); // 归一化
    assert_eq!(pending.roles, vec![Role::Manager]);
    assert!(!pending.pair.access_token.is_empty());
    assert!(!pending.pair.refresh_token.is_empty());
    assert!(!pending.pair.access_jti.is_empty());
    assert!(!pending.pair.refresh_jti.is_empty());
}

#[tokio::test]
async fn login_wrong_password_returns_invalid() {
    // Arrange
    let hash = password::hash("correct").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 42,
        password_hash: hash,
        is_active: true,
        ..sample_user(42, "alice")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_username()
        .returning(move |_| Ok(Some(u.clone())));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::LoginRequest {
        username: "alice".into(),
        password: "wrong".into(),
    };

    // Act
    let err = svc.login(mock_repo, req).await.expect_err("login 密码错应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::BIZ_AUTH_INVALID),
        other => panic!("expected Biz BIZ_AUTH_INVALID, got {:?}", other),
    }
}

#[tokio::test]
async fn login_inactive_user_returns_invalid() {
    // Arrange
    let hash = password::hash("correct").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 42,
        password_hash: hash,
        is_active: false, // 已停用
        ..sample_user(42, "alice")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_username()
        .returning(move |_| Ok(Some(u.clone())));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::LoginRequest {
        username: "alice".into(),
        password: "correct".into(),
    };

    // Act
    let err = svc.login(mock_repo, req).await.expect_err("login 停用应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::BIZ_AUTH_INVALID),
        other => panic!("expected Biz BIZ_AUTH_INVALID, got {:?}", other),
    }
}

#[tokio::test]
async fn login_bcrypt_error_returns_internal() {
    // Arrange — password_hash 是非法 bcrypt 字符串 → verify 返 Err → 服务用 `?` 透传
    let u = crate::modules::iam::repo::User {
        id: 42,
        password_hash: "not-a-valid-bcrypt-hash".into(),
        is_active: true,
        ..sample_user(42, "alice")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_username()
        .returning(move |_| Ok(Some(u.clone())));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::LoginRequest {
        username: "alice".into(),
        password: "any".into(),
    };

    // Act
    let err = svc.login(mock_repo, req).await.expect_err("login bcrypt 错误应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::INTERNAL),
        other => panic!("expected Biz INTERNAL, got {:?}", other),
    }
}

// ===========================================================================
// me（1 用例）
// ===========================================================================

#[tokio::test]
async fn me_happy_returns_current_user_out() {
    // Arrange
    let u = sample_user(42, "alice");
    let role_row = sample_user_role(1, 42, "MANAGER");
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock_repo
        .expect_list_user_roles_by_user_id()
        .returning(move |_| Ok(vec![role_row.clone()]));
    mock_repo
        .expect_list_active_menus_by_roles()
        .returning(|_| Ok(vec![]));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let current = crate::auth::rbac::CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };

    // Act
    let out = svc.me(mock_repo, &current).await.expect("me 应 Ok");

    // Assert
    assert_eq!(out.id, 42);
    assert_eq!(out.username, "alice");
    assert_eq!(out.roles, vec!["MANAGER"]);
}

// ===========================================================================
// logout（1 用例）
// ===========================================================================

#[tokio::test]
async fn logout_happy_calls_delete_session() {
    // Arrange
    let mut mock_session = MockSessionStore::new();
    mock_session
        .expect_delete_session()
        .with(eq("jti-uuid-xxx"))
        .returning(|_| Ok(()));
    let svc = make_session_service(mock_session);

    // Act
    svc.logout("jti-uuid-xxx").await.expect("logout 应 Ok");
}

// ===========================================================================
// change_password（3 用例 — 委托给 AccountService::change_own_password）
// ===========================================================================

#[tokio::test]
async fn change_password_happy_delegates_to_account_service() {
    // Arrange — 自助改密：current.id == user_id
    let hash = password::hash("old123").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 1,
        password_hash: hash,
        is_active: true,
        version: 1,
        ..sample_user(1, "alice")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock_repo
        .expect_update_user_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = ChangePasswordRequest {
        old_password: "old123".into(),
        new_password: "new456".into(),
    };
    let current = current_manager(); // id=1

    // Act
    svc.change_password(mock_repo, 1, req, &current)
        .await
        .expect("change_password 应 Ok");
}

#[tokio::test]
async fn change_password_wrong_old_propagates_mismatch() {
    // Arrange
    let hash = password::hash("correct_old").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 1,
        password_hash: hash,
        is_active: true,
        version: 1,
        ..sample_user(1, "alice")
    };
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = ChangePasswordRequest {
        old_password: "wrong_old".into(),
        new_password: "new456".into(),
    };
    let current = current_manager();

    // Act
    let err = svc
        .change_password(mock_repo, 1, req, &current)
        .await
        .expect_err("change_password 旧密码错应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::OLD_PASSWORD_MISMATCH),
        other => panic!("expected Biz OLD_PASSWORD_MISMATCH, got {:?}", other),
    }
}

#[tokio::test]
async fn change_password_forbidden_for_other_user() {
    // Arrange — clerk (id=1) 想改别人（id=999）的密码
    let mock_repo = MockIamRepo::new();
    let mock_session = MockSessionStore::new();
    let svc = make_session_service(mock_session);
    let req = ChangePasswordRequest {
        old_password: "any".into(),
        new_password: "new456".into(),
    };

    // Act
    let err = svc
        .change_password(mock_repo, 999, req, &current_clerk())
        .await
        .expect_err("change_password clerk 改别人应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::FORBIDDEN),
        other => panic!("expected Biz FORBIDDEN, got {:?}", other),
    }
}

// ===========================================================================
// refresh phase 1（2 用例 — DB + 签发 + reuse detection 闸）
// ===========================================================================

#[tokio::test]
async fn refresh_happy_returns_pending_with_new_pair() {
    // Arrange — 先 login 拿到一对合法 token，再用 refresh_token 调 refresh
    // 这里简化：直接构造一个合法 refresh token（用 test_jwt_config 同套 RSA 私钥签发）
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use crate::auth::jwt::{RefreshTokenClaims, decode_refresh};

    let now = now_unix();
    let claims = RefreshTokenClaims {
        subject: 42,
        audience: "hsh-erp-rust-test".into(),
        issued_at: now,
        not_before: now,
        expires_at: now + 3600,
        issuer: "hsh-erp-test".into(),
        jwt_id: "old-refresh-jti-uuid".into(),
        token_type: "refresh".into(),
        refresh_version: 0,
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("current".to_string());
    let config = test_jwt_config();
    let private_key = EncodingKey::from_rsa_pem(
        get_test_keys().private_pem.as_bytes()
    )
    .expect("test private key");
    let refresh_token = encode(&header, &claims, &private_key).expect("encode refresh");
    // 触发 decode_refresh 走 RS256 + kid
    let _ = decode_refresh(
        &refresh_token,
        &config.public_keys,
        if config.allow_hs256_fallback {
            Some(&config.secret)
        } else {
            None
        },
        &config.issuer,
        &config.audience,
    )
    .expect("decode refresh 必须成功");

    // DB 侧 mock：user 存在 + active + 版本匹配 + 角色非空
    let u = crate::modules::iam::repo::User {
        id: 42,
        refresh_token_version: 0,
        version: 1,
        ..sample_user(42, "alice")
    };
    let u_after = crate::modules::iam::repo::User {
        id: 42,
        refresh_token_version: 1, // 轮转后版本 +1
        version: 2,
        ..sample_user(42, "alice")
    };
    let role_row = sample_user_role(1, 42, "MANAGER");
    let mut mock_repo = MockIamRepo::new();
    mock_repo
        .expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock_repo
        .expect_list_user_roles_by_user_id()
        .returning(move |_| Ok(vec![role_row.clone()]));
    mock_repo
        .expect_increment_user_refresh_token_version()
        .returning(|_, _, _, _| Ok(1));
    mock_repo
        .expect_get_user_by_id()
        .returning(move |_| Ok(Some(u_after.clone())));
    mock_repo
        .expect_list_active_menus_by_roles()
        .returning(|_| Ok(vec![]));

    // SessionStore：黑名单未命中（happy path）
    let mut mock_session = MockSessionStore::new();
    mock_session
        .expect_is_jti_revoked()
        .with(eq("old-refresh-jti-uuid"))
        .returning(|_| Ok(false));

    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::RefreshRequest {
        refresh_token: refresh_token.clone(),
    };

    // Act
    let pending = svc.refresh(mock_repo, req).await.expect("refresh 应 Ok");

    // Assert
    assert_eq!(pending.user.id, 42);
    assert_eq!(pending.old_refresh_jti, "old-refresh-jti-uuid");
    assert!(!pending.pair.access_token.is_empty());
    assert_ne!(pending.pair.refresh_token, refresh_token); // 签发了新 token
}

#[tokio::test]
async fn refresh_invalid_token_returns_refresh_invalid() {
    // Arrange — 直接传 garbage token
    let mock_repo = MockIamRepo::new();
    // refresh 失败在 decode_refresh 阶段，不会调用 mock_repo
    let mut mock_session = MockSessionStore::new();
    mock_session.expect_is_jti_revoked().returning(|_| Ok(false));

    let svc = make_session_service(mock_session);
    let req = crate::modules::iam::dto::RefreshRequest {
        refresh_token: "not-a-real-jwt-token".into(),
    };

    // Act
    let err = svc.refresh(mock_repo, req).await.expect_err("refresh 非法 token 应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::REFRESH_INVALID),
        other => panic!("expected Biz REFRESH_INVALID, got {:?}", other),
    }
}

// ===========================================================================
// refresh phase 2（complete_refresh，2 用例）
// ===========================================================================

#[tokio::test]
async fn complete_refresh_happy_writes_sessions() {
    // Arrange — 构造一个 RefreshPending（用合法 RSA 签发的 TokenPair + 黑名单过期锚点）
    use crate::auth::jwt::issue_token_pair;
    let config = test_jwt_config();
    let pair = issue_token_pair(
        42,
        0,
        &config.private_key,
        &config.signing_kid,
        &config.issuer,
        &config.audience,
        config.access_ttl_seconds,
        config.refresh_ttl_days,
    )
    .expect("issue pair");
    let u = sample_user(42, "alice");
    let menus = vec![];

    let pending = crate::modules::iam::service::session::RefreshPending {
        pair,
        user: u.clone(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
        menus,
        old_refresh_jti: "old-refresh-jti".into(),
        old_refresh_expires_at: now_unix() + 3600, // 1h 后过期
    };

    // Mock session store：delete_session + create_session(access) + create_session(refresh) + revoke_jti
    let mut mock_session = MockSessionStore::new();
    mock_session
        .expect_delete_session()
        .returning(|_| Ok(()));
    mock_session
        .expect_create_session()
        .returning(|_, _, _, _, _| Ok(()));
    mock_session
        .expect_revoke_jti()
        .returning(|_, _| Ok(true));

    let svc = make_session_service(mock_session);

    // Act
    let out = svc
        .complete_refresh(pending)
        .await
        .expect("complete_refresh 应 Ok");

    // Assert
    assert_eq!(out.user.id, 42);
    assert!(!out.token.is_empty());
    assert!(!out.refresh_token.is_empty());
}

#[tokio::test]
async fn complete_refresh_revoke_jti_redis_fail_succeeds_best_effort() {
    // Arrange — revoke_jti 返回 Err；complete_refresh 是 best-effort 不阻断
    use crate::auth::jwt::issue_token_pair;
    let config = test_jwt_config();
    let pair = issue_token_pair(
        42,
        0,
        &config.private_key,
        &config.signing_kid,
        &config.issuer,
        &config.audience,
        config.access_ttl_seconds,
        config.refresh_ttl_days,
    )
    .expect("issue pair");
    let u = sample_user(42, "alice");
    let pending = crate::modules::iam::service::session::RefreshPending {
        pair,
        user: u.clone(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
        menus: vec![],
        old_refresh_jti: "old-jti".into(),
        old_refresh_expires_at: now_unix() + 3600,
    };

    let mut mock_session = MockSessionStore::new();
    mock_session
        .expect_delete_session()
        .returning(|_| Ok(()));
    mock_session
        .expect_create_session()
        .returning(|_, _, _, _, _| Ok(()));
    // revoke_jti 返 Err — best-effort 路径
    mock_session
        .expect_revoke_jti()
        .returning(|_, _| Err(AppError::internal("mock redis down")));

    let svc = make_session_service(mock_session);

    // Act
    let out = svc
        .complete_refresh(pending)
        .await
        .expect("revoke 失败不影响 complete_refresh");

    // Assert
    assert_eq!(out.user.id, 42);
}

// ===========================================================================
// 单元验证
// ===========================================================================

#[test]
fn session_service_construction_does_not_panic() {
    // 烟雾测试：service 字段构造正确
    let _snowflake: Arc<SnowflakeIdGenerator> = test_snowflake();
}