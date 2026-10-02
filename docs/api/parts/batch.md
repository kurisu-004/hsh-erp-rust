# Batch 操作 —— part 的批次集合 + 批次拆分

> 通用约定见 [`../index.md`](../index.md)
> 共享 DTO 见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 4 个 batch 端点（`POST /api/v2/parts/batch-with-pdfs` /
> `GET /api/v2/parts/{part_id}/batches` / `POST /api/v2/parts/{part_id}/batches` /
> `POST /api/v2/prod/batches/{batch_id}/split`）。CRUD / lifecycle / inspection 见
> [`./crud.md`](./crud.md) / [`./lifecycle.md`](./lifecycle.md) / [`./inspection.md`](./inspection.md)。
>
> **2026-10-02 归属变更**：`split` 以**单个批次**为操作对象，已迁到 prod 域
> `/api/v2/prod/batches/{batch_id}/split`，`batch_id` 改由路径提供。part 域保留
> `GET|POST /api/v2/parts/{part_id}/batches`（part 的批次集合读 / 在 part 下新开批次）——
> 操作对象是 part，不是某个批次。
>
> **2026-09-23 PR12 新增文件**：本节 3 个端点原 docs/api/parts/ 未覆盖，
> 本次按 PR11 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycleinspectionmd高优先级--大量端点缺失)）。

> **2026-09-30 Phase 2 dashboard 二次调整**：新增 `GET /api/v2/parts/{part_id}/batches`
> 文档章节，`PartBatchListItemOut` 字段从 11 扩展到 18（详见后文）。
> **2026-10-02 订正**：该 VO 现为 **19 字段**（新增 `is_repairing`），见下文
> 字段表下的订正段；上面这句保留 2026-09-30 当天的历史数字，不改。

## 本文件目录

