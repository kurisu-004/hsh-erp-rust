# part 域 API

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：CRUD / by-serial 查询 / upload-drawing / lifecycle 状态机（deliver / cancel / complete / start-repair）/ batch 集合读（`GET /{part_id}/batches`）。所有路径前缀 `/api/v2`。
>
> **⚠️ 2026-10-02 域收窄**：以**单个批次**为操作对象的 25 条路由已从 `/api/v2/parts/*` 迁到 `/api/v2/prod/batches/*`，锚点由 `part_id` 改为 `batch_id`。**part 域现在只剩多批次动作（`/{part_id}/cancel` / `force-complete`）与非批次动作（CRUD / 文件 / 列表 / `GET /{part_id}/batches`）**。本表仍列出迁出的端点并指向新路径，便于按动作查文档；25 条逐条清单见 [`../production/batches.md`](../production/batches.md#2026-10-02-t_part_batch-子资源迁入)。
> **⚠️ 2026-10-03 再收窄 2 条**：`/parts/outsource-in-flight` 与 `/parts/outsource-sendable` 两个外协读端点迁往 outsource 域（旧路径无 alias，见下表对应两行）。这两条与下表的 `GET /outsource-shipments/in-flight` / `GET /outsource-sendable` 行由**读侧分支**在同一次合并中落地，单分支快照下不存在。
> **计数口径（2026-10-03 登记）**：本表登记 **53 行**，其中 **27 行**指向已迁出 part 域的端点（25 条 batch 子资源 + 2 条外协读端点）、**2 行**是已下线端点（`send-to-programming` / `recall-to-programming`，返回 404），故 part 域**实际注册**端点 = 53 − 27 − 2 = **24**，与 `src/modules/part/mod.rs::router()` 的 method 级注册数逐条对齐（`GET /` + `POST /` 算 2 条）。
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
| GET | `/api/v2/prod/batches/inspection` | Manager / Inspector | 待品检批次列表（status=INSPECTION；含 batch_id + version + 工单 + holder/process/delivery_note/customer 名称一次解析）—— **2026-10-02 自 part 域迁入** | [`inspection.md`](./inspection.md#get-apiv2prodbatchesinspection) |
| POST | `/api/v2/parts/{part_id}/update` | Manager / Clerk | 字段可选 UPDATE（OCC + 软删守卫） | [`crud.md`](./crud.md#post-apiv2partspart_idupdate) |
| POST | `/api/v2/parts/{part_id}/soft-delete` | **Manager** | 软删（OCC + 终态禁 + delivery_note 锁禁） | [`crud.md`](./crud.md#post-apiv2partspart_idsoft-delete) |
| POST | `/api/v2/parts/{part_id}/upload-drawing` | Manager / Clerk | Multipart PDF 上传到 COS + 落 `t_part_file`（CAS key 格式 2026-09-11 变更） | [`crud.md`](./crud.md#post-apiv2partspart_idupload-drawing) |
| POST | `/api/v2/parts/{part_id}/upload-3d-model` | Manager / Clerk | Multipart 3D 模型上传到 COS（STEP/STP/IGES/IGS/STL/OBJ/3MF）+ 落 `t_part_file`（2026-09-11 新增） | [`crud.md`](./crud.md#post-apiv2partspart_idupload-3d-model) |
| POST | `/api/v2/prod/batches/{batch_id}/deliver` | Manager / Clerk | READY_TO_SHIP → DELIVERED —— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_iddeliver) |
| POST | `/api/v2/parts/{part_id}/cancel` | Manager / Clerk | 5 状态白名单 → CANCELLED（拒 delivery_note 锁） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcancel) |
| POST | `/api/v2/parts/{part_id}/force-complete` | **Manager** | 全部非 CANCELLED 批次强推 COMPLETED（逃生通道：**绕状态机 + 不走 OCC**）—— 2026-10-02 起留在 part 域（多批次动作） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idforce-complete) |
| POST | `/api/v2/prod/batches/{batch_id}/complete` | Manager / Clerk | DELIVERED → COMPLETED（**part 派生到终态时**才清空 serial_no：先写 `SERIAL_RELEASED` 归档事件再清列）—— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idcomplete) |
| POST | `/api/v2/prod/batches/{batch_id}/start-repair` | Manager / Clerk / Inspector | 置 `is_repairing=true`（status 不变，仍 IN_PROCESS）—— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idstart-repair) |
| POST | `/api/v2/prod/batches/to-inspection` | Manager / Inspector | 静态批量送检（PENDING/PROGRAMMING/IN_PROCESS → INSPECTION；`items[].batch_id` 仍在 body）—— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesto-inspection) |
| POST | `/api/v2/prod/batches/{batch_id}/to-inspection` | Manager / Inspector | 单件送检 —— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesbatch_idto-inspection) |
| POST | `/api/v2/prod/batches/to-ship` | Manager / Inspector | 静态批量通过品检（INSPECTION → READY_TO_SHIP；`items[].batch_id` 仍在 body）—— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesto-ship) |
| POST | `/api/v2/prod/batches/{batch_id}/to-ship` | Manager / Inspector | 单件通过品检（INSPECTION → READY_TO_SHIP）—— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesbatch_idto-ship) |
| POST | `/api/v2/prod/batches/{batch_id}/to-process` | Manager / Inspector | 单件指定下一工序（INSPECTION → IN_PROCESS）—— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesbatch_idto-process) |
| POST | `/api/v2/prod/batches/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检；成功后同事务触发 worker-pool refill —— **2026-10-02 迁往 prod 域**（无 Path，`serial_no` 主键） | [`inspection.md`](./inspection.md#post-apiv2prodbatchesworker-scan) |
| GET | `/api/v2/parts/pending-programming` | Manager / Clerk / Inspector / CncProgrammer | 待编程列表（Phase 1）—— **2026-10-01 起前端请改用 [`GET /api/v2/prod/programming/pending`](../production/pending-programming.md#get-apiv2prodprogrammingpending)**（本端点保留兼容，规则2 走「批次货架 → 工序」间接链路、开发库恒返空） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| GET | `/api/v2/outsource-shipments/in-flight` | Manager / Clerk | 外协在途列表 —— **2026-10-03 迁往 outsource 域**（旧 `/parts/outsource-in-flight` 无 alias，命中本表 `GET /{part_id}` catch-all → **400**；新形状含 `batch_id` / `batch.version` / 批次余量，是部分接收的输入源） | [`../outsource-shipments.md`](../outsource-shipments.md#get-apiv2outsource-shipmentsin-flight) |
| GET | `/api/v2/outsource-sendable` | Manager / Clerk / Inspector | 可发外协一览（APPROVAL / DIRECT 两种 `send_mode`）—— **2026-10-03 迁往 outsource 域独立顶层前缀**（旧 `/parts/outsource-sendable` 无 alias，命中本表 `GET /{part_id}` catch-all → **400**） | `docs/api/outsource-sendable.md`（**读侧分支文件，与本行端点同一次合并落地**，单分支快照下不存在） |
| GET | `/api/v2/prod/batches/repair` | Manager / Inspector | 维修批次列表（判据 `status='DELIVERED'`）—— **2026-10-02 自 part 域迁入** | [`lifecycle.md`](./lifecycle.md#get-apiv2prodbatchesrepair) |
| GET | `/api/v2/prod/batches/repairing` | Manager / Inspector | 维修中批次列表（判据 `is_repairing=true`）—— **2026-10-02 自 part 域迁入** | [`lifecycle.md`](./lifecycle.md#get-apiv2prodbatchesrepairing) |
| GET | `/api/v2/parts/location-tree` | Manager / Clerk / Inspector / CncProgrammer | 库位树（Phase 1） | [`crud.md`](./crud.md#phase-1-列表与筛选) |
| POST | `/api/v2/prod/batches/scan/deliver` | Manager / Clerk | 扫码发货 —— **2026-10-02 迁往 prod 域** | [`inspection.md`](./inspection.md#post-apiv2prodbatchesscandeliver) |
| POST | `/api/v2/parts/match-by-excel-items` | Manager / Clerk | Excel 行匹配（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/batch-update-order-info` | Manager / Clerk | 批量更新订单信息（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/parts/batch-with-pdfs` | Manager / Clerk | 多页 PDF 树形创建（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| GET | `/api/v2/parts/by-work-type/{work_type_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工种查 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| GET | `/api/v2/parts/pickable-by-work-type/{work_type_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工种查可领取 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| GET | `/api/v2/parts/by-worker/{worker_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工人查持有 part（Phase 2） | [`crud.md`](./crud.md#phase-2-工种工人视角) |
| POST | `/api/v2/prod/batches/{batch_id}/place-on-shelf` | Manager / Clerk | 上架 —— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idplace-on-shelf) |
| POST | `/api/v2/prod/batches/{batch_id}/recall-to-pending` | Manager / Clerk | 召回至 PENDING —— **2026-10-02 迁往 prod 域**（lifecycle.md 尚无独立章节） | [`production/batches.md`](./index.md) |
| POST | `/api/v2/parts/{part_id}/send-to-programming` | Manager / Clerk | 派发编程（Phase 1）—— **已下线，返回 404** | [`lifecycle.md`](./lifecycle.md#get-apiv2partspending-programming) |
| POST | `/api/v2/prod/batches/{batch_id}/release-from-programming` | Manager / Clerk | 编程完成释放（PROGRAMMING → IN_PROCESS）—— **2026-10-02 迁往 prod 域**（lifecycle.md 尚无独立章节；20706 守卫见 [`production/process-chain.md`](../production/process-chain.md#20706-biz_process_chain_required)） | [`production/process-chain.md`](../production/process-chain.md#20706-biz_process_chain_required) |
| POST | `/api/v2/parts/{part_id}/recall-to-programming` | Manager / Clerk | 召回编程（Phase 1）—— **已下线，返回 404** | [`lifecycle.md`](./lifecycle.md#get-apiv2partspending-programming) |
| POST | `/api/v2/prod/batches/{batch_id}/send-to-outsource` | Manager / Clerk / Inspector | 派发外协 —— **2026-10-02 迁往 prod 域**（APPROVAL / DIRECT 双模式 + 部分发送 `quantity`） | [`production/batches.md`](../production/batches.md#外协流转send--receive) |
| POST | `/api/v2/prod/batches/{batch_id}/receive-from-outsource` | Manager / Clerk / Inspector | 外协回收入库 —— **2026-10-02 迁往 prod 域**（支持部分接收 `quantity`） | [`production/batches.md`](../production/batches.md#外协流转send--receive) |
| POST | `/api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` | Manager / Clerk / Inspector | 外协回收 → 品检（整批） —— **2026-10-02 迁往 prod 域** | [`production/batches.md`](../production/batches.md#外协流转send--receive) |
| POST | `/api/v2/prod/batches/{batch_id}/complete-repair` | Manager / Clerk / Inspector | 完成维修 —— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idcomplete-repair) |
| POST | `/api/v2/prod/batches/{batch_id}/repair-dispatch` | Manager / Clerk | 派发维修 —— **2026-10-02 迁往 prod 域** | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idrepair-dispatch) |
| POST | `/api/v2/prod/batches/{batch_id}/scan-inspect` | Manager / Inspector | 扫码品检 —— **2026-10-02 迁往 prod 域**（本表尚无独立章节，见 [`./inspection.md` 状态机表](./inspection.md#状态机can_transition_to-白名单)） | [`inspection.md`](./inspection.md#状态机can_transition_to-白名单) |
| GET | `/api/v2/parts/{part_id}/events` | Manager / Clerk / Inspector / CncProgrammer | 工单事件时间线（Phase 1） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| GET | `/api/v2/parts/{part_id}/batches` | Manager / Clerk / Inspector / CncProgrammer | 列出 part 下所有批次（Phase 1） | [`batch.md`](./batch.md#get-apiv2partspart_idbatches) |
| GET | `/api/v2/parts/{part_id}/assembly` | Manager / Clerk / Inspector / CncProgrammer | 按 part 反查所属装配体（无父装配件为 null）—— 2026-09-25 D-08；路由注册在 part nest，**契约归 assembly 域** | [`../assemblies/crud.md`](../assemblies/crud.md#get-apiv2partspart_idassembly) |
| POST | `/api/v2/parts/{part_id}/files/confirm` | Manager / Clerk | 直传 COS 链路绑定（head 校验 size → copy 到 CAS key → INSERT READY part_file）—— 契约归 [`../files.md`](../files.md#post-apiv2partspart_idfiles-confirm) | [`crud.md`](./crud.md#post-apiv2partspart_idfiles-confirm) |
| POST | `/api/v2/prod/batches/{batch_id}/split` | Manager / Clerk | 拆分批次 —— **2026-10-02 迁往 prod 域** | [`./batch.md`](./batch.md#post-apiv2prodbatchesbatch_idsplit) |
| POST | `/api/v2/prod/batches/{batch_id}/cancel` | Manager / Clerk | 取消**单个**批次 —— **2026-10-02 迁往 prod 域**（区别：part 域 `POST /{part_id}/cancel` 翻转该 part 全部活跃批次） | [`crud.md`](./crud.md#phase-1-流程辅助) |
| POST | `/api/v2/prod/batches/{batch_id}/pick-up` | Manager / Clerk / ShelfAccount | B 方案手动 pick-up 兜底 —— **2026-10-02 迁往 prod 域**（本表尚无独立章节，载荷见 [`./lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idpick-up)） | [`lifecycle.md`](./lifecycle.md#post-apiv2prodbatchesbatch_idpick-up) |

> **路由顺序（part 域剩余端点）**：所有静态段必须在 `/{part_id}/...` catch-all 前注册。
> （`outsource-in-flight` / `outsource-sendable` 两个静态段 2026-10-03 已随端点迁往 outsource 域下线）
> 2026-10-02 之后 part 域的静态段只剩 `GET /` `POST /` `POST /batch` `POST /batch-with-pdfs`
> + `by-serial/*` + Phase 1 / 2 的 `pending-programming` / `location-tree` /
> `match-by-excel-items` / `batch-update-order-info` / `by-work-type/*` / `pickable-by-work-type/*`
> / `by-worker/*`，其后是 `GET /{part_id}` + `POST /{part_id}/{update,soft-delete,upload-*,
> files/confirm,events,cancel,force-complete}` 等单件 catch-all。
>
> **迁往 prod 域的 25 条**由 `prod::batch` 注册，挂在 `/api/v2/prod/batches/*` 之下；25 条
> 逐条清单见 [`../production/batches.md`](../production/batches.md#2026-10-02-t_part_batch-子资源迁入)。
> 同 nest 内**静态段必须先于 `/{batch_id}` 注册**：`POST /prod/batches/scan/deliver`（首段静态
> `scan`）与 `POST /prod/batches/{batch_id}/*` 段数相同，靠 axum / matchit 的静态优先规则消解。
>
> axum / matchit 的匹配规则是**静态段优先、参数段兜底**——任何静态段如果排在
> `/{part_id}` 之后，都会被 `/{part_id}` 吃掉并交给 `Path<i64>` 反序列化
> （`ErrorKind::ParseError`）→ **400**，不是 404。写文档时注意区分两种「打不到」：
> 有 catch-all 兜底 ⇒ 400；没有任何路由匹配 ⇒ 404。

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
| `has_cnc_program` | bool | 2026-09-29 新增（CNC 重构 5 任务之一）。是否已上传 G_CODE 数控程序。真相源：`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`。`GET /parts/pending-programming` 走专用 repo 填充真实值；其它 list 端点默认 `false`（service 不 enrich，避免 N+1）。详见 [`./lifecycle.md#get-apiv2partspending-programming`](./lifecycle.md#get-apiv2partspending-programming)。 |

> 2026-09-16 PR-2（migration 027）：`t_part` 删 `actual_delivery_date` /
> `location` / `current_holder_id` / `placed_at` / `delivery_note_id` /
> `has_been_repaired` 6 个批次依附列；`TPart` 由 29 列精简至 23 列。
> 列表项位置/持有人展示由 service 层按 min-progress 活跃批次派生（见上）。
>
> 2026-09-16 PR-3 批次 step 化（migration 028）：`t_part_batch.next_process_id` 列
> 替换为 `current_process_step_id`（逻辑 FK → `t_process_chain_step.id`）。
> `t_part_batch.placed_at` 列已删除（不再统计生产时间）。`TPart.next_process_id`
> DB 列保留作派生缓存。
>
> **2026-09-30 rollup 改直读 `t_part_batch.current_process_id`（migration 004）**：
> `TPart.next_process_id`（DB 列名与对外 DTO 字段名均**不变**）的派生源从
> 「min-progress 活跃 batch 的 `current_process_step_id` → JOIN
> `t_process_chain_step.process_id`」改为「min-progress 活跃 batch 的
> **`current_process_id` 直读**」。同时 `t_part_batch` 新增
> `current_process_id`（逻辑 FK → `t_process.id`），作为批次**工序池归属的唯一
> 权威依据**；`current_process_step_id` 降级为**可选的显示用定位信息**（仅当工单
> 有工序链时才有值，允许 NULL；且**只在首次定位工序时写、之后不再推进**，
> 不是「当前走到第几步」的进度指针 —— 见
> [`inspection.md`](./inspection.md#inspectionbatchlistitemout-字段)）。
> - 修掉隐患：原派生只看「最慢批次」的 step_id，而该值对无工序链工单恒为 NULL，
>   会把整个工单的 `t_part.next_process_id` 抹成 NULL —— 该列是**删工序的保护
>   条件之一**（`t_process` 软删前 `count_referencing` 的 5 个子查询之一），
>   被抹成 NULL 等于这道防线静默失效。
> - 少一次 DB 往返：原先每次 rollup 都要额外 SELECT 一次
>   `t_process_chain_step` 把 step_id 翻成 process_id。
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
- **part↔batch 同步（PR-B2/B3 改写 2026-09-11；2026-10-01 收口为 status_gate 单一写入口）**：
  part.status 不再直接 UPDATE，而是由「写批次状态」那一个函数一并在事务内派生
  （`part::service::status_gate::apply_batch_status_change`，min-progress 规则）。
  lifecycle 终态 / 翻转（deliver / cancel / complete / start-repair）只在最近一条
  source-status 批次上翻状态；装配体子件 rollup 同步触发（见
  [`../assemblies/index.md#子件状态聚合`](../assemblies/index.md#子件状态聚合auto-rollup)）。
  详见 [`docs/refactor-part-assembly-batch.md`](../../refactor-part-assembly-batch.md) 与
  [状态派生契约](#状态派生契约2026-10-01)。
---

## 状态派生契约（2026-10-01）

### 状态词汇（batch 与 part 共用同一套 8 个值）

| status | progress | 说明 |
|---|---|---|
| `PENDING` | 0 | 待下发 / 待加工 |
| `PROGRAMMING` | 1 | **已废弃的进入路径**（2026-09-29）：枚举、`as_str`、`from_str` 映射与 4 条出口边（`→ PENDING` / `→ IN_PROCESS` / `→ INSPECTION` / `→ CANCELLED`）保留，**只为消化历史数据**；新流程不产生该状态（待编程一览改由 `t_process.is_cnc` 列驱动） |
| `IN_PROCESS` | 2 | 生产中（含**返修中**） |
| `OUTSOURCE` | 3 | 外协加工中 |
| `INSPECTION` | 4 | 待品检 |
| `READY_TO_SHIP` | 5 | 待发货 |
| `DELIVERED` | 6 | 已交付 |
| `COMPLETED` / `CANCELLED` | 终态 | 终态（`part_status_progress` 不给终态定档，由 `compute_*_target` 单独短路处理） |

**返修不再是状态**（2026-10-01 BREAKING CHANGE，migration 005/006）：**起修**
（`start-repair`）只把 `status` 保持 `IN_PROCESS` 不翻转（progress 同档 2），返修
事实改由 **`t_part_batch.is_repairing`（boolean，默认 false）** 承载。所有「返修中」
的查询 / 守卫一律读该列，不再判 `status = 'REPAIRING'`（DB 里不再产生该字面量；
`PartStatus::from_str("REPAIRING")` 保留 → `IN_PROCESS` 的过渡兼容分支）。
**2026-10-02 review 第 2 轮订正**：原文「批次返修中时 `status` 保持 `IN_PROCESS`」
字面读成了「返修中 ⇒ `IN_PROCESS`」的不变式，**不成立** —— 标记与 `status`
**正交**：起修后送检 / 送检通过 / 发货都只保持标记，故返修件的 `status` 也可能是
`INSPECTION` / `READY_TO_SHIP` / `DELIVERED`（可达链见
[`./inspection.md`](./inspection.md) 订正段）。

### 三层单向派生 + 单一写入口

```
t_part_batch.status            ← 唯一真源
   │  status_gate::rollup_part_derived（min-progress）
   ▼
t_part.status / next_process_id   ← 派生缓存
   │  assembly::compute_assembly_target
   ▼
t_assembly.status               ← 派生缓存
```

- **所有 `t_part_batch.status` 写入必须走
  `src/modules/prod/batch/status_gate.rs`**（`apply_batch_status_change` 单行 /
  `apply_bulk_batch_status_change_for_part` 批量）。它在一事务内完成
  「写批次（OCC + 源状态白名单）→ 回填 part 派生列 → 级联 assembly →
  终态序列号归档 / 释放」。
- **caller 不需要、也不应该自己再调 sync**：13 个历史 `mark_batch_*` 写点已全部
  改写为 status_gate 之上的薄包装，函数名与参数不变。
- 改造动因：收口前有 3 个写点漏调 sync，派生缓存长期与真源不一致且**不报任何错**
  （只是列表页显示错状态）。
- **CI 强制**：`cargo test --lib` 里的
  `part::service::status_gate::write_guard_tests::no_outside_file_writes_batch_status`
  扫描全 `src/**/*.rs`，除 `status_gate.rs` 外任何文件写
  `UPDATE t_part_batch SET status …` 即测试失败（注释 / `#[cfg(test)]` 块 /
  只改其它列的 UPDATE 不在判定范围，细则见该测试文档注释）。
- 派生层的 OCC 冲突**一律降级为「跳过 + `tracing::warn!`」**，绝不让派生缓存否决
  用户的主操作（详见 [`../assemblies/index.md#子件状态聚合`](../assemblies/index.md#子件状态聚合auto-rollup)）。
- **派生层也不得覆盖主操作**（2026-10-01 review 第 1 轮 B1）：`POST /parts/{id}/cancel`
  先把 part 打成 CANCELLED，随后的批次级联若算出 COMPLETED（「已完成批次 + 其余被批量
  取消」）**不许**写回。`update_part_rollup` 的 `status NOT IN ('COMPLETED','CANCELLED')`
  是 SQL 层兜底，bulk 入口的 `PartDerivation::KeepPartTerminalAsIs` 是显式表达
  （跳过 part 写的同时**继续**派生父装配件）。已知代价：已终态的 part 不再被 rollup /
  对账端点改写。
- **`StatusChange` 的三态**：`Option` 的 `None` = 「保持原值」，「清 NULL」由
  `clear_location` / `clear_holder_id` / `clear_process_id` /
  `clear_process_step_id` 显式表达。出池写点（召回 / 送检 / 外协收回 / 返修出池）
  必须同时清这 4 列。
- **终态序列号归档事件的 id 取自 `SnowflakeIdGenerator`**（`StatusChange::event_id`
  由 caller 透传）：`GET /parts/{id}/events` 是 `ORDER BY id DESC`，用建单期的
  `part_id` 顶替会把归档事件排到时间线最底部，且 part 二次进终态时 pkey 冲突。
- 兜底修正入口：`POST /api/v2/admin/recompute-rollup`（Manager）—— **复用**上述
  派生函数重跑一遍并回报 before→after，幂等；全量对账用**分表游标**续扫
  （响应回 `next_part_after_id` / `next_assembly_after_id`，回传为请求的
  `part_after_id` / `assembly_after_id`）直到 `truncated=false`。两表 id 来自同一
  个雪花流且按时间序交错，**必须分表推进**（review 第 2 轮 MAJOR-2：共用一个游标会
  永久跳过 `(assembly_max, part_max]` 那段装配件却仍报 `truncated=false`）。
  报告里 `parts_skipped_terminal > 0` = 「已终态、派生被守卫跳过」，≠「数据已一致」。
  见 [`../admin.md`](../admin.md)。

> **migration 007 注释订正（2026-10-01 review 第 1 轮 m1）**：007 里「
> `t_part_event.id` 无默认值、SQL 里无法生成雪花 ID」这句是**错的** ——
> baseline 已有 `SET DEFAULT nextval('t_part_event_id_seq')`（reviewer 已在
> information_schema 确认）。007 选 `MAX(id)+ROW_NUMBER()` 的**结论**仍然可用
> （运行时那条路径拿不到雪花生成器），但理由是「不与运行时生成的 id 抢空间 /
> 排到时间线顶部」而不是「无默认值」。运行时路径已改为由 caller 透传真实雪花
> （见上一条），故 migration 007 本身按 append-only 约定**保持原样不改**
> （改它会变更 sqlx 记录的 checksum，让已 apply 过该迁移的库启动失败）。
>
> **TODO(2026-10-01 review 第 2 轮 MINOR-4，follow-up PR —— 上线窗口需人工评估)**：
> `migrations/20261001000200_007_serial_release.sql:113-118` 是
> `DROP INDEX` + **非并发** `CREATE UNIQUE INDEX` + 全表 `UPDATE`。在生产级
> `t_part` 上，事务内的 `CREATE INDEX` 会持 `ACCESS EXCLUSIVE` 锁**贯穿整个构建**，
> 部署即阻塞全部 part 写（读写一起停），且全表 UPDATE 耗时与表大小线性相关。
> 正确做法是 `-- no-transaction` 迁移 + `CREATE UNIQUE INDEX CONCURRENTLY`
> （并发建索引不持写锁；失败会留 INVALID 索引，需手工 DROP 重建）。
> **本轮不改**：append-only 铁律禁止修改已存在的 migration 文件（改内容会变更
> sqlx 记录的 checksum，让已 apply 过该迁移的库启动失败），而新增一个「重建索引」
> 的 migration 属于另一个 PR 的范围。上线前请人工评估该迁移的锁窗口，必要时改在
> 低峰期执行，或用「新 migration + CONCURRENTLY」补建。

### 序列号（`serial_no`）生命周期

| 阶段 | 行为 |
|---|---|
| 派发 | 建件时由 `t_serial_counter` 按 L1 客户 `serial_prefix` 生成；`uk_t_part_serial_no` 唯一索引保证不重复 |
| 流转中 | 序列号在 part 的**整个非终态期**持续占用该唯一索引（货还在厂里，正确） |
| 进入终态（`COMPLETED` / `CANCELLED`） | 由 rollup step 4 自动释放：**先**归档一条 `t_part_event`（`event_type='SERIAL_RELEASED'`，`note` 记原序列号）**再**清 `t_part.serial_no`。每个 part 至多 1 条归档事件（终态不可重复进入） |
| 父装配件进终态 | 直接清 `t_assembly.serial_no`（不归档：`t_assembly` 无事件表，其 `note` 是用户可编辑业务备注，拿它记系统动作会污染用户数据） |
| 取消（`CANCELLED`） | 同样释放（2026-10-01：唯一索引谓词已补 `deleted_at IS NULL AND status <> 'CANCELLED'`，软删 / 作废工单不再占坑） |

调用方**不需要**为序列号做任何事：释放是 rollup 的一步，`deliver` / `complete` /
`cancel` / `force-complete` 等端点都自动带上（2026-10-01 起 `force-complete`
也不再单独调清理函数）。

## 状态机

见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)。

## 错误码参考

part / lifecycle 错误码（20101 / 20103 / 20104 / 20109 / 20111 / 20115 / 20116 / 20117 / 20118 / 20119 / 21420 / 40001 / 40300 / 40901）见 [`./inspection.md`](./inspection.md#错误码参考part--lifecycle)。

货架错误码（20511 / 20512 — to-inspection / to-process 专用）见 [`./inspection.md`](./inspection.md#货架错误码205xx--to-xxx--worker-scan-触发)。

## 参考

- 集成测试：`tests/part_api.rs`（inspection 流全链路）+ `tests/part_crud.rs`（CRUD + lifecycle 27 用例）
- 仓库分层：`src/modules/part/handler.rs` (axum) → `service/{crud,inspection,lifecycle}.rs` (业务) → `repo/{part,batch,event}.rs` (SQL)
- 状态机：`src/modules/part/statemachine.rs`
- 错误码：`src/shared/error.rs::code`
- worker-scan 联动：详见 [`../production/worker-pool.md`](../production/worker-pool.md)
- Python myERP 参考：`/Users/ren/Code/myERP/api/v1/part.py`。**端点数口径**：一律按 **router 口径的 method 级注册数**计（`GET /` + `POST /` 算 2 条），即 part 域 **24** 条（推导见文件头「计数口径」一节）。逐条 Python↔Rust 差异见 [`../inconsistencies.md`](../inconsistencies.md)
