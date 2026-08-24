# WebSocket API

> 本文件须与 `src/infra/ws_hub.rs` + `src/modules/dashboard/handler.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 状态 |
|---|---|---|---|
| GET | `/ws/dashboard` | 已登录（**待 JWT 校验实现**） | 🟡 当前为 stub |

---

### `GET /ws/dashboard`  （WebSocket 升级）

权限: 已登录（**待 JWT 校验实现**）

Request：

- Header: `Upgrade: websocket`、`Connection: Upgrade`
- 鉴权方式（**待定**：建议 query `?token=<access_token>` 或在握手时校验 Header）

⚠️ **当前为 stub**：`src/modules/dashboard/handler.rs::ws_handler_stub` 是空函数，**未做 WebSocket 握手 / JWT 校验**，前端调用会**卡到超时**。请等待 `ws_handler_stub` 被替换为真实实现。

### 预期事件类型

来自 `src/infra/ws_hub.rs::WsEvent`（**实施后才会下发**）：

| kind | 含义 |
|---|---|
| `DashboardSnapshot` | 大屏初始快照 |
| `DashboardEvent` | 业务事件，payload 含 `kind` 子类型： |
| ↳ `DELIVERY_NOTE_CREATED` | 送货单创建 |
| ↳ `DELIVERY_NOTE_PARTS_ADDED` | 送货单加件 |
| ↳ `DELIVERY_NOTE_SCAN_ADD` | 扫码入单（高频） |
| ↳ `DELIVERY_NOTE_SUBMITTED` | 提交 |
| ↳ `DELIVERY_NOTE_PICKED_UP` | 司机领取 |
| ↳ `DELIVERY_NOTE_PRINTED` | 打印（kind=`note` 或 `label`） |
| `Notification` | 通知 |
| `Heartbeat` | 心跳 |

