//! WebSocket 广播中枢
//!
//! 对应 Python myERP/api/v1/ws.py：
//! - `broadcast_dashboard_snapshot()`：整张大屏快照
//! - `broadcast_dashboard_event(kind, payload)`：单条业务事件
//! - 用户级精准投递（`send_to`）
//!
//! 业务实现阶段：
//! - commit 成功后再调用 `broadcast`（Python 是 session.info 延迟到 commit 后）；
//! - 慢 WS 客户端不应拖慢 HTTP 响应，因此每个连接 spawn 独立 task。

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum WsEvent {
    /// 大屏完整快照（首连接 / 定时全量）
    DashboardSnapshot { data: serde_json::Value },
    /// 大屏增量事件（如 PICKED_UP）
    DashboardEvent { kind: String, payload: serde_json::Value },
    /// 用户级精准通知
    Notification { user_id: i64, content: String },
    /// 心跳
    Heartbeat,
}

pub struct WsHub {
    /// 频道广播（dashboard、events 等频道共享）
    pub broadcast_tx: broadcast::Sender<WsEvent>,
    /// 用户级精准投递（user_id -> mpsc tx）
    pub user_sinks: DashMap<i64, mpsc::UnboundedSender<WsEvent>>,
}

impl WsHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            broadcast_tx: tx,
            user_sinks: DashMap::new(),
        }
    }

    /// 向所有订阅者广播
    pub fn broadcast(&self, event: WsEvent) {
        // send 不阻塞：失败仅表示当前无订阅者
        let _ = self.broadcast_tx.send(event);
    }

    /// 订阅广播频道
    pub fn subscribe(&self) -> broadcast::Receiver<WsEvent> {
        self.broadcast_tx.subscribe()
    }

    /// 注册用户连接（用于精准投递）
    pub fn register_user(&self, user_id: i64, tx: mpsc::UnboundedSender<WsEvent>) {
        self.user_sinks.insert(user_id, tx);
    }

    pub fn unregister_user(&self, user_id: i64) {
        self.user_sinks.remove(&user_id);
    }

    /// 精准发送给特定用户
    pub fn send_to(&self, user_id: i64, event: WsEvent) {
        if let Some(tx) = self.user_sinks.get(&user_id) {
            let _ = tx.send(event);
        }
    }
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}