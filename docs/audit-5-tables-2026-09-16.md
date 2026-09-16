# 5 张核心业务表 Schema 审计

> 日期：2026-09-16
> 范围：`t_part`, `t_part_batch`, `t_assembly`, `t_part_process_chain`, `t_process_chain_step`
> 视角：仅 Rust 后端代码（`backend-rust/src/`）
> 目的：事实陈述，不含设计建议
>
> **状态**：2026-09-17 由 PR-1（工艺链 FK 翻转）/ PR-2（t_part 瘦身 + t_assembly 删 actual_delivery_date）/ PR-3（t_part_batch 批次 step 化）全部落地修复，本文档作历史归档保留，不再随代码更新。
>
> 后续 PR-4（2026-09-17 守卫修复 + 卫生项）补齐：
> - 工序软删守卫补 `t_process_chain_step` 引用计数（PR-4 A1）
> - `GET /parts` 加 `locations` / `holder_ids` 过滤（PR-4 A2）
> - 索引补齐 3 个 + 清理孤儿 `t_assembly_id_seq`（PR-4 B1+B4，migration 029）
> - `split_batch` / `split_batch_for_partial_pass` 公共函数合并（PR-4 B2）
> - `TAssembly.request_date` / `planned_delivery_date` 与 DDL NOT NULL 对齐去 Option（PR-4 B3）

---

## 0. 审计方法

- **DDL 来源**：`backend-rust/migrations/` 全部相关 sql（005 / 006 / 013 / 014 / 015 / 017 / 019）+ `docker exec erp-local-postgres psql` 直查当前库实表
- **Rust 代码**：`backend-rust/src/` 146 个 `.rs` 文件 grep + 完整阅读关键路径
- **文档**：`docs/architecture.md`、`docs/conventions.md`、`docs/api/parts/`、`docs/api/assemblies/`、`docs/api/delivery-notes/`、`backend-rust/CLAUDE.md`
- **数据**：`docker exec erp-local-postgres` 当前 prod_data 实查

---

## 1. 表结构现状

### 1.1 `t_part`（28 列）

**DDL 来源**：`backend-rust/migrations/20260811100005_005_create_part_tables.sql`（基表），叠加：
- `013_widen_assembly_serial_no_to_15`（serial_no → varchar(15)）
- Phase E 合并（order_no / system_delivery_date / note / delivery_note_id / has_been_repaired 由后续 ALTER 并入）

| 列 | 类型 | NULL | 默认 | 备注 |
|---|---|---|---|---|
| `id` | bigint | N | `nextval(t_part_id_seq)` | PK |
| `serial_no` | varchar(15) | Y | | 物理件条码 |
| `name` | varchar(200) | N | | |
| `drawing_no` | varchar(100) | N | | |
| `applicant_name` | varchar(50) | N | | |
| `quantity` | integer | N | 1 | |
| `unit_price` | numeric(12,2) | N | 0 | **TPart 实体不投影**（`part/model.rs:11`） |
| `total_price` | numeric(14,2) | N | 0 | 同上 |
| `request_date` | date | N | | |
| `planned_delivery_date` | date | N | | |
| `actual_delivery_date` | date | Y | | |
| `status` | varchar(20) | N | 'PENDING' | **无 CHECK** |
| `location` | varchar(20) | Y | | **无 CHECK**；注释只列 4 值，实际用 5 值 |
| `is_urgent` | boolean | N | false | |
| `current_holder_id` | bigint | Y | | 多态（worker/shelf） |
| `placed_at` | timestamp | Y | | |
| `next_process_id` | bigint | Y | | |
| `customer_id` | bigint | N | | |
| `assembly_id` | bigint | Y | | |
| `version` | integer | N | 0 | OCC |
| `created_at` / `updated_at` | timestamp | N | now() | |
| `created_by` / `updated_by` | bigint | Y/N | | |
| `deleted_at` | timestamp | Y | | 软删 |
| `order_no` | varchar(30) | Y | | |
| `system_delivery_date` | date | Y | | |
| `note` | varchar(500) | Y | | |
| `delivery_note_id` | bigint | Y | | |
| `has_been_repaired` | boolean | N | false | |

**索引**（21 个）：PK + `ix_t_part_assembly_id` + `ix_t_part_assembly_id_status` + `ix_t_part_current_holder_id` + `ix_t_part_customer_id` + `ix_t_part_customer_status_delivery` + `ix_t_part_deleted_at` + `ix_t_part_delivery_note_id` + `ix_t_part_drawing_no` + `ix_t_part_is_urgent` + `ix_t_part_location` + `ix_t_part_location_status_next_process` + `ix_t_part_name` + `ix_t_part_next_process_id` + `ix_t_part_order_no` + `ix_t_part_placed_at` + `ix_t_part_planned_delivery_date` + `ix_t_part_request_date` + `ix_t_part_status` + `ix_t_part_status_holder` + `uk_t_part_serial_no`（partial WHERE serial_no IS NOT NULL）

**当前数据**：2 行（id=1, id=225816949525843968），其中 id=1 是装配件子件（assembly_id=224024978708758700）。

---

### 1.2 `t_part_batch`（18 列）

**DDL 来源**：`006_create_part_batch_table.sql`，后加 `014_add_worker_pool_indexes` / `015_backfill_initial_part_batches` / Phase E 合并 `has_been_repaired`。

