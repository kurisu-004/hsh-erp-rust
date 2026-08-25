# part 域 API

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：CRUD（list / detail / create / batch-create / update / soft-delete）、
> by-serial 查询、upload-drawing（multipart PDF）、lifecycle 状态机
> （deliver / cancel / complete / start-repair）、inspection 流（pass / scan / fail）。
> 所有路径前缀 `/api/v2`。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/parts` | Manager / Clerk / Inspector / CncProgrammer | 列表查询 + 分页 + 多字段过滤 |
| POST | `/api/v2/parts` | Manager / Clerk | 单件创建工单（status=PENDING） |
| POST | `/api/v2/parts/batch` | Manager / Clerk | 批量创建（共享 customer_id；N≤200；per-item savepoint） |
| GET | `/api/v2/parts/{part_id}` | Manager / Clerk / Inspector / CncProgrammer | 工单详情（含 customer_name / current_batch_id 冗余） |
| GET | `/api/v2/parts/by-serial/{serial_no}` | Manager / Clerk / Inspector / CncProgrammer | 通过序列号查详情 |
| POST | `/api/v2/parts/{part_id}/update` | Manager / Clerk | 字段可选 UPDATE（OCC + 软删守卫） |
| POST | `/api/v2/parts/{part_id}/soft-delete` | **Manager** | 软删（OCC + 终态禁 + delivery_note 锁禁） |
| POST | `/api/v2/parts/{part_id}/upload-drawing` | Manager / Clerk | Multipart PDF 上传到 COS + 落 `t_part_file` |
| POST | `/api/v2/parts/{part_id}/deliver` | Manager / Clerk | READY_TO_SHIP → DELIVERED |
| POST | `/api/v2/parts/{part_id}/cancel` | Manager / Clerk | 5 状态白名单 → CANCELLED（拒 delivery_note 锁） |
| POST | `/api/v2/parts/{part_id}/complete` | Manager / Clerk | DELIVERED → COMPLETED（清空 serial_no） |
| POST | `/api/v2/parts/{part_id}/start-repair` | Manager / Clerk / Inspector | IN_PROCESS → REPAIRING |
| POST | `/api/v2/parts/batch-pass-inspection` | Manager / Inspector | 批量通过品检（INSPECTION → READY_TO_SHIP），per-item 独立处理 |
| POST | `/api/v2/parts/{part_id}/pass-inspection` | Manager / Inspector | 单件通过品检（INSPECTION → READY_TO_SHIP），payload 可空 |
| POST | `/api/v2/parts/batch-scan-inspect` | Manager / Inspector | 批量一键送检（共享品检架 + per-item decision，N≤200） |
| POST | `/api/v2/parts/{part_id}/scan-inspect` | Manager / Inspector | 单件一键送检（PENDING/PROGRAMMING/IN_PROCESS → INSPECTION → PASS/FAIL） |
| POST | `/api/v2/parts/{part_id}/fail-inspection` | Manager / Inspector | 单件品检打回（INSPECTION → IN_PROCESS，依赖 shelf+next_process） |
| POST | `/api/v2/parts/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检（SHELF_ACCOUNT @ 该 shelf）；成功后同事务触发 worker-pool refill |

> 路由顺序：`/batch-pass-inspection` 与 `/batch-scan-inspect` 必须在 `/{part_id}/...` 之前注册（axum 防止把静态段解析成 `part_id`）；`/batch`、`/by-serial/{serial_no}`、`/worker-scan` 同理。

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
- 20120 — 终态 DELIVERED/COMPLETED 禁删（HTTP 409）
- 40300 — 非 Manager

### `POST /api/v2/parts/{part_id}/upload-drawing`

权限: **Manager / Clerk**（**RBAC 在 handler 第一步守卫**，避免未授权请求触发 50 MB 内存分配）

Multipart 严格校验：

- 必须恰好含一个 `file` 字段；缺字段 / 多 `file` / 未知字段名一律 40001
- `file.content_type` 必须为 `application/pdf`（不默认可选 MIME，缺失 → 21102）
- `file` ≤ 50 MB → 21103

Response 200 `data`：最新 `TPartFile` 行（含 `content_type` / `file_size` / `content_sha256`）。

