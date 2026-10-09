# prod::queue 域 API（生产队列：工序候选池 + 工人持有 + 发放/召回/移动）

> 本文件是 `prod::queue` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本域同批改动的 `prod::batch` 域契约见 [`batch.md`](batch.md)；
> 与本域同事务编排的报工台端点（`POST /scan/worker-scan`）见 [`scan.md`](scan.md)。

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
| 8 | POST | `/api/v2/prod/queue/move` | **Manager 独占** | `{ batch_id: string, version: number, from, to, note? }`（`from` / `to` 是**两个不同**的 tagged enum，见 §2.8） | `MoveResult` |
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
| `has_process_chain` | boolean | `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR`（判据与理由见下） |
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
| `has_process_chain` | boolean | 同 `QueueHeldBatch` 的 `HAS_PROCESS_CHAIN_EXPR` |
| `note` | string \| null | `t_part.note` |
| `version` | number | `t_part_batch.version`（OCC 锚） |

`shelf_id` 是 `POST /queue/move` 的 `from.shelf_id` **唯一数据源**：`from.kind = "POOL"` 时它必须与批次真实所在货架一致（service 比对 `batch.current_holder_id`）。候选池跨货架，不能用用户当前激活货架凑（激活货架对 MANAGER / CLERK / INSPECTOR 恒为空）。

⚠️ **不含 `customer_path` 与 `location`**：前者前端自己拼 L1 / L2；后者恒为 `"PRODUCTION_SHELF"`，前端用 `shelf_code` 表达位置。见 §6。

### 2.8 `POST /queue/move` 的 `from` / `to` 入参（2026-10-10 拆成两个类型）

| 侧 | 类型 | `POOL` 分支 | `WORKER` 分支 |
|---|---|---|---|
| `from` | `MoveFromLocation` | `{ kind: "POOL", shelf_id }` —— **必填** | `{ kind: "WORKER", worker_id }` |
| `to` | `MoveToLocation` | `{ kind: "POOL" }` —— **无字段** | `{ kind: "WORKER", worker_id }` |

`from` 侧仍要 `shelf_id`：它是批次**真实所在**的货架，POOL→WORKER 方向 service 拿它比对
`batch.current_holder_id`（不符 → `20122`）。

`to` 侧的 `shelf_id` 于 2026-10-10 **删除**（WORKER→POOL 即「撤回候选池」）：目标货架改由
服务端按 `batch.current_process_id` 自动选（[`shelves.md`](shelves.md) §4），
候选集只含映射了该工序的活跃 `PRODUCTION` 架；选不出 → `20508 BIZ_SHELF_PROCESS_NOT_FOUND`。
`batch.current_process_id IS NULL` 的存量批次按「不按工序筛候选」处理（落在该 zone 全部
活跃生产架里），管理员「把卡住的手动放回货架」的自救路径不被堵死 —— 但 zone / 停用 / 软删
三条谓词仍然生效，不会落到品检架上。

⚠️ **向后兼容只有单向**：老客户端多发 `to.shelf_id` 会被 serde 静默忽略（本仓生产代码零
`deny_unknown_fields`），所以**老客户端 + 新服务端不受影响**；反过来**新客户端 + 老版本
服务端会得 HTTP 422 纯文本**（老服务端的 `to.shelf_id` 是必填、无 `#[serde(default)]`，
缺字段在 axum `Json` 提取器阶段就被拒，不进 `R<T>` 信封）⇒ **部署顺序必须后端先上**。
前端改造后撤回候选池时 `to` 就是 `{"kind":"POOL"}`；在新服务端上补发 `shelf_id` 无害但无用。

### 2.6 `has_process_chain` 判据（4 处卡片共用一个常量）

卡片绿色左边框的判据，**唯一真源**是 `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR`：

```sql
p.process_chain_id IS NOT NULL
AND ( (cs.process_id IS NOT NULL AND cs.process_id = pb.current_process_id)
   OR (pb.current_process_id IS NULL
       AND EXISTS (SELECT 1 FROM t_process_chain_step x
                   WHERE x.chain_id = p.process_chain_id AND x.deleted_at IS NULL)) )
```

