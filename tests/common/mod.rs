//! 集成测试共享基建
//!
//! 测试库由 docker-compose 的 `postgres-test` 服务提供（localhost:5429，
//! 账号 `hsh_test`）。首次跑测试时 `ensure_database_exists()` 自动建
//! `postgres_rust_test` 库（已存在则跳过），`test_pool()` 连接后跑
//! `sqlx::migrate!` apply 全部迁移。
//!
//! 每个集成测试用例在开头三步：
//! ```ignore
//! ensure_database_exists().await;
//! let pool = test_pool().await;
//! clean_db(&pool).await;
//! clean_redis(&redis_pool).await;  // 如需
//! ```
//!
//! 这之后所有表都处于「干净 + 已迁移」状态，可以放心 insert。

// 跨测试文件共享的 fixtures + helpers（admin_database_url / ensure_database_exists
// / test_pool / clean_db / insert_user_with_password 等）。每个 integration
// test binary 通过 `#[path = "common/mod.rs"] mod common;` 引入本模块，
// 但只用到其中一部分 helper → 编译单个 binary 时未引用的 helper 触发
// `dead_code` warning；统一在 common 根豁免，避免每个 binary 都加
// `#[allow(dead_code)]`。`duplicate-mod` 同理：多 test binary 共用本文件，
// clippy --all-targets 会扫到「同文件被多次作为模块加载」。
#![allow(dead_code, clippy::duplicate_mod)]

use std::sync::Arc;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::{Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime};

use hsh_erp_rust::auth::session::{RedisSessionStore, SessionStore};
use hsh_erp_rust::infra::config::{
    AppConfig, AutoCompleteConfig, CosConfig, JwtConfig, RedisConfig as AppRedisConfig,
    SnowflakeConfig,
};
use hsh_erp_rust::infra::cos::{CosClient, NoopCos, ObjectMeta};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::shared::error::{AppError, code};
use hsh_erp_rust::state::AppState;

/// 测试 DB URL：与 `postgres-test` 容器（端口5429）+ `postgres_rust_test` 库配对。
fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://hsh_test:6065161test@localhost:5429/postgres_rust_test".to_string()
    })
}

#[allow(dead_code)]
const TEST_DATABASE_URL_DEFAULT: &str =
    "postgres://hsh_test:6065161test@localhost:5429/postgres_rust_test";

/// Admin DB URL：用于在测试前创建 `postgres_rust_test` 库。
fn admin_database_url() -> String {
    std::env::var("ADMIN_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://hsh_test:6065161test@localhost:5429/postgres".to_string())
}

#[allow(dead_code)]
const ADMIN_DATABASE_URL_DEFAULT: &str = "postgres://hsh_test:6065161test@localhost:5429/postgres";

/// 测试用 JWT secret：长度 >= 32（HS256 建议）+ 与生产区分
const TEST_JWT_SECRET: &str = "test-secret-test-secret-test-secret-1234";

/// 测试用 Redis URL：默认连 `redis-test` 容器（端口6380），db index 15 与 dev 默认 0 隔离。
/// 可由 `TEST_REDIS_URL` 环境变量覆盖（跨 worktree 隔离用）。
pub fn test_redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://localhost:6380/15".to_string())
}

/// 第一次跑测试时建 `postgres_rust_test`（已存在则忽略）。
pub async fn ensure_database_exists() {
    let admin = PgPool::connect(&admin_database_url())
        .await
        .expect("connect admin db (postgres) — 确认 postgres-test 容器在 5429");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname='postgres_rust_test')",
    )
    .fetch_one(&admin)
    .await
    .expect("query pg_database");
    if !exists {
        sqlx::query(
            "CREATE DATABASE postgres_rust_test \
             ENCODING 'UTF8' LC_COLLATE 'en_US.utf8' LC_CTYPE 'en_US.utf8' TEMPLATE template0",
        )
        .execute(&admin)
        .await
        .expect("create test db");
    }
    admin.close().await;
}

/// 建测试连接池 + 跑迁移。已迁移过则 `migrate!` 是 no-op。
pub async fn test_pool() -> PgPool {
    let pool = PgPool::connect(&test_database_url())
        .await
        .expect("connect test db postgres_rust_test");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply migrations on test db");
    pool
}

/// 建测试用 Redis 连接池（db 15，与 dev 默认 db 0 隔离）。
#[allow(dead_code)]
pub async fn test_redis_pool() -> RedisPool {
    let cfg = RedisConfig::from_url(test_redis_url());
    cfg.create_pool(Some(RedisRuntime::Tokio1))
        .expect("create test redis pool — 确认 redis-test 容器在 6380")
}

/// 清空测试 Redis db（FLUSHDB；与 `clean_db` 配套保证 DB + Redis 状态都干净）。
///
/// 仅部分集成测试（如 auth_api）需要；其它测试不引用本函数 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub async fn clean_redis(pool: &RedisPool) {
    let mut conn = pool.get().await.expect("get redis conn from test pool");
    let _: () = AsyncCommands::flushdb::<()>(&mut conn)
        .await
        .expect("flushdb test redis");
}

