//! 2026-09-22 新增：SQLite POC 测试模块入口。
//!
//! **POC 目的**：验证 oneshot + SQLite + nextest 三件套能否让本地集成测试
//! 摆脱 PG 容器。独立 sandbox，不引入 axum、不动 src/、不动 migrations/、不动
//! tests/common/mod.rs 现有 PG 基建。
//!
//! 触发方式（绕开 .cargo/config.toml runner 的 PG 容器启动）：
//!   ```bash
//!   TEST_DATABASE_BASE_URL=sqlite-poc-skip \
//!       cargo nextest run -p hsh-erp-rust -E 'test(/sqlite_poc/)'
//!   ```
//! TEST_DATABASE_BASE_URL 非空触发 scripts/test_runner.sh 转义口 1 直接 exec，
//! 不起 PG 容器。
//!
//! 模块组成：
//!   * [`schema`] —— `schema.sql` 字符串（通过 include_str! 嵌入）。
//!   * [`repo`] —— 3 个静态 async fn（get_by_id / create / touch_login），对齐
//!     `src/modules/iam/repo/sql.rs` PG 版同名方法语义，但**不**实现 IamRepo trait。
//!   * [`oneshot_demo`] —— 用 tokio::sync::oneshot 演示 spawn → ready → 断言 → shutdown
//!     单进程生命周期。

pub mod oneshot_demo;
pub mod repo;
