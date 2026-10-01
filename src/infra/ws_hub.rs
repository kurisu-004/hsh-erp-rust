//! WebSocket 广播中枢
//!
//! 对应 Python myERP/api/v1/ws.py：
//! - `broadcast_dashboard_snapshot()`：整张大屏快照
//! - `broadcast_dashboard_event(kind, payload)`：单条业务事件
//!
//! 业务实现阶段：
//! - commit 成功后再调用 `broadcast`（Python 是 session.info 延迟到 commit 后）；
//! - 慢 WS 客户端不应拖慢 HTTP 响应，因此每个连接 spawn 独立 task。
//!
//! ## 2026-10-01 变更：死代码清理 + 接入真连接表
//! - **删除** `WsEvent::Notification` / `WsEvent::Heartbeat` 两个变体：全仓**无任何生产方**
//!   （原实现只有 dashboard handler 的消费侧匹配，等于永远走不到的死分支）；
//! - **删除** `user_sinks: DashMap<i64, UnboundedSender<WsEvent>>` + `register_user` /
//!   `unregister_user` / `send_to` 三个方法：同样零调用方。
//!   且它**不能**直接复用当连接表，两个硬伤：
//!   1. `DashMap<i64, ...>` 是**每用户单槽**——同一用户开 2 个标签页就互相覆盖；
//!   2. `UnboundedSender` 无背压，慢消费方可无限堆积内存。
//! - **新增** 真连接表 `conns: DashMap<u64, WsConnMeta>`（`conn_id` 维度，非 user 维度）
//!   + 4 个 accessor。私有字段强制走 accessor，避免外部绕过计数维护。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum WsEvent {
    /// 大屏完整快照（首连接 / 定时全量）
    DashboardSnapshot { data: serde_json::Value },
    /// 大屏增量事件（如 PICKED_UP）
    DashboardEvent {
        kind: String,
        payload: serde_json::Value,
    },
}

/// 2026-10-01 新增：单条 WS 连接的元信息（连接表 value）。
///
/// 刻意**不含** sender（写端由 `handle_socket` 自己持有）：连接表只做「谁在线」的
/// 记账与可观测，`conn_count()` / `user_conn_count()` 是唯一消费方（目前仅日志）。
/// 写端进连接表就必须解决「谁来 flush / 谁负责 Close 握手」的问题，反而把记账
/// 与 IO 生命周期耦在一起，故分开。
#[derive(Debug, Clone)]
pub struct WsConnMeta {
    pub user_id: i64,
    pub username: String,
    pub connected_at: Instant,
}

pub struct WsHub {
    /// 频道广播（dashboard、events 等频道共享）
    pub broadcast_tx: broadcast::Sender<WsEvent>,
    /// 2026-10-01 新增：在线连接表（`conn_id` → 元信息）。**私有**，强制走 accessor。
    conns: DashMap<u64, WsConnMeta>,
    /// 2026-10-01 新增：自增连接 id（`fetch_add` 保证并发唯一，1 起步，0 留作无效值）。
    next_conn_id: AtomicU64,
}

impl WsHub {
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// 2026-10-01 新增：自定义广播通道容量的构造入口（`new()` 委托给它）。
    ///
    /// 容量即「慢消费方最多能积压多少条事件」的上限（见 `handler.rs` 的
    /// `RecvError::Lagged` 分支）。生产固定 1024；测试要用 `1` 之类的极小容量
    /// 在毫秒级内复现慢消费方，灌 1025 条既慢又不稳，故把容量做成入参。
    pub fn with_capacity(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(cap);
        Self {
            broadcast_tx: tx,
            conns: DashMap::new(),
            next_conn_id: AtomicU64::new(1),
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

    /// 2026-10-01 新增：登记一条在线连接，返回 `conn_id`（`handle_socket` 入口调用）。
    ///
    /// `conn_id` 维度而非 `user_id` 维度：同一用户可以同时开多个标签页 / 多端，
    /// 每条连接都是独立实体（前端 WS 重连也会产生新连接，旧连接可能还没被回收）。
    pub fn register_conn(&self, user_id: i64, username: &str) -> u64 {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        self.conns.insert(
            conn_id,
            WsConnMeta {
                user_id,
                username: username.to_string(),
                connected_at: Instant::now(),
            },
        );
        conn_id
    }

    /// 2026-10-01 新增：注销一条连接（`handle_socket` 退出前调用）。
    /// 返回被注销的元信息（已注销过则 `None`）。
    pub fn unregister_conn(&self, conn_id: u64) -> Option<WsConnMeta> {
        self.conns.remove(&conn_id).map(|(_, meta)| meta)
    }

    /// 2026-10-01 新增：当前在线连接总数（跨全部用户）。
    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }

    /// 2026-10-01 新增：指定用户当前在线连接数（同一人可 >1）。
    pub fn user_conn_count(&self, user_id: i64) -> usize {
        self.conns
            .iter()
            .filter(|kv| kv.value().user_id == user_id)
            .count()
    }
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}
