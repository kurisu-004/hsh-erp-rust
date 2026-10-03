# pool 域 API（原 worker-pool，2026-09-30 重构路径收敛）

> 本文件须与 `src/modules/prod/worker_pool/{handler.rs,dto.rs,service.rs,model.rs,vo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：工人扫码台（worker-scan）配套的工序候选池管理：
> - `GET /state` —— 工人当前持有数 + 各工序候选池计数（前端轮询用）
> - `POST /refill` —— Manager 主动触发「为某 worker 抢满 max_held」
> - `POST /move` —— 通用移动端点（POOL ↔ WORKER + WORKER ↔ WORKER 三方向，取代旧 `assign` / `remove`）
> - `POST /auto-allocate` —— Manager 按 process + shelf 自动为多个 worker 抢批次/工时
>
> 2026-09-30 重构：worker-pool → pool 路径收敛，原 6 个端点归并为 5 个挂 `/api/v2/prod/pool/*`：
> - 旧 `/admin/worker-pool/{remove, assign}` → `/pool/move`（统一通用移动端点）
> - 旧 `/worker-pool/{state, counts, {process_id}}` + `/admin/worker-pool/{refill, auto-allocate}` → `/pool/{state, counts, {process_id}, refill, auto-allocate}`
>
> worker-scan 主入口 `POST /api/v2/prod/batches/worker-scan` 见 [`./batches.md`](./batches.md#2026-10-02-t_part_batch-子资源迁入)；worker-scan 成功后**同事务**触发 `refill_for_worker`，见 §WS 广播。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/pool/state` | 已登录（无 role guard） | worker 当前持有（含完整 held_batches）+ 工序池候选数（按工序分组） |
| GET | `/api/v2/prod/pool/counts` | **Manager+Clerk+Inspector** | **2026-09-30 新增**：全工序候选批次聚合计数（GROUP BY process_id），跨所有货架 |
| GET | `/api/v2/prod/pool/{process_id}` | **Manager+Clerk+Inspector** | 按工序返回候选池详情（workers + work_types + 跨货架批次列表） |
| POST | `/api/v2/prod/pool/refill` | **Manager** | 为指定 worker 抢满 `max_held_batches`（同事务） |
| POST | `/api/v2/prod/pool/move` | **Manager** | **2026-09-30 新增**：通用移动端点（POOL ↔ WORKER + WORKER ↔ WORKER 三方向），取代旧 `assign` + `remove` |
| POST | `/api/v2/prod/pool/auto-allocate` | **Manager** | 按 process + shelf 自动为多个 worker 抢批次数 / 累计工时（COUNT/TIME 模式 × fill_ratio） |

> 路由挂载：`pool/*` 全部挂 `/api/v2/prod/pool`（见 `src/modules/prod/worker_pool/mod.rs`）。
>
> 旧 `/api/v2/prod/worker-pool/*` 与 `/api/v2/prod/admin/worker-pool/*` 路径 404（router 层不再挂载）。

---

### `GET /api/v2/prod/pool/state`

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

### `POST /api/v2/prod/pool/refill`

权限: **Manager**

Request：`AdminRefillRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | 工人雪花 ID（`deserialize_i64` 反序列化） |
| `shelf_id` | string (i64) | ✓ | 候选池货架 ID |

#### 自动分配优先级（2026-09-29 新增）

`take_one_from_pool` SQL `ORDER BY` 第 1 键为 **`has_cnc_program DESC`**
（与候选池视图 `PoolBatchItem.has_cnc_program` 同源 EXISTS 子查询）；后续键位：
`system_delivery_date ASC NULLS LAST → planned_delivery_date ASC NULLS LAST →
is_urgent DESC → id ASC`。

业务意图：同交期同加急时优先 take 已上传 G_CODE 的 batch，省
"工人拿到手 → 还要等编程员传程序"的等待。

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

### `POST /api/v2/prod/pool/move`（2026-09-30 新增）

权限: **Manager**

Request：`MoveRequest`

```jsonc
{
  "batch_id": "123",
  "from": { "kind": "POOL",   "shelf_id": "100" },     // 当前位置
  "to":   { "kind": "WORKER", "worker_id": "50" },     // 目标位置
  "note": "退换料"                                       // 可选
}
```

`from` / `to` 为 tagged enum（`#[serde(tag = "kind", rename_all = "UPPERCASE")]`）：

| `kind` | 必填字段 | 说明 |
|---|---|---|
| `POOL`   | `shelf_id` (i64) | 候选池位置 |
| `WORKER` | `worker_id` (i64) | 工人持有位置 |

**支持的 4 个方向**（POOL→POOL 非法，返回 `40001 VALIDATION_ERROR`）：

| from       | to         | 旧端点（2026-09-30 前） | SQL（位于 `src/modules/prod/worker_pool/repo/sql.rs`） |
|------------|------------|------|------|
| `POOL`     | `WORKER`   | `admin/worker-pool/assign` | `take_specific_from_pool`（OCC） |
| `WORKER`   | `POOL`     | `admin/worker-pool/remove` | `part_mark_batch_returned`（**2026-09-30 重构去掉 step 写入**；`advance_to_process_id` 恒传 `None` = 不推进） |
| `WORKER`   | `WORKER`   | （新增） | `move_worker_to_worker`（**2026-09-30 新增，不写 step**） |
| `POOL`     | `POOL`     | （非法） | — |

#### 关键不变量（保证工序链不被破坏）

- 所有 **move** SQL **不写** `current_process_step_id`（move 不推进工序链）
- 所有 **move** SQL 同样 **不写** `current_process_id`（2026-09-30 写入不变式：池内移动工序不变，批次归还货架后仍属原工序候选池）
  - 实现：`part_mark_batch_returned` 的 `advance_to_process_id` 形参**恒传 `None`**，
    SQL 侧 `current_process_id = COALESCE($5::bigint, current_process_id)` 保留原值
- 批次必须 `status='IN_PROCESS'` 且 `deleted_at IS NULL`；否则 `20120 BIZ_BATCH_INVALID_STATUS`
- OCC：`UPDATE ... WHERE version = $expected`；0 行 → `40901 VERSION_CONFLICT`
- `from` 必与 batch 当前 `(location, current_holder_id)` 匹配：
  - POOL → `(location='PRODUCTION_SHELF', current_holder_id=shelf_id)`
  - WORKER → `(location='WORKER', current_holder_id=worker_id)`
  - 不匹配 → `20122 BIZ_BATCH_LOCATION_MISMATCH`（**新增**，HTTP 409）

> #### ⚠️ 2026-09-30 两处行为变化
>
> **(1) move 的 `to` 校验从「静默跳过」变为「强制执行」**（breaking behavior）
>
> `move_batch` 的工序资格来源已从 `current_process_step_id` → step JOIN 改为直读
> `current_process_id`。此前 dispatch 下发的批次 `current_process_step_id=NULL`
> → 工序取值为 `None` → 下表两条校验被 `if let Some(spid)` **整块跳过**：
>
> | 校验 | 此前（对 dispatch 批次） | 现在 |
> |---|---|---|
> | `to.kind=WORKER` 的「工种必须含 batch 当前工序」 | 跳过 | **执行** → 不含则 `20104 BIZ_INVALID_VALUE` |
> | `to.kind=POOL` 的「货架必须映射 batch 当前工序」 | 跳过 | **执行** → 未映射则 `20507 BIZ_SHELF_PROCESS_NOT_MAPPED` |
>
> 这是**修正漏检**（原本应校验而未校验），但既有前端流程可能因此开始收到上述两个错误码。

> **(2) worker-scan RETURNED 现在会推进 `current_process_id`**
>
> `part/service/worker_scan.rs` 的 RETURNED 事件是全仓唯一的**工序推进**路径。
> 它此前不写 `current_process_id`，导致工人在 P1 完工、扫 RETURNED 传
> `next_process_id=P2` 后，批次归还货架仍带 P1 → **落回 P1 池而非 P2 池**。
> 现已传 `advance_to_process_id = Some(next_process_id)`，批次正确落进 P2 池。
>
> 注意：RETURNED **仍不推进** `current_process_step_id`（该列的 step SET 子句在
> 2026-09-30 prod/pool move 重构中被移除，RETURNED 复用了同一函数）。这是**已知缺口**，
> 影响面仅限显示（`current_process_step_id` 不更新），池归属不受影响。后续单独一轮处理。
>
> ⚠️ 措辞（2026-09-30 附带发现）：`current_process_step_id` **不是**
> 「会随流转推进的进度指针」—— 它只在**首次定位**工序时写、之后一律不再推进
> （worker-scan 两条分支都不写），对多工序链工单永远停在首次定位那一步。


#### `to` 校验

| `to.kind` | 校验 |
|---|---|
| `POOL`   | `t_shelf_process WHERE shelf_id = $x AND process_id = $batch.current_process_id` 必须 ≥1 条（货架必须映射到 batch 当前工序；**2026-09-30 改直读 `current_process_id`**，原先是 `current_process_step_id` → step JOIN） |
| `WORKER` | worker 必须 `is_active=true`；worker 的工种必须含 batch 当前工序；`held < work_type.max_held_batches`（容量上限） |

业务流转（service `move_batch`）：

1. 角色守卫：Manager（service 内 `require_role`）
2. **POOL→POOL 同 kind 移动 → 40001 VALIDATION_ERROR**（仅 POOL→POOL 非法；WORKER→WORKER 同 src/dst 也抛 40001）
3. 取 batch（`include_deleted=false`）；不存在 → `20121 BIZ_BATCH_NOT_FOUND`
4. 校验 `status='IN_PROCESS'` → 否则 `20120 BIZ_BATCH_INVALID_STATUS`
5. 校验 `from` 与 batch 当前 `(location, holder_id)` 一致 → 否则 `20122 BIZ_BATCH_LOCATION_MISMATCH`
6. 按 (from, to) 选 SQL 分支（见上表）+ `to` 校验（worker 资格 / 容量 / shelf 映射）
7. 写 `MOVED` 事件日志（note 含 `move POOL→WORKER` / `WORKER→POOL` / `WORKER→WORKER` 或 caller 自定义）
8. `PartService::sync_from_batch_change_with_conn` 同步 part 派生列
9. 返回 `MoveResult`

Response 200 `data`：[`MoveResult`](#moveresult-字段2026-09-30-新增取代旧-assignresult)

错误码：

- **20122 BIZ_BATCH_LOCATION_MISMATCH**（**新增**，HTTP 409）—— `from` 与 batch 实际状态不一致
- **20121 BIZ_BATCH_NOT_FOUND**（HTTP 404）—— batch 不存在 / 已软删
- **20120 BIZ_BATCH_INVALID_STATUS**（HTTP 409）—— batch 非 IN_PROCESS
- **20201 BIZ_WORKER_NOT_FOUND**（HTTP 404）—— `to` 指定 worker 不存在
- **20202 BIZ_WORKER_INACTIVE**（HTTP 409）—— `to` worker 已停用
- **20204 BIZ_WORKER_HOLD_LIMIT_EXCEEDED**（HTTP 409）—— `to` worker 容量触顶
- **20507 BIZ_SHELF_PROCESS_NOT_MAPPED**（HTTP 422）—— `to` 为 POOL 时 shelf 未映射 batch 当前工序
- **20104 BIZ_INVALID_VALUE**（HTTP 400）—— `to` worker 工种不含 batch 当前工序
- **40001 VALIDATION_ERROR**（HTTP 422）—— POOL→POOL 同 kind 移动 / WORKER→WORKER src==dst / payload shape 错
- **40300 FORBIDDEN** —— 非 Manager
- **40901 VERSION_CONFLICT** —— 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_POOL_MOVE_DONE`（payload = `MoveResult`，含 `from_kind` / `to_kind` / `new_holder_id` / `new_location` / `version`）

> 旧 `WORKER_POOL_ADMIN_REMOVED` / `WORKER_POOL_ASSIGN_DONE` 不再发送，前端订阅统一事件名即可。

### `GET /api/v2/prod/pool/{process_id}`

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

### `GET /api/v2/prod/pool/counts`

**2026-09-30 新增**：admin 视角的全工序候选批次聚合（dashboard 快照型查询）。
前端 `WorkerQueueBoard.vue` 用 `counts[].count` 给各 tab 标题加 `(N)` 徽标，
不再依赖每 tab 的 worker-pool 详情是否已加载（早期方案是 N+1 轮询 per-process 端点）。

权限：**Manager + Clerk + Inspector**（与 `GET /api/v2/prod/worker-pool/{process_id}` 同集
—— `current.require_any_role(&[Role::Manager, Role::Clerk, Inspector])`，
admin 视角但不止 Manager）。

Query：无（按现有 per-process 端点惯例，不指定 shelf_id，返回所有货架）

业务流转（service `pool_counts_all_shelves`）：

1. 角色守卫：`Manager + Clerk + Inspector`（service 内 `require_any_role`）
2. 调 `WorkerPoolRepo::group_count_by_process_all_shelves` 单 SQL GROUP BY 取
   `(process_id, count)`：跨 `t_part_batch` 中
   `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND deleted_at IS NULL`
   的批次数（2026-09-30 起按 `current_process_id` 维度聚合，工序池归属的
   权威依据，不再 JOIN `t_process_chain_step`）
3. 二次调 `ProcessRepo::list_by_ids` 取 `process_code / process_name` 元数据
4. 装 `WorkerPoolCountsOut` 返回（`total = counts.iter().map(|c| c.count).sum()`）

> 业务口径与 `list_candidates_by_process_all_shelves`（per-process 候选池详情）完全一致：
> 两者都限定 `status + location + deleted_at` 三态，唯一区别是本端点只 GROUP BY 计次，
> 不返回批次明细。
>
> 含 0 候选批次的 process 不出现在 `counts` 中（SQL `GROUP BY` 不输出 0 行，
> 与前端 tab 数量语义对齐——admin 不关心"无候选"的工序）。
>
> 排序按 `process_id ASC` 稳定（repo SQL `ORDER BY pb.current_process_id ASC` 保证），
> 二次查元数据按 `list_by_ids` 的 `ORDER BY id ASC` 同序返回。

Response 200 `data`：[`WorkerPoolCountsOut`](#workerpoolcountsout-字段)

错误码：

- 40300 FORBIDDEN — 角色不在 Manager+Clerk+Inspector 集合内

WS 广播：无（counts 是 dashboard 快照型查询，无业务流转；与 `GET /state` 同形态
的轻量端点）。

### `POST /api/v2/prod/pool/auto-allocate`

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

---

## 共享 DTO

### AdminRefillRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `worker_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `shelf_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |

### MoveLocation 字段（2026-09-30 新增）

`POST /pool/move` 的 `from` / `to` tagged enum（`#[serde(tag = "kind", rename_all = "UPPERCASE")]`）：

| `kind` | 必填字段 | 说明 |
|---|---|---|
| `POOL`   | `shelf_id` (string i64) | 候选池位置（batch 在生产货架上） |
| `WORKER` | `worker_id` (string i64) | 工人持有位置（batch 被 worker 持有） |

```jsonc
{"kind":"POOL",   "shelf_id": "100"}
{"kind":"WORKER", "worker_id": "50"}
```

### MoveRequest 字段（2026-09-30 新增）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | `deserialize_i64` |
| `from` | MoveLocation | ✓ | 当前 batch 位置 |
| `to` | MoveLocation | ✓ | 目标位置 |
| `note` | string? | ✗ | 可选，写入 `t_part_event.note` |

### TakenItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `part_id` | string (i64) | 工单雪花 ID |
| `batch_no` | i32 | 批次序号 |
| `quantity` | i32 | 批次数量 |
| `serial_no` | string? | 工单序列号 |
| `drawing_no` | string | 图号 |
| `system_delivery_date` | date? | 系统交付日期 |
| `planned_delivery_date` | date? | 计划交付日期 |
| `is_urgent` | bool | 是否加急 |
| `version` | i32 | 乐观锁（admin_remove 返回 `batch.version + 1`） |
| `has_cnc_program` | bool | **2026-09-29 新增**。`refill_for_worker` / `assign_batch_to_worker` / `take_specific_from_pool` 透传 worker_pool 候选池视图同源 EXISTS；admin_remove 路径默认 `false`（admin_remove 不开 candidate EXISTS）。详见下文「§自动分配优先级」 |

### HeldBatchItem 字段

`WorkerPoolState.held_batches` 单条结构。在 `TakenItem` 字段基础上多 `name / customer_name /
parent_customer_name / applicant_name / location / shelf_code / note` 7 个展示字段，
JOIN t_part_batch + t_part + t_customer L1+L2 + t_applicant + t_shelf 一把拉全。

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `part_id` | string (i64) | 工单雪花 ID |
| `batch_no` | i32 | 批次序号 |
| `quantity` | i32 | 批次数量 |
| `serial_no` | string? | 工单序列号（手工工单可空） |
| `drawing_no` | string | 图号 |
| `name` | string | 工单 / 零件名称（t_part.name） |
| `system_delivery_date` | date? | 系统交付日期（t_part） |
| `planned_delivery_date` | date? | 计划交付日期（t_part） |
| `is_urgent` | bool | 是否加急（t_part.is_urgent） |
| `customer_name` | string? | L2 叶子客户名 |
| `parent_customer_name` | string? | L1 一级集团名 |
| `applicant_name` | string? | 申请人姓名（LEFT JOIN t_applicant.name，applicant 软删 / 不存在时为 None） |
| `location` | string | 当前 holder 位置 enum（`"WORKER"`） |
| `shelf_code` | string? | 当前货架编码（WORKER 持有时 `current_holder_id = worker_id` 非 shelf_id，故通常为 None） |
| `note` | string? | 工单级备注（t_part.note） |
| `version` | i32 | 乐观锁（t_part_batch.version） |
| `has_cnc_program` | bool | **2026-09-29 新增**：是否已上传 G_CODE 数控程序（与候选池视图 / take_one_from_pool 同源 EXISTS 子查询）。前端 `WorkerQueueBoard.vue`「已编程」tag 渲染依赖本字段。

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
| `pool_count` | i64 | 该工序在 shelf 上的候选批次数（`t_part_batch` 中 `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND current_holder_id = shelf_id AND current_process_id = process_id`） |

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
| `held_batches` | [HeldBatchItem](#heldbatchitem-字段)[] | **2026-09-14 follow-up-ux 新增**：worker 当前持有的完整 batch 列表（JOIN t_part），按 `t_part_batch.id ASC` 排序。避免前端按 worker 轮询 K 次单 batch 详情接口的 N+1；UI sink `WorkerQueueBoard.vue` 已对接 `:batches="workerHeld[w.id] ?? []"` |

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
| `version` | i32 | 乐观锁 |

> **2026-09-16 字段下线**：`placed_at` 字段已移除（t_part_batch 列已删）。
> 前端如需展示积压时长，由前端按 `PICKED_UP` 事件 `created_at` 自派生；或后端后续补字段。
>
> **2026-09-29 新增字段**：`has_cnc_program: bool`（CNC 重构 5 任务之一）。
> 真相源：`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`
> — 与 `GET /parts/pending-programming` Tab 切换同源 EXISTS。
> 用于前端 admin 候选池视图区分"待编程 vs 待上机"（已上传程序但还在候选池 = 等车间 release）。
> **自动分配优先级**（见下文 §自动分配优先级小节）：`take_one_from_pool` 在同交期同加急
> 时优先 take 已编程 batch（节省"工人拿到手 → 还要等编程员传程序"的等待）。

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

### MoveResult 字段（2026-09-30 新增，取代旧 `AssignResult`）

`POST /pool/move` 响应。覆盖 POOL ↔ WORKER + WORKER ↔ WORKER 三方向。

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `from_kind` | string | `"POOL"` 或 `"WORKER"`（入参 `from.kind`） |
| `to_kind` | string | `"POOL"` 或 `"WORKER"`（入参 `to.kind`） |
| `new_holder_id` | string (i64) | 移动后 `batch.current_holder_id`（POOL 时=shelf_id；WORKER 时=worker_id） |
| `new_location` | string | 移动后 `batch.location`（`"PRODUCTION_SHELF"` 或 `"WORKER"`） |
| `version` | i32 | `batch.version + 1` |
| `current_held` | i32? | 仅 `to_kind=WORKER` 时填：目标 worker 移动后持有数（含本批次） |
| `max_held` | i32? | 仅 `to_kind=WORKER` 时填：目标 worker 工种的 `max_held_batches` |
| `shelf_id` | string (i64)? | 涉及的候选池货架 ID：POOL→WORKER 填 `from.shelf_id`、WORKER→POOL 填 `to.shelf_id`、WORKER→WORKER 不填 |
| `taken` | [TakenItem](#takenitem-字段)? | 仅 POOL→WORKER 移动时填：从 pool 取出的 batch 详情 |

### ProcessBatchCount 字段

`WorkerPoolCountsOut.counts` 单条结构（**2026-09-30 新增**）。
对应后端 `src/modules/prod/worker_pool/dto.rs::ProcessBatchCount`。

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 工序雪花 ID |
| `process_code` | string | 工序代号 |
| `process_name` | string | 工序名 |
| `count` | i64 | 该工序候选批次数（cross-shelf 聚合；`t_part_batch` 中 `status='IN_PROCESS' AND location='PRODUCTION_SHELF' AND deleted_at IS NULL AND current_process_id = process_id`；**2026-09-30 起按 `current_process_id` 维度聚合**，工序池归属的权威列） |

### WorkerPoolCountsOut 字段

**2026-09-30 新增**：admin 视角的全工序候选批次聚合顶层响应。
对应后端 `src/modules/prod/worker_pool/dto.rs::WorkerPoolCountsOut`。

| 字段 | 类型 | 说明 |
|---|---|---|
| `counts` | [ProcessBatchCount](#processbatchcount-字段) | 各工序候选批次数（仅含 count > 0 的工序；按 `process_id ASC` 稳定排序） |
| `total` | i64 | 候选批次总数（`counts.iter().map(|c| c.count).sum()`；与 `counts[].count` 求和对齐） |

---

## WS 事件清单（worker-pool 相关）

> 全部走 `WsEvent::DashboardEvent { kind, payload }`，payload 字段如下：
> 详见 [`../websocket.md`](../websocket.md)

| kind | 触发端点 | payload 关键字段 |
|---|---|---|
| `WORKER_SCAN_RETURNED` | `POST /prod/batches/worker-scan`（event_type=RETURNED） | `{ worker_id, part_id, batch_id, event_type }`（即 `WorkerScanCoreOut`） |
| `WORKER_SCAN_INSPECTED` | `POST /prod/batches/worker-scan`（event_type=INSPECTED） | `{ worker_id, part_id, batch_id, event_type }`（即 `WorkerScanCoreOut`） |
| `WORKER_POOL_REFILL_DONE` | `POST /prod/batches/worker-scan` 同事务 refill 抢到 / `POST /pool/refill` | `{ worker_id, shelf_id, taken: [TakenItem], pool_empty }`（即 `RefillResult`） |
| `WORKER_POOL_EMPTY` | `POST /prod/batches/worker-scan` 同事务 refill 池空 / `POST /pool/refill` 池空 | `{ worker_id, shelf_id }` |
| `WORKER_POOL_MOVE_DONE` | `POST /pool/move` | `{ batch_id, from_kind, to_kind, new_holder_id, new_location, version, current_held?, max_held?, shelf_id?, taken? }`（即 `MoveResult`；**2026-09-30 新增**，取代旧 `WORKER_POOL_ADMIN_REMOVED` / `WORKER_POOL_ASSIGN_DONE`） |
| `WORKER_POOL_AUTO_ALLOCATE_DONE` | `POST /pool/auto-allocate` | `{ process_id, shelf_id, mode, fill_ratio, filled: [WorkerFillItem], pool_empty }`（即 `AutoAllocateResult`） |

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
- ✅ Task 8：`POST /prod/batches/worker-scan`（同事务联动 refill；2026-10-02 自 part 域迁入）
- ✅ 2026-09-11 part-worker-pool-federated-rocket：新增 `auto_allocate_for_process` + 端点 `POST /admin/worker-pool/auto-allocate` + COUNT/TIME 模式 + fill_ratio 校验（20704）；错误码段 20701/20702/20703/20704
- ✅ 2026-09-14 follow-up-ux：`WorkerPoolState` 新增 `held_batches` 字段（`list_held_by_worker_with_part` JOIN t_part 取全量）+ 新增 `POST /admin/worker-pool/assign` 端点（单 batch 拖拽分配，service `assign_batch_to_worker`）+ `WorkerPoolRepo::take_specific_from_pool`（单 SQL 限定 `(shelf_id, batch_id)` 原子切换 holder）；错误码沿用既有 20204 / 20114 / 20104
- ⏳ 未上线：`WorkerRepo` 列表 / 创建 / 软删等 CRUD（worker 域当前仅供 worker_pool / prod batches worker-scan 复用）

## 参考

- 集成测试：`tests/worker_pool_api.rs` / `tests/worker_pool_auto_allocate_api.rs`
- 仓库分层：`src/modules/prod/worker_pool/handler.rs` (axum) → `service.rs` (业务) → `repo.rs` (SQL)
- 错误码：`src/shared/error.rs::code`（20104 / 20109 / 20114 / 20201 / 20202 / 20204 / 20206 / 20901 / 20904 / 20905 / 20703 / 20704 / 40001 / 40300 / 40901）