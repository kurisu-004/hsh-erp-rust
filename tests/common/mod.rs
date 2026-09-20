//! 集成测试共享基建
//!
//! 2026-09-20 重大重构：自定义 bash runner 接管 PG 容器生命周期（替代原 `ephemeral-postgres`）。
//!
//! ## 两层隔离模型（新架构）
//!
//! ### Layer 1：容器生命周期 —— bash runner + trap EXIT
//! - `.cargo/config.toml` 的 `target.<cfg>.runner = "scripts/test_runner.sh"` 让每个
//!   integration test binary 启动前由 bash runner 起一个 `postgres:18-alpine` 容器，
//!   把容器 ID 绑到 shell `trap EXIT` —— binary 进程退出（无论正常 / panic / 信号）
//!   都强制 `docker rm -f`，**零 Rust Drop 依赖**。
//! - 4 个转义口：TEST_DATABASE_BASE_URL 已注入 / `--list` / 非 deps 路径（cargo run
//!   主 binary）/ hsh_erp_rust-*（lib/bin 单测）。详见 scripts/test_runner.sh。
//! - nextest session 模式：scripts/test_nextest.sh 起 1 个 session 级容器（绕过
//!   per-test 容器开销），binary 启动时走转义口 1 复用。
//!
//! ### Layer 2：每测试 fresh database —— test_pool() 内部
//! - `fresh_database_url()` 在 runner 起好的容器上 CREATE DATABASE test_<uuid>，
//!   返回独立 URL → `sqlx::migrate!` → 返回 PgPool。容器内多 db 共享，但 db 间
//!   schema 完全独立 → 彻底消除 TRUNCATE 残留 / `_sqlx_migrations` 状态串号。
//!
//! ### Snowflake ID 隔离（新增 2026-09-20）
//! - `test_snowflake_instance()` 用 pid ⊕ startup_nanos 派生 0-1023 unique instance，
//!   替代原固定 `instance=1` → 消除 nextest 并行下跨进程 user_id 撞 key（redis session key）。
//!
//! ## 已知限制
//! - 需要 docker daemon 在 PATH 且能 pull `postgres:18-alpine`
//! - 跑 `cargo test` 时建议 `RUST_TEST_THREADS=4`（.cargo/config.toml 已强制），
//!   或用 cargo-nextest 跑（scripts/test_nextest.sh —— 推荐路径）
//!
//! ## 惯用开头（保持与原 helper 接口一致）
//! ```ignore
//! ensure_database_exists().await; // no-op（保留仅为不让 caller 报错）
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
//
// 2026-09-16 PR-3 fix：允许 `clippy::await_holding_lock` —— PR-3 step3 引入的
// `pool_snowflake()` 全局共享 std::sync::Mutex，所有 fixture helper（part/user/
// chain/step 等）模式都是「lock 拿 guard → next_id() → .await INSERT」，guard
// 在 .await 期间仍持有。改成 `lock().next_id()` expression 形式（不持 guard
// 跨 await）需要重写所有 12+ helper，收益与风险不对等；测试场景下进程内单
// task 不会与其他 task 抢该 mutex（每个 test 内 fixture 串行调用），不会
// 真实死锁。
#![allow(dead_code, clippy::duplicate_mod, clippy::await_holding_lock)]

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::{Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime};

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

use std::sync::OnceLock;
/// 全局共享 snowflake 生成器（PR-3 测试 helper 批量插入时使用）；
/// 多个 helper 在同一毫秒调用不再产生冲突 ID（避免 shelf_id == process_id 等碰撞）。
///
/// 2026-09-20：instance 改为 `test_snowflake_instance()` 派生（pid ⊕ 启动纳秒），
/// 取代固定 `instance=1`。nextest process-per-test 模型下，跨进程并行若共享
/// instance=1 会撞 redis session key（sessions:user:{id} 等）。
static TEST_SNOWFLAKE_GEN: OnceLock<std::sync::Mutex<SnowflakeIdGenerator>> = OnceLock::new();
pub fn pool_snowflake() -> &'static std::sync::Mutex<SnowflakeIdGenerator> {
    TEST_SNOWFLAKE_GEN.get_or_init(|| {
        std::sync::Mutex::new(SnowflakeIdGenerator::new(
            1_577_836_800_000,
            test_snowflake_instance(),
        ))
    })
}

/// per-process snowflake instance：pid ⊕ 启动时间纳秒低位 → 0-1023。
///
/// 2026-09-20 新增：nextest process-per-test 模型下，同毫秒并行的多个测试进程若
/// 共享 (epoch, instance=1) 会生成相同 user_id → 撞 redis key（sessions:user:{id} /
/// upload_session:{id}:{scope}）。pid 与 startup_nanos 的低位异或后 mod 1024 即可在
/// 1024 个并行进程内几乎无碰撞；实现零依赖（不引入 fnv crate）。
fn test_snowflake_instance() -> u16 {
    static INSTANCE: OnceLock<u16> = OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let pid = std::process::id() as u64;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        ((pid ^ nanos) % 1024) as u16
    })
}
use hsh_erp_rust::state::AppState;

