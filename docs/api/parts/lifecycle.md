# part 域 — Lifecycle

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
> 状态机 / 错误码见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)
>
> 范围：本文件覆盖 4 个 lifecycle 端点（deliver / cancel / complete / start-repair） +
> 待编程一览列表（基于 `t_process.is_cnc` 列的新过滤规则）。CRUD / inspection 见
> [`./crud.md`](./crud.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录


- [POST /api/v2/parts/{part_id}/deliver](#post-apiv2partspart_iddeliver)
- [POST /api/v2/parts/{part_id}/cancel](#post-apiv2partspart_idcancel)
- [POST /api/v2/parts/{part_id}/complete](#post-apiv2partspart_idcomplete)
- [POST /api/v2/parts/{part_id}/start-repair](#post-apiv2partspart_idstart-repair)
- [POST /api/v2/parts/{part_id}/force-complete](#post-apiv2partspart_idforce-complete)（2026-09-30 新增：MANAGER 单角色强推逃生通道）
- [GET /api/v2/parts/pending-programming](#get-apiv2partspending-programming)（2026-09-29 新过滤规则）

---

### `POST /api/v2/parts/{part_id}/deliver`

权限: **Manager / Clerk**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：lifecycle 三端点（deliver /
> complete / start-repair）改为 **batch 级**，OCC 锚定 `t_part_batch.version`。
> 前端需先调 `GET /parts/by-serial/{serial_no}/part-batches` 拿 `batch_id` +
> `version`，再传入本端点。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id（雪花 i64）
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

业务流转：`READY_TO_SHIP → DELIVERED`（batch 级）；part 派生列由 PR-B2
rollup 自动回填；事件日志 `DELIVERED` 的 `batch_id` / `quantity` 来自
操作的批次。

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 流转后工单。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20117 — batch 当前状态非 READY_TO_SHIP（状态机白名单拒绝）
- 40901 — 乐观锁失败（batch version 冲突）

### `POST /api/v2/parts/{part_id}/cancel`

权限: **Manager / Clerk**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

**2026-10-01 订正**：cancel 级联的是该 part 下**全部非终态活跃批次**（不再只是
「最近一条 source-status 批次」），`status = 'COMPLETED' | 'CANCELLED'` 的批次不在
级联范围内。作废同时清空 `t_part.serial_no`（作废即退役，序列号**不**归档）；
父装配件 `t_assembly` 会被派生追平（唯一子件作废 → 父件 CANCELLED，父件序列号
同步释放）。

> **不变式（2026-10-01 review 第 1 轮 B1）**：`t_part.status` 由本端点的主操作
> 写下，级联批次的派生**不得**覆盖它。若该 part 存在已 COMPLETED 的批次，
> min-progress 会算出 COMPLETED —— 实现靠两道闸拦住：bulk 入口的
> `PartDerivation::KeepPartTerminalAsIs`（显式跳过 part 写）+ `update_part_rollup`
> 的 `status NOT IN ('COMPLETED','CANCELLED')` 终态守卫（SQL 层兜底）。
> 回归测试：`tests/part/lifecycle.rs::cancel_part_is_not_overwritten_by_rollup_completed`。
> 副作用：已终态的 part 不再被 rollup / admin 对账改写（见
> [`../admin.md`](../admin.md)）。

错误码：

- 20101 — part 不存在 / 软删
- 20103 — 当前状态不在 cancel 白名单（COMPLETED / CANCELLED 等终态）
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED
- 21420 — part 已挂送货单，禁 cancel
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/complete`

权限: **Manager / Clerk**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：收 `batch_id` + `version`，锚定
> `t_part_batch.version`（与 inspection 三流一致）。状态机守卫读 batch 当前
> 状态 `DELIVERED → COMPLETED`；part 终态后 `serial_no` 被清空（序列号已
> 转交送货单）。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — part 不存在 / 软删
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20116 — batch 当前状态非 DELIVERED（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/start-repair`

权限: **Manager / Clerk / Inspector**

> ⚠️ **2026-09-11 BREAKING CHANGE (PR-B3)**：收 `batch_id` + `version`，锚定
> `t_part_batch.version`。
>
> **2026-10-01 BREAKING CHANGE**：REPAIRING 降级为布尔标记列
> `t_part_batch.is_repairing`（migration 005/006）。本端点**不再发生 status
> 迁移** —— 守卫条件由「状态机 `IN_PROCESS → REPAIRING`」改为
> **「`status='IN_PROCESS'` 且 `is_repairing = false`」**，命中后只把
> `is_repairing` 置 `true`（`status` 保持 `IN_PROCESS`）。
>
> - 重复起修（`is_repairing` 已为 `true`）→ 20118 `BIZ_PART_REPAIR_NOT_TRIGGERED`。
> - `t_part.status` 恒为 `IN_PROCESS`（返修仍在生产中，progress 与原 REPAIRING
>   同档 2），**不再出现 `'REPAIRING'`**。
>
> 2026-09-16 PR-2（migration 027）：`has_been_repaired` 列已从 `t_part` 与
> `t_part_batch` 双删 —— 拆批后无法确定是哪一个批次返修，列语义失真整体废弃。
> 返修事实改由 `t_part_batch.is_repairing` 列 + `t_part_event.event_type=
> 'REPAIR_STARTED'` 事件日志共同追溯。事件 `from_status` / `to_status` 均写
> 真实值 `IN_PROCESS`（status 未变，变的是标记位）。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "reason": "string (可选)",  // 优先作为事件 note
  "note": "string (可选)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。

错误码：

- 20101 — part 不存在 / 软删
- 20109 — batch 不存在 / 不属于该 part
- 20115 — part 已 CANCELLED
- 20118 — batch 当前状态非 IN_PROCESS（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/force-complete`（2026-09-30 新增）

权限: **Manager 单角色**（明确不下放 Clerk —— 强改逃生通道）

> ⚠️ **强改语义**：
> - **完全绕状态机**：非 `CANCELLED` / 非 `COMPLETED` 状态可被强推到 `COMPLETED`。
> - **不走 OCC**：force-complete 是逃生通道，依赖 SQL 行锁串行化（`t_part_batch` 行锁
>   + `t_part` 行锁），不收 `version`、不要求 caller 侧 batch_id 选择。
> - **批次处理**：单 SQL 强推该 part 下所有活跃批次（非 `CANCELLED`、非软删）→
>   `COMPLETED`，复用 `sync_from_batch_change` rollup 让 `compute_part_target`
>   自动派生 `part.status='COMPLETED'`。
> - **终态清理**：复用现有 `clear_part_serial_no_when_completed` 清空 `serial_no`。
> - **事件日志**：`t_part_event.event_type='FORCE_COMPLETED'`（区别常规 `COMPLETED`），
>   `note` 自动加 `[FORCE]` 前缀（即便用户未传 note 也会写入 `[FORCE] ` 空字符串）以便审计追溯。
> - **WS 广播**：`PART_FORCE_COMPLETED`（区别 `PART_COMPLETED`），前端订阅 dashboard
>   可监听。

Request：

```json
{
  "note": "string (可选；将自动加 [FORCE] 前缀写入事件日志)"
}
```

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 强推后的工单。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED（终态不可被强推，语义对称 20123）
- 20123 — part 已 COMPLETED（幂等拒绝，避免重复强推副作用）
- 40300 — 无权限（仅 MANAGER 单角色）

---

## Lifecycle 专属 DTO

### DeliverRequest / CompleteRequest / StartRepairRequest 字段（PR-B3 batch 级）

三者均含必填 `batch_id` + `version`（锚 `t_part_batch.version`）+ 可选
`note` / `reason`（≤ 500 字符建议）。事件日志 `batch_id` / `quantity` 来自
操作的批次；cancel 与 start-repair 优先取 `reason` 作为事件 note。

### CancelRequest 字段（保持 part 级）

仅含可选 `reason` / `note`（cancel 走 part 级 + 级联取消全部活跃批次，详见
[重构方案 §4.2](../../refactor-part-assembly-batch.md#42-rollup-回调核心-新增-partservicesync_from_batch_change)）。

### ForceCompleteRequest 字段（2026-09-30 新增，MANAGER 单角色强推逃生通道）

仅含可选 `note`（≤ 500 字符建议；服务端自动加 `[FORCE] ` 前缀写入事件日志）。
不收 `batch_id` / `version`（绕 OCC）；service 层收尾时会复用现有
`complete` 路径的 `clear_part_serial_no_when_completed` + `sync_from_batch_change`
rollup 让 `part.status='COMPLETED'` 自动落地。

---

### `POST /api/v2/parts/{part_id}/pick-up`

权限: **Worker**（自己的 part 拣货；其它人需 Manager）

> 2026-09-22 起 P3 pickup 端点（替换原 `pick-up-batch` 独立端点为 batch-级 OCC 集成）。
> 用于 worker 主动拣走自己的在制件；`version` 锚定 `t_part_batch.version`。

Request：

```json
{
  "batch_id": 1234567890,    // 必填；操作的目标 batch id
  "version": 0,               // 必填；batch.version（OCC）
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20119 / 40901。

### `POST /api/v2/parts/{part_id}/place-on-shelf`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 lifecycle 端点（PR-B3 batch 级）。工人拣走件入库上货架。
> 状态机：`IN_PROCESS → ON_SHELF`。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "shelf_id": 42,            // 必填；货架 id
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20120 / 40400 / 40901。

### `POST /api/v2/parts/{part_id}/complete-repair`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 repair 收尾。`version` 锚 `t_part_batch.version`。
>
> **2026-10-01 BREAKING CHANGE**：守卫条件由「状态机 `REPAIRING → …`」改为
> **「`is_repairing = true`（确实在返修中）」**（REPAIRING 已降级为标记列）。
> 源状态非 `IN_PROCESS` 或 `is_repairing = false` → 20118
> `BIZ_PART_REPAIR_NOT_TRIGGERED`。
> 去向由 `shelf.zone` 决定：`PRODUCTION` → `IN_PROCESS`（落回生产架、重新入池
> 并写 `next_process_id`）/ `INSPECTION` → `INSPECTION`（送检区、出池）；
> 两条路径都把 `is_repairing` 清回 `false`。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20121 / 40901。

### `POST /api/v2/parts/{part_id}/repair-dispatch`

权限: **Manager**

> 2026-09-22 起 P3 repair 起始（与 `start-repair` 类似但用于派工而非自检）。
> 入口状态：IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED；去向由
> `shelf.zone` 决定（PRODUCTION → `IN_PROCESS` / INSPECTION → `INSPECTION`）。
>
> **2026-10-01**：一步式下发（一次调用完成「起修 + 到位」），故不写
> `is_repairing = true` 再清，而是**直接保持 `false`**。事件仍记两条
> （`REPAIR_STARTED` + `REPAIR_COMPLETED`），状态字段写真实值：
> `S → IN_PROCESS`（起修）`→ T`（到位）。

Request：

```json
{
  "batch_id": 1234567890,
  "version": 0,
  "worker_id": 42,           // 必填；被派工工人
  "reason": "string (可选)",
  "note": "string (可选)"
}
```

Response 200 `data`：`PartOut`。

错误码：20101 / 20109 / 20118 / 40901。

### `GET /api/v2/parts/by-worker/{worker_id}`

权限: **已登录**

> 2026-09-22 起 P3 list by worker。返回该 worker 名下所有活跃 part 列表。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 工人雪花 ID |

Query：`status?` / `limit?` / `offset?`（默认 50 / 0）。

Response 200 `data`：`{ items: [PartOut], total, limit, offset }`。

### `GET /api/v2/parts/outsource-sendable`

权限: **Manager / Clerk**

> 2026-09-22 起 P3 list。返回可发送外协的 part 列表（状态 = READY_TO_SHIP 且无 active 外协 quote）。

Query：`process_id?` / `limit?` / `offset?`。

Response 200 `data`：`{ items: [PartOut], total, limit, offset }`。

### `GET /api/v2/parts/repair-batches`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 list。返回所有 `status='DELIVERED'` 的 batch 汇总
> （历史文档误写为 REPAIRING，2026-10-01 订正：REPAIRING 已降级为标记列）。

Query：`worker_id?` / `process_id?` / `limit?` / `offset?`。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

### `GET /api/v2/parts/repairing-batches`

权限: **Manager / Inspector**

> 2026-09-22 起 P3 list（与 `repair-batches` 同形状，区别：判据不同）。
>
> **2026-10-01 BREAKING CHANGE**：判据由 `status = 'REPAIRING'` 改为
> **`t_part_batch.is_repairing = true`**。返回项的 `status` 字段恒为
> `IN_PROCESS`（REPAIRING 已不是任何列会取到的值）。
>
> 2026-10-01 review 第 1 轮 M5 补齐：返修标记已随
> `BatchOut`（= `InspectionBatchListItemOut`）的**新字段 `is_repairing: bool`**
> 一起返回。此前「只有 status」的端点让前端彻底失去「返修中」信号 ——
> 改造前靠 `status === 'REPAIRING'` 判定，改造后任何接口都拿不到该值。
> 影响端点：`GET /parts/repairing-batches`、`GET /parts/repair-batches`、
> `GET /parts/inspection-batches`，以及 `GET /parts/{id}/batches`
> （`PartBatchListItemOut` 同样新增 `is_repairing: bool`）。

Response 200 `data`：`{ items: [BatchOut], total, limit, offset }`。

---

> **2026-09-23 PR12 同步说明**：本节 8 个端点（pick-up / place-on-shelf / complete-repair / repair-dispatch / 4 个 GET 列表）原 docs/api/parts/lifecycle.md 未覆盖，本次按 PR11 drift 报告补齐（[docs/api/DRIFT_REPORT.md §2.2](../DRIFT_REPORT.md#22-partscrud-lifecycleinspectionmd高优先级--大量端点缺失)）。

---

### `GET /api/v2/parts/pending-programming`

权限: **Manager / Clerk / CncProgrammer / Inspector**

> **2026-09-29 BREAKING CHANGE**（CNC 重构 5 任务之一）：编程流转入口改造。
>
> 旧实现：`status = 'PROGRAMMING'` 一览 + `POST /parts/{id}/send-to-programming` /
> `POST /parts/{id}/recall-to-programming` 两个端点。新实现：**取消状态机
> `PROGRAMMING` 进入路径**（`PENDING → PROGRAMMING` 与 `IN_PROCESS → PROGRAMMING`
> 两条迁移已从 `part/statemachine.rs::can_transition_to` 删除），待编程一览改为
> 基于 `t_process.is_cnc` 列的链上 / 货架过滤，配合前端 Tab 切换
> `has_cnc_program?: bool`。
>
> 编程员现在通过工艺链 + CNC step 直接进入生产流；`POST /parts/{id}/send-to-programming`
> 与 `POST /parts/{id}/recall-to-programming` 端点已下线（返回 404）。
> `POST /parts/{id}/release-from-programming` 仍保留（PROGRAMMING → IN_PROCESS）。

Query：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keyword` | string? | — | 模糊匹配 `name` / `drawing_no` / `serial_no` |
| `sort_by` | string? | — | 白名单 `CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它退化为 `PLANNED_DELIVERY_DATE` |
| `sort_dir` | string? | — | `ASC` / `DESC`（缺省 `ASC`） |
| `limit` | int? | — | 1..=500（缺省 50） |
| `offset` | int? | — | ≥ 0（缺省 0） |
| `has_cnc_program` | bool? | — | **2026-09-29 新增**：Tab 切换。`true` 仅已上传 G_CODE；`false` 仅未上传；缺省全部 |

**新过滤规则**（取代旧 `status = 'PROGRAMMING'`）：

```sql
status IN ('PENDING','IN_PROCESS','PROGRAMMING')  -- 历史 PROGRAMMING 状态仍允许消化
AND (
  -- 条件 A：工艺链上含 CNC step
  EXISTS (
    SELECT 1 FROM t_process_chain_step s
    JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL
    WHERE s.chain_id = p.process_chain_id
      AND s.deleted_at IS NULL
      AND pr.is_cnc = TRUE
  )
  -- 条件 B：当前 active 批次所在货架关联 CNC 工序
  OR EXISTS (
    SELECT 1 FROM t_part_batch pb
    JOIN t_shelf_process sp
      ON sp.shelf_id = pb.current_holder_id AND sp.deleted_at IS NULL
    JOIN t_process pr
      ON pr.id = sp.process_id AND pr.deleted_at IS NULL
    WHERE pb.part_id = p.id
      AND pb.deleted_at IS NULL
      AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING')
      AND pr.is_cnc = TRUE
  )
)
```

外加可选 `has_cnc_program` 过滤：`EXISTS t_part_file.kind = 'G_CODE'`。

Response 200 `data`：[`PartListOut`](./index.md#partlistout-字段)。**2026-09-29 新增**：
[`PartListItem`](./index.md#partlistitem-字段) 含 `has_cnc_program: bool` 派生字段
（由 repo EXISTS 子查询填充）。

错误码：40001（limit/offset 越界）、40300（角色不符）、50001（DB）。

#### 业务场景

- **Tab = 待编程**（`has_cnc_program=false`）：未上传 G_CODE 的 CNC 工单（链上有 CNC
  step 或当前批次在 CNC 货架 + 未上传程序）。编程员先在 list 内点"上传 G_CODE"→
  后端走 `POST /part-files/upload-intents` + 直传 COS + `confirm`。
- **Tab = 已编程**（`has_cnc_program=true`）：已上传 G_CODE 的 CNC 工单，编程员
  确认无误后通知车间 release（走 `POST /parts/{id}/release-from-programming`）。
- **Tab = 全部**（`has_cnc_program` 缺省）：所有 CNC 相关工单（含历史 PROGRAMMING
  状态可消化的批次）。

---

> **2026-09-29 同步说明**：本节 `GET /parts/pending-programming` 端点 + 新 query 参数
> `has_cnc_program` + `PartListItem.has_cnc_program` 字段新增；旧端点
> `POST /parts/{id}/send-to-programming` 与 `POST /parts/{id}/recall-to-programming`
> 下线（返回 404），state machine 同步删除
> `PENDING → PROGRAMMING` 与 `IN_PROCESS → PROGRAMMING` 两条入口迁移。
