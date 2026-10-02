# WebSocket API

> 本文件须与 `src/infra/ws_hub.rs` + `src/modules/dashboard/{handler,service,dto}.rs` 保持同步
>
> **2026-10-01 WS 健壮性加固**：Lagged 不再静默吞（→ `4003` 踢出重连）+ 全部退出路径补主动
> Close 帧（关闭码表见下方「连接关闭码」）+ Ping/Pong 存活检测 + 周期性 re-auth（→ `4001`）。
> 实现说明见 `src/modules/dashboard/handler.rs` 的 module-level doc「2026-10-01 WS 健壮性加固（4 项）」。
>
> **2026-10-02 review 第 1 轮修复（3 Major）**：
> 1. **re-auth 失败按原因分流**：`40100/40102/40105`（鉴权类）→ `4001`；其余（含 Redis 故障
>    `50000 INTERNAL`）→ `1011 re-auth unavailable`。此前对两者一律发 `4001`，一次 Redis 抖动
>    就会按下表的约定把全站用户登出。
> 2. **启动期把 `pong_timeout` 下限从 `> ping` 收紧为 `>= 2 × ping`**（`ping=20 / pong=21`
>    这种 1s 余量的配置已不能通过启动校验）。
> 3. **`close_with` 加 2s 收尾超时**：半开 TCP 下 `send`/`poll_close` 会长时间 `Pending`，
>    原先会把 `unregister_conn` 一起卡住 → 连接表条目泄漏。
> 另：`select!` 加 `biased;`、re-auth 套 5s 调用超时（超时按「本轮跳过」不判死）、
> re-auth 周期改为可注入配置项 `WS_REAUTH_EVERY_N_HEARTBEATS`。
>
> **2026-09-28 新增**：v2 前端走 HTTP 全量首取（`GET /api/v2/dashboard/snapshot`，
> 详见 [`./dashboard.md`](./dashboard.md)），WS 首帧 `WsSnapshotMsg` 保留向后兼容，
> 新前端可忽略首帧改走「HTTP 首取 + WS 事件 invalidate」模式。
>
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 环境变量

| env | 缺省 | 含义 |
|---|---|---|
| `WS_HEARTBEAT_INTERVAL_SECONDS` | 30 | text 心跳帧间隔（前端 JS 感知） |
| `WS_PING_INTERVAL_SECONDS` | 20 | protocol-level `Ping` 间隔（服务端存活检测） |
| `WS_PONG_TIMEOUT_SECONDS` | 60 | Pong 超时阈值；启动期强制校验 **`>= 2 × WS_PING_INTERVAL_SECONDS`** |
| `WS_REAUTH_EVERY_N_HEARTBEATS` | 10 | 每 N 次 text 心跳做一次周期性 re-auth（生产 30s × 10 ≈ 5min）；启动期强制校验 `>= 1` |

## 端点列表

| Method | Path | 权限 | 状态 |
|---|---|---|---|
| GET | `/ws/dashboard` | 任意已登录（*） | ✅ 2026-09-15 takeover-fill：真实握手 + 快照推送 + 业务事件订阅<br>✅ 2026-10-01：关闭码 + 存活检测 + 周期性 re-auth |

> 路径：挂在 `modules::ws_router()` 下 `/ws` 前缀（**不带** `/api/v2`，与前端 nginx `/ws/*` → `rust-backend:3000` 反代一致）。

---

### `GET /ws/dashboard`  （WebSocket 升级）

权限: 任意已登录用户（MANAGER / SHELF_ACCOUNT / CLERK / INSPECTOR / CNC_PROGRAMMER）。

Request：

- Header: `Upgrade: websocket`、`Connection: Upgrade`
- 鉴权：query `?token=<access_token>`（必填）

握手流程：

1. 解 JWT + 验签（`iss` 绑定 `config.jwt.issuer`，`exp` 校验）。

