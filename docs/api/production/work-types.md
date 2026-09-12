# work_types 域 API

> 本文件须与 `src/modules/work_type/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：工种 CRUD。工种 ↔ 工序 映射见 [`./work-type-process-mapping.md`](./work-type-process-mapping.md)。

> **导航**：[`← index`](./index.md) · **work-types** · [`processes`](./processes.md) · [`work-type-process-mapping`](./work-type-process-mapping.md) · [`process-chain`](./process-chain.md) · [`worker-pool`](./worker-pool.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/work-types` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（`code_like` 过滤 + 分页） |
| POST | `/api/v2/work-types` | MANAGER | 创建工种 |
| GET | `/api/v2/work-types/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工种详情（含 `process_ids`） |
| POST | `/api/v2/work-types/{id}/update` | MANAGER | 部分更新（OCC） |
| POST | `/api/v2/work-types/{id}/soft-delete` | MANAGER | 软删（OCC，被 worker / process mapping 引用时拒） |

挂载点：`/api/v2/work-types`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型

工种是 worker-pool 域的核心枢纽：定义「一类工人可执行哪些工序 + 最多可同时持有几个批次」。
`t_work_type_process` 是无业务软删的 mapping 表（worker-pool / worker-scan 校验依赖），
`Process` 与 `Worker` 业务上挂到工种下：

- `code`：业务唯一键（`uk_t_work_type_code`，活跃行唯一）。**不可变** —— update 接口
  收到任何 `code` 字段一律 20104 `BIZ_INVALID_VALUE`。
- `description`：可选；空串视为 NULL；update 三态支持显式清空
- `sort_order`：显示顺序
- `max_held_batches`：工种工人最多可同时持有批次数；NULL=不限；update 改值时需 `≥1`
- `process_ids`（出参）：service 层用 `WorkTypeProcessRepo::list_by_work_types_batch`
  单条 SQL 批量补全（防 N+1；详见 [`./work-type-process-mapping.md`](./work-type-process-mapping.md)）

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| 创建 | `code` 非空、`name` 非空；`max_held_batches` 改值时 ≥1；撞 `uk_t_work_type_code` → 20902 |
| 更新 `code` | **不允许**（20104） |
| 更新 `name` | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| 更新 `description` | 三态：`None` 不改；`Some(null)` 清空；`Some(v)` 改 |
| 更新 `sort_order` | None = 不改；Some(v) = 改 |
| 更新 `max_held_batches` | 三态：`None` 不改；`Some(null)` 清空（NULL=不限）；`Some(v)` 改（v < 1 ⇒ 20104） |
| 软删 | `t_worker.work_type_id` 或 `t_work_type_process.work_type_id` 任一 > 0 ⇒ 20903 拒 |

---

### `GET /api/v2/work-types`

权限：已登录（M/C/CNC/SHELF/INSPECTOR；service 层 `require_any_role`）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code_like` | string | — | `ILIKE '%needle%'`；trim 后空串视为无过滤 |
| `limit` | i64 | — | 默认 50，clamp(1, 500) |
| `offset` | i64 | — | 默认 0，max(0, …) |

Response 200 `data`：`WorkTypeListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [WorkTypeOut](#worktypeout-字段) | 含 `process_ids` |
| `total` | i64 | 全量命中行数（无视 limit/offset） |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### `POST /api/v2/work-types`

权限：MANAGER（service 层 `require_role`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后非空；撞 `uk_t_work_type_code` → 20902 |
| `name` | string | ✓ | trim 后非空 |
| `description` | string? | — | trim 后空串视为 NULL |
| `sort_order` | i32? | — | 默认 0 |
| `max_held_batches` | i32? | — | NULL=不限；非空时 ≥1，否则 20104 |

Response 201 `data`：`WorkTypeOut`（`process_ids` 为空数组 — 创建时不传 mapping）

错误码：

- 20104 `BIZ_INVALID_VALUE` — code/name 空 / max_held_batches < 1
- 20902 `BIZ_WORK_TYPE_DUPLICATE_CODE` — `uk_t_work_type_code` 撞唯一索引

### `GET /api/v2/work-types/{id}`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工种 ID |

Response 200 `data`：`WorkTypeOut`（含 `process_ids` —— 单次批量查）

错误码：

- 20901 `BIZ_WORK_TYPE_NOT_FOUND`

### `POST /api/v2/work-types/{id}/update`

权限：MANAGER

Request（部分更新；与 Python `exclude_unset` 语义一致）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | — | — | **不支持**。本字段若传一律 20104 |
| `name` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| `description` | string? \| null? | — | 字段缺省 = 不改；`Some(null)` = 显式清空；`Some(value)` = 改值 |
| `sort_order` | i32? | — | None = 不改；Some(v) = 改 |
| `max_held_batches` | i32? \| null? | — | 字段缺省 = 不改；`Some(null)` = 显式清空（NULL=不限）；`Some(v)` = 改值（v < 1 ⇒ 20104） |

Response 200 `data`：`WorkTypeOut`（回读最新版本 + `process_ids`）

错误码：

- 20901 `BIZ_WORK_TYPE_NOT_FOUND`
- 20104 `BIZ_INVALID_VALUE` — code 试图改 / name 空 / max_held_batches < 1
- 40901 `VERSION_CONFLICT` — 乐观锁冲突（前端需重新 GET 后再试）

### `POST /api/v2/work-types/{id}/soft-delete`

权限：MANAGER

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工种 ID |

Request：空 body

Response 200 `data`：`null`

错误码：

- 20901 `BIZ_WORK_TYPE_NOT_FOUND`
- 20903 `BIZ_WORK_TYPE_IN_USE` — `t_worker.work_type_id` 活跃行或
  `t_work_type_process.work_type_id` 任一 > 0 → 拒
- 40901 `VERSION_CONFLICT`

---

## DTO 字段参考

### WorkTypeOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（JSON 字符串，防 JS 精度截断） |
| `code` | string | 业务唯一键；创建后不可改 |
| `name` | string | |
| `description` | string? | |
| `sort_order` | i32 | 显示顺序 |
| `max_held_batches` | i32? | NULL=不限 |
| `process_ids` | [string (i64)] | service 层补全；JSON 字符串数组防 JS 精度截断；详见 [`./work-type-process-mapping.md`](./work-type-process-mapping.md) |
| `version` | i32 | 乐观锁；每次写操作 +1 |
| `created_at` | naive datetime | Asia/Shanghai |
| `updated_at` | naive datetime | Asia/Shanghai |

---

## 维护约定

1. `max_held_batches` 撞 DB 唯一约束（如有）由 service 捕获 23505 兜底；
   当前阶段无 unique constraint，仅在 service 层校验 `≥1`。
2. 软删引用计数（`count_work_type_references`）单条 `UNION ALL` 查
   `t_worker.work_type_id`（活跃行）+ `t_work_type_process.work_type_id`（mapping 表
   无业务软删，不筛 `deleted_at`）；任一分支 > 0 ⇒ 20903 拒。**该 SQL 在 work_types 域的 soft-delete 端点执行**；mapping 端的整组替换语义见 [`./work-type-process-mapping.md`](./work-type-process-mapping.md)。
