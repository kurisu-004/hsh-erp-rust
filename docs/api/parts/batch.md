# part 域 — Batch 操作

> 本文件须与 `src/modules/part/{handler.rs,dto.rs, service/batch.rs}` 保持同步
> 通用约定见 [`../index.md`](../index.md)
> 共享 DTO 见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 3 个 batch 端点（batch-with-pdfs / `/{part_id}/batches` /
> `/{part_id}/batches/split`）。CRUD / lifecycle / inspection 见
> [`./crud.md`](./crud.md) / [`./lifecycle.md`](./lifecycle.md) / [`./inspection.md`](./inspection.md)。

> **2026-09-23 PR12 新增文件**：本节 3 个端点原 docs/api/parts/ 未覆盖，
> 本次按 PR11 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycleinspectionmd高优先级--大量端点缺失)）。

## 本文件目录

- [POST /api/v2/parts/batch-with-pdfs](#post-apiv2partsbatch-with-pdfs)
- [POST /api/v2/parts/{part_id}/batches](#post-apiv2partspart_idbatches)
- [POST /api/v2/parts/{part_id}/batches/split](#post-apiv2partspart_idbatchessplit)

---

### `POST /api/v2/parts/batch-with-pdfs`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 batch 创建（与 `POST /parts/batch` 同源不同形态）：一次性
> 创建 N 个 part + 同时上传 PDF 文件附件到 part_file。PartFile 上传走 OSS
> presigned URL（见 [`../files.md`](../files.md)）。

Request (multipart/form-data)：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | JSON | ✓ | `[PartBatchCreateItem]` 数组，每项含 `part_name` / `drawing_no` / `serial_no` / `quantity` |
| `pdfs` | file[] | — | 与 items 一一对应的 PDF 附件（可省略） |
| `default_work_type` | string? | — | 默认工种（如 `CNC`） |

Response 201 `data`：`PartBatchCreateOut`（见 [`./crud.md#partbatchcreateout-字段`](./crud.md#partbatchcreateout-字段)）

错误码：20102 / 20104 / 40001 / 40300。

### `POST /api/v2/parts/{part_id}/batches`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 split 准备：在 part 下创建新批次（区别于 `POST /parts/batch`
> 的"批量新建 part"，本端点是"在已有 part 下新开批次"，用于同 part 多批次流转）。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | part 雪花 ID |

Request：

```json
{
  "work_type": "string (必填)",
  "quantity": 1,
  "note": "string (可选)"
}
```

Response 201 `data`：`BatchOut`。

错误码：20101 / 20104 / 40001。

### `POST /api/v2/parts/{part_id}/batches/split`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 split：对 `t_part_batch` 拆批操作（一个批次 → 两个批次）。
> 用于：返工拆批 / 部分检验拆批 / 多工人合作拆批。`version` 锚
> `t_part_batch.version`。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | part 雪花 ID |

Request：

```json
{
  "batch_id": 1234567890,    // 必填；被拆的批次
  "version": 0,               // 必填；batch.version（OCC）
  "split_quantity": 1,        // 必填；新批次数量（>0 且 < source.batch.quantity）
  "reason": "string (可选)",
  "note": "string (可选)"
}
```

业务流转：source.quantity -= split_quantity → new_batch.quantity = split_quantity
两个新批次共享原状态（如 QUERY_TO_SHIP → 拆后两个批次都仍 QUERY_TO_SHIP）。

Response 200 `data`：`{ source_batch: BatchOut, new_batch: BatchOut, event: PartEventOut }`。

错误码：20101 / 20109 / 20111（quantity 非法）/ 40901。

---

## 共享 DTO

### BatchOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | batch 雪花 ID |
| `part_id` | string (i64) | 所属 part |
| `version` | i32 | OCC |
| `work_type` | string | 工种 |
| `status` | string | 状态机当前状态 |
| `quantity` | i32 | 当前数量 |
| `current_holder_worker_id` | string (i64)? | 当前持有的工人 |
| `current_inspection_shelf_id` | string (i64)? | 当前检测货架 |
| `next_process_id` | string (i64)? | 下一道工序 |
| `started_at` | naive datetime? | 开工时间 |
| `completed_at` | naive datetime? | 完成时间 |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

### PartBatchCreateItem 字段（POST /batch-with-pdfs 入参）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_name` | string | ✓ | |
| `drawing_no` | string? | — | |
| `serial_no` | string? | — | |
| `quantity` | i32 | ✓ | |
| `work_type` | string? | — | 覆盖 `default_work_type` |