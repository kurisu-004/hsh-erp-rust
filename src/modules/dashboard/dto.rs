//! dashboard DTO（2026-09-15 takeover-fill）
//!
//! 大屏 WS 消息外壳（与 `src/infra/ws_hub.rs::WsEvent` 配套）：
//! - `WsSnapshotMsg`  —— 完整快照
//! - `WsEventMsg`     —— 业务增量
//! - `WsHeartbeatMsg` —— 心跳

use serde::Serialize;

use crate::modules::dashboard::service::DashboardSnapshot;

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