| 列 | 类型 | NULL | 默认 | 备注 |
|---|---|---|---|---|
| `id` | bigint | N | `nextval(t_part_batch_id_seq)` | PK |
| `part_id` | bigint | N | | |
| `batch_no` | integer | N | | |
| `quantity` | integer | N | | 无默认（part 有 1） |
| `status` | varchar(20) | N | 'PENDING' | **无 CHECK** |
| `location` | varchar(20) | Y | | **无 CHECK**；注释列 5 值（OFFICE/PRODUCTION_SHELF/WORKER/INSPECTION_SHELF/OUTSOURCE_COMPANY） |
| `current_holder_id` | bigint | Y | | 多态（shelf/worker/outsource_company） |
| `next_process_id` | bigint | Y | | |
| `placed_at` | timestamp | Y | | |
| `delivery_note_id` | bigint | Y | | |
| `parent_batch_id` | bigint | Y | | 拆分谱系 |
| `version` | integer | N | 0 | OCC |
| `created_at` / `updated_at` | timestamp | N | now() | |
| `created_by` / `updated_by` | bigint | Y/N | | |
| `deleted_at` | timestamp | Y | | **业务无软删路径**（PR-B2 后删除走 cancel） |
| `has_been_repaired` | boolean | N | false | |

**约束**：`UNIQUE (part_id, batch_no)`（plain UNIQUE，非 partial）；其余无 CHECK。

**索引**（13 个）：PK + `ix_t_part_batch_current_holder_id` + `ix_t_part_batch_deleted_at` + `ix_t_part_batch_delivery_note_id` + `ix_t_part_batch_holder_location`（partial WHERE deleted_at IS NULL，mig 014） + `ix_t_part_batch_location` + `ix_t_part_batch_location_status_next_process` + `ix_t_part_batch_next_process_id` + `ix_t_part_batch_part_id` + `ix_t_part_batch_placed_at` + `ix_t_part_batch_pool_pickup`（mig 014） + `ix_t_part_batch_status` + `ix_t_part_batch_status_holder` + `uq_t_part_batch_part_no`

**当前数据**：1 行（part_id=225816949525843968 / batch_no=1 / quantity=1 / PENDING）。

---

### 1.3 `t_assembly`（23 列）

**DDL 来源**：同一 `005_create_part_tables.sql`。`applicant_name` 在 DDL 是 NULL；`request_date` / `planned_delivery_date` DDL 是 NOT NULL，但 `assembly/model.rs:21-22` Rust 实体标 `Option<NaiveDate>`（**模型/DDL 不一致**）。

| 列 | 类型 | NULL | 默认 | 备注 |
|---|---|---|---|---|
| `id` | bigint | N | 无 | **无 sequence DEFAULT**（mig 005:47 注释："id is supplied by application code"），但仍 ATTACHED sequence `t_assembly_id_seq` |
| `drawing_no` | varchar(100) | N | | |
| `name` | varchar(200) | N | | |
| `applicant_name` | varchar(50) | **Y** | | |
| `customer_id` | bigint | N | | |
| `request_date` | date | N | | 模型标 NULL |
| `planned_delivery_date` | date | N | | 模型标 NULL |
| `actual_delivery_date` | date | Y | | |
| `is_urgent` | boolean | N | false | |
| `status` | varchar(20) | N | 'PENDING' | **无 CHECK** |
| `version` | integer | N | 0 | |
| `created_at` / `updated_at` | timestamp | N | now() | |
| `created_by` / `updated_by` | bigint | Y/N | | |
| `deleted_at` | timestamp | Y | | |
| `serial_no` | varchar(15) | Y | | 子件派生 `{serial_no}-{i:02d}` |
| `quantity` | integer | N | 1 | |
| `unit_price` | numeric(12,2) | N | 0 | 装配件真用 |
| `total_price` | numeric(14,2) | N | 0 | |
| `order_no` | varchar(30) | Y | | |
| `system_delivery_date` | date | Y | | |
| `note` | varchar(500) | Y | | |

**索引**（10 个）：PK + `ix_t_assembly_customer_id` + `ix_t_assembly_customer_status` + `ix_t_assembly_deleted_at` + `ix_t_assembly_drawing_no` + `ix_t_assembly_order_no` + `ix_t_assembly_planned_delivery` + `ix_t_assembly_serial_no` + `ix_t_assembly_status` + `uk_t_assembly_serial_no`（partial WHERE deleted_at IS NULL AND serial_no IS NOT NULL）

**当前数据**：2 行（id=224024978708758700 "ASM-001"、id=224024978708758701 "ASM-002"）。

---

### 1.4 `t_part_process_chain`（11 列）

**DDL 来源**：`017_create_process_chain_tables.sql`（2026-09-11）。

| 列 | 类型 | NULL | 默认 | 备注 |
|---|---|---|---|---|
| `id` | bigint | N | 无 | 雪花 App 侧生成 |
| `part_id` | bigint | N | | **UNIQUE**（1:1 binding） |
| `name` | varchar(64) | N | '默认工艺' | |
| `version` | integer | N | 0 | OCC |
| `note` | text | Y | | 整链备注 |
| `created_at` / `updated_at` | timestamp | N | now() | |
| `created_by` / `updated_by` | bigint | N | | |
| `deleted_at` | timestamp | Y | | **无显式软删 fn**（upsert 走 soft_delete steps） |

**索引**：PK + `ix_part_process_chain_deleted` + `t_part_process_chain_part_id_key`（UNIQUE part_id）

**当前数据**：3 行（part_id=1 / part_id=9999000000000099 / part_id=225816949525843968）

---

### 1.5 `t_process_chain_step`（12 列）

