# WebSocket API

> 本文件须与 `src/infra/ws_hub.rs` + `src/modules/dashboard/{handler,service,dto}.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 状态 |
|---|---|---|---|
| GET | `/ws/dashboard` | 任意已登录（*） | ✅ 2026-09-15 takeover-fill：真实握手 + 快照推送 + 业务事件订阅 |

> 路径：挂在 `modules::ws_router()` 下 `/ws` 前缀（**不带** `/api/v2`，与前端 nginx `/ws/*` → `rust-backend:3000` 反代一致）。

---

### `GET /ws/dashboard`  （WebSocket 升级）

权限: 任意已登录用户（MANAGER / SHELF_ACCOUNT / CLERK / INSPECTOR / CNC_PROGRAMMER）。

Request：

- Header: `Upgrade: websocket`、`Connection: Upgrade`
- 鉴权：query `?token=<access_token>`（必填）

握手流程：

1. 解 JWT + 验签（`iss` 绑定 `config.jwt.issuer`，`exp` 校验）。
2. 服务端强制 session 校验：查 Redis `session:tok:<jti>` 必须存在；缺失 → 40105 `SESSION_REVOKED`。
   （2026-09-23 重构：Redis session key 由 sha256(token) 改为 JWT claims.jwt_id（UUID v4），
   直接以 jti 作为 key 后缀，**不**对 token 做哈希。）
3. `WebSocketUpgrade.on_upgrade` 触发 upgrade；失败（如 token 无效）走 axum normal response（HTTP 4xx + JSON 信封）。
4. 连接建立后立即推一次 `WsSnapshotMsg`（完整快照）。
5. 订阅 `state.ws_hub.broadcast`：
   - `WsEvent::DashboardSnapshot { data }` → 转发 `{"type":"snapshot","data":data,"ts":...}`
   - `WsEvent::DashboardEvent { kind, payload }` → 转发 `WsEventMsg { type:"event", event_type, data, ts }`
   - `WsEvent::Notification` / `WsEvent::Heartbeat` → 丢弃（dashboard 不消费）
6. 心跳：30s `WsHeartbeatMsg { type:"heartbeat", ts }`，**作为 text 帧下发**（浏览器 JS `onmessage` 可直接监听；不是 protocol-level Ping）。
7. 客户端 `Ping` → `Pong`；`Close` / `None` / 错误 → 清理连接。

响应（连接建立后服务端首发）：

```json
{
  "type": "snapshot",
  "data": {
    "on_production_shelves": [...],
    "on_inspection_shelves": [...],
    "in_process": [...],
    "upcoming_delivery": [{"date":"2026-09-15","count":0}, ...7 条],
    "ts": "2026-09-15T10:00:00+08:00"
  },
  "ts": "..."
}
```

错误码（握手阶段，HTTP 响应）：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 40100 | UNAUTHORIZED | 401 | 缺少 `?token` |
| 40100 | UNAUTHORIZED | 401 | JWT 解码 / 验签失败（过期 / 签名错） |
| 40105 | SESSION_REVOKED | 401 | Redis session 不存在 |

> 错误码段说明：本端点用通用 `UNAUTHORIZED`（非业务码），与 `BIZ_AUTH_INVALID 40101`（登录失败）刻意区分。

## 实现要点

- 快照构造：`DashboardService::build_snapshot_with_workers`（service 内部 `pool.begin()` + `commit()`）。
- 业务事件订阅：`tokio::sync::broadcast::Sender` 多生产者多消费者；客户端 buffer 受 `WsHub::new()` 的 channel 容量（1024）约束，慢消费方会丢消息——dashboard 不消费 Notification/Heartbeat 故影响可控。
- 心跳：服务端 30s 周期发 `WsHeartbeatMsg` **text 帧**（浏览器 JS `onmessage` 可直接监听）；客户端 `Ping` → axum 协议层自动 `Pong`（与心跳 text 帧是两件事，不要混用）。
- 鉴权镜像 HTTP `CurrentUser::from_request_parts` extractor 的 Redis 校验语义（含 TTL 滑动）。
- i64 字段在 WS payload 中序列化为字符串（与 HTTP `R<T>` 一致）。

### 预期事件类型

来自 `src/infra/ws_hub.rs::WsEvent`（**实施后才会下发**）：

| kind | 含义 | payload 关键字段 |
|---|---|---|
| `DashboardSnapshot` | 大屏初始快照 | （snapshot shape 由 dashboard 域定义） |
| `DashboardEvent` | 业务事件，payload 含 `kind` 子类型： | |
| ↳ `DELIVERY_NOTE_CREATED` | 送货单创建 | `note_id` |
| ↳ `DELIVERY_NOTE_PARTS_ADDED` | 送货单加件 | `note_id`, `added_part_ids` |
| ↳ `DELIVERY_NOTE_SCAN_ADD` | 扫码入单（高频） | `note_id`, `part_id`, `batch_id` |
| ↳ `DELIVERY_NOTE_BATCHES_ATTACHED` | 弹窗提交后（`POST /{note_id}/attach-batches`，2026-08-31 新增） | `{ delivery_note_id, attached_count, conflict_count }`（监听端用 `conflict_count > 0` 判断是否有失败项） |
| ↳ `DELIVERY_NOTE_SUBMITTED` | 提交 | `note_id` |
| ↳ `DELIVERY_NOTE_PICKED_UP` | 司机领取 | `note_id`, `driver_user_id` |
| ↳ `DELIVERY_NOTE_PRINTED` | 打印（kind=`note` 或 `label`） | `note_id`, `kind` |
| ↳ `PART_TO_SHIP` | to-ship 成功后 | `part_id` |
| ↳ `PART_TO_INSPECTION` | to-inspection 成功后 | `part_id`, `shelf_code` |
| ↳ `PART_TO_PROCESS` | to-process 成功后 | `part_id` |
| ↳ `BATCH_TO_SHIP` | batch-to-ship 完成后 | `{ submitted: i64, failed: i64 }`（仅计数，非完整数组；前端若需明细直接调 `GET /api/v2/parts/{id}`） |
| ↳ `BATCH_TO_INSPECTION` | batch-to-inspection 完成后 | `{ submitted: i64, failed: i64 }`（仅计数，非完整数组） |
| ↳ `PART_SOFT_DELETED` | part 软删 | `part_id` |
| ↳ `PART_DELIVERED` | part deliver 成功（2 处广播：lifecycle + scan/deliver-part） | `part_id`, `batch_id` |
| ↳ `PART_BATCH_SPLIT` | 批次拆分 | `part_id`, `batch_id`, `new_batch_id` |
| ↳ `PART_BATCH_CANCELLED` | 批次取消 | `part_id`, `batch_id` |
| ↳ `PART_SCAN_INSPECT_PASSED` | 扫码品检通过 | `part_id`, `batch_id` |
| ↳ `PART_SCAN_INSPECT_FAILED` | 扫码品检失败 | `part_id`, `batch_id`, `reason` |
| ↳ `PART_BATCH_WITH_PDFS_CREATED` | 多页 PDF 批量创建 | `part_ids`, `count` |
| ↳ `PART_PICKED_UP` | B 方案手动 pick-up 成功（Phase 2） | `part_id`, `worker_id`, `batch_id` |
| ↳ `WORKER_SCAN_RETURNED` | parts worker-scan RETURNED 成功后 | `worker_id`, `part_id`, `batch_id`, `event_type` |
| ↳ `WORKER_SCAN_INSPECTED` | parts worker-scan INSPECTED 成功后 | `worker_id`, `part_id`, `batch_id`, `event_type`, `target_inspection_shelf_id` |
| ↳ `WORKER_POOL_REFILL_DONE` | worker-scan / admin-refill 完成后（refill 抢到一批） | `worker_id`, `shelf_id`, `taken: [TakenItem]`, `pool_empty` |
| ↳ `WORKER_POOL_EMPTY` | refill 池空（refill 没抢到任何一批） | `worker_id`, `shelf_id` |
| ↳ `WORKER_POOL_ADMIN_REMOVED` | admin remove 完成后 | `batch_id`, `part_id`, `batch_no`, `quantity`, `serial_no`, `drawing_no`, `system_delivery_date`, `planned_delivery_date`, `is_urgent`, `version`, `worker_id`, `shelf_id` |
| ↳ `WORKER_POOL_AUTO_ALLOCATE_DONE` | auto-allocate 批量分配完成 | `worker_id`, `allocated_count`, `process_id` |
| ↳ `ASSEMBLY_CREATED` | 装配件创建 | `assembly_id` |
| ↳ `ASSEMBLY_DELETED` | 装配件软删 | `assembly_id` |
| ↳ `ASSEMBLY_CANCELLED` | 装配件取消 | `assembly_id` |
| ↳ `ASSEMBLY_UPDATED` | 父装配件 status 更新（前端主动改字段 / inspection 流 auto-rollup） | `assembly_id` |
| `Notification` | 通知 | `user_id`, `content` |
| `Heartbeat` | 心跳 | `ts` |

> **worker-pool 事件说明**：5 个 `WORKER_*` 事件均在 HTTP commit 之后广播（对齐 Python 延迟广播模式，参见 [`docs/architecture.md` §3.7](../architecture.md)）；payload 完整定义见 [`./parts/inspection.md#post-apiv2partsworker-scan`](./parts/inspection.md#post-apiv2partsworker-scan) 与 [`./production/worker-pool.md`](./production/worker-pool.md)。
>
> i64 字段在 WS payload 中序列化为字符串（与 HTTP `R<T>` 一致）。

### `ASSEMBLY_UPDATED`

- **Payload**: `{ "assembly_id": "<stringified i64>" }`
- **触发端点**:
 - `POST /api/v2/assemblies/{id}/update`（前端主动改字段）
 - `POST /api/v2/parts/{id}/to-inspection` / `to-ship` / `to-process`
 - `POST /api/v2/parts/batch-to-inspection` / `batch-to-ship`
 - `POST /api/v2/parts/worker-scan`（仅 `INSPECTED` 分支，**实际翻状态时**才下发；dedup by assembly_id）
- **频率**：每个 inspection 调用最多 1 次（per unique parent assembly）。

