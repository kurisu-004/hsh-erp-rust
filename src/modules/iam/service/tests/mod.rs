//! iam 域 service 层单元测试公共 fixture（2026-09-23 新增）
//!
//! 为 `account.rs`（27 用例）+ `session.rs`（13 用例）提供：
//! - `MockIamRepoTrait` 注入（mockall 0.15 automock 自动生成于 `iam/repo/mod.rs`）
//! - `test_snowflake()`：固定 `instance_id=1` 的雪花 ID 生成器
//! - `current_with_role(role)` / `current_manager()` / `current_worker()` / `current_inspector()`
//! - `make_account_service()` / `make_session_service()`：service 实例工厂
//! - `sample_user(...)` / `sample_user_role(...)`：mock 返回值构造
//! - `test_jwt_config()`：session service 测试用的 JwtConfig（含 RS256 RSA keypair）
//! - `MockSessionStore`：mockall 0.15 automock 自动生成于 `auth/session.rs`
//!
//! 零 DB / 零 Redis / 零网络依赖；`cargo test --lib iam::service::tests` 全自包含。

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use jsonwebtoken::{DecodingKey, EncodingKey};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use rsa::rand_core::OsRng;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::config::{AppConfig, JwtConfig, RedisConfig};
use crate::infra::snowflake::SnowflakeIdGenerator;

pub mod account;
pub mod session;

// ===========================================================================
// 雪花 ID 生成器（固定 instance_id=1）
// ===========================================================================

/// 测试用雪花 ID 生成器（instance_id=1，与生产对齐 `RUST_SNOWFLAKE_INSTANCE=1`）。
pub fn test_snowflake() -> Arc<SnowflakeIdGenerator> {
    Arc::new(SnowflakeIdGenerator::new(
        1_735_689_600_000, // 2025-01-01 UTC
        1,
    ))
}

// ===========================================================================
// CurrentUser 构造
// ===========================================================================

/// 按角色构造测试 `CurrentUser`（id 默认 1，username 默认 `"test"`）。
pub fn current_with_role(role: Role) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "test".to_string(),
        roles: vec![role],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// MANAGER 角色测试用户（id=1）。
pub fn current_manager() -> CurrentUser {
    current_with_role(Role::Manager)
}

/// CLERK 文员测试用户（id=1）。
pub fn current_clerk() -> CurrentUser {
    current_with_role(Role::Clerk)
}

// ===========================================================================
// Service 实例工厂
// ===========================================================================

/// AccountService 实例。
pub fn make_account_service() -> crate::modules::iam::service::AccountService {
    crate::modules::iam::service::AccountService::new(test_snowflake())
}

// ===========================================================================
// Sample 行构造（Mock 返回值）
// ===========================================================================

/// 构造一个最小化的 `User` 行（用于 `get_user_by_id` 等 mock 返回）。
pub fn sample_user(id: i64, username: &str) -> crate::modules::iam::repo::User {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 23)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    crate::modules::iam::repo::User {
        id,
        username: username.to_string(),
        password_hash: String::new(), // 测试场景密码字段由 mock 直接返固定值覆盖
        full_name: format!("Full {username}"),
        phone: None,
        is_active: true,
        last_login_at: None,
        refresh_token_version: 0,
        version: 1,
        created_at: now,
        created_by: Some(1),
        updated_at: now,
        updated_by: Some(1),
        deleted_at: None,
    }
}

/// 构造一个最小化的 `UserRoleRow`（用于 `list_user_roles_by_user_id` mock 返回）。
pub fn sample_user_role(id: i64, user_id: i64, role: &str) -> crate::modules::iam::repo::UserRoleRow {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 23)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    crate::modules::iam::repo::UserRoleRow {
        id,
        user_id,
        role: role.to_string(),
        scope_type: None,
        scope_id: None,
        version: 1,
        created_at: now,
        created_by: Some(1),
        updated_at: now,
        updated_by: Some(1),
        deleted_at: None,
        shelf_code: None,
        shelf_name: None,
    }
}

// ===========================================================================
// JWT 测试配置（仅 session service 测试需要 RSA keypair）
// ===========================================================================

