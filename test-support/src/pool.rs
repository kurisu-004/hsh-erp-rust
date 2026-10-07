//! DB 生命周期 + snowflake ID 生成
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `test_pool` + `fresh_database_url`（每测试 fresh database）
//! - `register_db_for_drop` + `drop_dbs_atexit`（进程退出回收）
//! - `shared_test_snowflake` + `pool_snowflake` + `test_snowflake_instance`
//!   （进程内唯一 ID 源 / 存量调用点的兼容薄壳 / 跨进程 instance 派生）
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
//! - 2026-10-09 新增：`shared_test_snowflake()` 是**全进程唯一的测试 ID 源**
//!   （`AppState.snowflake` 与所有 fixture helper 共用同一个 generator 对象）；
//!   `pool_snowflake()` 退化为给存量调用点的**兼容薄壳** —— 返回同一把锁、只在内层多包
//!   一层 `Arc`，`.lock()?.next_id()` 靠 `Deref` 照旧可用，`tests/` 里 183 处调用点
//!   一行未改。
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
///
/// 2026-10-09 新增：内层由 `SnowflakeIdGenerator` 改为 `Arc<SnowflakeIdGenerator>`。
/// 根因见 [`shared_test_snowflake`] 的 doc；包 `Arc` 是因为 `SnowflakeIdGenerator`
/// 内部是 `Mutex<Inner>` 且**没有** `Clone` 实现，不包 `Arc` 就没法把同一个对象同时
/// 交给 `pool_snowflake()` 的互斥访问与多个 `AppState`。
static TEST_SNOWFLAKE_GEN: OnceLock<std::sync::Mutex<std::sync::Arc<SnowflakeIdGenerator>>> =
    OnceLock::new();

/// 全局 snowflake 生成器访问器 —— **兼容薄壳**，唯一真源是 [`shared_test_snowflake`]。
///
/// 公开暴露：原 `tests/common/mod.rs` 写的是 `pub fn`，部分 integration test
/// binary（worker_pool_api 等）通过 `common::pool_snowflake().lock().unwrap().next_id()`
/// 直接拿 ID 串联多 fixture —— 保持公开语义以不破坏现有 51 个 binary 的 import。
/// [`fixtures`](super::fixtures) 的 INSERT helper 也走它。
///
/// 2026-10-09：返回类型由 `&'static Mutex<SnowflakeIdGenerator>` 改为
/// `&'static Mutex<Arc<SnowflakeIdGenerator>>`（理由见 [`shared_test_snowflake`]）。
/// 存量 `.lock().expect(..).next_id()` / `.lock().unwrap().next_id()` / 绑定 guard 后
/// 再 `next_id()` 的写法全部靠 `Arc: Deref<Target = SnowflakeIdGenerator>` 透明兼容，
/// `tests/` 里 129 处调用点一行未改。
/// ⚠️ **2026-10-09（review 第 1 轮 M5）订正**：初版此处写「183 处调用点」—— 183 是当时
/// CLAUDE.md 待办登记表里 `SnowflakeIdGenerator::new` 的**处数**（182 + 1），被误当成
/// 本函数的调用点数。实测（`rg -o 'pool_snowflake\(\)' tests/ | wc -l`）为 **129**。
pub fn pool_snowflake() -> &'static std::sync::Mutex<std::sync::Arc<SnowflakeIdGenerator>> {
    TEST_SNOWFLAKE_GEN.get_or_init(|| {
        std::sync::Mutex::new(std::sync::Arc::new(SnowflakeIdGenerator::new(
            1_577_836_800_000,
            test_snowflake_instance(),
        )))
    })
}

/// 供 [`shared_test_snowflake`] 交出 `&'static Arc<_>` 用的旁挂 static。
///
/// 实现说明：generator 只在 `TEST_SNOWFLAKE_GEN` 里构造一次，本 static 只是把**同一个
/// `Arc`** 另存一份 —— `MutexGuard` 是临时值，从 `&'static Mutex<_>` 解引用出来的引用
/// 无法逃出函数（E0515）。两个 static 存的永远是同一个 `Arc`，不存在分叉风险。
static SHARED_TEST_SNOWFLAKE: OnceLock<std::sync::Arc<SnowflakeIdGenerator>> = OnceLock::new();

/// 全进程唯一的测试雪花 ID 源（2026-10-09 新增）。
///
/// ## 为什么要有它（根因）
/// 位布局是 `ts << 22 | instance << 12 | seq`，其中 `last_ms` / `sequence` 属于
/// `SnowflakeIdGenerator` 的**实例私有**字段，而 `SnowflakeIdGenerator::new()` 一律从
/// `last_ms=0, sequence=0` 起步。于是**任意两个 instance 相同、但对象不同的
/// generator**，只要在同一毫秒各自取到第 j 个号，就发出**逐字节相同**的 id，撞
/// `t_*_pkey` 报 `23505` —— 与「同一个 helper 调几次」无关，最典型的形态恰恰是
/// **两个不同 helper 各调一次**。
///
/// 改造前本仓已有两处共享 generator，但它们**是彼此独立的对象、却共用同一个
/// instance**（都是 `test_snowflake_instance()`），所以照样撞：
/// - [`pool_snowflake`] 发的 fixture id；
/// - [`state`](super::state) 三个 `test_state*` 构造函数各建一个 generator，
///   `AppState.snowflake` 发的业务 id（同一测试里建两个 AppState 即自撞）。
///
/// 所以「进程内唯一」的正确保证点是**对象共享**，而不是 instance 编号 —— instance 只有
/// 10 bit = 1024 槽，本就该留给跨进程（见 `test_snowflake_instance()`）。本函数返回
/// `&'static Arc<..>`，调用方 `.clone()` 拿到的就是同一个对象。
pub fn shared_test_snowflake() -> &'static std::sync::Arc<SnowflakeIdGenerator> {
    SHARED_TEST_SNOWFLAKE.get_or_init(|| {
        pool_snowflake()
            .lock()
            .expect("TEST_SNOWFLAKE_GEN mutex poisoned")
            .clone()
    })
}

