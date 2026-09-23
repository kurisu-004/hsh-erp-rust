//! 集成测试共享 facade —— 重导出到 hsh-erp-test-support crate
//!
//! 2026-09-23 重构（PR13 Phase A）：tests/common 内容迁到 test-support crate。
//! 本文件保留仅为保持现有 51 个 test binary 不需改 import；后续 Phase B/C/D
//! 拆 domain 时统一切到直接 `use hsh_erp_test_support::*;`。
//!
//! ## facade 模式
//! - `pub use hsh_erp_test_support::*;` → 51 个 binary 现有的
//!   `use common::{test_pool, test_state, ...}` 全部命中
//! - `pub mod pem { ... }` → 转 `use common::pem::test_private_pem` 的老路径
//!   （tests/auth_middleware.rs 唯一一处 `use common::pem;` 走这条）
//!
//! 实现原理：每个 integration test binary 通过
//! `#[path = "common/mod.rs"] mod common;` 加载本文件，本文件 `pub use` 把
//! `hsh_erp_test_support` 的所有公开项转发到 `common::*`。编译期无重复代码，
//! 链接期同 crate 单实例（OnceLock 缓存的 pem keypair 跨 binary 各自独立——
//! 与原 tests/common/mod.rs 行为一致，因为每个 binary 是独立进程）。
//!
//! 详细 crate 设计见 `test-support/src/lib.rs`。

#![allow(
    dead_code,
    clippy::duplicate_mod,
    clippy::await_holding_lock,
    // facade 模式：每个 binary 通过 `use common::{X, Y, ...}` 只取若干项；
    // `pub use hsh_erp_test_support::*` 必然「导出但未在本 binary 全用上」，
    // 这是 facade 设计的固有 trade-off，统一下推到 crate root 抑制告警。
    unused_imports
)]

pub use hsh_erp_test_support::*;

/// `pub mod pem` 内联转发：原 `tests/common/pem.rs` 已迁到
/// `hsh_erp_test_support::pem`（独立子模块），`tests/common/pem.rs` 文件已删。
/// tests/auth_middleware.rs 仍 `use common::pem;`，本行保证它继续命中同一份
/// OnceLock 缓存的 RSA 密钥对（与 tests/state 的 test_state 系列 helper 共用）。
pub mod pem {
    #![allow(unused_imports)]
    pub use hsh_erp_test_support::pem::*;
}
