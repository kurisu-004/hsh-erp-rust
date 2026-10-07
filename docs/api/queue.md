# prod::queue 域 API（生产队列：工序候选池 + 工人持有 + 发放/召回/移动）

> 本文件是 `prod::queue` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本域同批改动的 `prod::batch` 域契约见 [`batch.md`](batch.md)。

## 0. 2026-10-08 变更摘要

本文件描述的域，原名 `prod::worker_pool`、URL 前缀 `/api/v2/prod/pool`，2026-10-08 一次做完四件事：

1. **重命名**：`worker_pool` → `queue`，URL `/pool` → `/queue`（**硬切，无 alias**，旧路径 404）。改名缘由：域职责从「工人候选池」扩到「工序队列」（候选池 + 工人持有 + 发放 / 召回 / 移动 / 自动分配），`pool` 只覆盖了第一块。
2. **从 `prod::batch` 吸收 4 个端点**：`pending` / `dispatch` / `auto-dispatch`（下发流）+ `recall`（召回）。它们原先挂在 `/api/v2/prod/batches/*`，消费方是队列页而非批次详情页。旧路径 404。
3. **新增 2 个只读聚合端点**（`/snapshot` 与 `/processes/{process_id}`），删掉 3 个旧读端点（`/state`、`/counts`、`/{process_id}`）—— 消掉两次 N+1（见 §4.4）。
4. **VO 按前端实际消费收敛**（逐字段证据见 §6）。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/prod/queue/snapshot` | Manager + Clerk + Inspector | 无 | `QueueBoardSnapshot` |
| 2 | GET | `/api/v2/prod/queue/processes/{process_id}` | Manager + Clerk + Inspector | path `process_id`（雪花 ID 字符串） | `QueueProcessBoardDetail` |
| 3 | GET | `/api/v2/prod/queue/pending` | Manager + Clerk + Inspector | `limit?`（缺省 200，clamp 1..500）、`offset?` | `PendingBatchListOut` |
| 4 | POST | `/api/v2/prod/queue/dispatch` | Manager + Clerk | `{ targets: [{batch_id, target_process_id}], note? }` | `DispatchResult` |
| 5 | POST | `/api/v2/prod/queue/auto-dispatch` | Manager + Clerk | `{ batch_ids?: string[] }` | `AutoDispatchResult` |
| 6 | POST | `/api/v2/prod/queue/recall` | Manager + Clerk | `{ batch_id, version, note? }` | `RecallOut` |
| 7 | POST | `/api/v2/prod/queue/refill` | **Manager 独占** | `{ worker_id, shelf_id }` | `RefillResult` |
| 8 | POST | `/api/v2/prod/queue/move` | **Manager 独占** | `{ batch_id, from, to, note? }` | `MoveResult` |
| 9 | POST | `/api/v2/prod/queue/auto-allocate` | **Manager 独占** | `{ process_id, shelf_id, mode, fill_ratio }` | `AutoAllocateResult` |

- 全部返回统一信封 `R { code, message, data }`。
- 端点 1 / 2 **不接受**任何 query 参数。端点 3 只接 `limit` / `offset`，传别的 query 参数不会报错但被忽略。
- 端点 4 / 5 是**契约变更**：`POST /api/v2/prod/batches/{batch_id}/recall-to-pending` 改为 `POST /api/v2/prod/queue/recall`，`batch_id` 由 path 参数改为 **body 字段**，出参由 `PartOut`（工单全量投影）改为 `RecallOut`（3 字段）。⚠️ 旧路径 404，**无 alias**。
- 端点 1 / 2 是**纯读**（`pool.acquire()` 不开事务、不发 WS 广播）；端点 3 / 5 同。端点 4 / 6 / 7 / 8 / 9 开事务，**广播在 commit 之后**。
- 端点 2 工序不存在或已软删 → `20801 BIZ_PROCESS_NOT_FOUND`（**HTTP 404**）。`{process_id}` 抽不出数字时走 axum 的 `PathRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**（全仓 `Path<i64>` 端点的统一行为，非本端点特例）。
- 端点 1 / 2 / 3 被 SHELF_ACCOUNT 访问 → `40300`（角色守卫下沉在 service 第一行）。
- i64 雪花主键一律序列化为 JSON **string**（`"1590000000000000001"`），防 JS `Number` 精度截断。