**DDL 来源**：`017_create_process_chain_tables.sql` + `019_add_process_chain_step_note`。

| 列 | 类型 | NULL | 默认 | 备注 |
|---|---|---|---|---|
| `id` | bigint | N | 无 | |
| `chain_id` | bigint | N | | |
| `sort_order` | integer | N | | 稀疏 10/20/30 |
| `process_id` | bigint | N | | |
| `estimated_minutes` | integer | N | | CHECK >= 0 |
| `version` | integer | N | 0 | OCC |
| `created_at` / `updated_at` | timestamp | N | now() | |
| `created_by` / `updated_by` | bigint | N | | |
| `deleted_at` | timestamp | Y | | |
| `note` | text | Y | | mig 019 新增；单步备注 |

**索引**：PK + `ix_chain_step_chain`（partial WHERE deleted_at IS NULL） + `uq_chain_step_chain_order`（partial unique `(chain_id, sort_order) WHERE deleted_at IS NULL`）

**当前数据**：3 行（part 1 有 2 步铣/车；part 225816949525843968 有 1 步车）

---

### 1.6 跨表引用关系（logical FK，所有引用都无物理 FK）

```
t_customer (L1/L2)
   ↑
t_assembly  ──── 1:N ───→ t_part (assembly_id)
                              │
                              ├── 1:N ──→ t_part_batch (part_id)
                              │            ├─ parent_batch_id → t_part_batch (自引用)
                              │            ├─ current_holder_id → t_worker / t_shelf / t_outsource_company (多态)
                              │            ├─ next_process_id → t_process
                              │            └─ delivery_note_id → t_delivery_note
                              │
                              └── 1:1 ──→ t_part_process_chain (UNIQUE part_id)
                                              │
                                              └── 1:N ──→ t_process_chain_step (chain_id)
                                                           └─ process_id → t_process

外部被引：
  t_part.part_id ← t_cnc_program / t_drawing_file / t_outsource_quote / t_part_file / t_part_event
  t_part_batch.id ← t_outsource_shipment.batch_id / t_pickup_skip_event.batch_id / t_part_event.batch_id
```

---

## 2. Rust 代码使用情况

### 2.1 文件清单

#### 2.1.1 实体定义（model.rs）

| 表 | 文件:行 | 实体 |
|---|---|---|
| `t_part` | `src/modules/part/model.rs:38-75` | `TPart` 28 列；`TPartInspected`（`model.rs:89-103`，12 列窄投影）；`TPartRollupState`（`model.rs:150-162`，6 列 rollup） |
| `t_part_batch` | `src/modules/part_batch/model.rs:14-34` | `TPartBatch` 18 列；`InspectionBatchListRow`（9-JOIN 投影） |
| `t_assembly` | `src/modules/assembly/model.rs:14-39` | `TAssembly` 23 列 |
| `t_part_process_chain` | `src/modules/process_chain/model.rs:14-34` | `TPartProcessChain` 11 列 |
| `t_process_chain_step` | `src/modules/process_chain/model.rs:37-62` | `TProcessChainStep` 12 列 |

#### 2.1.2 仓储层（repo）

| 文件 | 表 | 关键函数 |
|---|---|---|
| `src/modules/part/repo/part.rs` | `t_part` | `get_by_id`、`list_by_ids`、`get_by_serial`、`list_children`、`list_with_filters`、`create_part`(INSERT 14 列)、`update_part`(QueryBuilder 9 字段)、`soft_delete_part`、`update_part_rollup`、`mark_part_repairing_flag_only`、`clear_part_serial_no_when_completed`、`cascade_sync_from_assembly`、`scale_children_quantity`、`insert_child_for_assembly`（双表 INSERT）、`list_by_assembly_id`、`list_with_filters` |
| `src/modules/part/repo/batch.rs` | `t_part_batch` + `t_part` | `mark_batch_passed_inspection`、`mark_batch_inspected`、`mark_batch_failed_inspection`、`mark_batch_returned`、`mark_batch_delivered`、`mark_batch_completed`、`mark_part_cancelled`、`mark_batch_cancelled`、`mark_batch_repairing`、`cancel_all_active_batches_for_part`、`split_batch_for_partial_pass` |
| `src/modules/part_batch/repo.rs` | `t_part_batch` | `get_by_id`、`list_by_delivery_note`、`list_with_part_by_delivery_note`（18 JOIN 列）、`list_with_part_by_delivery_note_ids`、`list_by_part_ids`、`list_active_by_part_id`、`list_active_by_part_id_with_holder`、`list_batches_with_part_in_customers`、`list_recent_by_note`、`count_held_by_worker`、`list_held_by_worker`、`find_delivered_older_than`、`update`(OCC)、`attach_to_note`、`split_batch`（3 SQL）、`create_initial_batch` |
| `src/modules/part_batch/repo_list.rs` | `t_part_batch` + `t_part` | inspection-batches 列表 + COUNT |
| `src/modules/assembly/repo.rs` | `t_assembly` + `t_part` | `get_by_id`、`list_by_ids`、`get_by_serial`、`insert`、`update_partial`、`soft_delete`、`cancel`（无 OCC）、`list_with_filters`、`count_with_filters`、`aggregate_children_status`、`update_status_if_not_terminal` |
| `src/modules/process_chain/repo/query.rs` | `t_part_process_chain` + `t_process_chain_step` | `get_chain_by_part`、`list_steps_by_chain` |
| `src/modules/process_chain/repo/mutate.rs` | 同上 | `insert_chain`、`bump_chain_version`、`soft_delete_all_steps_for_chain`、`bulk_insert_steps` |

