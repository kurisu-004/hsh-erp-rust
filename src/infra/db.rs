//! sqlx PgPool 构建
//!
//! 连接池参数对齐 Python myERP/core/database.py：pool_size=10、max_overflow=20、pre_ping=true、recycle=3600s。

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::infra::config::AppConfig;

pub async fn create_pool(cfg: &AppConfig) -> sqlx::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(30) // 10 + 20 overflow
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Some(Duration::from_secs(600)))
        .max_lifetime(Some(Duration::from_secs(3600)))
        .connect(&cfg.database_url)
        .await
}