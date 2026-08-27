# worker_pool 域 API

> 本文件须与 `src/modules/worker_pool/{handler.rs,dto.rs,service.rs,model.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：工人扫码台（worker-scan）配套的工序候选池管理：
> - `GET /state` —— 工人当前持有数 + 各工序候选池计数（前端轮询用）
> - `POST /admin/.../refill` —— Manager 主动触发「为某 worker 抢满 max_held」
> - `POST /admin/.../remove` —— Manager 把 worker 持有批次按 RETURNED 语义放回池
>
> worker-scan 主入口 `POST /api/v2/parts/worker-scan` 见 [`./parts/index.md`](./parts/index.md)；worker-scan 成功后**同事务**触发 `refill_for_worker`，见 §WS 广播。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/worker-pool/state` | 已登录（无 role guard） | worker 当前持有 + 工序池候选数（按工序分组） |
| POST | `/api/v2/admin/worker-pool/refill` | **Manager** | 为指定 worker 抢满 `max_held_batches`（同事务） |
| POST | `/api/v2/admin/worker-pool/remove` | **Manager** | 把 worker 持有批次按 RETURNED 语义放回候选池 |

> 路由挂载：`/worker-pool/state` 走 `/api/v2/worker-pool`，admin 端点走 `/api/v2/admin/worker-pool`（见 `src/modules/worker_pool/mod.rs`）。

---

### `GET /api/v2/worker-pool/state`

权限: 已登录（**无 role guard** —— worker 自查 + admin 监控共用；admin 监控可传任意 `worker_id`）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | 工人雪花 ID |
| `shelf_id` | string (i64) | ✓ | 工人所在货架 ID（决定候选池范围） |