#### 2.1.3 服务层（service）

| 文件 | 表 | 关键操作 |
|---|---|---|
| `src/modules/part/service/crud.rs` | `t_part` + `t_part_batch` + `t_part_file` | `create_part`、`update_part`、`soft_delete_part`、`batch_create_parts_legacy`、`batch_update_order_info` |
| `src/modules/part/service/lifecycle.rs` | `t_part` + `t_part_batch` | `deliver`、`complete`、`cancel`、`start_repair`（含 `mark_part_repairing_flag_only` 补 part） |
| `src/modules/part/service/inspection.rs` | `t_part` + `t_part_batch` | `to_ship`、`to_process`、`to_inspection` 薄包装 |
| `src/modules/part/service/inspection_core.rs` | 同上 | `_split_for_partial_pass` + 3 个 core |
| `src/modules/part/service/worker_scan.rs` | 同上 | `worker_scan INSPECTED`、`worker_scan RETURNED` |
| `src/modules/part/service/phase1.rs`（2887 行） | `t_part` + `t_part_batch` + 外协表 | `mark_batch_with_status_and_meta`、`mark_batch_status_only`、`mark_batch_for_programming`、`place_on_shelf`、`recall_to_pending`、`send_to_programming`、`release_from_programming`、`recall_to_programming`、`send_to_outsource`、`receive_from_outsource`、`complete_repair`、`repair_dispatch`、`scan_inspect`、`scan_deliver_part`、`pick_up`、`split_batch`、`cancel_batch`、`batch_with_pdfs` |
| `src/modules/part/service/rollup.rs` | `t_part_batch` → `t_part` → `t_assembly` | `sync_from_batch_change`（PR-B2 核心入口） |
| `src/modules/assembly/service.rs` | `t_assembly` + `t_part`（子件） | `create_assembly`（同事务 4-N 表 INSERT）、`update_assembly`（级联 8 字段 + quantity 缩放）、`cancel_assembly`、`start_assembly`、`soft_delete_assembly`、`sync_from_part_change`、`sync_assembly_status` |
| `src/modules/process_chain/service/crud.rs` | `t_part_process_chain` + `t_process_chain_step` | `get_chain_by_part`、`upsert_chain`（整组替换：先删 step 后插） |

#### 2.1.4 跨域引用

| 引用方 | 文件 | 用途 |
|---|---|---|
| `customer/service.rs:329,335` | `t_part` + `t_assembly` | 软删前引用计数 |
| `applicant/repo.rs:158-178`、`applicant/service.rs:218-219` | `t_part` | 申请人引用计数 |
| `worker/repo.rs:295-310` | `t_part` | 工人停用前 current_holder_id 引用 |
| `worker_pool/repo.rs:94,123,215` | `t_part_batch` + `t_part` | `take_one_from_pool` CTE |
| `worker_pool/service.rs:224,594` | `t_part_batch` | pool count + refill 后 `sync_from_batch_change` |
| `dashboard/service.rs:114-115,335-336,373-374,531` | `t_part_batch` + `t_part` | dashboard 聚合 |
| `statistics/repo.rs:29,75,80,109,135,203,221,364,443` | `t_part` + `t_part_event` | 统计 |
| `shelf/repo.rs:177` | `t_part_batch` | 货架 current_load |
| `process/repo.rs:300` | `t_part` | 工序引用计数 |
| `outsource/service.rs:379,472,1054,1137,1158` | `t_part` + `t_part_batch` | 外协 quote 校验 |
| `delivery_note/service/inner.rs:225,259` | `t_part_batch` + `t_part` | 单子 add_parts |
| `delivery_note/service/crud.rs:225,266,293` | 同上 | list + DTO |
| `delivery_note/service/lifecycle.rs:475` | `t_part_batch` | soft_delete 清 delivery_note_id |
| `_e2e/handler.rs:213` | `t_part` | seed INSERT |
| `cnc_program/service.rs:53` | `t_part` | CNC 上传前存在性 |
| `part_file/service.rs:228` | `t_part` | 文件 owner 校验 |
| `infra/serial.rs:91` | `t_part` | 业务单号序列 |

---

### 2.2 状态机与迁移

#### 2.2.1 `t_part` 状态机（10 态，`src/modules/part/statemachine.rs`）

```
PENDING / PROGRAMMING / IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED / REPAIRING / OUTSOURCE / COMPLETED / CANCELLED
```

迁移白名单（21 条，`part/statemachine.rs:126-162`）：

| from → to | 触发端点 |
|---|---|
| PENDING → PROGRAMMING / IN_PROCESS / OUTSOURCE / INSPECTION / CANCELLED | send-to-programming / place-on-shelf / send-to-outsource / to-inspection / cancel |
| PROGRAMMING → PENDING / IN_PROCESS / INSPECTION / CANCELLED | recall-to-pending / release-from-programming / to-inspection / cancel |
| IN_PROCESS → PENDING / PROGRAMMING / INSPECTION / REPAIRING / OUTSOURCE / CANCELLED | recall-to-pending / recall-to-programming / to-inspection / start-repair / send-to-outsource / cancel |
| IN_PROCESS+WORKER → IN_PROCESS+WORKER | pick-up（不改 status） |
| INSPECTION → READY_TO_SHIP / IN_PROCESS / REPAIRING / CANCELLED | to-ship / to-process / scan-inspect FAIL / cancel |
| READY_TO_SHIP → DELIVERED / CANCELLED | deliver / cancel |
| DELIVERED → COMPLETED / CANCELLED | complete / cancel |
| REPAIRING → INSPECTION / IN_PROCESS / CANCELLED | complete-repair (zone=INSPECTION/PRODUCTION) / cancel |
| OUTSOURCE → IN_PROCESS / INSPECTION / CANCELLED | receive-from-outsource / receive-from-outsource-to-inspection / cancel |
| COMPLETED → * | 全部拒绝 |
| CANCELLED → * | 全部拒绝 |