### 1.1 路由注册顺序（当前无硬约束）

9 条路由里 8 条是 1 段静态、1 条是 2 段动态（`/processes/{process_id}`），**段数不同 ⇒ matchit 无同段位争用 ⇒ 注册顺序不影响匹配结果**。`src/modules/prod/queue/mod.rs::router` 里「1 段在前」只是书写习惯。

> 若将来新增 1 段动态段（如 `/{batch_id}/…`），届时 1 段组与它同段位，「静态段必须先注册」才重新成为硬约束（对照 `src/modules/outsource/handler.rs::company_router` 的 `/{id}` catch-all）。

## 2. 逐字段

### 2.1 `QueueBoardSnapshot`（端点 1）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `processes[]` | array | `board/repo.rs::board_snapshot` SQL 1 + 2（见下） |
| `pending_count` | number | `board/repo.rs::board_snapshot` SQL 3 `SQL_COUNT_PENDING` |
| `ts` | string | `infra::clock::now_shanghai_iso()`（格式见 §7） |

`processes[]` 元素（`QueueProcessBoard`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `process_id` | string | `SQL_POOL_COUNT_BY_PROCESS` 的 `current_process_id`（`t_part_batch`） |
| `process_code` | string | `SQL_PROCESS_META_BY_IDS` 的 `t_process.code` |
| `process_name` | string | `t_process.name` |
| `color` | string \| null | `t_process.color`（`#RRGGBBAA`，历史行可能为 null） |
| `category` | string | `t_process.category`（DB CHECK 约束 `INHOUSE` / `OUTSOURCE`） |
| `pool_count` | number | `COUNT(*)::bigint`，该工序候选批次数 |

**只返 `pool_count > 0` 的工序**（SQL `GROUP BY` 不产 0 行组）。工序元数据查不到（`t_process` 已软删）的行以 `process_code = ""` + `process_name = "(deleted#{id})"` 占位返回，**计数仍显示** —— 运营需要看到「这批货压在谁的池子里」。

### 2.2 `QueueProcessBoardDetail`（端点 2）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `process` | object | `SQL_PROCESS_META_ONE` |
| `workers[]` | array | `SQL_WORKERS_BY_PROCESS`（字段见 `QueueWorkerBrief`） |
| `items[]` | array | `SQL_POOL_ITEMS_BY_PROCESS`（字段见 `QueuePoolItem`） |
| `total` | number | `items.len()`（不分页，与 `items` 恒等） |
| `ts` | string | `infra::clock::now_shanghai_iso()` |

`process`（`QueueProcessMeta`）：`process_id` / `process_code` / `process_name` / `color`。

### 2.3 `QueueWorkerBrief`（`workers[]` 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `worker_id` | string | `t_worker.id` |
| `name` | string | `t_worker.name` |
| `work_type_code` | string | `t_work_type.code`（经 `t_work_type_process` JOIN） |
| `badge_code` | string | `t_worker.badge_code` |
| `max_held` | number | `t_work_type.max_held_batches`，NULL 时按 **0** 处理 |
| `current_held` | number | 持有批次行数（`held_batches.len()`） |
| `capacity_remaining` | number | **service 层算** `max(0, max_held - current_held)` |
| `held_batches[]` | array | `SQL_HELD_BATCHES_BY_WORKERS`（字段见 `QueueHeldBatch`） |

闸门：`w.is_active = TRUE` + `w.deleted_at IS NULL` + `wtp.deleted_at IS NULL` + `wt.deleted_at IS NULL`。`work_type_id IS NULL` 的工人被 INNER JOIN 工种表时自然排除（没有工种就没有 `max_held`，无法参与容量计算）。

