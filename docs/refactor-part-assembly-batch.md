# part / assembly / batch 关系重构方案

> 2026-09-11 立项。目标：理顺三者关系 ——
> **方向 A**：装配件数量语义改为「套」、子件信息字段与装配件自动同步；
> **方向 B**：车间流转对象统一为 batch、乐观锁锚定 batch，part / assembly 状态由 batch 变化自动 rollup 派生（最慢批次 / 最慢子件规则）。
>
> 本文档是实施的权威依据；落地时按 §6 顺序分 PR 进行。

## 1. 总体模型

```
t_assembly (quantity = 套数)                状态 7 态，由子件 min-progress rollup（现有机制保留）
  │ 1:N   信息字段向下同步：创建时继承 + 更新时无条件覆盖 + 套数变更按比例缩放
  ▼
t_part (quantity = 实际加工数)              status / location / current_holder_id 等 = 物化派生列
  │ 1:N   ▲ rollup 回调：取「最慢批次」（min-progress）
  ▼      │
t_part_batch ── 车间唯一状态流转对象；所有流转端点 OCC 锚定 t_part_batch.version
```

- 每套用量 = 子件 quantity / 装配件 quantity（创建时由入参确定，不单独存储）。
- `t_part.status` 等列**保留下线**为物化派生列（已确认决策）：由 rollup 回调统一维护，读侧（列表筛选 / DTO / shelf·worker 停用守卫 / assembly rollup）全部无感。

## 2. 已确认决策（2026-09-11 与需求方对齐）

| # | 决策点 | 结论 |
|---|---|---|
| D1 | 装配件套数被修改时子件数量 | **按比例自动缩放**：`new_child_qty = round(child_qty × new_qty / old_qty)`，下限 1；只改 `t_part.quantity`，不追溯已有批次数量 |
| D2 | 更新装配件时同步到子件的字段 | **全部共享信息字段无条件覆盖**（清单见 §3.2），排除 `actual_delivery_date`（流程产物） |
| D3 | lifecycle 四端点 | **cancel 保持 part 级**（级联取消全部活跃批次）；deliver / complete / start-repair **改 batch 级**（收 `batch_id` + `version`） |
| D4 | `t_part.status` 存续 | **保留物化派生列**，rollup 回调维护 |

## 3. 方向 A：assembly ↔ part 详细改动

### 3.1 创建时子件继承（`PartRepo::insert_child_for_assembly`，`src/modules/part/repo/part.rs:473`）

当前硬编码默认值（`applicant_name=''`、`request_date=CURRENT_DATE`、`is_urgent=FALSE`、`order_no=NULL`、`system_delivery_date=NULL`、`note=NULL`）改为从父件 `t_assembly` 继承：

| 子件列 | 新来源 |
|---|---|
| `applicant_name` | 父件 `applicant_name`（父可空 → 子写入空串兜底，列 NOT NULL） |
| `request_date` | 父件 `request_date` |
| `order_no` | 父件 `order_no` |
| `system_delivery_date` | 父件 `system_delivery_date` |
| `is_urgent` | 父件 `is_urgent` |
| `note` | 父件 `note` |
| `planned_delivery_date` | 子件入参优先，缺省继承父件（保持现有 COALESCE 形态，默认值由 `CURRENT_DATE` 改为父件值） |
| `customer_id` / `quantity` / `serial_no` | 现状不变（customer 继承父件；quantity 为实际加工数；serial `{asm_serial}-{i:02d}`） |
| `unit_price` / `total_price` | 保持 0（本期不动价格语义） |

- 保留「有 PDF 才派 serial、才建子件」的现有门槛（`assembly/service.rs:381`），不在本期放开。
- 函数签名扩展为接收父件行（或继承字段包），`AssemblyService::create_assembly`（`assembly/service.rs:379-412`）相应传入。

### 3.2 更新装配件时级联同步子件（`AssemblyService::update_assembly`，`assembly/service.rs:430`）

在 `AssemblyRepo::update_partial` 成功后、同一事务内级联：

```sql
UPDATE t_part
SET request_date = $x, applicant_name = $x, order_no = $x,
    system_delivery_date = $x, planned_delivery_date = $x,
    is_urgent = $x, note = $x, customer_id = $x,
    version = version + 1, updated_at = now(), updated_by = $uid
WHERE assembly_id = $aid AND deleted_at IS NULL
```

- 同步字段清单（无条件覆盖）：`request_date` / `applicant_name` / `order_no` / `system_delivery_date` / `planned_delivery_date` / `is_urgent` / `note` / `customer_id`。
- **排除** `actual_delivery_date`：它是 deliver 流程写入的产物，不属于「信息字段」。
- 只覆盖请求中实际变更的字段（与 `AssemblyUpdateRequest` 的可选语义对齐：None = 不动）；实现上按「父件更新后的当前行值」做覆盖，避免三态解析歧义。
- `customer_id` 变更时同样级联（子件 customer 必须始终等于父件）。

