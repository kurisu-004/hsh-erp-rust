# delivery-notes / 草稿变更

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · **`drafts.md`** · [`workflow.md`](./workflow.md) · [`print.md`](./print.md)

## 本文件目录

1. [POST /api/v2/delivery-notes/scan (P3 扫码建单)](#post-apiv2delivery-notesscan--p3-扫码建单)
2. [POST /api/v2/delivery-notes](#post-apiv2delivery-notes)
3. [POST /api/v2/delivery-notes/{id}/update](#post-apiv2delivery-notesidupdate)
4. [POST /api/v2/delivery-notes/{id}/add-parts](#post-apiv2delivery-notesidadd-parts)
5. [POST /api/v2/delivery-notes/{id}/remove-parts](#post-apiv2delivery-notesidremove-parts)
6. [POST /api/v2/delivery-notes/{id}/soft-delete](#post-apiv2delivery-notesidsoft-delete)

---

### `POST /api/v2/delivery-notes/scan`  （P3 扫码建单）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后 1..=64 字符；扫码载荷 |

Response 200 `data`：`ScanDeliveryOut`

> **批次状态 3 分组（路线 B，2026-08-27）**——扫码时按 `t_part_batch.status` 把命中的批次分成三组：
>
> | 组 | 状态 | 行为 |
> |---|---|---|
> | **A 直接入单** | `READY_TO_SHIP` / `INSPECTION`（且 `delivery_note_id IS NULL`） | 进 `added_batches[]` |
> | **B 候选→一键送检** | `PENDING` / `PROGRAMMING` / `IN_PROCESS`（未被工人持有）/ `REPAIRING` | 进 `unresolved_targets[i].available_batches[]`，前端调 `to-inspection` 后 re-scan |
> | **C 直接报错** | `DELIVERED` / `OUTSOURCE` / `IN_PROCESS`（工人持有）/ `COMPLETED` / `CANCELLED` | 短路报错 `21421` |

```jsonc
{
  "outcome": "ADDED" | "ALREADY_PRESENT" | "CANDIDATES_AVAILABLE" | "PARTIAL_ADDED",
  "resolved": { "kind": "PART", "id": "…", "serial_no": "…", "drawing_no": "…", "name": "…" },
  "note": { /* ScanDeliveryNoteSummaryDto */ },
  "added_batches": [ /* AddedBatchDto */ ],
  "unresolved_targets": [ /* UnresolvedTargetDto */ ]   // 字段缺省即表示无（serde skip_none）
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `outcome` | string | `ADDED` / `ALREADY_PRESENT` / `CANDIDATES_AVAILABLE` / `PARTIAL_ADDED` |
| `resolved` | object | 解析结果，见下 |
| `note` | object | 命中/新建的送货单概要，见下 |
| `added_batches` | [object] | 本次新加入的批次（A 组）；无则为 `[]` |
| `unresolved_targets` | [object]? | B 组候选清单；无则**字段不出现**（`skip_serializing_if`） |

4 场景 → outcome 映射：

| 场景 | outcome | `added_batches` | `unresolved_targets` |
|---|---|---|---|
| ① 散件全 A 组 | `ADDED` | ✓ | — |
| ② 散件仅 B 组 | `CANDIDATES_AVAILABLE` | `[]` | ✓（1 个元素） |
| ③ 装配件全 A 组 | `ADDED` | ✓ | — |
| ④ 装配件 A+B 混合 | `PARTIAL_ADDED` | ✓（A 组已挂载部分） | ✓（每个 B 组子件 1 个元素） |
| 全部批次已挂本单 | `ALREADY_PRESENT` | `[]` | — |

`resolved`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `kind` | string | `"PART"` / `"ASSEMBLY"` |
| `id` | string (i64) | part.id 或 assembly.id |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |

`note`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `delivery_note_no` | string | |
| `version` | i32 | |
| `status` | string | DRAFT / SUBMITTED / PICKED_UP / ... |
| `scope_label` | string | 范围展示文案 |
| `customer_path` | string | L1 > L2 > ... |
| `line_count` | usize | |
| `recent_items` | [RecentItem] | 最近加入的批次（最多 8 条），见下 |

`note.recent_items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `part_id` | string (i64) | |
| `serial_no` | string? | |
| `drawing_no` | string | |
| `name` | string | |
| `order_no` | string? | |

`added_batches[]`（`AddedBatchDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `part_id` | string (i64) | 跨子件场景需保留（装配件一次挂多个 part 的批次） |
| `serial_no` | string | 来自 `part.serial_no` |
| `quantity` | i32 | |

`unresolved_targets[]`（`UnresolvedTargetDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 未就绪工单 ID |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |
| `available_batches` | [object] | 该工单的 B 组候选批次，见下 |

`unresolved_targets[i].available_batches[]`（`AvailableBatchDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `quantity` | i32 | |
| `status` | string | `BatchStatusDto` 枚举，见下 |

> part 级信息（`serial_no` / `drawing_no` / `name`）只在外层 `UnresolvedTargetDto` 上，`available_batches[]` 内不重复。

`BatchStatusDto`（`t_part_batch.status` 强类型投影，序列化沿用 DB 列值）：

`PENDING` / `PROGRAMMING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` / `REPAIRING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED`

> 前端可基于 `unresolved_targets[i].available_batches[].status` 区分触发端点：
> - `status ∈ {PENDING, PROGRAMMING, IN_PROCESS, REPAIRING}` → 触发一键送检（`to-inspection` / `POST /parts/batch-scan-inspect`），成功后 re-scan 同一 code 完成入单

错误码：

| 错误码 | 常量 | 说明 |
|---|---|---|
| `20104` | `BIZ_INVALID_VALUE` | code 空白/空，或 trim 后长度不在 1..=64 |
| `21407` | `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` | 单内混客户 |
| `21416` | `BIZ_DELIVERY_NOTE_SCOPE_MISMATCH` | 零件分类与送货单范围不符 |
| `21417` | `BIZ_DELIVERY_SCAN_UNKNOWN_CODE` | 扫码无法识别（既非 part 也非 assembly 序列号） |
| `21406` | `BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED` | 所有目标批次都已挂在其它 active 单上（无可选 → 硬错误） |
| `21421` | `BIZ_DELIVERY_BATCH_STATE_INVALID` | scan 路径 C 组短路（DELIVERED / OUTSOURCE / IN_PROCESS 工人持有 / COMPLETED / CANCELLED） |
| `21405` | `BIZ_DELIVERY_NOTE_PART_NOT_READY` | **已不再由 scan 触发**；保留给 `add-parts` 等端点 |
| `21418` | `BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY` | **已不再由 scan 触发**（场景 ④ 改走 200 `PARTIAL_ADDED`）；保留给 `add-parts` 等端点 |
| `40300` | `FORBIDDEN` | 角色不符 |
| `40001` | `VALIDATION_ERROR` | 角色不足 |

### `POST /api/v2/delivery-notes`

权限: 已登录（**service 层 enforce 角色**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | |
| `delivery_date` | date? | — | |
| `items` | [AddItem] | — | 可同时入单（与 `/add-parts` 等价） |
| `note` | string? | — | 备注 |

`items[]`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | |
| `quantity` | i32? | — | None = 整批；Some(n) 且 n < batch.quantity → 拆分 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 20102 BIZ_CUSTOMER_NOT_FOUND
- 21405 BIZ_DELIVERY_NOTE_PART_NOT_READY
- 21406 BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY（data.failures）

### `POST /api/v2/delivery-notes/{id}/update`

权限: 已登录（**DRAFT / SUBMITTED** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |
| `delivery_date` | date? | — | None = 不改 |
| `note` | string? | — | None = 不改 |

Response 200 `data`：[`DeliveryNoteSummary`](./index.md#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION — 状态不允许修改
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/add-parts`

权限: 已登录（**DRAFT** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [AddItem] | ✓ | 同 `POST /` 的 items |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21405 BIZ_DELIVERY_NOTE_PART_NOT_READY
- 21406 BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
- 21412 BIZ_DELIVERY_NOTE_PARTS_LOCKED — SUBMITTED/PICKED_UP 后禁止
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY（data.failures）
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/remove-parts`

权限: 已登录（**DRAFT** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_ids` | [string (i64)] | ✓ | |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21412 BIZ_DELIVERY_NOTE_PARTS_LOCKED
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/soft-delete`

权限: 已登录（**仅 DRAFT**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`: `null`

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21403 BIZ_DELIVERY_NOTE_NOT_DRAFT — 非 DRAFT 不能软删
- 40901 VERSION_CONFLICT