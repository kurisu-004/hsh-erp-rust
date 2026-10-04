# Lifecycle 端点 —— t_part_batch 生产流转（2026-10-02 起归 prod 域）

> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
> 状态机 / 错误码见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)
>
> **归属（2026-10-02 变更）**：本文件中以**单个批次**为操作对象的端点 URL 已从
> `/api/v2/parts/*` 迁到 `/api/v2/prod/batches/*`，路径锚点由 `part_id` 改为 `batch_id`，
> `batch_id` 同时从请求体删除。**仍留在 part 域的只有多批次动作**：
> `POST /api/v2/parts/{part_id}/cancel`（翻转该 part 全部活跃批次）与
> `POST /api/v2/parts/{part_id}/force-complete`（全部非 CANCELLED 批次）。
> 判据：操作对象是「一个批次」还是「多个批次」。
>
> 范围：CRUD / inspection 见 [`./crud.md`](./crud.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录

- [POST /api/v2/prod/batches/{batch_id}/deliver](#post-apiv2prodbatchesbatch_iddeliver)
- [POST /api/v2/parts/{part_id}/cancel](#post-apiv2partspart_idcancel)（**留 part 域**：多批次动作）
- [POST /api/v2/prod/batches/{batch_id}/complete](#post-apiv2prodbatchesbatch_idcomplete)
- [POST /api/v2/prod/batches/{batch_id}/start-repair](#post-apiv2prodbatchesbatch_idstart-repair)
- [POST /api/v2/parts/{part_id}/force-complete](#post-apiv2partspart_idforce-complete)（MANAGER 单角色强推逃生通道；**留 part 域**）
- [GET /api/v2/parts/pickable-by-work-type/{work_type_id}](#get-apiv2partspickable-by-work-typework_type_id)（**2026-10-04 新增章节**；SHELF_ACCOUNT scope 收口）
- [GET /api/v2/parts/pending-programming](#get-apiv2partspending-programming)

---

### `POST /api/v2/prod/batches/{batch_id}/deliver`

权限: **Manager / Clerk**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

> 本端点是 **batch 级**动作，OCC 锚定 `t_part_batch.version`。前端需先调
> `GET /parts/by-serial/{serial_no}/part-batches` 拿 `batch_id` + `version`，
> 再把 `batch_id` 放路径、`version` 放 body。

Request：`DeliverRequest`（body 必填）

```json
{
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

业务流转：`READY_TO_SHIP → DELIVERED`（batch 级）；part 派生列由
rollup 自动回填；事件日志 `DELIVERED` 的 `batch_id` / `quantity` 来自
操作的批次。

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 流转后工单。

错误码：

- 20101 — 批次所属 part 已软删（2026-10-02 起本端点无 part 路径参数，20101 只能经此路径触发）
- 20104 — status 字符串非法
- 20109 — **2026-10-02 语义收窄**：batch 不存在 / 已软删 / 状态不是流转起点
- 20115 — part 已 CANCELLED
- 20117 — batch 当前状态非 READY_TO_SHIP（状态机白名单拒绝）
- 40901 — 乐观锁失败（batch version 冲突）

### `POST /api/v2/parts/{part_id}/cancel`

权限: **Manager / Clerk**

> **2026-10-02：留在 part 域** —— 操作对象是「该 part 的全部活跃批次」（多批次动作），
> 不是单个批次，故不走 `/api/v2/prod/batches/{batch_id}/*`。单个批次的取消走
> `POST /api/v2/prod/batches/{batch_id}/cancel`。

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

**2026-10-01 订正**：cancel 级联的是该 part 下**全部非终态活跃批次**（不再只是
「最近一条 source-status 批次」），`status = 'COMPLETED' | 'CANCELLED'` 的批次不在
级联范围内。作废同时清空 `t_part.serial_no`（作废即退役，序列号**不**归档）；
父装配件 `t_assembly` 会被派生追平（唯一子件作废 → 父件 CANCELLED，父件序列号
同步释放）。

> **不变式（2026-10-01）**：`t_part.status` 由本端点的主操作
> 写下，级联批次的派生**不得**覆盖它。若该 part 存在已 COMPLETED 的批次，
> min-progress 会算出 COMPLETED —— 实现靠两道闸拦住：bulk 入口的
> `PartDerivation::KeepPartTerminalAsIs`（显式跳过 part 写）+ `update_part_rollup`
> 的 `status NOT IN ('COMPLETED','CANCELLED')` 终态守卫（SQL 层兜底）。
> 回归测试：`tests/part/lifecycle.rs::cancel_part_is_not_overwritten_by_rollup_completed`。
> 副作用：已终态的 part 不再被 rollup / admin 对账改写（见
> [`../admin.md`](../admin.md)）。

错误码：

- 20101 — part 不存在 / 软删
- 20103 — 当前状态不在 cancel 白名单（COMPLETED / CANCELLED 等终态）
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED
- 21420 — part 已挂送货单，禁 cancel
- 40901 — 乐观锁失败

### `POST /api/v2/prod/batches/{batch_id}/complete`

权限: **Manager / Clerk**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

> batch 级动作，OCC 锚定 `t_part_batch.version`（与 inspection 三流一致）。状态机守卫读
> batch 当前状态 `DELIVERED → COMPLETED`；part 终态后 `serial_no` 被清空（序列号已
> 转交送货单）。

Request：`CompleteRequest`（body 必填）

```json
{
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — 批次所属 part 已软删
- 20109 — **2026-10-02 语义收窄**：batch 不存在 / 已软删 / 状态不是流转起点
- 20115 — part 已 CANCELLED
- 20116 — batch 当前状态非 DELIVERED（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/prod/batches/{batch_id}/start-repair`

权限: **Manager / Clerk / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

> batch 级动作，OCC 锚定 `t_part_batch.version`。
>
> **2026-10-01 起 REPAIRING 降级为布尔标记列**
> `t_part_batch.is_repairing`（migration 005/006）。本端点**不再发生 status
> 迁移** —— 守卫条件由「状态机 `IN_PROCESS → REPAIRING`」改为
> **「`status='IN_PROCESS'` 且 `is_repairing = false`」**，命中后只把
> `is_repairing` 置 `true`（`status` 保持 `IN_PROCESS`）。
>
> - 重复起修（`is_repairing` 已为 `true`）→ 20118 `BIZ_PART_REPAIR_NOT_TRIGGERED`。
> - 起修**不翻转 status**：`t_part_batch.status` 保持 `IN_PROCESS`（progress 与原
>   REPAIRING 同档 2）；派生列 `t_part.status` 按 min-progress 取活跃批次里最慢的
>   那条（`compute_part_target`，见 `src/modules/part/statemachine.rs`），故
>   **不再出现 `'REPAIRING'`**，但**不保证恒为** `IN_PROCESS`（同 part 下存在更慢的
>   活跃批次时取那条）—— 不可写成「恒为 `IN_PROCESS`」的不变式。
>
> 2026-09-16（migration 027）：`has_been_repaired` 列已从 `t_part` 与
> `t_part_batch` 双删 —— 拆批后无法确定是哪一个批次返修，列语义失真整体废弃。
> 返修事实改由 `t_part_batch.is_repairing` 列 + `t_part_event.event_type=
> 'REPAIR_STARTED'` 事件日志共同追溯。事件 `from_status` / `to_status` 均写
> 真实值 `IN_PROCESS`（status 未变，变的是标记位）。

Request：`StartRepairRequest`（body 必填）

```json
{
  "version": 0,               // 必填；batch.version（OCC）
  "reason": "string (可选)",  // 优先作为事件 note
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — 批次所属 part 已软删
- 20109 — **2026-10-02 语义收窄**：batch 不存在 / 已软删 / 状态不是流转起点
- 20115 — part 已 CANCELLED
- 20118 — batch 当前状态非 IN_PROCESS（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/force-complete`

权限: **Manager 单角色**（明确不下放 Clerk —— 强改逃生通道）

> **2026-10-02：留在 part 域** —— 操作对象是「该 part 下全部非 CANCELLED 批次」
> （多批次动作），故仍以 `part_id` 为锚。

> ⚠️ **强改语义**：
> - **完全绕状态机**：非 `CANCELLED` / 非 `COMPLETED` 状态可被强推到 `COMPLETED`。
> - **不走 OCC**：force-complete 是逃生通道，依赖 SQL 行锁串行化（`t_part_batch` 行锁
>   + `t_part` 行锁），不收 `version`、不要求 caller 侧 batch_id 选择。
> - **批次处理**：单 SQL 强推该 part 下所有活跃批次（非 `CANCELLED`、非软删）→
>   `COMPLETED`，复用 `sync_from_batch_change` rollup 让 `compute_part_target`
>   自动派生 `part.status='COMPLETED'`。
> - **终态清理**：复用现有 `clear_part_serial_no_when_completed` 清空 `serial_no`。
> - **事件日志**：`t_part_event.event_type='FORCE_COMPLETED'`（区别常规 `COMPLETED`），
>   `note` 自动加 `[FORCE]` 前缀（即便用户未传 note 也会写入 `[FORCE] ` 空字符串）以便审计追溯。
> - **WS 广播**：`PART_FORCE_COMPLETED`（区别 `PART_COMPLETED`），前端订阅 dashboard
>   可监听。

Request：

```json
{
  "note": "string (可选；将自动加 [FORCE] 前缀写入事件日志)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 强推后的工单。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED（终态不可被强推，语义对称 20123）
- 20123 — part 已 COMPLETED（幂等拒绝，避免重复强推副作用）
- 40300 — 无权限（仅 MANAGER 单角色）

---

## Lifecycle 专属 DTO

### DeliverRequest / CompleteRequest / StartRepairRequest 字段（batch 级）

三者的 `batch_id` 均在**路径参数**里（`POST /api/v2/prod/batches/{batch_id}/<action>`），
**body 内已无 `batch_id` 字段**（2026-10-02）。body 必填 `version`（锚
`t_part_batch.version`）+ 可选 `note` / `reason`（≤ 500 字符建议）。事件日志
`batch_id` / `quantity` 来自操作的批次；start-repair 优先取 `reason` 作为事件 note。

### CancelRequest 字段（保持 part 级）

仅含可选 `reason` / `note`（cancel 走 part 级 + 级联取消全部活跃批次）。

### ForceCompleteRequest 字段（2026-09-30 新增，MANAGER 单角色强推逃生通道）

仅含可选 `note`（≤ 500 字符建议；服务端自动加 `[FORCE] ` 前缀写入事件日志）。
不收 `batch_id` / `version`（绕 OCC）；service 层收尾时会复用现有
`complete` 路径的 `clear_part_serial_no_when_completed` + `sync_from_batch_change`
rollup 让 `part.status='COMPLETED'` 自动落地。

---

### `POST /api/v2/prod/batches/{batch_id}/pick-up`

权限: **Manager / Clerk / ShelfAccount**（扫码台即 ShelfAccount 角色）

> P3 pickup 端点（batch 级 OCC 集成）。用于把在制批次交到工人手上；`version` 锚定
> `t_part_batch.version`。起点状态：`PENDING` / `IN_PROCESS`（后者要求批次停在
> `PRODUCTION_SHELF` 上），目标 `IN_PROCESS + location=WORKER`。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`PickUpRequest`（body 必填）

```json
{
  "version": 0,               // 必填；batch.version（OCC）
  "worker_id": "42",          // 必填；拣货工人（须 is_active 且已绑 work_type）
  "shelf_id": "43",           // — 可选（2026-10-04 起）；缺省 = 完全不校验
  "quantity": "4",            // 可选；缺省 = 整批；小于总量时自动拆批（JSON 字符串）
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`（**响应体形状与整批领取完全一致**，与 `shelf_id` 传不传
无关；拆批信息只走 WS `PART_BATCH_SPLIT`）。

#### `shelf_id` 可选（2026-10-04 变更）

`shelf_id` 由**必填 `i64`** 放宽为**可选**（`Option<i64>`）。语义：

| 取值 | 行为 |
|---|---|
| 不传 / `null` | **不做任何校验、不推导、不回退**，请求照常受理 |
| `"shelf_id": "<id>"` | 校验「存在 + `is_active` + `zone='PRODUCTION'`」 |
| `"shelf_id": <数字>` | `422`（纯文本，非业务信封）——线上形态是 **JSON 字符串**（雪花 id 精度） |

> ⚠️ **本字段不影响任何持久化结果**（这是它可以变可选的根据）：
>
> - 不落库：pick-up 路径上 `t_part_batch` 的**全部 3 个写入点**（拆成 4 条 SQL）——
>   `pickup.rs` 的内联 `UPDATE`（IN_PROCESS 分支）、`guard.rs` → `status_gate.rs` 的
>   通用 `BATCH_STATUS_UPDATE_SQL`（PENDING 分支）、部分领取的
>   `split_batch_for_partial_pass`（= `_split_batch_inner` 的 `INSERT` 新批次 +
>   `UPDATE` 源批次 quantity 两条语句）—— 它们的 SET 与 WHERE **均无货架列、也无
>   货架条件**；
> - 事件无货架列：`t_part_event` 没有 shelf 字段，`PICKED_UP` / `SPLIT` 两条
>   事件都不记货架；
> - 响应无 shelf 字段：响应体是 `PartOut`，不含任何 shelf 属性；
> - `t_shelf` 零写入：那条校验内部只
>   `SELECT ... FROM t_shelf WHERE id = $1 AND deleted_at IS NULL`。
>
> ⇒ 原先那条校验是**防呆断言**（让手填错区的人当场看见 `20104`），**不是安全
> 边界**。缺省它，扫码台 / 看板等自动发起 pick-up 的调用方（本就无从知道「批次
> 此刻名义上在哪一个架」）才可正常调用。

> **为什么不做「从批次自身的 `current_holder_id` 推导货架」**（2026-10-04 逐条
> 核实后否决，技术上不可行）：
>
> 1. **PENDING 起点的批次 `current_holder_id` 恒为 `NULL`** ——
>    `create_initial_batch` 写死 `NULL, NULL`，推导不出任何值；而 pick-up 的
>    PENDING 分支正是给「待下发池」用的；
> 2. **IN_PROCESS 起点只守 `location='PRODUCTION_SHELF'`、不守 holder** ——
>    `dispatch` 与 `pool/move` 两个写点能把 INSPECTION 区的架写进
>    `current_holder_id`，推导出来的值可能根本不在 PRODUCTION 区；
> 3. **`current_holder_id` 可能指向已软删 / 非 `PRODUCTION` 区的架** ——
>    「推导 + 施加同样校验」会把这类批次**永久锁死**（既领不走、也不报错可解释）。
>    三条机制（**2026-10-04 review 第 1 轮订正**：原表述「货架被停用 / 软删时
>    holder 仍指向失效 id」按字面不成立 —— `deactivate` 与 soft-delete 是同一操作，
>    且 soft-delete 在被引用时会被 `20503 BIZ_SHELF_IN_USE` 拦住）：
>    （a）`dispatch` 的 `ShelfProcessRepo::find_first_shelf_for_process` 只按
>    `t_shelf_process.deleted_at IS NULL` 过滤、**不 JOIN `t_shelf`** ⇒ 既不过滤
>    `zone` 也不过滤 `is_active`，映射残留时会把已软删的架 id 写进
>    `current_holder_id`；
>    （b）soft-delete 的 `20503` 守卫（`ShelfRepo::count_in_use_parts`）谓词是
>    `location IN ('PRODUCTION_SHELF','INSPECTION_SHELF') AND status IN
>    ('IN_PROCESS','INSPECTION')` ⇒ `location='PRODUCTION_SHELF'` 但 status 落在该
>    集合之外的行**不被计入**；
>    （c）该守卫是「先 count、再 soft_delete」两条独立语句、中间无锁 ⇒ 并发上架可
>    穿过守卫（TOCTOU）。
>
> 故选择「缺省就什么都不做」，而不是替调用方猜一个值。

> ⚠️ **本字段无 scope 校验**。与 worker-scan 对照：后者对 `shelf_id` 走
> `current.can_access_shelf()`，越权返 `40301 SHELF_MISMATCH`
> （见 [`./inspection.md`](./inspection.md#post-apiv2prodbatchesworker-scan)）。
> pick-up **不做**该校验 ⇒ `shelf_id` 缺省时没有任何货架维度的权限收敛；本端点
> 唯一的权限边界是角色门 `require_any_role(&[Manager, Clerk, ShelfAccount])`。

#### 部分领取（2026-10-03 新增，`quantity`）

`quantity` 的三种落法：

| 取值 | 行为 |
|---|---|
| 不传 / `null` | 整批领取（既有行为） |
| `== batch.quantity` | 整批领取（显式写法，**不报错、不拆批**） |
| `0 < q < batch.quantity` | 自动拆批：拆出 `q` 件成交给工人的新批次，源批次留原处、数量递减 |
| `q ≤ 0` 或 `q > batch.quantity` | 拒：`20111 BIZ_PART_BATCH_INVALID_QUANTITY`（HTTP 400） |

> ⚠️ 与 `POST /api/v2/prod/batches/{batch_id}/split` 的数量语义**不同**：split 要求
> `q < batch.quantity`（等于即非法，因为整批无需拆）；pick-up 允许等于（等于就是整批
> 领取）。也不要与 `to-inspection` / `to-ship` 的 `op_qty` 混淆——那边 `op_qty >`
> 总量时按整批处理，pick-up 则直接报 20111。

拆批语义（同一事务内，与整批领取共用一个响应）：

- 新批次 = 被领走的那部分：`quantity = q`、`status` 与源批次相同、`batch_no =
  max + 1`、`parent_batch_id` = 源批次 id；继承源批次的 `location` /
  `current_holder_id` / `current_process_id` / `current_process_step_id` /
  `is_repairing`，随后被翻到 `IN_PROCESS + WORKER + current_holder_id=worker_id`；
- 源批次**保留原 `batch_no`**，只是 `quantity -= q`，位置与 holder 不变（余量仍在
  生产架上，可被下一个工人再领）；
- **OCC 仍锚源批次**：请求的 `version` 校验源批次、拆批 SQL 也以它为条件；新批次
  建出时 `version = 0`；
- 事件流留痕：`SPLIT`（`quantity = q`，`note = "pick-up 部分领取自动拆批"`）+
  `PICKED_UP`（`quantity = q`）两条，都挂在新批次上，可经
  `GET /api/v2/parts/{part_id}/events` 查到；
- WS（均在 commit 之后广播）：`PART_BATCH_SPLIT`（`part_id` / `new_batch_id` /
  `source_batch_id` / `quantity`）+ `PART_PICKED_UP`（`part_id` / `worker_id` /
  `batch_id` / `quantity`，整批路径下后两者即源批次与整批量）。

错误码（2026-10-04 逐条从代码核实补全；此前只列了 5 个，且其中 `20119` 与本端点
无关）：

| code | 名称 | HTTP | 触发条件 |
|---|---|---|---|
| 20109 | `BIZ_PART_BATCH_NOT_FOUND` | 404 | `batch_id` 不存在 / 已软删 |
| 20101 | `BIZ_PART_NOT_FOUND` | 404 | 批次所属 part 已软删 |
| 40901 | `VERSION_CONFLICT` | 409 | ① `version` 与 `t_part_batch.version` 不符；② `status_gate` 的 `UPDATE ... RETURNING` 无行（源状态不在白名单 / 已软删）；③ 部分领取时拆批 SQL 未命中 |
| 20103 | `BIZ_INVALID_TRANSITION` | 400 | 起点状态不是 `PENDING` / `IN_PROCESS`；或 `IN_PROCESS` 批次不在 `PRODUCTION_SHELF` 上 |
| 20104 | `BIZ_INVALID_VALUE` | 400 | ① `t_part_batch.status` 存了非法字符串；② **`shelf_id` 的 zone 不是 `PRODUCTION`**（仅当传了 `shelf_id`） |
| 20111 | `BIZ_PART_BATCH_INVALID_QUANTITY` | 400 | `quantity` ≤ 0、`> batch.quantity`，或超出 `i32` 范围 |
| 20201 | `BIZ_WORKER_NOT_FOUND` | 404 | `worker_id` 不存在 / 已软删 |
| 20202 | `BIZ_WORKER_INACTIVE` | 400 | `worker.is_active = false` |
| 20206 | `BIZ_WORKER_NO_WORK_TYPE` | 400 | `worker.work_type_id IS NULL` |
| 20501 | `BIZ_SHELF_NOT_FOUND` | 404 | `shelf_id` 不存在 / 已软删（**仅当传了 `shelf_id`**） |
| 20512 | `BIZ_SHELF_INACTIVE` | 400 | `shelf_id` 指向的货架 `is_active = false`（**仅当传了 `shelf_id`**） |
| 40300 | `FORBIDDEN` | 403 | 角色门 `require_any_role(&[Manager, Clerk, ShelfAccount])` 不通过 |
| 40100 | `UNAUTHORIZED` | 401 | 缺 / 坏 Bearer token（含签名失败、claims 不合规） |
| 40105 | `SESSION_REVOKED` | 401 | 服务端 Redis session 已不存在（已 logout / 改密 / 被吊销） |
| 40800 | `REQUEST_TIMEOUT` | 408 | 超过请求级超时（默认 `request_timeout_seconds = 30s`） |
| 50000 / 50001 | `INTERNAL` / `DATABASE` | 500 | DB 故障 / handler panic |

> **两条非业务信封的 4xx**（有 HTTP 状态、body 里**没有** `code` 字段）：
> - **422 + 纯文本**：axum `Json` 提取器拒绝。如 `"shelf_id": 43` 传了 JSON 数字
>   （线上形态是字符串），或 `version` / `worker_id` 缺字段。
> - **413 + 纯文本**：`tower_http::limit::RequestBodyLimitLayer` 拒绝（超
>   `max_request_body_size`）。⚠️ 它**不产** `41301` —— 该码在 `error.rs` 里只
>   有状态映射登记、无产生点。

> **顺序（决定同一请求多个错误时先报哪个）**：角色门 → 批次/ part 读取 → OCC →
> 起点状态 / location 守卫 → `quantity` 范围 → worker 三项 → **`shelf_id` 校验**
> → 拆批 → 翻状态。`shelf_id` 校验在**事务内、拆批之前**，故非法 `shelf_id`
> 不会留下任何拆批残留。

### `POST /api/v2/prod/batches/{batch_id}/place-on-shelf`

权限: **Manager / Clerk**

> P3 lifecycle 端点（batch 级）。工人拣走件入库上货架。状态机：`IN_PROCESS → ON_SHELF`。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`PlaceOnShelfRequest`（body 必填；该 struct 被 3 个 handler 复用）

```json
{
  "version": 0,
  "shelf_id": 42,            // 必填；货架 id
  "next_process_id": 44,     // 可选
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20120 / 40400 / 40901。

### `POST /api/v2/prod/batches/{batch_id}/complete-repair`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

> P3 repair 收尾。`version` 锚 `t_part_batch.version`。
>
> **2026-10-01 BREAKING CHANGE**：守卫条件由「状态机 `REPAIRING → …`」改为
> **「`is_repairing = true`（确实在返修中）」**（REPAIRING 已降级为标记列）。
> 源状态非 `IN_PROCESS` 或 `is_repairing = false` → 20118
> `BIZ_PART_REPAIR_NOT_TRIGGERED`。
> 去向由 `shelf.zone` 决定：`PRODUCTION` → `IN_PROCESS`（落回生产架、重新入池
> 并写 `next_process_id`）/ `INSPECTION` → `INSPECTION`（送检区、出池）；
> 两条路径都把 `is_repairing` 清回 `false`。

Request：`CompleteRepairRequest`（body 必填）

```json
{
  "shelf_id": 42,            // 必填；去向由 shelf.zone 决定
  "version": 0,
  "next_process_id": 44,     // 可选
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20121 / 40901（20109 语义 2026-10-02 起收窄为「批次不存在 / 已软删 / 状态不是流转起点」）。

### `POST /api/v2/prod/batches/{batch_id}/repair-dispatch`

权限: **Manager / Clerk / Inspector**（`require_any_role(&[Manager, Clerk, Inspector])`）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

> P3 repair 起始（与 `start-repair` 类似但用于派工而非自检）。
> 入口状态：IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED；去向由
> `shelf.zone` 决定（PRODUCTION → `IN_PROCESS` / INSPECTION → `INSPECTION`）。
>
> **2026-10-01**：一步式下发（一次调用完成「起修 + 到位」），故不写
> `is_repairing = true` 再清，而是**直接保持 `false`**。事件仍记两条
> （`REPAIR_STARTED` + `REPAIR_COMPLETED`），状态字段写真实值：
> `S → IN_PROCESS`（起修）`→ T`（到位）。

Request：`RepairDispatchRequest`（body 必填）

```json
{
  "shelf_id": 42,            // 必填；去向由 shelf.zone 决定
  "version": 0,
  "next_process_id": 44,     // 可选
  "reason": "string (可选)",
  "note": "string (可选)"
}
```

> `RepairDispatchRequest` **无 `worker_id` / 工人字段**（2026-10-03 订正：原文档示例里的
> `worker_id` 是虚构字段，`dto.rs` 的该 struct 里不存在，故照抄的客户端会静默被丢弃）。
> 被派工人不由本端点的请求体指定：操作人身份取 JWT 解析出的 `CurrentUser`。

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20118 / 40901。

### `GET /api/v2/parts/pickable-by-work-type/{work_type_id}`

权限: **Manager / Clerk / Inspector / ShelfAccount**
（实现见 `src/modules/part/service/phase1/work_type.rs::list_pickable_by_work_type`
的 `current.require_any_role(&[Manager, Clerk, Inspector, ShelfAccount])`。
⚠️ **不含 `CncProgrammer`** —— 同族的 `by-work-type` / `by-worker` 三处一致。）

> 2026-10-04 新增本章节。此前本端点在 `docs/api/` 下**无任何章节**，只在
> [`./index.md`](./index.md#端点总表) 的端点总表里出现过一行。行单位是**批次**
> （取行 SQL 从 `t_part_batch b` 起），语义单位与 `by-worker` 同款。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `work_type_id` | string (i64) | 工种雪花 ID |

Query：

| 参数 | 类型 | 说明 |
|---|---|---|
| `shelf_id` | string (i64)? | 客户端过滤：只看该货架上的批次。**2026-10-04 起语义收窄** —— 见下方「scope 收口」 |
| `limit` | i64? | 默认 50，clamp 到 `[1, 200]` |
| `offset` | i64? | 默认 0 |

Response 200 `data`：`{ items: [PartListItem], total, limit, offset }`。
`PartListItem` 全字段表见 [`./index.md#partlistitem-字段`](./index.md#partlistitem-字段)。

#### 完整 WHERE 谓词全集

取行 SQL 的全部过滤条件（`total` 的 COUNT **缺 `p.deleted_at IS NULL` 一条**，见下）：

| # | 谓词 | 位置 |
|---|---|---|
| 1 | `b.deleted_at IS NULL` | WHERE |
| 2 | `p.deleted_at IS NULL` | WHERE（**仅取行**；COUNT 不 join `t_part`） |
| 3 | `b.status = 'IN_PROCESS'` | WHERE |
| 4 | `b.location = 'PRODUCTION_SHELF'` | WHERE |
| 5 | `sh.is_active = true` | WHERE |
| 6 | `sh.zone = 'PRODUCTION'` | WHERE |
| 7 | `sh.deleted_at IS NULL` | **JOIN** `t_shelf sh ON sh.id = b.current_holder_id`（2026-10-04 补） |
| 8 | `wtp.deleted_at IS NULL` | **JOIN** `t_work_type_process wtp ON wtp.process_id = b.current_process_id`（2026-10-02 补） |
| 9 | `wtp.work_type_id = $1` | WHERE |
| 10 | `($2::bigint IS NULL OR b.current_holder_id = $2)` | WHERE（`?shelf_id=`） |
| 11 | `($5::bigint[] IS NULL OR sh.id = ANY($5))` | WHERE（**用户 scope 收口**，2026-10-04 补） |

`ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC`。

> ⚠️ **这条 `ORDER BY` 在前端会被覆盖**：报工台「领料」页在客户端按选中货架 /
> 扫描顺序重排（见前端 `views/production` 的领料列表），故后端排序只对「不带客户端
> 排序的直接调用方」可见。排序键排的是 **DB 真实列**（`p.is_urgent` /
> `p.planned_delivery_date`），与响应里投出的同名字段同源 —— 不存在「按真值排、
> 按假值显示」的落差。**本文件不修这个双重排序**，属前端侧范围。

> ⚠️ **COUNT 与取行的既有不对称**（**本文件不修**）：COUNT 不 join `t_part`，
> 故 #2 缺失 ⇒ 软删 part 的 active batch 计入 `total` 而不计入 `items`，分页总数
> 可能偏大。是否补 join 属 `total` 语义决策，`work_type.rs` 的 COUNT 处注释已
> 登记。**但 #11（scope 谓词）与 #7（`sh.deleted_at`）两条必须两边同形** ——
> 漏任一条都会让「`items` 已按 scope 收窄、`total` 仍报全厂数」或反之。COUNT 侧
> 的 scope 参数编号是 `$3` 而非 `$5`：**PG 扩展协议要求 Parse 消息声明的参数类型个数
> == SQL 里被引用的参数个数**（个数 = 被引用的最大 `$n`），而 sqlx 按 `.bind()`
> 个数声明类型 —— COUNT 若沿用 `$5`，就得额外 bind 两个没人引用的 `$3`/`$4`，
> Parse 期直接被 PG 拒（`bind message supplies 5 parameters, but prepared statement
> ... requires 3`）。故 COUNT 按自身 bind 顺序连续编号，谓词语义与取行**逐字相同**。

#### scope 收口（2026-10-04 新增，**安全修复**）

> **2026-10-04 之前本端点完全没有按 `user.shelf_ids` 收口**：全文零
> `can_access_shelf` / 零 `shelf_ids` / 零 `shelf_wildcard`，唯一的货架输入是
> 客户端可控的 `?shelf_id=`，而它不与用户 scope 求交、不传时谓词恒真。
> ⇒ **绑了架 A 的 SHELF_ACCOUNT 账号能看到全厂所有 PRODUCTION 架上该工种可领的
> 批次**（信息泄露）。

收口规则**逐条**对齐 `src/auth/rbac.rs::CurrentUser::can_access_shelf`：

```text
shelf_wildcard || shelf_ids.contains(&shelf_id) || has_role(Role::Manager)
```

| 账号 | scope 谓词 |
|---|---|
| `shelf_wildcard = true`（存在 `role='SHELF_ACCOUNT' AND scope_type='shelf' AND scope_id IS NULL` 的 `t_user_role` 行） | `None` ⇒ **不加谓词，全集** |
| 角色含 `MANAGER` | `None` ⇒ **不加谓词，全集** |
| 其余（`shelf_ids` 非空） | `Some(shelf_ids)` ⇒ `sh.id = ANY($n)` |
| 其余（`shelf_ids` 为空） | `Some([])` ⇒ **空集**（不是「无限制」） |

`shelf_wildcard` 的三个限定缺一不可（`role='SHELF_ACCOUNT'` **且**
`scope_type='shelf'` **且** `scope_id IS NULL`），判据见
`iam::service::session::resolve_roles_and_scope`。

⚠️ **`shelf_wildcard` 这一档在产品 API 下建不出来**：
`iam::service::account::validate_role_scope` 对 `Role::ShelfAccount` 硬校验
`scope_id.is_some()`（缺一即 `40001 VALIDATION` / HTTP 422
`SHELF_ACCOUNT role requires scope_type='shelf' and scope_id`），而 `t_user_role` 的
唯一生产写路径就是它（`POST /iam/users/{id}/roles` → `add_role`）。⇒ 上表第一档只有
fixture / 直插 SQL 能造出来，保留它是为了「万一库里存在这种行，读侧按全集处理」，
不代表产品支持这个配置。

⚠️ 空 scope（第四档）在生产的真实成因：`resolve_roles_and_scope` 会把绑到
**已停用 / 已软删 / 不存在**货架的 `scope_id` 过滤掉（登录时求值），这类账号登录后
`shelf_ids == []` 且 `shelf_wildcard == false`。

`?shelf_id=` 与 scope 求**交**：最终作用域 = `scope ∩ {shelf_id}`，`shelf_id` 不在
scope 内 ⇒ 返回空集。`shelf_id` 入参本身（`ByWorkTypeQuery.shelf_id`）保留不动，
只是语义从「不传即全给」变成「收口后的进一步收窄」—— 只能更严不能更松。

> ⚠️ **Clerk / Inspector 的行为变更**：本端点角色白名单含 Clerk / Inspector，
> 而这两类角色按惯例不配 `t_user_role` 的 SHELF_ACCOUNT 行 ⇒ `shelf_ids` 为空且
> `shelf_wildcard = false` ⇒ 收口后**返回空列表**。这是「与 `can_access_shelf`
> 对齐」的必然结果：写侧 `POST /api/v2/prod/batches/worker-scan` 的角色白名单只有
> `[Manager, ShelfAccount]` 并对 `req.shelf_id` 调 `can_access_shelf`，故对这两类
> 账号本来就不存在「列表给出但提交被拒」的落差。
> **爆炸半径**：本端点唯一前端消费方是 `/scan/pick`（`listPartsByWorkTypeAllShelves`），
> 该路由 `meta.allowRoles = ['SHELF_ACCOUNT']`，Clerk / Inspector 进不来。
> ⚠️ **若业务上要放开，唯一经产品 API 可达的办法是给它们逐架配 `scope_id` 的
> SHELF_ACCOUNT 行**（每架一行，`POST /iam/users/{id}/roles`）。`scope_id = NULL`
> 的 wildcard 行做不到 —— `validate_role_scope` 对 SHELF_ACCOUNT 硬拒
> `scope_id IS NULL`（见上「`shelf_wildcard` 这一档在产品 API 下建不出来」）。

#### 2026-10-04 行为变更：`t_shelf` 软删守卫

`JOIN t_shelf sh ON sh.id = b.current_holder_id` 此前缺 `sh.deleted_at IS NULL`。
而 pick-up 写侧 `validate_shelf_zone` 走 `ShelfRepo::get_by_id`（带软删守卫）会拒
软删架 ⇒ 现状是「**列表给出但提交必被拒**」。取行与 COUNT 同步补齐后：软删架上的
批次不再出现在结果里。

#### 出参相对 `PartListItem` 的填充口径

| 字段 | 口径 |
|---|---|
| `id` / `serial_no` / `name` / `drawing_no` | `t_part` 真实投影。⚠️ **2026-10-04 起 `name` 才是工单名**（此前填的是 `drawing_no` 的副本，卡片第 1 / 2 行重复）；`serial_no` 可空（手工工单） |
| `is_urgent` | `t_part.is_urgent` 真实值。⚠️ **2026-10-04 起**（此前恒 `false`，报工台「加急」tag 永不渲染） |
| `planned_delivery_date` | `t_part.planned_delivery_date` 真实值。⚠️ **2026-10-04 起**（此前恒 `1970-01-01`） |
| `system_delivery_date` | `t_part.system_delivery_date` 真实值或 `null`（该列可空）。⚠️ **2026-10-04 起**（此前恒 `null`，交期 chip 永不渲染） |
| `quantity` | **`b.quantity`（批次数量）**，不是 `p.quantity` |
| `batch_id` / `batch_version` | 本端点**填**（行单位是批次）：`t_part_batch.id` / `.version`。批次 OCC 只认 `batch_version` |
| `process_chain_id` | `NULL`（`FromRow` 按列名匹配，取行 SQL 显式投影 `NULL::bigint`）。口径见 [`./index.md`](./index.md#partlistitem-字段) |
| `chain_state` / `chain_next_process_id` / `chain_next_process_name` / `chain_current_process_name` | 全 `NONE` / `"0"` / `null` / `null`（本端点不填；链位置是批次级事实，仅 `by-worker` 填） |
| `version` | 恒 `0`（**有意占位**）：本 VO 的 `version` 是 **part 级**（`t_part.version`），而取行 SQL 不投影 `p.version` |
| `applicant_name` / `request_date` / `customer_id` / `order_no` / `note` / `unit_price` / `total_price` / `created_at` / `created_by` / `updated_at` / `updated_by` / `deleted_at` / `assembly_id` / `customer_name` / `l1_customer_name` / `location` / `holder_name` / `delivered_quantity` | 恒为占位值（空串 / 0 / `null` / epoch）。**前端无消费方**，2026-10-04 有意不动 |
| `next_process_id` | **恒不出现在响应里** —— `PartListItem` 根本没有该字段（2026-09-27 用户决策范围 C），`From<TPart> for PartListItem` 因此也不复制它 |

> 2026-10-04 三个同族端点（`by-work-type` / `pickable-by-work-type` / `by-worker`）
> 的占位值来源已从「三份手抄 20 字段的 `TPart { ... }` 字面量」收敛为**一个**共享
> 投影 struct（`part::service::phase1::work_type::WorkTypeListRow` +
> `into_list_item`），列集与 struct 字段集由 `#[derive(sqlx::FromRow)]` 逐字对齐 ——
> 端点不填的字段在 SQL 里显式投影 `NULL::<type> AS <字段名>`。上表的「恒为占位值」
> 与「恒不出现在响应里」两条不变量因此由类型系统 + 这一个穷尽 struct 字面量保证。

错误码：40300（角色不在白名单）/ 40100（未带 token 或 token 非法）/ 40102（access
token 过期）/ 40105（Redis session 已失效）/ 50001+（DB 错误）。

### `GET /api/v2/parts/by-worker/{worker_id}`

权限: **Manager / Clerk / Inspector / ShelfAccount**

> 2026-09-22 起 P3 list by worker。返回该 worker 名下所有活跃 part 列表。
> 2026-10-04 起本端点是**报工台「放回」页的唯一数据源**（前端不另查别的端点），
> 故行内多带一层批次语义：批次锚点 + 工序链位置。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |

Query：`limit?` / `offset?`（默认 50 / 0）。

Response 200 `data`：`{ items: [PartListItem], total, limit, offset }`。

**筛选条件**：`t_part_batch` 侧 `status='IN_PROCESS'` + `location='WORKER'` +
`deleted_at IS NULL` + `current_holder_id = {worker_id}`，且 part 本身
`deleted_at IS NULL`；`ORDER BY b.id DESC`。行单位是**批次**（不是 part）。

#### 2026-10-04 part 侧真实字段投影

`name` / `is_urgent` / `system_delivery_date` / `planned_delivery_date` 四个字段
自 2026-10-04 起是 `t_part` 的**真实投影值**（此前是占位值：`name` 填成图号副本、
`is_urgent` 恒 `false`、两个交期恒 `1970-01-01` / `null`，报工台「放回」页的加急
tag 与交期 chip 因此永不渲染）。`quantity` 取自 `b.quantity`（批次数量），不是
`p.quantity`。字段级口径与占位值清单见
[`pickable-by-work-type` 节的填充口径表](#get-apiv2partspickable-by-work-typework_type_id)
（三端点共用同一份）。

> 本端点的**行单位是工人持有物**，没有货架维度，故**不做** SHELF_ACCOUNT 货架
> scope 收口（`by-worker` 的 `can_access_shelf` 语义无处可施）。收口只加在
> [`pickable-by-work-type`](#get-apiv2partspickable-by-work-typework_type_id)。
> `by-work-type` 同理（行是 `location='WORKER'` 的工人持有物）。
> ⚠️ 该取舍连同「SHELF_ACCOUNT 仍可枚举任意 `worker_id`」的边界已登记进
> [`../inconsistencies.md`](../inconsistencies.md#96-2026-10-04-登记：by-worker--by-work-type-不做货架-scope-收口)。

#### 2026-10-04 批次锚点字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 该行的 `t_part_batch.id`（**本端点填**）。放回页据此定位批次发写请求 |
| `batch_version` | i32 | 该批次的乐观锁版本（`t_part_batch.version`），发写请求时作 OCC 版本回传 |
| `process_chain_id` | string (i64)? | 零件的工艺链逻辑 FK（`t_part.process_chain_id`），无链为 `null`（本端点填真实投影值） |

> ⚠️ 本 VO 的 `version` 字段是 **part 级**（`t_part.version`）且本端点取行 SQL
> 不投影 `p.version` ⇒ 恒 `0`（有意占位）。批次 OCC 只认 `batch_version`。

#### 2026-10-04 工序链派生字段（三值 `chain_state`）

| `chain_state` | 语义 | 配套字段 | 前端动作（报工台放回） |
|---|---|---|---|
| `NONE` | 无链 / 链已软删 / 锚链解析失败 / **当前工序不在链内（位置指针漂移）** / **链内同一 `process_id` 出现多次（位置有歧义）** | `chain_next_process_id = "0"`、`chain_next_process_name = null`、`chain_current_process_name = null` | 弹工序选择框，让工人手填下一道工序 |
| `NEXT` | 当前工序在链内**且有下一道** | `chain_next_process_id` = 下一道工序 id、`chain_next_process_name` = 其 `t_process.name`（⚠️ 下一道工序被软删时为 `null`）、`chain_current_process_name` = 当前工序名 | 免填，确认后直接放回，提示「下一道工序为 xxx，请将工件放到 xx 货架」 |
| `TAIL` | 当前工序是链内**最后一道** | `chain_next_process_id = "0"`、`chain_next_process_name = null`、`chain_current_process_name` = 当前工序名 | 提示「当前为最后一道工序，加工完成后请送检」 |

四个字段（`chain_state` / `chain_next_process_id` / `chain_next_process_name` /
`chain_current_process_name`）**仅本端点填**，其余 6 处复用 `PartListItem` 的返回点
恒为 `NONE` / `"0"` / `null` / `null`（行单位是 part，链位置是批次级事实，填任一
活跃批次都是错锚点）。`chain_state` 用三值互斥枚举而不是两个 bool：两个 bool 会
产生「可免填 + 是链尾」这类自相矛盾组合。

`chain_next_process_id` 是**非可空**的 `"0"` 兜底口径（沿用
`GET /outsource-pool/state` 的 `receive_next_process_id` 同一约定：JSON 里恒出现，
`"0"` = 无下一道），前端不要按 `null` 判空。

前端判空必须读字段本身，不要从 `chain_state` 推断：

- `chain_next_process_name` 在 `chain_next_process_id == "0"` 时为 `null`，在
  `chain_state == "NEXT"` 时**也可能**为 `null`（下一道工序被软删，id 仍有值）。
- `chain_current_process_name` 的门控是**链内定位**而不是工序存不存在：
  `chain_state == "NONE"` 时恒为 `null`（`NONE` 态不展示当前工序名）；`NEXT` /
  `TAIL` 下为该工序的 `t_process.name`，工序本身被软删时为 `null`。

#### 派生口径（为什么能这么判）

- **锚链** = `COALESCE(p.process_chain_id, <step 指针所在 step 的 chain_id>)`；
  step 指针无行、或锚链已软删 ⇒ `NONE`。中间 JOIN `t_part_process_chain` 就是为了
  让「锚链已软删」也落 `NONE`。
- **当前 step 在锚链内的位置按 `b.current_process_id` 重新定位**
  （`cur2.process_id = b.current_process_id`），**绝对不拿 `b.current_process_step_id`
  的 `sort_order` 当位置**。step 指针与「当前工序在链内的位置」是两个独立事实，而
  worker-scan 的 RETURNED 分支只写 `current_process_id = next_process_id`、**不推进**
  `current_process_step_id`（已知缺口，见 [`./inspection.md`](./inspection.md)
  worker-scan 业务流转节）。于是多工序链的批次在第 2 次放回时 step 指针仍停在**首次
  定位**那一步：按 `sort_order` 推进会把**当前工序自己**当成下一道返回（如指针停在
  A 的 step 而 `current_process_id = B` ⇒ 返回 B），而 `chain_state` 仍在说「可免填」
  ⇒ 写侧照单全收，**静默把工件投回原工序**比拒收更难发现。同一批次第 N 次放回都只能
  靠 `current_process_id` 定位。
- **「下一道」按 `sort_order > 当前 ORDER BY sort_order ASC LIMIT 1` 取**，与写侧
  `prod::process_chain::repo::query::next_step_in_chain` 逐条同形，读侧不替写侧产生
  分歧。**不能**写成 `sort_order = 当前 + 1`：`sort_order` 的**密度不由读侧决定**，
  写侧只保证链内 `sort_order` 互不重复（`upsert_chain` 校验 +
  `uq_chain_step_chain_order (chain_id, sort_order)` 兜底），稠密 0-based（前端
  `usePartProcessDesign` 保存时拍平成 `0,1,2…`）与稀疏 `10/20/30` 两种密度都能落库。
  ⚠️ [`../production/process-chain.md`](../production/process-chain.md) 记的稀疏口径与
  真实写路径不符（漂移登记见 [`../inconsistencies.md`](../inconsistencies.md) §9.4），
  别拿它当密度依据。`+ 1` 只在稠密下正确、在稀疏下会把「还有两道工序」误判成
  链尾、让报工台对工人谎报「当前为最后一道工序」；`>` 对两种密度都成立。
- `cur2` 是 **inner** `JOIN LATERAL`：定位不到链内位置时整个派生子查询无行、四个派生
  列全 `NULL`，由最外层 `COALESCE(..., 'NONE')` 兜底成 `NONE`；位置解析成功但没有更大的
  `sort_order` ⇒ `TAIL`；否则 `NEXT`。
- **链内 `process_id` 重复 ⇒ `NONE`（显式降级，不靠 `LIMIT 1` 取舍）**：
  `t_process_chain_step` 只有 `uq_chain_step_chain_order (chain_id, sort_order)
  WHERE deleted_at IS NULL` 一个唯一约束，**没有** `(chain_id, process_id)` 唯一
  约束；写侧 `upsert_chain` 也只校验 `sort_order` 重复、不校验 `process_id` 重复 ⇒
  重复工序的链后端照收。此时按 `process_id` 定位当前 step 会**扇出多行**（链
  `[(A,10),(A,20),(B,30)]` 而 `current_process_id = A` ⇒ 一行派生 `NEXT → A`
  即**当前工序自己**、另一行派生 `TAIL`）。取行 SQL 用 `(count(*) OVER ())` 带出
  链内命中数，命中 >1 时**显式落 `NONE`** 并门控全部派生列（下一道 id 为 `"0"`、
  两个名字为 `null`），与「未知一律往保守方向降」一致。
- LATERAL 末尾 `ORDER BY cur.id ASC LIMIT 1` 收口：`cur` / `pc` 都按主键定位，本就
  至多一行，此处只为把「至多一行」这条不变量写进 SQL —— 一旦上游改动放宽了任一
  JOIN，一行批次会扇成多行、破坏 `items.len()` 等于持有批次数的不变量。排序键
  `cur.id` 在任何假设的扇行里都是同一个常量、打不破平局，故这个 `ORDER BY` 只表达
  行数上界，**不承担消歧**。

### 外协两条 list 端点 — 2026-10-03 已下线，迁往 outsource 域

> **BREAKING（硬切，无 alias）**：`GET /api/v2/parts/outsource-in-flight` 与
> `GET /api/v2/parts/outsource-sendable` **已从 part 域删除**（实现文件
> `src/modules/part/service/phase1/outsource.rs` 整文件移除）。
>
> **下线原因**：二者返回的是通用 `PartListItem`，与前端外协域
> （`frontend/src/views/outsource/`）需要的字段**形状不匹配** ——
> 缺批次级 `version` / `quantity`、外协公司、`customer_path`；`sendable` 还缺
> `send_mode` / `company_options` / `quote_id` / `source_status`，**根本无法表达
> DIRECT（无报价直发）模式**。前端因此长期「在途 tab 空白 / 可发送 tab 全灰」。
>
> **迁移指引**：

| 变更前 | 变更后 | 文档 |
|---|---|---|
| `GET /api/v2/parts/outsource-in-flight` | **`GET /api/v2/outsource-shipments/in-flight`** | [`../outsource-quotes.md`](../outsource-quotes.md) 同批登记 / [`../outsource-sendable.md`](../outsource-sendable.md) |
| `GET /api/v2/parts/outsource-sendable` | **`GET /api/v2/outsource-sendable`** | [`../outsource-sendable.md`](../outsource-sendable.md) |

> ⚠️ 旧路径的**实际 HTTP 状态码是 400 而非 404**：part 域 `Router` 注册了
> `/{part_id}`（`Path<i64>`）catch-all，matchit 静态段优先、参数段兜底 ⇒ 任何
> 未注册的 1 段静态路径都先落到 `/{part_id}`，再由 `Path` extractor 拒绝非数字段
> （`Invalid URL: Cannot parse '...' to a 'i64'`）—— 任意不存在的静态段（如
> `/api/v2/parts/zzz-not-a-real-endpoint`）行为完全相同。
> 旧 handler 已彻底删除，不再有任何 outsource 专用处理。
>
> 端点数影响：part 域 26 → **24**（**method 级**注册口径：`route("/")` 上的
> `get().post()` 记 2 条。基数 26 = method 级 51 − 2026-10-02 迁往 prod 域的 25 条
> `t_part_batch` 子资源；本次再下线 2 条外协 list → 24）。
> 现值以 `src/modules/part/mod.rs` 逐个数为准；同一口径与推导见
> [`../inconsistencies.md`](../inconsistencies.md) § 2 与
> [`../DRIFT_REPORT.md`](../DRIFT_REPORT.md)。完整变更登记见
> [`../inconsistencies.md`](../inconsistencies.md) § 9.2。

### `GET /api/v2/prod/batches/repair`

权限: **Manager / Inspector**

> P3 list。返回所有 `status='DELIVERED'` 的 batch 汇总（判据 `status='DELIVERED'`；REPAIRING 已降级为 `is_repairing` 标记列）。

Query（`RepairBatchListQuery`）：`keyword?`（跨字段 ILIKE，匹配图号 / 名称）/
`customer_id?`（**不**做 L1 展开，与待品检端点不同）/ `serial_no?` /
`planned_delivery_date_from?` / `planned_delivery_date_to?` / `limit?`（默认 200，
clamp `[1,500]`）/ `offset?`（默认 0）。

> ⚠️ **已知差距（2026-10-03 记录，待单独 issue）**：`keyword` 走
> `p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%'`，
> **不拒** SQL 通配符 —— 传 `keyword=%` 会命中全表（注入面由 bind 参数化保证，
> 但「通配符放大」这条语义约束缺失）。待品检端点
> `GET /prod/batches/inspection` 已在 service 层拒 `%` / `_` / `\`（40001），
> 返修两条端点尚未跟进。本次未改，避免把返修 VO 收口混进待品检 VO 收口。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

#### `InspectionBatchListItemOut` 字段

**恰好 28 个**。`BatchOut` 是 `InspectionBatchListItemOut` 的别名
（`src/modules/prod/batch/vo.rs`），本表是返修两条端点
（`/prod/batches/repair` + `/prod/batches/repairing`）响应 `items[]` 的字段契约。
排序固定 `b.id DESC`（`service/repair.rs::list_batches_matching`）—— **不做**
「紧急件优先」排序；`total` 与 `items` 走同一条 WHERE 拼装。

批次字段段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `batch_no` | i32 | 批次号（`t_part_batch.batch_no`，非空列，恒为 JSON number） |
| `quantity` | i32 | 批次数量 |
| `status` | string | 批次状态枚举字符串。`/repair` 固定 `DELIVERED`（判据 `status='DELIVERED'`）；`/repairing` **不固定** —— 判据是 `is_repairing = true` 且 `status NOT IN ('COMPLETED','CANCELLED')`，故可能是 `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` |
| `is_repairing` | bool | 是否处于返修中。直读 `t_part_batch.is_repairing` 标记列（migration 005/006）；非 `Option`、无 `#[serde(default)]`、无 `skip_serializing_if` ⇒ **恒定出现在 JSON 里**，前端 Zod 必须按必填 `boolean` 声明、不能 `.optional()`。`REPAIRING` 已从 `PartStatus` 降级、DB 不再产生 `REPAIRING` 字面量，且本字段与 `status` **正交**（起修后送检可得 `INSPECTION` + `is_repairing = true`）⇒ **判「返修中」只能读本字段**，端点语义与 `status` 都推不出来。完整可达链见本文件 [`GET /api/v2/prod/batches/repairing`](#get-apiv2prodbatchesrepairing) 末「返修标记与 status 正交 —— 可达链」 |
| `location` | string? | 批次所在位置（`t_part_batch.location`） |
| `version` | i32 | 乐观锁（`t_part_batch.version`，caller OCC 锚点；**不是** `part_version`） |
| `current_process_step_id` | string (i64)? | 逻辑 FK → `t_process_chain_step.id`；**只在首次定位工序时写、之后不再推进**（显示用定位信息，不是「当前走到第几步」的进度指针），允许 NULL |
| `parent_batch_id` | string (i64)? | 拆批来源的父批次 ID（仅拆批产生的新批次非 NULL） |

holder 解析段（`current_holder_id` 同一列 LEFT JOIN 三张表，按
`t_shelf` → `t_worker` → `t_outsource_company` 顺序 COALESCE 出 `holder_name`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `current_holder_id` | string (i64)? | 当前持有人 id（`t_shelf.id` / `t_worker.id` / `t_outsource_company.id` 三义，单列无法区分归属表） |
| `holder_name` | string? | `COALESCE(shelf.name, worker.name, outsource_company.name)`；`current_holder_id` 为 NULL 时为 `null` |
| `next_process_id` | string (i64)? | 下一道工序 id（对应 `t_process.id`）。由 `current_process_step_id` 经 `LEFT JOIN t_process_chain_step` 取 `process_id` 派生；**刻意不直读 `t_part_batch.current_process_id`** —— 送检 = 出池，该列被写点置 NULL，对本 VO 的 `DELIVERED` / 返修行结构性恒 NULL，直读会让这两个字段恒 `null`。字段名保留以兼容前端契约 |
| `next_process_name` | string? | 下一道工序名称（`LEFT JOIN t_process p2 ON p2.id = s2.process_id` 拼齐） |

> **读取方分工（勿越界）**：`t_part_batch.current_process_id` 是**工序池归属的权威
> 列**，读取方严格限定为 5 条工序池 SQL + `list_pickable_by_work_type` + rollup 派生
> `t_part.next_process_id`；**展示类列表（本 VO / dashboard / part 批次明细）一律走
> step 派生**。完整清单见 `src/modules/prod/batch/model.rs` 模块 doc。

delivery_note 解析段（LEFT JOIN `t_delivery_note` 一次拼齐）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `delivery_note_id` | string (i64)? | 关联送货单 id（`t_part_batch.delivery_note_id`） |
| `delivery_note_no` | string? | 关联送货单号（`t_delivery_note.delivery_note_no`） |

工单字段段（JOIN `t_part`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |
| `serial_no` | string? | 工单序列号（手工工单可空） |
| `drawing_no` | string | 图号 |
| `name` | string | 工单名 |
| `order_no` | string? | 订单号 |
| `planned_delivery_date` | date | 计划交付日（`t_part.planned_delivery_date`，**非空列**；两个 query 参数 `planned_delivery_date_from` / `_to` 即作用于它） |
| `is_urgent` | bool | 是否加急；纯展示字段，本 VO 端点不按它排序 |
| `part_version` | i32 | part 聚合 version（**caller OCC 必须用 `version`（`t_part_batch.version`），不能用本字段**） |
| `created_at` | naive datetime | 工单创建时间 |
| `updated_at` | naive datetime | 工单更新时间 |

客户解析段（LEFT JOIN `t_customer` + 自连 L1 一次拼齐）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `customer_id` | string (i64) | 工单客户 id（`t_part.customer_id`） |
| `customer_name` | string? | 客户名（`t_customer.name`） |
| `l1_customer_name` | string? | L1 客户名（`c_l1.id = c.parent_id` → `c_l1.name`；**不回落到 `c.name`**，故客户自身即 L1（`parent_id IS NULL`）时为 `null`。注意与 `GET /prod/batches/inspection` 的同名派生口径不同 —— 那个 VO 走 `COALESCE`，自身即 L1 时等于 `customer_name`） |

SQL 形态：单条 SQL 9 个 JOIN / 10 张表（`t_part_batch` + `t_part` + `t_customer` +
自连 L1 `t_customer` + `t_shelf` + `t_worker` + `t_outsource_company` +
`t_process_chain_step` + `t_process` + `t_delivery_note`）。

> **2026-10-03 迁入本节**：本表原先挂在 `inspection.md` 的
> `GET /api/v2/prod/batches/inspection` 端点段下，但该端点已于同日换成 13 字段的
> `InspectionQueueItemOut`、不再共用本 VO，返修两条端点仍在用 ⇒ 表随 VO 实际宿主
> 迁到本节。`GET /prod/batches/inspection` 的字段表见
> [`./inspection.md`](./inspection.md#inspectionqueueitemout-字段)。

### `GET /api/v2/prod/batches/repairing`

权限: **Manager / Inspector**

> P3 list（与 `/prod/batches/repair` 同形状，区别：判据不同）。
>
> **2026-10-01 BREAKING CHANGE**：判据由 `status = 'REPAIRING'` 改为
> **`t_part_batch.is_repairing = true`**。判据**不含 status**（SQL 只额外排除
> `COMPLETED` / `CANCELLED`）⇒ 返回项的 `status` **不固定**：起修时为
> `IN_PROCESS`，但起修后送检 / 送检通过 / 发货三步都**只保持标记**、不改判据，
> 故本端点也可能返回 `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` 的返修件。
> ⚠️「DB 不再产生 `REPAIRING` 字面量」**不等于**「`status` 恒为 `IN_PROCESS`」——
> 判「返修中」一律读 `is_repairing`。标记与 `status` **正交**的完整推导见本节末
> 「返修标记与 status 正交 —— 可达链」。
>
> 返修标记随 `BatchOut`（= [`InspectionBatchListItemOut`](#inspectionbatchlistitemout-字段)，
> 字段表见上一节）的字段
> `is_repairing: bool` 一起返回 —— 「只有 status」的端点会让前端彻底失去
> 「返修中」信号（改造前靠 `status === 'REPAIRING'` 判定，改造后任何接口都拿不到
> 该值）。影响端点：`GET /prod/batches/repairing`、`GET /prod/batches/repair`、
> 以及 `GET /parts/{id}/batches`（`PartBatchListItemOut` 同样有 `is_repairing`）。
> ⚠️ `GET /prod/batches/inspection` **不在此列**：该端点 2026-10-03 VO 收口成
> `InspectionQueueItemOut`（13 字段），不投 `is_repairing`；待品检页不按返修标记
> 分流，返修件与普通送检件在该页混排（同页原语义）。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

#### 返修标记与 status 正交 —— 可达链

本节是「为什么必须有 `is_repairing` 字段」的权威推导（原先放在
`inspection.md` 的端点 VO 段，2026-10-03 待品检 VO 收口时随该段一并删除，
现搬回本主题下）。

**可达链**（`start-repair` 之后标记为 `true`，三步都**只保持标记**、不改判据）：

1. `POST /prod/batches/{batch_id}/start-repair` → `status` 保持 `IN_PROCESS`，
   `is_repairing = true`。
2. 走 `POST /prod/batches/{batch_id}/to-inspection` 或 worker-scan `INSPECTED`
   → 两条路都经 `mark_batch_inspected`，它对 `is_repairing` 传 `None` =
   **保持**标记，`allowed_from` 含 `IN_PROCESS` ⇒ 落到
   `status = 'INSPECTION'` + `is_repairing = true`。
   该状态可达的反证是 `to-process` 对它有 20118 守卫
   （`src/modules/part/service/inspection_core.rs` step 4.6）。
3. `mark_batch_passed_inspection` → `status = 'READY_TO_SHIP'`，标记仍保持。
4. `mark_batch_delivered`（`to-ship`）→ `status = 'DELIVERED'`，标记仍保持。
   `to-ship` **无**返修守卫（只有 `to-process` 有），故 DELIVERED 批次同样可能带标记。

**由此得出三条不能省的结论**：

- `REPAIRING` 降级为标记列后，DB 不再产生 `REPAIRING` 字面量 ⇒ 前端的老判据
  `status === 'REPAIRING'` 在**任何**端点都取不到值，「是否返修中」只能读
  `is_repairing`。
- **端点语义推不出每一行是否返修中**：`status='INSPECTION'` 的列表里返修件与
  普通送检件混排（上表第 2 步），`status='DELIVERED'` 的列表里同样混排
  （第 4 步）。
- **只调 `GET /api/v2/prod/batches/repairing` 也不是答案**：该端点只返回
  `is_repairing = true` 的批次，覆盖不了「同一个列表里既有返修件又有普通在制品」
  的展示场景，而后者才是队列类页面的常态。

字段形态：`bool`（**非** `Option`、**无** `#[serde(default)]`、**无**
`skip_serializing_if`）⇒ **恒定出现在 JSON 里**，前端 Zod schema 必须按必填
`boolean` 声明，不能 `.optional()`。同批新增的另一个 VO `PartBatchListItemOut`
（`GET /api/v2/parts/{id}/batches`）同样有 `is_repairing: bool`，见
[`./batch.md`](./batch.md)。

---

> **2026-09-23 同步说明**：本节 8 个端点（pick-up / place-on-shelf / complete-repair / repair-dispatch / 4 个 GET 列表）原 docs/api/parts/lifecycle.md 未覆盖，本次按 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycle-inspectionmd高优先级--大量端点缺失)）。

---

### `GET /api/v2/parts/pending-programming`

权限: **Manager / Clerk / CncProgrammer / Inspector**

> **2026-09-29 BREAKING CHANGE**（CNC 重构 5 任务之一）：编程流转入口改造。
>
> 旧实现：`status = 'PROGRAMMING'` 一览 + `POST /parts/{id}/send-to-programming` /
> `POST /parts/{id}/recall-to-programming` 两个端点。新实现：**取消状态机
> `PROGRAMMING` 进入路径**（`PENDING → PROGRAMMING` 与 `IN_PROCESS → PROGRAMMING`
> 两条迁移已从 `part/statemachine.rs::can_transition_to` 删除），待编程一览改为
> 基于 `t_process.is_cnc` 列的链上 / 货架过滤，配合前端 Tab 切换
> `has_cnc_program?: bool`。
>
> 编程员现在通过工艺链 + CNC step 直接进入生产流；`POST /parts/{id}/send-to-programming`
> 与 `POST /parts/{id}/recall-to-programming` 端点已下线（返回 404）。
> `POST /prod/batches/{batch_id}/release-from-programming` 仍保留（PROGRAMMING → IN_PROCESS）。

Query：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keyword` | string? | — | 模糊匹配 `name` / `drawing_no` / `serial_no` |
| `sort_by` | string? | — | 白名单 `CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它退化为 `PLANNED_DELIVERY_DATE` |
| `sort_dir` | string? | — | `ASC` / `DESC`（缺省 `ASC`） |
| `limit` | int? | — | 1..=500（缺省 50） |
| `offset` | int? | — | ≥ 0（缺省 0） |
| `has_cnc_program` | bool? | — | **2026-09-29 新增**：Tab 切换。`true` 仅已上传 G_CODE；`false` 仅未上传；缺省全部 |

**新过滤规则**（取代旧 `status = 'PROGRAMMING'`）：

```sql
status IN ('PENDING','IN_PROCESS','PROGRAMMING')  -- 历史 PROGRAMMING 状态仍允许消化
AND (
  -- 条件 A：工艺链上含 CNC step
  EXISTS (
    SELECT 1 FROM t_process_chain_step s
    JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL
    WHERE s.chain_id = p.process_chain_id
      AND s.deleted_at IS NULL
      AND pr.is_cnc = TRUE
  )
  -- 条件 B：当前 active 批次所在货架关联 CNC 工序
  OR EXISTS (
    SELECT 1 FROM t_part_batch pb
    JOIN t_shelf_process sp
      ON sp.shelf_id = pb.current_holder_id AND sp.deleted_at IS NULL
    JOIN t_process pr
      ON pr.id = sp.process_id AND pr.deleted_at IS NULL
    WHERE pb.part_id = p.id
      AND pb.deleted_at IS NULL
      AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING')
      AND pr.is_cnc = TRUE
  )
)
```

外加可选 `has_cnc_program` 过滤：`EXISTS t_part_file.kind = 'G_CODE'`。

Response 200 `data`：[`PartListOut`](./index.md#partlistout-字段)。**2026-09-29 新增**：
[`PartListItem`](./index.md#partlistitem-字段) 含 `has_cnc_program: bool` 派生字段
（由 repo EXISTS 子查询填充）。

错误码：40001（limit/offset 越界）、40300（角色不符）、50001（DB）。

#### 业务场景

- **Tab = 待编程**（`has_cnc_program=false`）：未上传 G_CODE 的 CNC 工单（链上有 CNC
  step 或当前批次在 CNC 货架 + 未上传程序）。编程员先在 list 内点"上传 G_CODE"→
  后端走 `POST /part-files/upload-intents` + 直传 COS + `confirm`。
- **Tab = 已编程**（`has_cnc_program=true`）：已上传 G_CODE 的 CNC 工单，编程员
  确认无误后通知车间 release（走 `POST /prod/batches/{batch_id}/release-from-programming`）。
- **Tab = 全部**（`has_cnc_program` 缺省）：所有 CNC 相关工单（含历史 PROGRAMMING
  状态可消化的批次）。

> **2026-10-01 弃用说明**：前端「待编程一览」页已切到 prod 域
> `GET /api/v2/prod/programming/pending`（part 状态白名单闸门 + 三规则并集口径，见
> [`../production/pending-programming.md`](../production/pending-programming.md)）。
> 切换原因：① 本端点规则 B 走「批次货架 `current_holder_id` → `t_shelf_process` →
> `t_process.is_cnc`」间接链路，开发库 `t_process.is_cnc` 全 false 且
> `t_process_chain_step` 0 行 → 谓词恒返空；② migration 004 起
> `t_part_batch.current_process_id` 才是批次工序归属的**唯一权威依据**，新端点规则 3
> 直接读该列。**本端点保留兼容，不删除、行为不变，不再新增前端调用方**（part 域
> 代码一行未改）。
>
> 与新端点的口径差异（切换时须知）：本端点规则 1 是全局
> `status IN ('PENDING','IN_PROCESS','PROGRAMMING')` 白名单，新端点把同一条白名单
> 提为**约束全部三规则**的独立闸门（结论一致：已交付/已完成/已取消的工单两边都
> 不出现）；差别只在规则 2/3 的「CNC 工序定位方式」（本端点走货架间接链路，
> 新端点走链 / `current_process_id`）。另新端点 `keyword` 的 `%` / `_` 按字面量
> 转义（`ESCAPE '\'`），本端点未转义。

---

> **2026-09-29 同步说明**：本节 `GET /parts/pending-programming` 端点 + 新 query 参数
> `has_cnc_program` + `PartListItem.has_cnc_program` 字段新增；旧端点
> `POST /parts/{id}/send-to-programming` 与 `POST /parts/{id}/recall-to-programming`
> 下线（返回 404），state machine 同步删除
> `PENDING → PROGRAMMING` 与 `IN_PROCESS → PROGRAMMING` 两条入口迁移。