> 2026-09-23 重构：RS256 + kid —— JWT 签名算法由 HS256 切到 RS256，header.kid
> 按 `JWT_SIGNING_KID` env 路由公钥字典（`JWT_PUBLIC_KEYS_DIR` 目录扫描 `*.pem`，
> kid = 文件名去后缀）；HS256 仅作 fallback（`JWT_ALLOW_HS256_FALLBACK=true`）。
> 详见 `index.md` "JWT 签名算法与 kid" 段。前端无感（payload 字段不变）。

2. 服务端强制 session 校验：查 Redis `session:tok:<jti>` 必须存在；缺失 → 40105 `SESSION_REVOKED`。
   （2026-09-23 重构：Redis session key 由 sha256(token) 改为 JWT claims.jwt_id（UUID v4），
   直接以 jti 作为 key 后缀，**不**对 token 做哈希。）
3. `WebSocketUpgrade.on_upgrade` 触发 upgrade；失败（如 token 无效）走 axum normal response（HTTP 4xx + JSON 信封）。
4. 连接建立后立即推一次 `WsSnapshotMsg`（完整快照）。
5. 订阅 `state.ws_hub.broadcast`：
   - `WsEvent::DashboardSnapshot { data }` → 转发 `{"type":"snapshot","data":data,"ts":...}`
   - `WsEvent::DashboardEvent { kind, payload }` → 转发 `WsEventMsg { type:"event", event_type, data, ts }`
   - `Err(Lagged(n))` → 广播队列溢出，本连接**永久丢了 n 条事件** → 记 `warn!` + 发
     `4003 lagged` Close 帧断开（见下方「连接关闭码」）
   - `Err(Closed)` → 广播通道已关闭（所有 Sender 都 drop 了）→ 记 `info!` 断开
6. 心跳（**两套并存、职责分离，勿合并**）：
   - `ws_heartbeat_interval_seconds`（缺省 30s）周期发 `WsHeartbeatMsg { type:"heartbeat", ts }`
     作为 **text 帧**下发（浏览器 JS `onmessage` 可直接监听；**不是** protocol-level Ping）；
   - `ws_ping_interval_seconds`（缺省 20s）周期发 protocol-level `Message::Ping`（空 payload）：
     浏览器按 RFC 6455 §5.5.2 在**协议栈**自动回 `Pong`（JS 完全不可见），**给服务端存活检测用**。
7. 空闲超时（存活检测）：`ws_pong_timeout_seconds`（缺省 60s = 3× ping 间隔，容忍连续丢 2 次 Pong）
   内没收到**任何入站帧**（Pong / 业务帧 / Close 都算）→ 判定对端已死 → 发 `1011 pong timeout`
   断开。**启动期强制校验 `pong_timeout >= 2 × ping_interval`**：余量只有 1~2 倍扛不住
   RTT + 调度抖动，会误杀健康连接（2026-10-02 由 `> ping_interval` 收紧）。
8. 周期性 re-auth：每 `WS_REAUTH_EVERY_N_HEARTBEATS` 次（缺省 10，生产 ≈ 5min）调一次
   `verify_session_token` 重验 session。**按失败原因分流**（2026-10-02 修复）：
   - 鉴权类（`40100` / `40102` / `40105`：session 吊销 / access jti 进黑名单 / JWT 失效）
     → 记 `warn!` + 发 `4001 auth expired` 断开；
   - 其余（典型是 Redis 挂 / 连接池耗尽 → `50000 INTERNAL`）→ 发 `1011 re-auth unavailable`
     断开，**前端只重连、不登出**。
   单次调用另有 5s 超时（Redis 卡住时冻结 `select!`，期间 1012 发不出），**超时按「本轮跳过」
   处理、不判死**（健康客户端的 Pong 已在接收缓冲里，解冻后由 `biased` 入站分支续命）。
   ⚠️ 该调用内部会 `touch_session` **滑动续期** Redis session TTL：只要大屏页开着，session 就不会
   自然过期（与 HTTP 路径「每次请求滑动 TTL」语义一致）；用户关掉页面后不再续期才开始走向过期。
