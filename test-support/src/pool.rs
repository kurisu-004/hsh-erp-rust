//! DB 生命周期 + snowflake ID 生成
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `test_pool` + `fresh_database_url`（每测试 fresh database）
//! - `register_db_for_drop` + `drop_dbs_atexit`（进程退出回收）
//! - `pool_snowflake` + `test_snowflake_instance`（per-process 雪花 ID）
//!
//! ## 设计要点（沿用原 mod.rs 实现）
//! - 容器由 bash runner 启好（`scripts/test_runner.sh` / `scripts/test_nextest.sh`），
//!   `TEST_DATABASE_BASE_URL` 注入即可。
//! - `fresh_database_url()` 优先 `CREATE DATABASE test_<uuid> TEMPLATE
//!   hsh_erp_template`（nextest session 路径，~100ms tmpfs clone）；template
//!   不存在时 fallback 到 plain `CREATE DATABASE`（test_runner.sh 单 binary 兼容）。
//! - 每建库登记 `(server_url, db_name)` 到 `CREATED_DBS`，进程退出时
//!   `libc::atexit` 注册的 handler 在独立 std::thread + 全新 current_thread
//!   runtime 里跑 `DROP DATABASE ... WITH (FORCE)`。
//! - `pool_snowflake` 全局互斥，按 call 顺序发 ID，同进程内 `next_id()` 串行，
//!   跨进程 unique (epoch, instance) 不撞 ID。
//!
//! ## 模块可见性
//! - 公开 helper（外部 `common::*` 入口）：`test_pool` / `ensure_database_exists`
//! - 内部（pub(crate)）：`pool_snowflake` / `test_snowflake_instance` /
//!   `test_database_url` / `fresh_database_url` / `register_db_for_drop`
//!   —— 仅在 crate 内的 `state.rs` / `fixtures.rs` 使用

use std::sync::Once;
use std::sync::OnceLock;

use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// 全局共享 snowflake 生成器（PR-3 测试 helper 批量插入时使用）；
/// 多个 helper 在同一毫秒调用不再产生冲突 ID（避免 shelf_id == process_id 等碰撞）。
///
/// 2026-09-20：instance 改为 `test_snowflake_instance()` 派生（pid ⊕ 启动纳秒），
/// 取代固定 `instance=1`。nextest process-per-test 模型下，跨进程并行若共享
/// instance=1 会撞 redis session key（sessions:user:{id} 等）。
static TEST_SNOWFLAKE_GEN: OnceLock<std::sync::Mutex<SnowflakeIdGenerator>> = OnceLock::new();