- [POST /api/v2/parts/batch-with-pdfs](#post-apiv2partsbatch-with-pdfs)
- [GET /api/v2/parts/{part_id}/batches](#get-apiv2partspart_idbatches)
- [POST /api/v2/parts/{part_id}/batches](#post-apiv2partspart_idbatches)
- [POST /api/v2/prod/batches/{batch_id}/split](#post-apiv2prodbatchesbatch_idsplit)

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

### `GET /api/v2/parts/{part_id}/batches`

工单全部活跃批次列表（含 holder 名称 / 下一工序 / 父批次 / 送货单号 / 时间戳等元信息）。

**权限**：Manager / Clerk / Inspector / CncProgrammer（2026-09-30 Phase 2 与 list 端点对齐，沿用 PartListQuery 权限层级）。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | path `i64` | 工单雪花 ID |

Response 200 `data`：`PartBatchListItemOut[]`，数组按 `batch_no ASC` 升序。

错误码：20101 / 40001 / 40300。

#### `PartBatchListItemOut` 字段（2026-09-30 Phase 2 dashboard 二次调整扩展）

| 字段 | 类型 | nullable | 说明 |
|---|---|---|---|
| `id` | string | no | 批次雪花 ID |
| `part_id` | string | no | 所属工单 ID |
| `batch_no` | number | no | 批次序号（同 part 内递增） |
| `batch_label` | string | no | 展示标签，格式 `L{id}`（与 delivery_note 一致） |
| `quantity` | number | no | 批次数量 |
| `status` | string | no | `OrderStatus` 枚举字符串 |
| `is_repairing` | bool | no | 是否处于返修中（**2026-10-01 新增**，BREAKING）。直读 `t_part_batch.is_repairing` 标记列（migration 005/006）；非 `Option`、无 `skip_serializing_if` ⇒ 恒定返回。`REPAIRING` 已从 `PartStatus` 降级，起修时 `status` 保持 `IN_PROCESS` ⇒ 判断「返修中」只能读本字段。前端 Zod schema 必须按**必填** `boolean` 声明，不能 `.optional()`（返修两条端点的宽 VO `InspectionBatchListItemOut` 同形）。语义与 Rust 侧 `src/modules/part/vo/part.rs` 一致 |
| `location` | string | yes | `OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF / OUTSOURCE_COMPANY` |
| `current_holder_id` | string | yes | 当前持有者 ID |
| `current_holder_display` | string | yes | 当前持有者解析名（货架 code / 工人姓名 / 外协公司名） |
| `current_process_step_id` | string | yes | 当前工艺链步骤 ID |
| `next_process_id` | string | yes | 下一工艺链步骤的 process_id（DTO 兼容保留，2026-09-16 PR-3 之后与 `current_process_step_id` 同源） |
| `next_process_name` | string | yes | 下一工序名称 |
| `delivery_note_id` | string | yes | 关联送货单 ID |
| `delivery_note_no` | string | yes | 关联送货单号 |
| `parent_batch_id` | string | yes | 父批次 ID（拆分场景） |
| `created_at` | string | no | ISO 8601 timestamp |
| `updated_at` | string | no | ISO 8601 timestamp |
| `version` | number | no | 乐观锁版本号 |

> **2026-09-30 Phase 2 更新**：相比此前 11 字段版本，新增 `part_id` / `batch_label` / `current_holder_display`（重命名自 `holder_name`） / `current_process_step_id` / `next_process_name` / `delivery_note_no` / `created_at` / `updated_at`。前端 dashboard PartPreviewDialog Zod schema 已对齐 18 字段。
>
> **2026-10-02 订正**：上段是 **2026-09-30 的历史记录**（当时 11 → 18 属实，不改）。
> 2026-10-01 review 第 1 轮 M5 之后本 VO 实际为 **19 字段** —— 新增
> `is_repairing`（**BREAKING**，直读 `t_part_batch.is_repairing` 标记列，非
> `Option` ⇒ 恒定返回）。`REPAIRING` 已从 `PartStatus` 降级为标记列，起修时
> `status` 保持 `IN_PROCESS`；且标记与 `status` 正交（起修后送检可得
> `INSPECTION` + `is_repairing = true`）⇒ 前端判「返修中」**只能**读
> `is_repairing`，靠 `status` 区分不出来。本文件此前（2026-09-30 起）**从未记录过
> 该字段**，属文档漂移，本次补齐。背景与端点影响面见
> [`./lifecycle.md` § GET /api/v2/prod/batches/repairing](./lifecycle.md#get-apiv2prodbatchesrepairing)；
> 返修两条端点（`GET /prod/batches/repair` / `repairing`）用的宽 VO
> （`InspectionBatchListItemOut`）字段表见
> [`./lifecycle.md`](./lifecycle.md#get-apiv2prodbatchesrepair)；
> `GET /prod/batches/inspection` 已于 2026-10-03 换成精简 VO
> （`InspectionQueueItemOut`，13 字段），见
> [`./inspection.md`](./inspection.md#get-apiv2prodbatchesinspection)。

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

### `POST /api/v2/prod/batches/{batch_id}/split`

权限: **Manager / Inspector**

> P3 split：对 `t_part_batch` 拆批操作（一个批次 → 两个批次）。
> 用于：返工拆批 / 部分检验拆批 / 多工人合作拆批。`version` 锚
> `t_part_batch.version`。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | 被拆批次的雪花 ID（`t_part_batch.id`，全局唯一；**2026-10-02 起由路径锚定**） |

Request：`SplitBatchRequest`（body 必填）

```json
{
  "version": 0,               // 必填；batch.version（OCC）
  "quantity": 1,              // 必填；新批次数量（>0 且 < source.batch.quantity）
  "note": "string (可选)"
}
```

> **2026-10-02 BREAKING**：`batch_id` 从 body 删除（成为路径参数）。
> 被拆批次的 `part_id` 由 service 按 `batch_id` 反查得到。

业务流转：source.quantity -= split_quantity → new_batch.quantity = split_quantity
两个新批次共享原状态（如 QUERY_TO_SHIP → 拆后两个批次都仍 QUERY_TO_SHIP）。

Response 200 `data`：`{ source_batch: BatchOut, new_batch: BatchOut, event: PartEventOut }`。

错误码：20101 / 20109（批次不存在 / 已软删 / 状态不是流转起点）/ 20111（`quantity ≤ 0`）/ 40901。

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