9. 客户端 `Ping` → `Pong`；`Close` / `None` / 错误 → 清理连接（无需发 Close 帧，客户端已发起关闭）。
10. 服务优雅退出（Ctrl-C / `state.shutdown.cancel()`）→ 发 `1012 server restart` 断开，
    前端应立即重连（而不是干等 TCP 超时）。

响应（连接建立后服务端首发）：

```json
{
  "type": "snapshot",
  "data": {
    "on_production_shelves": [...],
    "on_inspection_shelves": [...],
    "in_process": [...],
    "upcoming_delivery": [{"date":"2026-09-15","count":3,"by_status":{"PENDING":2,"INSPECTION":1}}, ...N 条（N 来自 service 默认值 14，与 HTTP `/snapshot` 端点对齐；2026-09-30 同步）],
    "ts": "2026-09-15T10:00:00+08:00"
  },
  "ts": "..."
}
```

> 2026-10-01 订正：本节原写 `upcoming_delivery[]` 的状态字段是 `status`，与实现漂移
> （`src/modules/dashboard/vo/snapshot.rs::UpcomingDeliveryBucket` 的字段是 **`by_status`**：
> `BTreeMap<OrderStatus 字面, i64>`，即「按状态细分的件数」；`count` 是当日合计）。

错误码（握手阶段，HTTP 响应）：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 40100 | UNAUTHORIZED | 401 | 缺少 `?token` |
| 40100 | UNAUTHORIZED | 401 | JWT 解码 / 验签失败（过期 / 签名错） |
| 40105 | SESSION_REVOKED | 401 | Redis session 不存在 |

> 错误码段说明：本端点用通用 `UNAUTHORIZED`（非业务码），与 `BIZ_AUTH_INVALID 40101`（登录失败）刻意区分。

## 连接关闭码（2026-10-01 新增）

握手成功**之后**，服务端在**能发出 Close 帧的路径**上都主动发（此前全仓零主动发送，退出路径
全是裸 drop，浏览器只能看到 1006，无法区分「服务端主动踢 / 网络断 / session 失效」）。

2026-10-02 起有两处**不发** Close 帧（2026-10-02 修复 Minor-3 / Minor-5 / Nit-2）：

- **写侧已失败的 6 条路径**（推初始快照 / 回 Pong / 广播快照 / 广播事件 / text 心跳 /
  协议层 Ping 写失败）：`send` 已返回 Err，写端与 socket 已不可用，再发 Close 帧必然再失败一次
  （只会多一条 `warn!` 噪音），故改为只记原始错误即断开 → 浏览器看到 1006，走通用重连。
  判据是 tokio-tungstenite 的 `max_write_buffer_size` 缺省 `usize::MAX` ⇒ 写缓冲永不主动限流，
  `Sink::send` 唯一可能的失败就是 socket 级致命错，此时 Close 帧**物理上**发不出去。
- **广播通道 `Closed`**：这是「hub 已被整体 drop」的全局信号，前端该做的是重连（新连接会重新
  建立订阅），语义等同 1006；「进程收尾」由下面的 `1012` 分支专门负责。

`run_socket` 共有 **14 条退出路径**（逐条表见 `src/modules/dashboard/handler.rs` module doc
末尾的「退出路径全景」），其中 **5 条**发 Close 帧、**9 条**裸断。`close_with` 内部有 **2s 收尾
超时**：半开 TCP + 发送缓冲满时 `send`/`poll_close` 会长时间 `Pending`，那会把
`unregister_conn` 一起卡住导致连接表条目泄漏（正是存活检测要回收的那类连接），超时即放弃
flush 直接断。

