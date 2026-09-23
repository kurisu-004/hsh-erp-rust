//! # hsh-erp-test-support —— 集成测试共享基建
//!
//! 2026-09-23 新增（PR13 Phase A）：从主仓 `tests/common/mod.rs` (1393 行) +
//! `tests/common/pem.rs` (209 行) 抽到独立 dev-only crate。
//!
//! ## 设计动机
//! - `tests/common/mod.rs` 单文件已 1393 行（2026-09-23 实测），主仓 `tests/`
//!   一级目录不便再分层（51 个 binary 通过 `#[path = "common/mod.rs"] mod common;`
//!   引用）；独立 crate 提供物理边界。
//! - 后续 PR13 Phase B/C/D 按 domain 拆 helper 时，本 crate 内继续切子模块
//!   （fixture / state / router 等已有雏形），不动主仓 51 个 binary 的
//!   `use common::{...}` 入口（`tests/common/mod.rs` 保留为 facade）。
//!
//! ## 子模块切分
//! - [`pool`]：DB 生命周期 + snowflake ID 生成（`test_pool` /
//!   `fresh_database_url` / `pool_snowflake` / `register_db_for_drop` 等）
//! - [`pem`]：进程级缓存的 RSA 密钥对（`test_private_pem` /
//!   `test_public_pem` / `test_public_kids`）
//! - [`redis`]：redis 连接池 + clean_redis + URL 派生（`test_redis_pool` /
//!   `clean_redis` / `test_redis_url`）
//! - [`state`]：构造测试 `AppState` 的 helper（`test_state` /
//!   `test_state_with_cos` / `test_state_with_disabled_session` / `test_app`
//!   / `test_ws_app`）
//! - [`fixtures`]：建最小化世界（admin/MANAGER/货架/菜单/工艺链）的 helper
//!   + `MockCos` stub（`insert_user_with_password` / `add_role` /
//!   `clean_db` / `MockCos` 等）
//!
//! ## 反向依赖关系
//! 本 crate 的 `[dependencies]` 声明 `hsh-erp-rust = { path = ".." }`，
//! cargo 允许集成测试基建 crate 反向依赖主二进制（无循环构建）。编译顺序：
//! 1. `hsh-erp-rust`（主二进制）
//! 2. `hsh-erp-test-support`（依赖主二进制，但仅编译一次，后续 cached）
//! 3. integration tests（dev-deps 拉入本 crate）
//!
//! ## 两层隔离模型（沿用原 mod.rs 设计，未变）
//! - **Layer 1**：容器生命周期由 `.cargo/config.toml` 的 `runner =
//!   "scripts/test_runner.sh"` 接管；nextest 模式由 `scripts/test_nextest.sh`
//!   起 session 级容器。
//! - **Layer 2**：每测试 `fresh_database_url()` 在 template 上
//!   `CREATE DATABASE test_<uuid> TEMPLATE hsh_erp_template`（~100ms tmpfs 克隆）。
//! - **Layer 2.5**：进程退出回收（libc::atexit + admin DROP）。
//! - **Snowflake 隔离**：per-process instance 由 `pid ⊕ startup_nanos` 派生。
//!
//! ## 公开入口（crate root re-export）
//! `pub use pool::*; pub use pem::*; pub use redis::*; pub use fixtures::*;
//! pub use state::*;` —— 让上层 `use hsh_erp_test_support::{test_pool, ...};`
//! 直接拿到全部 helper，无需逐个 `pub use`。
//!
//! ## 主仓集成测试 facade
//! `tests/common/mod.rs` 保留为单层 re-export：
//! ```ignore
//! pub use hsh_erp_test_support::*;
//! pub mod pem { pub use hsh_erp_test_support::pem::*; }
//! ```
//! 51 个 test binary 不需改 import。后续 Phase 按 domain 切到直接
//! `use hsh_erp_test_support::*;` 时再清 facade。

// 与原 tests/common/mod.rs 同款属性：51 个 binary 共用 helper，未引用项触发
// `dead_code` warning 噪音；`duplicate_mod` 因为多 binary 用 `#[path]` 共用
// mod.rs；`await_holding_lock` 因为 fixture 模式锁+await+INSERT（具体见
// pool.rs / fixtures.rs 注释）。
#![allow(dead_code, clippy::duplicate_mod, clippy::await_holding_lock)]

pub mod fixtures;
pub mod pem;
pub mod pool;
pub mod redis;
pub mod state;

pub use fixtures::*;
pub use pem::*;
pub use pool::*;
pub use redis::*;
pub use state::*;