`capacity_remaining` clamp 到 0 的原因：历史数据里 `max_held` 被改小会让 `max - current` 为负，展示负容量会让 UI 渲染出「-2 个空位」。

### 2.4 `QueueHeldBatch`（`workers[].held_batches[]` 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `batch_id` / `part_id` | string | `t_part_batch.id` / `.part_id` |
| `batch_no` / `quantity` | number | `t_part_batch.batch_no` / `.quantity` |
| `serial_no` | string \| null | `t_part.serial_no` |
| `name` / `drawing_no` | string | `t_part.name` / `.drawing_no` |
| `system_delivery_date` | string \| null | `t_part.system_delivery_date` |
| `planned_delivery_date` | string \| null | `t_part.planned_delivery_date` |
| `is_urgent` | boolean | `t_part.is_urgent` |
| `has_cnc_program` | boolean | `EXISTS (t_part_file kind='G_CODE' AND part_id=pb.part_id AND deleted_at IS NULL)` |
| `customer_name` | string \| null | `t_customer`（`p.customer_id`，L2 叶子） |
| `parent_customer_name` | string \| null | `t_customer`（`c2.parent_id`，L1 集团） |
| `applicant_name` | string \| null | `t_applicant.name`（`a.name = p.applicant_name`，非 FK） |
| `location` | string | `t_part_batch.location`，恒为 `"WORKER"`（写入闸门保证） |
| `note` | string \| null | `t_part.note` |
| `version` | number | `t_part_batch.version`（**OCC 锚**，下次写操作必须带） |

⚠️ **不含 `shelf_code`**（旧 VO 有）：持有态 `current_holder_id = worker_id`，`t_shelf` JOIN 恒不命中，该字段永远是 `null`。见 §6。

### 2.5 `QueuePoolItem`（`items[]` 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `batch_id` / `part_id` | string | `t_part_batch.id` / `.part_id` |
| `batch_no` / `quantity` | number | `t_part_batch.batch_no` / `.quantity` |
| `serial_no` | string \| null | `t_part.serial_no` |
| `name` / `drawing_no` | string | `t_part.name` / `.drawing_no` |
| `system_delivery_date` | string \| null | `t_part.system_delivery_date` |
| `customer_name` / `parent_customer_name` | string \| null | `t_customer` L2 / L1 |
| `applicant_name` | string \| null | `t_applicant.name`，回退到 `t_part.applicant_name` |
| `shelf_id` | string | `t_part_batch.current_holder_id` |
| `shelf_code` / `shelf_name` | string | `t_shelf.code` / `.name`（INNER JOIN，未命中则该批不进候选池） |
| `is_urgent` | boolean | `t_part.is_urgent` |
| `has_cnc_program` | boolean | 同 `QueueHeldBatch` 的 EXISTS |
| `note` | string \| null | `t_part.note` |
| `version` | number | `t_part_batch.version`（OCC 锚） |

`shelf_id` 是 `POST /queue/move` 的 `from.shelf_id` **唯一数据源**：候选池跨货架，不能用用户当前激活货架凑（激活货架对 MANAGER / CLERK / INSPECTOR 恒为空）。

⚠️ **不含 `customer_path` 与 `location`**：前者前端自己拼 L1 / L2；后者恒为 `"PRODUCTION_SHELF"`，前端用 `shelf_code` 表达位置。见 §6。

## 3. 下发流 VO（端点 3 / 4 / 5 / 6）