| 分支 | 形态 | 含义 |
|---|---|---|
| 1 | 指针存在且其 step 的工序 == 批次当前工序 | 批次当前工序能在链内定位（dispatch / 顺工序推进后的正常形态） |
| 2 | `current_process_id IS NULL` 且链内有活跃 step | 批次**尚未定位工序**但工单有链（PENDING / 未下发） |

两条分支互斥（分支 1 蕴含 `current_process_id IS NOT NULL`），故可并列 `OR`。

⚠️ **必须用 `IS NOT NULL AND =` 而不是 `IS NOT DISTINCT FROM`**：后者在 `NULL = NULL` 时为真，会让未定位（`current_process_id IS NULL`）的批次走分支 1（`cs.process_id` 也是 NULL，与 NULL 比「相等」），把「还没进任何工序」的批次也画上绿框。

⚠️ `t_process_chain_step cs` 一律 **LEFT JOIN**（`cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL`）：INNER 会让无 step 的批次（无链工单的常态）从列表里整批消失 —— 那比给错边框更糟。

⚠️ **本判据与 Rust 侧 `ChainPosition::is_pointer_consistent` 只是「近似判据」，不是同一判据**（2026-10-09 登记）：两者共同的那条只有「指针 step 的工序 == 批次当前工序」。本表达式只看 SQL 可表达的形状，判不了下面三种形态，而 `is_pointer_consistent` 会判 `false`：

| 形态 | 本表达式（绿框） | `is_pointer_consistent`（写侧闸门） |
|---|---|---|
| 链行已软删（`t_part_process_chain.deleted_at` 非空）但链内 step 仍活跃 | 分支 1 成立 ⇒ `true` | `false`：锚链 JOIN 要求 `pc.deleted_at IS NULL`，查不到链行 ⇒ 位置解析无行 |
| 链内同一 `process_id` 出现多次（后端照收的合法脏形态） | 分支 1 成立 ⇒ `true` | `false`：`hit_count > 1` 门控掉，`current_step_id` 保持 `NULL` |
| 指针 step 属于**另一条**链 | 分支 1 成立 ⇒ `true` | `false`：按 `pb.current_process_id` 在锚链内重新定位，命中的 step 不是指针 |

写成「同一判据 / 同款判据的纯 SQL 表达」是错的：把这三条搬进列表 SQL 等于把整个 `CHAIN_POSITION_LATERAL_SQL` 塞进上表 4 处 SQL，代价与逐批次 LATERAL 的查询成本都不接受（本轮**刻意不做**）。

**因此绿框的语义按「近似」理解**：它表示「有链、且指针 step 的工序与批次当前工序对得上」，是前端**提示**（可免填下一道工序），**不是**安全保证。真正决定放回时能否免填的是 `is_pointer_consistent` —— 两者不一致时以写侧为准：放回端点会要求显式指定下一道工序，拒收而非静默错值。

形态 ① 的**写侧只对齐了一半**：dispatch 侧的 `ProcessChainRepo::first_step_in_chain` 补上了 `t_part_process_chain.deleted_at IS NULL` 闸门，该形态的 dispatch 落 `20702`（见 §3.1），与读侧 `chain_state = NONE` 口径一致；**但 worker-scan RETURNED 的显式分支尚未对齐** —— `is_pointer_consistent` 因锚链 JOIN 落空而落 false，于是要求前端显式传 `next_process_id`，而解析该工序 step 的 `resolve_step_id_by_process` 不带链行闸门，**在已软删链里照样解析出活跃 step 并落库** ⇒ 绿框给 `true`、闸门放行，与 dispatch 的 `20702` 矛盾。分叉登记见 §8.4「锚链软删的写侧分叉」。

形态 ② ③ 同样仍可分叉，且三种都靠列表 SQL 自身无法判别。

**4 处落点**（四处必须同改，共用同一常量）：