/// per-process snowflake instance：pid ⊕ 启动时间纳秒低位 → 0-1023。
///
/// 2026-09-20 新增：nextest process-per-test 模型下，同毫秒并行的多个测试进程若
/// 共享 (epoch, instance=1) 会生成相同 user_id → 撞 redis key（sessions:user:{id}）。
/// pid 与 startup_nanos 的低位异或后 mod 1024 即可在 1024 个并行进程内几乎无碰撞；
/// 实现零依赖（不引入 fnv crate）。
///
/// 2026-09-28 备注：相关 STS 会话域 redis key 已下线；本函数仍服务于 `sessions:user:{id}`
/// 防撞；instance 派生逻辑未变。
///
/// **2026-10-09 职责收窄**：instance 现在**只负责跨进程**这 10 bit（1024 槽）的区分，
/// **不再承担进程内唯一性** —— 后者已由「全进程共享同一个 generator 对象」
/// （[`shared_test_snowflake`]）保证。根因：位布局里 `last_ms` / `sequence` 是 generator
/// 的实例私有字段，`new()` 又从 `last_ms=0, sequence=0` 起步，故两个 instance 相同的
/// 独立 generator 在同一毫秒各自取第 j 个号会发出逐字节相同的 id。
///
/// **由此产生的新约定 —— 进程内只从 [`shared_test_snowflake`] 取号**（或其兼容薄壳
/// [`pool_snowflake`]）。测试代码（`tests/**` 与本 crate）**不应**再 `SnowflakeIdGenerator::new`
/// 另起一条 id 流。
///
/// ## 冲突条件（review 第 1 轮 Q1 订正，勿再简化为「必撞」）
/// 位布局 `ts << 22 | instance << 12 | seq`，故两个独立 generator 的 id 相同，当且仅当
/// **同 instance + 同毫秒 + 同 seq** 三条同时成立。由此：
/// - **instance 不同 ⇒ 位段不同 ⇒ 必然不撞**。刻意给另一个 generator 填不同 instance
///   曾是**有效**的权宜之计 —— 2026-10-09 本轮改造前本仓确有多处在用
///   （`tests/part/purchase_order_import.rs` 的 `777`、`tests/part/lifecycle.rs` 的
///   `11`/`12`/`13`、`tests/part/crud.rs` 的 `99` 等）。
///   ⚠️ **2026-10-09（review 第 1 轮 M5）订正：那些写法现已全部迁走**（全仓真实代码行
///   的 `SnowflakeIdGenerator::new` 只剩两个进程级唯一源 + 生产 1 处 + 被测对象自身 8 处），
///   本条已从「本仓多处正在用」降级为**历史陈述**。「换 instance 有效」这个知识本身仍然
///   成立，但它不再是本仓任何一处的做法。
/// - 但 instance 只有 10 bit = **1024 个取值**，刻意填的字面量（如 `1`）仍有
///   **1/1024 概率**等于本进程 `test_snowflake_instance()` 的派生值 —— 而两个 fresh
///   generator 的首个 id 恰好都是 `seq=0`，故一旦相等就同毫秒同 seq 相撞 `t_*_pkey`
///   （23505）。
/// - 结论：**「刻意换 instance」只是次优的权宜之计，唯一无歧义的规矩是「进程内只从
///   [`shared_test_snowflake`] 取号」**。历史真实事故形态正是两个**同为 instance=1** 的
///   独立 generator（`tests/part/crud.rs` 与 `tests/part/rollup_recompute.rs` 的域内单例，
///   同一 `part` binary 内）——已由 2026-10-09 本轮改造收敛掉，并由主仓
///   `src/shared/snowflake_guard.rs`（`cargo test --lib` 强制）钉死「不得再就地 new」。
///   登记与统计口径见 CLAUDE.md「测试取号：进程内唯一 generator」一节（原「待办登记：
///   测试内联造 snowflake 生成器应收敛到 `pool_snowflake()`」，已随本轮改造重写）。
///
/// pub(crate)：[`state`](super::state) 构造 `AppConfig::snowflake::instance` 也读它，
/// 必须 crate 内可见。
///
/// ⚠️ **2026-10-09（review 第 1 轮 I2）：本函数与主仓 `src/shared/test_snowflake.rs` 的
/// `test_snowflake_instance` 是逐字相同的两份，改一必须同步另一份。**
/// 跨 crate **无编译期保障**（两份代码互不可见），漂移不会让任何测试变红，只会静默
/// 削弱跨进程撞号保护。两份并存的原因是 dev-dependency 环：本 crate 的
/// `[dependencies]` 指回主仓，而主仓的 `[dev-dependencies]` 指回本 crate，于是
/// `cargo test --lib` 的同一个二进制里链进**两份** `hsh_erp_rust` —— 本函数所在的那份
/// 永远是「集成测试 binary」的那份，`src/**` 的 lib 单测走的是主仓自己那份
/// （`src/shared/test_snowflake.rs`），两者类型不通、对象不同源。
/// 收敛手段只能是文档约定 + 双向日期戳（现状：两边已互链）。
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