- `PendingBatchListOut`：`{ items: PendingBatchItem[], total, limit, offset }`。
- `DispatchResult`：`{ succeeded: DispatchSuccessItem[], failed: DispatchFailureItem[] }`。`failed` **当前总为空**（保留为 partial commit 启用预留）；任一 target 失败 → service 抛错 → handler tx Drop 全回滚。
- `AutoDispatchResult`：`{ items: AutoDispatchItem[] }`。`skip_reason` ∈ `NOT_FOUND` / `NO_PROCESS_CHAIN` / `NO_PROCESS_STEP` / `NO_SHELF` / `null`（可下发）。
- `RecallOut`：**3 字段** `{ batch_id: string, part_id: string, version: number }`。`version` 是写入后的 `version + 1`（OCC 锚），前端下一次对本批次的操作必须带这个值。

## 4. 口径表

### 4.1 候选池判据（`status` / `location` 两列在 3 处一致，**货架 JOIN 不一致**）

```
status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF'
```

`status` / `location` 这两列是「一个批次在某道工序的候选池里」的判据，被 3 处共用：

| 用途 | SQL 位置 | `current_process_id` 闸门 | `t_shelf` JOIN |
|---|---|---|---|
| 序列板各工序计数（端点 1） | `board/repo.rs::SQL_POOL_COUNT_BY_PROCESS` | `IS NOT NULL` | **无** |
| 单工序候选池明细（端点 2 `items[]`） | `board/repo.rs::SQL_POOL_ITEMS_BY_PROCESS` | `= $1` | **INNER**（`s.id = pb.current_holder_id AND s.deleted_at IS NULL`） |
| 抢占（`take_one_from_pool` / `take_specific_from_pool`） | `repo/sql.rs` | `= ANY($1)` | **无** |

`current_process_id` 闸门是必需的：端点 1 靠它丢弃「池归属为空」的批次（否则 `GROUP BY` 会产出一个 NULL 组而解码进 `i64` 直接报错），端点 2 / 抢占是拿它当等值 / 数组匹配条件。⚠️ 这条谓词是「出池必须置 `current_process_id` NULL」这条不变式的**兜底**：写点万一漏清，脏值也命中不了池查询。

⚠️ **端点 2 额外带了 `t_shelf` INNER JOIN，端点 1 没有** —— 后果见 §8.4 已知偏差登记。改这两条 SQL 时不要顺手动对方的 JOIN。

**与前端是人工同步关系，无编译期保障**。改后端这 3 处时必须同步前端筛选逻辑；改前端时必须同步这 3 处。集成测试 `board_snapshot_matches_legacy_pool_counts` 钉住端点 1 的后端口径（跨货架聚合、零候选工序不出现、total 为求和），前端侧无对应断言。

### 4.2 待下发判据（2 处共用）

```
pb.status IN ('PENDING', 'PROGRAMMING') AND pb.deleted_at IS NULL AND p.deleted_at IS NULL
```

`PROGRAMMING` 是**已废弃**状态（`part::statemachine` 的入口端点已下线，只留出口），但存量行需要在待下发页被消化，故与 `PENDING` **同链路、同待遇**。白名单分布在 6 处，必须同步：

| 用途 | SQL 位置 |
|---|---|
| 待下发列表（端点 3 `items[]`） | `repo/dispatch.rs::list_pending_batches` |
| 待下发总数（端点 3 `total`） | `repo/dispatch.rs::count_pending_batches` |
| 自动下发预览（端点 5） | `repo/dispatch.rs::preview_auto_dispatch` |
| 下发写入的源状态白名单（端点 4） | `repo/dispatch.rs::update_batch_dispatched` 的 `allowed_from` |
| 序列板 `pending_count`（端点 1） | `board/repo.rs::SQL_COUNT_PENDING` |
| service 内二次校验（端点 4） | `service/dispatch.rs::dispatch_single` |

漏改任一处的症状：列得出但下发不了（40901），或 `total` 与 `items` 口径不一致。

### 4.3 工人持有判据

```
status = 'IN_PROCESS' AND location = 'WORKER' AND current_holder_id = ANY($worker_ids)
```

`location = 'WORKER'` 是「在手加工」的语义边界（批次已从货架 / 品检区出池、压在工人手上）。工位容量按这两列实时 COUNT，召回时把两列一起清 NULL 即完成回收，无需额外动作。