/// 清表（auth 链路涉及的最小集）：用户/角色/菜单/角色-菜单/货架。
/// `schema_migrations`（sqlx 自动维护）不动。
///
/// 该函数**只**清理 auth 域相关表，**不**触碰业务域表（delivery / part / customer
/// 等），保证后续要追加业务域集成测试时可按需调用 `clean_business_db` 而不互相干扰。
pub async fn clean_db(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE t_user, t_user_role, t_menu, t_role_menu, t_shelf RESTART IDENTITY CASCADE",
    )
    .execute(pool)
    .await
    .expect("truncate auth-related tables");
}

/// 清表（业务域全集）：配送分组 / 配送单 / 批次 / 工单 / 装配体 / 客户 / 申请人 / 工种 / 工人
/// / 工艺链。
///
/// 与 `clean_db` 互补 —— 后者只清 auth 表，本函数负责 P1+ 业务域测试需要的「干净世界」。
/// 顺序按 FK 依赖自顶向下；CASCADE 兜底防止漏列。
///
/// 仅部分集成测试（如 delivery_*）需要；其它测试不引用本函数 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub async fn clean_business_db(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE \
            t_delivery_group_member, t_delivery_group, \
            t_delivery_note_event, t_delivery_note_counter, t_delivery_note, \
            t_part_batch, t_part_event, t_part, \
            t_process_chain_step, t_part_process_chain, \
            t_assembly, \
            t_customer, t_applicant, \
            t_work_type, t_worker, \
            t_shelf_process, t_work_type_process, t_process, \
            t_outsource_company_process, t_outsource_company, \
            t_outsource_shipment, t_outsource_quote_event, t_outsource_quote \
         RESTART IDENTITY CASCADE",
    )
    .execute(pool)
    .await
    .expect("truncate business tables");
}

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
            access_ttl_hours: 12,
            refresh_ttl_days: 7,
        },
        cos: CosConfig {
            // 2026-09-11 修改：新增 enabled / app_id / endpoint 字段；测试场景全部置 false / 空。
            // 2026-09-16 M2-A：新增 sts_duration_seconds / tmp_prefix 字段（STS 占位用）。
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
            sts_duration_seconds: 900,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            instance: 1,
        },
        redis: AppRedisConfig {
            url: test_redis_url(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
            session_check_enabled: true,
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
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));
    let ws_hub = Arc::new(WsHub::new());
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);
    // 2026-09-16 M2-A：测试场景 STS 用 NoopSts 占位（不连真实 GetFederationToken）。
    let sts: Arc<dyn hsh_erp_rust::infra::sts::StsCredentialIssuer> =
        Arc::new(hsh_erp_rust::infra::sts::NoopSts);
    let shutdown = CancellationToken::new();
    let session: Arc<dyn SessionStore> = Arc::new(RedisSessionStore::new(redis_pool));
    Arc::new(AppState::new(
        pool, config, snowflake, ws_hub, cos, sts, shutdown, session,
    ))
}

/// 构造测试用 AppState：session check **关闭**，**不**建 Redis 池。
///
/// 用途：验证 `REDIS_SESSION_CHECK_ENABLED=false` 时，extractor 直接用 JWT Claims
/// 构造 CurrentUser，不依赖 Redis 进程存在。
///
/// 仅 `tests/auth_api.rs` 调用；其它 integration test 不引用 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub fn test_state_with_disabled_session(pool: PgPool) -> Arc<AppState> {
    let config = Arc::new(AppConfig {
        database_url: test_database_url(),
        listen_addr: "0.0.0.0:3000".to_string(),
        jwt: JwtConfig {
            secret: TEST_JWT_SECRET.to_string(),
            issuer: "hsh-erp-test".to_string(),
            access_ttl_hours: 12,
            refresh_ttl_days: 7,
        },
        cos: CosConfig {
            // 2026-09-11 修改：新增 enabled / app_id / endpoint 字段；测试场景全部置 false / 空。
            // 2026-09-16 M2-A：新增 sts_duration_seconds / tmp_prefix 字段（STS 占位用）。
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
            sts_duration_seconds: 900,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            instance: 1,
        },
        redis: AppRedisConfig {
            url: test_redis_url(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
            session_check_enabled: false,
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
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));
    let ws_hub = Arc::new(WsHub::new());
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);
    // 2026-09-16 M2-A：测试场景 STS 用 NoopSts 占位（不连真实 GetFederationToken）。
    let sts: Arc<dyn hsh_erp_rust::infra::sts::StsCredentialIssuer> =
        Arc::new(hsh_erp_rust::infra::sts::NoopSts);
    let shutdown = CancellationToken::new();
    // 注意：NoopSessionStore 不需要 Redis 池
    use hsh_erp_rust::auth::session::NoopSessionStore;
    let session: Arc<dyn SessionStore> = Arc::new(NoopSessionStore::new());
    Arc::new(AppState::new(
        pool, config, snowflake, ws_hub, cos, sts, shutdown, session,
    ))
}