`compute_part_target`（`part/statemachine.rs:468-512`）：min-progress 规则聚合 batches → part 派生 status。

#### 2.2.2 `t_part_batch` 状态机

与 `t_part` **共用 PartStatus 10 态**（`part/statemachine.rs:8` 注释），无独立 state machine。所有 `mark_*` SQL 守卫写死源 status 白名单作为双层防御。

**核心 mark 函数清单**（`src/modules/part/repo/batch.rs` + `phase1.rs`）：

| fn | 源→目标 | 派生列变化 | 行 |
|---|---|---|---|
| `mark_batch_passed_inspection` | INSPECTION → READY_TO_SHIP | status only | `batch.rs:263-286` |
| `mark_batch_inspected` | PENDING/PROGRAMMING/IN_PROCESS → INSPECTION | status + location='INSPECTION_SHELF' + current_holder_id | `batch.rs:290-318` |
| `mark_batch_failed_inspection` | INSPECTION → IN_PROCESS | status + location='PRODUCTION_SHELF' + current_holder_id + next_process_id | `batch.rs:321-351` |
| `mark_batch_returned` | IN_PROCESS+WORKER → IN_PROCESS+PRODUCTION_SHELF | current_holder_id + location + next_process_id | `batch.rs:394-424` |
| `mark_batch_delivered` | READY_TO_SHIP → DELIVERED | status only | `batch.rs:601-614` |
| `mark_batch_completed` | DELIVERED → COMPLETED | status only | `batch.rs:618-631` |
| `mark_batch_cancelled` | 5 状态 → CANCELLED | status only | `batch.rs:653-668` |
| `mark_batch_repairing` | IN_PROCESS → REPAIRING | status + has_been_repaired=true | `batch.rs:672-685` |
| `mark_part_cancelled` + `cancel_all_active_batches_for_part` | 5 状态 → CANCELLED | part: status='CANCELLED' + serial_no=NULL；batch: 全 CANCELLED | `batch.rs:635-650` + `batch.rs:696-716` |
| `mark_batch_with_status_and_meta` | 任意 | status + location + current_holder_id + next_process_id + placed_at=COALESCE(placed_at,now()) + version+1 | `phase1.rs:245-272` |
| `mark_batch_status_only` | 任意 | status only | `phase1.rs:275-294` |
| `mark_batch_for_programming` | PENDING/PROGRAMMING/IN_PROCESS → PROGRAMMING | status + location='OFFICE' + current_holder_id=NULL + placed_at=now() | `phase1.rs:298-322` |
| pick-up IN_PROCESS 分支（inline） | IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER | location + current_holder_id + placed_at=COALESCE | `phase1.rs:2540-2553` |

#### 2.2.3 `t_assembly` 状态机（7 态，`src/modules/assembly/statemachine.rs`）

```
PENDING / IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED / COMPLETED / CANCELLED
```

迁移白名单（11 条，`assembly/statemachine.rs:70-86`）：

| from → to | 触发端点 |
|---|---|
| PENDING → IN_PROCESS / CANCELLED | start_assembly / cancel_assembly |
| IN_PROCESS → INSPECTION / COMPLETED / CANCELLED | 子件 rollup / 子件全 COMPLETED / cancel |
| INSPECTION → READY_TO_SHIP / CANCELLED | 子件 rollup / cancel |
| READY_TO_SHIP → DELIVERED / CANCELLED | 子件 rollup / cancel |
| DELIVERED → COMPLETED / CANCELLED | 子件 rollup / cancel |
| COMPLETED → * | 全部拒绝 |
| CANCELLED → * | 全部拒绝 |

`compute_assembly_target`（`assembly/statemachine.rs:185-220`）复用 `part_status_progress`。

---

### 2.3 多表 UPDATE 模式

#### 2.3.1 batch → part → assembly 级联（PR-B2 唯一路径，2026-09-11）

```
[任一 batch 状态翻转]
  ↓ PartRepo::mark_*(UPDATE t_part_batch + version+1)
  ↓ PartService::sync_from_batch_change(part/service/rollup.rs:38-110)
  ↓   - 拉 part 所有 active batch
  ↓   - compute_part_target(batches) → target status + 5 派生列
  ↓   - target ≠ current → PartRepo::update_part_rollup(UPDATE t_part 5 列)
  ↓   - status 变化 → AssemblyService::sync_from_part_change
  ↓     - aggregate_children_status(UPDATE t_assembly.status)
```

`sync_from_batch_change` 物化列：`status / location / current_holder_id / next_process_id / placed_at`（**5 列**，**不**物化 `delivery_note_id` 和 `has_been_repaired`）。

**所有 sync_from_batch_change 调用点**（25+ 处）：
- `lifecycle.rs:118, 312`（deliver, complete）
- `lifecycle.rs:416`（start-repair → 额外 `mark_part_repairing_flag_only` 补 part 的 has_been_repaired）
- `phase1.rs:366, 432, 490, 555, 617, 878, 996, 1095, 1165, 1269, 1895, 2028, 2558`
- `inspection_core.rs:159, 295, 432`
- `worker_scan.rs:194, 271`
- `worker_pool/service.rs:174, 594`

