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
//! - pool.rs       — 2026-10-03 新增外协看板读端点（原 `/outsource-pool/*` 三件套
//!   counts / {process_id} / state）；2026-10-09 改打 `/outsource-queue/*` 两条新路径
//!   （`snapshot` / `processes/{id}`，在途批次已内联进公司列），文件名沿用不变
//!   （改文件名要同步本文件的 `mod` 声明与 nextest filter，收益仅为命名一致性）
//!
//! 2026-10-09 随写端点三合一删除 `sendable.rs`（`GET /outsource-sendable` 已下线，
//! 它的行是看板候选列的分页子集；候选侧的等价覆盖见 `pool.rs` 的候选列用例）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod company;
mod pool;
mod quotable;
mod quote;
mod send_receive;
mod shipment;
