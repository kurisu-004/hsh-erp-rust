//! Redis 测试池 + URL 定位 + FLUSHDB
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `test_redis_pool`（建 redis 连接池）
//! - `test_redis_url`（定位到测试实例 + **一个固定 db**）
//! - `test_key_prefix`（进程级 key 前缀，隔离的实际承担者）
//! - `clean_redis`（FLUSHDB；当前无调用方，见其 doc）
//!
//! ## 2026-10-09：隔离由 key 前缀承担，`test_redis_url()` 只定位到实例
//!
//! **原文（已作废）**：本文件曾按测试 binary 名派生 db index（FNV-1a hash mod 13 +
//! `_e2e_api`/`auth_middleware`/`iam_api`/`idempotency_api` 四条固定独占分支）来隔离
//! binary。该方案有三处硬伤，已整体删除：
//! - 21 个测试 binary 只落到 **10 个** db（FNV-1a 必然碰撞）；
//! - `auth_middleware` / `iam_api` 两条分支是**死分支**——这两个 binary 已于
//!   2026-09-23 PR13 合并为 `tests/iam/`（binary 名现在是 `iam`）；
//! - 根本困难：Redis 默认只有 16 个 db（容器没配 `--databases`），而用 Redis 的测试
//!   binary 有 17 个，**任何分桶方案都堵不住**（且 db 16 会越界）。
//!
//! **现行设计**：隔离改由 `RedisConfig::key_prefix` 承担。测试进程填 `t{pid}:`，
//! Redis 里两个进程写的物理 key 物理不相交（`t123:sessions:user:456` vs
//! `t456:sessions:user:456`），从而**不需要**任何分桶；`test_redis_url()` 退化为
//! 「定位到测试实例 + 固定 db 0」。
//!
//! ## 其它仍成立的事实
//! - 默认连 `redis-test` 容器（端口 6380）
//! - 同进程内多线程共享同一前缀空间 → 前缀**不**替代逐测试清理，session 相关断言
//!   仍需自行保证 key 不串（各测试用 UUID / unique key 生成 jti 与 client_key）
//! - 可由 `TEST_REDIS_URL` 环境变量整体覆盖（跨 worktree 隔离用）

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::{Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime};

/// 测试用 Redis URL：默认连 `redis-test` 容器（端口 6380）+ **固定 db 0**。
///
/// 2026-10-09：db index 派生（FNV-1a hash + 4 条固定独占分支）已删除，隔离改由
/// [`test_key_prefix`] 按进程承担（详见模块 doc）。可由 `TEST_REDIS_URL` 环境变量
/// 整体覆盖（跨 worktree 隔离用）。
pub fn test_redis_url() -> String {
    if let Ok(url) = std::env::var("TEST_REDIS_URL") {
        return url;
    }
    "redis://localhost:6380/0".to_string()
}

/// 2026-10-09 新增：测试侧 Redis key 统一前缀，**按进程**取值。
///
/// 形如 `t{pid}:`，作为 `RedisConfig::key_prefix` 填进 `RedisSessionStore` /
/// `RedisIdempotencyStore`。作用：两个并行测试进程即便 mint 出同一个雪花 ID
/// （`sessions:user:{user_id}` / `idem:{client_key}` 的下标），落到 Redis 的物理 key
/// 仍不相交 ⇒ 一方 `delete_all_user_sessions` 不会连坐吊销另一方的 session
/// （`40105 SESSION_REVOKED` flake 的唯一来源）。
///
/// 生产缺省为空串（key 与历史逐字节一致），故本函数只在 test-support 里有引用者。
pub fn test_key_prefix() -> String {
    format!("t{}:", std::process::id())
}

/// 建测试用 Redis 连接池（2026-10-09 起固定 db 0，隔离由 `test_key_prefix` 承担）。
#[allow(dead_code)]
pub async fn test_redis_pool() -> RedisPool {
    let cfg = RedisConfig::from_url(test_redis_url());
    cfg.create_pool(Some(RedisRuntime::Tokio1))
        .expect("create test redis pool — 确认 redis-test 容器在 6380")
}

/// 清空测试 Redis db（FLUSHDB）。
///
/// ⚠️ 2026-10-09：**FLUSHDB 不认识 key 前缀** —— 所有测试进程共用 db 0，故本函数
/// 会连带清掉其它并行进程的整个前缀空间，key 前缀隔离在此**不生效**（这正是原
/// 「按 binary 分配独占 db」方案想解决的问题）。因此本函数当前**零调用方**
/// （仅保留导出）；任何未来调用都必须先把该 binary 挪回 nextest 的 `redis-flush`
/// 串行组，或改成按前缀 `SCAN` + `DEL` 的自清理。
#[allow(dead_code)]
pub async fn clean_redis(pool: &RedisPool) {
    let mut conn = pool.get().await.expect("get redis conn from test pool");
    let _: () = AsyncCommands::flushdb::<()>(&mut conn)
        .await
        .expect("flushdb test redis");
}