/// 测试便捷入口：只传 PgPool，自动建 Redis 池（db 15，与 dev 隔离）。
#[allow(dead_code)]
pub async fn test_state(pool: PgPool) -> Arc<AppState> {
    let redis_pool = test_redis_pool().await;
    test_state_with_redis(pool, redis_pool)
}

/// axum Router：与 main.rs 中的 `/api/v2` nest 同形。
///
/// 不再装 `inject_current_user_layer`：handler 现在用 `current: CurrentUser`
/// 直接参数（依赖 `CurrentUser` 的 `FromRequestParts<Arc<AppState>>` impl 自动
/// 从 Bearer JWT 解析），与生产路径一致。
#[allow(dead_code)]
pub fn test_app(state: Arc<AppState>) -> axum::Router {
    hsh_erp_rust::modules::v2_router().with_state(state)
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
// Fixture helpers：建最小化的「admin / MANAGER / 一组货架 / 菜单」世界。
// ===========================================================================

#[allow(dead_code)]
pub async fn insert_user_with_password(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        id,
        username.to_lowercase(),
        hash,
        username,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 is_active=false 的用户（用于测试「已停用账号」拒绝登录）
#[allow(dead_code)]
pub async fn insert_inactive_user(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, false, 0, 0, $5, $5)",
        id,
        username.to_lowercase(),
        hash,
        username,
        now,
    )
    .execute(pool)
    .await
    .expect("insert inactive t_user");
    id
}

#[allow(dead_code)]
pub async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
        id,
        user_id,
        role,
        scope_type,
        scope_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

#[allow(dead_code)]
pub async fn insert_menu(
    pool: &PgPool,
    code: &str,
    title: &str,
    path: Option<&str>,
    parent_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_menu (id, parent_id, code, title, path, sort_order, is_active, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, true, 0, $6, $6)",
        id,
        parent_id,
        code,
        title,
        path,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_menu");
    id
}

#[allow(dead_code)]
pub async fn add_role_menu(pool: &PgPool, role: &str, menu_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_role_menu (id, role, menu_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
        id,
        role,
        menu_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_role_menu");
}

#[allow(dead_code)]
pub async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        id,
        code,
        name,
        zone,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 取 user 当前 `refresh_token_version`
#[allow(dead_code)]
pub async fn get_refresh_token_version(pool: &PgPool, user_id: i64) -> i32 {
    let row = sqlx::query!(
        "SELECT refresh_token_version AS \"ver!\" FROM t_user WHERE id = $1",
        user_id
    )
    .fetch_one(pool)
    .await
    .expect("query refresh_token_version");
    row.ver
}

// ===========================================================================
// worker-pool 域 fixture helpers（Task 10 e2e 测试用）：
//   - seed_process: 插一个 t_process 工序（INHOUSE 类别）
//   - link_work_type_to_process: t_work_type_process 映射
//   - link_shelf_to_process: t_shelf_process 映射
//
// 命名风格：与 part_api.rs 的 insert_part / insert_batch 同形（prefix=动词 + 名词）。
// 雪花 ID：复用同一 epoch/instance/seq（1_577_836_800_000 / 1 / 1），与其它 fixture 一致。
// ===========================================================================

/// 插一个 INHOUSE 类别 `t_process` 工序（worker-pool 用：INHOUSE 自产）。
#[allow(dead_code)]
pub async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        id,
        code,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// `t_work_type_process` 映射（无业务软删：`deleted_at` 留默认 NULL）。
#[allow(dead_code)]
pub async fn link_work_type_to_process(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        wt_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}

/// `t_shelf_process` 映射（无业务软删）。
#[allow(dead_code)]
pub async fn link_shelf_to_process(pool: &PgPool, s_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        s_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

// ===========================================================================
// 2026-09-16 M2-B review 第 1 轮：MockCos
//
// 给 part_file / batch_create 集成测试用，按 key lookup 决定 head/copy/get/...
// 的返回内容（NoopCos 全部返回 success 或 size=0，无法驱动 21114/21115/21116
// 错误码分支）。生产实现 `NoopCos` 仍在 cos.rs 维护，本文件仅补测试侧 stub。
//
// 设计要点：
// - `Arc<dyn CosClient>` 可直接替换 `state.cos`，对 service 层零侵入
// - `head_responses`：按 tmp_key 返回不同 ObjectMeta（驱动 size mismatch 测试）
// - `copy_results`：按 (src, dst) 返回 Ok 或带错误码的 Err（驱动 copy 失败分支）
// - 默认返回 NoSuchKey 错误（与真实 COS 行为一致），便于验证 21114 TMP_MISSING
// - 线程安全：`parking_lot::Mutex`（无锁实现，性能足够测试用）
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
/// （`parking_lot` 不在依赖树，避免本轮新增 dev-deps）。
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
        self.head_responses.lock().unwrap().get(key).cloned().ok_or_else(|| {
            // 与 TencentCos 行为对齐：404 → NoSuchKey 包装为业务错误
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