| # | 域 | 端点 / 出参 |
|---|---|---|
| a | `prod::queue` | `GET /prod/queue/processes/{id}` → `items[].has_process_chain`（`QueuePoolItem`） |
| b | `prod::queue` | 同上 → `workers[].held_batches[].has_process_chain`（`QueueHeldBatch`） |
| c | `outsource` | `GET /outsource-queue/processes/{id}` → `items[].has_process_chain`（`OutsourceQueueCandidate`） |
| d | `prod::scan` | `GET /prod/scan/pickable` 与 `GET /prod/scan/held` → `items[].has_process_chain`（`ScanListItem`）。2026-10-10 自 part 域迁入报工台域，判据常量不变 |

`PartListItem` 本身仍是 `part` / `assembly` / `com::union_list` 三个域共用的 VO（wire 上共 4 个端点；`outsource` / `wx` 只在注释里拿它做字段对照、各有自己的 VO，不算），但 2026-10-10 报工台两条 list 端点连同本列迁往 `prod::scan`（行 VO 换成 `ScanListItem`）后，它**已无任何填真值的端点** —— 本域是上表行 a / b，`outsource` 是行 c。其余构造点（`From<TPart>` /
`com::union_list` 的两个 project 函数）显式填 `false`：链位置是**批次级**事实，
part 级行无从推导（没有 `#[serde(default)]`，漏赋值会编译失败）。

## 3. 下发流 VO（端点 3 / 4 / 5 / 6）

- `PendingBatchListOut`：`{ items: PendingBatchItem[], total, limit, offset }`。
- `DispatchResult`：`{ succeeded: DispatchSuccessItem[], failed: DispatchFailureItem[] }`。`failed` **当前总为空**（保留为 partial commit 启用预留）；任一 target 失败 → service 抛错 → handler tx Drop 全回滚。
- `AutoDispatchResult`：`{ items: AutoDispatchItem[] }`。`skip_reason` ∈ `NOT_FOUND` / `NO_PROCESS_CHAIN` / `NO_PROCESS_STEP` / `NO_SHELF` / `null`（可下发）。
- `RecallOut`：**3 字段** `{ batch_id: string, part_id: string, version: number }`。`version` 是写入后的 `version + 1`（OCC 锚），前端下一次对本批次的操作必须带这个值。

### 3.1 端点 4 的下发口径（2026-10-09）

**有链时按下发链首工序 + 指针落链首 step；无链时回落 `target_process_id`。**

| 工单形态 | `current_process_id` | `current_process_step_id` | 货架解析基准 |
|---|---|---|---|
| `t_part.process_chain_id IS NOT NULL` | 链内第一道未软删 step 的 `process_id`（`sort_order ASC, id ASC LIMIT 1`） | 链首 step 的 id | 链首工序 |
| `t_part.process_chain_id IS NULL`（手工工单的常态） | 请求里的 `target_process_id` | `NULL` | 请求里的 `target_process_id` |

