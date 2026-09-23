//! Redis 测试池 + URL 派生 + FLUSHDB
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `test_redis_pool`（建 redis 连接池）
//! - `test_redis_url`（按 binary 名派生 URL，固定 db 隔离）
//! - `clean_redis`（FLUSHDB；保证每个测试 session 隔离）
//!
//! ## 设计要点（沿用原 mod.rs 实现）
//! - 默认连 `redis-test` 容器（端口 6380）
//! - 跨 binary 隔离：`_e2e_api` / `auth_middleware` / `iam_api` / `idempotency_api`
//!   分配**固定独占** db（13/14/15/12），其它 binary 走 FNV-1a hash mod 13 派生
//! - 同 binary 内多线程仍共享 db → clean_redis() 的 FLUSHDB 必须在每个需要
//!   session 的测试前调
//! - 可由 `TEST_REDIS_URL` 环境变量整体覆盖（跨 worktree 隔离用）

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::{Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime};

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
        // 2026-09-23 新增 Idempotency 中间件集成测试 binary：独占 db 12，
        // 与其它 3 个 FLUSHDB binary 隔离（理由同 race #2 修复）。Redis 默认
        // 16 个 db（0-15），db 16 会越界。
        "idempotency_api" => 12,
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

/// 建测试用 Redis 连接池（按 binary 派生 db index，与 dev 默认 db 0 隔离）。
#[allow(dead_code)]
pub async fn test_redis_pool() -> RedisPool {
    let cfg = RedisConfig::from_url(test_redis_url());
    cfg.create_pool(Some(RedisRuntime::Tokio1))
        .expect("create test redis pool — 确认 redis-test 容器在 6380")
}

/// 清空测试 Redis db（FLUSHDB；与 `clean_db` 配套保证 DB + Redis 状态都干净）。
///
/// 仅部分集成测试（如 iam_api）需要；其它测试不引用本函数 —— 故 `dead_code` 抑制。
#[allow(dead_code)]
pub async fn clean_redis(pool: &RedisPool) {
    let mut conn = pool.get().await.expect("get redis conn from test pool");
    let _: () = AsyncCommands::flushdb::<()>(&mut conn)
        .await
        .expect("flushdb test redis");
}
