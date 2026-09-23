//! outsource 域集成测试（PR13 Phase D 拆分）
//!
//! 3 个原 test binary（outsource_company_api / outsource_quote_api /
//! outsource_send_receive_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - company.rs      ← 原 outsource_company_api.rs
//! - quote.rs        ← 原 outsource_quote_api.rs
//! - send_receive.rs ← 原 outsource_send_receive_api.rs

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod company;
mod quote;
mod send_receive;