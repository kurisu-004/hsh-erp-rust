# delivery-notes / 草稿变更

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · **`drafts.md`** · [`workflow.md`](./workflow.md) · [`print.md`](./print.md)

---

### `POST /api/v2/delivery-notes/scan`  （P3 扫码建单）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后 1..=64 字符；扫码载荷 |

Response 200 `data`：`ScanDeliveryOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `outcome` | string | `ADDED` / `ALREADY_PRESENT` |
| `resolved` | object | 解析结果，见下 |
| `note` | object | 命中/新建的送货单概要，见下 |
| `added_batches` | [object] | 本次新加入的批次 |
| `already_present` | [object] | 已在本单的批次 |
| `skipped` | [object] | 失败明细（200 路径下始终为空数组） |

`resolved`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `kind` | string | `"PART"` / `"ASSEMBLY"` |
| `id` | string (i64) | part.id 或 assembly.id |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |
| `assembly_id` | string (i64)? | 子件所属装配体（`kind=PART` 且属于装配时） |
| `child_count` | usize? | `kind=ASSEMBLY` 时的子件数 |

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

`added_batches[]` / `already_present[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `part_id` | string (i64) | |
| `serial_no` | string | |
| `quantity` | i32 | |

`skipped[]`（仅 21418 失败路径下填充）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `serial_no` | string | 失败子件序列号 |
| `name` | string | 失败子件名称 |
| `reason` | string | 失败原因说明 |
| `part_id` | string (i64)? | 工单 ID；21405 场景下为 `"0"`（无具体 part） |
| `batch_id` | string (i64)? | 批次 ID；21405 场景下为 `null` |
| `drawing_no` | string? | 图号；21405 场景下为 `null` |
| `status` | string? | 工单当前 status（如 `IN_PROCESS` / `READY_TO_SHIP` 等）；21405 场景下为 `null` |

> 21405 场景（零件状态非 READY_TO_SHIP）：`part_id="0"`、其余三字段为 `null`，由前端按 `reason` 文案兜底展示。21418 场景（装配件整套拒绝）：`part_id` 为真实工单 ID，可直接用其构造批量送检请求（见下文 21418 错误码）。

> 前端可基于 `data.failures[].status` 区分触发端点：
> - `status ∈ {PENDING, PROGRAMMING, IN_PROCESS}` → 触发 `POST /parts/batch-scan-inspect`（一键送检）
> - `status === 'INSPECTION'` → 触发 `POST /parts/batch-pass-inspection`（一键过检）

错误码：

- 20104 BIZ_INVALID_VALUE — code 空白/空
- 21407 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS — 单内混客户
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH — 零件分类与送货单范围不符
- 21417 BIZ_DELIVERY_SCAN_UNKNOWN_CODE — 扫码无法识别
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY — 装配件整套拒绝（**BizWithFailures**，data.failures 含跳过明细）。前端可基于 `data.failures[].part_id` 构造对 `POST /api/v2/parts/batch-pass-inspection` 的批量送检请求，免去多次 round-trip
- 40300 FORBIDDEN — 角色不符
- 40001 VALIDATION_ERROR — 角色不足

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