/// 2026-09-20：fresh database URL —— 在 runner 已起好的容器上 CREATE DATABASE。
///
/// 容器由 scripts/test_runner.sh（或 scripts/test_nextest.sh session 级）提前
/// 起好，通过 `TEST_DATABASE_BASE_URL` 注入（形如 `postgres://postgres:postgres@127.0.0.1:<port>`）。
///
/// `max_connections(10) → 2`（plan §4 B7 调整）：admin pool 只跑一条 CREATE
/// DATABASE；nextest 8 并行下 8×(2 admin + 10 test pool + 迁移连接) ≈ 120
/// 连接 < session 容器 max_connections=500 上限。`Cluster::create_database()`
/// 内部 admin pool 的 5 连接 PoolTimedOut 问题不再存在（已经不走那个 API）。
async fn fresh_database_url() -> String {
    let base_url = std::env::var("TEST_DATABASE_BASE_URL").expect(
        "TEST_DATABASE_BASE_URL 未设置 —— 经 `cargo test` 运行（runner 自动注入）；\
         或手动 export 指向 postgres-test：\
         postgres://hsh_test:6065161test@localhost:5429",
    );
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&format!("{base_url}/postgres"))
        .await
        .expect("connect ephemeral admin pool");
    let db_name = format!("test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE \"{db_name}\""
    )))
    .execute(&admin)
    .await
    .expect("CREATE DATABASE on ephemeral admin pool");
    // admin pool 在函数末尾 drop，PG 后端进程立即关闭。
    drop(admin);
    format!("{base_url}/{db_name}")
}

/// 测试 DB URL：仅作 `AppConfig.database_url` 字段占位。**实际连接走 caller 传入
/// 的 `PgPool`**，测试路径不经过 `infra/db.rs::create_pool`，所以本返回值是否
/// 「合法」无关紧要 —— 永远不会被任何代码 dial。
///
/// 2026-09-20：保留固定占位字符串。runner 起容器 + test_pool() 走 fresh_database_url()
/// 派生真 URL，AppConfig 这一字段值不再被任何代码实际读取。
fn test_database_url() -> String {
    "postgres://ephemeral/pending".to_string()
}

/// 测试用 JWT secret：长度 >= 32（HS256 建议）+ 与生产区分
const TEST_JWT_SECRET: &str = "test-secret-test-secret-test-secret-1234";

/// 测试用 Redis URL：默认连 `redis-test` 容器（端口6380），db index 用测试
/// binary 名派生（替代原固定 db=15）。跨 binary 隔离 session key；同 binary
/// 内多线程仍共享 → clean_redis() 的 FLUSHDB 必须在每个需要 session 的测试
/// 前调。
///
/// 2026-09-20 race #2 修复：3 个 FLUSHDB binary（_e2e_api / auth_middleware /
/// iam_api）分配**固定独占** db（13/14/15），其它 binary 走 FNV-1a hash mod 13
/// 派生。理由：nextest 跨 binary 并行下，这 3 个 binary 的 setup 调 FLUSHDB
/// 会杀其它并行测试的 session → 40105 SESSION_REVOKED flake。即便走
/// `.config/nextest.toml` 的 `test-group = "redis-flush"` + max-threads=1 串行
/// 仍可能因 hash collision 误清空其它 binary 的 db（derive 算法 hash bin 名
/// 可能落到 13/14/15 之一）；固定独占 db 从源头断绝冲突。
///
/// 来源选择：nextest 下 `CARGO_BIN_NAME` 是测试 binary 名（"iam_api"），
/// 但保险起见改用 `std::env::current_exe()` 解析文件名（rust test binary 路径
/// 形如 `target/debug/deps/iam_api-<hash>`），对两种调用模式（cargo test 与
/// cargo nextest run）都生效。
///
/// 可由 `TEST_REDIS_URL` 环境变量整体覆盖（跨 worktree 隔离用）。
pub fn test_redis_url() -> String {
    if let Ok(url) = std::env::var("TEST_REDIS_URL") {
        return url;
    }
    let bin = current_test_binary_name();
    let db_index = match bin.as_str() {
        "_e2e_api" => 13,
        "auth_middleware" => 14,
        "iam_api" => 15,
        _ => redis_db_index(&bin) % 13,
    };
    format!("redis://localhost:6380/{db_index}")
}

/// 取当前测试 binary 名（去掉 cargo 注入的 hash 后缀）。
///
/// nextest 下 binary 路径形如 `target/debug/deps/iam_api-<16hex>`；
/// 解析出 `iam_api` 这部分作为 caller 路由 db index 的依据。
fn current_test_binary_name() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("default"));
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default");
    // binary 名形如 "iam_api-3f4a5b6c7d8e9f01" → 切 '-' 取首段
    stem.split('-').next().unwrap_or("default").to_string()
}