### 3.3 套数变更 → 子件数量缩放

- 触发点：`update_assembly` 中 `req.quantity` 有值且 ≠ 父件现值。
- 规则：`new_child_qty = round(child_qty × new_qty / old_qty)`，下限 1；同事务 UPDATE 每个子件的 `quantity`（version++）。
- 不追溯调整 `t_part_batch.quantity`（已拆分流转中的批次保持原量）；在文档中注明该语义。

### 3.4 DTO 与文档透传

- `AssemblyChildOut`（`assembly/dto.rs:65`）补字段：`applicant_name` / `request_date` / `order_no` / `system_delivery_date` / `is_urgent` / `note`；`AssemblyService::get_assembly`（`assembly/service.rs:247`）透传。
- 同步更新 `docs/api/assemblies/index.md`、`docs/api/assemblies/crud.md`（子件字段表 + 创建继承 / 更新级联 / 数量缩放行为说明）。

## 4. 方向 B：part ↔ batch 详细改动

### 4.1 消灭「无 batch 窗口」（初始批次）

现状：Rust 端 `create_part` / `batch_create_parts` / `insert_child_for_assembly` 均不建批次，新建工单直接 to-inspection 会 20109。

- 三个创建入口在同事务内 INSERT 初始批次：`batch_no=1`、`quantity=part.quantity`、`status='PENDING'`、`location='OFFICE'`（子件）或 NULL、`version=0`。
- **存量迁移**：新增 migration `<13位时间戳>_015_backfill_initial_part_batches.sql`：

```sql
INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location,
                          current_holder_id, next_process_id, placed_at,
                          delivery_note_id, has_been_repaired, version,
                          created_at, created_by, updated_at, updated_by)
SELECT <雪花id生成策略>, p.id, 1, p.quantity, p.status, p.location,
       p.current_holder_id, p.next_process_id, p.placed_at,
       p.delivery_note_id, p.has_been_repaired, 0,
       now(), NULL, now(), NULL
FROM t_part p
WHERE p.deleted_at IS NULL
  AND NOT EXISTS (SELECT 1 FROM t_part_batch b
                  WHERE b.part_id = p.id AND b.deleted_at IS NULL);
```

  - id 生成：迁移内用序列或 `nextval` 类方案（雪花为 App 侧生成，迁移中可用 `pg` 序列兜底，实施时定）。
  - 依赖 `uq_t_part_batch_part_no (part_id, batch_no)`，对已存在 batch 的 part 用 NOT EXISTS 跳过。

### 4.2 rollup 回调核心（新增 `PartService::sync_from_batch_change`）

**规则**（与 assembly rollup 同构，但 part 与 batch 状态词汇相同，直接取状态字符串）：

1. 拉该 part 全部活跃批次（`deleted_at IS NULL`）的 status；空集 → 不动（防御）。
2. 全部 CANCELLED → part `CANCELLED`。
3. 非 CANCELLED 全部 COMPLETED → part `COMPLETED`。
4. 否则取 min-progress 批次的状态作为 part 目标状态；progress 序复用 `assembly/statemachine.rs::part_status_progress`（PENDING=0 < PROGRAMMING=1 < IN_PROCESS/REPAIRING=2 < OUTSOURCE=3 < INSPECTION=4 < READY_TO_SHIP=5 < DELIVERED=6），**提升为共享函数**（移到 part 域 statemachine 或 shared，assembly 侧改引用）。
5. `location` / `current_holder_id` / `next_process_id` / `placed_at` 一并从「最慢批次」物化到 part。
6. 目标 == 当前 → NoChange；否则内部 UPDATE（派生写不走 OCC 冲突，仍 `version+1` + `updated_by`）。
7. part 状态实际变化 → 调 `AssemblyService::sync_from_part_change`，闭合 batch → part → assembly 链路；返回 `SyncOutcome` 供 handler 决定 WS 广播。

**改造点（去掉双写 / 特判，统一走 rollup）**：

| 调用点 | 现状 | 改为 |
|---|---|---|
| `inspection_core.rs` to_ship / to_process | 翻 batch + `count_other_inprocess_batches` 特判 + `mark_part_*` + assembly sync | 翻 batch + `sync_from_batch_change`（内部含 assembly sync） |
| `inspection_core.rs` to_inspection | 翻 batch + 翻 part + assembly sync | 同上 |
| `worker_scan.rs` RETURNED / INSPECTED | batch + part 双写 location/holder/status | 只写 batch + rollup |
| `worker_pool` take_one / admin_remove | CTE 双写 part.location/holder | batch 更新 + rollup |
| `delivery_note/service/lifecycle.rs` pickup | 批量翻 batch=DELIVERED，不碰 part | 翻 batch 后按 part_id 去重逐个 `sync_from_batch_change`（与 part deliver 语义统一收口） |
| `lifecycle.rs` cancel（保持 part 级） | 翻 part + 「最近一条」批次 | 级联取消**全部活跃批次** → part 置 CANCELLED（或直接走 rollup：批次全 CANCELLED → part CANCELLED） |