- **有链时请求里的 `target_process_id` 被忽略**（它只是无链时的回落值）。前端要展示「实际下发到哪道工序」读 `DispatchSuccessItem.current_process_id` 即可。
- ⚠️ **出参的 `target_process_id` 不是请求字段的回声**（2026-10-10 登记）：service 解析链首时用 `let (target_process_id, …)` 遮蔽了同名形参，并把遮蔽后的值**同时**填进 `current_process_id` 与 `target_process_id` 两个出参字段 ⇒ **有链工单下两者同值（都是链首工序）**，无链工单下才是请求里的回落值。读出参 `target_process_id` 也能拿到「实际下发到哪道工序」，但**不能**用它复现「用户当时传了什么」——那在有链工单下已丢失。
- 与端点 5 `auto_dispatch_preview` 的 `first_process_id` **同源**（都取链首，见 `ProcessChainRepo::first_step_in_chain`），故「照 preview 显示的值操作」与「实际落库」一致。
- 链首解析带**锚链软删闸门**（`ProcessChainRepo::first_step_in_chain` JOIN `t_part_process_chain` 且 `pc.deleted_at IS NULL`），与读侧 `resolve_chain_position` 的锚链 JOIN 同口径。
- **锚链已软删**（`t_part.process_chain_id` 仍指向已删链，链内 step 未随链软删）**或**链行活跃但**链内一个未软删 step 都没有**（已清空）→ `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`（HTTP 404），批次保持 `PENDING` 不被写脏，`current_process_id` / `current_process_step_id` 都不落。不新造错误码：`20702` 原语义是「链内找不到某工序」，本处是「链内一道都没有」，两者都指向同一个动作 —— 去修链。
- `shared::batch::status::BATCH_STATUS_UPDATE_SQL` 的 `current_process_step_id = CASE WHEN $14 THEN NULL ELSE COALESCE($6::bigint, current_process_step_id) END` 早已支持两种写法，dispatch 侧只改传参（`new_process_step_id` / `clear_process_step_id`），**SQL 文本逐字未动**。`clear_process_step_id` 与 `new_process_step_id.is_none()` 配对：有链写链首 step（`false`）、无链清 step（`true`，与本口径改动前逐字一致）。无链侧不采用「保留原值」写法 —— 那需要论证「无链批次的 step 恒为 NULL」这条**无任何约束保证**的不变式（`allowed_from` 之外的旁路写点、手工 SQL、历史脏数据都能破坏它）。
- 出参 `DispatchSuccessItem.current_process_step_id` 由「恒 `null`」改为**真实写入值**（JSON **字符串**，走 `serialize_i64_opt`，与同 VO 的 `batch_id` 等同形态；`None` → `null`）。⚠️ 不能落成 JSON number：step id 是雪花 id（量级 8.7×10¹⁷），远超 JS 的 `Number.MAX_SAFE_INTEGER`（2^53 ≈ 9.007×10¹⁵），number 进 JS 会被舍入。

## 4. 口径表

### 4.1 候选池判据（`status` / `location` 两列在 3 处一致，**货架 JOIN 不一致**）

```
status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF'
```

`status` / `location` 这两列是「一个批次在某道工序的候选池里」的判据，被 3 处共用：

| 用途 | SQL 位置 | `current_process_id` 闸门 | 货架范围 | `t_shelf` JOIN |
|---|---|---|---|---|
| 序列板各工序计数（端点 1） | `board/repo.rs::SQL_POOL_COUNT_BY_PROCESS` | `IS NOT NULL` | 跨全部货架 | **无** |
| 单工序候选池明细（端点 2 `items[]`） | `board/repo.rs::SQL_POOL_ITEMS_BY_PROCESS` | `= $1` | 跨全部货架（明细本身 INNER JOIN `t_shelf` 顺带展示架信息） | **INNER**（`s.id = pb.current_holder_id AND s.deleted_at IS NULL`） |
| 抢占 `take_one_from_pool`（refill） | `repo/sql.rs` | `= ANY($3)` | **跨全部货架**（`$2::bigint IS NULL OR pb.current_holder_id = $2`） | **无** |
| 抢占 `take_specific_from_pool`（admin 单批） | `repo/sql.rs` | 无（按 `batch_id` 定位） | **限架**（`pb.current_holder_id = $2`，必传） | **无** |
| 撤回候选池 `POST /queue/move`（WORKER→POOL） | 不经本 SQL（走 `pick_least_loaded` + `part_mark_batch_returned`） | 不改（`COALESCE` 保留） | 目标架**自动选**（`to` 侧无 `shelf_id`） | — |

### 「货架范围」列的口径（2026-10-10）

- **refill（`take_one_from_pool`）不再有架锚**：`$2::bigint IS NULL` 时候选跨全部
  活跃生产架。worker-scan 的 `shelf_id` 入参已删除（目标架由
  `shared::shelf::select::pick_least_loaded` 选），refill 若还按架过滤就会在
  「放回到 A 架 → 随即从 A 架补料」这个闭环里查空池。**负载均衡的整体职责在放回时
  的选架一侧**。
