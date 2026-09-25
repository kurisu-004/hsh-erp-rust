//! `AppState` 构造 helper + `MockCos` stub
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `test_state_with_redis`（主构造 helper）
//! - `test_state` / `test_state_with_cos` / `test_state_with_disabled_session` /
//!   `test_state_with_hs256_fallback_off`（变体）
//! - `test_app` / `test_ws_app`（axum Router helper）
//!
//! ## 设计要点（沿用原 mod.rs 实现）
//! - `AppConfig.jwt.private_key` 走 RS256 + kid（与 PR9 v2 重构同形）
//! - 默认 1s WS 心跳、enable_e2e_hooks=true、auto_complete 默认 7d threshold
//! - `test_state_with_disabled_session` 注入 `NoopSessionStore + NoopUploadSessionRepo +
//!   NoopIdempotencyStore`，跑 service 单元测试无需 Redis 进程
//! - `test_state_with_cos` 注入 caller 提供的 `Arc<dyn CosClient>` 替换 `NoopCos`
//!   —— 让 handler 后置 `spawn delete` 在集成测试里可端到端断言
//! - `MockCos` 给 part_file / batch_create 集成测试用，按 key 查找 head/copy
//!   响应，驱动 NoopCos 无法触发的 21114/21115/21116 错误码分支

use std::sync::Arc;

use deadpool_redis::Pool as RedisPool;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use hsh_erp_rust::auth::session::{RedisSessionStore, SessionStore};
use hsh_erp_rust::infra::config::{
    AppConfig, AutoCompleteConfig, CosBackend, CosConfig, JwtConfig, RedisConfig as AppRedisConfig,
    SnowflakeConfig, UploadSessionConfig,
};
use hsh_erp_rust::infra::cos::{CosClient, NoopCos, ObjectMeta};
use hsh_erp_rust::infra::python_sts::{NoopPythonSts, PythonSts};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::modules::upload_session::repo::{
    NoopUploadSessionRepo, RedisUploadSessionRepo, UploadSessionRepo,
};
use hsh_erp_rust::shared::error::{AppError, code};
use hsh_erp_rust::state::AppState;

use crate::pem;
use crate::pool::{test_database_url, test_snowflake_instance};
use crate::redis::test_redis_url;

/// 测试用 JWT secret：长度 >= 32（HS256 建议）+ 与生产区分
///
/// 2026-09-23 重构：HS256 fallback 过渡期仍占用（`allow_hs256_fallback=true`
/// 时 JWT_SECRET 必填，decode 端走 secret 验签历史 HS256 token）；下轮 cleanup
/// PR 删除 secret 字段 + fallback 路径。
const TEST_JWT_SECRET: &str = "test-secret-test-secret-test-secret-1234";

