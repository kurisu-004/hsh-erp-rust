//! sqlx PgPool 构建
//!
//! 连接池参数对齐 Python myERP/core/database.py：pool_size=10、max_overflow=20、pre_ping=true、recycle=3600s。

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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

/// 2026-09-23 PR13 Phase A 新增：在主仓跑 `sqlx::migrate!("./migrations")`。
///
/// ## 设计动机
/// `sqlx::migrate!` 是过程宏，路径相对 `CARGO_MANIFEST_DIR`（= 当前 crate 的
/// `Cargo.toml` 所在目录）解析。`test-support/` crate 若直接在自己的 `pool.rs`
/// 写 `sqlx::migrate!("./migrations")`，会查 `test-support/migrations/`（不存在）。
///
/// 解决：把迁移加载统一收敛到主仓（本函数），test-support crate 调用
/// `hsh_erp_rust::infra::db::run_migrations(&pool).await`，路径解析自然走主仓
/// manifest dir，与 `src/main.rs` 启动时迁移行为完全一致。
///
/// ## 调用方
/// - `src/main.rs`：启动时自动跑
/// - `test-support/src/pool.rs::test_pool()`：cargo test 单 binary 兼容路径
///   （template 不存在 → fresh database 是空 → 必须跑 migrate 建 schema）
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
