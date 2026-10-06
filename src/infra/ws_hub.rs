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

//! ## 2026-10-07 变更：再删一个零生产方的变体
//! - **删除** `WsEvent::DashboardSnapshot { data }`：全仓无任何生产方
//!   （只有 dashboard handler 的消费侧匹配，等于永远走不到的死分支）。快照首帧
//!   直接由 `handle_socket` 建好推给新连接，其余时刻一律走 `DashboardEvent`
//!   增量（前端语义是「WS 事件 → invalidate → HTTP 重取」）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum WsEvent {
    /// 大屏增量事件（如 PICKED_UP）
    DashboardEvent {
        kind: String,
        payload: serde_json::Value,
    },
}

/// 2026-10-01 新增：单条 WS 连接的元信息（连接表 value）。
///
/// 刻意**不含** sender（写端由 `handle_socket` 自己持有）：连接表只做「谁在线」的
/// 记账与可观测，`conn_count()` / `user_conn_count()` 是主要消费方。
/// 写端进连接表就必须解决「谁来 flush / 谁负责 Close 握手」的问题，反而把记账
/// 与 IO 生命周期耦在一起，故分开。
///
/// 2026-10-02（review 第 1 轮 Minor-6）：三个字段改为**私有** + accessor。此前三字段
/// 全 `pub`，外部可以随手改 `meta.username`（与连接表记账不一致）；私有化后与
/// `WsHub::conns` 的私有策略一致，只能读不能改。
#[derive(Debug, Clone)]
pub struct WsConnMeta {
    /// 登录用户 id（`CurrentUser::id`）。
    user_id: i64,
    /// 登录用户名。**用途**：日志把 `conn_id` 定位到人（「这条大屏是谁的」），
    /// 也是未来「按用户踢连接 / 在线用户列表」端点的数据源。
    username: String,
    /// 连接建立时刻（单调时钟 `Instant`，不可序列化）。
    /// **用途**：`handle_socket` 退出时用它算连接存活时长打点——
    /// 半开 TCP 被 pong timeout 回收时，日志里能直接看出这条连接活了多久
    /// （几十秒 = 刚建立就死；几小时 = 正常长连）。
    /// 刻意用 `Instant` 而非 `DateTime`：时长计算要单调时钟（系统时间回拨不影响），
    /// 而人类可读的建立时间可由 `Instant` 落日志时刻反推。
    connected_at: Instant,
}

impl WsConnMeta {
    /// 登录用户 id。
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// 登录用户名（见字段 doc 的用途说明）。
    pub fn username(&self) -> &str {
        &self.username
    }

    /// 连接建立时刻（见字段 doc 的用途说明）。
    pub fn connected_at(&self) -> Instant {
        self.connected_at
    }
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
    ///
    /// 2026-10-02（review 第 1 轮 Minor-8）：`cap == 0` 会被 tokio
    /// `broadcast::channel` 直接 panic（"broadcast channel requires a positive
    /// capacity"）。显式 `assert!` 是为了把那条英文 panic 换成能指路的报错。
    /// 选 `assert!` 而非 `cap.max(1)`：容量 0 一定是调用方 bug，静默改写成 1
    /// 会让「慢消费方测试」在毫秒级内狂丢事件、变成难查的 flaky。
    pub fn with_capacity(cap: usize) -> Self {
        assert!(
            cap > 0,
            "WsHub 广播容量必须 ≥ 1（收到 0：tokio broadcast::channel 会 panic，\
             且语义上等于「任何慢消费方都立刻 Lagged」）"
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2026-10-02（review 第 1 轮 Minor-8）：容量 0 必须被显式拦住，而不是让 tokio
    /// `broadcast::channel(0)` 抛那句与本仓上下文无关的英文 panic。
    #[test]
    #[should_panic(expected = "WsHub 广播容量必须 ≥ 1")]
    fn with_capacity_zero_panics_with_readable_message() {
        let _ = WsHub::with_capacity(0);
    }

    /// 2026-10-02（review 第 1 轮 Minor-6）：字段私有化后 accessor 仍能读回原值，
    /// 且注销返回值携带的元信息可算出连接存活时长（`connected_at` 的用途）。
    #[test]
    fn unregister_returns_readable_meta() {
        let hub = WsHub::new();
        let conn_id = hub.register_conn(7, "alice");
        let meta = hub.unregister_conn(conn_id).expect("首次注销应返回元信息");
        assert_eq!(meta.user_id(), 7);
        assert_eq!(meta.username(), "alice");
        assert!(meta.connected_at().elapsed() < Duration::from_secs(1));
        assert!(hub.unregister_conn(conn_id).is_none(), "重复注销返回 None");
    }
}
