# Inspection 端点 —— t_part_batch 生产流转（2026-10-02 起归 prod 域）

> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
> 端点清单（按域）见 [`../production/batches.md`](../production/batches.md#2026-10-02-t_part_batch-子资源迁入)
>
> **归属（2026-10-02 变更）**：本文件除 `GET /api/v2/parts/by-serial/{serial_no}/part-batches`
> 外，全部端点的 URL 已从 `/api/v2/parts/*` 迁到 `/api/v2/prod/batches/*`，路径锚点由
> `part_id` 改为 `batch_id`，`batch_id` 同时从请求体删除（它成了路径参数）。
> 依据：`t_part_batch` 是生产执行单元，其 OCC / `status_gate` rollup / 状态机本体整体归
> prod 域；part 域只留**多批次动作**（`cancel` / `force-complete`）与**非批次动作**
> （CRUD / 文件 / 列表）。`t_part_batch.id` 全局唯一，故「跨 part 批次」这一场景不再存在，
> `part_id` 形参本就冗余（`status_gate` 的 part rollup 由 `RETURNING part_id` 反推，不依赖
> 调用方传值）。
>
> 范围：本文件覆盖 6 个 inspection 端点（`to-inspection` / `to-ship` / `to-process` 三个
> batch 子资源 + 三个静态批量 / 事件端点）+ 3 个集合读。CRUD / lifecycle / by-serial /
> upload-drawing 见 [`./crud.md`](./crud.md) / [`./lifecycle.md`](./lifecycle.md)。

## 本文件目录

- [POST /api/v2/prod/batches/to-inspection](#post-apiv2prodbatchesto-inspection)
- [POST /api/v2/prod/batches/{batch_id}/to-inspection](#post-apiv2prodbatchesbatch_idto-inspection)
- [POST /api/v2/prod/batches/to-ship](#post-apiv2prodbatchesto-ship)
- [POST /api/v2/prod/batches/{batch_id}/to-ship](#post-apiv2prodbatchesbatch_idto-ship)
- [POST /api/v2/prod/batches/{batch_id}/to-process](#post-apiv2prodbatchesbatch_idto-process)
- [POST /api/v2/prod/batches/worker-scan](#post-apiv2prodbatchesworker-scan)
- [GET /api/v2/parts/by-serial/{serial_no}/part-batches](#get-apiv2partsby-serialserial_nopart-batches)
- [GET /api/v2/prod/batches/inspection](#get-apiv2prodbatchesinspection)
- [乐观锁（caller 侧 OCC）](#乐观锁caller-侧-occ)
- [自动拆批（auto-split）](#自动拆批auto-split)

---

### `POST /api/v2/prod/batches/to-inspection`

权限: **Manager / Inspector**

Request：`BatchToInspectionRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 批量共享品检架；service 一次性校验 `zone='INSPECTION'` + `is_active=true`；**整批失败**返回顶层 `20511` / `20512` |
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_INSPECTION_MAX_ITEMS`（200）；空数组 / 超出上限 → `40001` |

公共字段（`items[]`）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | **必填**；service 按 `id` 反查 `t_part_batch` 拿 part_id（不再要求 caller 在 item 内填 part_id）；找不到批次 → `20109 BIZ_PART_BATCH_NOT_FOUND` |
| `version` | i32 | ✓ | 目标批次 `t_part_batch.version`（**不是** part 的 version）；不符 → 该 item 落 `failed[].code = 40901`，详见 [乐观锁](#乐观锁caller-侧-occ) |
| `quantity` | i32? | — | 缺省 = 整批；详见 [自动拆批（auto-split）](#自动拆批auto-split) |

> 与单件端点的关键差异：`items[]` **不带** `part_id` —— DTO 更精简、单 / 批 item shape 统一靠 `batch_id` 反查 part_id。
>
> 起始状态：item.batch 必须在 `{PENDING, PROGRAMMING, IN_PROCESS}` 之一（IN_PROCESS 还须 `location='PRODUCTION_SHELF'` + holder 是 shelf 而非 worker）。

Response 200 `data`：`BatchToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | `ToXxxOut[]` | 成功并完成送检的 item；与 `items` 顺序一一对应（`submitted[i]` 对应 `items[i]`） |
| `failed` | `BatchOpFailure[]` | 失败的 item；按 `batch_id` 定位失败项；单 item 不会同时出现在 `submitted` 与 `failed` |

每条 `ToXxxOut` 形状：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 送检后 part 的最新投影（含 OCC 更新后的 `version`） |
| `new_batch_id` | string (i64)? | 仅当 `quantity < batch.quantity` 走拆批分支时为 `Some(remainder_id)`；整批操作时为 `null` |

每条 `BatchOpFailure` 形状：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 失败的批次 ID |
| `code` | i32 | item-level 错误码（参见 `错误码` 节） |
| `message` | string | 失败原因（中文） |

业务流转：

- 起点状态：item.batch ∈ `{PENDING, PROGRAMMING, IN_PROCESS}`（部分通过拆批后 remainder 留在源状态；详见 [自动拆批](#自动拆批auto-split)）
- 终点状态：`INSPECTION`；事件日志：`event_type='INSPECTED'`

WS 广播（commit 后下发）：

- `BATCH_TO_INSPECTION` —— payload `{ submitted: <count>, failed: <count> }`（仅计数，不含数组）
- **父装配件自动同步**：若某 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。**响应字段位置**：实际翻转时，在该 item 对应 `data.submitted[i].synced_assembly_id = Some(<id>)`；不翻转时为 `null`（同 part 全无 assembly_id 时，整个字段缺省为 `null`）。

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200；或 item 缺 `batch_id` / `version`（`batch_id` 非数字串 → `failed[]` 里以 `batch_id: "0"` sentinel 落 40001）
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- 外层校验错误（顶层）：20511 / 20512（共享品检架不合法 → 整批失败）
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901（40901 = 该 item 的 `version` 不符，其余 item 照常处理）

---

### `POST /api/v2/prod/batches/{batch_id}/to-inspection`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`ToInspectionRequest`（body 必填 —— `target_inspection_shelf_id` / `version` 是必填字段）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 目标品检架；service 校验 `zone='INSPECTION'` + `is_active=true`（违反 → `20511` / `20512` / `20501`） |
| `note` | string? | — | 送检备注；`≤ 500` 字符 |
| `version` | i32 | ✓ | 目标批次 `t_part_batch.version`（**不是** part 的 version）；不符 → `40901`，详见 [乐观锁](#乐观锁caller-侧-occ) |
| `quantity` | i32? | — | 本次送检数量；缺省 = 整批；详见 [自动拆批（auto-split）](#自动拆批auto-split) |

> **2026-10-02 BREAKING**：`batch_id` 从请求体**删除**（它现在是路径参数）。
> 相应地，「批次不属于该 part」不再是可表达的场景 —— 传 `batch_id` 找不到批次即
> `20109`，找不到则说明批次不存在 / 已软删 / 状态不是流转起点。

业务流转：

- 起点状态：`PENDING` / `PROGRAMMING` / `IN_PROCESS`
  - `IN_PROCESS` 必须 `location='PRODUCTION_SHELF'` + `current_holder_id` 命中 `t_shelf`（service 启发式区分 worker 持有 vs shelf 持有；worker 持有 → 20103 / "工人持有件请先归还或送检"）
- 终点状态：`INSPECTION`（`location='INSPECTION_SHELF'` + `current_holder_id=target_shelf.id`）
- **2026-09-30（review H2 修复）：`current_process_id` 置 NULL**（送检 = **出池**，
  写入不变式第 2 行）。此前本端点只翻 `status` / `location` / `current_holder_id`，
  批次会带着上一道工序的 `current_process_id` 停在 `INSPECTION` 状态，与 migration 004
  的列定义「NULL 表示批次不在生产工序池中」矛盾。
  - 不会造成池污染：4 条工序池 SQL 与 `list_pickable_by_work_type` 均硬限定
    `status='IN_PROCESS'` + `location='PRODUCTION_SHELF'`。
  - `current_process_step_id` **仍保留**，它是 `next_process_id` 的唯一数据来源
    （`LEFT JOIN t_process_chain_step` 派生），供 `INSPECTION` 期间前端显示批次
    **首次定位**在工艺链的哪一步。它**不是会随流转推进的「进度指针」** ——
    送检（以及 worker-scan RETURNED / `send_to_inspection` 等）都不写它，
    对多工序链工单它永远停在首次定位的那一步。
    它也不再是状态机依赖：检验不合格打回（`to-process`）会按
    `chain_id + next_process_id` **重新解析** step_id 写入，不会真的丢上下文。
- 事件日志：`event_type='INSPECTED'`

WS 广播（commit 后下发）：

- `PART_TO_INSPECTION` —— payload `{ part_id, shelf_code }`
- **父装配件自动同步**：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。实际翻转时响应 `data.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。

Response 200 `data`：`ToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 流转后的 part 投影 |
| `new_batch_id` | string (i64)? | 仅拆批时为 `Some(remainder_id)`；整批操作时为 `null` |

错误码：

- 20101 BIZ_PART_NOT_FOUND — 批次所属工单已软删（**2026-10-02**：本端点不再有 part 路径参数，工单不存在只能经由「批次的 part 已软删」触发，语义不变）
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不在 `{PENDING, PROGRAMMING, IN_PROCESS}` 白名单；或 `IN_PROCESS` 但 holder 是 worker；或 `IN_PROCESS` 但 holder 是非 PRODUCTION 区货架
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单
- 20109 BIZ_PART_BATCH_NOT_FOUND — **2026-10-02 语义收窄**：`batch_id`（路径参数）不存在 / 已软删；或其状态不在 `{PENDING, PROGRAMMING, IN_PROCESS}`。「不属于该工单」不再是独立场景（`batch_id` 全局唯一即锚点）
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 20501 BIZ_SHELF_NOT_FOUND — `target_inspection_shelf_id` 不存在
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`
- 40901 VERSION_CONFLICT — caller 传的 `version` ≠ 目标批次当前 `version`（见 [乐观锁](#乐观锁caller-侧-occ)）；或事务内 UPDATE 撞并发
- HTTP 422 — payload shape 错误（缺 `target_inspection_shelf_id` / `version` / 空 body）：axum `Json` extractor 在 service 之前直接拒，**非项目统一信封**
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/prod/batches/to-ship`

权限: **Manager / Inspector**

Request：`BatchToShipRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_SHIP_MAX_ITEMS`（200）；空数组 / 超出上限 → `40001` |

公共字段（`items[]`）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | **必填**；service 按 `id` 反查 `t_part_batch` 拿 part_id；找不到 → `20109 BIZ_PART_BATCH_NOT_FOUND` |
| `version` | i32 | ✓ | 目标批次 `t_part_batch.version`（**不是** part 的 version）；不符 → 该 item 落 `failed[].code = 40901`，详见 [乐观锁](#乐观锁caller-侧-occ) |
| `quantity` | i32? | — | 缺省 = 整批；详见 [自动拆批（auto-split）](#自动拆批auto-split) |

> 与 `to-inspection`（静态批量）同形；差异：
>
> - 不需要 `target_inspection_shelf_id`（to-ship 状态机终态是 `READY_TO_SHIP`，与品检货架无关）
> - 起点状态：item.batch 必须是 `INSPECTION`；非 INSPECTION → `20103`

Response 200 `data`：`BatchToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | `ToXxxOut[]` | 成功通过品检的 item；与 `items` 顺序一一对应 |
| `failed` | `BatchOpFailure[]` | 失败的 item（按 `batch_id` 定位） |

`ToXxxOut` / `BatchOpFailure` 形状同 `to-inspection`（静态批量）。

业务流转：

- 起点状态：`INSPECTION`（item.batch.status 必须为 `INSPECTION`，否则 20103）
- 终点状态：`READY_TO_SHIP`
- 多轮 rollup：service 检查 part 下是否还有其它 INSPECTION 批次；若有，**part.status 保持 `INSPECTION`**（仅 operated 批次翻状态）；若无，part.status 同步翻 `READY_TO_SHIP`
- 事件日志：`event_type='STATUS_CHANGED'`（from=`INSPECTION` → to=`READY_TO_SHIP`）

WS 广播（commit 后下发）：

- `BATCH_TO_SHIP` —— payload `{ submitted: <count>, failed: <count> }`
- **父装配件自动同步**：若某 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。**响应字段位置**：实际翻转时，在该 item 对应 `data.submitted[i].synced_assembly_id = Some(<id>)`；不翻转时为 `null`（同 part 全无 assembly_id 时，整个字段缺省为 `null`）。

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200；或 item 缺 `batch_id` / `version`（`batch_id` 非数字串 → `failed[]` 里以 `batch_id: "0"` sentinel 落 40001）
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901（40901 = 该 item 的 `version` 不符，其余 item 照常处理）

---

### `POST /api/v2/prod/batches/{batch_id}/to-ship`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`ToShipRequest`（body 必填 —— `version` 是必填字段，不再支持省略整个 body）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 目标批次 `t_part_batch.version`（**不是** part 的 version）；不符 → `40901`，详见 [乐观锁](#乐观锁caller-侧-occ) |
| `quantity` | i32? | — | 本次通过品检数量；缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |
| `note` | string? | — | ≤ 500 字符 |

> **2026-10-02 BREAKING**：`batch_id` 从请求体**删除**（它现在是路径参数）。
> 「批次不属于该 part」不再是可表达的场景。

业务流转：

- 起点状态：`INSPECTION`
- 终点状态：`READY_TO_SHIP`
- 多轮 rollup 守卫：service 检查 part 下其它 INSPECTION 批次；同 `batch-to-ship`
- 事件日志：`event_type='STATUS_CHANGED'`

WS 广播（commit 后下发）：

- `PART_TO_SHIP` —— payload `{ part_id }`
- **父装配件自动同步**：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。实际翻转时响应 `data.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。

Response 200 `data`：`ToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 通过品检后的 part 投影 |
| `new_batch_id` | string (i64)? | 仅拆批时为 `Some(remainder_id)`；整批操作时为 `null` |

错误码：

- 20101 BIZ_PART_NOT_FOUND — 批次所属工单已软删（**2026-10-02**：本端点不再有 part 路径参数，语义不变）
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单
- 20109 BIZ_PART_BATCH_NOT_FOUND — **2026-10-02 语义收窄**：`batch_id`（路径参数）不存在 / 已软删；或其状态不是 `INSPECTION`
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 40901 VERSION_CONFLICT — caller 传的 `version` ≠ 目标批次当前 `version`（见 [乐观锁](#乐观锁caller-侧-occ)）；或事务内 UPDATE 撞并发
- HTTP 422 — payload shape 错误（缺 `version` / 空 body）：axum `Json` extractor 在 service 之前直接拒，**非项目统一信封**
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/prod/batches/{batch_id}/to-process`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`ToProcessRequest`（body 必填）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（`zone='PRODUCTION'` 且 `is_active=true`）；违反 → `20501` / `20512` |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id（须与 `shelf_id` 在 `t_shelf_process` 存在映射 —— **当前实现仅校验 shelf 存在 / zone / active**；跨 shelf ↔ process 强校验留待 shelf 域 PR） |
| `note` | string? | — | ≤ 500 字符 |
| `version` | i32 | ✓ | 目标批次 `t_part_batch.version`（**不是** part 的 version）；不符 → `40901`，详见 [乐观锁](#乐观锁caller-侧-occ) |
| `quantity` | i32? | — | 本次打回数量；缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |

> **2026-10-02 BREAKING**：`batch_id` 从请求体**删除**（它现在是路径参数）。
> 「批次不属于该 part」不再是可表达的场景。

业务流转：

- 起点状态：`INSPECTION`
- 终点状态：`IN_PROCESS`（`location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id`）
- 多轮 rollup 守卫：同 `to-ship` —— 若 part 下还有其它 INSPECTION 批次，**part.status 保持 `INSPECTION`**
- 事件日志：`event_type='INSPECTION_FAILED'`
- ⚠️ **返修守卫（2026-10-01 引入）**：目标批次
  `is_repairing = true`（返修中）→ **20118** 拒绝，请改调
  [`POST /prod/batches/{batch_id}/complete-repair`](./lifecycle.md#post-apiv2prodbatchesbatch_idcomplete-repair)。
  理由：to-process 表达「检验不合格、回**正常生产流**继续做」，而 complete-repair
  才是**唯一**被授权清 `is_repairing` 的动作（它同样把批次落到生产架 + 写目标工序）。
  放行的话批次会停在生产架却仍挂「返修中」标记，在 `GET /prod/batches/repairing`
  里长期显示异常，且 complete-repair 仍会接受它（用户可把普通在制品当「完成返修」
  搬走）。可达链：`start-repair` → `to-inspection`（送检**保持**标记）→ 本端点；
  `scan-inspect(pass=false)` 那条链**不成立**（它的第二步把批次写成
  `status='IN_PROCESS'`，而本端点只捞 `status='INSPECTION'` 的批次）。
  改造前不可能发生：返修批次 `status='REPAIRING'`，不在 `allowed_from=["INSPECTION"]` 里。

WS 广播（commit 后下发）：

- `PART_TO_PROCESS` —— payload `{ part_id }`
- **父装配件自动同步**：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。实际翻转时响应 `data.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。

Response 200 `data`：`ToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 打回后的 part 投影 |
| `new_batch_id` | string (i64)? | 仅拆批时为 `Some(remainder_id)`；整批操作时为 `null` |

错误码：

- 20101 BIZ_PART_NOT_FOUND — 批次所属工单已软删（**2026-10-02**：本端点不再有 part 路径参数，语义不变）
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单；或 shelf 不在 PRODUCTION 区
- 20109 BIZ_PART_BATCH_NOT_FOUND — **2026-10-02 语义收窄**：`batch_id`（路径参数）不存在 / 已软删；或其状态不是 `INSPECTION`
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 20118 BIZ_PART_REPAIR_NOT_TRIGGERED — 目标批次**处于返修中**（`is_repairing = true`）；应改调 `complete-repair`（HTTP 400）
- 20501 BIZ_SHELF_NOT_FOUND — `shelf_id` 不存在
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` ↔ `next_process_id` 未映射（shelf 域 2026-08-26 已上线，**现已在 service 内触发**）
- 20512 BIZ_SHELF_INACTIVE — `shelf.is_active = false`
- 40901 VERSION_CONFLICT — caller 传的 `version` ≠ 目标批次当前 `version`（见 [乐观锁](#乐观锁caller-侧-occ)）；或事务内 UPDATE 撞并发
- HTTP 422 — payload shape / 必填字段缺失（`shelf_id` / `next_process_id` / `version` / 空 body）：axum `Json` extractor 在 service 之前直接拒，**非项目统一信封**
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/prod/batches/worker-scan`

权限: **Manager** / **ShelfAccount**（**scope 校验**：`shelf_id` 与 `target_inspection_shelf_id` 必须在 `current.shelf_ids` 内或 `current.shelf_wildcard=true`；否则 `40301 SHELF_MISMATCH`）

Request：`WorkerScanRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | ✓ | 扫码得到的序列号（service 反查 part） |
| `badge_code` | string | ✓ | 工人 badge_code（service 反查 worker） |
| `event_type` | string | ✓ | `"RETURNED"` / `"INSPECTED"`（`WorkerScanEvent` 枚举） |
| `shelf_id` | string (i64) | ✓ | RETURNED 时是 worker-scan 货架（PRODUCTION 区）；INSPECTED 时是工人触发扫码的货架（PRODUCTION 区校验） |
| `next_process_id` | string (i64)? | — | **仅 RETURNED 必填**；缺 / 非法 → `40001` |
| `target_inspection_shelf_id` | string (i64)? | — | **仅 INSPECTED 必填**；缺 / 非法 → `40001`；service 校验 `zone='INSPECTION'` 且 `is_active=true` |
| `batch_id` | string (i64)? | — | 多批次歧义时 caller 显式指定以消除歧义 |

> **本端点无 Path 参数**（`serial_no` 是主键，`batch_id` 只是可选消歧），故 2026-10-02
> 迁往 prod 域时**无 Path 表、请求体逐字不变**。它一笔事务改 2 个批次（扫的那个 + 同事务
> 从工人池补的），是全仓唯一的跨 part 批次写点，但动作语义仍是「以批次为对象的工人报工」，
> 故归 prod。
>
> **本端点豁免 `version`**：`worker-scan` 是「扫序列号 + 扫胸牌」的纯扫码流，前端手上没有批次 `version`（强加会要求工人先查一次批次）。该端点语义即「以 DB 当前状态为准」，仅保留 service 内部 OCC（事务内自读自写），不做 caller 侧 OCC。因此 `batch_id` 在这里仍是可选的，保留「按持有关系唯一匹配」推断。详见 [乐观锁](#乐观锁caller-侧-occ)。

业务流转：

- **RETURNED**：worker 把 IN_PROCESS+WORKER 批次放回生产架
  - `shelf_id` 必须映射 `next_process_id`（service 校验 `t_shelf_process`）→ 不匹配 `20507 BIZ_SHELF_PROCESS_NOT_MAPPED`
  - `part_batch` 与 `part` 状态切回 IN_PROCESS+PRODUCTION_SHELF+holder=shelf（OCC）
  - **2026-09-30（review H1 修复）：写 `current_process_id = next_process_id`** ——
    RETURNED 是全仓唯一的**工序推进**路径，批次归还货架后落进**下一道工序**的候选池。
    此前该列不写，批次带着旧工序 id 落回**原工序**池（权威列在主干流程上说谎）。
    另：RETURNED 仍**不更新** `current_process_step_id`（可选的显示用定位信息），
    这是已知缺口，影响仅限显示，池归属不受影响。
  - 写 `RETURNED_TO_SHELF` 事件日志
- **INSPECTED**：worker 把持有件直接送检
  - `target_inspection_shelf_id` 必须属于 INSPECTION 区且 active
  - 不符合 → `20511 BIZ_SHELF_NOT_INSPECTION_ZONE` / `20512 BIZ_SHELF_INACTIVE`
  - 内部走 `to_inspection_core`：状态机 `IN_PROCESS → INSPECTION` + holder worker → target_shelf + 写 `SENT_TO_INSPECTION` 事件日志
  - **2026-09-30（review H2 修复）：`current_process_id` 置 NULL**（送检 = 出池），
    与单件送检 `to-inspection` 口径一致
  - 不带 quantity 拆批（worker-scan 是单件持有件流转，不涉及批次拆分）
- **任一成功后**同事务调用 `WorkerPoolService::refill_for_worker`：
  - 工人当前工种可加工工序池有候选 → 自动抢满 `work_type.max_held_batches`（或池空为止）
  - 池空 → 业务侧返回 `data.refill.pool_empty=true`，不报错
  - refill 失败（如工种未映射工序）→ 业务错（如 `20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING`），事务回滚 scan 写入

> **OM-6 决议**：scan 与 refill 必须**同事务**——若 scan 成功后再调 refill，期间并发 worker 可能把同一批抢走，破坏「放回 → 抢下一批」原子语义。当前实现已合并到单事务。

Response 200 `data`：`WorkerScanOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `scan` | `WorkerScanCoreOut` | 扫码事件最小投影 |
| `refill` | [`RefillResult`](../production/worker-pool.md#refillresult-字段) | 同事务 refill 结果 |

错误码：

- 20101 BIZ_PART_NOT_FOUND — `serial_no` 无法解析为 part
- 20103 BIZ_INVALID_TRANSITION — part 当前状态不允许（INSPECTED 分支）
- 20109 BIZ_PART_BATCH_NOT_FOUND — 指定的 `batch_id` 不存在 / 已软删（**2026-10-02**：无 part 路径参数，不存在「不属于该工单」场景；未传 `batch_id` 时靠持有关系唯一匹配，多批次歧义走 20114）
- 20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER — `(worker, batch)` 不在 IN_PROCESS+WORKER 持有中（worker 不是该批次当前持有人）；或多批次歧义
- 20201 BIZ_WORKER_NOT_FOUND — `badge_code` 无法解析为 worker
- 20202 BIZ_WORKER_INACTIVE — worker 已停用
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL
- 20901 BIZ_WORK_TYPE_NOT_FOUND — worker.work_type_id 指向不存在的工种（防御性）
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — work_type.max_held_batches IS NULL
- 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING — work_type 未映射工序（refill 触发）
- 20501 BIZ_SHELF_NOT_FOUND — `shelf_id` 不存在 / 非 PRODUCTION / target 非 INSPECTION
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` 未映射 `next_process_id`（RETURNED 路径）
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`（INSPECTED 路径）
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`（INSPECTED 路径）
- 40301 SHELF_MISMATCH — `shelf_id` / `target_inspection_shelf_id` 不在 `current.shelf_ids` 内且非 wildcard
- 40300 FORBIDDEN — 非 Manager / 非 ShelfAccount
- HTTP 422 — payload shape / 必填字段缺失：axum `Json` extractor 在 service 之前直接拒，**非项目统一信封**
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 `event_type`）—— payload = `WorkerScanCoreOut`
- **父装配件自动同步**（仅 `INSPECTED` 分支）：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。**响应字段位置**：实际翻转时 `data.scan.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。`RETURNED` 分支不参与（part.status 不变，无副作用）。
- 若 `refill.taken.len() > 0` → `WORKER_POOL_REFILL_DONE` —— payload = `RefillResult`
- 若 `refill.pool_empty=true` 且 `taken` 为空 → `WORKER_POOL_EMPTY` —— payload `{ worker_id, shelf_id }`

详见 [`../websocket.md`](../websocket.md) 与 [`../production/worker-pool.md`](../production/worker-pool.md)。

---

### `GET /api/v2/parts/by-serial/{serial_no}/part-batches`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `serial_no` | string | 工单序列号（service 反查 part，串归一化同 `POST /prod/batches/worker-scan`） |

> 只读查询，无状态变更、无事务、无 WS 广播。

业务说明：

- 与既有 `/by-serial/{serial_no}` 共存但**字段更窄**：后者返回 `PartDetailOut`（`TPart` 完整 28 列 + `customer_name` / `l1_customer_name` / `current_batch_id` 冗余），适合详情页全字段渲染；本端点返回**工单窄字段 + 全部活跃批次**，专为扫码弹窗场景设计。
- 前端扫码弹窗场景：工人 / 拣货员扫序列号 → 弹窗显示 `PartScanInfoOut` + `PartBatchScanOut[]` → 操作员选定一个 `INSPECTION` 批次 → 直接把它作为路径参数调 `POST /prod/batches/{batch_id}/to-ship`（或用 `available_batches[]` 调 `POST /prod/batches/to-ship`）。省去先拉详情再单独拉批次的两次往返。
- **不返回 `customer_name`**（与 `by-serial` 的差异）：客户名取自 `t_customer`，本端点窄字段投影不冗余该列；若前端展示需要客户名，应另查 `t_customer` API。

Response 200 `data`：`PartScanContextOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartScanInfoOut`](#partscaninfoout-字段) | 工单窄字段（9 字段） |
| `batches` | [`PartBatchScanOut`](#partbatchscanout-字段)[] | 该 part 下全部活跃批次；按 `batch_no ASC` 排序；空数组表示该 part 尚无批次（不影响 200） |

#### `PartScanInfoOut` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工单雪花 ID |
| `drawing_no` | string | 图号 |
| `name` | string | 工单名 |
| `quantity` | i32 | 工单数量 |
| `customer_id` | string (i64) | 客户 id（**不返回 `customer_name`**） |
| `system_delivery_date` | date? | 计划交付日 |
| `is_urgent` | bool | 是否加急 |
| `order_no` | string? | 订单号 |
| `note` | string? | 备注 |

#### `PartBatchScanOut` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 批次雪花 ID |
| `quantity` | i32 | 批次数量 |
| `status` | string | 批次状态枚举字符串（`PENDING` / `PROGRAMMING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` / `COMPLETED` / `CANCELLED`） |
| `holder_name` | string? | 当前持有人名称（worker 真名 / 货架 code / null）；供弹窗直接展示，不用前端二次拼 holder 字典 |
| `version` | i32 | 乐观锁（供 to-ship / to-process / to-inspection 等 caller OCC 锚点；**此即 `t_part_batch.version`，不是 part 的 version**） |

排序：`batches` 按 `batch_no ASC`（批次创建顺序）；同一 part 下批次稳定顺序便于前端表格渲染。

错误码：

- 20101 BIZ_PART_NOT_FOUND — `serial_no` 无法解析为 part / 已软删

实现位置：

- handler：`src/modules/part/handler.rs::get_by_serial_part_batches` (line 423) → `PartScanContextOut`
- service：`src/modules/part/service/crud.rs::get_part_batches_by_serial` (line 366)
- dto：`src/modules/part/dto.rs`（`PartScanContextOut` / `PartScanInfoOut` / `PartBatchScanOut`）
- repo（批次 + holder 名称）：`src/modules/part_batch/repo.rs::list_active_by_part_id_with_holder` (line 547)，LEFT JOIN `t_worker` / `t_shelf` 拼 holder_name
- repo（part）：`src/modules/part/repo/part.rs::get_by_serial` (line 140)
- model：`src/modules/part_batch/model.rs::TPartBatch`（批次行 + 状态枚举）

---

### `GET /api/v2/prod/batches/inspection`

权限: **Manager / Inspector**

Query：

| 参数 | 类型 | 必填 | 默认值 | 校验 / 说明 |
|---|---|---|---|---|
| `keyword` | string | — | — | ILIKE 匹配 `t_part.drawing_no` / `name` / `serial_no` / `order_no`；含 `%` / `_` / `\\` → `40001 VALIDATION_ERROR` |
| `customer_id` | string (i64) | — | — | 单值；service 用 `expand_customer_id` 展开为 L1+L2 ids（关联 `t_customer.parent_id`） |
| `serial_no` | string | — | — | ILIKE 匹配 `t_part.serial_no`；含 `%` / `_` / `\\` → `40001 VALIDATION_ERROR` |
| `planned_delivery_date_from` | date | — | — | 范围下界（包含），匹配 `t_part.planned_delivery_date` |
| `planned_delivery_date_to` | date | — | — | 范围上界（包含），匹配 `t_part.planned_delivery_date` |
| `limit` | string (i64) | — | `200` | clamp 到 `[1, 200]`（`0` / 负数 → `1`；超过 `200` → `200`；非法 → `200`） |
| `offset` | string (i64) | — | `0` | `max(0, v)`（负数 / 非法 → 取 0） |

> 只读查询，无状态变更、无事务、无 WS 广播。

业务说明：

- 与现有 `by-serial/{serial_no}/part-batches` 的区别：
  - **`by-serial/.../part-batches`** —— 按序列号扫码上下文，单 part 的全部活跃批次（不限 status），工单窄字段 + 全部活跃批次；典型场景：工人扫序列号弹窗显示该工单下全部批次
  - **`/prod/batches/inspection`** —— 按状态筛选（固定 `status='INSPECTION'`）的全量批次列表，page-style（`limit` / `offset` / `total`），典型场景：品检员进入待品检队列页加载下一页
- 与 to-XXX 流程的衔接：本端点返回的 `batch_id` + `version` 是后续 `POST /prod/batches/{batch_id}/to-ship` / `to-process`（`batch_id` 作路径参数）或 `POST /prod/batches/to-ship`（`batch_id` 进 `items[]`）等 caller OCC 锚点的**权威来源**（前端列表页拿到后直接拼路径 / 请求体）。注意 `version` 是 `t_part_batch.version`，不是 `t_part.version`

排序：`is_urgent DESC, planned_delivery_date ASC, batch.id ASC`（紧急件优先 → 交期近优先 → 批次 id 兜底稳定排序）

Response 200 `data`：`InspectionBatchListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [`InspectionBatchListItemOut`](#inspectionbatchlistitemout-字段)[] | 批次列表（按上述排序规则排序） |
| `total` | string (i64) | 满足过滤条件的总数（`COUNT(*)`） |
| `limit` | string (i64) | 实际生效的 limit（clamp 后） |
| `offset` | string (i64) | 实际生效的 offset（max(0) 后） |

#### `InspectionBatchListItemOut` 字段

批次字段段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 批次雪花 ID |
| `batch_no` | string? | 批次号 |
| `quantity` | i32 | 批次数量 |
| `status` | string | 批次状态枚举字符串（本端点固定为 `INSPECTION`） |
| `is_repairing` | bool | 是否处于返修中（**2026-10-01 新增**，BREAKING）。直读 `t_part_batch.is_repairing` 标记列（migration 005/006），非 `Option`、无 `skip_serializing_if` ⇒ 恒定返回。`REPAIRING` 已从 `PartStatus` 降级，起修时 `status` 保持 `IN_PROCESS`（DB 不再产生 `REPAIRING` 字面量）；本字段与 `status` **正交**（起修后送检可得 `INSPECTION` + `is_repairing = true`，可达链见下方订正段）—— **「是否返修中」只能读本字段**。`/prod/batches/repairing` 的判据即 `is_repairing = true`。语义与 Rust 侧 `src/modules/prod/batch/vo.rs` 一致 |
| `location` | string? | 批次所在位置（`INSPECTION_SHELF` 等） |
| `version` | i32 | 乐观锁（`t_part_batch.version`，caller OCC 锚点） |
| `current_process_step_id` | string (i64)? | 逻辑 FK → `t_process_chain_step.id`（PR-3 批次 step 化 2026-09-16 新增，替代 next_process_id 列） |
| `parent_batch_id` | string (i64)? | 拆批来源的父批次 ID（仅拆批产生的新批次非 None） |

> 2026-09-16 PR-3 批次 step 化（migration 028）：删 `placed_at` 字段
> —— `t_part_batch.placed_at` 列已删（不再统计生产时间）。
> 新增 `current_process_step_id`（逻辑 FK → 工艺链步骤）。
>
> **2026-09-30（migration 004）**：`t_part_batch` 新增 `current_process_id`
> （逻辑 FK → `t_process.id`），作为批次**工序池归属的权威依据**。
> `current_process_step_id` 相应降级为**可选的显示用定位信息**
> （**只在首次定位工序时写、之后不再推进**）。
>
> **2026-09-30 review 第 3 轮 M3 —— 本 DTO 的工序字段仍走 step 派生，未改直读**
> 一度把 `next_process_id` / `next_process_name` 改直读 `current_process_id`，
> 但送检 = 出池、该列对 `INSPECTION` 批次**恒为 NULL**，而本端点只查
> `status='INSPECTION'` → 改直读会让这两个字段**恒 null**（用户可见回归，且原有
> 集成测试不断言该字段，无人发现）。已回退到 `current_process_step_id` →
> step JOIN 派生。
>
> **读取方分工（勿越界）**：`current_process_id` 只服务 5 条工序池 SQL
> （`/prod/pool/{process_id}` / `/prod/pool/counts` / `take_one_from_pool` /
> `take_specific_from_pool` / `count_pool_by_shelf_and_process`）+
> `list_pickable_by_work_type` + rollup 派生 `t_part.next_process_id`；
> **展示类列表（本端点 / `/prod/batches/repair` / part 批次明细 / dashboard）一律继续走
> step 派生**。两个 DTO 字段名（`next_process_id` / `next_process_name`）始终不变。
>
> 2026-09-16 PR-2（migration 027）：`InspectionBatchListItemOut` 删 `has_been_repaired`
> 字段 —— `t_part_batch.has_been_repaired` 列已删。
>
> **2026-10-01**：返修事实改由 `t_part_batch.is_repairing` 标记列 +
> `t_part_event.event_type='REPAIR_STARTED'` 事件日志共同追溯（详见
> [`../../api/parts/lifecycle.md`](../../api/parts/lifecycle.md) § start-repair）。
>
> **2026-10-02 订正 —— 上一段「都不新增 `is_repairing` 字段」的结论是错的**，
> 已由 2026-10-01 review 第 1 轮 M5 推翻（与
> [`./lifecycle.md` § GET /api/v2/prod/batches/repairing](./lifecycle.md#get-apiv2prodbatchesrepairing)
> 的记载矛盾）。**订正后的事实**：`is_repairing: bool` **已随本 VO 的 3 个共用端点
> （`/prod/batches/inspection` / `/prod/batches/repair` / `/prod/batches/repairing`）一起返回**，
> 见上方批次字段表。
>
> **为什么必须有这个字段**（原结论的推理漏洞，留档以免回潮）：
> - `REPAIRING` 降级为标记列后，DB 不再产生 `REPAIRING` 字面量（起修时 `status`
>   保持 `IN_PROCESS`）⇒ 前端的老判据 `status === 'REPAIRING'` 在**任何**端点
>   都取不到值。
> - 「前端从 `status` + 端点语义即可判断」不成立：**返修标记与 `status` 正交**。
>   返修中的批次可以停在非 `IN_PROCESS` 的状态上而标记不变 —— `start-repair`
>   （`IN_PROCESS` + 标记 `true`）之后走 `POST /prod/batches/{batch_id}/to-inspection` 或
>   worker-scan `INSPECTED`，两条路都经 `mark_batch_inspected`
>   （`is_repairing: None` = **保持**标记，`allowed_from` 含 `IN_PROCESS`）
>   ⇒ 批次落到 `INSPECTION` + `is_repairing = true`。该状态可达的反证是
>   `to-process` 对它有 20118 守卫（`src/modules/part/service/inspection_core.rs`
>   step 4.6）。于是 `/prod/batches/inspection`（判据 `status='INSPECTION'`）返回的行里
>   返修件与普通送检件**混在一起**；标记还会经 `mark_batch_passed_inspection` /
>   `mark_batch_delivered`（同样 `is_repairing: None` = 保持）一路带到
>   `DELIVERED`（`to-ship` 无返修守卫，只有 `to-process` 有），故
>   `/prod/batches/repair`（判据 `status='DELIVERED'`）同样混。端点语义（该端点在查什么）
>   推不出每一行是否返修中。
> - 「要区分就只调 `GET /api/v2/prod/batches/repairing`」也不是答案：该端点只
>   返回 `is_repairing = true` 的批次，覆盖不了「同一个列表里既有返修件又有
>   普通在制品」的展示场景，而后者才是队列类页面的常态。
>
> 字段形态：`bool`（**非** `Option`、**无** `#[serde(default)]`、**无**
> `skip_serializing_if`）⇒ **恒定出现在 JSON 里**，前端 Zod schema 必须按必填
> `boolean` 声明，不能 `.optional()`。同批新增的另一个 VO `PartBatchListItemOut`
> （`GET /api/v2/parts/{id}/batches`）同样有 `is_repairing: bool`，见
> [`./batch.md`](./batch.md)。

holder 解析段（LEFT JOIN `t_worker` / `t_shelf` 一次拼齐）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `current_holder_id` | string (i64)? | 当前持有人 id（worker.id 或 shelf.id） |
| `holder_name` | string? | 当前持有人名称（worker 真名 / 货架 code / null） |
| `next_process_id` | string (i64)? | 下一道工序 id（对应 `t_process`）。由 `current_process_step_id` 经 `LEFT JOIN t_process_chain_step` 取 `s.process_id` 派生（**不直读 `current_process_id`**，理由见上方 review M3 段） |
| `next_process_name` | string? | 下一道工序名称（`t_process.name`，`LEFT JOIN t_process np ON np.id = s.process_id` 拼齐） |

delivery_note 解析段（LEFT JOIN `t_delivery_note` 一次拼齐）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `delivery_note_id` | string (i64)? | 关联送货单 id（`t_part_batch.delivery_note_id`，LEFT JOIN 后填 `delivery_note_id`） |
| `delivery_note_no` | string? | 关联送货单号（`t_delivery_note.delivery_note_no`） |

工单字段段（`t_part`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |
| `serial_no` | string? | 工单序列号 |
| `drawing_no` | string | 图号 |
| `name` | string | 工单名 |
| `order_no` | string? | 订单号 |
| `planned_delivery_date` | date? | 计划交付日（用于范围过滤 + 排序） |
| `is_urgent` | bool | 是否加急（用于排序：紧急件优先） |
| `part_version` | i32 | part 聚合 version（**注意：caller OCC 必须用 `version`（即 `t_part_batch.version`），不能用 `part_version`**） |
| `created_at` | naive datetime | 工单创建时间 |
| `updated_at` | naive datetime | 工单更新时间 |

客户解析段（LEFT JOIN `t_customer` L1 一次拼齐）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `customer_id` | string (i64) | 工单客户 id（`t_part.customer_id`） |
| `customer_name` | string? | 客户名（`t_customer.name`） |
| `l1_customer_name` | string? | L1 客户名（`t_customer.parent_id` → `t_customer.name` 拼齐；与 `crud.md` 列表保持一致） |

错误码：

- 40001 VALIDATION_ERROR — `keyword` / `serial_no` 含通配符 `%` / `_` / `\\`
- 40100 UNAUTHORIZED — 未登录 / token 过期 / session 失效
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

实现位置：

- handler：`src/modules/prod/batch/handler/transition.rs`（`list_inspection_batches` → `InspectionBatchListOut`）
- service：`src/modules/prod/batch/service/list.rs`（`BatchService::list_inspection_batches`：状态过滤 + 范围展开 + clamp + ILIKE 校验）
- dto：`src/modules/prod/batch/dto.rs`（`InspectionBatchListQuery`）
- vo：`src/modules/prod/batch/vo.rs`（`InspectionBatchListItemOut` / `InspectionBatchListOut` + `From<Row> for InspectionBatchListItemOut` impl）
- repo：`src/modules/prod/batch/repo/list.rs`（`PartBatchRepo::list_batches_with_part` / `count_batches_with_part`，单 SQL JOIN 8 表：t_part_batch + t_part + t_worker + t_shelf + t_process + t_delivery_note + t_customer + L1 customer）
- model：`src/modules/prod/batch/model.rs`（`InspectionBatchListRow`）

---

## 乐观锁（caller 侧 OCC）

to-XXX 五个端点（`POST /prod/batches/to-inspection` / `/{batch_id}/to-inspection` /
`/to-ship` / `/{batch_id}/to-ship` / `/{batch_id}/to-process`）的入参必带 `version`，锚定的是
**`t_part_batch.version`**，不是 `t_part.version`。

理由：`t_part.status` / `t_part.version` 是该工单下所有批次的**聚合投影**（冗余列，为列表页免 join 而存在）。同一 part 下任意其它批次的操作都会把 `part.version` +1，用它当 caller 锚点会产生假冲突（A 批次送检 → `part.version` +1 → 与之无关的 B 批次 to-ship 请求误报 40901）。批次才是状态的真实载体。

- **单件端点**（`/{batch_id}/to-inspection` / `/{batch_id}/to-ship` / `/{batch_id}/to-process`）：`version` 不符 → 顶层 `40901 VERSION_CONFLICT`（HTTP 409），无副作用
- **批量端点**（`/to-inspection` / `/to-ship` 静态）：per-item 校验，不符的 item 落 `failed[] { code: 40901 }`，**不中断**其余 item（per-item savepoint 回滚该 item 的部分写入）
- `version` 从何而来：批次列表接口、`POST /delivery-notes/{id}/submit` 的 `unresolved_targets[].available_batches[].version`、`POST /delivery-notes/scan` 的 B 候选均已返回。注意**不能**用 `PartOut.version`（那是 part 的聚合 version）
- 拆批场景：`quantity < batch.quantity` 时 caller 送的 `version` 校验的是**拆批前的源批次**。拆批后源批次（= 响应里的 `new_batch_id`，留在源状态的 remainder）`version` 已 +1，caller 手上的旧值随即失效；响应体不含该新 `version`，对 remainder 的下一次操作前必须重新拉批次列表
- service 内部对 `t_part` / `t_part_batch` 的 UPDATE 仍带 `WHERE version=$n`，但那是**同事务自读自写**的防御，与 caller 侧 OCC 是两层，互不替代（两者都可能抛 40901）

> **`POST /api/v2/prod/batches/worker-scan` 豁免**：扫序列号 + 扫胸牌的纯扫码流，前端手上没有 `version`（强加会要求工人先查一次）。该端点语义即「以 DB 当前状态为准」，仅保留 service 内部 OCC。

---

## 自动拆批（auto-split）

to-XXX 流共用的部分通过拆批语义。**所有 5 个单 / 批端点行为一致**：

| `quantity` 取值 | 行为 |
|---|---|
| **缺省**（`None`） | 整批操作（`op_qty = batch.quantity`），**不拆批** |
| `quantity == batch.quantity` | 整批操作，**不拆批** |
| `quantity < batch.quantity`（且 > 0） | **拆批**：operated 部分（拆出的新批次，quantity = op_qty）走状态翻转；remainder 部分（原批次 quantity 减少）留在源状态待后续操作 |
| `quantity ≤ 0` | `20111 BIZ_PART_BATCH_INVALID_QUANTITY` |

> 注：`quantity > batch.quantity` 在新实现下**不会**触发 20111 —— service 直接走整批分支（`op_qty ≥ batch.quantity` 时不拆批），请求等价于 `quantity == batch.quantity`。

拆批响应语义（`ToXxxOut.new_batch_id`）：

| 操作类型 | `new_batch_id` | 部分说明 |
|---|---|---|
| 整批操作（缺省 / `quantity == batch.quantity`） | `null` | operated = 原批次，part.status 同步翻目标状态（若有 rollup 守卫则可能保留源状态） |
| 部分通过（`quantity < batch.quantity`） | `Some(remainder_id)` | operated = 拆出的新批次；part.status 由 rollup 守卫判定：若还有其它源状态批次则保留源状态，否则同步翻目标状态 |

**前端的拆批后处理**：

- 拿到非 null `new_batch_id` → 刷新批次列表（出现一行新批次 quantity = `batch.quantity - op_qty`）
- 拿到 null → 不需要刷新批次列表（仅 part.status 翻状态）

**回滚语义**：拆批写入与 operated 批次状态翻转在同一事务；事务失败时拆出的新批次与状态翻转一并回滚，不会出现「拆了批但没翻转」的中间态。

---

## Inspection 专属 DTO

### `BatchOpItem` 字段（`POST /prod/batches/to-ship` / `POST /prod/batches/to-inspection` 共用 item shape）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | **必填**；service 按 `id` 反查 `t_part_batch` 拿 `part_id` + `status`，DTO 不带 `part_id`。**2026-10-02：静态批量端点无 Path 参数，故 `batch_id` 仍在 body**（只有子资源端点把它挪进路径） |
| `version` | i32 | ✓ | **必填**；目标批次 `t_part_batch.version`；不符 → 该 item 落 `failed[] { code: 40901 }`，不中断其余 item |
| `quantity` | i32? | — | 缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |

> 重要：`batch_id` 是 `String`、`version` 是 `i32`（均非 `Option`）—— `#[serde(default)]` 不会被触发；缺字段由 axum `Json` extractor 在 service 之前直接拒（HTTP 422，非项目统一信封）。

> **2026-10-02 BREAKING**：以下 3 个子资源入参 struct 的 `batch_id` 字段**已删除**
> —— `batch_id` 由路径参数提供，重复出现在 body 里不再是契约的一部分（多余字段被
> serde 忽略，故旧 payload 不会报错，但 `version` 缺失会 422）。

### `ToInspectionRequest` 字段（`POST /prod/batches/{batch_id}/to-inspection` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | service 校验 `zone='INSPECTION'` + `is_active=true` |
| `version` | i32 | ✓ | **必填**；目标批次 `t_part_batch.version`；不符 → `40901` |
| `note` | string? | — | ≤ 500 字符 |
| `quantity` | i32? | — | 缺省 = 整批 |

### `ToShipRequest` 字段（`POST /prod/batches/{batch_id}/to-ship` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | **必填**；目标批次 `t_part_batch.version`；不符 → `40901` |
| `quantity` | i32? | — | 缺省 = 整批 |
| `note` | string? | — | ≤ 500 字符 |

> body 必填 —— `version` 为必填字段，`ToShipRequest` 未实现 `Default`。空 body / 缺
> `version` 由 axum `Json` extractor 在 service 之前直接拒（HTTP 422，非项目统一信封）。

### `ToProcessRequest` 字段（`POST /prod/batches/{batch_id}/to-process` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（PRODUCTION 区 active） |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id（与 shelf 映射 —— 当前仅校验 shelf 存在 / zone / active） |
| `version` | i32 | ✓ | **必填**；目标批次 `t_part_batch.version`；不符 → `40901` |
| `note` | string? | — | ≤ 500 字符 |
| `quantity` | i32? | — | 缺省 = 整批 |

### `ToXxxOut` 字段（单件 / 批量 to-XXX 端点共用出参）

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 操作后 part 的最新投影（含 OCC 更新后的 `version`） |
| `new_batch_id` | string (i64)? | 仅 `quantity < target.quantity` 走拆批分支时为 `Some(remainder_id)`（拆批后**剩余批次**的 id，留在源状态待后续操作）；整批操作时为 `null`（序列化为 JSON `null`）。前端拿到非 null 时应刷新批次列表 |
| `synced_assembly_id` | string (i64)? | 仅 to-inspection / to-process / to-ship / worker-scan 端点附带：父装配件（`assembly_id`）因本次 part 状态变更被翻转到新状态时为 `Some(asm_id)`（序列化为 JSON 字符串，与 `asm_id` 字段类型对称）；父 asm 已是终态或不存在时为 `null`。静态批量端点（`/prod/batches/to-inspection` / `/prod/batches/to-ship`）per-item 独立返回，前端应用 `HashSet` 去重后只对每个发生 flip 的 asm 拉一次详情 / 刷新列表 |

### `BatchToInspectionRequest` 字段（`POST /prod/batches/to-inspection` 入参；**body 逐字不变**）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 批量共享品检架（与单件入参同形校验） |
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_INSPECTION_MAX_ITEMS`（200） |

### `BatchToShipRequest` 字段（`POST /prod/batches/to-ship` 入参；**body 逐字不变**）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_SHIP_MAX_ITEMS`（200） |

> 不需要 `target_inspection_shelf_id`（to-ship 状态机终态是 `READY_TO_SHIP`，与品检货架无关）。

### `BatchOpFailure` 字段（per-item 失败明细）

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 失败的批次 ID（`i64` 是 service 已 parse 过，用 `serialize_i64` 序列化为 JSON 字符串与前端 batch_id 字段类型对称）；解析失败时填 `0` sentinel |
| `code` | i32 | item-level 错误码（透传 service 层：20101 / 20103 / 20104 / 20109 / 20111 / 20511 / 20512 / 40901） |
| `message` | string | 失败原因（中文，透传 service 层文案） |

### `BatchToXxxOut` 字段（`POST /prod/batches/to-ship` / `POST /prod/batches/to-inspection` 共用出参）

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | `ToXxxOut[]` | 成功并完成状态流转的 item（含 `PartOut` 最小投影 + 拆批后的 `new_batch_id`） |
| `failed` | `BatchOpFailure[]` | item 级别错误（按 `batch_id` 定位） |

> `submitted` 与 `failed` 互斥，单 item 不会同时出现在两侧。

---

## Phase F2 Rust 代码示例（to-XXX DTOs）

```rust
use serde::{Deserialize, Serialize};
use crate::shared::types::{deserialize_i64, serialize_i64, serialize_i64_opt};

/// 批量端点 item 公共结构（无 part_id；service 从 batch_id 反查）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchOpItem {
    pub batch_id: String,                    // 必填；非 Option
    pub version: i32,                        // 必填；t_part_batch.version（caller OCC）
    #[serde(default)]
    pub quantity: Option<i32>,               // 缺省 = 整批
}

/// 单件 to-inspection 入参（`POST /prod/batches/{batch_id}/to-inspection`）。
/// 2026-10-02：batch_id 由路径参数提供，从 body 移除。
#[derive(Debug, Clone, Deserialize)]
pub struct ToInspectionRequest {
    pub target_inspection_shelf_id: String,  // 必填（雪花 ID 字符串）
    pub version: i32,                        // 必填；t_part_batch.version
    #[serde(default)]
    pub note: Option<String>,                // ≤ 500 字符
    #[serde(default)]
    pub quantity: Option<i32>,               // 缺省 = 整批
}

/// 单件 to-ship 入参（`POST /prod/batches/{batch_id}/to-ship`）。
/// 注：未实现 `Default` derive —— body 不可省略；batch_id 来自路径参数（2026-10-02）。
#[derive(Debug, Clone, Deserialize)]
pub struct ToShipRequest {
    pub version: i32,                        // 必填；t_part_batch.version
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// 单件 to-process 入参（`POST /prod/batches/{batch_id}/to-process`）。
/// 2026-10-02：batch_id 由路径参数提供，从 body 移除。
#[derive(Debug, Clone, Deserialize)]
pub struct ToProcessRequest {
    pub shelf_id: String,                    // 必填（PRODUCTION 区 active）
    pub next_process_id: String,             // 必填
    pub version: i32,                        // 必填；t_part_batch.version
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 单件 / 批量 to-XXX 端点的统一出参 shape。
#[derive(Debug, Clone, Serialize)]
pub struct ToXxxOut {
    pub part: PartOut,                       // 操作后的 part 投影
    #[serde(serialize_with = "serialize_i64_opt")]
    pub new_batch_id: Option<i64>,           // 拆批 remainder id；整批为 None
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,     // 父 assembly 被翻转时为 Some(id)
}

/// 批量入参（`POST /prod/batches/to-inspection`）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToInspectionRequest {
    pub target_inspection_shelf_id: String,  // 批量共享品检架
    pub items: Vec<BatchOpItem>,             // 1..=200
}

/// 批量入参（`POST /prod/batches/to-ship`）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToShipRequest {
    pub items: Vec<BatchOpItem>,             // 1..=200
}

/// Per-item 失败明细。
#[derive(Debug, Clone, Serialize)]
pub struct BatchOpFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,                       // 解析失败时为 0 sentinel
    pub code: i32,                           // 业务错误码
    pub message: String,                     // 错误 message
}

/// 批量端点统一出参（`/prod/batches/to-ship` / `/prod/batches/to-inspection` 共用）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchToXxxOut {
    pub submitted: Vec<ToXxxOut>,            // 成功 item（含 part + new_batch_id）
    pub failed: Vec<BatchOpFailure>,         // 失败 item
}

// `PartOut` 字段集与 `model::TPartInspected` 完全对齐：
// id (i64 string) / serial_no / name / drawing_no / status / version /
// quantity / order_no / updated_at / updated_by
// （详见 [`./index.md`](./index.md#partout-字段)；
// 2026-09-16 PR-2 删 `actual_delivery_date`，由 t_part_event DELIVERED 事件派生）
```

---

## Phase W（worker-scan）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent { RETURNED, INSPECTED }

pub struct WorkerScanRequest {
    pub serial_no: String,                   // 扫码：序列号
    pub badge_code: String,                  // 扫码：工人 badge_code
    pub event_type: WorkerScanEvent,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,                       // worker-scan 货架（PRODUCTION 区 active）
    #[serde(default)]
    pub next_process_id: Option<String>,     // 仅 RETURNED 必填
    #[serde(default)]
    pub target_inspection_shelf_id: Option<String>,  // 仅 INSPECTED 必填
    #[serde(default)]
    pub batch_id: Option<String>,
}

pub struct WorkerScanCoreOut {
    pub worker_id: i64,
    pub part_id: i64,
    pub batch_id: i64,
    pub event_type: String,                  // "RETURNED" / "INSPECTED"
}

// WorkerScanOut = { scan: WorkerScanCoreOut, refill: RefillResult }
// （RefillResult 定义在 worker_pool/model.rs，详见 ../production/worker-pool.md）
```

---

## 状态机（can_transition_to 白名单）

> 状态**词汇**（8 值 + `is_repairing` 标记）与「三层派生 + status_gate 单一写入口」
> 见 [`./index.md#状态派生契约2026-10-01`](./index.md#状态派生契约2026-10-01)。
>
> 2026-10-01 起本表的**所有 `t_part_batch.status` 写入**都由
> `part::service::status_gate::apply_batch_status_change` 承担，源状态白名单
> （`allowed_from`）在 **SQL 层**与写入同属一条语句，不存在「service 校验通过 →
> 另一个人改掉状态 → 我的 UPDATE 照写」的 TOCTOU 窗口。0 行 → `40901`。
> 派生（part / assembly / 序列号释放）也在同一个函数里，故**调用方不需要也不应该
> 再手工调任何 sync**（手工补调会因第二次派生必为 `NoChange` 而把响应的
> `synced_assembly_id` 吞成 `null`）。

| from | to | 触发场景 |
|---|---|---|
| INSPECTION | READY_TO_SHIP | `POST /prod/batches/{batch_id}/to-ship`（单件）或 `POST /prod/batches/to-ship`（静态批量） |
| PROGRAMMING | INSPECTION | `POST /prod/batches/{batch_id}/to-inspection`（PROGRAMMING 工件）；`POST /prod/batches/to-inspection`；worker-scan INSPECTED |
| PENDING | INSPECTION | `POST /prod/batches/{batch_id}/to-inspection`（待下发工单）；`POST /prod/batches/to-inspection` |
| IN_PROCESS | INSPECTION | `POST /prod/batches/{batch_id}/to-inspection`（生产架工件，**必须 IN_PROCESS+PRODUCTION_SHELF**；service 层组合校验）；`POST /prod/batches/to-inspection`；worker-scan INSPECTED |
| INSPECTION | IN_PROCESS | `POST /prod/batches/{batch_id}/to-process`（品检打回 / 选下一工序） |
| IN_PROCESS | IN_PROCESS | worker-scan RETURNED（holder worker → shelf，状态不变） |
| READY_TO_SHIP | DELIVERED | `deliver`（同事务翻最近一条 source-status 批次） |
| DELIVERED | COMPLETED | `complete`（同事务；清空 `serial_no`） |
| IN_PROCESS | IN_PROCESS | **2026-10-01**：REPAIRING 降级为标记列 `t_part_batch.is_repairing`（migration 005/006），`start-repair` **不再是状态迁移** —— 只把标记置 `true`（`status` 保持 IN_PROCESS，progress 同档 2）；重复起修（标记已 true）→ 20118。`scan-inspect` `pass=false` 同理（INSPECTION → IN_PROCESS + 标记 true）|
| PENDING / PROGRAMMING / INSPECTION / READY_TO_SHIP / DELIVERED | CANCELLED | `cancel`（同事务翻最近一条 source-status 批次；delivery_note 锁禁） |

INSPECTION → IN_PROCESS 由 `POST /prod/batches/{batch_id}/to-process`（to_process 流）走 service 流程：

- INSPECTION 状态 + `location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id=...`
- 事件日志：`event_type='INSPECTION_FAILED'`

---

## 错误码参考（part / lifecycle）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20101 | BIZ_PART_NOT_FOUND | 404 | 工单不存在 / 已软删。**2026-10-02**：batch 子资源端点已无 part 路径参数，本码只能经「批次的 part 已软删」触发，语义不变 |
| 20103 | BIZ_INVALID_TRANSITION | 400 | 状态机白名单拒绝（cancel 时 COMPLETED / CANCELLED 等终态；to-XXX 时起点状态不匹配；IN_PROCESS 但 holder 是 worker） |
| 20104 | BIZ_INVALID_VALUE | 400 | DB status 字符串不在 enum 白名单；或 shelf 不在 PRODUCTION 区 |
| 20109 | BIZ_PART_BATCH_NOT_FOUND | 404 | **2026-10-02 语义收窄**：`batch_id` 反查失败 = 批次不存在 / 已软删 / 状态不是流转起点。`batch_id` 全局唯一即锚点，「不属于该 part」不再是独立场景（part 域 lifecycle 端点仍保留 part 路径参数，其 20109 另含「不属于该 part」） |
| 20111 | BIZ_PART_BATCH_INVALID_QUANTITY | 400 | **`quantity ≤ 0`**（拆批语义收紧：`quantity > batch.quantity` 不再报 20111，等价于整批操作） |
| 20114 | BIZ_PART_BATCH_NOT_HELD_BY_WORKER | 400 | worker-scan：worker 未持有该 part 活跃批次 / 多批次歧义 |
| 20115 | BIZ_PART_ALREADY_CANCELLED | 409 | cancel/deliver/complete/start-repair 遇到 CANCELLED 状态 |
| 20116 | BIZ_PART_NOT_DELIVERED | 400 | complete 要求 DELIVERED |
| 20117 | BIZ_PART_NOT_READY_TO_SHIP | 400 | deliver 要求 READY_TO_SHIP |
| 20118 | BIZ_PART_REPAIR_NOT_TRIGGERED | 400 | start-repair 要求 IN_PROCESS |
| 20119 | BIZ_PART_NOT_DELETABLE | 409 | soft-delete 终态禁 |
| 21420 | BIZ_DELIVERY_NOTE_LOCKED_PART | 409 | cancel / soft-delete 遇 part 已挂送货单 |
| 40001 | VALIDATION_ERROR | 422 | 入参 shape 错 / multipart 字段错 / 必填字段缺失（to-XXX 的 `version`；`batch_id` 改由路径提供） |
| 40300 | FORBIDDEN | 403 | 角色不符 |
| 40901 | VERSION_CONFLICT | 409 | caller 送的 `version` ≠ 目标批次当前 `version`（见 [乐观锁](#乐观锁caller-侧-occ)）；或事务内 UPDATE 撞并发 |

### 货架错误码（205xx — to-XXX / worker-scan 触发）

| code | 名称 | 触发场景 |
|---|---|---|
| 20501 | BIZ_SHELF_NOT_FOUND | shelf 不存在 / 已软删；worker-scan 时 shelf 不在 PRODUCTION 区 / target 非 INSPECTION 区 |
| 20507 | BIZ_SHELF_PROCESS_NOT_MAPPED | worker-scan RETURNED：`shelf_id` ↔ `next_process_id` 在 `t_shelf_process` 未映射；to-process 待 shelf 域 PR 启用 |
| 20511 | BIZ_SHELF_NOT_INSPECTION_ZONE | `target_inspection_shelf.zone ≠ 'INSPECTION'` |
| 20512 | BIZ_SHELF_INACTIVE | `shelf.is_active = false` |

---

### `POST /api/v2/prod/batches/scan/deliver`

权限: **已登录**

> 2026-09-23 起 PR12 补：扫码交付端点（区别于 `delivery_note/scan`，本端点是
> 工人扫码交付在制 part，触发 P3 流转）。前端从 PDA / 扫描枪直接 POST，body
> 仅含 `part_serial_no`（或 `batch_id`），服务端按状态机守卫 + 自动流转。
>
> **2026-10-02**：`ScanDeliverPartRequest` body 逐字不变，本端点**无 Path 参数**。
> 路由注册顺序要点：本路径首段静态 `scan`，与
> `POST /prod/batches/{batch_id}/*`（首段动态）**段数相同**，靠 axum / matchit 的静态优先
> 规则消解 —— 新增/调整同类路由时必须实测本路径未被 `/{batch_id}` 吞掉。

Request：

```json
{
  "serial_no": "string (必填；若用 batch_id 则二选一)",
  "batch_id": 1234567890,    // 可选；扫描到 part_id 后再用
  "to_worker_id": 42,         // 可选；指定交付目标工人（默认当前 user）
  "note": "string (可选)"
}
```

业务流转：
- 扫描 `serial_no` → 找到 part + 当前 active batch
- 状态机检查：`IN_PROCESS → DELIVERED`（worker scan 流程）
- 同事务：写 `t_part_event(event_type='DELIVERED')` + `t_part_event(actor_user_id=worker)`

Response 200 `data`：`{ part_id, batch_id, from_status, to_status, version }`。

错误码：
- 20101 / 20109 / 20114 / 20115 / 20116 / 20117 / 40901

---

> **2026-09-23 PR12 同步说明**：本节 1 个端点（`POST /prod/batches/scan/deliver`）原 docs/api/parts/inspection.md 未覆盖，本次按 PR11 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycleinspectionmd高优先级--大量端点缺失)）。