- 管理员端点仍是限架：`POST /prod/queue/refill`（`AdminRefillRequest.shelf_id`）与
  `POST /prod/queue/auto-allocate`（`AutoAllocateRequest.shelf_id`）的 `shelf_id`
  **保留必填**，handler 传 `Some(req.shelf_id)` 进 `take_one_from_pool` —— 它们是
  「为某工人在某架上抢料」的显式管理员操作。
- `POST /prod/queue/move` 的 POOL→WORKER 方向不经本 SQL（走
  `take_specific_from_pool`），其 `from.shelf_id` 语义未变（仍是「批次真实所在货架」）。
  WORKER→POOL（撤回候选池）方向的目标架 2026-10-10 起**不再由调用方指定**，改走
  `shared::shelf::select::pick_least_loaded`（见 §2.8）。

### 取件优先级（2026-10-10 统一）

看板池明细与 refill 取料共用**同一份** `ORDER BY` 片段
（`shared::shelf::pool_priority::POOL_PRIORITY_ORDER_SQL`），4 层语义与取舍见该常量
的 doc：

1. `p.is_urgent DESC` —— 加急在前（人工判定的例外，优先级高于系统交期）
2. `p.system_delivery_date ASC NULLS LAST`
3. `p.planned_delivery_date ASC NULLS LAST`
4. CNC 工序内已上传 G_CODE 的批次优先（`pr.is_cnc` 门控）
5. `pb.id ASC` —— 稳定兜底（`FOR UPDATE SKIP LOCKED` 的前提）

⚠️ 本节之前两处排序**已经漂移**：refill 把「已编程」排最前，看板把「系统交期」排最前
⇒ 工人以为在按加急抢货，板子上却是另一套顺序。现已收口为一份片段。

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

queue 域**整体不适用**域隔离护栏：它继承 worker_pool 的「经本域 trait 转发其它域单表查询」pattern（`repo/mod.rs::QueueRepoTrait` 转发 `worker` / `work_type` / `process` / `process_chain` / `part` / `shelf_process`），这是逐域剥离期间的既定 pattern（与 assembly / `iam::shelf` / process_chain 同形）。**这是本域唯一被允许的跨域面**。

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
10. **4 处卡片 DTO 新增 `has_process_chain`（boolean）**（2026-10-09）：`QueuePoolItem` / `QueueHeldBatch` / `OutsourceQueueCandidate` / `ScanListItem`。第四处原先是 `PartListItem`，2026-10-10 随报工台两条 list 端点迁往 `prod::scan`（行 VO 换成 `ScanListItem`）后它在 part 域恒为 `false`，字段保留只为不动其余复用该 VO 的域的 wire 形状。它是**绿色左边框的判据**，判据见 §2.6。zod 侧按 `z.boolean()` 声明 —— 前端不要按「工单有没有绑链」重新在前端推一遍（后端已经算好，且未定位批次走的是另一个分支）。
11. **`POST /queue/move` 的 `version` 升为必填**（2026-10-09）：此前三个方向都由 service 用「本次事务里刚读到的 `batch.version`」当 `expected_version`，等价于**没有 OCC** —— 看板数据是 30s 缓存的快照，期间他人改过批次时「用户看到 5 件 → 实际移动 3 件」会静默成功。值取候选卡 / 持有卡的 `version`；漏传 → **HTTP 422 纯文本**（`version` 无 `#[serde(default)]`）。

### 8.4 已知偏差登记

---

**`POST /queue/move` 的 `from` / `to` 从一个 `MoveLocation` 拆成两个类型（2026-10-10）**

- `MoveLocation::Pool { shelf_id }` 一个变体同时服务两侧，导致 `to` 侧也被迫要求
  `shelf_id`。目标架自动选之后该字段在 `to` 侧没有角色可留，而前端**不发**它 ⇒
  axum `Json` 提取器直接 422 纯文本，撤回候选池对所有角色都不可用。
- 拆成 `MoveFromLocation`（`Pool { shelf_id }` 必填）+ `MoveToLocation`（`Pool` 无字段）
  之后，「`from` 需要架、`to` 不需要」这个不对称才在类型上可表达。
