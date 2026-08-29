# delivery-notes / 查询

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · **`queries.md`** · [`drafts.md`](./drafts.md) · [`workflow.md`](./workflow.md) · [`print.md`](./print.md)

## 本文件目录

1. [GET /api/v2/delivery-notes/batch-detail](#get-apiv2delivery-notesbatch-detail)
2. [GET /api/v2/delivery-notes/candidate-parts](#get-apiv2delivery-notescandidate-parts)
3. [GET /api/v2/delivery-notes/pickup-pending](#get-apiv2delivery-notespickup-pending)
4. [GET /api/v2/delivery-notes](#get-apiv2delivery-notes)
5. [GET /api/v2/delivery-notes/{id}](#get-apiv2delivery-notesid)
6. [GET /api/v2/delivery-notes/{id}/events](#get-apiv2delivery-notesidevents)

---

### `GET /api/v2/delivery-notes/batch-detail`

权限: 已登录

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ids` | string | ✓ | 逗号分隔 i64 列表；1..=200 个；服务端 trim 后解析并去重（保留首次出现顺序） |

校验（按顺序触发，先匹配先返回）：

- `ids` 缺失 / 全空白 → 20104 BIZ_INVALID_VALUE
- 任一 token 解析为非整数 → 20104 BIZ_INVALID_VALUE
- 去重后超过 200 个 → 20104 BIZ_INVALID_VALUE

Response 200 `data`：`{ items: [DeliveryNoteDetail] }`

> 复用 [`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段) DTO。`items` 长度可能小于请求 id 数：不存在 / 已软删的 id 静默跳过，不报错。

只读端点，不广播 WS 事件。静态段 `/batch-detail` 必须先于 `/{id}` 注册（axum 路由顺序约束，否则会被 `{id}` 捕获）。

错误码：

- 20104 BIZ_INVALID_VALUE

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

Response 200 `data`：`{ items: [DeliveryNoteOut] }`

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
| `items` | [DeliveryNoteOut] | |
| `total` | i64 | |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### `GET /api/v2/delivery-notes/{id}`

权限: 已登录

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

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