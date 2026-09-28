# part 域 API

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：CRUD / by-serial 查询 / upload-drawing / lifecycle 状态机（deliver / cancel / complete / start-repair）/ inspection 流（to-inspection / to-ship / to-process / worker-scan）/ batch split（batch-with-pdfs / `/{part_id}/batches` / `batches/split`）。所有路径前缀 `/api/v2`。
> 已拆为子目录：
>
> 导航：[**`index.md`**](./index.md) · [`crud.md`](./crud.md) · [`lifecycle.md`](./lifecycle.md) · [`inspection.md`](./inspection.md) · [`batch.md`](./batch.md)
>
> · part-batches 详情见 [inspection.md](./inspection.md#get-apiv2partsby-serialserial_nopart-batches)

---

## 端点列表

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/parts` | Manager / Clerk / Inspector / CncProgrammer | 列表查询 + 分页 + 多字段过滤 | [`crud.md`](./crud.md#get-apiv2parts) |
| POST | `/api/v2/parts` | Manager / Clerk | 单件创建工单（status=PENDING） | [`crud.md`](./crud.md#post-apiv2parts) |
| POST | `/api/v2/parts/batch` | Manager / Clerk | 批量创建（共享 customer_id；N≤200） | [`crud.md`](./crud.md#post-apiv2partsbatch) |
| GET | `/api/v2/parts/{part_id}` | Manager / Clerk / Inspector / CncProgrammer | 工单详情（含 customer_name / current_batch_id 冗余） | [`crud.md`](./crud.md#get-apiv2partspart_id) |
| GET | `/api/v2/parts/by-serial/{serial_no}` | Manager / Clerk / Inspector / CncProgrammer | 通过序列号查详情 | [`crud.md`](./crud.md#get-apiv2partsby-serialserial_no) |
| GET | `/api/v2/parts/by-serial/{serial_no}/part-batches` | Manager / Clerk / Inspector / CncProgrammer | 扫码快捷品检上下文（工单窄字段 + 全部活跃批次含 holder 名称） | [`inspection.md`](./inspection.md#get-apiv2partsby-serialserial_nopart-batches) |
| GET | `/api/v2/parts/inspection-batches` | Manager / Inspector | 待品检批次列表（status=INSPECTION；含 batch_id + version + 工单 + holder/process/delivery_note/customer 名称一次解析） | [`inspection.md`](./inspection.md#get-apiv2partsinspection-batches) |
| POST | `/api/v2/parts/{part_id}/update` | Manager / Clerk | 字段可选 UPDATE（OCC + 软删守卫） | [`crud.md`](./crud.md#post-apiv2partspart_idupdate) |
| POST | `/api/v2/parts/{part_id}/soft-delete` | **Manager** | 软删（OCC + 终态禁 + delivery_note 锁禁） | [`crud.md`](./crud.md#post-apiv2partspart_idsoft-delete) |
| POST | `/api/v2/parts/{part_id}/upload-drawing` | Manager / Clerk | Multipart PDF 上传到 COS + 落 `t_part_file`（CAS key 格式 2026-09-11 变更） | [`crud.md`](./crud.md#post-apiv2partspart_idupload-drawing) |
| POST | `/api/v2/parts/{part_id}/upload-3d-model` | Manager / Clerk | Multipart 3D 模型上传到 COS（STEP/STP/IGES/IGS/STL/OBJ/3MF）+ 落 `t_part_file`（2026-09-11 新增） | [`crud.md`](./crud.md#post-apiv2partspart_idupload-3d-model) |
| POST | `/api/v2/parts/{part_id}/deliver` | Manager / Clerk | READY_TO_SHIP → DELIVERED | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_iddeliver) |
| POST | `/api/v2/parts/{part_id}/cancel` | Manager / Clerk | 5 状态白名单 → CANCELLED（拒 delivery_note 锁） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcancel) |
| POST | `/api/v2/parts/{part_id}/complete` | Manager / Clerk | DELIVERED → COMPLETED（清空 serial_no） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcomplete) |
| POST | `/api/v2/parts/{part_id}/start-repair` | Manager / Clerk / Inspector | IN_PROCESS → REPAIRING | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idstart-repair) |
| POST | `/api/v2/parts/batch-to-inspection` | Manager / Inspector | 批量送检（PENDING/PROGRAMMING/IN_PROCESS → INSPECTION） | [`inspection.md`](./inspection.md#post-apiv2partsbatch-to-inspection) |
| POST | `/api/v2/parts/{part_id}/to-inspection` | Manager / Inspector | 单件送检 | [`inspection.md`](./inspection.md#post-apiv2partspart_idto-inspection) |
| POST | `/api/v2/parts/batch-to-ship` | Manager / Inspector | 批量通过品检（INSPECTION → READY_TO_SHIP） | [`inspection.md`](./inspection.md#post-apiv2partsbatch-to-ship) |
| POST | `/api/v2/parts/{part_id}/to-ship` | Manager / Inspector | 单件通过品检（INSPECTION → READY_TO_SHIP） | [`inspection.md`](./inspection.md#post-apiv2partspart_idto-ship) |
| POST | `/api/v2/parts/{part_id}/to-process` | Manager / Inspector | 单件指定下一工序（INSPECTION → IN_PROCESS） | [`inspection.md`](./inspection.md#post-apiv2partspart_idto-process) |
| POST | `/api/v2/parts/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检；成功后同事务触发 worker-pool refill | [`inspection.md`](./inspection.md#post-apiv2partsworker-scan) |
| GET | `/api/v2/parts/pending-programming` | Manager / Clerk / Inspector / CncProgrammer | 待编程列表（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/parts/outsource-in-flight` | Manager / Clerk / Inspector / CncProgrammer | 外协在途列表（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/parts/outsource-sendable` | Manager / Clerk / Inspector / CncProgrammer | 可发外协列表（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/parts/repair-batches` | Manager / Inspector | 维修批次列表（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/parts/repairing-batches` | Manager / Inspector | 维修中批次列表（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/parts/location-tree` | Manager / Clerk / Inspector / CncProgrammer | 库位树（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| POST | `/api/v2/parts/scan/deliver-part` | Manager / Clerk | 扫码发货（Phase 1） | [`inspection.md`](./inspection.md#post-apiv2partsscandeliver-part) |
| POST | `/api/v2/parts/match-by-excel-items` | Manager / Clerk | Excel 行匹配（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/batch-update-order-info` | Manager / Clerk | 批量更新订单信息（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/batch-with-pdfs` | Manager / Clerk | 多页 PDF 树形创建（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| GET | `/api/v2/parts/by-work-type/{work_type_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工种查 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| GET | `/api/v2/parts/pickable-by-work-type/{work_type_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工种查可领取 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| GET | `/api/v2/parts/by-worker/{worker_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工人查持有 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| POST | `/api/v2/parts/{part_id}/place-on-shelf` | Manager / Clerk | 上架（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idplace-on-shelf) |
| POST | `/api/v2/parts/{part_id}/recall-to-pending` | Manager / Clerk | 召回至 PENDING（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idrecall-to-pending) |
| POST | `/api/v2/parts/{part_id}/send-to-programming` | Manager / Clerk | 派发编程（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idsend-to-programming) |
| POST | `/api/v2/parts/{part_id}/release-from-programming` | Manager / Clerk | 编程完成释放（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idrelease-from-programming) |
| POST | `/api/v2/parts/{part_id}/recall-to-programming` | Manager / Clerk | 召回编程（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idrecall-to-programming) |
| POST | `/api/v2/parts/{part_id}/send-to-outsource` | Manager / Clerk | 派发外协（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idsend-to-outsource) |
| POST | `/api/v2/parts/{part_id}/receive-from-outsource` | Manager / Clerk | 外协回收入库（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idreceive-from-outsource) |
| POST | `/api/v2/parts/{part_id}/receive-from-outsource-to-inspection` | Manager / Clerk / Inspector | 外协回收 → 品检（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idreceive-from-outsource-to-inspection) |
| POST | `/api/v2/parts/{part_id}/complete-repair` | Manager / Clerk / Inspector | 完成维修（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcomplete-repair) |
| POST | `/api/v2/parts/{part_id}/repair-dispatch` | Manager / Clerk | 派发维修（Phase 1） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idrepair-dispatch) |
| POST | `/api/v2/parts/{part_id}/scan-inspect` | Manager / Inspector | 扫码品检（Phase 1） | [`inspection.md`](./inspection.md#post-apiv2partspart_idscan-inspect) |
| GET | `/api/v2/parts/{part_id}/events` | Manager / Clerk / Inspector / CncProgrammer | 工单事件时间线（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| GET | `/api/v2/parts/{part_id}/batches` | Manager / Clerk / Inspector / CncProgrammer | 列出 part 下所有批次（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/{part_id}/batches/split` | Manager / Clerk | 拆分批次（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/{part_id}/batches/{batch_id}/cancel` | Manager / Clerk | 取消批次（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/{part_id}/pick-up` | Manager / Clerk / ShelfAccount | B 方案手动 pick-up 兜底（Phase 2） | [`inspection.md`](./inspection.md#post-apiv2partspart_idpick-up) |

> 路由顺序：所有静态段必须在 `/{part_id}/...` catch-all 前注册。`src/modules/part/mod.rs` 当前注册顺序：
> 1. `GET /` + `POST /`
> 2. `POST /batch`
> 3. `GET /by-serial/{serial_no}` + `GET /by-serial/{serial_no}/part-batches`
> 4. `POST /batch-to-ship` + `POST /batch-to-inspection` + `GET /inspection-batches`
> 5. `POST /worker-scan`
> 6. Phase 1 静态段：`pending-programming` / `outsource-in-flight` / `outsource-sendable` / `repair-batches` / `repairing-batches` / `location-tree` / `scan/deliver-part` / `match-by-excel-items` / `batch-update-order-info` / `batch-with-pdfs`
> 7. Phase 2 静态段：`by-work-type/{work_type_id}` / `pickable-by-work-type/{work_type_id}` / `by-worker/{worker_id}`
> 8. `GET /{part_id}` + Phase 1 单件 catch-all + `POST /{part_id}/to-ship` / `to-inspection` / `to-process`
>
> axum `/:col` 占位匹配会**先吃静态段**——任何静态段如果排在 `/{part_id}` 之后都会被解析成 `part_id`，返回 404。

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
| `updated_at` | naive datetime | |
| `updated_by` | string (i64)? | |

> 2026-09-16 PR-2（migration 027）：`PartOut` 删 `actual_delivery_date` —— 由
> `t_part_event.event_type='DELIVERED'` 事件派生（详见
> [`../../api/statistics.md`](../../api/statistics.md) 交付口径）。实际交付日期前端
> 应通过 `GET /parts/{part_id}/events` 拉时间线或由对应 DELIVERED 事件携带。

### PartListItem 字段

`TPart` 完整 25 列（2026-09-27 增 NUMERIC 金额列 `unit_price` / `total_price` 至 23+2=25 列）+
`customer_name` / `l1_customer_name` 冗余字段 + 列表项专用派生字段
`location` / `holder_name`；见 [`./index.md#mainconventions`](./index.md#端点约束与-python-一致)
关于 i64 字段序列化为 string 的约定。

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（`serialize_i64`） |
| `serial_no` | string? | 序列号 |
| `name` | string | 工单名 |
| `drawing_no` | string | 图号 |
| `applicant_name` | string | 申请人 |
| `quantity` | i32 | > 0 |
| `request_date` | date | 客户请求日 |
| `planned_delivery_date` | date | 计划交付日 |
| `customer_id` | string (i64) | 二级客户 id |
| `assembly_id` | string (i64)? | 父装配体 |
| `status` | string | part 状态枚举字符串（`INSPECTION` / `READY_TO_SHIP` 等） |
| `is_urgent` | bool | 紧急标记 |
| `order_no` | string? | 订单号 |
| `system_delivery_date` | date? | 系统派工日 |
| `note` | string? | 备注 |
| `unit_price` | string (Decimal) | 2026-09-27 新增：单价（NUMERIC(12,2) NOT NULL DEFAULT 0）。`rust_decimal::Decimal` + `serde-with-str` 自动序列化为 string。<br>**2026-09-28 行类型合并引入**：装配体行（`row_type='ASSEMBLY'`）的 `unit_price` 来自 `t_assembly.unit_price: Option<Decimal>`，service 层用 `unwrap_or(Decimal::ZERO)` 兜底——与 `t_part` DB DEFAULT 0 语义对齐、与 frontend Zod schema `unit_price: z.string()`（non-nullable）契约一致。原 plan §1.3 设想改 `PartListItem` 为 `Option<Decimal>`，经核实前端 Zod 同样 non-nullable，保持现状是两端契约对齐的最佳选择。 |
| `total_price` | string (Decimal) | 2026-09-27 新增：总价（NUMERIC(14,2) NOT NULL DEFAULT 0）。同上，string 避免 JS 浮点丢精度。<br>**2026-09-28 行类型合并引入**：同 `unit_price`，ASSEMBLY 行由 `t_assembly.total_price: Option<Decimal>` 用 `unwrap_or(Decimal::ZERO)` 兜底。 |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `created_by` | string (i64)? | |
| `updated_at` | naive datetime | |
| `updated_by` | string (i64)? | |
| `deleted_at` | naive datetime? | 软删标记 |
| `process_chain_id` | string (i64)? | 2026-09-16 migration 026 新增：逻辑指向 `t_part_process_chain.id`；`null` = 未制定工艺链 |
| `customer_name` | string? | 冗余（lookup_customer_names） |
| `l1_customer_name` | string? | 冗余（lookup_customer_names） |
| `location` | string? | **派生**（2026-09-16 PR-2 § part/service/crud.rs::enrich_part_list_with_location_and_holder）；`min-progress 活跃批次.location`（与 `compute_part_target` 一致）。无活跃批次 → `null`。前端展示文案规范化由前端承担（`PRODUCTION_SHELF` → "货架 X" 等）；后端只负责值。**仅 PART 行有值；ALL 模式装配件段恒 `null`**（t_assembly 不持 location 字段；真相源在 t_part_batch）。 |
| `holder_name` | string? | **派生**（同上）；按 min-progress 活跃批次的 `current_holder_id` 解析（按 batch.location 分桶：`PRODUCTION_SHELF` / `INSPECTION_SHELF` → `t_shelf.code`；`WORKER` → `t_worker.name`；`OUTSOURCE_COMPANY` → `t_outsource_company.name`；`OFFICE` / `None` → `null`）。**仅 PART 行有值；ALL 模式装配件段恒 `null`**（t_assembly 不持 holder_name 派生字段）。 |
| `row_type` | string? | 2026-09-28 新增。行类型标识：`"PART"`（零件）/ `"ASSEMBLY"`（装配件）/ `null`（历史 caller 旧 PART-only 形态）。ALL 模式混合列表用；前端按此字段切普通行 vs Tree 节点 lazy load。 |
| `has_children` | bool | 2026-09-28 新增。是否有子件（Tree data lazy mode 必需）：PART 行 → `false`；ASSEMBLY 行 → `child_count.unwrap_or(0) > 0`。后端不强制走 `GET /assemblies/{id}` 预拉，前端按此字段切换展开/折叠交互即可。 |
| `child_count` | string (i64)? | 2026-09-28 新增。子件计数（装配表行专用）；PART 行 → `null`。真相源：`t_part WHERE assembly_id = $1 AND deleted_at IS NULL` 的 COUNT（≤200 ids / 1 extra query）。 |

> 2026-09-16 PR-2（migration 027）：`t_part` 删 `actual_delivery_date` /
> `location` / `current_holder_id` / `placed_at` / `delivery_note_id` /
> `has_been_repaired` 6 个批次依附列；`TPart` 由 29 列精简至 23 列。
> 列表项位置/持有人展示由 service 层按 min-progress 活跃批次派生（见上）。
>
> 2026-09-16 PR-3 批次 step 化（migration 028）：`t_part_batch.next_process_id` 列
> 替换为 `current_process_step_id`（逻辑 FK → `t_process_chain_step.id`）。
> `t_part_batch.placed_at` 列已删除（不再统计生产时间）。`TPart.next_process_id`
> DB 列保留作派生缓存，由 `sync_from_batch_change` rollup 派生
> （min-progress 活跃 batch 的 step JOIN `t_process_chain_step.process_id`）。
>
> **2026-09-27 part 域前后端字段对齐**：
> - 响应中**不再出现** `next_process_id` / `next_process_name` —— 但仅 list 端点不
>   暴露；detail 端点（`PartDetailOut`）仍含 `next_process_id`（`PartListItem`
>   改显式列字段、不再 flatten `TPart`；`TPart.next_process_id` 撤销
>   `#[serde(skip)]` 恢复序列化）。DB 列、statemachine rollup、batch repo
>   派生链路完全不变。inspection / dashboard / outsource 域另标
>   `/// @deprecated 2026-09-27` 注释（行为不变）。前端如需该信息，按
>   `current_process_step_id` 派生即可。
> - 本目录 VOs **从未**包含过 `customer_path` / `parent_customer_name` 字段
>   —— 前端若仍读取请改读 `l1_customer_name`（2026-09-16 PR-2 24 列对齐后
>   即稳定）。
>
> 2026-09-27 part 域前后端字段对齐：新增 `unit_price` / `total_price` 两个
> NUMERIC 金额列（NOT NULL DEFAULT 0），后端用 `rust_decimal::Decimal` +
> `serde-with-str` 序列化为 JSON string，前端按 string 解析（避免 JS
> `Number.MAX_SAFE_INTEGER` 浮点丢精度）。
>
> 2026-09-16（migration 026 FK 翻转）：`TPart` 新增 `process_chain_id`（string i64?）
> —— 逻辑指向 `t_part_process_chain.id`；`null` = 未制定工艺链。前端「工序制定」页
> 按此字段是否为 `null` 批量区分已制定 / 未制定工序的零件。

### PartListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartListItem](#partlistitem-字段)[] | |
| `total` | int | 满足过滤的总数 |
| `limit` | int | 实际生效 |
| `offset` | int | 实际生效 |

> **2026-09-27 part 域前后端字段对齐**：`total` / `limit` / `offset` 改为裸 i64
> → JSON number，对齐其它 9 域（UserListOut / CustomerListOut / WorkerListOut /
> OutsourceCompanyListOut / ProcessListOut / ShelfListOut / DeliveryNoteListOut /
> DeliveryGroupListOut / OutsourceQuoteListOut）。雪花 ID 仍走 `serialize_i64` →
> JSON string 规避 JS `Number.MAX_SAFE_INTEGER` 精度截断，本处分页字段是普通
> i64（远小于 2^53），无精度风险，直接 JSON number。

### PartDetailOut 字段

`TPart` 完整 25 列（2026-09-16 PR-2 瘦身后 23 列 + 2026-09-27 新增 `unit_price` / `total_price` 2 列；**含** `next_process_id`——detail 端点保留）+ `customer_name` / `l1_customer_name` / `current_batch_id`（仅 INSPECTION 时非 None）。

> 2026-09-27 review 第 1 轮修复语义：`TPart.next_process_id` 撤销
> `#[serde(skip)]`，detail 端点（`PartDetailOut` 仍 flatten `TPart`）保留
> `next_process_id` 字段；list 端点（`PartListItem` 改显式列字段）不含
> `next_process_id`。

## 端点约束（与 Python 一致）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → `40901 VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → `20101`
- **状态机**：详见 [状态机（can_transition_to 白名单）](./inspection.md#状态机can_transition_to-白名单)；不在白名单内的 source / target 组合返回 `20103 BIZ_INVALID_TRANSITION`（迁移表见 `src/modules/part/statemachine.rs`）
- **事件日志**：状态迁移在 service 内事务内统一插入对应事件，service 提交后由 WS 中枢广播
- **part↔batch 同步（PR-B2/B3 改写，2026-09-11）**：
  part.status 不再直接 UPDATE，而是由 `PartService::sync_from_batch_change`
  按"最慢批次"规则 rollup（min-progress）。lifecycle 终态 / 翻转（deliver / cancel /
  complete / start-repair）只在最近一条 source-status 批次上翻状态；装配体子件
  rollup 同步触发（见 [`../assemblies/index.md#子件状态聚合`](../assemblies/index.md#子件状态聚合auto-rollup)）。
  详见 [`docs/refactor-part-assembly-batch.md`](../../refactor-part-assembly-batch.md)。
---

## 状态机

见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)。

## 错误码参考

part / lifecycle 错误码（20101 / 20103 / 20104 / 20109 / 20111 / 20115 / 20116 / 20117 / 20118 / 20119 / 21420 / 40001 / 40300 / 40901）见 [`./inspection.md`](./inspection.md#错误码参考part-lifecycle)。

货架错误码（20511 / 20512 — to-inspection / to-process 专用）见 [`./inspection.md`](./inspection.md#货架错误码205xx--to-xxx--worker-scan-触发)。

## 参考

- 集成测试：`tests/part_api.rs`（inspection 流全链路）+ `tests/part_crud.rs`（CRUD + lifecycle 27 用例）
- 仓库分层：`src/modules/part/handler.rs` (axum) → `service/{crud,inspection,lifecycle}.rs` (业务) → `repo/{part,batch,event}.rs` (SQL)
- 状态机：`src/modules/part/statemachine.rs`
- 错误码：`src/shared/error.rs::code`
- worker-scan 联动：详见 [`../production/worker-pool.md`](../production/worker-pool.md)
- Python myERP 参考：`/Users/ren/Code/myERP/api/v1/part.py`（46 个端点；本目录 18 个端点之外的 32 个 Python 独有端点（其中 Rust 18 中有 4 个 Rust-only 端点）见 [`../inconsistencies.md`](../inconsistencies.md)）