- 契约：§2.8。选架口径与错误码见 [`shelves.md`](shelves.md) §4。
- ⚠️ `MoveResult.new_holder_id` 在 WORKER→POOL 方向是**服务端选的架**，客户端无法预知，
  必须从响应（或刷新后的批次详情）读。

---

**`ShelfProcessRepo::find_first_shelf_for_process` 已无调用方**（2026-10-10 登记）。

dispatch 的目标货架自 2026-10-10 起改走
`shared::shelf::select::pick_least_loaded`（按 `current_load / capacity` 升序），
该方法（`t_shelf_process` 上 `sort_order ASC, id ASC LIMIT 1`）因此**失去唯一调用方**。

**保留不删**，理由与后续处置：

- **口径已不同**：新的退化路径（候选集里全部架都没配 `capacity`）在选架 SQL 内
  自然退化成 `display_order ASC, id ASC`，而旧方法是 `t_shelf_process.sort_order ASC`
  —— **两者的「第一」不是同一个**。同一道工序在两种口径下可能落到不同的架（映射行
  的 `sort_order` 与货架的 `display_order` 是两套独立的人工排序）。
- **下一轮决定**：要么删（判定它已无价值），要么复用为「全部架都不限容量时按映射
  顺序取首个」的显式退化路径（那样就要把 `display_order` 与 `sort_order` 的优先级
  写进选架 SQL，并同步改那条退化路径的文档与测试）。**不要**在没想清楚这两套排序的
  关系之前就把它接回去。

---

**`capacity IS NULL OR <= 0` 视为「不限」时，选架退化到 `display_order ASC, id ASC`**
（2026-10-10 登记，与上一条同源）。

存量货架的 `capacity` 全为 NULL（migration 未 backfill，容量未知），故生产库现状下
选架**恒走这条退化路径**。此时排序等价于「按人工排的物理顺序取第一个可用架」，
与 2026-10-10 之前 dispatch 的行为相近但不完全相同（见上一条：映射 `sort_order` vs
货架 `display_order`）。要让负载均衡真正生效，需要在货架管理页给货架配容量；
在此之前，本仓**不对退化路径与旧口径的差异做补偿**。

---

**存量批次的 `current_process_step_id` 为 NULL 或陈旧**（2026-10-09 新增登记）。

`current_process_step_id`（链内位置指针）自 2026-10-09 起才真正随工序推进：dispatch 落链首 step、worker-scan RETURNED 顺工序时推进到下一 step。**在此之前走过至少一次领取 / 放回的存量批次，其指针恒为 NULL**（dispatch 旧实现 `clear_process_step_id: true` 把它清成 NULL，RETURNED 旧实现算出了下一个 step 却丢弃），另有一批「指针停在首次定位那一步」的陈旧数据。

影响面与自愈路径：

- 读侧 `GET /prod/scan/held?worker_id=` 的 `chain_state` **不受影响** —— 它按 `current_process_id` 在锚链内重新定位（纪律见 `shared::batch::chain` 模块 doc），不依赖指针。
- 写侧 worker-scan RETURNED 的「自动推进」分支**对存量批次不生效**：指针为 NULL 或陈旧时 `ChainPosition::is_pointer_consistent` 为 false，走「要求前端显式指定 `next_process_id`」分支。前端体验上就是「本来能免填的字段现在要填」，功能不受损。
- `has_process_chain` 那类按「指针的工序 == 当前工序」判定的卡片列，在 `current_process_id` 非 NULL 时对陈旧指针与 NULL 指针同样落 `false`，语义自洽。
- 自愈需要**重新走一次会重定位指针的流转**（再走一次 dispatch，或 worker-scan RETURNED）才会被纠正。⚠️ **`POST /queue/move`（admin 主动退回，`WORKER → POOL`）不算**：它传 `new_process_step_id = None` + `clear_process_step_id = false`，`COALESCE` 保留原值、**刻意不重定位** —— 管理员的意图是「退回候选池让人重领」，不表达任何链上位置意图。存量数据清洗不在本轮范围内；前端不要把「绿色左边框缺失」当成新缺陷上报。

