//! `t_part_batch.status` 的强类型投影（域内多处共用）
//!
//! 2026-10-08：`vo/scan.rs` 的其余类型（`ScanDeliveryOut` / `ScanOutcomeDto` /
//! `Resolved*` / `RecentItemDto` / `AddedBatchDto` / `UnresolvedTargetDto` /
//! `AvailableBatchDto` / `AttachableBatchDto` / `ScanDeliveryNoteSummaryDto`）随
//! `POST /scan` 重写与 submit 候选分流删除一并下线，只有本枚举还在用（详情 /
//! 领取的状态展示）。

use serde::Serialize;

/// `t_part_batch.status` 强类型投影。序列化沿用 DB 列值（SCREAMING_SNAKE_CASE）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BatchStatusDto {
    Pending,
    Programming,
    InProcess,
    Inspection,
    ReadyToShip,
    Delivered,
    /// 2026-10-01 起为**遗留兼容值**：REPAIRING 已降级为
    /// `t_part_batch.is_repairing` 标记列（migration 005/006），DB 层不再产生
    /// 该 status，本变体只在 `from_db` 里为「migration 006 未 apply 的存量行」
    /// 保留一条映射。
    ///
    /// 刻意**不删**：删掉后 `from_db("REPAIRING")` 返回 `None`，而两个调用点
    /// 的兜底分别是 `unwrap_or(Pending)`（`scan::helpers`，会把返修批次误标
    /// 成 PENDING）和 `Err(BIZ_DELIVERY_NOTE_INVALID_VALUE)`（`lifecycle.rs`，
    /// 会让整张送货单候选查询 400）—— 都是比「显示一个过时标签」严重得多的
    /// 故障。保留它与 `statemachine::PartStatus::from_str` 的
    /// `"REPAIRING" => IN_PROCESS` 兼容分支是同款取舍：宁可容忍脏数据，也不让
    /// 读路径崩。
    ///
    /// 前端注意：`REPAIRING` 已不在 status 取值域内（新数据恒为
    /// `IN_PROCESS` + `is_repairing` 标记），该值仅可能出现在迁移窗口内的存量
    /// 数据上。
    ///
    /// 刻意**不**加 `#[deprecated]` 属性：本枚举唯一的构造入口
    /// `from_db` 必须写这一条映射才能读存量行，加了属性会在自己仓内触发
    /// `deprecated` 警告（`clippy --all-targets` 零警告门禁过不了），而 Rust
    /// 侧又无法只对外部调用方生效。此处以上方 doc 作为约定的单一来源。
    Repairing,
    Outsource,
    Completed,
    Cancelled,
}

impl BatchStatusDto {
    /// 由 DB 字符串反序列化为枚举；未知值返回 `None`。
    #[allow(clippy::should_implement_trait)]
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => Self::Pending,
            "PROGRAMMING" => Self::Programming,
            "IN_PROCESS" => Self::InProcess,
            "INSPECTION" => Self::Inspection,
            "READY_TO_SHIP" => Self::ReadyToShip,
            "DELIVERED" => Self::Delivered,
            "REPAIRING" => Self::Repairing,
            "OUTSOURCE" => Self::Outsource,
            "COMPLETED" => Self::Completed,
            "CANCELLED" => Self::Cancelled,
            _ => return None,
        })
    }
}