### 4.4 N+1 的消除：旧路径 vs 新路径

| 场景 | 旧 | 新 |
|---|---|---|
| 进程序列板（N 道工序有货） | 1（`/counts`）+ 1（`/pending` 拿 tab 徽标）= 2 | **1** |
| 打开一道工序的板（M 个可用工人） | 1（`/pool/{id}`）+ 1（每工人一次 `/pool/state`）= **1 + M** | **1** |

M = 10 时：12 个 HTTP 请求 → 1 个。

### 4.5 SQL 条数固定（与工人数 / 批次数无关）

| 方法 | SQL 条数 | 组成 |
|---|---:|---|
| `board_snapshot` | **3** | 工序计数 / 工序元数据（`id = ANY($1)`）/ 待下发计数 |
| `board_process_detail` | **4** | 工序元数据（单行）/ 工人+工种 `max_held`（一条 JOIN 带出）/ **全部工人持有批次一条 `current_holder_id = ANY($1)`** / 候选池 |

`board_process_detail` 的第 3 条是消灭 N+1 的关键：`ANY($1::bigint[])`（不是 `= $1`），10 个工人与 2 个工人发的是**同一条 SQL**，只是数组长度不同。集成测试 `board_process_detail_held_batches_complete_for_ten_workers` 钉住「10 个工人一次返回 10 个 worker 且 held 批次不漏不错」。

`board_process_detail` 的 SQL 里**没有**「工种 `max_held` 单独一条 `id = ANY`」—— 工人查询本身已 JOIN `t_work_type`，拆出去等于同一份数据取两次；也**没有**「待下发计数」—— `QueueProcessBoardDetail` 无该字段（`pending_count` 是工序无关的全局量，由端点 1 提供）。

**恒定性由 `cargo test --lib` 的 `modules::prod::queue::board::sql_count_guard_tests` 强制**（源码级护栏，与 `shared::batch::status::write_guard_tests` 同款）：`no_sqlx_query_inside_loop_body` 禁止 `sqlx::query` 出现在 `board/` 任何 `for` / `while` / `loop` 循环体内，`aggregate_query_counts_are_pinned` 钉死上表两个数字。集成测试数不了 SQL 条数（sqlx 0.9 不再为 `sqlx::query` 发 tracing 事件，PG 侧 `pg_stat_statements` 要预热 + 扩展才有意义），故走源码级。

### 4.6 实现约定

- 走运行时 `sqlx::query` + `Row::get`，**不用 `query!` 宏**（照 dashboard 域：复杂聚合 SQL 字段多、迭代频繁，不进 `.sqlx/` 离线缓存）。
- SQL 写成模块级 `const SQL_*` 字面量，**不做字符串拼接**（拼列名会开注入面）。
- 时间口径从 service 层绑进 SQL，**不写 `CURRENT_DATE`**（DB 会话时区与本仓统一的 Asia/Shanghai 是两个时钟，测试容器会话时区正是 UTC；不一致时会静默丢行）。本域 2 个聚合方法当前都无日期窗口，一旦加窗口必须走形参。

## 5. 状态域约定（无编译期保障）

- 待下发源状态白名单 `IN ('PENDING', 'PROGRAMMING')`：6 处共用（见 §4.2），**无编译期约束**，漏改任一处症状是「列得出但下发不了」或 `total` 与 `items` 对不上。
- 候选池判据 `status='IN_PROCESS' AND location='PRODUCTION_SHELF'`：3 处共用（见 §4.1），与前端是人工同步关系。
- `t_part_batch.status` 的**写**入口是全仓唯一的 `shared::batch::status::apply_batch_status_change`，由 `cargo test --lib` 的 `shared::batch::status::write_guard_tests::no_outside_file_writes_batch_status` 强制（扫全 `src/**/*.rs`）。本域的 `move` / `recall` / `dispatch` / `refill` 全部走该入口的薄包装。

