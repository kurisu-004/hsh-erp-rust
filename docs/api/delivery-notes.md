# delivery-notes 域 API

> 本文件须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围字段：`scope_label` / `customer_path` / `delivery_group_id` 等用于"按范围创建草稿 + 防混客户"的展示与校验。
>
> `delivery_groups` 域见 [`./delivery-groups.md`](./delivery-groups.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/delivery-notes/scan` | Manager / Clerk / Inspector | P3 扫码建单（find-or-create 草稿） |
| GET | `/api/v2/delivery-notes/candidate-parts` | 已登录 | 候选入单零件（INSPECTION/READY_TO_SHIP） |
| GET | `/api/v2/delivery-notes/pickup-pending` | 已登录 | 待司机领取一览 |
| GET | `/api/v2/delivery-notes` | 已登录 | 列表（带过滤分页） |
| POST | `/api/v2/delivery-notes` | 已登录 | 创建草稿（可同时入件） |
| GET | `/api/v2/delivery-notes/{id}` | 已登录 | 详情（head + line_items + scanned_serials） |
| GET | `/api/v2/delivery-notes/{id}/events` | 已登录 | 事件时间线 |
| POST | `/api/v2/delivery-notes/{id}/update` | 已登录（DRAFT/SUBMITTED） | partial update（OCC） |
| POST | `/api/v2/delivery-notes/{id}/add-parts` | 已登录（DRAFT） | 加件（OCC） |
| POST | `/api/v2/delivery-notes/{id}/remove-parts` | 已登录（DRAFT） | 移除件（OCC） |
| POST | `/api/v2/delivery-notes/{id}/submit` | 已登录 | DRAFT → SUBMITTED（OCC） |
| POST | `/api/v2/delivery-notes/{id}/recall` | 已登录 | SUBMITTED → DRAFT（OCC） |
| POST | `/api/v2/delivery-notes/{id}/pickup-scan` | 已登录 | 拣货扫描（验证 part_serial 在本单） |
| POST | `/api/v2/delivery-notes/{id}/pickup` | 已登录 | SUBMITTED → PICKED_UP（OCC + 司机） |
| POST | `/api/v2/delivery-notes/{id}/soft-delete` | 已登录（仅 DRAFT） | 软删（OCC） |
| POST | `/api/v2/delivery-notes/{id}/print` | Manager / Clerk / Inspector | P4 打印送货单 xlsx |
| POST | `/api/v2/delivery-notes/{id}/print-labels` | Manager / Clerk / Inspector | P4 打印标签 xlsx |

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
| `serial_no` | string | |
| `name` | string | |
| `reason` | string | |

错误码：

- 20104 BIZ_INVALID_VALUE — code 空白/空
- 21407 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS — 单内混客户
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH — 零件分类与送货单范围不符
- 21417 BIZ_DELIVERY_SCAN_UNKNOWN_CODE — 扫码无法识别
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY — 装配件整套拒绝（**BizWithFailures**，data.failures 含跳过明细）
- 40300 FORBIDDEN — 角色不符
- 40001 VALIDATION_ERROR — 角色不足

### `GET /api/v2/delivery-notes/candidate-parts`

权限: 已登录

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | L1 客户 ID |

Response 200 `data`：`{ items: [DeliveryNoteCandidatePart] }`

`items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工单 ID |
| `batch_id` | string (i64) | 批次 ID（入单回传） |
| `batch_no` | i32 | |
| `batch_label` | string | |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |
| `quantity` | i32 | |
| `applicant_name` | string? | |
| `status` | string | `INSPECTION` / `READY_TO_SHIP` |
| `planned_delivery_date` | date? | |
| `order_no` | string? | |
| `customer_name` | string? | |
| `parent_customer_name` | string? | |
| `customer_path` | string? | |

错误码：

- 20102 BIZ_CUSTOMER_NOT_FOUND

### `GET /api/v2/delivery-notes/pickup-pending`

权限: 已登录（**ShelfAccount 仅看自己货架范围**）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64)? | — | 过滤客户 |

Response 200 `data`：`{ items: [DeliveryNoteSummary] }`

错误码：

- 40301 SHELF_MISMATCH

### `GET /api/v2/delivery-notes`

权限: 已登录

Query：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `statuses` | string? | — | 逗号分隔：`?statuses=DRAFT,SUBMITTED` |
| `customer_id` | string (i64)? | — | |
| `keyword` | string? | — | 关键字（送货单号/客户名） |
| `sort_by` | string? | `CREATED_AT` | `CREATED_AT` / `SUBMITTED_AT` / `PICKED_UP_AT` / `DELIVERY_NOTE_NO` |
| `sort_dir` | string? | `DESC` | `ASC` / `DESC` |
| `limit` | i64? | 50 | |
| `offset` | i64? | 0 | |

Response 200 `data`：`DeliveryNoteListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [DeliveryNoteSummary] | |
| `total` | i64 | |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

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

Response 200 `data`：[`DeliveryNoteDetail`](#deliverynotedetail-字段)

错误码：

- 20102 BIZ_CUSTOMER_NOT_FOUND
- 21405 BIZ_DELIVERY_NOTE_PART_NOT_READY
- 21406 BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY（data.failures）

### `GET /api/v2/delivery-notes/{id}`

权限: 已登录

Response 200 `data`：[`DeliveryNoteDetail`](#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND

### `GET /api/v2/delivery-notes/{id}/events`

权限: 已登录

Response 200 `data`：`[DeliveryNoteEvent]`

`DeliveryNoteEvent`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `delivery_note_id` | string (i64) | |
| `event_type` | string | 例：`CREATED` / `SUBMITTED` / `PICKED_UP` |
| `from_status` | string? | |
| `to_status` | string? | |
| `note` | string? | |
| `created_by` | string (i64)? | |
| `created_at` | naive datetime? | |

### `POST /api/v2/delivery-notes/{id}/update`

权限: 已登录（**DRAFT / SUBMITTED** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |
| `delivery_date` | date? | — | None = 不改 |
| `note` | string? | — | None = 不改 |

Response 200 `data`：[`DeliveryNoteSummary`](#deliverynotesummary-字段)

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

Response 200 `data`：[`DeliveryNoteDetail`](#deliverynotedetail-字段)

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

Response 200 `data`：[`DeliveryNoteDetail`](#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21412 BIZ_DELIVERY_NOTE_PARTS_LOCKED
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/submit`

