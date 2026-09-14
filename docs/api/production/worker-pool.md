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
| GET | `/api/v2/worker-pool/state` | 已登录（无 role guard） | worker 当前持有（含完整 held_batches）+ 工序池候选数（按工序分组） |
| GET | `/api/v2/worker-pool/{process_id}` | **Manager+Clerk+Inspector** | 按工序返回候选池详情（workers + work_types + 跨货架批次列表） |
| POST | `/api/v2/admin/worker-pool/refill` | **Manager** | 为指定 worker 抢满 `max_held_batches`（同事务） |
| POST | `/api/v2/admin/worker-pool/remove` | **Manager** | 把 worker 持有批次按 RETURNED 语义放回候选池 |
| POST | `/api/v2/admin/worker-pool/auto-allocate` | **Manager** | 按 process + shelf 自动为多个 worker 抢批次数 / 累计工时（COUNT/TIME 模式 × fill_ratio） |
| POST | `/api/v2/admin/worker-pool/assign` | **Manager** | 单 batch 拖拽分配（不循环触顶 max_held；用于 UI 单 batch 拖拽场景） |

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

### `GET /api/v2/worker-pool/{process_id}`

权限：**Manager + Clerk + Inspector**（`current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])` —— admin 视角但不止 Manager；不指定 shelf_id，返回所有货架）。

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | i64 (snowflake) | ✓ | 工序雪花 ID |

业务流转（service `pool_by_process`）：

1. 取 process 元数据（`code / name`）；不存在 → `20801 BIZ_PROCESS_NOT_FOUND`
2. 取该 process 映射的所有 work_type（`WorkTypeRepo::list_by_process_id`）：返回 `Vec<WorkTypeMaxHeld>`
3. 取该 process 可执行的所有 worker（DISTINCT worker）：返回 `Vec<WorkerBrief>`
4. 取所有货架上的候选批次（`WorkerPoolRepo::list_candidates_by_process_all_shelves`）：JOIN t_part_batch + t_part + t_customer L2 + t_customer L1 + t_shelf，单 SQL，排序 `system_delivery_date ASC NULLS LAST → is_urgent DESC → id ASC`，无 LIMIT（admin 视图）
5. 装 `ProcessPoolDetail` 返回