/// FNV-1a 32-bit hash（手写，避开新增 fnv crate 依赖）。
fn redis_db_index(bin: &str) -> u8 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bin.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (h % 256) as u8
}

/// 2026-09-20 起为 no-op：runner 已起容器 + `fresh_database_url()` 每次 `test_pool()`
/// 都建独立 db，不需要预创建共享库。保留函数签名仅为不让 caller 报错。
pub async fn ensure_database_exists() {
    // no-op：容器由 runner 起，database 由 test_pool() 内部 fresh_database_url() 派生
}

/// 建测试连接池：从 runner 已起好的容器派生一个 fresh database + 跑全部迁移。
///
/// 两层隔离：
/// - 容器：runner 进程级 1 个（同进程多测试共享容器，但 DB 独立）
/// - 数据库：每测试 fresh database → TRUNCATE 残留 / `_sqlx_migrations` 状态不串号
/// - snowflake instance：per-process 派生（test_snowflake_instance）→ 跨进程不撞 ID
pub async fn test_pool() -> PgPool {
    // 1) fresh database URL —— 临时 admin pool 跑 CREATE DATABASE 后立即 drop。
    let db_url = fresh_database_url().await;

    // 2) Open 一个 PgPool 指向新 db（max_connections=10 足够测试）。
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&db_url)
        .await
        .expect("connect to fresh ephemeral database");

    // 3) 跑全部迁移。
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply migrations on ephemeral test db");

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
        // 2026-09-20 新增：HTTP nest 请求超时；30s 默认足够测试用例（<1s）。
        request_timeout_seconds: 30,
        // 2026-09-18 新增：upload_session 域默认配置（测试场景）
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://backend-test:8000".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        },
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
        Arc::new(RedisUploadSessionRepo::new(redis_pool));
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
    ))
}

/// 构造测试用 AppState：session check **关闭**，**不**建 Redis 池。
///
/// 用途：验证 `REDIS_SESSION_CHECK_ENABLED=false` 时，`auth::middleware::verify_access_token`
/// 的关闭分支直接用 JWT Claims 构造 CurrentUser，不依赖 Redis 进程存在。
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
        // 2026-09-20 新增：HTTP nest 请求超时。
        request_timeout_seconds: 30,
        // 2026-09-18 新增：upload_session 域默认配置（测试场景）
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://backend-test:8000".into(),
            ttl_seconds: 86400,
            sts_duration_seconds: 7200,
            renew_threshold_seconds: 600,
        },
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
    ))
}

/// 测试便捷入口：只传 PgPool，自动建 Redis 池（db 15，与 dev 隔离）。
#[allow(dead_code)]
pub async fn test_state(pool: PgPool) -> Arc<AppState> {
    let redis_pool = test_redis_pool().await;
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
/// let state = common::test_state_with_cos(pool.clone(), cos.clone()).await;
/// // ... 调用 handler → cos.delete_calls.len() 应 = cleanup_tmp_keys.len()
/// ```
#[allow(dead_code)]
pub async fn test_state_with_cos(
    pool: PgPool,
    cos: Arc<dyn hsh_erp_rust::infra::cos::CosClient>,
) -> Arc<AppState> {
    let redis_pool = test_redis_pool().await;
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
            session_check_enabled: true,
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
        Arc::new(RedisUploadSessionRepo::new(redis_pool));
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
    ))
}

/// axum Router：与 main.rs 中的 `/api/v2` nest 同形。
///
/// 2026-09-20 修改：`v2_router(state)` 收 Arc<AppState>（用于 from_fn_with_state 挂
/// auth_middleware），不再需要额外 `with_state`；中间件已内置，handler 端
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
// Fixture helpers：建最小化的「admin / MANAGER / 一组货架 / 菜单」世界。
// ===========================================================================

#[allow(dead_code)]
pub async fn insert_user_with_password(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap();
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
    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

    let snowflake = pool_snowflake().lock().unwrap();
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

/// 2026-09-16 PR-3 批次 step 化：to_process / place_on_shelf / send_to_outsource /
/// repair 等"进入生产流"端点要求 part 已绑定工艺链（migration 028 +
/// error code 20706 BIZ_PROCESS_CHAIN_REQUIRED）。本 helper 帮 part 建链 + 绑 part。
///
/// 返回 chain_id；caller 可继续调 `create_step` 加 step。
pub async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{part_id}"))
    .execute(pool)
    .await
    .expect("insert chain");
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
    chain_id
}

/// 2026-09-16 PR-3 批次 step 化：在指定 chain 内创建 step（process_id + sort_order）。
pub async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert chain step");
    step_id
}

/// 2026-09-16 PR-3 适配：seed 一个 active process（PR-3 之前测试用 999_999 占位 process_id，
/// 现 to_process / place_on_shelf 等端点会经 process_chain 守卫 + step JOIN 校验。
/// 本 helper 提供真实 t_process 行便于测试）。
pub async fn seed_test_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let proc_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, now(), now())",
    )
    .bind(proc_id)
    .bind(code)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert process");
    proc_id
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