## 6. 移除记录（2026-10-08）

### 6.1 端点

| 被移除项 | 原因 |
|---|---|
| `GET /api/v2/prod/pool/state?worker_id=&shelf_id=` | 每 worker 一次请求 = N+1；持有批次已并入端点 2 的 `workers[].held_batches` |
| `GET /api/v2/prod/pool/counts` | 被端点 1 覆盖（后者多工序元数据 + `pending_count`），且不需第 2 个 HTTP 拿待下发数 |
| `GET /api/v2/prod/pool/{process_id}` | 被端点 2 覆盖（后者多工人维度 + 持有批次） |
| `GET /api/v2/prod/batches/pending` | 迁到 `/api/v2/prod/queue/pending`（消费方是队列页） |
| `POST /api/v2/prod/batches/dispatch` | 迁到 `/api/v2/prod/queue/dispatch` |
| `POST /api/v2/prod/batches/auto-dispatch` | 迁到 `/api/v2/prod/queue/auto-dispatch` |
| `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` | 迁到 `/api/v2/prod/queue/recall`，`batch_id` 改入 body，出参改 `RecallOut` |

### 6.2 字段

| 被移除字段 | 原因（前端零消费，grep 证据） |
|---|---|
| `WorkerPoolState.pool_count_by_process[]` | 前端仅在 zod schema / contract / spec 里声明，无一处读取。删它同时消掉 `StateQuery.shelf_id` 与 `service.rs` 里 `for pid in process_ids { count_pool_by_shelf_and_process }` 的**内层 N+1** |
| `StateQuery.shelf_id` | 唯一用途就是填上面那个字段 |
| `ProcessPoolDetail.work_types[]`（`WorkTypeMaxHeld` 数组） | `WorkerPoolTab.vue` 只从 `workers[]` 取 `worker_id` / `name` / `work_type_code`；`max_held` 改由端点 2 直接挂到每个 worker 上 |
| `WorkerBrief.work_type_id` | 同上，`WorkerPoolTab.vue` 从不读 |
| `QueueHeldBatch.shelf_code` | 持有态 `current_holder_id = worker_id`，`t_shelf` JOIN 恒不命中 → 恒 `null`；前端零消费 |
| `QueuePoolItem.customer_path` | 前端用 `customer_name` + `parent_customer_name` 自行拼 |
| `QueuePoolItem.location`（原始 enum） | 恒为 `"PRODUCTION_SHELF"`；前端用 `shelf_code` 表达位置 |
| `RecallOut` 的 `part`（原 `PartOut` 全量投影） | 召回的语义锚点是**批次**，返工单投影让前端为拿 `part_id` 解析上百字段对象，且批次自己的 `version` 根本不在里面 |

### 6.3 文件

| 被移除文件 | 原因 |
|---|---|
| `src/modules/prod/queue/model.rs` | 5 个 struct（`TakenItem` / `HeldBatchItem` / `RefillResult` / `ProcessPoolCount` / `WorkerPoolState`）全是纯出参，没有一列对应独立表行模型 —— 照 dashboard 域做法「无 model.rs，出参在 vo/、行精简在 repo/」，搬进 `vo/worker.rs` |
| `QueueDispatchRepo::first_step_of_chain` | 零调用（被 `preview_auto_dispatch` 的 `LEFT JOIN LATERAL` 取代） |
| `QueueDispatchRepo::part_get_process_chain_id` | 零调用（同上） |
| `QueueRepoTrait` 4 个只读 helper | 只服务已删的 3 个读端点，等价能力在 `board/repo.rs` 的聚合 SQL 里 |

## 7. 与 WS 的关系

- **queue 域不订阅 WS。** 本域的两个读端点都是 HTTP `pool.acquire()` 拉取，前端靠 TanStack Query 的 staleTime + `POST /queue/move` 成功后的失效编排来刷新。
- **但本域的写端点会发广播**（都是 `WsEvent::DashboardEvent { kind, payload }`，**commit 之后**发）：