Response 200 `data`：[`ProcessPoolDetail`](#processpooldetail-字段)

错误码：

- 20801 BIZ_PROCESS_NOT_FOUND — process_id 不存在 / 已软删
- 40300 FORBIDDEN — 角色不在 Manager+Clerk+Inspector 集合内

### `POST /api/v2/admin/worker-pool/auto-allocate`

权限: **Manager**（`current.require_role(Role::Manager)`）

按 `process_id + shelf_id` 范围为该 process 上的每个 active worker 计算 `target`，循环 `take_one_from_pool` 抢到 target / 池空。

Request：`AutoAllocateRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | string (i64) | ✓ | 候选池工序 ID（决定 work_type 映射范围） |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID（决定批次范围） |
| `mode` | string | ✓ | `COUNT`（按批次数）或 `TIME`（按累计预估工时） |
| `fill_ratio` | f64 | ✓ | 填充比例 ∈ `[0.0, 1.0]`；out-of-range → 20704 |

业务流转（service `auto_allocate_for_process`）：

1. 校验 `fill_ratio ∈ [0.0, 1.0]` → `20704 BIZ_AUTO_ALLOCATE_INVALID_RATIO` (HTTP 400)
2. 取 process 元数据；不存在 → `20801 BIZ_PROCESS_NOT_FOUND` (HTTP 404)
3. 取 process 映射的 work_type 列表（含 `max_held_batches` + 二次查 `max_held_minutes`）；空 → `20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING`
4. 取 `shelf_id` 上 active worker 列表（`WorkerRepo::list_active_by_process_id`）
5. 对每个 worker：
   - 找到其所属 work_type 的 max 阈值：
     - `COUNT`：`max_held_batches`；NULL → `20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET`
     - `TIME`：`max_held_minutes`；NULL → `20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET`
   - 计算 `target = ceil(max × fill_ratio)` as i32
   - 循环 `WorkerPoolRepo::take_one_from_pool` 直到 target 满 / 池空
   - 每抢到一批：写 `TAKEN_FROM_POOL` 事件 + 调 `PartService::sync_from_batch_change` rollup
6. 累计所有 worker 的 filled，组装 `AutoAllocateResult` 返回

Response 200 `data`：[`AutoAllocateResult`](#autoallocateresult-字段)

错误码：

- 20801 BIZ_PROCESS_NOT_FOUND — process_id 不存在 / 已软删
- 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING — process 无 work_type 映射
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — COUNT 模式但 work_type.max_held_batches IS NULL
- 20703 BIZ_WORK_TYPE_MAX_HELD_MINUTES_NOT_SET — TIME 模式但 work_type.max_held_minutes IS NULL
- 20704 BIZ_AUTO_ALLOCATE_INVALID_RATIO — fill_ratio ∉ [0.0, 1.0]
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL（防御性，正常流不撞）
- 40300 FORBIDDEN — 非 Manager
- 40001 VALIDATION_ERROR — payload shape 错误

WS 广播（commit 后下发）：

- 始终 → `WORKER_POOL_AUTO_ALLOCATE_DONE`（payload = `AutoAllocateResult`，前端按 `pool_empty + filled` 综合判断）

### `POST /api/v2/admin/worker-pool/assign`

权限: **Manager**

Request：`AdminAssignRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | 目标 worker（`deserialize_i64`） |
| `batch_id` | string (i64) | ✓ | 要分配的批次（必须位于候选池中：`status=IN_PROCESS AND location=PRODUCTION_SHELF`） |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID（`current_holder_id` 必须等于） |
| `process_id` | string (i64)? | ✗ | 可选；提供时校验 `batch.next_process_id` 必须匹配（防止对未排到该工序的批做 assign） |

业务流转（service `assign_batch_to_worker`）：

1. 角色守卫：Manager（service 内 `require_role`）
2. 校验 worker：`is_active=false` → `20202 BIZ_WORKER_INACTIVE`；`work_type_id IS NULL` → `20206 BIZ_WORKER_NO_WORK_TYPE`
3. 取 work_type；`max_held_batches IS NULL` → `20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET`
4. 取 worker 当前持有批次数 `current_held`，若 ≥ `max_held_batches` → `20204 BIZ_WORKER_HOLD_LIMIT_EXCEEDED`（assign 路径仍守 max 上限，不循环触顶）
5. 若 `process_id` 提供：校验 `batch.next_process_id == process_id`，否则 → `20104 BIZ_INVALID_VALUE`
6. `WorkerPoolRepo::take_specific_from_pool`：单 SQL 限定 `(shelf_id, batch_id)` 原子切换 holder；找不到 → `20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER`（语义复用："不是 worker 可领取的批次"）
7. `PartService::sync_from_batch_change` 同步 part 派生列（PR-B2）
8. 写 `TAKEN_FROM_POOL` 事件日志（`note="admin_assign"` 区分 refill 来源）
9. 返回 `AssignResult`

Response 200 `data`：[`AssignResult`](#assignresult-字段)

错误码：

- 20201 BIZ_WORKER_NOT_FOUND — worker 不存在
- 20202 BIZ_WORKER_INACTIVE — worker 已停用
- 20204 BIZ_WORKER_HOLD_LIMIT_EXCEEDED — worker 持有数已达 max_held_batches 上限
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL
- 20901 BIZ_WORK_TYPE_NOT_FOUND — work_type 不存在（防御性）
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — work_type.max_held_batches 未设置
- 20109 BIZ_PART_BATCH_NOT_FOUND — 提供 process_id 时 batch 不存在
- 20104 BIZ_INVALID_VALUE — 提供 process_id 时 batch.next_process_id 与之不匹配
- 20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER — batch 不在候选池（status/location/holder 不符或已软删）
- 40300 FORBIDDEN — 非 Manager
- 40001 VALIDATION_ERROR — payload shape 错误

WS 广播（commit 后下发）：

- 始终 → `WORKER_POOL_ASSIGN_DONE`（payload = `AssignResult`）

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
| `held_batches` | [TakenItem](#takenitem-字段)[] | **2026-09-14 follow-up-ux 新增**：worker 当前持有的完整 batch 列表（JOIN t_part），按 `t_part_batch.id ASC` 排序。避免前端按 worker 轮询 K 次单 batch 详情接口的 N+1；UI sink `WorkerQueueBoard.vue` 已对接 `:batches="workerHeld[w.id] ?? []"` |

### PoolBatchItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `part_id` | string (i64) | 工单雪花 ID |
| `batch_no` | i32 | 批次序号（同一 part 下从 1 开始） |
| `quantity` | i32 | 批次数量 |
| `serial_no` | string? | 工单序列号（手工工单可空） |
| `name` | string | 工单 / 零件名称（源自 t_part） |
| `drawing_no` | string | 图号 |
| `system_delivery_date` | date? | 系统交付日期 |
| `customer_name` | string? | L2 客户名（叶子） |
| `parent_customer_name` | string? | L1 客户名（一级集团），L2.parent_id 为空时为 None |
| `customer_path` | string? | `"L1 / L2"` 路径；L1 自指仅给 leaf 名 |
| `applicant_name` | string? | 申请人字符串列（t_part.applicant_name，非 FK） |
| `location` | string | 候选池当前货架 raw enum（如 `"PRODUCTION_SHELF"`） |
| `shelf_id` | string (i64) | 当前货架 id（t_part_batch.current_holder_id） |
| `shelf_code` | string | 当前货架代号 |
| `shelf_name` | string | 当前货架名 |
| `is_urgent` | bool | 是否加急（取自 t_part.is_urgent） |
| `note` | string? | 工单级备注（t_part.note，DB 无 batch 级 remark 字段；复用） |
| `placed_at` | datetime | 批次上架时间（t_part_batch.placed_at）—— 用于前端展示「积压多久」 |
| `version` | i32 | 乐观锁 |

### WorkerBrief 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |
| `name` | string | 工人姓名 |
| `work_type_id` | string (i64) | 工种雪花 ID |
| `work_type_code` | string | 工种代号 |

### WorkTypeMaxHeld 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `work_type_id` | string (i64) | 工种雪花 ID |
| `work_type_code` | string | 工种代号 |
| `work_type_name` | string | 工种名 |
| `max_held_batches` | i32? | 工种最大持有批次数；None = 未设置（与既有 20904 `BIZ_WORK_TYPE_MAX_HELD_NOT_SET` 同语义，但不在此处报错） |

### ProcessPoolDetail 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 工序雪花 ID |
| `process_code` | string | 工序代号 |
| `process_name` | string | 工序名 |
| `workers` | [WorkerBrief](#workerbrief-字段) | 可执行该工序的工人列表（同一工人可能因所属工种映射该工序而出现多次） |
| `work_types` | [WorkTypeMaxHeld](#worktypemaxheld-字段) | 该工序映射到的工种 + max_held（按 work_type 分组） |
| `total` | i64 | 候选批次总数（与 items.len() 一致，不分页；admin 视角全量） |
| `items` | [PoolBatchItem](#poolbatchitem-字段) | 跨货架候选批次列表，排序 `system_delivery_date ASC NULLS LAST → is_urgent DESC → id ASC` |

### AutoAllocateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | string (i64) | ✓ | 候选池工序 ID |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID |
| `mode` | `AutoAllocateMode` | ✓ | `COUNT` 或 `TIME`（Rust enum，JSON 形态 `UPPERCASE`） |
| `fill_ratio` | f64 | ✓ | 填充比例 ∈ `[0.0, 1.0]` |

### AutoAllocateResult 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 入参工序 ID |
| `shelf_id` | string (i64) | 入参货架 ID |
| `mode` | `AutoAllocateMode` | 入参模式 |
| `fill_ratio` | f64 | 入参比例 |
| `filled` | [WorkerFillItem](#workerfillitem-字段) | 各 worker 的填充结果（按 process 上的 worker 列表顺序） |
| `pool_empty` | bool | 任一 worker 的 take 循环中途遇 `None`（池空 / 容量触顶）；前端按 `pool_empty + filled` 综合判断 |

### WorkerFillItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |
| `target` | i32 | 该 worker 的目标：COUNT 模式 = 抢批次数；TIME 模式 = 累计分钟数 |
| `filled_count` | i32 | 实际抢到的批次 / 累计分钟数（按 `mode` 解释，与 `target` 同单位） |
| `skipped_reason` | string? | 跳过原因（如 `worker 无 work_type`）；存在字段 ⇒ 跳过该 worker |

### AdminAssignRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | `deserialize_i64` |
| `batch_id` | string (i64) | ✓ | `deserialize_i64` |
| `shelf_id` | string (i64) | ✓ | `deserialize_i64` |
| `process_id` | string (i64)? | ✗ | `deserialize_i64_opt`；提供时校验 `batch.next_process_id` 必须匹配 |

### AssignResult 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID（透传入参） |
| `batch_id` | string (i64) | 批次雪花 ID（透传入参） |
| `shelf_id` | string (i64) | 货架雪花 ID（透传入参） |
| `taken` | [TakenItem](#takenitem-字段) | 新持有的批次（JOIN t_part 元数据） |
| `current_held` | i32 | 分配后 worker 持有数（含本批次） |
| `max_held` | i32 | 分配后 worker 工种的 `max_held_batches` |

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
| `WORKER_POOL_AUTO_ALLOCATE_DONE` | `POST /admin/worker-pool/auto-allocate` | `{ process_id, shelf_id, mode, fill_ratio, filled: [WorkerFillItem], pool_empty }`（即 `AutoAllocateResult`） |
| `WORKER_POOL_ASSIGN_DONE` | `POST /admin/worker-pool/assign` | `{ worker_id, batch_id, shelf_id, taken: TakenItem, current_held, max_held }`（即 `AssignResult`） |

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
- ✅ 2026-09-11 part-worker-pool-federated-rocket：新增 `auto_allocate_for_process` + 端点 `POST /admin/worker-pool/auto-allocate` + COUNT/TIME 模式 + fill_ratio 校验（20704）；错误码段 20701/20702/20703/20704
- ✅ 2026-09-14 follow-up-ux：`WorkerPoolState` 新增 `held_batches` 字段（`list_held_by_worker_with_part` JOIN t_part 取全量）+ 新增 `POST /admin/worker-pool/assign` 端点（单 batch 拖拽分配，service `assign_batch_to_worker`）+ `WorkerPoolRepo::take_specific_from_pool`（单 SQL 限定 `(shelf_id, batch_id)` 原子切换 holder）；错误码沿用既有 20204 / 20114 / 20104
- ⏳ 未上线：`WorkerRepo` 列表 / 创建 / 软删等 CRUD（worker 域当前仅供 worker_pool / parts worker-scan 复用）

## 参考

- 集成测试：`tests/worker_pool_api.rs` / `tests/worker_pool_auto_allocate_api.rs`
- 仓库分层：`src/modules/worker_pool/handler.rs` (axum) → `service.rs` (业务) → `repo.rs` (SQL)
- 错误码：`src/shared/error.rs::code`（20104 / 20109 / 20114 / 20201 / 20202 / 20204 / 20206 / 20901 / 20904 / 20905 / 20703 / 20704 / 40001 / 40300 / 40901）