错误码：40001（multipart 字段错）、40300（角色不符）、21102（MIME 错）、21103（size 错）、21104（COS 失败）、21105（part 不存在）、21108（同 part+kind+sha256 撞唯一索引）。

### `POST /api/v2/parts/{part_id}/deliver`

权限: **Manager / Clerk**

Request：`{ "note"?: string }`（可空）

Response 200 `data`：[`PartOut`](#partout-字段) — 流转后工单。同步翻转最近一条 `READY_TO_SHIP` 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20114 — part 已 CANCELLED
- 20116 — 当前状态非 READY_TO_SHIP（状态机白名单拒绝）
- 40901 — 乐观锁失败（part 或 batch）

### `POST /api/v2/parts/{part_id}/cancel`

权限: **Manager / Clerk**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](#partout-字段)。同步翻转最近一条 source-status 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20103 — 当前状态不在 cancel 白名单（COMPLETED / REPAIRING / OUTSOURCE 等）
- 20104 — status 字符串非法
- 20114 — part 已 CANCELLED
- 21420 — part 已挂送货单，禁 cancel
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/complete`

权限: **Manager / Clerk**

Request：`{ "note"?: string }`（可空）

Response 200 `data`：[`PartOut`](#partout-字段)。**`t_part.serial_no` 被清空**（序列号已转交送货单）。同步翻转最近一条 DELIVERED 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20114 — part 已 CANCELLED
- 20115 — 当前状态非 DELIVERED（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/start-repair`

权限: **Manager / Clerk / Inspector**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](#partout-字段)。`t_part.has_been_repaired` 置 `true`；同步翻转最近一条 IN_PROCESS 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20114 — part 已 CANCELLED
- 20117 — 当前状态非 IN_PROCESS（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/batch-pass-inspection`

权限: **Manager / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [BatchPassItem](#batchpassitem-字段) | ✓ | 1..=200 个；空数组 / 超出上限 → `40001` |
| `items[].part_id` | string (i64) | ✓ | 工单雪花 ID（字符串避免 JS 精度截断） |
| `items[].batch_id` | string (i64)? | — | 指定批次；当 part 下存在多个 INSPECTION 批次时用于消歧，缺省按 part_id 唯一匹配 |
| `items[].quantity` | i32? | — | 本次送检数量；当前仅支持整批送检，`quantity ≤ 0` 或 `quantity > 批次剩余量` → `20111` |

Response 200 `data`：`BatchPassInspectionOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `passed` | [PartOut](#partout-字段) | 成功送检的件；与 `items` 顺序一一对应（`passed[i]` 对应 `items[i]`） |
| `failed` | [BatchPassFailure](#batchpassfailure-字段) | 失败的 item；单 item 不会同时出现在 `passed` 与 `failed` |

> 整体响应**始终为 200**。item 级别的失败通过 `data.failed[]` 体现（每个 item 含 `code` + `message`，调用方可按 `code` 分支处理）。
> `20111` 仅在 item-level 报出，不影响整批响应状态。

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901

### `POST /api/v2/parts/{part_id}/pass-inspection`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：可选 body `PassInspectionRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 缺省按 part_id 唯一匹配；多 INSPECTION 批次时必填 |
| `quantity` | i32? | — | 整批送检；`quantity ≤ 0` 或 `quantity > 批次剩余量` → `20111` |

> 当 body 完全省略（`Content-Length: 0`）时，按空对象处理。

Response 200 `data`：[`PartOut`](#partout-字段) — 流转后的工单最新投影

错误码：

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0` 或超过批次剩余量
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape 错误
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

### `POST /api/v2/parts/{part_id}/scan-inspect`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：`ScanInspectRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 品检架；service 校验 `zone='INSPECTION'` 且 `is_active=true`（违反 → `20511` / `20512`） |
| `decision` | string | ✓ | `"PASS"` / `"FAIL"`（`ScanDecision` 枚举） |
| `shelf_id` | string (i64)? | — | **仅 `decision=FAIL` 必填**；目标生产货架（`zone='PRODUCTION'` 且 `is_active=true`） |
| `next_process_id` | string (i64)? | — | **仅 `decision=FAIL` 必填**；下一道工序 id（须与 `shelf_id` 映射 → 违反 → `20507`） |
| `note` | string? | — | 品检备注；`≤ 500` 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时 caller 显式指定以消除歧义；缺省按状态唯一匹配 `{PENDING, PROGRAMMING, IN_PROCESS}` 批次（多批 → `20109`） |
| `quantity` | i32? | — | 本次送检数量；缺省 = 整批 |

业务流转：

- 起始状态：`PENDING` / `PROGRAMMING` / `IN_PROCESS`（`IN_PROCESS` 须 `location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id`，service 组合校验）
- **PASS**：`part` + `part_batch` → `INSPECTION` → `READY_TO_SHIP`（commit 前一次性走完，事件日志 `INSPECTED`）
- **FAIL**：`part` + `part_batch` → `INSPECTION` → `IN_PROCESS`（须填齐 `shelf_id` + `next_process_id`，事件日志 `INSPECTION_FAILED`）

WS 广播（commit 后下发）：

- `INSPECTED` —— payload `{ part_id, shelf_code: "scan-inspect" }`（详见 [`./websocket.md`](./websocket.md)）

Response 200 `data`：[`PartOut`](#partout-字段) — 流转后的工单最新投影

错误码：

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — 当前 status 不在 `{PENDING, PROGRAMMING, IN_PROCESS}` 白名单（状态机迁移失败）
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉；或缺省匹配下 `{PENDING, PROGRAMMING, IN_PROCESS}` 多于一个
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `decision=FAIL` 时 `shelf_id` 未映射 `next_process_id`
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失（如 `decision=FAIL` 时缺 `shelf_id` / `next_process_id`）
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

### `POST /api/v2/parts/batch-scan-inspect`

权限: **Manager / Inspector**

Request：`BatchScanInspectRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 批量共享品检架；service 校验 `zone='INSPECTION'` 且 `is_active=true`（违反 → 整批失败 `20511` / `20512`） |
| `items` | [BatchScanInspectItem](#batchscaninspectitem-字段) | ✓ | 1..=200 个；空数组 / 超出上限 → `40001` |
| `items[].part_id` | string (i64) | ✓ | 工单雪花 ID |
| `items[].decision` | string? | — | `"PASS"` / `"FAIL"`；缺省 = `"PASS"`（高频场景：整组送检全 PASS） |
| `items[].shelf_id` | string (i64)? | — | **仅 `decision=FAIL` 必填**；目标生产货架 |
| `items[].next_process_id` | string (i64)? | — | **仅 `decision=FAIL` 必填**；下一道工序 id |
| `items[].note` | string? | — | 品检备注；`≤ 500` 字符 |
| `items[].batch_id` | string (i64)? | — | 多批次歧义时显式指定 |
| `items[].quantity` | i32? | — | 本次送检数量；缺省 = 整批 |

业务流转：

- `target_inspection_shelf_id` 共享：批量内所有 item 走同一品检架
- per-item `decision`：缺省 `PASS`；`FAIL` 路径独立走 `fail_inspection_core`（须填齐 `shelf_id` + `next_process_id`）

Response 200 `data`：`BatchScanInspectOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | [PartOut](#partout-字段) | 成功并完成 PASS/FAIL 流转的件；与 `items` 顺序一一对应（`submitted[i]` 对应 `items[i]`） |
| `failed` | [BatchScanInspectFailure](#batchscaninspectfailure-字段) | 失败的 item；单 item 不会同时出现在 `submitted` 与 `failed` |

> 整体响应**始终为 200**。item 级别失败通过 `data.failed[]` 体现（每个 item 含 `code` + `message`）。`target_inspection_shelf_id` 不属于 INSPECTION 区 / 不 active 这种**外层校验错误**会立即终止整批并以顶层错误码返回（`20511` / `20512`）。

WS 广播（commit 后下发）：

- `BATCH_INSPECTED` —— payload `{ submitted: <count>, failed: <count> }`（仅计数，不含数组）

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- 外层校验错误（顶层）：20511 / 20512
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 20507 / 40901

### `POST /api/v2/parts/{part_id}/fail-inspection`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：`FailInspectionRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（`zone='PRODUCTION'` 且 `is_active=true`） |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id（须与 `shelf_id` 映射 → 违反 → `20507`） |
| `note` | string? | — | 品检备注；`≤ 500` 字符 |
| `batch_id` | string (i64)? | — | 多 INSPECTION 批次歧义时 caller 显式指定以消除歧义；缺省按状态唯一匹配 |
| `quantity` | i32? | — | 本次打回数量；缺省 = 整批；`quantity ≤ 0` 或超过批次剩余量 → `20111` |

业务流转：

- 起始状态：`INSPECTION`
- 终止状态：`IN_PROCESS`（`location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id`）
- 事件日志：`event_type='INSPECTION_FAILED'`

WS 广播（commit 后下发）：

- `INSPECTION_FAILED` —— payload `{ part_id }`

Response 200 `data`：[`PartOut`](#partout-字段) — 流转后的工单最新投影

错误码：

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0` 或超过批次剩余量
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` 未映射 `next_process_id`
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

### `POST /api/v2/parts/worker-scan`

权限: **Manager** / **ShelfAccount**（**scope 校验**：`shelf_id` 与 `target_inspection_shelf_id` 必须在 `current.shelf_ids` 内或 `current.shelf_wildcard=true`；否则 `40301 SHELF_MISMATCH`）

Request：`WorkerScanRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | ✓ | 扫码得到的序列号（service 反查 part） |
| `badge_code` | string | ✓ | 工人 badge_code（service 反查 worker） |
| `event_type` | string | ✓ | `"RETURNED"` / `"INSPECTED"`（`WorkerScanEvent` 枚举） |
| `shelf_id` | string (i64) | ✓ | RETURNED 时是 worker-scan 货架（PRODUCTION 区）；INSPECTED 时是工人触发扫码的货架（可为任意 INSPECTION 区货架） |
| `next_process_id` | string (i64)? | — | **仅 RETURNED 必填**；缺 / 非法 → `40001` |
| `target_inspection_shelf_id` | string (i64)? | — | **仅 INSPECTED 必填**；缺 / 非法 → `40001`；service 校验 `zone='INSPECTION'` 且 `is_active=true` |
| `batch_id` | string (i64)? | — | 多批次歧义时 caller 显式指定以消除歧义 |

业务流转：

- **RETURNED**：worker 把 IN_PROCESS+WORKER 批次放回生产架
  - `shelf_id` 必须映射 `next_process_id`（service 校验）→ 不匹配 `20507 BIZ_SHELF_PROCESS_NOT_MAPPED`
  - `part_batch` 与 `part` 状态切回 IN_PROCESS+PRODUCTION_SHELF+holder=shelf（OCC）
  - 写 `RETURNED_BY_WORKER` 事件日志
- **INSPECTED**：worker 把持有件直接送检
  - `target_inspection_shelf_id` 必须属于 INSPECTION 区且 active
  - 不符合 → `20511 BIZ_SHELF_NOT_INSPECTION_ZONE` / `20512 BIZ_SHELF_INACTIVE`
  - part 流转到 INSPECTION 状态
- **任一成功后**同事务调用 `WorkerPoolService::refill_for_worker`：
  - 工人当前工种可加工工序池有候选 → 自动抢满 `work_type.max_held_batches`（或池空为止）
  - 池空 → 业务侧返回 `data.refill.pool_empty=true`，不报错
  - refill 失败（如工种未映射工序）→ 业务错（如 `20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING`），事务回滚 scan 写入

> **OM-6 决议**：scan 与 refill 必须**同事务**——若 scan 成功后再调 refill，期间并发 worker 可能把同一批抢走，破坏「放回 → 抢下一批」原子语义。当前实现已合并到单事务。

Response 200 `data`：`WorkerScanOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `scan` | [`WorkerScanCoreOut`](#workerscancoreout-字段) | 扫码事件最小投影 |
| `refill` | [`RefillResult`](./worker-pool.md#refillresult-字段) | 同事务 refill 结果 |

错误码：

- 20101 BIZ_PART_NOT_FOUND — `serial_no` 无法解析为 part
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
- 20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER — `(worker, batch)` 不在 IN_PROCESS+WORKER 持有中（worker 不是该批次当前持有人）
- 20201 BIZ_WORKER_NOT_FOUND — `badge_code` 无法解析为 worker
- 20202 BIZ_WORKER_INACTIVE — worker 已停用
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL
- 20901 BIZ_WORK_TYPE_NOT_FOUND — worker.work_type_id 指向不存在的工种（防御性）
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — work_type.max_held_batches IS NULL
- 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING — work_type 未映射工序（refill 触发）
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` 未映射 `next_process_id`（RETURNED 路径）
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`（INSPECTED 路径）
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`（INSPECTED 路径）
- 40301 SHELF_MISMATCH — `shelf_id` / `target_inspection_shelf_id` 不在 `current.shelf_ids` 内且非 wildcard
- 40300 FORBIDDEN — 非 Manager / 非 ShelfAccount
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 `event_type`）—— payload = `WorkerScanCoreOut`
- 若 `refill.taken.len() > 0` → `WORKER_POOL_REFILL_DONE` —— payload = `RefillResult`
- 若 `refill.pool_empty=true` 且 `taken` 为空 → `WORKER_POOL_EMPTY` —— payload `{ worker_id, shelf_id }`

详见 [`./websocket.md`](./websocket.md) 与 [`./worker-pool.md`](./worker-pool.md)。

---

## 共享 DTO

### PartOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（`serialize_i64`） |
| `serial_no` | string? | 序列号 |
| `name` | string | |
| `drawing_no` | string | 图号 |
| `status` | string | part 状态枚举字符串（`INSPECTION` / `READY_TO_SHIP` 等） |
| `version` | i32 | 乐观锁 |
| `quantity` | i32 | |
| `order_no` | string? | |
| `actual_delivery_date` | date? | 实际交付日 |
| `updated_at` | naive datetime | |
| `updated_by` | string (i64)? | |

### PartListItem 字段

`TPart` 完整 28 列 + `customer_name` / `l1_customer_name` 冗余字段；见 [`./auth.md`](./auth.md) 关于 i64 字段序列化为 string 的约定。

### PartListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartListItem](#partlistitem-字段)[] | |
| `total` | string (i64) | 满足过滤的总数 |
| `limit` | string (i64) | 实际生效 |
| `offset` | string (i64) | 实际生效 |

### PartDetailOut 字段

`TPart` 完整 28 列 + `customer_name` / `l1_customer_name` / `current_batch_id`（仅 INSPECTION 时非 None）。

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

### DeliverRequest / CancelRequest / CompleteRequest / StartRepairRequest 字段

均仅含可选 `note` / `reason`（≤ 500 字符建议）；事件日志透传 `note`，cancel 与 start-repair 优先取 `reason` 作为事件 note。

### PassInspectionRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 见端点小节 |
| `quantity` | i32? | — | 见端点小节 |

> 整个 body 可省略，等价于全部字段全部 `None`。

### BatchPassItem 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `batch_id` | string (i64)? | — | 同 `PassInspectionRequest.batch_id` |
| `quantity` | i32? | — | 同 `PassInspectionRequest.quantity` |

### BatchPassInspectionRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [BatchPassItem](#batchpassitem-字段) | ✓ | 1..=`BATCH_PASS_INSPECTION_MAX_ITEMS`（200） |

### BatchPassFailure 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 失败的工单 ID |
| `code` | i32 | item-level 错误码（参见 endpoint `错误码` 节） |
| `message` | string | 失败原因（中文） |

### BatchPassInspectionOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `passed` | [PartOut](#partout-字段) | 成功送检的件 |
| `failed` | [BatchPassFailure](#batchpassfailure-字段) | 失败的件（`passed` ∩ `failed` = ∅） |

### ScanInspectRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 见 `scan-inspect` 端点 |
| `decision` | string | ✓ | `"PASS"` / `"FAIL"`（`ScanDecision` 枚举） |
| `shelf_id` | string (i64)? | — | 仅 FAIL 必填 |
| `next_process_id` | string (i64)? | — | 仅 FAIL 必填 |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |

### BatchScanInspectItem 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `decision` | string? | — | 缺省 = `"PASS"` |
| `shelf_id` | string (i64)? | — | 仅 FAIL 必填 |
| `next_process_id` | string (i64)? | — | 仅 FAIL 必填 |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |

### BatchScanInspectRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `target_inspection_shelf_id` | string (i64) | ✓ | 批量共享品检架 |
| `items` | [BatchScanInspectItem](#batchscaninspectitem-字段) | ✓ | 1..=`BATCH_SCAN_INSPECT_MAX_ITEMS`（200） |

### BatchScanInspectFailure 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 失败的工单 ID |
| `code` | i32 | item-level 错误码（参见 `batch-scan-inspect` 端点 `错误码` 节） |
| `message` | string | 失败原因（中文） |

### BatchScanInspectOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `submitted` | [PartOut](#partout-字段) | 成功流转的件 |
| `failed` | [BatchScanInspectFailure](#batchscaninspectfailure-字段) | 失败的件（`submitted` ∩ `failed` = ∅） |

### FailInspectionRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string (i64) | ✓ | 目标生产货架（PRODUCTION 区 active） |
| `next_process_id` | string (i64) | ✓ | 下一道工序 id |
| `note` | string? | — | ≤ 500 字符 |
| `batch_id` | string (i64)? | — | 多批次歧义时必填 |
| `quantity` | i32? | — | 缺省 = 整批 |

### Phase F2（scan-inspect）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScanDecision { PASS, FAIL }

pub struct ScanInspectRequest {
    pub target_inspection_shelf_id: String,  // 雪花 ID（i64 字符串）
    pub decision: ScanDecision,
    pub shelf_id: Option<String>,            // FAIL 必填
    pub next_process_id: Option<String>,     // FAIL 必填
    pub note: Option<String>,                // ≤ 500 字符
    pub batch_id: Option<String>,            // 多批次歧义时必填
    pub quantity: Option<i32>,               // 缺省=整批
}

pub struct BatchScanInspectItem {
    pub part_id: i64,                        // 雪花 ID
    pub decision: Option<ScanDecision>,      // 缺省=PASS
    pub shelf_id: Option<String>,
    pub next_process_id: Option<String>,
    pub note: Option<String>,
    pub batch_id: Option<String>,
    pub quantity: Option<i32>,
}

pub struct BatchScanInspectRequest {
    pub target_inspection_shelf_id: String,
    pub items: Vec<BatchScanInspectItem>,    // 1..=200
}

pub struct BatchScanInspectFailure {
    pub part_id: i64,
    pub code: i32,                           // 业务错误码
    pub message: String,                     // 错误 message
}

pub struct BatchScanInspectOut {
    pub submitted: Vec<PartOut>,
    pub failed: Vec<BatchScanInspectFailure>,
}

pub struct FailInspectionRequest {
    pub shelf_id: String,                    // 必填（PRODUCTION 区 active）
    pub next_process_id: String,             // 必填
    pub note: Option<String>,
    pub batch_id: Option<String>,
    pub quantity: Option<i32>,
}
```

### Phase W（worker-scan + worker-pool 联动）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent { RETURNED, INSPECTED }

pub struct WorkerScanRequest {
    pub serial_no: String,                   // 扫码：序列号
    pub badge_code: String,                  // 扫码：工人 badge_code
    pub event_type: WorkerScanEvent,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,                       // worker-scan 货架
    #[serde(default)]
    pub next_process_id: Option<String>,     // 仅 RETURNED 必填
    #[serde(default)]
    pub target_inspection_shelf_id: Option<String>,  // 仅 INSPECTED 必填
    #[serde(default)]
    pub batch_id: Option<String>,
}

pub struct WorkerScanCoreOut {
    pub worker_id: i64,
    pub part_id: i64,
    pub batch_id: i64,
    pub event_type: String,                  // "RETURNED" / "INSPECTED"
}

// WorkerScanOut = { scan: WorkerScanCoreOut, refill: RefillResult }
// （RefillResult 定义在 worker_pool/model.rs，详见 ./worker-pool.md）
```

---

## 端点约束（与 Python 一致）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → `40901 VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → `20101`
- **状态机**：详见 [状态机（can_transition_to 白名单）](#状态机can_transition_to-白名单)；不在白名单内的 source / target 组合返回 `20103 BIZ_INVALID_TRANSITION`（迁移表见 `src/modules/part/statemachine.rs`）
- **事件日志**：状态迁移在 service 内事务内统一插入对应事件，service 提交后由 WS 中枢广播
- **part↔batch 同步**：lifecycle 终态 / 翻转（deliver / cancel / complete / start-repair）同事务内除翻 `t_part` 外还需翻最近一条 source-status 批次（`PartRepo::find_most_recent_batch_for_part`），保证候选批次不被 stale 状态污染

## 状态机（can_transition_to 白名单）

| from | to | 触发场景 |
|---|---|---|
| INSPECTION | READY_TO_SHIP | `pass_inspection` 单/批；`scan-inspect` PASS 分支 |
| PROGRAMMING | INSPECTION | `scan-inspect`（PROGRAMMING 工件） |
| PENDING | INSPECTION | `scan-inspect`（待下发工单） |
| IN_PROCESS | INSPECTION | `scan-inspect`（生产架工件，**必须 IN_PROCESS+PRODUCTION_SHELF**；service 层组合校验） |
| READY_TO_SHIP | DELIVERED | `deliver`（同事务翻最近一条 source-status 批次） |
| DELIVERED | COMPLETED | `complete`（同事务；清空 `serial_no`） |
| IN_PROCESS | REPAIRING | `start-repair`（同事务翻最近一条 IN_PROCESS 批次；置 `has_been_repaired=true`） |
| PENDING / PROGRAMMING / INSPECTION / READY_TO_SHIP / DELIVERED | CANCELLED | `cancel`（同事务翻最近一条 source-status 批次；delivery_note 锁禁） |

INSPECTION → IN_PROCESS 由 `fail_inspection`（推荐需求 3）走 service 流程：

- INSPECTION 状态 + `location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id=...`
- 事件日志：`event_type='INSPECTION_FAILED'`

## 错误码参考（part / lifecycle）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20101 | BIZ_PART_NOT_FOUND | 404 | 工单不存在 / 已软删 |
| 20103 | BIZ_INVALID_TRANSITION | 400 | 状态机白名单拒绝（cancel 时 COMPLETED/REPAIRING 等） |
| 20104 | BIZ_INVALID_VALUE | 400 | DB status 字符串不在 enum 白名单 |
| 20109 | BIZ_PART_BATCH_NOT_FOUND | 404 | inspection 流找不到 INSPECTION 批次 / 多批次歧义 |
| 20111 | BIZ_PART_BATCH_INVALID_QUANTITY | 400 | quantity ≤ 0 或超过批次剩余量 |
| 20114 | BIZ_PART_ALREADY_CANCELLED | 409 | cancel/deliver/complete/start-repair 遇到 CANCELLED 状态 |
| 20115 | BIZ_PART_NOT_DELIVERED | 400 | complete 要求 DELIVERED |
| 20116 | BIZ_PART_NOT_READY_TO_SHIP | 400 | deliver 要求 READY_TO_SHIP |
| 20117 | BIZ_PART_REPAIR_NOT_TRIGGERED | 400 | start-repair 要求 IN_PROCESS |
| 20120 | BIZ_PART_NOT_DELETABLE | 409 | soft-delete 终态禁 |
| 21420 | BIZ_DELIVERY_NOTE_LOCKED_PART | 409 | cancel / soft-delete 遇 part 已挂送货单 |
| 40001 | VALIDATION_ERROR | 422 | 入参 shape 错 / multipart 字段错 |
| 40300 | FORBIDDEN | 403 | 角色不符 |

### 货架错误码（20511 / 20512 — scan-inspect / fail-inspection 专用）

| code | 名称 | 触发场景 |
|---|---|---|
| 20511 | BIZ_SHELF_NOT_INSPECTION_ZONE | `target_inspection_shelf.zone ≠ 'INSPECTION'` |
| 20512 | BIZ_SHELF_INACTIVE | `target_inspection_shelf.is_active = false` |

## 参考

- 集成测试：`tests/part_api.rs`（inspection 流全链路）+ `tests/part_crud.rs`（CRUD + lifecycle 27 用例）
- 仓库分层：`src/modules/part/handler.rs` (axum) → `service/{crud,inspection,lifecycle}.rs` (业务) → `repo/{part,batch,event}.rs` (SQL)
- 状态机：`src/modules/part/statemachine.rs`
- 错误码：`src/shared/error.rs::code`（20101 / 20103 / 20104 / 20109 / 20111 / 20114 / 20115 / 20116 / 20117 / 20118 / 20119 / 21420 / 40001 / 40300 / 40901）
- worker-scan 联动：详见 [`./worker-pool.md`](./worker-pool.md)