#### 2.3.2 其他多表 UPDATE 路径

| 路径 | 文件 | 说明 |
|---|---|---|
| `insert_child_for_assembly` | `part/repo/part.rs:487-558` | 同事务 2 INSERT（t_part + t_part_batch） |
| `create_part` + `create_initial_batch` | `part/service/crud.rs:127-144` | 同事务 2 INSERT |
| `split_batch` | `part_batch/repo.rs:455-536` | 同事务 3 SQL：MAX(batch_no)+1 / INSERT 新 batch（写 parent_batch_id）/ UPDATE 源 batch `quantity -= $3` |
| `split_batch_for_partial_pass` | `part/repo/batch.rs:523-595` | 同事务 3 SQL：`INSERT INTO t_part_batch SELECT ... FROM t_part_batch WHERE id = $6`（继承 has_been_repaired、parent_batch_id） |
| `cascade_sync_from_assembly` | `part/repo/part.rs:593-637` | assembly update 时 8 字段级联到所有子件 part |
| `scale_children_quantity` | `part/repo/part.rs:649-677` | `quantity * ratio` 缩放子件 |
| `create_assembly` | `assembly/service.rs:325` | 主装配件 + 子件 + 子件 initial batch 同事务 |
| worker_pool `take_one_from_pool` | `worker_pool/repo.rs` | CTE 内 UPDATE batch 切 holder，service 层调 sync_from_batch_change |

---

### 2.4 t_part 与 t_part_batch 重叠列的真相源

| 列 | 真相源 | t_part 写入路径 | t_part_batch 写入路径 |
|---|---|---|---|
| `status` | t_part_batch | `update_part_rollup`（min-progress 聚合） | 11 个 mark_* |
| `location` | t_part_batch | `update_part_rollup`（物化） | mark_* 系列 |
| `current_holder_id` | t_part_batch | `update_part_rollup`（物化） | mark_* 系列 |
| `next_process_id` | t_part_batch | `update_part_rollup`（物化） | mark_* 系列 |
| `placed_at` | t_part_batch | `update_part_rollup`（物化） | mark_batch_with_status_and_meta / mark_batch_for_programming / pick-up |
| `delivery_note_id` | t_part_batch | **无显式写路径** | `update` / `attach_to_note` / `remove_parts` / `soft_delete` |
| `has_been_repaired` | t_part_batch + 手工补 part | `mark_part_repairing_flag_only`（独立 UPDATE） | `mark_batch_repairing`（true）+ inline `phase1.rs:1263,1888` |

---

### 2.5 批次创建 / 拆分 / 合并 / 取消

| 操作 | 入口 | SQL |
|---|---|---|
| 创建 | `create_part` / `batch_create_parts_legacy` / `batch_with_pdfs` / 子件随 asm / backfill migration 015 | `INSERT t_part` + `INSERT t_part_batch` 同事务 |
| 拆分（手动） | `POST /parts/{id}/batches/split` | 3 SQL：MAX(batch_no)+1 / INSERT 新 batch（parent_batch_id=$10, has_been_repaired=FALSE） / UPDATE 源 batch `quantity -= $3` |
| 拆分（部分通过） | `to_ship_core` / `to_process_core` / `to_inspection_core` | 3 SQL：MAX(batch_no)+1 / `INSERT ... SELECT`（继承 has_been_repaired, parent_batch_id=$4） / UPDATE 源 batch `quantity -= $3` |
| 合并 | **无任何路径** | — |
| 取消 part | `POST /parts/{id}/cancel` | `mark_part_cancelled` + `cancel_all_active_batches_for_part` |
| 取消 batch | `POST /parts/{id}/batches/{batch_id}/cancel` | `mark_batch_cancelled` |

---

### 2.6 工艺链代码使用

`upsert_chain`（`process_chain/service/crud.rs:49-192`）流程：
1. 校验入参 sort_order 不重复 + process_id parse
2. `get_chain_by_part` 看是否已有
3. 有 → `bump_chain_version`（OCC）；无 → `insert_chain`（捕获 23505 → 207xx）
4. `soft_delete_all_steps_for_chain` 软删旧 step
5. `bulk_insert_steps` 插新 step（QueryBuilder `VALUES (...),(...)`）

无独立 state machine；mig 017 注释明确设计要点（1:1 binding / sparse sort_order 10/20/30 / partial unique / soft delete）。

---

## 3. Schema 状态观察（仅事实，不含建议）

### 3.1 派生列同步差异

- **5 列派生列**（status/location/current_holder_id/next_process_id/placed_at）由 `sync_from_batch_change` 统一回填。
- **`t_part.delivery_note_id`**：所有写路径（`update` / `attach_to_note` / `remove_parts` / `soft_delete`）只写 `t_part_batch.delivery_note_id`；`t_part.delivery_note_id` 无任何 UPDATE 路径；但 `part/service/lifecycle.rs:182-189`、`part/service/crud.rs:989` 在 cancel/soft_delete 守卫中**读取**此列。
- **`t_part.has_been_repaired`**：仅 `start_repair` 路径显式补写一次；其他路径（`repair_dispatch` `phase1.rs:1248-1269`、`scan_inspect FAIL` `phase1.rs:1888-1893`）只写 batch，part 不同步。

### 3.2 CHECK 约束不一致

