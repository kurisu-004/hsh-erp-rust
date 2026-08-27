# part 域 — CRUD

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / PartListItem / PartListOut / PartDetailOut / 端点约束）见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 8 个 CRUD 端点（list / get / by-serial / create / batch / update / soft-delete / upload-drawing）。lifecycle / inspection 见 [`./lifecycle.md`](./lifecycle.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录


- [GET /api/v2/parts](#get-apiv2parts)
- [POST /api/v2/parts](#post-apiv2parts)
- [POST /api/v2/parts/batch](#post-apiv2partsbatch)
- [GET /api/v2/parts/{part_id}](#get-apiv2partspart_id)
- [GET /api/v2/parts/by-serial/{serial_no}](#get-apiv2partsby-serialserial_no)
- [POST /api/v2/parts/{part_id}/update](#post-apiv2partspart_idupdate)
- [POST /api/v2/parts/{part_id}/soft-delete](#post-apiv2partspart_idsoft-delete)
- [POST /api/v2/parts/{part_id}/upload-drawing](#post-apiv2partspart_idupload-drawing)

---

### `GET /api/v2/parts`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Query：

| 字段 | 类型 | 说明 |
|---|---|---|
| `customer_id` | string (i64)? | L1 → 自身 + L2 ids；L2 → 自身 + 同 L1 兄弟 ids |
| `status` | string? | 单状态过滤（PENDING / INSPECTION / READY_TO_SHIP / DELIVERED / COMPLETED / CANCELLED / 等） |
| `statuses` | string? | 多状态过滤，逗号分隔（如 `PENDING,READY_TO_SHIP`） |
| `is_urgent` | bool? | 紧急标记过滤 |
| `keyword` | string? | 模糊匹配 `name` / `drawing_no` / `serial_no` |
| `sort_by` | string? | 白名单 `CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它退化为 `CREATED_AT` |
| `sort_dir` | string? | `ASC` / `DESC`（缺省 `DESC`） |
| `limit` | int? | 1..=200（缺省 50） |
| `offset` | int? | ≥ 0（缺省 0） |

Response 200 `data`：`PartListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartListItem](#partlistitem-字段)[] | 含 TPart 完整列 + `customer_name` / `l1_customer_name` 冗余 |
| `total` | string (i64) | 满足过滤的总数（与 `items` 解耦，便于前端独立显示） |
| `limit` | string (i64) | 实际生效的 limit |
| `offset` | string (i64) | 实际生效的 offset |

错误码：40001（limit/offset 越界）、40300（角色不符）、50001（DB）。

### `POST /api/v2/parts`

权限: **Manager / Clerk**

Request：`PartCreateRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | 工单名 |
| `drawing_no` | string | ✓ | 图号 |
| `applicant_name` | string | ✓ | 申请人 |
| `quantity` | i32 | ✓ | > 0 |
| `request_date` | date | ✓ | 客户请求日 |
| `planned_delivery_date` | date | ✓ | 计划交付日 |
| `is_urgent` | bool | — | 缺省 `false` |
| `customer_id` | string (i64) | ✓ | 二级客户 id（雪花字符串） |
| `assembly_id` | string (i64)? | — | 父装配体（可选） |
| `order_no` | string? | — | 订单号 |
| `system_delivery_date` | date? | — | 系统派工日 |
| `note` | string? | — | 备注 |

Response 201 `data`：[`PartDetailOut`](#partdetailout-字段) — 含 TPart 完整列 + 客户冗余 + `current_batch_id`。

错误码：40001（字段空 / quantity≤0）、40300（角色不符）、20105（customer 不存在）。

### `POST /api/v2/parts/batch`

权限: **Manager / Clerk**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | 批量共享的二级客户 id |
| `items` | [PartBatchCreateItem](#partbatchcreateitem-字段)[] | ✓ | 1..=200；每件独立校验 |

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `created` | [PartDetailOut](#partdetailout-字段)[] | 成功插入并读取详情的件 |
| `failed` | [PartBatchCreateFailure](#partbatchcreatefailure-字段)[] | 单件失败明细（含 item_index）；成功与失败互斥 |

错误码：40001（items 空 / 超过 200）、40300；item-level（`failed[].code`）：50001 / 20101 等。

### `GET /api/v2/parts/{part_id}`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Response 200 `data`：[`PartDetailOut`](#partdetailout-字段)。

错误码：20101（不存在 / 已软删）、40300。

### `GET /api/v2/parts/by-serial/{serial_no}`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Response 同 [`GET /parts/{id}`](#get-apiv2partspart_id)；通过 `t_part.serial_no` partial unique 索引定位。

错误码：20101（不存在）、40300。

### `POST /api/v2/parts/{part_id}/update`

权限: **Manager / Clerk**

Request：`PartUpdateRequest` — 字段全部可选（缺省 = DB 不动）；`version` 必填（OCC）。

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁；与 DB 不匹配 → 40901 |
| `name` | string? | — | |
| `drawing_no` | string? | — | |
| `applicant_name` | string? | — | |
| `quantity` | i32? | — | > 0 |
| `order_no` | string? | — | |
| `system_delivery_date` | date? | — | |
| `planned_delivery_date` | date? | — | |
| `actual_delivery_date` | date? | — | |
| `note` | string? | — | |
| `is_urgent` | bool? | — | |

Response 200 `data`：[`PartDetailOut`](#partdetailout-字段)。

错误码：40901（版本冲突 / 已软删）、40300、20112（已流转禁改总量，留待后续 PR 启用）。

### `POST /api/v2/parts/{part_id}/soft-delete`

权限: **Manager**

Request：`{ "version": i32 }`

Response 200 `data: null`（软删成功；commit 后 WS 广播 `PART_SOFT_DELETED`）。

错误码：

- 20101 — part 不存在 / 已软删（HTTP 404）
- 40901 — version 不匹配（HTTP 409）
- 21420 — part 已挂送货单（HTTP 409）
- 20119 — 终态 DELIVERED/COMPLETED 禁删（HTTP 409）
- 40300 — 非 Manager

### `POST /api/v2/parts/{part_id}/upload-drawing`

权限: **Manager / Clerk**（**RBAC 在 handler 第一步守卫**，避免未授权请求触发 50 MB 内存分配）

Multipart 严格校验：

- 必须恰好含一个 `file` 字段；缺字段 / 多 `file` / 未知字段名一律 40001
- `file.content_type` 必须为 `application/pdf`（不默认可选 MIME，缺失 → 21102）
- `file` ≤ 50 MB → 21103

Response 200 `data`：最新 `TPartFile` 行（含 `content_type` / `file_size` / `content_sha256`）。

错误码：40001（multipart 字段错）、40300（角色不符）、21102（MIME 错）、21103（size 错）、21104（COS 失败）、21105（part 不存在）、21108（同 part+kind+sha256 撞唯一索引）。

---

## CRUD 专属 DTO

### PartCreateRequest / PartBatchCreateItem 字段

见上文 [`POST /parts`](#post-apiv2parts) / [`POST /parts/batch`](#post-apiv2partsbatch) 字段表。`PartBatchCreateItem` 不含 `customer_id`（提到 batch 级共享）。

### PartBatchCreateFailure 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64)? | `Some(id)` = INSERT 成功但 detail lookup 失败；`None` = INSERT 失败 |
| `code` | i32 | item-level 错误码 |
| `message` | string | 失败原因（中文） |
| `item_index` | usize | 在原 `items[]` 中的位置 |

### PartBatchCreateOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `created` | [PartDetailOut](#partdetailout-字段)[] | 成功创建的件 |
| `failed` | [PartBatchCreateFailure](#partbatchcreatefailure-字段)[] | 失败的件（`created` ∩ `failed` = ∅） |

### PartUpdateRequest 字段

见上文 [`POST /parts/{id}/update`](#post-apiv2partspart_idupdate) 字段表。
