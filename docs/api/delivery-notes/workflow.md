# delivery-notes / 状态流转

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · **`workflow.md`** · [`print.md`](./print.md)

## 本文件目录

1. [POST /api/v2/delivery-notes/{id}/submit](#post-apiv2delivery-notesidsubmit)
2. [POST /api/v2/delivery-notes/{id}/recall](#post-apiv2delivery-notesidrecall)
3. [POST /api/v2/delivery-notes/{id}/pickup-scan](#post-apiv2delivery-notesidpickup-scan)
4. [POST /api/v2/delivery-notes/{id}/pickup](#post-apiv2delivery-notesidpickup)

---

### `POST /api/v2/delivery-notes/{id}/submit`

权限: **Manager / Clerk / Inspector**（service 层 enforce）（**DRAFT → SUBMITTED**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 送货单乐观锁 |

Response 200 `data`：`SubmitDeliveryOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `outcome` | string | `SUBMITTED` / `CANDIDATES_AVAILABLE` |
| `note` | [`DeliveryNoteOut`](./index.md#deliverynoteout-字段)? | 仅 `SUBMITTED` 时非 null；`CANDIDATES_AVAILABLE` 时字段**存在且为 `null`**（不省略） |
| `unresolved_targets` | [`UnresolvedTargetDto`](./drafts.md#post-apiv2delivery-notesscan--p3-扫码建单)[]? | 仅 `CANDIDATES_AVAILABLE` 时出现；`SUBMITTED` 时该 key **整个缺省**（不序列化） |

> 前端判据：`data.note != null` ⇔ 本次真的提交了。两个分支都是 HTTP 200 + `code: 0`。

**批次状态口径**：批次能挂上送货单的前提就是 `INSPECTION` 或 `READY_TO_SHIP`（入单校验见 [`add-parts`](./drafts.md#post-apiv2delivery-notesidadd-parts)），因此 submit 只需分这三类：

| 挂单批次状态 | submit 行为 |
|---|---|
| 全部 `READY_TO_SHIP` | `outcome=SUBMITTED`，状态机 DRAFT → SUBMITTED（单据 `version` +1），WS 广播 `DELIVERY_NOTE_SUBMITTED` |
| 存在 `INSPECTION` | `outcome=CANDIDATES_AVAILABLE`，**不做任何写入**（单据留在 DRAFT、`version` 不变、无 WS 广播），这些批次按 part 分组进 `unresolved_targets` |
| 其它状态 | `21421 BIZ_DELIVERY_BATCH_STATE_INVALID`（数据非法：挂单后被旁路改状态） |

> `unresolved_targets` 只收 `INSPECTION` 批次；同一单里已 `READY_TO_SHIP` 的批次不会出现在候选里。分组顺序 = part 首次出现顺序，组内 = 批次 id 升序。

**一键过检衔接**：`unresolved_targets[i].available_batches[]` 的 `{ batch_id, version, quantity }` 可直接映射为 [`POST /api/v2/parts/batch-to-ship`](../parts/inspection.md#post-apiv2partsbatch-to-ship) 的 `items[]`；`version` 不符 → 该 item 落 `failed[].code = 40901`。全部过检成功后用**同一个** `version`（候选分支没有 bump 单据 version）重新 submit 即可。

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION — 非 DRAFT
- 21411 BIZ_DELIVERY_NOTE_INVALID_VALUE — 空单（未挂任何批次）
- 21421 BIZ_DELIVERY_BATCH_STATE_INVALID — 挂单批次状态不在 `{INSPECTION, READY_TO_SHIP}`
- 40901 VERSION_CONFLICT — 送货单 `version` 不符
- 40300 FORBIDDEN — 角色不符

> 校验顺序（先命中先返回）：角色 → 单据存在（21401）→ `version`（40901）→ DRAFT（21402）→ 非空单（21411）→ 批次状态分类（21421 / 候选 / 提交）。
>
> 注：submit **不再**返回 `21405 BIZ_DELIVERY_NOTE_PART_NOT_READY` —— 原「有批次未过检」场景已改为 200 + `CANDIDATES_AVAILABLE`。该错误码仍由 `POST /delivery-notes`、`add-parts`、`pickup` 触发。

### `POST /api/v2/delivery-notes/{id}/recall`

权限: 已登录（**SUBMITTED → DRAFT**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteOut`](./index.md#deliverynoteout-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 21419 BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT — 同范围已存在 DRAFT
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/pickup-scan`

权限: 已登录

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_serial` | string | ✓ | 扫码的序列号 |
| `badge_code` | string? | — | 工人胸牌码 |

Response 200 `data`：`PickupScanOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `delivery_note_id` | string (i64) | |
| `scanned_count` | i64 | P2 阶段始终为 0（前端本地 Set 驱动） |
| `expected_count` | i64 | |
| `ready` | bool | |
| `scanned_serials` | [string] | P2 阶段始终为 `[]` |

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21408 BIZ_DELIVERY_NOTE_SCAN_MISMATCH — 序列号不在本单

### `POST /api/v2/delivery-notes/{id}/pickup`

权限: 已登录（**SUBMITTED → PICKED_UP**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `driver_worker_id` | string (i64) | ✓ | 司机 worker ID |
| `badge_code` | string? | — | |
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteOut`](./index.md#deliverynoteout-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 21404 BIZ_DELIVERY_NOTE_NOT_SUBMITTED — 非 SUBMITTED 不能 pickup
- 21409 BIZ_DELIVERY_NOTE_DRIVER_INVALID — 司机非送货司机/不活跃
- 21410 BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE — 还没扫齐（保留语义）
- 40901 VERSION_CONFLICT