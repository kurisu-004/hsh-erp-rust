//! dashboard HTTP handler / WebSocket 占位
//!
// 对应 Python myERP/api/v1/ws.py。端点：`GET /ws/dashboard`。
// 实施约定：
//! 1. WebSocketUpgrade 握手
//! 2. JWT 校验（query ?token= 或 Authorization 头）
//! 3. 订阅 ws_hub.broadcast_tx 后写入 socket；接收端读客户端指令（subscribe dashboard/events）
