//! deadpool-redis 连接池构建
//!
//! 与 `src/infra/db.rs::create_pool` 同形态。`deadpool_redis::Config::builder()` 返回
//! `PoolBuilder`，按需 `.max_size(...)` / `.runtime(...)` 后 `.build()`。

use anyhow::{anyhow, Result};
use deadpool_redis::{Config as RedisPoolConfig, Pool, Runtime};

use crate::infra::config::AppConfig;

pub fn create_pool(cfg: &AppConfig) -> Result<Pool> {
    let redis_url = &cfg.redis.url;
    let max_size = cfg.redis.pool_max_size;
    let redis_cfg = RedisPoolConfig::from_url(redis_url);
    redis_cfg
        .builder()
        .map_err(|e| anyhow!("创建 Redis 连接池配置失败: {e}"))?
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| anyhow!("创建 Redis 连接池失败: {e}"))
}