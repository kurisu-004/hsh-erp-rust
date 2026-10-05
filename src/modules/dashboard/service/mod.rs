//! dashboard 域 service 子模块聚合
//!
//! 拆分依据（Group E 重构 + 单文件职责 / 1000 行上限）：把单文件 `service.rs`
//! 拆为
//! - `snapshot` —— `build_snapshot_with_workers` 装配大屏完整快照（含工人持有的
//!   PICKED_UP 时间戳、产线架每架 top-10 截流、worker 名称查表、未来 N 天交付分桶）
//!
//! 形参 `days` / `basis` 语义见 `service/snapshot.rs::build_snapshot_with_workers` / `dto.rs::DeliveryBasis`。
//!
//! `DashboardSnapshot` / `OnProductionShelfGroup` / `DashboardItem` /
//! `UpcomingDeliveryBucket` 等数据结构在 `vo/snapshot.rs`（2026-09-22 从 service.rs 平移），
//! `BatchLite` / `PartLite` 等 SQL 行精简在 `repo/sql.rs`。
//!
//! ## 调用方契约
//! `handler.rs` 仅引 `crate::modules::dashboard::service::DashboardService::*`，
//! 不直接访问 `snapshot` 子模块。本模块用 `pub use snapshot::*` 把 `DashboardService`
//! 类型 + 方法重新汇出到 `service` 命名空间。Rust 的 inherent 方法按类型名寻址，
//! 不依赖定义所在文件。
//!
//! ## 事务分层（2026-09-22 Group E 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑 + 装配，
//! 所有跨 repo 操作经 `repo: R`（by-value；`R: DashboardRepoTrait`）参数传入——
//! handler/service 借 `&mut *tx` / `&mut *conn` 喂给 `DashboardRepoTrait` trait
//! （trait 已直接 `impl for &mut PgConnection`，2026-09-22 同 iam 范式）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! `DashboardService` 是 unit struct（无字段依赖，iam 范本 §6）；方法签名
//! `<R: DashboardRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。
//!
//! ## dashboard 域两个端点
//! - `GET /ws/dashboard`（WS，2026-09-15 takeover-fill）—— 握手首推 snapshot +
//!   订阅 `WsEvent::DashboardEvent` 增量 + 30s text 心跳 + 周期 re-auth
//! - `GET /api/v2/dashboard/snapshot`（HTTP，2026-09-28 新增）—— HTTP 全量首取
//!   大屏快照，与 WS 端点共用 service（同一 service、同 SQL；不引入新 repo 调用）

pub mod snapshot;

// 把 DashboardService 类型 + snapshot 子模块定义的所有方法重新汇出，
// 让 handler.rs 仍走 `crate::modules::dashboard::service::DashboardService::*`
// 路径访问（路径稳定，零调用方修改）。
pub use snapshot::{
    DASHBOARD_DEFAULT_DAYS, DASHBOARD_MAX_DAYS, DASHBOARD_MIN_DAYS, DASHBOARD_TOP_N,
    DashboardService,
};
