//! iam 域集成测试（PR13 Phase D 拆分）
//!
//! 2 个原 test binary（iam_api / auth_middleware）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - api.rs       ← 原 iam_api.rs
//! - middleware.rs ← 原 auth_middleware.rs

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod api;
mod middleware;