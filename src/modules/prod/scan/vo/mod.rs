//! prod::scan 出参 VO 汇总（handler 响应序列化层）
//!
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number`
//! 会丢精度，参见 `shared::types` 模块 doc）。
//!
//! ## 分组
//! - `worker.rs` —— `ScanWorkerBrief`（扫工牌 1 端点、4 字段）
//! - `listing.rs` —— `ScanListItem` / `ScanListOut` / `ScanChainState`
//!   （两条只读聚合端点的分页行）
//! - `transition.rs` —— `WorkerScanCoreOut` / `WorkerScanOut` / `WorkerScanSplitInfo`
//!   （放回 / 送检）
//!
//! pick-up 端点的出参**不在本模块**：它的 HTTP 响应体是 `R<PartOut>`（10 字段的
//! part 级最小投影，与 to-XXX 三流共用），本次迁移只改 URL 与 handler / service
//! 的归属，响应形状逐字不变；WS 广播用的 `PickUpOutcome` 是纯内部结构，放在
//! `service/pickup.rs`。

pub mod listing;
pub mod transition;
pub mod worker;

pub use listing::{ScanChainState, ScanListItem, ScanListOut};
pub use transition::{WorkerScanCoreOut, WorkerScanOut, WorkerScanSplitInfo};
pub use worker::ScanWorkerBrief;
