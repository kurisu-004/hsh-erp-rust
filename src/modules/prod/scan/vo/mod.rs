//! prod::scan 出参 VO 汇总（handler 响应序列化层）
//!
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number`
//! 会丢精度，参见 `shared::types` 模块 doc）。
//!
//! ## 分组
//! - `worker.rs` —— `ScanWorkerBrief`（扫工牌 1 端点、4 字段）
//! - `listing.rs` —— `ScanListItem` / `ScanListOut` / `HeldListOut` / `ScanChainState`
//!   （两条只读聚合端点的分页行；`HeldListOut` 是 `/held` 专用的「+ 容量两字段」信封）
//! - `locate.rs` —— `LocateOut` / `LocatePartBrief` / `LocateBatchItem` /
//!   `LocateProcessBriefOut` / `LocateAction` / `LocateDisabledReason`（2026-10-15
//!   新增的诊断端点 `GET /scan/locate`；`chain_state` 复用 `listing.rs` 的
//!   `ScanChainState`；`LocateProcessBriefOut` 是 2026-10-16 起 `LocateOut` 新增的
//!   **工人维度**目标工序 picker 行）
//! - `transition.rs` —— `WorkerScanCoreOut` / `WorkerScanOut` / `WorkerScanSplitInfo`
//!   （放回 / 送检）+ `ScanBannerItem`（2026-10-13 新增的 `SCAN_BANNER` 事件 payload，
//!   业务事件 banner 的专属展示 VO）
//! - `take_over.rs` —— `TakeOverOut`（2026-10-15 新增的第 7 条端点
//!   `POST /scan/take-over` 扫码接管；`synced_assembly_id` 是内部管道字段，不进 JSON）
//! - `outsource_receive.rs` —— `OutsourceReceiveOut`（2026-10-15 新增的第 8 条端点
//!   `POST /scan/outsource-receive` 外协确认收回；`picked_quantity` 是内部管道字段，
//!   不进 JSON）
//!
//! pick-up 端点的出参**不在本模块**：它的 HTTP 响应体是 `R<PartOut>`（10 字段的
//! part 级最小投影，与 to-XXX 三流共用），本次迁移只改 URL 与 handler / service
//! 的归属，响应形状逐字不变；WS 广播用的 `PickUpOutcome` 是纯内部结构，放在
//! `service/pickup.rs`。

pub mod listing;
pub mod locate;
pub mod outsource_receive;
pub mod take_over;
pub mod transition;
pub mod worker;

pub use listing::{HeldListOut, ScanChainState, ScanListItem, ScanListOut};
pub use locate::{
    LocateAction, LocateBatchItem, LocateDisabledReason, LocateOut, LocatePartBrief,
    LocateProcessBriefOut,
};
pub use outsource_receive::OutsourceReceiveOut;
pub use take_over::TakeOverOut;
pub use transition::{ScanBannerItem, WorkerScanCoreOut, WorkerScanOut, WorkerScanSplitInfo};
pub use worker::ScanWorkerBrief;
