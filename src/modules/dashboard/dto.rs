//! dashboard DTO（2026-09-15 takeover-fill）
//!
//! 大屏 WS 消息外壳（与 `src/infra/ws_hub.rs::WsEvent` 配套）：
//! - `WsSnapshotMsg`  —— 完整快照
//! - `WsEventMsg`     —— 业务增量
//! - `WsHeartbeatMsg` —— 心跳
//!
//! 大屏 snapshot 数据结构（2026-09-22 Group E 重构从 `service.rs` 平移过来，
//! service 不再持有数据类，只做装配 + 4 次 trait call）：
//! - `DashboardSnapshot`        —— 完整快照
//! - `OnProductionShelfGroup`    —— 单个生产区货架分组
//! - `DashboardItem`            —— 单个 item 行
//! - `UpcomingDeliveryBucket`   —— 未来 N 天交付分桶（counter）

use serde::Serialize;

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
        let ts = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();
        Self {
            msg_type: "snapshot",
            data,
            ts,
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
        let ts = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();
        Self {
            msg_type: "event",
            event_type: event_type.into(),
            data,
            ts,
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

/// 大屏快照结构（与 v1 Python 端 JSON 字段命名一致；前端可平滑切 v2 WS）。
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub on_production_shelves: Vec<OnProductionShelfGroup>,
    pub on_inspection_shelves: Vec<DashboardItem>,
    pub in_process: Vec<DashboardItem>,
    pub upcoming_delivery: Vec<UpcomingDeliveryBucket>,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnProductionShelfGroup {
    pub shelf_id: String,
    pub shelf_code: String,
    pub shelf_name: String,
    pub total_count: usize,
    pub items: Vec<DashboardItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardItem {
    pub id: String,
    pub batch_id: Option<String>,
    pub batch_no: Option<i32>,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub planned_delivery_date: Option<String>,
    pub picked_up_at: Option<String>,
    pub current_holder_id: Option<String>,
    pub current_holder_kind: Option<String>,
    pub shelf_code: Option<String>,
    pub customer_id: Option<String>,
    pub customer_name: Option<String>,
    pub customer_path: Option<String>,
    pub next_process_id: Option<String>,
    pub next_process_name: Option<String>,
    pub worker_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpcomingDeliveryBucket {
    pub date: String,
    pub count: i64,
}