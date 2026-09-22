//! dashboard 域 service 子模块聚合
//!
//! 拆分依据（Group E 重构 + 单文件职责 / 1000 行上限）：把单文件 `service.rs`
//! 拆为
//! - `snapshot` —— `build_snapshot_with_workers` 装配大屏完整快照（含工人持有的
//!   PICKED_UP 时间戳、产线架每架 top-10 截流、worker 名称查表、未来 7 天交付分桶）
//!
//! `DashboardSnapshot` / `OnProductionShelfGroup` / `DashboardItem` /
//! `UpcomingDeliveryBucket` 等数据结构在 `dto.rs`（2026-09-22 从 service.rs 平移），
//! `BatchLite` / `PartLite` 等 SQL 行精简在 `repo/sql.rs`。
//!
//! ## 调用方契约
//! `handler.rs` 仅引 `crate::modules::dashboard::service::DashboardService::*`，
//! 不直接访问 `snapshot` 子模块。本模块用 `pub use snapshot::*` 把 `DashboardService`
//! 类型 + 方法重新汇出到 `service` 命名空间；`impl DashboardService` 块写在
//! `snapshot.rs` 里不影响方法可见性 —— Rust 的 inherent 方法按类型名寻址，
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
//! ## dashboard 域只有 snapshot 业务
//! dashboard 是 WS-only 域，端点只有 `GET /ws/dashboard`，handler 三形态 ①（snapshot
//! 拉一次即结束，开 tx 但只读无副作用，commit 即可）。后续 ws_hub.broadcast 是订阅模式，
//! handler 不再走 service。

pub mod snapshot;

// 把 DashboardService 类型 + snapshot 子模块定义的所有方法重新汇出，
// 让 handler.rs 仍走 `crate::modules::dashboard::service::DashboardService::*`
// 路径访问（路径稳定，零调用方修改）。
pub use snapshot::{DashboardService, DASHBOARD_TOP_N};