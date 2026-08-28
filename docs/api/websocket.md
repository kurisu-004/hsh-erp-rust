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

| kind | 含义 | payload 关键字段 |
|---|---|---|
| `DashboardSnapshot` | 大屏初始快照 | （snapshot shape 由 dashboard 域定义） |
| `DashboardEvent` | 业务事件，payload 含 `kind` 子类型： | |
| ↳ `DELIVERY_NOTE_CREATED` | 送货单创建 | `note_id` |
| ↳ `DELIVERY_NOTE_PARTS_ADDED` | 送货单加件 | `note_id`, `added_part_ids` |
| ↳ `DELIVERY_NOTE_SCAN_ADD` | 扫码入单（高频） | `note_id`, `part_id`, `batch_id` |
| ↳ `DELIVERY_NOTE_SUBMITTED` | 提交 | `note_id` |
| ↳ `DELIVERY_NOTE_PICKED_UP` | 司机领取 | `note_id`, `driver_user_id` |
| ↳ `DELIVERY_NOTE_PRINTED` | 打印（kind=`note` 或 `label`） | `note_id`, `kind` |
| ↳ `PART_TO_SHIP` | to-ship 成功后 | `part_id` |
| ↳ `PART_TO_INSPECTION` | to-inspection 成功后 | `part_id`, `shelf_code` |
| ↳ `PART_TO_PROCESS` | to-process 成功后 | `part_id` |
| ↳ `BATCH_TO_SHIP` | batch-to-ship 完成后 | `{ submitted: i64, failed: i64 }`（仅计数，非完整数组；前端若需明细直接调 `GET /api/v2/parts/{id}`） |
| ↳ `BATCH_TO_INSPECTION` | batch-to-inspection 完成后 | `{ submitted: i64, failed: i64 }`（仅计数，非完整数组） |
| ↳ `WORKER_SCAN_RETURNED` | parts worker-scan RETURNED 成功后 | `worker_id`, `part_id`, `batch_id`, `event_type` |
| ↳ `WORKER_SCAN_INSPECTED` | parts worker-scan INSPECTED 成功后 | `worker_id`, `part_id`, `batch_id`, `event_type`, `target_inspection_shelf_id` |
| ↳ `WORKER_POOL_REFILL_DONE` | worker-scan / admin-refill 完成后（refill 抢到一批） | `worker_id`, `shelf_id`, `taken: [TakenItem]`, `pool_empty` |
| ↳ `WORKER_POOL_EMPTY` | refill 池空（refill 没抢到任何一批） | `worker_id`, `shelf_id` |
| ↳ `WORKER_POOL_ADMIN_REMOVED` | admin remove 完成后 | `batch_id`, `part_id`, `batch_no`, `quantity`, `serial_no`, `drawing_no`, `system_delivery_date`, `planned_delivery_date`, `is_urgent`, `version`, `worker_id`, `shelf_id`, `next_process_id` |
| `Notification` | 通知 | `user_id`, `content` |
| `Heartbeat` | 心跳 | `ts` |

> **worker-pool 事件说明**：5 个 `WORKER_*` 事件均在 HTTP commit 之后广播（对齐 Python 延迟广播模式，参见 [`docs/architecture.md` §3.7](../architecture.md)）；payload 完整定义见 [`./parts/inspection.md#post-apiv2partsworker-scan`](./parts/inspection.md#post-apiv2partsworker-scan) 与 [`./worker-pool.md`](./worker-pool.md)。
>
> i64 字段在 WS payload 中序列化为字符串（与 HTTP `R<T>` 一致）。

