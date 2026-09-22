# part 域 — Lifecycle

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
> 状态机 / 错误码见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)
>
> 范围：本文件覆盖 4 个 lifecycle 端点（deliver / cancel / complete / start-repair）。CRUD / inspection 见 [`./crud.md`](./crud.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录


- [POST /api/v2/parts/{part_id}/deliver](#post-apiv2partspart_iddeliver)
- [POST /api/v2/parts/{part_id}/cancel](#post-apiv2partspart_idcancel)
- [POST /api/v2/parts/{part_id}/complete](#post-apiv2partspart_idcomplete)
- [POST /api/v2/parts/{part_id}/start-repair](#post-apiv2partspart_idstart-repair)

---

### `POST /api/v2/parts/{part_id}/deliver`

权限: **Manager / Clerk**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：lifecycle 三端点（deliver /
> complete / start-repair）改为 **batch 级**，OCC 锚定 `t_part_batch.version`。
> 前端需先调 `GET /parts/by-serial/{serial_no}/part-batches` 拿 `batch_id` +
> `version`，再传入本端点。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id（雪花 i64）
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

业务流转：`READY_TO_SHIP → DELIVERED`（batch 级）；part 派生列由 PR-B2
rollup 自动回填；事件日志 `DELIVERED` 的 `batch_id` / `quantity` 来自
操作的批次。

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 流转后工单。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20117 — batch 当前状态非 READY_TO_SHIP（状态机白名单拒绝）
- 40901 — 乐观锁失败（batch version 冲突）

### `POST /api/v2/parts/{part_id}/cancel`

权限: **Manager / Clerk**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。同步翻转最近一条 source-status 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20103 — 当前状态不在 cancel 白名单（COMPLETED / REPAIRING / OUTSOURCE 等）
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED
- 21420 — part 已挂送货单，禁 cancel
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/complete`

权限: **Manager / Clerk**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：收 `batch_id` + `version`，锚定
> `t_part_batch.version`（与 inspection 三流一致）。状态机守卫读 batch 当前
> 状态 `DELIVERED → COMPLETED`；part 终态后 `serial_no` 被清空（序列号已
> 转交送货单）。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — part 不存在 / 软删
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20116 — batch 当前状态非 DELIVERED（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/start-repair`

权限: **Manager / Clerk / Inspector**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：收 `batch_id` + `version`，锚定
> `t_part_batch.version`。状态机守卫读 batch 当前状态 `IN_PROCESS → REPAIRING`。
>
> 2026-09-16 PR-2（migration 027）：`has_been_repaired` 列已从 `t_part` 与
> `t_part_batch` 双删 —— 拆批后无法确定是哪一个批次返修，列语义失真整体废弃。
> 返修事实改由 `t_part_event.event_type='REPAIR_STARTED'` 事件日志追溯（PR-2 §
> part/service/lifecycle.rs:422）。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "reason": "string (可选)",  // 优先作为事件 note
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — part 不存在 / 软删
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20118 — batch 当前状态非 IN_PROCESS（状态机白名单拒绝）
- 40901 — 乐观锁失败

---

## Lifecycle 专属 DTO

### DeliverRequest / CompleteRequest / StartRepairRequest 字段（PR-B3 batch 级）

三者均含必填 `batch_id` + `version`（锚 `t_part_batch.version`）+ 可选
`note` / `reason`（≤ 500 字符建议）。事件日志 `batch_id` / `quantity` 来自
操作的批次；cancel 与 start-repair 优先取 `reason` 作为事件 note。

### CancelRequest 字段（保持 part 级）

仅含可选 `reason` / `note`（cancel 走 part 级 + 级联取消全部活跃批次，详见
[重构方案 §4.2](../../refactor-part-assembly-batch.md#42-rollup-回调核心-新增-partservicesync_from_batch_change)）。

---

### `POST /api/v2/parts/{part_id}/pick-up`

权限: **Worker**（自己的 part 拣货；其它人需 Manager）

> 2026-09-22 起 P3 pickup 端点（替换原 `pick-up-batch` 独立端点为 batch-级 OCC 集成）。
> 用于 worker 主动拣走自己的在制件；`version` 锚定 `t_part_batch.version`。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20119 / 40901。

### `POST /api/v2/parts/{part_id}/place-on-shelf`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 lifecycle 端点（PR-B3 batch 级）。工人拣走件入库上货架。
> 状态机：`IN_PROCESS → ON_SHELF`。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "shelf_id": 42,            // 必填；货架 id
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20120 / 40400 / 40901。

### `POST /api/v2/parts/{part_id}/complete-repair`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 repair 收尾。`version` 锚 `t_part_batch.version`。
> 状态机：`REPAIRING → IN_PROCESS`。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20121 / 40901。

### `POST /api/v2/parts/{part_id}/repair-dispatch`

权限: **Manager**

> 2026-09-22 起 P3 repair 起始（与 `start-repair` 类似但用于派工而非自检）。
> 状态机：`IN_PROCESS → REPAIRING`。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "worker_id": 42,           // 必填；被派工工人
  "reason": "string (可选)",
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20118 / 40901。

### `GET /api/v2/parts/by-worker/{worker_id}`

权限: **已登录**

> 2026-09-22 起 P3 list by worker。返回该 worker 名下所有活跃 part 列表。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |

Query：`status?` / `limit?` / `offset?`（默认 50 / 0）。

Response 200 `data`：`{ items: [PartOut], total, limit, offset }`。

### `GET /api/v2/parts/outsource-sendable`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 list。返回可发送外协的 part 列表（状态 = READY_TO_SHIP 且无 active 外协 quote）。

Query：`process_id?` / `limit?` / `offset?`。

Response 200 `data`：`{ items: [PartOut], total, limit, offset }`。

### `GET /api/v2/parts/repair-batches`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 list。返回所有 REPAIRING 状态的 batch 汇总。

Query：`worker_id?` / `process_id?` / `limit?` / `offset?`。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

### `GET /api/v2/parts/repairing-batches`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 list（与 `repair-batches` 同语义，区别：含 worker 在制中）。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

---

> **2026-09-23 PR12 同步说明**：本节 8 个端点（pick-up / place-on-shelf / complete-repair / repair-dispatch / 4 个 GET 列表）原 docs/api/parts/lifecycle.md 未覆盖，本次按 PR11 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycleinspectionmd高优先级--大量端点缺失)）。
