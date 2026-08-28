# part 域 — Inspection

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 6 个 inspection 端点（`to-inspection` / `batch-to-inspection` / `to-ship` / `batch-to-ship` / `to-process` / `worker-scan`）。CRUD / lifecycle / by-serial / upload-drawing 见 [`./crud.md`](./crud.md) / [`./lifecycle.md`](./lifecycle.md)。

## 本文件目录

- [POST /api/v2/parts/batch-to-inspection](#post-apiv2partsbatch-to-inspection)
- [POST /api/v2/parts/{part_id}/to-inspection](#post-apiv2partspart_idto-inspection)
- [POST /api/v2/parts/batch-to-ship](#post-apiv2partsbatch-to-ship)
- [POST /api/v2/parts/{part_id}/to-ship](#post-apiv2partspart_idto-ship)
- [POST /api/v2/parts/{part_id}/to-process](#post-apiv2partspart_idto-process)
- [POST /api/v2/parts/worker-scan](#post-apiv2partsworker-scan)

---

### `POST /api/v2/parts/batch-to-inspection`

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

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- 外层校验错误（顶层）：20511 / 20512（共享品检架不合法 → 整批失败）
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901

---

### `POST /api/v2/parts/{part_id}/to-inspection`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：`ToInspectionRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 目标品检架；service 校验 `zone='INSPECTION'` + `is_active=true`（违反 → `20511` / `20512` / `20501`） |
| `note` | string? | — | 送检备注；`≤ 500` 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时 caller 显式指定以消除歧义；缺省按状态唯一匹配 `{PENDING, PROGRAMMING, IN_PROCESS}` 批次（多批 → `20109`） |
| `quantity` | i32? | — | 本次送检数量；缺省 = 整批；详见 [自动拆批（auto-split）](#自动拆批auto-split) |

业务流转：

- 起点状态：`PENDING` / `PROGRAMMING` / `IN_PROCESS`
  - `IN_PROCESS` 必须 `location='PRODUCTION_SHELF'` + `current_holder_id` 命中 `t_shelf`（service 启发式区分 worker 持有 vs shelf 持有；worker 持有 → 20103 / "工人持有件请先归还或送检"）
- 终点状态：`INSPECTION`（`location='INSPECTION_SHELF'` + `current_holder_id=target_shelf.id`）
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

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不在 `{PENDING, PROGRAMMING, IN_PROCESS}` 白名单；或 `IN_PROCESS` 但 holder 是 worker；或 `IN_PROCESS` 但 holder 是非 PRODUCTION 区货架
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉；或缺省匹配下 `{PENDING, PROGRAMMING, IN_PROCESS}` 多于一个
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 20501 BIZ_SHELF_NOT_FOUND — `target_inspection_shelf_id` 不存在
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape 错误（如缺 `target_inspection_shelf_id`）
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/parts/batch-to-ship`

权限: **Manager / Inspector**

Request：`BatchToShipRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_SHIP_MAX_ITEMS`（200）；空数组 / 超出上限 → `40001` |

公共字段（`items[]`）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | **必填**；service 按 `id` 反查 `t_part_batch` 拿 part_id；找不到 → `20109 BIZ_PART_BATCH_NOT_FOUND` |
| `quantity` | i32? | — | 缺省 = 整批；详见 [自动拆批（auto-split）](#自动拆批auto-split) |

> 与 `batch-to-inspection` 同形；差异：
>
> - 不需要 `target_inspection_shelf_id`（to-ship 状态机终态是 `READY_TO_SHIP`，与品检货架无关）
> - 起点状态：item.batch 必须是 `INSPECTION`；非 INSPECTION → `20103`

Response 200 `data`：`BatchToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | `ToXxxOut[]` | 成功通过品检的 item；与 `items` 顺序一一对应 |
| `failed` | `BatchOpFailure[]` | 失败的 item（按 `batch_id` 定位） |

`ToXxxOut` / `BatchOpFailure` 形状同 `batch-to-inspection`。

业务流转：

- 起点状态：`INSPECTION`（item.batch.status 必须为 `INSPECTION`，否则 20103）
- 终点状态：`READY_TO_SHIP`
- 多轮 rollup：service 检查 part 下是否还有其它 INSPECTION 批次；若有，**part.status 保持 `INSPECTION`**（仅 operated 批次翻状态）；若无，part.status 同步翻 `READY_TO_SHIP`
- 事件日志：`event_type='STATUS_CHANGED'`（from=`INSPECTION` → to=`READY_TO_SHIP`）

WS 广播（commit 后下发）：

- `BATCH_TO_SHIP` —— payload `{ submitted: <count>, failed: <count> }`
- **父装配件自动同步**：若某 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。**响应字段位置**：实际翻转时，在该 item 对应 `data.submitted[i].synced_assembly_id = Some(<id>)`；不翻转时为 `null`（同 part 全无 assembly_id 时，整个字段缺省为 `null`）。

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901

---

### `POST /api/v2/parts/{part_id}/to-ship`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：`ToShipRequest`（**整个 body 可省略**，等价于全部字段全部 `None`；`Content-Length: 0` 按空对象处理）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 多 INSPECTION 批次歧义时必填；缺省按 part_id 唯一匹配 |
| `quantity` | i32? | — | 本次通过品检数量；缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |
| `note` | string? | — | ≤ 500 字符 |

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

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉；或多 INSPECTION 批次歧义
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape 错误
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/parts/{part_id}/to-process`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：`ToProcessRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（`zone='PRODUCTION'` 且 `is_active=true`）；违反 → `20501` / `20512` |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id（须与 `shelf_id` 在 `t_shelf_process` 存在映射 —— **当前实现仅校验 shelf 存在 / zone / active**；跨 shelf ↔ process 强校验留待 shelf 域 PR） |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多 INSPECTION 批次歧义时 caller 显式指定；缺省按状态唯一匹配 |
| `quantity` | i32? | — | 本次打回数量；缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |

业务流转：

- 起点状态：`INSPECTION`
- 终点状态：`IN_PROCESS`（`location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id`）
- 多轮 rollup 守卫：同 `to-ship` —— 若 part 下还有其它 INSPECTION 批次，**part.status 保持 `INSPECTION`**
- 事件日志：`event_type='INSPECTION_FAILED'`

WS 广播（commit 后下发）：

- `PART_TO_PROCESS` —— payload `{ part_id }`
- **父装配件自动同步**：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。实际翻转时响应 `data.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。

Response 200 `data`：`ToXxxOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 打回后的 part 投影 |
| `new_batch_id` | string (i64)? | 仅拆批时为 `Some(remainder_id)`；整批操作时为 `null` |

错误码：

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20104 BIZ_INVALID_VALUE — part 状态字段不在 enum 白名单；或 shelf 不在 PRODUCTION 区
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉；或多 INSPECTION 批次歧义
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0`
- 20501 BIZ_SHELF_NOT_FOUND — `shelf_id` 不存在
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` ↔ `next_process_id` 未映射（**待 shelf 域 PR 启用**，当前不报）
- 20512 BIZ_SHELF_INACTIVE — `shelf.is_active = false`
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

---

### `POST /api/v2/parts/worker-scan`

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

业务流转：

- **RETURNED**：worker 把 IN_PROCESS+WORKER 批次放回生产架
  - `shelf_id` 必须映射 `next_process_id`（service 校验 `t_shelf_process`）→ 不匹配 `20507 BIZ_SHELF_PROCESS_NOT_MAPPED`
  - `part_batch` 与 `part` 状态切回 IN_PROCESS+PRODUCTION_SHELF+holder=shelf（OCC）
  - 写 `RETURNED_TO_SHELF` 事件日志
- **INSPECTED**：worker 把持有件直接送检
  - `target_inspection_shelf_id` 必须属于 INSPECTION 区且 active
  - 不符合 → `20511 BIZ_SHELF_NOT_INSPECTION_ZONE` / `20512 BIZ_SHELF_INACTIVE`
  - 内部走 `to_inspection_core`：状态机 `IN_PROCESS → INSPECTION` + holder worker → target_shelf + 写 `SENT_TO_INSPECTION` 事件日志
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
| `refill` | [`RefillResult`](../worker-pool.md#refillresult-字段) | 同事务 refill 结果 |

错误码：

- 20101 BIZ_PART_NOT_FOUND — `serial_no` 无法解析为 part
- 20103 BIZ_INVALID_TRANSITION — part 当前状态不允许（INSPECTED 分支）
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
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
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 `event_type`）—— payload = `WorkerScanCoreOut`
- **父装配件自动同步**（仅 `INSPECTED` 分支）：若 `part.assembly_id IS NOT NULL`，同事务内按 [`auto-rollup 算法`](../assemblies/index.md#子件状态聚合auto-rollup) 聚合该 assembly 下所有子件状态，翻父 `t_assembly.status`。**响应字段位置**：实际翻转时 `data.scan.synced_assembly_id = Some(<id>)`；不翻转时为 `null`。`RETURNED` 分支不参与（part.status 不变，无副作用）。
- 若 `refill.taken.len() > 0` → `WORKER_POOL_REFILL_DONE` —— payload = `RefillResult`
- 若 `refill.pool_empty=true` 且 `taken` 为空 → `WORKER_POOL_EMPTY` —— payload `{ worker_id, shelf_id }`

详见 [`../websocket.md`](../websocket.md) 与 [`../worker-pool.md`](../worker-pool.md)。

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

### `BatchOpItem` 字段（`POST /batch-to-ship` / `POST /batch-to-inspection` 共用 item shape）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | **必填**；service 按 `id` 反查 `t_part_batch` 拿 `part_id` + `status`，DTO 不带 `part_id` |
| `quantity` | i32? | — | 缺省 = 整批；详见 [自动拆批](#自动拆批auto-split) |

> 重要：`batch_id` 是 `String`（非 `Option`）—— `#[serde(default)]` 不会被触发；缺字段直接 `40001 VALIDATION_ERROR`。

### `ToInspectionRequest` 字段（`{part_id}/to-inspection` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | service 校验 `zone='INSPECTION'` + `is_active=true` |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |

### `ToShipRequest` 字段（`{part_id}/to-ship` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 多 INSPECTION 批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |
| `note` | string? | — | ≤ 500 字符 |

> 整个 body 可省略，等价于全部字段全部 `None`。

### `ToProcessRequest` 字段（`{part_id}/to-process` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（PRODUCTION 区 active） |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id（与 shelf 映射 —— 当前仅校验 shelf 存在 / zone / active） |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多 INSPECTION 批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |

### `ToXxxOut` 字段（单件 / 批量 to-XXX 端点共用出参）

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | [`PartOut`](./index.md#partout-字段) | 操作后 part 的最新投影（含 OCC 更新后的 `version`） |
| `new_batch_id` | string (i64)? | 仅 `quantity < target.quantity` 走拆批分支时为 `Some(remainder_id)`（拆批后**剩余批次**的 id，留在源状态待后续操作）；整批操作时为 `null`（序列化为 JSON `null`）。前端拿到非 null 时应刷新批次列表 |

### `BatchToInspectionRequest` 字段（`POST /batch-to-inspection` 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 批量共享品检架（与单件入参同形校验） |
| `items` | `BatchOpItem[]` | ✓ | 1..=`BATCH_TO_INSPECTION_MAX_ITEMS`（200） |

### `BatchToShipRequest` 字段（`POST /batch-to-ship` 入参）

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

### `BatchToXxxOut` 字段（`POST /batch-to-ship` / `POST /batch-to-inspection` 共用出参）

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
    #[serde(default)]
    pub quantity: Option<i32>,               // 缺省 = 整批
}

/// 单件 to-inspection 入参（`POST /parts/{id}/to-inspection`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ToInspectionRequest {
    pub target_inspection_shelf_id: String,  // 必填（雪花 ID 字符串）
    #[serde(default)]
    pub note: Option<String>,                // ≤ 500 字符
    #[serde(default)]
    pub batch_id: Option<String>,            // 多批次歧义时必填
    #[serde(default)]
    pub quantity: Option<i32>,               // 缺省 = 整批
}

/// 单件 to-ship 入参（`POST /parts/{id}/to-ship`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToShipRequest {
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// 单件 to-process 入参（`POST /parts/{id}/to-process`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ToProcessRequest {
    pub shelf_id: String,                    // 必填（PRODUCTION 区 active）
    pub next_process_id: String,             // 必填
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
}

/// 单件 / 批量 to-XXX 端点的统一出参 shape。
#[derive(Debug, Clone, Serialize)]
pub struct ToXxxOut {
    pub part: PartOut,                       // 操作后的 part 投影
    #[serde(serialize_with = "serialize_i64_opt")]
    pub new_batch_id: Option<i64>,           // 拆批 remainder id；整批为 None
}

/// 批量入参（`POST /parts/batch-to-inspection`）。
#[derive(Debug, Clone, Deserialize)]
pub struct BatchToInspectionRequest {
    pub target_inspection_shelf_id: String,  // 批量共享品检架
    pub items: Vec<BatchOpItem>,             // 1..=200
}

/// 批量入参（`POST /parts/batch-to-ship`）。
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

/// 批量端点统一出参（`batch-to-ship` / `batch-to-inspection` 共用）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchToXxxOut {
    pub submitted: Vec<ToXxxOut>,            // 成功 item（含 part + new_batch_id）
    pub failed: Vec<BatchOpFailure>,         // 失败 item
}

// `PartOut` 字段集与 `model::TPartInspected` 完全对齐：
// id (i64 string) / serial_no / name / drawing_no / status / version /
// quantity / order_no / actual_delivery_date / updated_at / updated_by
// （详见 [`./index.md`](./index.md#partout-字段)）
```

---

## Phase W（worker-scan 不变）

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
// （RefillResult 定义在 worker_pool/model.rs，详见 ./worker-pool.md）
```

---

## 状态机（can_transition_to 白名单）

| from | to | 触发场景 |
|---|---|---|
| INSPECTION | READY_TO_SHIP | `POST /parts/{id}/to-ship`（单件）或 `POST /parts/batch-to-ship`（批量） |
| PROGRAMMING | INSPECTION | `POST /parts/{id}/to-inspection`（PROGRAMMING 工件）；`POST /parts/batch-to-inspection`；worker-scan INSPECTED |
| PENDING | INSPECTION | `POST /parts/{id}/to-inspection`（待下发工单）；`POST /parts/batch-to-inspection` |
| IN_PROCESS | INSPECTION | `POST /parts/{id}/to-inspection`（生产架工件，**必须 IN_PROCESS+PRODUCTION_SHELF**；service 层组合校验）；`POST /parts/batch-to-inspection`；worker-scan INSPECTED |
| INSPECTION | IN_PROCESS | `POST /parts/{id}/to-process`（品检打回 / 选下一工序） |
| IN_PROCESS | IN_PROCESS | worker-scan RETURNED（holder worker → shelf，状态不变） |
| READY_TO_SHIP | DELIVERED | `deliver`（同事务翻最近一条 source-status 批次） |
| DELIVERED | COMPLETED | `complete`（同事务；清空 `serial_no`） |
| IN_PROCESS | REPAIRING | `start-repair`（同事务翻最近一条 IN_PROCESS 批次；置 `has_been_repaired=true`） |
| PENDING / PROGRAMMING / INSPECTION / READY_TO_SHIP / DELIVERED | CANCELLED | `cancel`（同事务翻最近一条 source-status 批次；delivery_note 锁禁） |

INSPECTION → IN_PROCESS 由 `POST /parts/{id}/to-process`（to_process 流）走 service 流程：

- INSPECTION 状态 + `location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id=...`
- 事件日志：`event_type='INSPECTION_FAILED'`

---

## 错误码参考（part / lifecycle）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20101 | BIZ_PART_NOT_FOUND | 404 | 工单不存在 / 已软删 |
| 20103 | BIZ_INVALID_TRANSITION | 400 | 状态机白名单拒绝（cancel 时 COMPLETED/REPAIRING 等；to-XXX 时起点状态不匹配；IN_PROCESS 但 holder 是 worker） |
| 20104 | BIZ_INVALID_VALUE | 400 | DB status 字符串不在 enum 白名单；或 shelf 不在 PRODUCTION 区 |
| 20109 | BIZ_PART_BATCH_NOT_FOUND | 404 | inspection 流找不到目标批次 / 多批次歧义；批量端点 batch_id 反查失败 |
| 20111 | BIZ_PART_BATCH_INVALID_QUANTITY | 400 | **`quantity ≤ 0`**（拆批语义收紧：`quantity > batch.quantity` 不再报 20111，等价于整批操作） |
| 20114 | BIZ_PART_BATCH_NOT_HELD_BY_WORKER | 400 | worker-scan：worker 未持有该 part 活跃批次 / 多批次歧义 |
| 20115 | BIZ_PART_ALREADY_CANCELLED | 409 | cancel/deliver/complete/start-repair 遇到 CANCELLED 状态 |
| 20116 | BIZ_PART_NOT_DELIVERED | 400 | complete 要求 DELIVERED |
| 20117 | BIZ_PART_NOT_READY_TO_SHIP | 400 | deliver 要求 READY_TO_SHIP |
| 20118 | BIZ_PART_REPAIR_NOT_TRIGGERED | 400 | start-repair 要求 IN_PROCESS |
| 20119 | BIZ_PART_NOT_DELETABLE | 409 | soft-delete 终态禁 |
| 21420 | BIZ_DELIVERY_NOTE_LOCKED_PART | 409 | cancel / soft-delete 遇 part 已挂送货单 |
| 40001 | VALIDATION_ERROR | 422 | 入参 shape 错 / multipart 字段错 / 必填字段缺失 |
| 40300 | FORBIDDEN | 403 | 角色不符 |

### 货架错误码（205xx — to-XXX / worker-scan 触发）

| code | 名称 | 触发场景 |
|---|---|---|
| 20501 | BIZ_SHELF_NOT_FOUND | shelf 不存在 / 已软删；worker-scan 时 shelf 不在 PRODUCTION 区 / target 非 INSPECTION 区 |
| 20507 | BIZ_SHELF_PROCESS_NOT_MAPPED | worker-scan RETURNED：`shelf_id` ↔ `next_process_id` 在 `t_shelf_process` 未映射；to-process 待 shelf 域 PR 启用 |
| 20511 | BIZ_SHELF_NOT_INSPECTION_ZONE | `target_inspection_shelf.zone ≠ 'INSPECTION'` |
| 20512 | BIZ_SHELF_INACTIVE | `shelf.is_active = false` |
