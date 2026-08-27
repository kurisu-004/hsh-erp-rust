# part 域 — Lifecycle

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
> 状态机 / 错误码见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)
>
> 范围：本文件覆盖 4 个 lifecycle 端点（deliver / cancel / complete / start-repair）。CRUD / inspection 见 [`./crud.md`](./crud.md) / [`./inspection.md`](./inspection.md)。

## 本文件目录


- [POST /api/v2/parts/{part_id}/deliver](#post-apiv2partspart_iddeliver)
- [POST /api/v2/parts/{part_id}/cancel](#post-apiv2partspart_idcancel)
- [POST /api/v2/parts/{part_id}/complete](#post-apiv2partspart_idcomplete)
- [POST /api/v2/parts/{part_id}/start-repair](#post-apiv2partspart_idstart-repair)

---

### `POST /api/v2/parts/{part_id}/deliver`

权限: **Manager / Clerk**

Request：`{ "note"?: string }`（可空）

Response 200 `data`：[`PartOut`](./index.md#partout-字段) — 流转后工单。同步翻转最近一条 `READY_TO_SHIP` 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED
- 20117 — 当前状态非 READY_TO_SHIP（状态机白名单拒绝）
- 40901 — 乐观锁失败（part 或 batch）

### `POST /api/v2/parts/{part_id}/cancel`

权限: **Manager / Clerk**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。同步翻转最近一条 source-status 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20103 — 当前状态不在 cancel 白名单（COMPLETED / REPAIRING / OUTSOURCE 等）
- 20104 — status 字符串非法
- 20115 — part 已 CANCELLED
- 21420 — part 已挂送货单，禁 cancel
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/complete`

权限: **Manager / Clerk**

Request：`{ "note"?: string }`（可空）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。**`t_part.serial_no` 被清空**（序列号已转交送货单）。同步翻转最近一条 DELIVERED 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20115 — part 已 CANCELLED
- 20116 — 当前状态非 DELIVERED（状态机白名单拒绝）
- 40901 — 乐观锁失败

### `POST /api/v2/parts/{part_id}/start-repair`

权限: **Manager / Clerk / Inspector**

Request：`{ "reason"?: string, "note"?: string }`（`reason` 优先作为事件 note）

Response 200 `data`：[`PartOut`](./index.md#partout-字段)。`t_part.has_been_repaired` 置 `true`；同步翻转最近一条 IN_PROCESS 批次（同事务）。

错误码：

- 20101 — part 不存在 / 软删
- 20115 — part 已 CANCELLED
- 20118 — 当前状态非 IN_PROCESS（状态机白名单拒绝）
- 40901 — 乐观锁失败

---

## Lifecycle 专属 DTO

### DeliverRequest / CancelRequest / CompleteRequest / StartRepairRequest 字段

均仅含可选 `note` / `reason`（≤ 500 字符建议）；事件日志透传 `note`，cancel 与 start-repair 优先取 `reason` 作为事件 note。