/// 全局 snowflake 生成器访问器。
///
/// 公开暴露：原 `tests/common/mod.rs` 写的是 `pub fn`，部分 integration test
/// binary（worker_pool_api 等）通过 `common::pool_snowflake().lock().unwrap().next_id()`
/// 直接拿 ID 串联多 fixture —— 保持公开语义以不破坏现有 51 个 binary 的 import。
/// [`fixtures`](super::fixtures) 的 INSERT helper 也走它。
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
///
/// pub(crate)：[`state`](super::state) 构造 `AppConfig::snowflake::instance` 也读它，
/// 必须 crate 内可见。
pub(crate) fn test_snowflake_instance() -> u16 {
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

/// 2026-09-21：test_<uuid> 库进程级回收（atexit）。
///
/// `fresh_database_url()` 在每个测试里 `CREATE DATABASE test_<uuid> ...`，但
/// plan 2 仅由 session 容器删除回收 —— nextest session 跑 439 个测试时，
/// tmpfs 上残留 439 个完整 TEMPLATE 拷贝直到 trap EXIT。临时抱佛脚：进程内
/// 登记 `(server_url, db_name)`，退出时 `libc::atexit` 回调里建一次性
/// current_thread runtime，开 admin 连接 `DROP DATABASE ... WITH (FORCE)`。
///
/// 设计要点：
/// - `OnceLock<Mutex<Vec<...>>>`：进程级共享，`Once::call_once` 保证 `atexit`
///   只注册一次；
/// - `TEST_KEEP_DB=1`：调试逃生门，handler 立即返回，旧「失败保留容器 + 提示
///   inspect test_%」工作流通过 `TEST_KEEP_DB=1 ./scripts/test_nextest.sh <filter>`
///   重跑失败子集复现；
/// - `WITH (FORCE)`（PG13+ 语法，镜像 PG18 ✓）杀残留后端连接，不依赖 pool
///   close 时序；
/// - 仅 SIGTERM/SIGKILL 等信号杀死进程走不到 atexit（nextest slow-timeout
///   terminate-after=2 超时强杀场景）；量级 ≤ 并发数 × 单库大小，session
///   结束删容器时一并清。可接受，不引入 reaper。
///
/// 零调用点改动：`test_pool()` 签名不变，59 处 caller / 145 处 `pool.clone()`
/// 不动。
static CREATED_DBS: OnceLock<std::sync::Mutex<Vec<(String, String)>>> = OnceLock::new();
static ATEXIT_ONCE: Once = Once::new();

/// 登记新建的 test_<uuid> 库到进程全局回收列表。pub(crate)：仅 `fresh_database_url`
/// 内部调用。
fn register_db_for_drop(server_url: &str, db_name: &str) {
    ATEXIT_ONCE.call_once(|| unsafe {
        libc::atexit(drop_dbs_atexit);
    });
    CREATED_DBS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .expect("CREATED_DBS mutex poisoned")
        .push((server_url.to_string(), db_name.to_string()));
}

extern "C" fn drop_dbs_atexit() {
    let _ = std::panic::catch_unwind(|| {
        if std::env::var_os("TEST_KEEP_DB").is_some() {
            return;
        }
        let entries = std::mem::take(
            &mut *CREATED_DBS
                .get()
                .expect("atexit handler ran without registry init")
                .lock()
                .expect("CREATED_DBS mutex poisoned"),
        );
        if entries.is_empty() {
            return;
        }
        let mut by_url: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (url, db) in entries {
            by_url.entry(url).or_default().push(db);
        }
        // 2026-09-21 调试心得：复用常驻 `cleanup_runtime()` 在 atexit 阶段
        // `block_on` 仍 panic（推测 IO driver 与进程退出路径上的资源清理
        // 冲突），catch_unwind 也只能吞部分错误。改在独立 std::thread 内
        // 全新建一个 current_thread runtime，绕开所有 atexit 阶段的不确定
        // 状态。
        let handle = std::thread::Builder::new()
            .name("test-db-cleanup".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                rt.block_on(async move {
                    for (server_url, db_names) in by_url {
                        let admin = match PgPoolOptions::new()
                            .max_connections(1)
                            .acquire_timeout(std::time::Duration::from_secs(5))
                            .connect(&format!("{server_url}/postgres"))
                            .await
                        {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        for db_name in db_names {
                            let stmt = sqlx::query(sqlx::AssertSqlSafe(format!(
                                "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
                            )));
                            let _ = stmt.execute(&admin).await;
                        }
                        admin.close().await;
                    }
                });
            });
        if let Ok(h) = handle {
            let _ = h.join();
        }
    });
}

// 2026-09-21 设计心得（atexit 阶段踩坑）：main thread 上建 / 复用 tokio runtime
// 都会 panic —— IO driver 注册 epoll 与进程退出路径上的资源清理冲突，catch_unwind
// 也只能吞部分错误。唯一稳的路径是 atexit 阶段另起独立 std::thread，在新线程里建
// 全新的 current_thread runtime 跑 DROP，与进程退出路径完全隔离。具体实现见上面
// `drop_dbs_atexit`。

/// 2026-09-20 plan 2：fresh database URL —— 在 template 上 `CREATE DATABASE ... TEMPLATE`。
///
/// 容器由 scripts/test_runner.sh（或 scripts/test_nextest.sh session 级）提前
/// 起好，通过 `TEST_DATABASE_BASE_URL` 注入。plan 2 后 URL 形如：
/// `postgres://postgres:postgres@127.0.0.1:<port>/hsh_erp_template`
/// （带 /hsh_erp_template 后缀，由 session 启动时建好 template + 跑完 24 个 schema 迁移）。
///
/// 每测试走 `CREATE DATABASE test_<uuid> TEMPLATE hsh_erp_template` 文件级克隆，
/// tmpfs 下 ~100ms，远快于旧版 CREATE DATABASE + sqlx::migrate! (~600ms)。
///
/// admin pool `max_connections=1`：每次只跑一条 CREATE DATABASE；nextest 8 并行
/// 下 8×(1 admin + 10 test pool) ≈ 88 连接 < session 容器 max_connections=500 上限。
///
/// ## 兼容路径（test_runner.sh 单 binary 调用）
/// plan §Phase 1 step 8 要求 test_runner.sh 保持不变 → runner 只起容器不建 template。
/// 此时 fresh_database_url() 探测到 template 不存在，自动 fallback 到
/// plain `CREATE DATABASE` + test_pool() 跑 `sqlx::migrate!`。nextest session 路径
/// 走 TEMPLATE 克隆（plan §Phase 1 step 3），绕过此 fallback。
///
/// pub(crate)：仅 `test_pool` 内调用，外部 binary 走 `test_pool` 即可。
pub(crate) async fn fresh_database_url() -> (String, bool) {
    let raw_url = std::env::var("TEST_DATABASE_BASE_URL").expect(
        "TEST_DATABASE_BASE_URL 未设置 —— 经 `cargo test` 运行（runner 自动注入）；\
         或手动 export 指向 postgres-test：\
         postgres://hsh_test:6065161test@localhost:5429/hsh_erp_template",
    );
    // 2026-09-20 plan 2：URL 可能带 /<dbname> 后缀（指向 template），
    // admin pool 必须连 /postgres（内置管理库）而不是 /template/postgres。
    // 不能 split_once('/') —— postgres:// 中的 `//` 会把切到 scheme 后面（应取 LAST '/'，即 path 分隔）。
    let server_url = match raw_url.find("://") {
        Some(scheme_end) => match raw_url[scheme_end + 3..].find('/') {
            Some(path_idx) => raw_url[..scheme_end + 3 + path_idx].to_string(),
            None => raw_url,
        },
        None => raw_url,
    };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&format!("{server_url}/postgres"))
        .await
        .expect("connect ephemeral admin pool");
    // 探测 template 是否存在：nextest session 启动时建好，cargo test 单 binary 路径无。
    let template_exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = 'hsh_erp_template')",
    )
    .fetch_one(&admin)
    .await
    .unwrap_or(false);
    let db_name = format!("test_{}", uuid::Uuid::new_v4().simple());
    if template_exists {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{db_name}\" TEMPLATE hsh_erp_template"
        )))
        .execute(&admin)
        .await
        .expect("CREATE DATABASE TEMPLATE hsh_erp_template");
    } else {
        // 兼容路径（test_runner.sh 不建 template）—— fallback 到 plain CREATE DATABASE。
        // test_pool() 后续会跑 sqlx::migrate! 建 schema。
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{db_name}\""
        )))
        .execute(&admin)
        .await
        .expect("CREATE DATABASE on ephemeral admin pool");
    }
    // admin pool 在函数末尾 drop，PG 后端进程立即关闭。
    drop(admin);
    register_db_for_drop(&server_url, &db_name);
    (format!("{server_url}/{db_name}"), template_exists)
}

