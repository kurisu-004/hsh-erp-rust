# delivery-notes 域 API

> 本文件须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围字段：`scope_label` / `customer_path` / `delivery_group_id` 等用于"按范围创建草稿 + 防混客户"的展示与校验。
>
> `delivery_groups` 域见 [`../delivery-groups.md`](../delivery-groups.md)

## 目录

> **导航**：[**`index.md`**](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · [`workflow.md`](./workflow.md) · [`print.md`](./print.md)

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/delivery-notes/batch-detail` | 已登录 | 批量详情（按 id 列表，复用 `DeliveryNoteDetail`） |
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