/// 构造测试用 AppState：与 main.rs 同形，差别仅在 secret / 数据库 / Redis URL。
///
/// `redis_pool` 必须事先建立并 `FLUSHDB`；返回的 `Arc<AppState>` 在每个用例内独占。
#[allow(dead_code)]
pub fn test_state_with_redis(pool: PgPool, redis_pool: RedisPool) -> Arc<AppState> {
    let config = Arc::new(AppConfig {
        database_url: test_database_url(),
        listen_addr: "0.0.0.0:3000".to_string(),
        jwt: JwtConfig {
            secret: TEST_JWT_SECRET.to_string(),
            issuer: "hsh-erp-test".to_string(),
            // 2026-09-22 新增 audience 字段（与生产 `hsh-erp-rust` 对齐；测试用独立值便于排查）
            audience: "hsh-erp-rust-test".to_string(),
            access_ttl_seconds: 900,
            refresh_ttl_days: 7,
            // 2026-09-23 重构：RS256 + kid 多密钥轮换。signing_kid = "current"
            // 与 pem 模块 DEFAULT_KID 对齐；public_keys 字典装入 (kid, pub_pem)
            // 多对（current + next），与生产 JwtConfig 同结构。private_key 从
            // pem 模块缓存的 PKCS#8 PEM 派生。allow_hs256_fallback=true 与
            // 生产默认对齐（JWT_SECRET 仍被 decode 端用于 fallback 验签）。
            signing_kid: "current".into(),
            private_key: jsonwebtoken::EncodingKey::from_rsa_pem(
                pem::test_private_pem().as_bytes(),
            )
            .expect("test private pem"),
            public_keys: {
                let mut m = std::collections::BTreeMap::new();
                for (kid, pem_str) in pem::test_public_kids() {
                    m.insert(
                        kid.to_string(),
                        jsonwebtoken::DecodingKey::from_rsa_pem(pem_str.as_bytes())
                            .expect("test public pem"),
                    );
                }
                m
            },
            allow_hs256_fallback: true,
        },
        cos: CosConfig {
            // 2026-09-11 修改：新增 enabled / app_id / endpoint 字段；测试场景全部置 false / 空。
            // 2026-09-20 spike：新增 backend 字段。
            // 2026-09-20 迁移清理：删 `sts_duration_seconds` 字段；backend 从 `CosSdk`
            // 改为 `OpenDal`（迁移后唯一真实 backend）。测试场景 `enabled=false` → 走
            // `NoopOpenDal`（OpenDAL Memory backend 本地内存）。
            backend: CosBackend::OpenDal,
            enabled: false,
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: "test".into(),
            secret_key: "test".into(),
            app_id: "".into(),
            endpoint: "".into(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 314_572_800,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            // 2026-09-20：per-process instance（pid ⊕ 启动纳秒 mod 1024），
            // 替代原固定 1，消除 nextest 跨进程并行撞 snowflake ID（详见 test_snowflake_instance）。
            instance: test_snowflake_instance(),
        },
        redis: AppRedisConfig {
            url: test_redis_url(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
        },
        max_request_body_size: 314_572_800,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        delivery_note_template_dir: std::path::PathBuf::from("template"),
        enable_e2e_hooks: true,
        // 2026-09-15 followup-cleanup A5/A6：测试默认 1s 心跳，E2E WS 用例可在 2s 内验到 text 帧。
        ws_heartbeat_interval_seconds: 1,
        // 2026-09-20 新增：HTTP nest 请求超时；30s 默认足够测试用例（<1s）。
        request_timeout_seconds: 30,
        // 2026-09-18 新增：upload_session 域默认配置（测试场景）
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://backend-test:8000".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        },
        // 2026-09-23 新增 Idempotency 中间件 TTL（测试默认 24h，与生产对齐）
        idempotency_ttl_seconds: 86400,
        // 2026-09-26 新增：测试默认禁用初始管理员 seed（与生产配置对齐；调用方
        // 测试需要时可走 `Arc::make_mut` 局部 patch）。
        bootstrap_admin_enabled: false,
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));
    let ws_hub = Arc::new(WsHub::new());
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);
    // 2026-09-18 M3-B：测试场景 STS 转发用 NoopPythonSts 占位（不连真实 python 后端）。
    // 原 `state.sts` (TencentSts / NoopSts) 2026-09-18 已删除——rust 不再直连腾讯云 STS。
    let python_sts: Arc<dyn PythonSts> = Arc::new(NoopPythonSts);
    let shutdown = CancellationToken::new();
    let session: Arc<dyn SessionStore> = Arc::new(RedisSessionStore::new(redis_pool.clone()));
    let upload_session_repo: Arc<dyn UploadSessionRepo> =
        Arc::new(RedisUploadSessionRepo::new(redis_pool.clone()));
    // 2026-09-23 新增 Idempotency 中间件存储：默认走 RedisIdempotencyStore
    // （与 session 共享同一 redis_pool）。
    let idempotency_store: Arc<dyn hsh_erp_rust::middleware::idempotency::IdempotencyStore> =
        Arc::new(hsh_erp_rust::middleware::idempotency::RedisIdempotencyStore::new(redis_pool));
    Arc::new(AppState::new(
        pool,
        config,
        snowflake,
        ws_hub,
        cos,
        python_sts,
        shutdown,
        session,
        upload_session_repo,
        // 2026-09-23 新增 Idempotency 中间件存储
        idempotency_store,
    ))
}

/// 2026-09-23 review #1 新增 fixture：构造 `allow_hs256_fallback=false` 的
/// `AppState`，其它字段与 `test_state_with_redis` 完全一致。
///
/// 用法：`tests/auth_middleware.rs::hs256_rejected_when_fallback_off_returns_40100`
/// —— 验证 `verify_session_token` / `decode_refresh` 在 fallback 关闭时把
/// hs256_fallback_secret 传 `None`，HS256 + 空 secret 的 token 一律 40100
/// "HS256 not allowed"（而不是 `DecodingKey::from_secret(b"")` 走空 HMAC bypass）。
///
/// 实现：`Arc::make_mut(&mut state.config)` —— 我们是 AppState 的唯一 Arc 持有者，
/// `config: Arc<AppConfig>` 也只被本 AppState 引用，copy-on-write 安全；
/// 直接修改 `.jwt.allow_hs256_fallback = false` 即可，不重建 services（service
/// 字段对 JWT 配置无依赖：JwtConfig 改造只影响 encode/decode，session_service
/// 只在 login/refresh 时透传给 jwt 函数，重建 service 字段无谓增加复杂度）。
#[allow(dead_code)]
pub async fn test_state_with_hs256_fallback_off(pool: PgPool) -> Arc<AppState> {
    let redis_pool = crate::redis::test_redis_pool().await;
    let mut state = test_state_with_redis(pool, redis_pool);
    // state.config 在 SessionService::new 内被 .clone() —— 共享强计数 > 1，
    // `Arc::get_mut(&mut state.config)` 会 panic。改走「构造新 config 替换」路径：
    // 1. 拿到唯一 state Arc（Arc::get_mut 在 state 上是 unique 的）
    // 2. 替换 state_inner.config 为新 Arc<AppConfig>（allow_hs256_fallback=false）
    // 3. session_service 仍持有旧 config —— 但本 fixture 仅走 auth_middleware 路径
    //    （不被 login/refresh 调用），不影响测试断言；HS256 fallback 关闸逻辑
    //    完全由 state.config.jwt.allow_hs256_fallback 控制（见
    //    `verify_session_token` 与 `iam::service::session::refresh` 的 hs256 分支透传）。
    let state_inner = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
    let old_cfg = (*state_inner.config).clone();
    let new_cfg = Arc::new(AppConfig {
        jwt: JwtConfig {
            allow_hs256_fallback: false,
            ..old_cfg.jwt.clone()
        },
        ..old_cfg
    });
    state_inner.config = new_cfg;
    state
}

/// service 单元测试 fixture：显式注入 `NoopSessionStore` + `NoopUploadSessionRepo`，
/// 不依赖 Redis 进程存在。
///
/// 当前 caller：auto_complete_api / part_crud（service 层单测，不发 HTTP）。
/// 其它 HTTP integration test 不引用 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub fn test_state_with_disabled_session(pool: PgPool) -> Arc<AppState> {
    let config = Arc::new(AppConfig {
        database_url: test_database_url(),
        listen_addr: "0.0.0.0:3000".to_string(),
        jwt: JwtConfig {
            secret: TEST_JWT_SECRET.to_string(),
            issuer: "hsh-erp-test".to_string(),
            // 2026-09-22 新增 audience 字段（与生产 `hsh-erp-rust` 对齐；测试用独立值便于排查）
            audience: "hsh-erp-rust-test".to_string(),
            access_ttl_seconds: 900,
            refresh_ttl_days: 7,
            // 2026-09-23 重构：RS256 + kid（与 test_state_with_redis 同形）
            signing_kid: "current".into(),
            private_key: jsonwebtoken::EncodingKey::from_rsa_pem(
                pem::test_private_pem().as_bytes(),
            )
            .expect("test private pem"),
            public_keys: {
                let mut m = std::collections::BTreeMap::new();
                for (kid, pem_str) in pem::test_public_kids() {
                    m.insert(
                        kid.to_string(),
                        jsonwebtoken::DecodingKey::from_rsa_pem(pem_str.as_bytes())
                            .expect("test public pem"),
                    );
                }
                m
            },
            allow_hs256_fallback: true,
        },
        cos: CosConfig {
            // 2026-09-11 修改：新增 enabled / app_id / endpoint 字段；测试场景全部置 false / 空。
            // 2026-09-20 spike：新增 backend 字段。
            // 2026-09-20 迁移清理：删 `sts_duration_seconds`；backend 从 `CosSdk` 改为 `OpenDal`。
            backend: CosBackend::OpenDal,
            enabled: false,
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: "test".into(),
            secret_key: "test".into(),
            app_id: "".into(),
            endpoint: "".into(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 314_572_800,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            // 2026-09-20：per-process instance（pid ⊕ 启动纳秒 mod 1024），
            // 替代原固定 1，消除 nextest 跨进程并行撞 snowflake ID（详见 test_snowflake_instance）。
            instance: test_snowflake_instance(),
        },
        redis: AppRedisConfig {
            url: test_redis_url(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
        },
        max_request_body_size: 314_572_800,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        delivery_note_template_dir: std::path::PathBuf::from("template"),
        enable_e2e_hooks: true,
        // 2026-09-15 followup-cleanup A5/A6：测试默认 1s 心跳。
        ws_heartbeat_interval_seconds: 1,
        // 2026-09-20 新增：HTTP nest 请求超时。
        request_timeout_seconds: 30,
        // 2026-09-18 新增：upload_session 域默认配置（测试场景）
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://backend-test:8000".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        },
        // 2026-09-23 新增 Idempotency 中间件 TTL（测试默认 24h，与生产对齐）
        idempotency_ttl_seconds: 86400,
        // 2026-09-26 新增：测试默认禁用初始管理员 seed（与生产配置对齐；调用方
        // 测试需要时可走 `Arc::make_mut` 局部 patch）。
        bootstrap_admin_enabled: false,
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));
    let ws_hub = Arc::new(WsHub::new());
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);
    // 2026-09-18 M3-B：测试场景 STS 转发用 NoopPythonSts 占位。
    let python_sts: Arc<dyn PythonSts> = Arc::new(NoopPythonSts);
    let shutdown = CancellationToken::new();
    // 注意：NoopSessionStore 不需要 Redis 池
    use hsh_erp_rust::auth::session::NoopSessionStore;
    let session: Arc<dyn SessionStore> = Arc::new(NoopSessionStore::new());
    let upload_session_repo: Arc<dyn UploadSessionRepo> = Arc::new(NoopUploadSessionRepo);
    // 2026-09-23 新增 Idempotency 中间件存储：disabled session 场景走 Noop
    let idempotency_store: Arc<dyn hsh_erp_rust::middleware::idempotency::IdempotencyStore> =
        Arc::new(hsh_erp_rust::middleware::idempotency::NoopIdempotencyStore::new());
    Arc::new(AppState::new(
        pool,
        config,
        snowflake,
        ws_hub,
        cos,
        python_sts,
        shutdown,
        session,
        upload_session_repo,
        // 2026-09-23 新增 Idempotency 中间件存储
        idempotency_store,
    ))
}

/// 测试便捷入口：只传 PgPool，自动建 Redis 池（db 15，与 dev 隔离）。
#[allow(dead_code)]
pub async fn test_state(pool: PgPool) -> Arc<AppState> {
    let redis_pool = crate::redis::test_redis_pool().await;
    test_state_with_redis(pool, redis_pool)
}

/// 2026-09-16 M2-C 增：构造测试用 AppState（用自定义 `CosClient` 替换 `state.cos`）。
///
/// 用途：让 handler 后置 `spawn delete` 在集成测试里可端到端断言
/// （如 `MockCos::delete_calls`）。其它配置与 `test_state_with_redis` 同形，
/// Redis 池 + session_store 一致；仅 `state.cos` 用 `cos` 参数替换。
///
/// ## 用法示例
/// ```ignore
/// let cos = std::sync::Arc::new(MockCos::new());
/// let state = hsh_erp_test_support::test_state_with_cos(pool.clone(), cos.clone()).await;
/// // ... 调用 handler → cos.delete_calls.len() 应 = cleanup_tmp_keys.len()
/// ```
#[allow(dead_code)]
pub async fn test_state_with_cos(
    pool: PgPool,
    cos: Arc<dyn hsh_erp_rust::infra::cos::CosClient>,
) -> Arc<AppState> {
    let redis_pool = crate::redis::test_redis_pool().await;
    let config = Arc::new(AppConfig {
        database_url: test_database_url(),
        listen_addr: "0.0.0.0:3000".to_string(),
        jwt: JwtConfig {
            secret: TEST_JWT_SECRET.to_string(),
            issuer: "hsh-erp-test".to_string(),
            // 2026-09-22 新增 audience 字段（与生产 `hsh-erp-rust` 对齐；测试用独立值便于排查）
            audience: "hsh-erp-rust-test".to_string(),
            access_ttl_seconds: 900,
            refresh_ttl_days: 7,
            // 2026-09-23 重构：RS256 + kid（与前两处同形）
            signing_kid: "current".into(),
            private_key: jsonwebtoken::EncodingKey::from_rsa_pem(
                pem::test_private_pem().as_bytes(),
            )
            .expect("test private pem"),
            public_keys: {
                let mut m = std::collections::BTreeMap::new();
                for (kid, pem_str) in pem::test_public_kids() {
                    m.insert(
                        kid.to_string(),
                        jsonwebtoken::DecodingKey::from_rsa_pem(pem_str.as_bytes())
                            .expect("test public pem"),
                    );
                }
                m
            },
            allow_hs256_fallback: true,
        },
        cos: CosConfig {
            // 2026-09-20 迁移清理：删 `sts_duration_seconds`；backend 从 `CosSdk` 改为 `OpenDal`。
            backend: CosBackend::OpenDal,
            enabled: false,
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: "test".into(),
            secret_key: "test".into(),
            app_id: "".into(),
            endpoint: "".into(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 314_572_800,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            // 2026-09-20：per-process instance（pid ⊕ 启动纳秒 mod 1024），
            // 替代原固定 1，消除 nextest 跨进程并行撞 snowflake ID（详见 test_snowflake_instance）。
            instance: test_snowflake_instance(),
        },
        redis: AppRedisConfig {
            url: test_redis_url(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
        },
        max_request_body_size: 314_572_800,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        delivery_note_template_dir: std::path::PathBuf::from("template"),
        enable_e2e_hooks: true,
        ws_heartbeat_interval_seconds: 1,
        // 2026-09-20 新增：HTTP nest 请求超时。
        request_timeout_seconds: 30,
        // 2026-09-18 新增：upload_session 域默认配置（测试场景）
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://backend-test:8000".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        },
        // 2026-09-23 新增 Idempotency 中间件 TTL（测试默认 24h，与生产对齐）
        idempotency_ttl_seconds: 86400,
        // 2026-09-26 新增：测试默认禁用初始管理员 seed（与生产配置对齐；调用方
        // 测试需要时可走 `Arc::make_mut` 局部 patch）。
        bootstrap_admin_enabled: false,
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));
    let ws_hub = Arc::new(WsHub::new());
    // 2026-09-18 M3-B：测试场景 STS 转发用 NoopPythonSts 占位（与 test_state_with_redis 一致）
    let python_sts: Arc<dyn PythonSts> = Arc::new(NoopPythonSts);
    let shutdown = CancellationToken::new();
    let session: Arc<dyn SessionStore> = Arc::new(RedisSessionStore::new(redis_pool.clone()));
    let upload_session_repo: Arc<dyn UploadSessionRepo> =
        Arc::new(RedisUploadSessionRepo::new(redis_pool.clone()));
    // 2026-09-23 新增 Idempotency 中间件存储：cos 替换场景同 test_state_with_redis
    let idempotency_store: Arc<dyn hsh_erp_rust::middleware::idempotency::IdempotencyStore> =
        Arc::new(hsh_erp_rust::middleware::idempotency::RedisIdempotencyStore::new(redis_pool));
    Arc::new(AppState::new(
        pool,
        config,
        snowflake,
        ws_hub,
        cos, // 注入的 cos（替换默认 NoopCos）
        python_sts,
        shutdown,
        session,
        upload_session_repo,
        // 2026-09-23 新增 Idempotency 中间件存储
        idempotency_store,
    ))
}

/// axum Router：与 main.rs 中的 `/api/v2` nest 同形。
///
/// 2026-09-20 修改：`v2_router(state)` 收 Arc<AppState>（用于 from_fn_with_state 挂
/// authenticate_middleware），不再需要额外 `with_state`；中间件已内置，handler 端
/// `current: CurrentUser` 直接从 extensions 读。
///
/// 显式 `with_state(state.clone())` 把 `Router<Arc<AppState>>` 类型擦回到
/// `axum::Router`（S 由调用方 inference），让测试侧 `send(app: axum::Router)`
/// 无需改签名。
#[allow(dead_code)]
pub fn test_app(state: Arc<AppState>) -> axum::Router {
    let state_for_router = state.clone();
    hsh_erp_rust::modules::v2_router(state).with_state(state_for_router)
}

/// axum Router：与 main.rs 中的 `/ws` nest 同形（用于 dashboard WS E2E 测试）。
///
/// 2026-09-15 followup-cleanup A4：测试需要真实 socket 客户端连接 ws://.../ws/dashboard，
/// 在 axum::Router 上 bind TcpListener 后跑 axum::serve，再用 tokio_tungstenite 连接。
#[allow(dead_code)]
pub fn test_ws_app(state: Arc<AppState>) -> axum::Router {
    hsh_erp_rust::modules::ws_router().with_state(state)
}

// ===========================================================================
// 测试用 MockCos：按 key 查找 `head_object` / `copy_object` 的响应，其余方法走默认行为。
//
// 2026-09-16 M2-B review 第 1 轮：补齐 T2.5 / T2.6 验收要求的集成测试 stub。
// NoopCos 的 `head_object` 返回 `size=0`、无法驱动 21114/21115 错误码分支；
// MockCos 按 key 查找 ObjectMeta，`copy_object` 按 (src, dst) 返回 Result。
//
// 设计要点：
// - `Arc<dyn CosClient>` 可直接替换 `state.cos`，对 service 层零侵入
// - `head_responses`：按 tmp_key 返回不同 ObjectMeta（驱动 size mismatch 测试）
// - `copy_results`：按 (src, dst) 返回 Ok 或带错误码的 Err（驱动 copy 失败分支）
// - 默认返回 NoSuchKey 错误（与真实 COS 行为一致），便于验证 21114 TMP_MISSING
// - 线程安全：`std::sync::Mutex`（`parking_lot` 不在依赖树，避免本轮新增 dev-deps）
//
// 当前实现覆盖的 6 个方法：put_object / get_object / presigned_get_url /
// delete_object / head_object / copy_object。
// ===========================================================================

/// 测试用 MockCos：按 key 查找 `head_object` / `copy_object` 的响应，其余方法走默认行为。
///
/// 2026-09-16 M2-B review 第 1 轮：补齐 T2.5 / T2.6 验收要求的集成测试 stub。
/// NoopCos 的 `head_object` 返回 `size=0`、无法驱动 21114/21115 错误码分支；
/// MockCos 按 key 查找 ObjectMeta，`copy_object` 按 (src, dst) 返回 Result。
///
/// 所有 `Mutex` 均在返回前 drop，无 `.await` 跨锁，故用 `std::sync::Mutex`
/// （`parking_lot` 不在依赖树，避免本轮新增 dev-deps`）。
pub struct MockCos {
    /// `head_object` 按 key 返回不同 `ObjectMeta`；缺省 → NoSuchKey 错误。
    pub head_responses: std::sync::Mutex<std::collections::HashMap<String, ObjectMeta>>,
    /// `copy_object` 按 (src, dst) 返回 Ok / Err；缺省 → Ok(())。
    pub copy_results:
        std::sync::Mutex<std::collections::HashMap<(String, String), Result<(), String>>>,
    /// `get_object` 按 key 返回字节；缺省 → NoSuchKey 错误。
    pub get_responses: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    /// 触发过的 `head_object` key 列表（测试可断言"head 被调用了 N 次"）。
    pub head_calls: std::sync::Mutex<Vec<String>>,
    /// 触发过的 `copy_object` (src, dst) 列表（测试可断言"copy 被调用了 N 次"）。
    pub copy_calls: std::sync::Mutex<Vec<(String, String)>>,
    /// 触发过的 `delete_object` key 列表（验证 spawn 兜底删除）。
    pub delete_calls: std::sync::Mutex<Vec<String>>,
}

impl Default for MockCos {
    fn default() -> Self {
        Self::new()
    }
}

impl MockCos {
    pub fn new() -> Self {
        Self {
            head_responses: std::sync::Mutex::new(std::collections::HashMap::new()),
            copy_results: std::sync::Mutex::new(std::collections::HashMap::new()),
            get_responses: std::sync::Mutex::new(std::collections::HashMap::new()),
            head_calls: std::sync::Mutex::new(Vec::new()),
            copy_calls: std::sync::Mutex::new(Vec::new()),
            delete_calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 注册 key → ObjectMeta（让 head_object 返回指定 size）。
    pub fn set_head(&self, key: &str, size: i64) {
        self.head_responses.lock().unwrap().insert(
            key.to_string(),
            ObjectMeta {
                size,
                etag: format!("mock-etag-{}", &key[..key.len().min(8)]),
            },
        );
    }

    /// 注册 (src, dst) → copy 结果（Ok 或 Err）。
    pub fn set_copy(&self, src: &str, dst: &str, result: Result<(), String>) {
        self.copy_results
            .lock()
            .unwrap()
            .insert((src.to_string(), dst.to_string()), result);
    }

    /// 取 head 被调用次数（用于断言 head 被调用 / 未被调用）。
    pub fn head_call_count(&self, key: &str) -> usize {
        self.head_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k.as_str() == key)
            .count()
    }

    /// 取 delete 被调用次数。
    pub fn delete_call_count(&self, key: &str) -> usize {
        self.delete_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k.as_str() == key)
            .count()
    }
}

#[async_trait::async_trait]
impl CosClient for MockCos {
    async fn put_object(
        &self,
        _key: &str,
        _body: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), AppError> {
        // Mock 不模拟服务端写入（测试不走真实上传）
        Ok(())
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError> {
        self.get_responses
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("MockCos get_object NoSuch key={key}"),
                )
            })
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        _expires_seconds: u32,
    ) -> Result<String, AppError> {
        Ok(format!("local://mock/{key}"))
    }

    async fn delete_object(&self, key: &str) -> Result<(), AppError> {
        self.delete_calls.lock().unwrap().push(key.to_string());
        // 模拟幂等：删除总成功（NoSuchKey 也视为成功）
        Ok(())
    }

    async fn head_object(&self, key: &str) -> Result<ObjectMeta, AppError> {
        self.head_calls.lock().unwrap().push(key.to_string());
        self.head_responses
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| {
                // 业务层 head_object 期望语义清晰：不存在 → 业务侧 404 / NoSuchKey
                // 包装为业务错误（与 OpenDalCos 行为对齐）
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("MockCos head_object NoSuch key={key}"),
                )
            })
    }

    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError> {
        self.copy_calls
            .lock()
            .unwrap()
            .push((src_key.to_string(), dst_key.to_string()));
        match self
            .copy_results
            .lock()
            .unwrap()
            .get(&(src_key.to_string(), dst_key.to_string()))
            .cloned()
        {
            Some(Ok(())) => Ok(()),
            Some(Err(msg)) => Err(AppError::biz(code::BIZ_PART_FILE_UPLOAD_FAILED, msg)),
            None => Ok(()), // 缺省成功
        }
    }
}
