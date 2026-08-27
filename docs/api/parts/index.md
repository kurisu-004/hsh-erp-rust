# part 域 API

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：CRUD / by-serial 查询 / upload-drawing / lifecycle 状态机（deliver / cancel / complete / start-repair）/ inspection 流（pass / scan-inspect / fail / worker-scan）。所有路径前缀 `/api/v2`。
> 已拆为子目录：
>
> 导航：[**`index.md`**](./index.md) · [`crud.md`](./crud.md) · [`lifecycle.md`](./lifecycle.md) · [`inspection.md`](./inspection.md)

---

## 端点列表

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/parts` | Manager / Clerk / Inspector / CncProgrammer | 列表查询 + 分页 + 多字段过滤 | [`crud.md`](./crud.md#get-apiv2parts) |
| POST | `/api/v2/parts` | Manager / Clerk | 单件创建工单（status=PENDING） | [`crud.md`](./crud.md#post-apiv2parts) |
| POST | `/api/v2/parts/batch` | Manager / Clerk | 批量创建（共享 customer_id；N≤200） | [`crud.md`](./crud.md#post-apiv2partsbatch) |
| GET | `/api/v2/parts/{part_id}` | Manager / Clerk / Inspector / CncProgrammer | 工单详情（含 customer_name / current_batch_id 冗余） | [`crud.md`](./crud.md#get-apiv2partspart_id) |
| GET | `/api/v2/parts/by-serial/{serial_no}` | Manager / Clerk / Inspector / CncProgrammer | 通过序列号查详情 | [`crud.md`](./crud.md#get-apiv2partsby-serialserial_no) |
| POST | `/api/v2/parts/{part_id}/update` | Manager / Clerk | 字段可选 UPDATE（OCC + 软删守卫） | [`crud.md`](./crud.md#post-apiv2partspart_idupdate) |
| POST | `/api/v2/parts/{part_id}/soft-delete` | **Manager** | 软删（OCC + 终态禁 + delivery_note 锁禁） | [`crud.md`](./crud.md#post-apiv2partspart_idsoft-delete) |
| POST | `/api/v2/parts/{part_id}/upload-drawing` | Manager / Clerk | Multipart PDF 上传到 COS + 落 `t_part_file` | [`crud.md`](./crud.md#post-apiv2partspart_idupload-drawing) |
| POST | `/api/v2/parts/{part_id}/deliver` | Manager / Clerk | READY_TO_SHIP → DELIVERED | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_iddeliver) |
| POST | `/api/v2/parts/{part_id}/cancel` | Manager / Clerk | 5 状态白名单 → CANCELLED（拒 delivery_note 锁） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcancel) |
| POST | `/api/v2/parts/{part_id}/complete` | Manager / Clerk | DELIVERED → COMPLETED（清空 serial_no） | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idcomplete) |
| POST | `/api/v2/parts/{part_id}/start-repair` | Manager / Clerk / Inspector | IN_PROCESS → REPAIRING | [`lifecycle.md`](./lifecycle.md#post-apiv2partspart_idstart-repair) |
| POST | `/api/v2/parts/batch-pass-inspection` | Manager / Inspector | 批量通过品检（INSPECTION → READY_TO_SHIP） | [`inspection.md`](./inspection.md#post-apiv2partsbatch-pass-inspection) |
| POST | `/api/v2/parts/{part_id}/pass-inspection` | Manager / Inspector | 单件通过品检（INSPECTION → READY_TO_SHIP） | [`inspection.md`](./inspection.md#post-apiv2partspart_idpass-inspection) |
| POST | `/api/v2/parts/batch-scan-inspect` | Manager / Inspector | 批量一键送检（PENDING/PROGRAMMING/IN_PROCESS → INSPECTION） | [`inspection.md`](./inspection.md#post-apiv2partsbatch-scan-inspect) |
| POST | `/api/v2/parts/{part_id}/scan-inspect` | Manager / Inspector | 单件一键送检 → PASS/FAIL | [`inspection.md`](./inspection.md#post-apiv2partspart_idscan-inspect) |
| POST | `/api/v2/parts/{part_id}/fail-inspection` | Manager / Inspector | 单件品检打回（INSPECTION → IN_PROCESS） | [`inspection.md`](./inspection.md#post-apiv2partspart_idfail-inspection) |
| POST | `/api/v2/parts/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检；成功后同事务触发 worker-pool refill | [`inspection.md`](./inspection.md#post-apiv2partsworker-scan) |

> 路由顺序：`/batch-pass-inspection`、`/batch-scan-inspect`、`/batch`、`/by-serial/{serial_no}`、`/worker-scan` 必须在 `/{part_id}/...` 之前注册（axum 防止静态段被解析成 `part_id`）。

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

`TPart` 完整 28 列 + `customer_name` / `l1_customer_name` 冗余字段；见 [`../auth.md`](../auth.md) 关于 i64 字段序列化为 string 的约定。

### PartListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartListItem](#partlistitem-字段)[] | |
| `total` | string (i64) | 满足过滤的总数 |
| `limit` | string (i64) | 实际生效 |
| `offset` | string (i64) | 实际生效 |

### PartDetailOut 字段

`TPart` 完整 28 列 + `customer_name` / `l1_customer_name` / `current_batch_id`（仅 INSPECTION 时非 None）。

## 端点约束（与 Python 一致）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → `40901 VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → `20101`
- **状态机**：详见 [状态机（can_transition_to 白名单）](./inspection.md#状态机can_transition_to-白名单)；不在白名单内的 source / target 组合返回 `20103 BIZ_INVALID_TRANSITION`（迁移表见 `src/modules/part/statemachine.rs`）
- **事件日志**：状态迁移在 service 内事务内统一插入对应事件，service 提交后由 WS 中枢广播
- **part↔batch 同步**：lifecycle 终态 / 翻转（deliver / cancel / complete / start-repair）同事务内除翻 `t_part` 外还需翻最近一条 source-status 批次（`PartRepo::find_most_recent_batch_for_part`），保证候选批次不被 stale 状态污染
---

## 状态机

见 [`./inspection.md`](./inspection.md#状态机can_transition_to-白名单)。

## 错误码参考

part / lifecycle 错误码（20101 / 20103 / 20104 / 20109 / 20111 / 20115 / 20116 / 20117 / 20118 / 20119 / 21420 / 40001 / 40300 / 40901）见 [`./inspection.md`](./inspection.md#错误码参考part-lifecycle)。

货架错误码（20511 / 20512 — scan-inspect / fail-inspection 专用）见 [`./inspection.md`](./inspection.md#货架错误码20511-20512-scan-inspect-fail-inspection-专用)。

## 参考

- 集成测试：`tests/part_api.rs`（inspection 流全链路）+ `tests/part_crud.rs`（CRUD + lifecycle 27 用例）
- 仓库分层：`src/modules/part/handler.rs` (axum) → `service/{crud,inspection,lifecycle}.rs` (业务) → `repo/{part,batch,event}.rs` (SQL)
- 状态机：`src/modules/part/statemachine.rs`
- 错误码：`src/shared/error.rs::code`
- worker-scan 联动：详见 [`../worker-pool.md`](../worker-pool.md)
- Python myERP 参考：`/Users/ren/Code/myERP/api/v1/part.py`（46 个端点；本目录 18 个端点之外的 32 个 Python 独有端点（其中 Rust 18 中有 4 个 Rust-only 端点）见 [`../inconsistencies.md`](../inconsistencies.md)）