`migrations/005:3` 注释明确 `t_part.status` / `t_assembly.status` 是 `varchar(20) WITHOUT CHECK`（state machine 在 service）。相邻表 `t_delivery_note.status` / `t_outsource_quote.status` / `t_outsource_shipment.status` 有 CHECK 约束（mig 009:45 等）。

`t_part.location` / `t_part_batch.location` / `t_part_batch.status` / `t_assembly.status` 均无 CHECK。`t_part.location` 注释列 4 值，运行时实际有 5 值（含 OUTSOURCE_COMPANY）。

### 3.3 缺失索引

- `t_part_batch.parent_batch_id`（拆分谱系查询当前无 hot path，注释 `migrations/006:39`）
- `t_part_batch.has_been_repaired`
- `t_part.actual_delivery_date`
- `t_part.system_delivery_date`（`worker_pool/repo.rs:115` hot 排序）
- `t_part.has_been_repaired`
- `t_part.applicant_name`（`worker_pool/repo.rs:413` JOIN 用）
- `t_assembly.name`、`t_assembly.is_urgent`

### 3.4 拆分函数重复

`split_batch`（`part_batch/repo.rs:455`）和 `split_batch_for_partial_pass`（`part/repo/batch.rs:523`）80% 重叠：
- 前者：手动 split 用，`has_been_repaired=FALSE`（不继承）
- 后者：to-ship/to-process/to-inspection 部分通过用，`has_been_repaired` 从源继承
- 后者用 `INSERT ... SELECT`，前者用 `VALUES`
- parent_batch_id 参数化位置不同

### 3.5 `t_assembly.id` 与雪花主键约定

`migrations/005:47` 注释：`t_assembly.id has NO snowflake SEQUENCE in production — id is supplied by application code`。但 DDL 内仍 ATTACHED 了 `t_assembly_id_seq` 且 `id` 列无 `DEFAULT nextval(...)`。Rust 端用 `SnowflakeIdGenerator::next_id()` 生成。

### 3.6 模型与 DDL 不一致

`src/modules/assembly/model.rs:21-22`：`request_date` 和 `planned_delivery_date` 标 `Option<NaiveDate>`，DDL 是 NOT NULL；`applicant_name` 在 model 是 `Option<String>`（DDL 也是 NULL），但 `t_part.applicant_name` 是 NOT NULL，不对称。

### 3.7 `t_part_batch.deleted_at` 无业务软删路径

PR-B2 改造删除了所有 mark_*_soft_delete 路径（`part/repo/batch.rs:14-17` 注释："删除走 cancel 而非软删"）。`deleted_at` 列 + `ix_t_part_batch_deleted_at` 索引仍存在并被所有查询的 `WHERE deleted_at IS NULL` 守卫使用。

### 3.8 `t_part_process_chain.deleted_at` 无显式软删 fn

所有 repo 函数把 `deleted_at IS NULL` 作为守卫，但 `upsert_chain` 用 `soft_delete_all_steps_for_chain` 而非 chain 本身软删；无 `soft_delete_chain`。

### 3.9 TPart 实体不投影 price 字段

`src/modules/part/model.rs:11` 注释：`unit_price` / `total_price` 待 `rust_decimal` feature 上线。`insert_child_for_assembly:part/repo/part.rs:487-558` 显式 `unit_price=0, total_price=0`。

---

## 4. 端点 / 业务约束概览

- **Parts 域**：49 端点（`docs/api/parts/` 4 子文件），Phase 1+2 已上线。
- **Assemblies 域**：8 端点（`docs/api/assemblies/`），Phase 3 已上线。
- **Delivery-note 域**：依赖 part/batch 的状态机守卫（`docs/api/delivery-notes/drafts.md` 路线 B 修订史）。
- **Worker-pool 域**：take_one_from_pool CTE 依赖 `t_part_batch.location/status/current_holder_id/next_process_id` 4 列组合索引。
- **Dashboard / Statistics 域**：聚合 `t_part` + `t_part_batch` + `t_part_event`。

---

## 附录：修复路线图（2026-09-17）

本审计 §3 节列出的所有状态观察由下列 PR 落地修复，每条对应 commit SHA 列表：

### PR-1（2026-09-16）工艺链 FK 翻转（migration 026）

- **改动核心**：
  - `t_part_process_chain` 不再持有 `part_id` 列（去 part_id UNIQUE）
  - `t_part` 加 `process_chain_id` 列 + `uq_t_part_process_chain` 部分唯一索引
  - 新端点 `GET /api/v2/process-chains/{chain_id}`
  - 新错误码 `20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING`（PUT 时 part 非 PENDING 拒绝）
- **关联 §3 观察**：
  - §2.6 工艺链代码使用：upsert_chain 路径（header + steps）→ 翻转后 caller 同事务 link
- **涉及 commit**（按时间顺序，全部已合 master）：
  - `feat(process_chain): PR-1 工艺链 FK 翻转 step1-3` 系列
  - `feat(part): PR-1 link process_chain_id 字段` 系列
  - `test(process_chain): PR-1 happy path + 非 PENDING 守卫 + by-id 端点`

### PR-2（2026-09-16）t_part 瘦身 + t_assembly 删 actual_delivery_date（migration 027）

