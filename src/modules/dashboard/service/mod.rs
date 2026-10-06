//! dashboard 域 service 子模块聚合
//!
//! 拆分依据（单文件职责 / 1000 行上限）：
//! - `snapshot` —— `build_snapshot` 装配大屏快照（KPI + 在加工清单 + 最紧急面板）
//! - `delivery` —— `build_upcoming_buckets` / `build_delivery_order_details`
//!   交期分桶与下钻抽屉
//!
//! `days` / `basis` 的缺省与 clamp 语义见 `service/delivery.rs` / `dto.rs::DeliveryBasis`。
//! VO 全在 `vo/`；SQL 行精简在 `repo/sql.rs`，交期 SQL 在 `repo/delivery.rs`。
//!
//! ## 事务分层
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑 + 装配，
//! 所有跨 repo 操作经 `repo: R`（by-value；`R: DashboardRepoTrait`）参数传入——
//! handler/service 借 `&mut *tx` / `&mut *conn` 喂给 `DashboardRepoTrait` trait。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! ## dashboard 域三个 HTTP 端点 + 一个 WS 端点
//! - `GET /api/v2/dashboard/snapshot` —— 大屏首帧（HTTP 全量首取）
//! - `GET /api/v2/dashboard/upcoming-delivery` —— 交期柱状图分桶
//! - `GET /api/v2/dashboard/delivery-orders` —— 柱状图下钻抽屉
//! - `GET /ws/dashboard`（WS）—— 握手首推 snapshot + 订阅 `WsEvent::DashboardEvent`

pub mod delivery;
pub mod snapshot;

// 把 DashboardService 类型 + 两个子模块定义的所有方法重新汇出，
// 让 handler.rs 仍走 `crate::modules::dashboard::service::DashboardService::*`
// 路径访问（路径稳定，零调用方修改）。
pub use delivery::{DASHBOARD_DEFAULT_DAYS, DASHBOARD_MAX_DAYS, DASHBOARD_MIN_DAYS};
pub use snapshot::DashboardService;