权限: 已登录（**DRAFT → SUBMITTED**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteSummary`](#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/recall`

权限: 已登录（**SUBMITTED → DRAFT**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteSummary`](#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 21419 BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT — 同范围已存在 DRAFT
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/pickup-scan`

权限: 已登录

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_serial` | string | ✓ | 扫码的序列号 |
| `badge_code` | string? | — | 工人胸牌码 |

Response 200 `data`：`PickupScanOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `delivery_note_id` | string (i64) | |
| `scanned_count` | i64 | P2 阶段始终为 0（前端本地 Set 驱动） |
| `expected_count` | i64 | |
| `ready` | bool | |
| `scanned_serials` | [string] | P2 阶段始终为 `[]` |

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21408 BIZ_DELIVERY_NOTE_SCAN_MISMATCH — 序列号不在本单

### `POST /api/v2/delivery-notes/{id}/pickup`

权限: 已登录（**SUBMITTED → PICKED_UP**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `driver_worker_id` | string (i64) | ✓ | 司机 worker ID |
| `badge_code` | string? | — | |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteSummary`](#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 21404 BIZ_DELIVERY_NOTE_NOT_SUBMITTED — 非 SUBMITTED 不能 pickup
- 21409 BIZ_DELIVERY_NOTE_DRIVER_INVALID — 司机非送货司机/不活跃
- 21410 BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE — 还没扫齐（保留语义）
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

### `POST /api/v2/delivery-notes/{id}/print`  （P4 打印）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 批次 ID 序列；与 `line_items[*].id` 一一对应 |
| `merge_assemblies` | bool? | — | true → 同装配件子件合并一行（默认 `false`） |
| `merge_quantities` | object? | — | `{ "<assembly_id>": <count> }`，按装配件 ID 覆盖合并行数量 |

Response 200：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-note.xlsx"`
- Body: xlsx 二进制

错误码：

- 21109 BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED — root prefix 未配置模板
- 21112 BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS — 所选零件超过模板容量
- 21113 BIZ_DELIVERY_PRINT_BAD_ORDER（422） — custom_order 含非法 batch id 或漏行
- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION — 状态不允许打印

### `POST /api/v2/delivery-notes/{id}/print-labels`  （P4 标签打印）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 同 `/print` |
| `merge_assemblies` | bool? | — | 标签默认 `true`（与 Python 一致） |
| `merge_quantities` | object? | — | 同 `/print` |
| `line_item_ids` | [string (i64)]? | — | None / 缺省 = 全部数据行；Some([]) → 400 |

Response 200：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-labels.xlsx"`
- Body: xlsx 二进制

错误码：

- 同 `/print`，外加 20104 BIZ_INVALID_VALUE（line_item_ids=[]）

---

## 共享 DTO

### DeliveryNoteSummary 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `version` | i32 | |
| `delivery_note_no` | string | 例 `DN-20260824-0001` |
| `customer_id` | string (i64) | |
| `customer_name` | string? | |
| `parent_customer_name` | string? | |
| `customer_path` | string? | |
| `status` | string | DRAFT / SUBMITTED / PICKED_UP / COMPLETED |
| `submitted_at` | naive datetime? | |
| `picked_up_at` | naive datetime? | |
| `submitted_by` | string (i64)? | |
| `picked_up_by` | string (i64)? | |
| `driver_worker_id` | string (i64)? | |
| `driver_worker_name` | string? | |
| `part_count` | i64 | |
| `note` | string? | 备注 |
| `delivery_date` | date? | |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |
| `delivery_group_id` | string (i64)? | 送货分组 ID |
| `delivery_group_name` | string? | |
| `leaf_customer_id` | string (i64)? | |
| `leaf_customer_name` | string? | |
| `scope_label` | string? | 范围展示文案 |

### DeliveryNoteDetail 字段

```jsonc
{
  // head 字段与 DeliveryNoteSummary 全部相同（flatten）
  ...DeliveryNoteSummary,
  "line_items": [DeliveryNoteLineItem],
  "scanned_serials": []   // P2 阶段始终为 []（前端本地 Set 驱动）
}
```

`line_items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 批次 ID（行身份） |
| `part_id` | string (i64) | 工单 ID |
| `batch_no` | i32 | |
| `batch_label` | string | |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |
| `quantity` | i32 | |
| `is_urgent` | bool | |
| `status` | string | |
| `applicant_name` | string? | |
| `request_date` | date? | |
| `planned_delivery_date` | date? | |
| `system_delivery_date` | date? | |
| `order_no` | string? | |
| `note` | string? | |
| `customer_name` | string? | |
| `parent_customer_name` | string? | |
| `customer_path` | string? | |
| `is_scanned` | bool | 兼容字段 |
| `scanned` | bool | 兼容字段 |
| `assembly_id` | string (i64)? | 父装配体（子件行） |
| `assembly_serial_no` | string? | |
| `assembly_drawing_no` | string? | |
| `assembly_name` | string? | |
| `assembly_order_no` | string? | |