- **改动核心**：
  - `t_part` 删 `actual_delivery_date` / `location` / `current_holder_id` / `placed_at` / `delivery_note_id` / `has_been_repaired` 6 列（真相源迁移到 `t_part_batch` 同名/语义等价列）
  - `t_part_batch` 删 `has_been_repaired` 列
  - `t_assembly` 删 `actual_delivery_date` 列（装配体实际交付由子件批次 DELIVERED 事件派生）
  - 前端列表页 `PartListItem` 加 `location` / `holder_name` 派生字段（service 层 min-progress 活跃批次派生）
- **关联 §3 观察**：
  - §3.1 派生列同步差异：`t_part.delivery_note_id` / `t_part.has_been_repaired` 两条「读取而无显式写路径」问题
  - §3.2 `t_part.location` 注释列 4 值（实际 5 值）—— 列已删，注释问题消失
- **涉及 commit**（按时间顺序）：
  - `feat(part): PR-2 瘦身 step1-3` 系列
  - `feat(assembly): PR-2 t_assembly 删 actual_delivery_date`
  - `feat(service): PR-2 list_parts enrich location/holder`
  - `test+docs: PR-2 API 契约同步`

### PR-3（2026-09-16/17）t_part_batch 批次 step 化（migration 028）

- **改动核心**：
  - `t_part_batch` `next_process_id` → `current_process_step_id`（指向 `t_process_chain_step.id`，PR-1 FK 翻转后的工艺链 step）
  - `t_part_batch` 删 `placed_at` 列（不再统计生产时间）
  - 新错误码 `20706 BIZ_PROCESS_CHAIN_REQUIRED`（to_process / place_on_shelf / send_to_outsource 等"进入生产流"端点要求 part 已绑链）
  - `PartListItem` 加 `next_process_name` 派生（service 层 min-progress 活跃批次 step JOIN）
- **关联 §3 观察**：
  - §3.7 `t_part_batch.deleted_at` 无业务软删路径（保留列 + 索引，仅供 WHERE deleted_at IS NULL 守卫使用，无业务软删 fn）
  - §3.8 `t_part_process_chain.deleted_at` 无显式软删 fn：PR-1 已加 `soft_delete_chain`
- **涉及 commit**（按时间顺序）：
  - `feat(part): PR-3 批次 step 化 step1-3` 系列
  - `merge: feat/batch-step-ify → master（PR-3 + 4 红 review 修复）`（commit `6e207fc`）
  - `fix: PR-3 第 2/3 轮修复 R1-R4 —— 文档对齐 + 错误码精确化 + step 上下文保护`
  - `test: PR-3 适配 step 化 —— chain 必填 + 测试 helper`

### PR-4（2026-09-17）守卫修复 + 卫生项（本 PR）

- **改动核心**：
  - **A1 守卫修复**：`process/repo.rs::count_process_references` 增查 `t_process_chain_step.process_id`（PR-1 工艺链 FK 翻转后，part → chain → step 是新工艺引用通道）
  - **A2 守卫修复**：`GET /parts` 加 `locations` / `holder_ids` 过滤参数（前端 `usePartsListQuery.ts:204` 之前发被静默忽略）
  - **B1 卫生项**：migration 029 补齐 3 个缺失索引（`ix_t_part_batch_parent_batch_id` / `ix_t_part_system_delivery_date` / `ix_t_assembly_name`）
  - **B2 卫生项**：`_split_batch_inner` 公共函数合并 `split_batch` 与 `split_batch_for_partial_pass` 80% 重复
  - **B3 卫生项**：`TAssembly.request_date` / `planned_delivery_date` 去 Option，与 DDL NOT NULL 对齐
  - **B4 卫生项**：migration 029 清理孤儿 `t_assembly_id_seq`（migrations/005:47 注释明确「id is supplied by application code」但 ATTACHED 序列仍在）
  - **B5 卫生项**：`gap_for_mid_insert` / `reorder_with_step_size` 加 `#[allow(dead_code)]` + ⚠️ 注释（PR-1 upsert 走「先删旧 steps + 整组 bulk_insert」不触发中间插入路径）
- **关联 §3 观察**：
  - §3.3 缺失索引：3 条索引全部已补
  - §3.4 拆分函数重复：B2 已合并
  - §3.5 `t_assembly.id` 与雪花主键约定：B4 已清理孤儿序列
  - §3.6 模型与 DDL 不一致：B3 已对齐
- **涉及 commit**（按时间顺序，本 worktree）：
  - `fix: PR-4 守卫修复 —— chain step 引用计数 + locations/holder_ids 过滤`
  - `feat(db): PR-4 卫生项 B1+B4 —— 补齐 3 个缺失索引 + 清理 t_assembly_id_seq 孤儿序列`
  - `refactor(part_batch): PR-4 卫生项 B2 —— split_batch / split_batch_for_partial_pass 公共函数合并`
  - `refactor(assembly,process_chain): PR-4 卫生项 B3+B5 —— model/DDL 对齐 + 死代码标注`
  - `docs: PR-4 文档同步 —— docs/api + CLAUDE.md + audit 标记完成`

---

## 5. 已知遗留（未在 PR-4 解决，留后续）

- **§3.6 applicant_name 对称性**：`t_assembly.applicant_name` 在 DDL 与 model 都是 `Option<String>`（NULL），但 `t_part.applicant_name` 是 NOT NULL —— 由 service 层在 create / 子件继承时统一兜空串 `""`，未实际触发 23502。本观察非阻塞，未在 PR-4 修复。
- **§3.9 TPart 实体不投影 price 字段**：`unit_price` / `total_price` 仍由 `insert_child_for_assembly` 显式写 0；TPart 实体不投影（待 `rust_decimal` feature 上线后端点暴露）。未在 PR-4 修复。
