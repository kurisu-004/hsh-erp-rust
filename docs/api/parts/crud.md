# part 域 — CRUD

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / PartListItem / PartListOut / PartDetailOut / 端点约束）见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 9 个 CRUD 端点（list / get / by-serial / create / batch / update / soft-delete / upload-drawing / upload-3d-model）。lifecycle / inspection 见 [`./lifecycle.md`](./lifecycle.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录


- [GET /api/v2/parts](#get-apiv2parts)
- [POST /api/v2/parts](#post-apiv2parts)
- [POST /api/v2/parts/batch](#post-apiv2partsbatch)
- [GET /api/v2/parts/{part_id}](#get-apiv2partspart_id)
- [GET /api/v2/parts/by-serial/{serial_no}](#get-apiv2partsby-serialserial_no)
- [POST /api/v2/parts/{part_id}/update](#post-apiv2partspart_idupdate)
- [POST /api/v2/parts/{part_id}/soft-delete](#post-apiv2partspart_idsoft-delete)
- [POST /api/v2/parts/{part_id}/upload-drawing](#post-apiv2partspart_idupload-drawing)
- [POST /api/v2/parts/{part_id}/upload-3d-model](#post-apiv2partspart_idupload-3d-model)（2026-09-11 新增）

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
| `locations` | string? | 2026-09-17 新增。位置白名单，逗号分隔（`OFFICE` / `PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OUTSOURCE_COMPANY`），查 `t_part_batch.location`（多态批次的 `location` 字段） |
| `holder_ids` | string? | 2026-09-17 新增。持有人 ID 列表，逗号分隔雪花字符串（多态：t_shelf / t_worker / t_outsource_company 任一表匹配同雪花 id 即命中）；查 `t_part_batch.current_holder_id`。非法雪花 ID → `40001 VALIDATION_ERROR`（422） |
| `row_type` | string? | 2026-09-28 新增。行类型筛选：`"PART"` / `"ASSEMBLY"` / 缺省（=ALL）。非法值 → `40001 VALIDATION_ERROR`。详见下方「行类型合并规则」。 |
| `include_assemblies` | bool? | 2026-09-28 新增。是否合并装配件：仅 `row_type` 缺省时生效。`false` 强制仅零件（兼容 `/parts/pending-programming` 等内部 caller）。`true` 或缺省 → 默认 ALL 模式。详见下方「行类型合并规则」。 |
| `sort_by` | string? | 白名单 `CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它退化为 `CREATED_AT`。ALL 模式下 `SERIAL_NO` 不在 t_part / t_assembly 共有列交集 → 降级为 `CREATED_AT`（见下方「SORT 键交互」）。 |

> **2026-09-29 新增字段**（CNC 重构 5 任务之一）：响应 [`PartListItem`](./index.md#partlistitem-字段)
> 含 `has_cnc_program: bool` 派生字段。`GET /parts/pending-programming` 通过
> repo EXISTS 子查询填充真实值（已上传 G_CODE → true）；其它 list 端点默认
> `false`（service 层不在那里 enrich，避免 N+1）。详见
> [`./lifecycle.md#get-apiv2partspending-programming`](./lifecycle.md#get-apiv2partspending-programming)。
| `sort_dir` | string? | `ASC` / `DESC`（缺省 `DESC`） |
| `limit` | int? | 1..=200（缺省 50） |
| `offset` | int? | ≥ 0（缺省 0） |

Response 200 `data`：`PartListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartListItem](./index.md#partlistitem-字段)[] | 含 TPart 完整列（不含 `next_process_id`）+ `customer_name` / `l1_customer_name` 冗余 |
| `total` | int | 满足过滤的总数（与 `items` 解耦，便于前端独立显示） |
| `limit` | int | 实际生效的 limit |
| `offset` | int | 实际生效的 offset |

> **2026-09-27 part 域前后端字段对齐**：`total` / `limit` / `offset` 改为裸 i64
> → JSON number，对齐其它 9 域；雪花 ID 仍按 `serialize_i64` → JSON string。
> 详见 [`./index.md#partlistout-字段`](./index.md#partlistout-字段) 备注。

> 2026-09-16（migration 026 FK 翻转）：每行新增 `process_chain_id`（string i64?）——
> 逻辑指向 `t_part_process_chain.id`；`null` = 未制定工艺链。前端「工序制定」页
> 按此字段是否为 `null` 批量区分已制定 / 未制定工序的零件，点击后调
> `GET /api/v2/prod/process-chains/{chain_id}` 加载工序（见
> [`../production/process-chain.md`](../production/process-chain.md)）。

错误码：40001（limit/offset 越界）、40300（角色不符）、50001（DB）。

#### 行类型合并规则（2026-09-28 新增）

`GET /api/v2/parts` 支持三种行类型返回模式，由 `row_type` + `include_assemblies` 组合控制：

| `row_type` | `include_assemblies` | 模式 | 数据源 | `total` 语义 | `items[i].row_type` |
|---|---|---|---|---|---|
| `"PART"` | 任意 | **Part** | `t_part WHERE assembly_id IS NULL` | 仅零件计数 | `"PART"` |
| `"ASSEMBLY"` | 任意 | **Assembly** | `t_assembly`（投影为 `PartListItem`） | 仅装配件计数 | `"ASSEMBLY"` |
| 缺省 | `false` | **Part**（兼容旧 caller） | `t_part`（无 `assembly_id IS NULL` 守卫；与历史行为一致） | 整张 t_part 计数 | `"PART"` |
| 缺省 | `true` / 缺省 | **All** | `t_part`（part_only 段） UNION `t_assembly`，内存合并排序 | `parts_count + assemblies_count` | `"PART"` / `"ASSEMBLY"` 混合 |
| 其它非空字符串 | 任意 | — | — | — | 返回 `40001 VALIDATION_ERROR` |

**All 模式实现要点**：
- 两次 list：part 段 `limit = (query.limit + query.offset).min(200)`，assembly 段同 cap（最大页宽 200 与单段 list 对齐）。
- 两次 count → 相加得 `total`。
- 内存 merge sort by 统一 sort_key（t_part / t_assembly 共有列交集：`CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `DRAWING_NO` / `NAME`），二级 id DESC 保证稳定。
- 切 `[offset, offset+limit)`。

**SORT 键交互**：`SERIAL_NO` 仅 t_part 独有，t_assembly 无该列；ALL 模式下 `sort_by=SERIAL_NO` 会被降级为 `CREATED_AT`（不报错；文档标注以便前端解释）。

> 默认行为变更（2026-09-28）：不传 `row_type` / `include_assemblies` 时，`GET /parts` 默认走 **All** 模式（合并装配件）。旧 PART-only caller（如 `/parts/pending-programming` 等内部端点）需显式传 `include_assemblies=false` 才能保留原行为——本次 task 内已对 `list_pending_programming` 走 `PartListFilters.part_only=false` 兜底，对外不受影响。
>
> **2026-10-03 订正**：原文此处并列点名了 `list_outsource_in_flight` /
> `list_outsource_sendable` 两个内部 endpoint。二者已于 2026-10-03 随 part 域两条
> 外协 list 端点一起删除（`service/phase1/outsource.rs` 整文件移除），取代者迁往
> outsource 域：见 [`../lifecycle.md`](./lifecycle.md#外协两条-list-端点--2026-10-03-已下线迁往-outsource-域)。

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
| `unit_price` | decimal（**JSON 字符串**） | — | 单价，如 `"95.00"`；缺省 `0` |
| `total_price` | decimal（**JSON 字符串**） | — | 总价，如 `"950.00"`；缺省 `0` |

> 2026-10-05 新增 `unit_price` / `total_price`。**JSON 里必须是字符串**，不能写裸
> 数字 `95` —— 后端 `rust_decimal` 只开了 `serde-with-str`，裸数字会在反序列化
> 阶段直接 400。
>
> **序列号不收**：建件时按 L1 客户 `serial_prefix` 自动派发（见
> [`./index.md#序列号serial_no生命周期`](./index.md#序列号serial_no生命周期)）。
> L1 客户未配 `serial_prefix` → `20308 BIZ_CUSTOMER_NO_SERIAL_PREFIX`，**整单拒**
> （不落任何行、不消耗序列号 counter）。

Response 201 `data`：[`PartDetailOut`](./index.md#partdetailout-字段) — 含 TPart 完整列 + 客户冗余 + `current_batch_id`。

错误码：40001（字段空 / quantity≤0）、40300（角色不符）、20102（customer 不存在 / 其 L1 父行已软删，**HTTP 404**）、20308（L1 客户无 `serial_prefix`）、20108（L1 的 `serial_prefix` 未在 `t_serial_counter` 注册，见 [`./index.md`](./index.md#序列号serial_no生命周期)）。

### `POST /api/v2/parts/batch`

权限: **Manager / Clerk**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | 批量共享的二级客户 id |
| `items` | `PartBatchCreateItem`[] | ✓ | 1..=200；每件独立校验 |

`PartBatchCreateItem` 字段（2026-10-05 起与 `PartCreateRequest` 同构）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` / `drawing_no` / `applicant_name` | string | ✓ | 工单名 / 图号 / 申请人 |
| `quantity` | i32 | ✓ | > 0 |
| `request_date` / `planned_delivery_date` | date | ✓ | 客户请求日 / 计划交付日 |
| `is_urgent` | bool | — | 缺省 `false` |
| `order_no` / `note` | string? | — | 订单号 / 备注 |
| `system_delivery_date` | date? | — | 系统派工日 |
| `assembly_id` | string (i64)? | — | 父装配体 |
| `unit_price` | decimal（**JSON 字符串**） | — | 单价，如 `"95.00"`；缺省 `0` |
| `total_price` | decimal（**JSON 字符串**） | — | 总价，如 `"950.00"`；缺省 `0` |
| `drawing_file` / `model3d_file` | object? | — | 文件绑定（`tmp_key` + `content_sha256` + `original_filename` + `file_size` + `content_type` + `ext?`） |

> 金额两列同样是 **JSON 字符串**，裸数字会被反序列化拒。
>
> **序列号不收**：每件建单时按 L1 客户 `serial_prefix` 自动派发一个
> `serial_no`（`prefix` + 4 位数字，如 `P1000`），INSERT 期写入。L1 客户未配
> `serial_prefix` → `20308`，**整批拒**（在任何一行落库之前，连文件绑定都不
> head/copy）。
>
> 带 `drawing_file` / `model3d_file` 时走 COS 直传绑定路径：任一 binding
> head/copy 失败 → 整体报错回滚；DB 层仍是 per-item savepoint，单件失败只进
> `failed[]` 不影响其余件。

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `created` | [PartDetailOut](./index.md#partdetailout-字段)[] | 成功插入并读取详情的件 |
| `failed` | `PartBatchCreateFailure`[] | 单件失败明细（含 item_index）；成功与失败互斥 |

错误码：40001（items 空 / 超过 200）、40300、20102（customer 不存在 / 其 L1 父行已软删，**HTTP 404**）、20308（L1 客户无 `serial_prefix`，整批拒）、20108（L1 的 `serial_prefix` 未在 `t_serial_counter` 注册，整批拒）；item-level（`failed[].code`）：50001 / 20101 等。

### `GET /api/v2/parts/{part_id}`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Response 200 `data`：[`PartDetailOut`](./index.md#partdetailout-字段)。

> 2026-09-16（migration 026 FK 翻转）：响应含 `process_chain_id`（string i64?）——
> 逻辑指向 `t_part_process_chain.id`；`null` = 未制定工艺链。

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
| `note` | string? | — | |
| `is_urgent` | bool? | — | |
| `unit_price` | string (Decimal)? | — | 2026-09-27 新增：单价（NUMERIC(12,2) NOT NULL DEFAULT 0）。`Decimal` 由 axum extractor 从 JSON string 反序列化 |
| `total_price` | string (Decimal)? | — | 2026-09-27 新增：总价（NUMERIC(14,2) NOT NULL DEFAULT 0）。同上 |

> 2026-09-16（migration 027）：`PartUpdateRequest` 删 `actual_delivery_date`
> 入参 —— 该列已从 `t_part` 删除；实际交付日期由 `t_part_event.event_type='DELIVERED'`
> 事件派生，不接受手工改（详见
> [`../../api/statistics.md`](../../api/statistics.md)）。
>
> **2026-09-27 part 域前后端字段对齐**：增 `unit_price?` / `total_price?` 入参
> （`Option<Decimal>`）。DB 列 `t_part.unit_price` / `t_part.total_price` 为
> NUMERIC(12,2) / NUMERIC(14,2) NOT NULL DEFAULT 0，由 `rust_decimal::Decimal`
> 反序列化 JSON string → Decimal 避免 JS 浮点丢精度。OCC 语义不变。

Response 200 `data`：[`PartDetailOut`](./index.md#partdetailout-字段)。

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

**CAS key 格式（2026-09-11 变更）**：

`object_key` 字段遵循 Python `core/file_hash.py:41-78` 同款模板：

```
{COS_UPLOAD_PREFIX}part/{part_id}/{KIND}/{sha16}_{safe_filename}
```

示例：`uploads/part/12345/DRAWING/abc123def4567890_drawing.pdf`

跨语言可读：同一 part + sha256 + filename 在两个后端派生出同一 key。

### `POST /api/v2/parts/{part_id}/upload-3d-model`

权限: **Manager / Clerk**（同 `upload-drawing`）

Multipart 严格校验：

- 必须恰好含一个 `file` 字段；缺字段 / 多 `file` / 未知字段名一律 40001
- 扩展名必须在 `3D_MODEL` 白名单内（`step` / `stp` / `iges` / `igs` / `stl` / `obj` / `3mf`），否则 21102
- `file.content_type` 必须与扩展名匹配（白名单见下表）；不匹配 → 21102
- `file` ≤ 50 MB → 21103

`file_type` 字段由 service 层按扩展名推导（`policy::file_type_for_ext`）：

| 扩展名 | `file_type` | 允许的 `content_type` |
|---|---|---|
| `step` / `stp` | `STEP` | `application/step` / `application/stp` / `application/octet-stream` |
| `iges` / `igs` | `IGES` | `application/iges` / `application/igs` / `application/octet-stream` |
| `stl` | `STL` | `model/stl` / `application/sla` / `application/octet-stream` |
| `obj` | `OBJ` | `model/obj` / `application/octet-stream` |
| `3mf` | `3MF` | `application/vnd.ms-3mfdocument` / `model/3mf` / `application/octet-stream` |

Response 200 `data`：最新 `TPartFile` 行（`kind` = `3D_MODEL`，`file_type` 如上表）。

错误码：40001（multipart 字段错 / 缺扩展名）、40300（角色不符）、21102（扩展名不在白名单 / content_type 与扩展名不一致）、21103（size 错）、21104（COS 失败）、21105（part 不存在）、21108（同 part+kind+sha256 撞唯一索引）。

**CAS key 示例**：`uploads/part/12345/3D_MODEL/abc123def4567890_bracket.step`

### `POST /api/v2/parts/{part_id}/files/confirm`

直传 COS 链路的「提交绑定」端点。完整契约见 [`../files.md`](../files.md#post-apiv2partspart_idfilesconfirm)（part_file 域文档统一托管，本处只列要点）。

- **权限**：Manager / Clerk
- **用途**：客户端 PUT 到 tmp 区成功后调用本端点 → 把 tmp 对象 copy 到 CAS key + INSERT `t_part_file` + 异步清理 tmp
- **场景**：批量预签（场景 A，`/parts/batch` 入参 items 含 `drawing_file` / `model3d_file`）的并发 PUT 完成后；详情页补传（场景 B） 单文件上传完成后
- **错误码**：21102 / 21104 / 21105 / 21108 / 21114 / 21115（详见 files.md 完整表）

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
| `created` | [PartDetailOut](./index.md#partdetailout-字段)[] | 成功创建的件 |
| `failed` | `PartBatchCreateFailure`[] | 失败的件（`created` ∩ `failed` = ∅） |

### PartUpdateRequest 字段

见上文 [`POST /parts/{id}/update`](#post-apiv2partspart_idupdate) 字段表。

---

### `GET /api/v2/parts/{part_id}/events`

权限: **已登录**

> 2026-09-23 补：列出 part 全部事件日志（含状态机流转 + batch 流转 + repair + delivery note 挂接等）。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 雪花 ID |

Query：`event_type?` / `batch_id?` / `limit?` / `offset?`（默认 100 / 0）。

Response 200 `data`：`{ items: [PartEventOut], total, limit, offset }`。

`PartEventOut` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 |
| `part_id` | string (i64) | |
| `batch_id` | string (i64)? | batch 级事件有值 |
| `event_type` | string | `CREATED` / `STATUS_CHANGED` / `DELIVERED` / `COMPLETED` / `CANCELLED` / `REPAIR_STARTED` / `PICKED_UP` / `SHIPPED` 等 |
| `from_status` | string? | 状态机起点 |
| `to_status` | string? | 状态机终点 |
| `actor_user_id` | string (i64)? | 操作者 |
| `note` | string? | |
| `created_at` | naive datetime | |

### `GET /api/v2/parts/location-tree`

权限: **已登录**

> 2026-09-23 补：返回货架树（按 zone 分组：PRODUCTION / INSPECTION / RETURN），用于前端 picker。

Response 200 `data`：`[ShelfNode]`（递归 `children`）。

`ShelfNode` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `zone` | string | `PRODUCTION` / `INSPECTION` / `RETURN` |
| `zone_label` | string | 中文显示 |
| `shelves` | [Shelf] | 该 zone 下货架（按 `sort_order` 排序） |

`Shelf` 见 [`../shelves.md#dto-字段参考`](../shelves.md#dto-字段参考)。

### `POST /api/v2/parts/match-by-excel-items`

权限: **Manager / Clerk**

> 2026-09-23 补：Excel 批量匹配（POST 入参 body）。前端上传 Excel → 服务端按 `part_name` / `drawing_no` / `serial_no` 三种 key 分别匹配已有 part；返回每个 item 的匹配结果（用于人工确认导入）。

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [ExcelItem] | ✓ | Excel 解析后的 item 列表 |
| `default_match_key` | string? | — | `part_name` / `drawing_no` / `serial_no`，默认 `part_name` |

`ExcelItem` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_name` | string | |
| `drawing_no` | string? | |
| `serial_no` | string? | |
| `quantity` | i32? | |

Response 200 `data`：`{ matches: [MatchResult], unmatched: [ExcelItem] }`。

`MatchResult` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `item_index` | usize | 在原 `items[]` 中的位置 |
| `matched_part_id` | string (i64)? | null = 未匹配 |
| `match_key` | string | 实际命中的 key 类型 |
| `confidence` | string | `EXACT` / `FUZZY` / `BY_DRAWING_NO` / `BY_SERIAL` |

错误码：40001 / 20104。

---

> **2026-09-23 同步说明**：本节 3 个端点（`/{part_id}/events` / `/location-tree` / `/match-by-excel-items`）原 docs/api/parts/crud.md 未覆盖，本次按 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycle-inspectionmd高优先级--大量端点缺失)）。