| 写端点 | `kind` | 条件 |
|---|---|---|
| `POST /queue/recall` | `PART_RECALLED` | 无条件（payload `{ part_id }`） |
| `POST /queue/dispatch` | `BATCH_PLACED_ON_SHELF` | `succeeded` 非空 |
| `POST /queue/refill` | `WORKER_POOL_REFILL_DONE` | `taken` 非空 |
| `POST /queue/refill` | `WORKER_POOL_EMPTY` | 一批也没抢到 |
| `POST /queue/move` | `WORKER_POOL_MOVE_DONE` | 无条件 |
| `POST /queue/auto-allocate` | `WORKER_POOL_AUTO_ALLOCATE_DONE` | 无条件 |

- **消费方是 dashboard 域**（`/ws/dashboard` + 前端 `AFFECTS_DASHBOARD` 白名单），不是 queue 域自己的实时刷新。
- `ts` 时间戳：两个聚合端点出参的 `ts` 走 `infra::clock::now_shanghai_iso()`，恒为 RFC 3339 带固定偏移 `YYYY-MM-DDTHH:MM:SS[.小数秒]+08:00`。小数秒位数按纳秒有效位自适应（0 / 3 / 6 / 9 位），**不保证逐字等长** —— 前端不要按固定小数位数做字符串截取比较。**禁止**改用 `chrono::Local::now()`（那会让时间戳跟宿主时区走）。

## 8. 表依赖与前端配套

### 8.1 读的表

| 用途 | 表 |
|---|---|
| 候选池 / 持有批次 / 待下发 | `t_part_batch` |
| 工单展示字段 | `t_part` |
| 工序元数据 | `t_process` |
| 工人与工种 | `t_worker` / `t_work_type` / `t_work_type_process` |
| 客户两级名 | `t_customer`（L2 + `parent_id` L1） |
| 申请人 | `t_applicant` |
| 货架 | `t_shelf` |
| CNC 程序存在性 | `t_part_file`（`EXISTS` 子查询） |
| 下发预览的工艺链 | `t_process_chain_step` / `t_part.process_chain_id` |
| 下发时的货架解析 | `t_shelf_process` |

### 8.2 跨域依赖登记

queue 域**整体不适用**域隔离护栏：它继承 worker_pool 的「经本域 trait 转发其它域单表查询」pattern（`repo/mod.rs::QueueRepoTrait` 转发 `worker` / `work_type` / `process` / `process_chain` / `part` / `shelf_process`），这是逐域剥离期间的既定 pattern（与 assembly / shelf / process_chain 同形）。**这是本域唯一被允许的跨域面**。

**新增的 board 聚合 SQL 零跨域依赖** —— `board/` 子模块单独由 `cargo test --lib` 的 `modules::prod::queue::board::tests::board_aggregation_depends_on_no_other_domain` 守住（扫 `src/modules/prod/queue/board/**/*.rs`，代码区里任何 `crate::modules::<他域>` 路径即失败，含同父兄弟域 `prod::batch`）。护栏为什么只扫 `board/`：聚合 SQL 读的 9 张表完全可以在本域 SQL 内聚合，写端点的转发 pattern 则是既有事实，圈出来单独守比整域不守要强。

### 8.3 前端配套改动清单