/// 测试 DB URL：仅作 `AppConfig.database_url` 字段占位。**实际连接走 caller 传入
/// 的 `PgPool`**，测试路径不经过 `infra/db.rs::create_pool`，所以本返回值是否
/// 「合法」无关紧要 —— 永远不会被任何代码 dial。
///
/// 2026-09-20：保留固定占位字符串。runner 起容器 + test_pool() 走 fresh_database_url()
/// 派生真 URL，AppConfig 这一字段值不再被任何代码实际读取。
///
/// pub(crate)：仅 [`state`](super::state) 构造 AppConfig 时使用。
pub(crate) fn test_database_url() -> String {
    "postgres://ephemeral/pending".to_string()
}

/// 2026-09-20 起为 no-op：runner 已起容器 + `fresh_database_url()` 每次 `test_pool()`
/// 都建独立 db，不需要预创建共享库。保留函数签名仅为不让 caller 报错。
pub async fn ensure_database_exists() {
    // no-op：容器由 runner 起，database 由 test_pool() 内部 fresh_database_url() 派生
}

/// 建测试连接池：从 runner 已起好的容器派生一个 fresh database。
///
/// 两层隔离（plan 2）：
/// - 容器：runner 进程级 1 个（同进程多测试共享容器，但 DB 独立）
/// - 数据库：每测试 fresh database（nextest session：TEMPLATE 克隆 ~100ms；
///   cargo test 单 binary 兼容路径：plain CREATE DATABASE + sqlx::migrate ~600ms）
/// - snowflake instance：per-process 派生（test_snowflake_instance）→ 跨进程不撞 ID
///
/// template_used=true（nextest session）：schema 已在 hsh_erp_template 跑过 24 个迁移，
/// 不再跑 migrate。template_used=false（test_runner.sh 单 binary 兼容路径）：
/// fresh database 是空的，必须跑 migrate 建 schema。
pub async fn test_pool() -> PgPool {
    // 1) fresh database URL —— 临时 admin pool 跑 CREATE DATABASE 后立即 drop。
    let (db_url, template_used) = fresh_database_url().await;

    // 2) Open 一个 PgPool 指向新 db（max_connections=10 足够测试）。
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&db_url)
        .await
        .expect("connect to fresh ephemeral database");

    // 3) 仅兼容路径跑迁移 + seed：nextest session 走 TEMPLATE 克隆，schema + 菜单
    //    baseline 已在 hsh_erp_template 跑好（见 scripts/test_nextest.sh），per-test
    //    DB 通过 CREATE DATABASE TEMPLATE 继承，无需重跑。
    //    2026-09-23 PR13 Phase A：`sqlx::migrate!` 是过程宏，路径相对调用方
    //    crate 的 CARGO_MANIFEST_DIR 解析。本 crate 在 `test-support/`，没有自己的
    //    `migrations/`，所以绕到主仓 `hsh_erp_rust::infra::db::run_migrations`：
    //    主仓的 `sqlx::migrate!("./migrations")` 在编译时把表结构烘进 crate，
    //    路径相对主仓 manifest dir（= 根仓）解析，命中主仓 `migrations/`。
    //
    //    2026-09-25 sqlx 接管新增：seed 同样走主仓 `hsh_erp_rust::infra::seed::run_seeds`，
    //    让单 binary fallback 路径（test_runner.sh）也能拿到菜单 baseline，与
    //    production 行为一致。
    if !template_used {
        hsh_erp_rust::infra::db::run_migrations(&pool)
            .await
            .expect("apply migrations on ephemeral test db");
        hsh_erp_rust::infra::seed::run_seeds(&pool, false)
            .await
            .expect("apply seeds on ephemeral test db");
    }

    pool
}
