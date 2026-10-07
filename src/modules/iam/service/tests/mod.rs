//! iam 域 service 层单元测试公共 fixture（2026-09-23 新增）
//!
//! 为 `account.rs`（27 用例）+ `session.rs`（13 用例）提供：
//! - `MockIamRepoTrait` 注入（mockall 0.15 automock 自动生成于 `iam/repo/mod.rs`）
//! - `test_snowflake()`：**lib 单测进程内唯一**的雪花 ID 生成器（2026-10-09 起
//!   转发到 `crate::shared::test_snowflake::shared_test_snowflake()`）
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
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::config::{AppConfig, JwtConfig, RedisConfig};
use crate::infra::snowflake::SnowflakeIdGenerator;

pub mod account;
pub mod session;

// ===========================================================================
// 雪花 ID 生成器（lib 单测进程内唯一，2026-10-09）
// ===========================================================================

/// 测试用雪花 ID 生成器 —— **lib 单测进程内唯一的同一个 generator 对象**
/// （`crate::shared::test_snowflake::shared_test_snowflake()`；为什么不用
/// test-support 的同名函数，见 `src/shared/test_snowflake.rs` 的模块 doc）。
///
/// 2026-10-09 改造：本函数原先每次调用都 `SnowflakeIdGenerator::new(1_735_689_600_000, 1)`
/// 现造一个**新对象**。位布局 `ts << 22 | instance << 12 | seq` 里 `last_ms` /
/// `sequence` 是 generator **对象私有**字段、`new()` 从 0 起步 ⇒ 任意两个 instance
/// 相同、对象不同的 generator 同毫秒各取 seq 0 会发出**逐字节相同**的 id，撞
/// `t_*_pkey`（23505）。instance 这 10 bit（1024 槽）现在只留给**跨进程**区分；
/// 进程内唯一性由共享对象按调用顺序串行发号保证。
///
/// 对 iam 单测无行为影响：本模块零 DB，任何 case 都不把生成的 id 落库或比对绝对值。
pub fn test_snowflake() -> Arc<SnowflakeIdGenerator> {
    crate::shared::test_snowflake::shared_test_snowflake().clone()
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
pub fn sample_user_role(
    id: i64,
    user_id: i64,
    role: &str,
) -> crate::modules::iam::repo::UserRoleRow {
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
        // cos / snowflake / auto_complete 等字段 session service 不读
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
            // 2026-10-09 新增：session service 单测走 NoopSessionStore，不读 Redis，
            // 前缀取生产缺省（空串）。
            key_prefix: String::new(),
        },
        enable_e2e_hooks: false,
        ws_heartbeat_interval_seconds: 30,
        // 2026-10-01 新增：WS 存活检测（session service 单测不读 WS，占位默认值，
        // 且需满足 `pong_timeout > ping_interval` 的语义约束）。
        ws_ping_interval_seconds: 20,
        ws_pong_timeout_seconds: 60,
        // 2026-10-02 新增：周期性 re-auth 周期（与生产缺省一致）。
        ws_reauth_every_n_heartbeats: 10,
        request_timeout_seconds: 30,
        // 2026-10-03 新增：打印路径长档（session service 单测不读，取生产缺省值）。
        print_request_timeout_seconds: 660,
        // 2026-09-28 删除：相关上传会话域字段（域整体下线）。
        idempotency_ttl_seconds: 86400,
        bootstrap_admin_enabled: false,
        // 2026-09-28 新增：rust → python 后端转发配置（session service 单测不读，留默认）。
        python_backend: crate::infra::config::PythonBackendConfig::default(),
        // 2026-09-29 新增：企业微信登录配置（session service 不读，占位默认值）
        wecom: crate::infra::config::WeComConfig::default(),
    })
}

/// 当前 unix 时间戳（秒）。便捷调用，少 import `chrono::Utc`。
#[allow(dead_code)]
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}