1. **URL 全量替换**：`/api/v2/prod/pool/*` → `/api/v2/prod/queue/*`；`/api/v2/prod/batches/pending|dispatch|auto-dispatch` 与 `/api/v2/prod/batches/{id}/recall-to-pending` → `/api/v2/prod/queue/pending|dispatch|auto-dispatch|recall`。**无 alias**，旧路径 404。
2. **`POST /queue/recall` 入参形态变更**：`batch_id` 从 path 参数移到 body，且**必须是 JSON 字符串**（`"1590000000000000001"`）。它走 `shared::types::deserialize_i64`，该函数体是 `String::deserialize` → **只接受字符串**；发 JSON number 会被 axum 的 `JsonRejection`（`JsonDataError`）拒掉 → **HTTP 422 纯文本，不进 `R<T>` 信封**（故响应里没有 `code` 字段，勿按 `40001` 分支解析）。原调用方传 `{ version, note }` + path 的要改成 `{ batch_id, version, note }`。
3. **`POST /queue/recall` 出参变更**：`data` 由 `PartOut` 换成 `RecallOut`（3 字段）。读 `out.id` 拿 part_id 的改成 `out.part_id`；OCC 版本号改读 `out.version`（原 `PartOut` 里根本没有批次的 `version`）。
4. **新增 2 个端点的 composable**：`GET /snapshot`（序列板 + 待下发 tab 徽标）、`GET /processes/{id}`（单工序板）。原 `useWorkerPoolByProcessQuery` + `useWorkerStateByWorkerQuery`（每 worker 一次）应合并为**一次**请求；`useWorkerPoolCountsQuery` 迁到 snapshot。
5. **删 3 个 composable**：`useWorkerStateByWorkerQuery`（`/state`）、`useWorkerPoolCountsQuery`（`/counts`）、`useWorkerPoolByProcessQuery` 的旧形态（`/pool/{id}`）—— 后者改指 `/processes/{id}`。
6. **zod schema 同步**：`workerBriefSchema` 删 `work_type_id`（4 → 3 字段）；删 `workTypeMaxHeldSchema` 与 `poolBatchItemSchema` 的 `customer_path` / `location`；新增 `queueBoardSnapshotSchema` / `queueProcessBoardDetailSchema` / `queueHeldBatchSchema` / `queuePoolItemSchema`。**注意 zod 默认 strip 模式**会让漏声明的字段静默丢失，数组元素必须全字段声明。
7. **i64 字符串化**：所有雪花 id 仍是 JSON string，本仓不因本次改动变更该约定。
8. **`max_held` 取值位置变更**：原从 `work_types[].max_held_batches` 按工种查，改从 `workers[].max_held` 按工人直接读。`max_held_batches` 未设置时后端返 0（不是 null）—— 展示「未设置上限」占位的逻辑需自行按 0 判断。
9. **代码里残留的 `WORKER_POOL_*` WS 事件名不变**（`kind` 是 WS 协议的一部分，改它要同步 dashboard 域的白名单与前端 `AFFECTS_DASHBOARD`）。本域改的只是 URL 与类型名。

### 8.4 已知偏差登记

**端点 1 的 `pool_count` 可能大于端点 2 的 `items.length`（差值 = 指向已软删货架的批次数）。**

成因：端点 1 的计数 SQL（`SQL_POOL_COUNT_BY_PROCESS`）不带 `t_shelf` JOIN；端点 2 的明细 SQL（`SQL_POOL_ITEMS_BY_PROCESS`）带 **INNER JOIN `t_shelf s ON s.id = pb.current_holder_id AND s.deleted_at IS NULL`**。`current_holder_id` 是「货架还是工人」同列承载的复用列，理论上可残留一个已被软删的货架 id —— 这样的批次命中计数（它确实是 `IN_PROCESS` + `PRODUCTION_SHELF`）却不命中明细（拿不到货架行）。

现有测试抓不到这个跨端点分歧：`board_snapshot_matches_legacy_pool_counts` 的两个断言都不带货架 JOIN（它对照的是被取代的旧 `pool_counts_all_shelves` 口径，两者都不 JOIN `t_shelf`）。

产品决议（2026-10-08）：**暂不处理**。已软删货架上的批次本就是需要人工清理的脏数据，让它在明细里消失反而符合「不该被认领」的直觉；口径差留给将来做货架软删清理时一并收敛（届时把明细的 INNER JOIN 改成 LEFT JOIN + 占位，或给计数 SQL 补同一套货架闸门）。改这两条 SQL 时请先回到本节确认决议是否仍然有效。