/// 测试用 RSA 私钥/公钥 PEM（2048-bit）。
///
/// 进程级 OnceLock 缓存：每个测试 binary 首调时生成一次（~100ms），
/// 后续零成本。生成在内存中（不写磁盘），避免密钥泄露到 git。
pub(super) struct TestKeys {
    pub(super) private_pem: String,
    pub(super) public_pem: String,
}

static TEST_KEYS: OnceLock<TestKeys> = OnceLock::new();

/// 给同模块测试子文件使用：`session.rs` 需要拿私钥 PEM 自己签测试 token。
#[allow(dead_code)]
pub(super) fn get_test_keys() -> &'static TestKeys {
    TEST_KEYS.get_or_init(|| {
        let mut rng = OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成 RSA 私钥");
        let public = RsaPublicKey::from(&private);
        // sanity: key size 必须是 2048-bit（防未来 KEY_BITS 改小被静默吞）
        debug_assert_eq!(public.size() * 8, 2048);
        let private_pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .expect("私钥 PKCS#8 PEM 编码")
            .to_string();
        let public_pem = public
            .to_public_key_pem(LineEnding::LF)
            .expect("公钥 SPKI PEM 编码")
            .to_string();
        TestKeys {
            private_pem,
            public_pem,
        }
    })
}

/// 测试用 JwtConfig（signing_kid = "current"，RS256，allow_hs256_fallback=true）。
pub fn test_jwt_config() -> JwtConfig {
    let keys = get_test_keys();
    let mut public_keys = BTreeMap::new();
    public_keys.insert(
        "current".to_string(),
        DecodingKey::from_rsa_pem(keys.public_pem.as_bytes()).expect("test public key"),
    );
    JwtConfig {
        secret: "test-secret-do-not-use-in-prod".to_string(),
        issuer: "hsh-erp-test".to_string(),
        audience: "hsh-erp-rust-test".to_string(),
        access_ttl_seconds: 900,
        refresh_ttl_days: 7,
        signing_kid: "current".to_string(),
        private_key: EncodingKey::from_rsa_pem(keys.private_pem.as_bytes())
            .expect("test private key"),
        public_keys,
        allow_hs256_fallback: true,
    }
}

/// 测试用完整 AppConfig（仅 `jwt` 字段真实；其它字段保持默认值零填充）。
///
/// SessionService 仅读 `config.jwt` + `config.redis.session_ttl_seconds`，故其余
/// 字段可不填（不过 `AppConfig` 是完整结构体，必须给出）。
pub fn test_app_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        database_url: String::new(),
        listen_addr: "0.0.0.0:0".to_string(),
        jwt: test_jwt_config(),
        // cos / snowflake / auto_complete / upload_session 等字段 session service 不读
        // 但 AppConfig 字段全填，故用占位零值。详见 config.rs。
        cos: crate::infra::config::CosConfig {
            backend: crate::infra::config::CosBackend::Noop,
            enabled: false,
            region: String::new(),
            bucket: String::new(),
            secret_id: String::new(),
            secret_key: String::new(),
            app_id: String::new(),
            endpoint: String::new(),
            scheme: String::new(),
            upload_prefix: String::new(),
            presign_expire_seconds: 0,
            max_file_size: 0,
            tmp_prefix: String::new(),
        },
        snowflake: crate::infra::config::SnowflakeConfig {
            instance: 1,
            epoch_ms: 1_735_689_600_000,
        },
        max_request_body_size: 0,
        auto_complete: crate::infra::config::AutoCompleteConfig {
            threshold_days: 0,
            interval_hours: 0,
        },
        redis: RedisConfig {
            url: String::new(),
            session_ttl_seconds: 900,
            pool_max_size: 1,
        },
        delivery_note_template_dir: std::path::PathBuf::new(),
        enable_e2e_hooks: false,
        ws_heartbeat_interval_seconds: 30,
        request_timeout_seconds: 30,
        upload_session: crate::infra::config::UploadSessionConfig {
            python_backend_base_url: String::new(),
            ttl_seconds: 0,
            sts_duration_seconds: 0,
            renew_threshold_seconds: 0,
        },
        idempotency_ttl_seconds: 86400,
        bootstrap_admin_enabled: false,
    })
}

/// 当前 unix 时间戳（秒）。便捷调用，少 import `chrono::Utc`。
#[allow(dead_code)]
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}