### 4.3 lifecycle 三端点改 batch 级（D3）

`deliver` / `complete` / `start-repair`（`part/service/lifecycle.rs`）：

- 请求 DTO 增加 `batch_id: String` + `version: i32`（锚 `t_part_batch.version`，与 inspection 三流一致）。
- 状态机守卫改读 **batch** 当前状态（READY_TO_SHIP→DELIVERED / DELIVERED→COMPLETED / IN_PROCESS→REPAIRING）。
- 事件日志（DELIVERED / COMPLETED / REPAIR_STARTED）`batch_id` / `quantity` 由 None 改为实际操作批次。
- `start_repair` 的 `has_been_repaired=true` 同时写 batch（现有）与 part（rollup 时同步或单独物化，实施时定）。
- ⚠️ **breaking change**：前端这三个端点的调用必须同步改造；`docs/api/parts/lifecycle.md` 更新。

### 4.4 读侧兼容性（D4 物化列的收益）

以下读侧**无需改动**：part 列表 status 筛选（`repo/part.rs:376/411`）、`PartOut`/`PartDetailOut` DTO、assembly rollup（`aggregate_children_status` 继续读 `t_part.status`）、shelf/worker 停用守卫（`shelf/repo.rs:294`、`worker/repo.rs:277`）。

前提：rollup 回调必须覆盖**所有**写 batch 状态的路径（§4.2 表格 + 存量迁移），实施时全局检索 `UPDATE t_part_batch` / `mark_batch_*` 调用点逐一核对。

### 4.5 暂不做的整理（后续项）

- `part/repo/batch.rs::split_batch_for_partial_pass` 与 `part_batch/repo.rs::split_batch` 的合并。
- `t_part.delivery_note_id` Rust 端只读不写的挂单链路补齐。

## 5. 测试计划

| 测试 | 内容 |
|---|---|
| rollup 纯函数单测（100%） | min-progress 取最慢批次、全 CANCELLED、全 COMPLETED、空集防御 |
| `tests/assembly_api.rs` 既有用例更新 | `create_with_pdf_creates_children_with_serial_pattern` / `create_assembly_default_not_null_columns` 补子件继承字段断言 |
| 新增：update_assembly 级联 | 改共享字段 → 子件全部覆盖；改 quantity → 子件等比缩放（含四舍五入/下限 1） |
| 新增：初始批次 | create_part / batch_create / 子件创建后 batch_no=1 存在且 quantity 一致 |
| 集成测试更新 | to-ship / to-process / to-inspection / worker-scan / pickup 的 part 状态断言改走 rollup 语义（如部分通过时 part 保持 INSPECTION 由 rollup 得出而非特判） |
| lifecycle batch 级 | deliver / complete / start-repair 收 batch_id+version 的 200 / 409 / 20103 路径 |
| 迁移回归 | 测试库跑 015 迁移：存量无 batch part 补建成功、已有 batch 的跳过 |

## 6. 实施顺序（worktree 内分 PR，每步独立可过 CI）

1. **PR-B1**：初始批次创建（3 入口）+ 015 存量迁移
2. **PR-B2**：rollup 核心 `sync_from_batch_change` + §4.2 各流转点改造 + `part_status_progress` 共享化
3. **PR-B3**：lifecycle 三端点 batch 级（含前端契约变更通知 + 文档）
4. **PR-A**：assembly 创建继承（§3.1）+ 更新级联（§3.2）+ 数量缩放（§3.3）+ DTO 透传（§3.4）
5. 每 PR 收尾：`./scripts/sqlx_prepare.sh`、`cargo clippy --all-targets`、`cargo test`、对应 `docs/api/` 同步

## 7. 风险与注意

- **015 迁移的雪花 id**：App 侧雪花在 SQL 迁移中不可用，实施时确定 id 生成策略（如专用序列），并与 `takeover.sql` 的交互核对（新库初始化顺序）。
- **pickup 与 deliver 收口**：delivery_note pickup 批量翻 batch 后必须逐 part rollup，否则 part.status 停留在 READY_TO_SHIP。
- **并发**：rollup 是事务内派生写；同一 part 多批次并发流转由 batch OCC 串行化，后提交的事务基于最新批次集重算，物化列最终一致。
- **前端 breaking**：lifecycle 三端点 DTO 变更需前端排期（见 `frontend/CLAUDE.md` 硬约束第 7 条确认调用侧）。
