# delivery-notes / 状态流转

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · **`workflow.md`** · [`print.md`](./print.md)

---

### `POST /api/v2/delivery-notes/{id}/submit`

权限: 已登录（**DRAFT → SUBMITTED**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteSummary`](./index.md#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-notes/{id}/recall`

权限: 已登录（**SUBMITTED → DRAFT**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`：[`DeliveryNoteSummary`](./index.md#deliverynotesummary-字段)

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

Response 200 `data`：[`DeliveryNoteSummary`](./index.md#deliverynotesummary-字段)

错误码：

- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION
- 21404 BIZ_DELIVERY_NOTE_NOT_SUBMITTED — 非 SUBMITTED 不能 pickup
- 21409 BIZ_DELIVERY_NOTE_DRIVER_INVALID — 司机非送货司机/不活跃
- 21410 BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE — 还没扫齐（保留语义）
- 40901 VERSION_CONFLICT