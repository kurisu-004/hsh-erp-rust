//! delivery_note 状态机
//!
//! 对应 Python myERP/statemachines/delivery_note_state_machine.py。
//!
//! 实施约定：
//! - 手写 enum + 迁移表（`can_transition_to`），状态机不写 DB
//! - 事件日志（SUBMITTED / WITHDRAWN / PICKED_UP / ARCHIVED + CREATED）由
//!   service 在事务内统一插入
//! - 扫码入单**不写**噪音事件（沿用 Python 2026-07-23「ITEM_ADDED 不记录」决策）
//!
//! 状态机本体定义在 `model.rs::DeliveryNoteStatus`，本文件是入口模块。
//! 单元测试 `status_machine_round_trip` 已由 `model.rs` 内 `#[cfg(test)]` 完成。

// 状态机本体在 `model.rs::DeliveryNoteStatus`；本文件仅留模块文档。
// `status_machine_round_trip` 单元测试见 `model.rs`。