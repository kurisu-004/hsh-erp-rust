//! dashboard 域 snapshot + WS 消息外壳响应 VO（2026-09-22 PR4 重构）
//!
//! 大屏 WS 消息外壳（与 `src/infra/ws_hub.rs::WsEvent` 配套）：
//! - `WsSnapshotMsg`  —— 完整快照
//! - `WsEventMsg`     —— 业务增量
//! - `WsHeartbeatMsg` —— 心跳
//!
//! 大屏 snapshot 数据结构（service 只做装配 + 5 次 trait call）：
//! - `DashboardSnapshot`      —— 完整快照
//! - `WorkerHeldBatch`        —— 工人在手加工批次行
//! - `UpcomingDeliveryBucket` —— 未来 N 天交付分桶（counter）
//!
//! ## `ts` 时间戳格式（全域唯一口径）
//! 三种 message 的 `ts` 一律走 `crate::infra::clock::now_shanghai_iso()`
//! （= `to_rfc3339()`，固定 `+08:00` 偏移，小数秒位数按纳秒有效位自适应）。
//! **禁止**用 `chrono::Local::now()`：那会让时间戳跟着**宿主**时区走，而域内其余
//! 全部锁死 Asia/Shanghai，同一帧里两层 `ts` 于是可能给出两种时区表示。契约登记见
//! `docs/api/dashboard.md` §7。

use serde::Serialize;
use std::collections::BTreeMap; // 2026-09-30 新增：upcoming_delivery 桶按状态细分（按 OrderStatus 字面 → 件数；字母序保证 key 顺序确定，前端按 key 精确查）

// `DashboardSnapshot` 在本文件内定义（2026-09-22 Group E 重构从 service.rs 平移过来），
// 不需要再从 service 模块导入。

#[derive(Debug, Clone, Serialize)]
pub struct WsSnapshotMsg {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub data: DashboardSnapshot,
    pub ts: String,
}

impl WsSnapshotMsg {
    pub fn new(data: DashboardSnapshot) -> Self {
        Self {
            msg_type: "snapshot",
            data,
            ts: crate::infra::clock::now_shanghai_iso(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WsEventMsg {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub event_type: String,
    pub data: serde_json::Value,
    pub ts: String,
}

impl WsEventMsg {
    pub fn new(event_type: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            msg_type: "event",
            event_type: event_type.into(),
            data,
            ts: crate::infra::clock::now_shanghai_iso(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WsHeartbeatMsg {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts: i64,
}

// =======================================================================
// 大屏 snapshot 数据结构（2026-09-22 Group E 重构从 service.rs 平移过来）
// =======================================================================

/// 大屏快照结构
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    /// 逾期未交（2026-10-07 新增；口径见 repo/delivery.rs::count_overdue）
    pub overdue_count: i64,
    /// 品检区待品检批次数（替代原 on_inspection_shelves 的 `.length()`）
    pub in_inspection_count: i64,
    /// 工人在手加工批次（替代原 in_process，字段已收窄，见 WorkerHeldBatch）
    pub in_process: Vec<WorkerHeldBatch>,
    /// 最紧急工单 + 部分已交（2026-10-07 新增，从 com/union_list 域外聚合迁入本域）
    pub system_delivery_orders: super::delivery::SystemDeliveryOrders,
    pub ts: String,
}

/// 工人在手加工批次（7 字段最小集）
///
/// 字段集按前端「在加工」列表实际渲染反推：工单锚点 + 批次锚点 + 展示名 + 数量 +
/// 加急标记 + 持有工人身份。`batch_id` **必须保留** —— 前端列表以它做 `:key`
/// （`t_part` 无唯一约束，同一工单的多个 IN_PROCESS 批次会产生多行，只用 `id`
/// 会造成重复 key）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerHeldBatch {
    pub id: String,
    pub batch_id: Option<String>,
    pub serial_no: Option<String>,
    /// 批次量，取自 `t_part_batch.quantity`
    pub quantity: i32,
    pub is_urgent: bool,
    pub current_holder_id: Option<String>,
    pub worker_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpcomingDeliveryBucket {
    pub date: String,
    pub count: i64,
    /// OrderStatus → 件数（2026-09-30 新增：dashboard 柱状图分层堆叠底座）。
    /// 用 `BTreeMap` 保证 JSON key 字母序确定（前端 `LAYERS` 表自带 statuses 数组按 key 精确查，
    /// 不依赖 JSON key 顺序）。空 map 序列化为 `{}`（不 skip，与 frontend
    /// `z.record(z.string(), z.number())` 必填契约对齐）。
    pub by_status: BTreeMap<String, i64>,
}