| code | 名称 | reason | 触发场景 | 前端应采取的动作 |
|---|---|---|---|---|
| 1000 | normal closure | —（客户端通常不带 reason） | **服务端当前不主动发 1000**；只会在客户端 `close(1000)` 的回声里出现 | 正常关闭，无需动作 |
| 1001 | going away | — | **服务端当前不主动发 1001**（2026-10-02 起写失败路径改为裸断，见上文） | 走通用重连 |
| 1011 | internal error | `snapshot build failed` | 首次快照构建失败（DB 故障等） | **可重试但应退避**（服务端侧问题，连续重试无意义 → 提示用户稍后再试） |
| 1011 | internal error | `send snapshot failed` | **服务端当前不主动发此 reason**（2026-10-02 Nit-2 起「推初始快照写失败」也改为裸断，见上文） | 无需动作（不会出现） |
| 1011 | internal error | `re-auth unavailable` | **2026-10-02 新增**：周期性 re-auth 失败但**不是**鉴权问题（典型：Redis 挂 / 连接池耗尽 → `50000`） | 走通用重连（**不要**清 token —— 重连后 re-auth 大概率就恢复了） |
| 1011 | internal error | `pong timeout` | 超过 `ws_pong_timeout_seconds` 未收到任何入站帧（对端已死 / 半开连接） | 走通用重连（**必须**重连：旧连接不会再有数据） |
| 1012 | service restart | `server restart` | 服务优雅退出（Ctrl-C / 发布重启） | **立即重连**（可能需退避，避免重启风暴期打满） |
| 4001 | auth expired | `auth expired` | 周期性 re-auth 失败且**属鉴权类**（`40100` / `40102` / `40105`：session 被吊销 / access jti 进黑名单 / JWT 失效） | **清本地 token 并跳登录页**（不要重连——重连会被 40105 拒） |
| 4003 | lagged | `lagged` | 慢消费方：广播队列（容量 1024）溢出，永久丢了 n 条事件 | **重连 + 全量 HTTP 重取**（`GET /api/v2/dashboard/snapshot`）；重连时自然重新收到首帧 snapshot |

约定：

- `1000-2999` 是 RFC 6455 §7.4.2 协议保留段，`4000-4999` 是应用私有段；WS Close code 是 2 字节
  u16，与 HTTP 信封错误码（`40100` 等）**是两套协议**，不要混用。
- 浏览器侧统一从 `event.code` / `event.reason` 取；`1006` 仍然表示「没有任何 Close 帧的异常断开」
  （纯网络问题），此时照常重连即可。
- 前端不应依赖具体 reason 文案（可能后续调整），**只依赖 code**。

## 实现要点

- 快照构造：`DashboardService::build_snapshot_with_workers`（service 内部 `pool.begin()` + `commit()`）。
- 业务事件订阅：`tokio::sync::broadcast::Sender` 多生产者多消费者；客户端 buffer 受
  `WsHub::new()` 的 channel 容量（1024）约束。**慢消费方会丢的是 `DashboardEvent`**（不是
  Notification/Heartbeat——那两个变体 2026-10-01 已作为死代码删除），而前端更新语义是
  「WS 事件 → invalidate → HTTP 重取」**不是**增量 patch：所以**丢 1 个事件 = 对应区块永不刷新**
  （服务端此前连日志都没有）。处理是 `Lagged` → `warn!(missed=n)` + 发 `4003` 踢出 → 前端重连 +
  全量 HTTP 重取（比补发 n 条后续事件更便宜也更正确）。
- 心跳 / 存活检测：text 心跳（前端感知）与 protocol-level Ping（服务端判活）**两套并存**，
  间隔独立配置（`WS_HEARTBEAT_INTERVAL_SECONDS` / `WS_PING_INTERVAL_SECONDS` /
  `WS_PONG_TIMEOUT_SECONDS`）。超时判定必须用「最后一次入站帧 + 固定时长」的**绝对 deadline**，
  不能用相对 `sleep`——`select!` 每轮迭代都重建 future，相对时长会被 30s 的 text 心跳不断重置。