---

**`has_process_chain` 绿框与放回端点的「可免填」判据分叉（3 种形态）**（2026-10-09 新增登记）。

绿框判据（`HAS_PROCESS_CHAIN_EXPR`，纯 SQL）与放回端点的闸门（`ChainPosition::is_pointer_consistent`，Rust）**不是同一判据**，只共同覆盖「指针 step 的工序 == 批次当前工序」。分叉形态逐条见 §2.6 的表。现状与决议：

- 形态 ①（链行已软删、step 仍活跃）**写侧只对齐了一半**：dispatch 侧的 `ProcessChainRepo::first_step_in_chain` 补了 `t_part_process_chain.deleted_at IS NULL` 闸门，该形态的 dispatch 落 `20702`（§3.1），与读侧 `chain_state = NONE` 一致；**worker-scan RETURNED 的显式分支仍会放行**并写下悬空 step 指针 —— 详见本节下一条登记。
- 形态 ②（链内 `process_id` 重复）③（指针 step 属于另一条链）**仍会分叉**：绿框给 `true`、放回要求显式指定下一道工序。**本轮刻意不修** —— 修它要把 `CHAIN_POSITION_LATERAL_SQL` 整块搬进 4 处列表 SQL，代价与逐批次 LATERAL 的查询成本都不接受。**后果是可接受的**：分叉方向永远是「绿框误报可免填 → 放回端点拒收并要求显式指定」，即**提示偏松、闸门偏严**，不会静默落错值。
- **前端不要拿绿框当安全保证**，只当提示；「免填对话框」仍应处理「用户没填 / 填了但写端点返 `20701`/`20702`」这条路径。

---

**锚链软删的写侧分叉：dispatch 拒收 / worker-scan 显式分支放行**（2026-10-09 新增登记）。

形态 ①（`t_part_process_chain.deleted_at` 非空、链内 step 仍活跃）在写侧**只对齐了一半** —— 两个写入口的 step 来源不同，只有一个带了链行闸门：

| 写入口 | step 来源 | 链行软删闸门 | 该形态下结果 |
|---|---|---|---|
| dispatch | `ProcessChainRepo::first_step_in_chain` | 有（`JOIN t_part_process_chain … AND pc.deleted_at IS NULL`） | 落 `20702` 拒收 |
| worker-scan RETURNED 的**显式分支** | `optional_step_id` → `ProcessChainRepo::resolve_step_id_by_process` | **无**（只查 `t_process_chain_step`，`WHERE chain_id=$1 AND process_id=$2 AND deleted_at IS NULL`，不 JOIN 链行） | **放行**，并写下悬空 step 指针 |

可达路径（代码级确定）：锚链软删 ⇒ `resolve_chain_position` 的锚链 JOIN（`pc.deleted_at IS NULL`）落空 ⇒ 派生子查询无行 ⇒ `current_step_id = NULL` ⇒ `is_pointer_consistent` 落 false ⇒ RETURNED 走**显式分支**、要求前端传 `next_process_id`；而 `optional_step_id` 经 `resolve_step_id_by_process` 只按 `chain_id` + `process_id` 找 step，**链行软删不影响它命中**，于是解析出 `Some(step_id)`，随 `mark_batch_returned` 落库。

**后果（悬空 step 指针）**：该批次的 `current_process_step_id` 指向一条**软删链**的 step，于是

- 读侧 `GET /prod/scan/held?worker_id=` 的 `chain_state` 恒 `NONE` —— 它按锚链 JOIN 重新定位，链行软删就无行；
- 绿框 `HAS_PROCESS_CHAIN_EXPR` 走分支 1（`cs.process_id = pb.current_process_id`，该表达式不 JOIN 链行）⇒ 给 `true`，与上一条矛盾；
- 后续每一次 worker-scan RETURNED 都重新落回显式分支（`is_pointer_consistent` 永远 false）⇒ 该批次**永远无法自动顺工序推进**，前端每次都要显式填 `next_process_id`。

