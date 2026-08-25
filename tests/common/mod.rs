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
use hsh_erp_rust::infra::cos::{CosClient, NoopCos};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::state::AppState;

/// 测试 DB URL：与 `postgres-test` 容器（端口5429）+ `postgres_rust_test` 库配对。
fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://hsh_test:6065161test@localhost:5429/postgres_rust_test".to_string())
}

#[allow(dead_code)]
const TEST_DATABASE_URL_DEFAULT: &str = "postgres://hsh_test:6065161test@localhost:5429/postgres_rust_test";

/// Admin DB URL：用于在测试前创建 `postgres_rust_test` 库。
fn admin_database_url() -> String {
    std::env::var("ADMIN_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://hsh_test:6065161test@localhost:5429/postgres".to_string())
}

#[allow(dead_code)]
const ADMIN_DATABASE_URL_DEFAULT: &str = "postgres://hsh_test:6065161test@localhost:5429/postgres";

/// 测试用 JWT secret：长度 >= 32（HS256 建议）+ 与生产区分
const TEST_JWT_SECRET: &str = "test-secret-test-secret-test-secret-1234";

/// 测试用 Redis URL：连 `redis-test` 容器（端口6380），db index 15 与 dev 默认 0 隔离。
const TEST_REDIS_URL: &str = "redis://localhost:6380/15";

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
pub async fn test_redis_pool() -> RedisPool {
    let cfg = RedisConfig::from_url(TEST_REDIS_URL);
    cfg.create_pool(Some(RedisRuntime::Tokio1))
        .expect("create test redis pool — 确认 redis-test 容器在 6380")
}

/// 清空测试 Redis db（FLUSHDB；与 `clean_db` 配套保证 DB + Redis 状态都干净）。
///
/// 仅部分集成测试（如 auth_api）需要；其它测试不引用本函数 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub async fn clean_redis(pool: &RedisPool) {
    let mut conn = pool
        .get()
        .await
        .expect("get redis conn from test pool");
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

/// 清表（业务域全集）：配送分组 / 配送单 / 批次 / 工单 / 装配体 / 客户 / 申请人 / 工种 / 工人。
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
            t_assembly, \
            t_customer, t_applicant, \
            t_work_type, t_worker, \
            t_shelf_process, t_work_type_process, t_process \
         RESTART IDENTITY CASCADE",
    )
    .execute(pool)
    .await
    .expect("truncate business tables");
}

/// 构造测试用 AppState：与 main.rs 同形，差别仅在 secret / 数据库 / Redis URL。
///
/// `redis_pool` 必须事先建立并 `FLUSHDB`；返回的 `Arc<AppState>` 在每个用例内独占。
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
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: "test".into(),
            secret_key: "test".into(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 314_572_800,
        },
        snowflake: SnowflakeConfig {
            epoch_ms: 1_577_836_800_000,
            instance: 1,
            seq: 1,
        },
        redis: AppRedisConfig {
            url: TEST_REDIS_URL.to_string(),
            session_ttl_seconds: 3600,
            pool_max_size: 5,
        },
        max_request_body_size: 314_572_800,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        delivery_note_template_dir: std::path::PathBuf::from("template"),
    });
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
        config.snowflake.seq,
    ));
    let ws_hub = Arc::new(WsHub::new());
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);
    let shutdown = CancellationToken::new();
    let session: Arc<dyn SessionStore> = Arc::new(RedisSessionStore::new(redis_pool));
    Arc::new(AppState::new(
        pool, config, snowflake, ws_hub, cos, shutdown, session,
    ))
}

/// 测试便捷入口：只传 PgPool，自动建 Redis 池（db 15，与 dev 隔离）。
pub async fn test_state(pool: PgPool) -> Arc<AppState> {
    let redis_pool = test_redis_pool().await;
    test_state_with_redis(pool, redis_pool)
}

/// axum Router：与 main.rs 中的 `/api/v2` nest 同形。
///
/// 不再装 `inject_current_user_layer`：handler 现在用 `current: CurrentUser`
/// 直接参数（依赖 `CurrentUser` 的 `FromRequestParts<Arc<AppState>>` impl 自动
/// 从 Bearer JWT 解析），与生产路径一致。
pub fn test_app(state: Arc<AppState>) -> axum::Router {
    hsh_erp_rust::modules::v2_router().with_state(state)
}

// ===========================================================================
// Fixture helpers：建最小化的「admin / MANAGER / 一组货架 / 菜单」世界。
// ===========================================================================

pub async fn insert_user_with_password(
    pool: &PgPool,
    username: &str,
    plain_password: &str,
) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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
pub async fn insert_inactive_user(
    pool: &PgPool,
    username: &str,
    plain_password: &str,
) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

pub async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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

    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1, 1);
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