- `select!` 必须写 `biased;` 且入站分支排第一（2026-10-02 修复 Minor-1）：否则 pong deadline 与
  socket 可读**同时** ready 时随机选分支，有概率先命中超时分支、把健康连接判死。dashboard 入站
  流量只有客户端自动回的 Pong（1 个 / ping_interval，随收随走），饿不死其它分支。
- 在线连接表：`WsHub::conns`（`conn_id` 维度，同一用户可多条）+ `conn_count()` /
  `user_conn_count(user_id)` accessor；`handle_socket` 入口 register、退出前 unregister，
  两端各打一行 `info!` 连接数日志，**退出那行带上连接存活时长**（取自
  `WsConnMeta::connected_at`，半开连接被回收时能直接看出活了多久）。
  `WsConnMeta` 三字段私有 + accessor（`user_id()` / `username()` / `connected_at()`）。
  **当前无统计端点**（后续独立任务）。
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
| ↳ `PART_BATCH_SPLIT` | 批次拆分（**两种 payload**，见下方「`PART_BATCH_SPLIT` 双 payload」注） | `.../split`：`part_id`, `new_batch_id`；`.../pick-up` 部分领取：`part_id`, `new_batch_id`, `source_batch_id`, `quantity` |
| ↳ `PART_BATCH_CANCELLED` | 批次取消 | `part_id`, `batch_id` |
| ↳ `PART_SCAN_INSPECT_PASSED` | 扫码品检通过 | `part_id`, `batch_id` |
| ↳ `PART_SCAN_INSPECT_FAILED` | 扫码品检失败 | `part_id`, `batch_id`, `reason` |
| ↳ `PART_BATCH_WITH_PDFS_CREATED` | 多页 PDF 批量创建 | `part_ids`, `count` |
| ↳ `PART_PICKED_UP` | B 方案手动 pick-up 成功（Phase 2） | `part_id`, `worker_id`, `batch_id`, `quantity`（后两个 2026-10-03 新增；`batch_id` / `quantity` 反映**实际领走的那一批**：整批路径 = 源批次与整批量，部分领取 = 拆出的新批次与拆走量） |
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
| ↳ `BATCH_PLACED_ON_SHELF` | 车间下发台 PENDING 批次 → IN_PROCESS 成功（2026-09-29 新增，`prod::batch` 单条 dispatch 端点） | `{ batch_id, target_process_id, shelf_id, version }` |
| ↳ `BATCH_PLACED_ON_SHELF` | 车间下发台多 batch 批量下发成功（2026-09-29 新增，`prod::batch` bulk-dispatch / auto-dispatch 端点，仅 succeeded 部分；skipped 不广播） | `{ batches: [{ batch_id, target_process_id, shelf_id, version }, ...] }` |
| ↳ `ROLLUP_RECOMPUTED` | 2026-10-01 新增：admin 对账端点修正了派生缓存（**仅真有变化时**发，幂等空跑不发） | `{ scope, parts_changed, assemblies_changed, operator_id }` |

> **2026-10-01 变更**：`Notification` / `Heartbeat` 两个 `WsEvent` 变体已**删除**（全仓无任何
> 生产方，只有 dashboard handler 的消费侧匹配，属于永远走不到的死分支）。`WsEvent` 现在只有
> `DashboardSnapshot` 与 `DashboardEvent` 两个变体；「心跳」走独立的 `WsHeartbeatMsg` text 帧 +
> protocol-level `Ping`（见「握手流程」第 6 条），不再占用 `WsEvent` 变体。

