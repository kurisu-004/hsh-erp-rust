# delivery-notes / 草稿变更

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · **`drafts.md`** · [`workflow.md`](./workflow.md) · [`print.md`](./print.md)

## 本文件目录

1. [POST /api/v2/delivery-notes/scan (P3 扫码建单)](#post-apiv2delivery-notesscan--p3-扫码建单)
2. [POST /api/v2/delivery-notes](#post-apiv2delivery-notes)
3. [POST /api/v2/delivery-notes/{id}/update](#post-apiv2delivery-notesidupdate)
4. [POST /api/v2/delivery-notes/{id}/add-parts](#post-apiv2delivery-notesidadd-parts)
5. [POST /api/v2/delivery-notes/{id}/remove-parts](#post-apiv2delivery-notesidremove-parts)
6. [POST /api/v2/delivery-notes/{id}/attach-batches](#post-apiv2delivery-notesidattach-batches--2026-08-31-新增)
7. [POST /api/v2/delivery-notes/{id}/soft-delete](#post-apiv2delivery-notesidsoft-delete)
8. [设计历史](#设计历史)

---

### `POST /api/v2/delivery-notes/scan`  （P3 扫码建单）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后 1..=64 字符；扫码载荷 |

Response 200 `data`：`ScanDeliveryOut`

> **批次状态 3 分组（路线 B，2026-08-27；C 组行为 2026-08-31 修订）**——扫码时按 `t_part_batch.status` 把命中的批次分成三组：
>
> | 组 | 状态 | 行为 |
> |---|---|---|
> | **A 可直接 attach** | `READY_TO_SHIP` / `INSPECTION`（且 `delivery_note_id IS NULL`） | 装配件全 A 时自动入 `added_batches[]`；混合场景下随 `unresolved_targets[i].attachable_batches[]` 携带，前端弹窗勾选后调 [`POST /{id}/attach-batches`](#post-apiv2delivery-notesidattach-batches--2026-08-31-新增) 显式 attach |
> | **B 候选→一键送检** | `PENDING` / `PROGRAMMING` / `IN_PROCESS`（未被工人持有）/ `REPAIRING` | 进 `unresolved_targets[i].available_batches[]`，前端调 `to-inspection` 后 re-scan |
> | **C 静默过滤 / 仅全 C 触发 21421** | `DELIVERED` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` / `IN_PROCESS`（工人持有） | 混合场景下静默过滤，不入任何 Vec；**仅当某 target 的全部 batch 都为 C 组时**才硬报错 `21421` |
>
> 「工人持有」指 `t_part_batch.location = 'WORKER'`（2026-08-28 修正：原按 `current_holder_id IS NOT NULL` 判定，但该列多态——放货架时存 `t_shelf.id`，会把货架上的工件误判为工人持有）。
>
> **混合场景（C 组过滤，2026-08-31 修订）**：原行为「任一 C 组 → 21421」过于严格——装配件场景下工人临时拿走某个零件后整单被拒，与业务预期不符。新规则：
>
> - **散件**：单 target；若该 target 加载到的全部 batch 都为 C 组 → 21421；否则 C 组静默过滤、A/B 正常分类。
> - **装配件**：多 target；**任一** target 的全部 batch 都为 C 组 → 21421；否则各 target 独立过滤 C 组，A/B 正常分类。
>
> 21421 触发条件表（按 target 维度聚合）：
>
> | 场景 | 散件结果 | 装配件结果 |
> |---|---|---|
> | 全 target 全 C | 21421 | 21421 |
> | 部分 target 全 C、其余有 A/B | 仅过滤全 C 的 target → 200 | 任一全 C → 21421；过滤后返回 200 |
> | 部分 target 部分 C（含 A/B） | C 组过滤 → 200 | C 组过滤 → 200 |
> | 无 C 组 | 走常规 A/B 分类 | 走常规 A/B 分类 |
>
> 实现：`src/modules/delivery_note/service/scan.rs::has_fully_invalid_target`（单测在 `c_group_distribution_tests`）。

```jsonc
{
  "outcome": "ADDED" | "ALREADY_PRESENT" | "CANDIDATES_AVAILABLE" | "PARTIAL_ADDED",
  "resolved": { "kind": "PART", "id": "…", "serial_no": "…", "drawing_no": "…", "name": "…" },
  "note": { /* ScanDeliveryNoteSummaryDto */ },
  "added_batches": [ /* AddedBatchDto */ ],
  "unresolved_targets": [ /* UnresolvedTargetDto */ ]   // 字段缺省即表示无（serde skip_none）
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `outcome` | string | `ADDED` / `ALREADY_PRESENT` / `CANDIDATES_AVAILABLE` / `PARTIAL_ADDED` |
| `resolved` | object | 解析结果，见下 |
| `note` | object | 命中/新建的送货单概要，见下 |
| `added_batches` | [object] | 本次新加入的批次（A 组）；无则为 `[]` |
| `unresolved_targets` | [object]? | B 组候选清单；无则**字段不出现**（`skip_serializing_if`） |

4 场景 → outcome 映射（**2026-08-31 修订**：A 组不再由 scan 自动 attach）：

| 场景 | outcome | `added_batches` | `unresolved_targets` |
|---|---|---|---|
| ① 散件全 A 组 | `ADDED` | ✓（自动 attach） | — |
| ② 散件仅 B 组 | `CANDIDATES_AVAILABLE` | `[]` | ✓（1 个元素；`attachable_batches` 为空） |
| ③ 装配件全 A 组 | `ADDED` | ✓（自动 attach） | — |
| ④ 装配件 A+B 混合 | `PARTIAL_ADDED` | `[]`（**不再自动 attach**） | ✓（每个还有 A 或 B 的子件 1 个元素，`attachable_batches` 含该子件的 A 组） |
| 全部批次已挂本单 | `ALREADY_PRESENT` | `[]` | — |

`resolved`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `kind` | string | `"PART"` / `"ASSEMBLY"` |
| `id` | string (i64) | part.id 或 assembly.id |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |

`note`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `delivery_note_no` | string | |
| `version` | i32 | |
| `status` | string | DRAFT / SUBMITTED / PICKED_UP / ... |
| `scope_label` | string | 范围展示文案 |
| `customer_path` | string | L1 > L2 > ... |
| `line_count` | usize | |
| `recent_items` | [RecentItem] | 最近加入的批次（最多 8 条），见下 |

`note.recent_items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `part_id` | string (i64) | |
| `serial_no` | string? | |
| `drawing_no` | string | |
| `name` | string | |
| `order_no` | string? | |

`added_batches[]`（`AddedBatchDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `part_id` | string (i64) | 跨子件场景需保留（装配件一次挂多个 part 的批次） |
| `serial_no` | string | 来自 `part.serial_no` |
| `quantity` | i32 | |

`unresolved_targets[]`（`UnresolvedTargetDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 未就绪工单 ID |
| `serial_no` | string | |
| `drawing_no` | string | |
| `name` | string | |
| `available_batches` | [object] | 该工单的 B 组候选批次，见下 |
| `attachable_batches` | [object] | **2026-08-31 新增**。该工单的 A 组候选批次（INSPECTION / READY_TO_SHIP）；前端弹窗勾选后转发到 [`POST /{id}/attach-batches`](#post-apiv2delivery-notesidattach-batches--2026-08-31-新增) 显式 attach。`Added` / `AlreadyPresent` 时为空 `[]`；`CandidatesAvailable` / `PartialAdded` 时按 part 维度携带 |

`unresolved_targets[i].available_batches[]`（`AvailableBatchDto`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `version` | i32 | 批次乐观锁版本；转发给 `batch-to-inspection` / `batch-to-ship` 的 `items[]` 时必填 |
| `quantity` | i32 | |
| `status` | string | `BatchStatusDto` 枚举，见下 |

`unresolved_targets[i].attachable_batches[]`（`AttachableBatchDto`，**2026-08-31 新增**）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | |
| `version` | i32 | 批次乐观锁版本；转发到 [`POST /{id}/attach-batches`](#post-apiv2delivery-notesidattach-batches--2026-08-31-新增) 的 `batches[]` 时必填 |
| `quantity` | i32 | |
| `status` | string | `BatchStatusDto` 枚举，**仅 INSPECTION / READY_TO_SHIP**（A 组定义） |

> part 级信息（`serial_no` / `drawing_no` / `name`）只在外层 `UnresolvedTargetDto` 上，`available_batches[]` / `attachable_batches[]` 内不重复。
>
> B 候选转一键送检：前端把 `available_batches[]` 逐条映射为 `{ batch_id, version, quantity? }` 塞进 `POST /api/v2/parts/batch-to-inspection` 的 `items[]`。`version` 不符 → 该 item 落 `failed[].code = 40901`。
>
> A 组转 attach：前端把 `attachable_batches[]` 逐条映射为 `{ batch_id, version }` 塞进 `POST /{id}/attach-batches` 的 `batches[]`；部分失败 → 200 + `conflicts[]`（详见下文）。

`BatchStatusDto`（`t_part_batch.status` 强类型投影，序列化沿用 DB 列值）：

`PENDING` / `PROGRAMMING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` / `REPAIRING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED`

> 前端可基于 `unresolved_targets[i].available_batches[].status` 区分触发端点：
> - `status ∈ {PENDING, PROGRAMMING, IN_PROCESS, REPAIRING}` → 触发一键送检（`to-inspection` / `POST /parts/batch-scan-inspect`），成功后 re-scan 同一 code 完成入单

错误码：

| 错误码 | 常量 | 说明 |
|---|---|---|
| `20104` | `BIZ_INVALID_VALUE` | code 空白/空，或 trim 后长度不在 1..=64 |
| `21407` | `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` | 单内混客户 |
| `21416` | `BIZ_DELIVERY_NOTE_SCOPE_MISMATCH` | 零件分类与送货单范围不符 |
| `21417` | `BIZ_DELIVERY_SCAN_UNKNOWN_CODE` | 扫码无法识别（既非 part 也非 assembly 序列号） |
| `21406` | `BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED` | 所有目标批次都已挂在其它 active 单上（无可选 → 硬错误） |
| `21421` | `BIZ_DELIVERY_BATCH_STATE_INVALID` | scan 路径 C 组短路（2026-08-31 修订：仅当任一 target 加载到的全部 batch 都为 C 组时触发；混合场景下 C 组静默过滤） |
| `21405` | `BIZ_DELIVERY_NOTE_PART_NOT_READY` | **已不再由 scan 触发**；保留给 `add-parts` 等端点 |
| `21418` | `BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY` | **已不再由 scan 触发**（场景 ④ 改走 200 `PARTIAL_ADDED`）；保留给 `add-parts` 等端点 |
| `40300` | `FORBIDDEN` | 角色不符 |
| `40001` | `VALIDATION_ERROR` | 角色不足 |

### `POST /api/v2/delivery-notes`

权限: 已登录（**service 层 enforce 角色**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | |
| `delivery_date` | date? | — | |
| `items` | [AddItem] | — | 可同时入单（与 `/add-parts` 等价） |
| `note` | string? | — | 备注 |

`items[]`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | |
| `quantity` | i32? | — | None = 整批；Some(n) 且 n < batch.quantity → 拆分 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 20102 BIZ_CUSTOMER_NOT_FOUND
- 21405 BIZ_DELIVERY_NOTE_PART_NOT_READY
- 21406 BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY（data.failures）

### `POST /api/v2/delivery-notes/{id}/update`

权限: 已登录（**DRAFT / SUBMITTED** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |
| `delivery_date` | date? | — | None = 不改 |
| `note` | string? | — | None = 不改 |

Response 200 `data`：[`DeliveryNoteOut`](./index.md#deliverynoteout-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION — 状态不允许修改
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/add-parts`

权限: 已登录（**DRAFT** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [AddItem] | ✓ | 同 `POST /` 的 items |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21405 BIZ_DELIVERY_NOTE_PART_NOT_READY
- 21406 BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED
- 21412 BIZ_DELIVERY_NOTE_PARTS_LOCKED — SUBMITTED/PICKED_UP 后禁止
- 21416 BIZ_DELIVERY_NOTE_SCOPE_MISMATCH
- 21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY（data.failures）
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/remove-parts`

权限: 已登录（**DRAFT** 才允许）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_ids` | [string (i64)] | ✓ | |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteDetail`](./index.md#deliverynotedetail-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21412 BIZ_DELIVERY_NOTE_PARTS_LOCKED
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/attach-batches`  （2026-08-31 新增）

权限: **Manager / Clerk**（比 `add-parts` 更严格：本端点仅在 DRAFT 草稿做显式 attach，不走扫码 / 工人路径，故不放宽到 Inspector）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batches` | [AttachItem] | ✓ | **最多 200 项**；超出 → 400 `BIZ_INVALID_VALUE` |

`batches[]`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | 雪花 ID 字符串；须为 A 组（INSPECTION / READY_TO_SHIP）且 `delivery_note_id IS NULL` |
| `version` | i32 | ✓ | 乐观锁；与 `t_part_batch.version` 不一致 → 该 item 进 `conflicts[]`，reason=`VERSION_CONFLICT` |

Response 200 `data`：`AttachBatchesOut`

```jsonc
{
  "attached": 1,        // 成功 attach 的项数
  "conflicts": [        // 失败项明细；空数组表示全部成功
    { "batch_id": "string", "reason": "VERSION_CONFLICT" }
    // 或 "BATCH_NOT_FOUND" / "ALREADY_ATTACHED" / "INVALID_STATE:<STATUS>"
  ]
}
```

**部分失败也始终返回 200**，前端按 `conflicts[]` 列表做差异处理：

| 情况 | `attached` | `conflicts` |
|---|---|---|
| 全部成功 | `n` | `[]` |
| 部分失败 | `>0` | 列出失败项 |
| 全部失败 | `0` | 列出失败项 |

`conflicts[].reason` 枚举（稳定的 SCREAMING_SNAKE_CASE 字符串，便于前端 i18n / 分类）：

| reason | 触发条件 |
|---|---|
| `BATCH_NOT_FOUND` | `batch_id` 不存在 / 已软删 |
| `ALREADY_ATTACHED` | `t_part_batch.delivery_note_id IS NOT NULL`（已挂在某张 active 单上，包括本单） |
| `INVALID_STATE:<STATUS>` | 批次当前 `status` 不在 A 组（INSPECTION / READY_TO_SHIP）；尖括号内为原 status 值（如 `INVALID_STATE:PENDING` / `INVALID_STATE:DELIVERED`） |
| `VERSION_CONFLICT` | `req.version` 与 `t_part_batch.version` 不一致（OCC 失败，`attach_to_note` 0 行） |

**与 `scan` 端点的联动**：

- [`POST /scan`](#post-apiv2delivery-notesscan--p3-扫码建单) 在 `CANDIDATES_AVAILABLE` / `PARTIAL_ADDED` 时返回的 `unresolved_targets[i].attachable_batches[]` 是本端点的主要调用来源——前端弹窗勾选若干 A 组后把 `{ batch_id, version }` 转发到这里。
- `scan` 在 outcome=`ADDED`（全 A 无 B）时仍自动 attach；本端点仅用于**显式补挂**（弹窗确认 / 二次决策）。
- 不调用本端点、不传 `attachable_batches` 给前端，等同于「放弃 A 组」——本单不会自动挂这些批次，前端可继续 re-scan。

错误码（硬错误，仅 note 自身状态 / 入参非法）：

| 错误码 | 常量 | HTTP | 说明 |
|---|---|---|---|
| `20104` | `BIZ_INVALID_VALUE` | 400 | `batches` 数组 > 200 项 |
| `21401` | `BIZ_DELIVERY_NOTE_NOT_FOUND` | 404 | `note_id` 不存在 |
| `21403` | `BIZ_DELIVERY_NOTE_NOT_DRAFT` | **409**（由 `biz_with_status` 强制） | note 状态非 DRAFT |
| `40300` | `FORBIDDEN` | 403 | 角色不符（非 Manager / Clerk） |

WS 事件：commit 后广播 `DELIVERY_NOTE_BATCHES_ATTACHED`（payload：`{ delivery_note_id, attached_count, conflict_count }`；详见 [`../../websocket.md`](../../websocket.md)）；监听端用 `conflict_count > 0` 判断是否有失败项。

完整示例：

```bash
curl -X POST http://localhost:8080/api/v2/delivery-notes/123/attach-batches \
  -H "Authorization: Bearer <jwt>" \
  -H "Content-Type: application/json" \
  -d '{
    "batches": [
      { "batch_id": "9001", "version": 5 },
      { "batch_id": "9002", "version": 2 }
    ]
  }'

# 200 OK：
# { "code": 0, "message": "ok",
#   "data": { "attached": 1,
#             "conflicts": [
#               { "batch_id": "9002", "reason": "VERSION_CONFLICT" }
#             ] } }
```

### `POST /api/v2/delivery-notes/{id}/soft-delete`

权限: 已登录（**仅 DRAFT**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`: `null`

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21403 BIZ_DELIVERY_NOTE_NOT_DRAFT — 非 DRAFT 不能软删
- 40901 VERSION_CONFLICT

---

## 设计历史

### 路线 B 修复（2026-08-27 ~ 2026-08-31）

> 原设计文档 `scan-route-b-fix.md` 已折叠进本节（2026-08-31）。如需 git 历史，参见 `git log -- docs/api/delivery-notes/scan-route-b-fix.md`（HEAD 中最后一次修订为 169f442）。

#### Context

`POST /api/v2/delivery-notes/scan` 原为「原子失败」思路——散件 / 装配件扫描失败直接抛 `21405` / `21418`，前端要弹错误 toast → 弹窗驱动一键送检 → 关闭弹窗 → 重新扫码，工作流断裂。路线 B 把它改造成「软成功 + 候选批次清单」：失败路径走 `200 OK + CANDIDATES_AVAILABLE / PARTIAL_ADDED` + `unresolved_targets[]`。

#### 5 类分组（A/B/D + 2 个边界）

按业务语义把 `t_part_batch.status`（10 值枚举，详见 `src/modules/part/statemachine.rs`）分成 3 组 + 2 个分组边界条件：

| 组 | 状态 | 行为 |
|---|---|---|
| **A 可直接 attach** | `READY_TO_SHIP`, `INSPECTION`（且 `delivery_note_id IS NULL`） | 进 `added_batches[]` 或 `unresolved_targets[i].attachable_batches[]` |
| **B 候选→一键送检** | `PENDING`, `PROGRAMMING`, `IN_PROCESS`（非工人持有）, `REPAIRING` | 进 `unresolved_targets[i].available_batches[]`；前端调 `to-inspection` 后 re-scan |
| **C 静默过滤 / 仅全 C 触发 21421** | `DELIVERED`, `OUTSOURCE`, `COMPLETED`, `CANCELLED`, `IN_PROCESS`（工人持有） | 混合场景下静默过滤；**任一 target 全 C 才报 21421** |

**分组边界条件**：

- `IN_PROCESS` 是否「被工人持有」——以 `t_part_batch.location = 'WORKER'` 判定（**2026-08-28 修正**：`current_holder_id` 是多态列——放货架时存 `t_shelf.id`，工人取件时才存 worker id，不能用作判据；按 `current_holder_id IS NOT NULL` 判定会把货架上的工件误判为工人持有）。
- B 组送检需通过状态机白名单：原 `REPAIRING → INSPECTION` 不在白名单，本次新增（详见 `src/modules/part/statemachine.rs`）。

#### 错误码迁移

| 错误码 | 路线 B 前的语义 | 路线 B 后的语义 |
|---|---|---|
| `21405 BIZ_DELIVERY_NOTE_PART_NOT_READY` | 散件扫描失败 / 装配件部分失败 | **保留**：scan 路径不再触发；改由 `add_parts` 等端点使用 |
| `21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY` | 装配件整套失败 | **保留**：scan 路径不再触发；改由 `add_parts` 等端点使用 |
| `21421 BIZ_DELIVERY_BATCH_STATE_INVALID` | （不存在） | **新增**：C 组短路专用；**2026-08-31 修订**——仅当某 target 加载到的全部 batch 都为 C 组时触发；混合场景下 C 组静默过滤 |

#### 改动时间线

| 日期 | 改动 |
|---|---|
| 2026-08-26 | 路线 B 初始设计稿（`scan-route-b-fix.md` 创建） |
| 2026-08-27 | DTO 精简 + 错误码 21421 重写（commit `169f442`） |
| 2026-08-28 | `IN_PROCESS` 持有判定从 `current_holder_id IS NOT NULL` 改为 `location = 'WORKER'`（commit `81bb858`） |
| 2026-08-31 | **C 组语义修订**：任一 C → 21421 改为「任一 target 全 C 才 21421」，混合场景静默过滤；A 组不再由 scan 自动 attach（混合场景下），改由 `POST /{id}/attach-batches` 弹窗显式提交；新增 `UnresolvedTargetDto.attachable_batches` 字段与 `POST /{id}/attach-batches` 端点；WS 新增 `DELIVERY_NOTE_BATCHES_ATTACHED` 事件 |

#### 关键实现位置

| 模块 | 文件 | 函数 / DTO |
|---|---|---|
| DTO | `src/modules/delivery_note/dto.rs` | `ScanDeliveryOut`, `UnresolvedTargetDto`, `AvailableBatchDto`, `AttachableBatchDto` (2026-08-31), `AttachBatchesRequest`/`Out` (2026-08-31) |
| service | `src/modules/delivery_note/service/scan.rs` | `scan_add`, `has_fully_invalid_target` (2026-08-31), `classify_outcome`, `is_attachable_state`, `is_inspectable_state`, `classify_invalid_state`, `build_unresolved_target` |
| service | `src/modules/delivery_note/service/attach.rs` (2026-08-31 新建) | `attach_batches`（弹窗提交专用，与 `scan_add` 共用 `is_attachable_state`） |
| handler | `src/modules/delivery_note/handler.rs` | `scan_delivery_note`, `attach_batches` (2026-08-31), WS 广播 |
| 错误码 | `src/shared/error.rs` | `BIZ_DELIVERY_BATCH_STATE_INVALID = 21421` |
| 状态机 | `src/modules/part/statemachine.rs` | `PartStatus::can_transition_to` 新增 `REPAIRING → INSPECTION` |
| 单测 | `src/modules/delivery_note/service/scan.rs` 末尾 | `classify_5groups_tests`, `outcome_tests`, `c_group_distribution_tests` (2026-08-31), `attachable_batches_tests` (2026-08-31) |
| 单测 | `src/modules/delivery_note/service/attach.rs` 末尾 | `attach_batches_logic_tests` |