即「提示偏松 + 自动推进被永久打断」，**不是静默错工序**（下一道工序仍由前端显式给值，链读不出来也猜不出来）。修复链（恢复软删链或换绑）后自愈：重新定位出的指针会落回活跃链。

**本轮刻意不修**：给 `resolve_step_id_by_process` 补同一道链行闸门，会改变**全部 8 个 `optional_step_id` 调用点**的既有行为（`prod/batch/service/shelf.rs`、`programming.rs`、`transition_core.rs`、`repair.rs` 两处、`outsource/move.rs` 两处、`worker_scan.rs`），其中多处当前依赖「链软删时仍能解析 step」的历史行为，需**独立一轮逐点核对**后再改。`first_step_in_chain` 已有的闸门保留不动。

---

**`auto_dispatch_preview` 与 dispatch 对「锚链已软删」分叉**（2026-10-09 新增登记）。

端点 5 的 LATERAL 取链首 step 时**不 JOIN `t_part_process_chain` 行**，而端点 4 的写侧现在带了锚链软删闸门（§3.1）。故「锚链已软删、链内 step 仍活跃」这一形态下：preview 仍会报「可下发到工序 X」，实际 dispatch 落 `20702`。

**本轮刻意不改 preview 的 SQL**：① 它只影响**预览提示**、不影响任何实际写入（写侧才是闸门，且已经拒收）；② 改它要动 `.sqlx/` 离线缓存（preview 走 `query!` 宏），为一处提示分叉付维护成本不划算。**后果**是 preview 可能报出一个 dispatch 会拒的工序 —— 报错文案已写明「链已软删或已清空」，运营按提示去修链即可。改 preview 前请先回到本节确认决议是否仍然有效。

**外协在途卡不画绿框（有意的不对称）**（2026-10-09 新增登记）。

绿框的 4 处落点里，外协只加在**候选卡** `OutsourceQueueCandidate`，同屏的**在途卡** `OutsourceQueueHeldBatch`（外协公司列，消费形态与候选卡同款）**刻意不带**这一列。理由：在途批次已发到外协公司、不在厂内工序链上，「按链顺推到下一道」这个语义在收发阶段不成立；给它加绿框等于宣称「这批货能免填下一道工序」，而收发端点的 `next_process_id` 走的是**另一套**判据（`OutsourceQueueHeldBatch.chain_resolvable` = `receive_next_process_id != "0"`）。两套判据并存时给在途卡画绿框会误导。**待决议**：若将来外协收回也要支持「按链顺推」，届时应统一到 `chain_resolvable`，而不是补一个 `has_process_chain`。

---

**端点 1 的 `pool_count` 可能大于端点 2 的 `items.length`（差值 = 指向已软删货架的批次数）。**

成因：端点 1 的计数 SQL（`SQL_POOL_COUNT_BY_PROCESS`）不带 `t_shelf` JOIN；端点 2 的明细 SQL（`SQL_POOL_ITEMS_BY_PROCESS`）带 **INNER JOIN `t_shelf s ON s.id = pb.current_holder_id AND s.deleted_at IS NULL`**。`current_holder_id` 是「货架还是工人」同列承载的复用列，理论上可残留一个已被软删的货架 id —— 这样的批次命中计数（它确实是 `IN_PROCESS` + `PRODUCTION_SHELF`）却不命中明细（拿不到货架行）。

现有测试抓不到这个跨端点分歧：`board_snapshot_matches_legacy_pool_counts` 的两个断言都不带货架 JOIN（它对照的是被取代的旧 `pool_counts_all_shelves` 口径，两者都不 JOIN `t_shelf`）。

产品决议（2026-10-08）：**暂不处理**。已软删货架上的批次本就是需要人工清理的脏数据，让它在明细里消失反而符合「不该被认领」的直觉；口径差留给将来做货架软删清理时一并收敛（届时把明细的 INNER JOIN 改成 LEFT JOIN + 占位，或给计数 SQL 补同一套货架闸门）。改这两条 SQL 时请先回到本节确认决议是否仍然有效。