> **worker-pool 事件说明**：5 个 `WORKER_*` 事件均在 HTTP commit 之后广播（对齐 Python 延迟广播模式，参见 [`docs/architecture.md` §3.7](../architecture.md)）；payload 完整定义见 [`./parts/inspection.md#post-apiv2prodbatchesworker-scan`](./parts/inspection.md#post-apiv2prodbatchesworker-scan) 与 [`./production/worker-pool.md`](./production/worker-pool.md)。
>
> **batch 事件说明（2026-09-29 新增）**：`BATCH_PLACED_ON_SHELF` 同样在 HTTP commit 之后广播（沿 worker_pool 范本）；payload 含 4 个字段（batch_id / target_process_id / shelf_id / version）。单条 dispatch 端点发单条形态（payload 顶层字段）；bulk-dispatch / auto-dispatch 端点发批量形态（payload.batches 数组，仅含 succeeded 部分，skipped 不广播）。详见 [`./production/batches.md#ws-事件`](./production/batches.md#ws-事件)。
>
> i64 字段在 WS payload 中序列化为字符串（与 HTTP `R<T>` 一致）。
>
> **`PART_BATCH_SPLIT` 双 payload（2026-10-03 订正）**：同一事件名有**两种** payload，
> 消费方按「哪些 key 存在」分支，不要假设字段集固定：
>
> | 触发端点 | payload | 何时发 |
> |---|---|---|
> | `POST /api/v2/prod/batches/{batch_id}/split` | `{ part_id, new_batch_id }` | 总是（手动拆批） |
> | `POST /api/v2/prod/batches/{batch_id}/pick-up` | `{ part_id, new_batch_id, source_batch_id, quantity }` | **仅部分领取**（`0 < quantity < batch.quantity` 自动拆批时）；整批领取不发本事件 |
>
> 两种 payload **共用** `part_id` / `new_batch_id` 两个字段名（这是消费方唯一可以
> 无条件依赖的部分），后两个字段是 pick-up 侧的增量：`source_batch_id` = 被扣减的
> 源批次（= URL 里的 `batch_id`）、`quantity` = 拆走量。
>
> ⚠️ **2026-10-03 订正**：本事件**从来不发 `batch_id` 字段**（旧版本表误记为
> `part_id` / `batch_id` / `new_batch_id`）——源批次走 `source_batch_id` 且只在
> pick-up 侧有值；`split` 端点的源批次就是 URL 里的 `batch_id`，未冗余重发。
>
> ⚠️ **消费方注意（源批次 OCC 已过期）**：部分领取会在同一事务里对源批次做
> `quantity -= q` **且 `version += 1`**（`PartBatchRepo::_split_batch_inner`），所以
> 收到本事件后**源批次的乐观锁版本已失效**，必须重新拉取列表拿新的
> `batch_version` 再发下一次写请求。本事件不携带 `source_batch_version`。

### `ASSEMBLY_UPDATED`

- **Payload**: `{ "assembly_id": "<stringified i64>" }`
- **触发端点**:
 - `POST /api/v2/assemblies/{id}/update`（前端主动改字段）
 - `POST /api/v2/prod/batches/{batch_id}/to-inspection` / `to-ship` / `to-process`
 - `POST /api/v2/prod/batches/to-inspection` / `to-ship`
 - `POST /api/v2/prod/batches/worker-scan`（仅 `INSPECTED` 分支，**实际翻状态时**才下发；dedup by assembly_id）

 > **2026-10-02**：上列触发端点自 part 域迁入 prod 域（锚点 `part_id` → `batch_id`）。事件 `kind` 字符串不变。
- **频率**：每个 inspection 调用最多 1 次（per unique parent assembly）。

### `ROLLUP_RECOMPUTED`

- **Payload**: `{ "scope": "ALL|PART_IDS|ASSEMBLY_IDS|PART_IDS+ASSEMBLY_IDS", "parts_changed": <int>, "assemblies_changed": <int>, "operator_id": "<stringified i64>" }`
- **触发端点**: `POST /api/v2/admin/recompute-rollup`（Manager 单角色；见 [`./admin.md`](./admin.md)）
- **时机**: 全部分块事务 commit **之后**广播一次（该端点内部按 200 行分块提交，故这是**汇总**事件，不是逐行事件）
- **频率**: 每次调用最多 1 次；**0 变化时不发**（幂等空跑不该刷新大屏）