Response 200 `data`：[`WorkerPoolState`](#workerpoolstate-字段)

错误码：

- 20201 BIZ_WORKER_NOT_FOUND — worker 不存在 / 已软删

> 端点不要求 worker.work_type_id 已设置；`max_held` 退化为 0、`process_ids` 为空、`capacity_remaining=0`、`pool_count_by_process=[]`（前端应展示「工种未设置」占位）。

### `POST /api/v2/admin/worker-pool/refill`

权限: **Manager**

Request：`AdminRefillRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | 工人雪花 ID（`deserialize_i64` 反序列化） |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID |

业务流转（service `refill_for_worker`）：

1. 取 worker（带 `work_type_id`）；`is_active=false` → `20202 BIZ_WORKER_INACTIVE`；`work_type_id IS NULL` → `20206 BIZ_WORKER_NO_WORK_TYPE`
2. 取 work_type；`max_held_batches IS NULL` → `20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET`
3. 取工种可加工工序 id 列表；空 → `20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING`
4. 循环调 `WorkerPoolRepo::take_one_from_pool`，每抢到一批写 `TAKEN_FROM_POOL` 事件日志
5. 池空 / 容量触顶时 `take_one_from_pool` 返回 `Ok(None)`，本方法跳出循环（业务层不区分二者）

Response 200 `data`：[`RefillResult`](#refillresult-字段)

错误码：

- 20201 BIZ_WORKER_NOT_FOUND — worker 不存在
- 20202 BIZ_WORKER_INACTIVE — worker 已停用
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL
- 20901 BIZ_WORK_TYPE_NOT_FOUND — work_type 不存在（防御性，正常流不该撞）
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — work_type.max_held_batches 未设置
- 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING — work_type 未映射工序
- 40300 FORBIDDEN — 非 Manager
- 40001 VALIDATION_ERROR — payload shape 错误

WS 广播（commit 后下发）：

- `taken.len() > 0` → `WORKER_POOL_REFILL_DONE`（payload = `RefillResult`）
- `pool_empty=true` 且 `taken=[]` → `WORKER_POOL_EMPTY`（payload `{ worker_id, shelf_id }`）

### `POST /api/v2/admin/worker-pool/remove`

权限: **Manager**

Request：`AdminRemoveRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | 工人雪花 ID |
| `batch_id` | string (i64) | ✓ | 要放回的批次 ID（必须是该 worker 当前持有） |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID（放回的目标） |
| `next_process_id` | string (i64) | ✓ | 下一道工序 ID（与 shelf 映射） |

业务流转（service `admin_remove_held_batch`）：

1. 取 worker（事件日志 `badge_code` 需要）
2. 按 `(batch_id, holder_id = worker_id)` 找 IN_PROCESS+WORKER 批次；找不到 → `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`
3. `mark_batch_returned` + `mark_part_returned`（OCC，version 冲突 → `40901`）
4. 写 `ADMIN_REMOVED_FROM_WORKER` 事件日志
5. 返回 `TakenItem`（`version = batch.version + 1`）

> shelf+next_process 由 admin 在 req 里显式指定（不校验 shelf 是否映射该 process —— 若 shelf 不映射，下一次 worker refill 自然拿不到，由 service 业务错时处理）。

Response 200 `data`：[`TakenItem`](#takenitem-字段)

错误码：

- 20201 BIZ_WORKER_NOT_FOUND — worker 不存在
- 20101 BIZ_PART_NOT_FOUND — batch 关联的 part 不存在（防御性，正常流不该撞）
- 20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER — `(worker, batch)` 不在 IN_PROCESS+WORKER 持有中
- 40300 FORBIDDEN — 非 Manager
- 40001 VALIDATION_ERROR — payload shape 错误
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_POOL_ADMIN_REMOVED`（payload = `TakenItem`）

---

## 共享 DTO

### AdminRefillRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `shelf_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |

### AdminRemoveRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `batch_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `shelf_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `next_process_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |

### TakenItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `part_id` | string (i64) | 工单雪花 ID |
| `batch_no` | i32 | 批次序号（同一 part 下从 1 开始） |
| `quantity` | i32 | 批次数量 |
| `serial_no` | string? | 工单序列号 |
| `drawing_no` | string | 图号 |
| `system_delivery_date` | date? | 系统交付日期 |
| `planned_delivery_date` | date? | 计划交付日期 |
| `is_urgent` | bool | 是否加急 |
| `version` | i32 | 乐观锁（admin_remove 返回 `batch.version + 1`） |

### RefillResult 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |
| `shelf_id` | string (i64) | 货架雪花 ID |
| `taken` | [TakenItem](#takenitem-字段) | 本次抢到的批次（`length ≤ work_type.max_held_batches`） |
| `pool_empty` | bool | 是否池空；`taken.len() == max_held_batches` 时也可能 `false`（池恰好满足），前端须按 `pool_empty + taken.len()` 综合判断 |

> `pool_empty=true && taken=[]` —— 池空且 worker 持有为 0（未抢到任何批次）
> `pool_empty=false && taken.len() < max_held_batches` —— 候选池已耗尽，未触顶
> `pool_empty=false && taken.len() == max_held_batches` —— 候选池仍有剩余但 worker 已满

### ProcessPoolCount 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 工序雪花 ID |
| `pool_count` | i64 | 该工序在 shelf 上的候选批次数（`t_part_batch` 中 `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND current_holder_id = shelf_id AND next_process_id = process_id`） |

### WorkerPoolState 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |
| `worker_name` | string | 工人姓名 |
| `work_type_code` | string | 工种代号（worker.work_type_id IS NULL 时为空串） |
| `max_held` | i32 | `work_type.max_held_batches`（未设置时为 0） |
| `current_held` | i64 | worker 当前持有批次数（`t_part_batch` 中 `status='IN_PROCESS' AND location='WORKER' AND current_holder_id = worker_id`） |
| `capacity_remaining` | i32 | `max(0, max_held - current_held)` |
| `pool_count_by_process` | [ProcessPoolCount](#processpoolcount-字段) | 各工序候选池计数（仅含 work_type 映射到的工序） |

---

## WS 事件清单（worker-pool 相关）

> 全部走 `WsEvent::DashboardEvent { kind, payload }`，payload 字段如下：
> 详见 [`./websocket.md`](./websocket.md)

| kind | 触发端点 | payload 关键字段 |
|---|---|---|
| `WORKER_SCAN_RETURNED` | `POST /parts/worker-scan`（event_type=RETURNED） | `{ worker_id, part_id, batch_id, event_type }`（即 `WorkerScanCoreOut`） |
| `WORKER_SCAN_INSPECTED` | `POST /parts/worker-scan`（event_type=INSPECTED） | `{ worker_id, part_id, batch_id, event_type }`（即 `WorkerScanCoreOut`） |
| `WORKER_POOL_REFILL_DONE` | `POST /parts/worker-scan` 同事务 refill 抢到 / `POST /admin/worker-pool/refill` | `{ worker_id, shelf_id, taken: [TakenItem], pool_empty }`（即 `RefillResult`） |
| `WORKER_POOL_EMPTY` | `POST /parts/worker-scan` 同事务 refill 池空 / `POST /admin/worker-pool/refill` 池空 | `{ worker_id, shelf_id }` |
| `WORKER_POOL_ADMIN_REMOVED` | `POST /admin/worker-pool/remove` | `{ batch_id, part_id, batch_no, quantity, serial_no, drawing_no, system_delivery_date, planned_delivery_date, is_urgent, version }`（即 `TakenItem`） |

> 监听实现：`src/infra/ws_hub.rs::WsHub::broadcast`。前端订阅 `/ws/dashboard` 后按 `kind` 字段分发。

---

## 端点约束（与 Python 一致）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → `40901 VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → `20201 BIZ_WORKER_NOT_FOUND`
- **事务边界在 handler**：handler `state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；repo 用 `impl PgExecutor<'_>` 以同时接受 pool/conn/tx
- **WS 广播在 commit 之后**：避免慢 WS 拖慢 HTTP 响应

## 实施状态（worker-pool-take 分支）

- ✅ Task 4：基础错误码（20205/20206/20114）+ `worker.repo::get_by_badge_code` + `part_batch.count_held_by_worker`
- ✅ Task 6：`worker_pool.repo::take_one_from_pool` CTE（FOR UPDATE SKIP LOCKED）
- ✅ Task 7：`worker_pool.service`（refill_for_worker / compute_state / admin_remove_held_batch）+ handler 三端点 + admin router
- ✅ Task 8：`POST /parts/worker-scan`（同事务联动 refill）
- ⏳ 未上线：`WorkerRepo` 列表 / 创建 / 软删等 CRUD（worker 域当前仅供 worker_pool / parts worker-scan 复用）

## 参考

- 集成测试：`tests/worker_pool_api.rs`（如已添加）/ `tests/part_worker_scan_api.rs`（如已添加）
- 仓库分层：`src/modules/worker_pool/handler.rs` (axum) → `service.rs` (业务) → `repo.rs` (SQL)
- 错误码：`src/shared/error.rs::code`（20101 / 20114 / 20201 / 20202 / 20206 / 20901 / 20904 / 20905 / 40001 / 40300 / 40901）