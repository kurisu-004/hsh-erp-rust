//! outsource 域集成测试（PR13 Phase D 拆分）
//!
//! 3 个原 test binary（outsource_company_api / outsource_quote_api /
//! outsource_send_receive_api）合并为 1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射
//! - company.rs    ← 原 outsource_company_api.rs
//! - quote.rs      ← 原 outsource_quote_api.rs
//! - send_receive.rs ← 原 outsource_send_receive_api.rs
//! - shipment.rs   — 2026-10-03 新增：对账页 sent-parts + 在途 in-flight
//! - quotable.rs   — 2026-10-03 新增：报价 picker
//! - sendable.rs   — 2026-10-03 新增：可发送外协一览（APPROVAL / DIRECT）
//! - pool.rs       — 2026-10-03 新增：`/outsource-pool/*` 看板三件套
//!   （counts / {process_id} / state；形态照抄 `/prod/pool`）

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod company;
mod pool;
mod quotable;
mod quote;
mod send_receive;
mod sendable;
mod shipment;
