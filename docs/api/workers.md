# workers 域 API

> 本文件须与 `src/modules/worker/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/workers/verify-badge` | 已登录（任意角色） | 扫码台按 badge_code 定位工人 |
| GET | `/api/v2/workers` | 已登录（MANAGER） | 列表（按 `name_like` / `is_active` 过滤 + 分页） |
| POST | `/api/v2/workers` | 已登录（MANAGER） | 创建工人 |
| GET | `/api/v2/workers/{id}` | 已登录（MANAGER） | 工人详情 |
| POST | `/api/v2/workers/{id}/update` | 已登录（MANAGER） | 部分更新（OCC） |
| POST | `/api/v2/workers/{id}/deactivate` | 已登录（MANAGER） | 停用（OCC，被 IN_PROCESS/INSPECTION/REPAIRING/RETURNED 零件引用时拒） |
| POST | `/api/v2/workers/{id}/reactivate` | 已登录（MANAGER） | 重启（OCC；is_active=true + deleted_at=NULL） |

挂载点：`/api/v2/workers`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型

- `badge_code`：业务唯一键（`uk_t_worker_badge_code`，活跃行唯一；软删行不参与去重）
- `id_card_no`：可选，部分唯一索引（`uk_t_worker_id_card_no`，NULL 不参与去重；撞 → 40901）
- `phone`：可选
- `work_type_id`：可选；NULL 表示未分配工种。`update` 三态编码支持显式清空
- `is_active` / `deleted_at`：共同追踪生命周期
  - `deactivate` 同时置 `is_active=false` + `deleted_at=now()`
  - `reactivate` 同时置 `is_active=true` + `deleted_at=NULL`

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| `verify-badge` | 命中且 `is_active=false` → 20202 `BIZ_WORKER_INACTIVE`（HTTP 400）；未命中 → 20201 `BIZ_WORKER_NOT_FOUND`（HTTP 404） |
| `create` | `work_type_id`（若提供）必须指向现存工种；`badge_code` / `id_card_no` 撞唯一索引 → 40901 |
| `update` | `work_type_id` 三态编码支持显式清空；OCC |
| `deactivate` | `t_part.current_holder_id = worker_id` 且 `status IN ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')` 引用 > 0 → 20203 拒 |
| `reactivate` | 行必须存在（已软删亦可）；行已是激活态 → 20104 拒 |

---

### `POST /api/v2/workers/verify-badge`

权限：已登录（任意角色；service 层只校验 JWT，不限角色）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `badge_code` | string | ✓ | trim 后非空；1..=50 字符 |

Response 200 `data`：`WorkerOut`（**不**含 `work_type_name`；如需由前端再调 `GET /workers/{id}`）

错误码：

- 20201 `BIZ_WORKER_NOT_FOUND` — `badge_code` 不存在（或空串）
- 20202 `BIZ_WORKER_INACTIVE` — 工人存在但 `is_active=false`（HTTP 400）

### `GET /api/v2/workers`

权限：已登录（MANAGER）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name_like` | string | — | `ILIKE '%needle%'`；trim 后空串视为无过滤 |
| `is_active` | bool | — | 精确匹配；缺省 = 不过滤 |
| `limit` | i64 | — | 默认 50，clamp(1, 500) |
| `offset` | i64 | — | 默认 0，max(0, …) |

Response 200 `data`：`WorkerListOut`

### `POST /api/v2/workers`

权限：已登录（MANAGER）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `badge_code` | string | ✓ | trim 后非空；1..=50 字符 |
| `name` | string | ✓ | trim 后非空；1..=50 字符 |
| `id_card_no` | string? | — | 可选；空串视为 NULL |
| `phone` | string? | — | 可选；空串视为 NULL |
| `work_type_id` | string (i64)? | — | 可选；空串视为 NULL；非空时必须指向现存 `t_work_type` |

Response 201 `data`：`WorkerOut`

错误码：

- 20104 `BIZ_INVALID_VALUE` — badge_code 空 / name 空 / work_type_id 非整数 / 工种不存在（20901）
- 40901 `VERSION_CONFLICT` — badge_code 或 id_card_no 已存在（DB 唯一索引兜底）
- 20901 `BIZ_WORK_TYPE_NOT_FOUND` — work_type_id 指向不存在或已软删的工种

### `GET /api/v2/workers/{id}`

权限：已登录（MANAGER）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工人 ID |

Response 200 `data`：`WorkerOut`（含 `work_type_name`）

错误码：

- 20201 `BIZ_WORKER_NOT_FOUND`

### `POST /api/v2/workers/{id}/update`

权限：已登录（MANAGER）

Request（部分更新；与 Python `exclude_unset` 语义一致）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改值 |
| `badge_code` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改值（撞 uk_t_worker_badge_code → 40901） |
| `id_card_no` | string? \| null? | — | None = 不改；`Some(null)` = 显式清空；`Some(value)` = 改值 |
| `phone` | string? \| null? | — | 同 id_card_no 三态 |
| `work_type_id` | string? \| null? | — | None = 不改；`Some(null)` = 显式清空（SET NULL）；`Some(value)` = 改值（校验工种存在） |

Response 200 `data`：`WorkerOut`（回读最新版本）

错误码：

- 20201 `BIZ_WORKER_NOT_FOUND`
- 20104 `BIZ_INVALID_VALUE` — name/badge_code 空 / work_type_id 非整数 / 工种不存在
- 40901 `VERSION_CONFLICT` — OCC 冲突 / badge_code 撞 uk / id_card_no 撞 uk

### `POST /api/v2/workers/{id}/deactivate`

权限：已登录（MANAGER）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工人 ID |

Request：空 body

Response 200 `data`：`null`

错误码：

- 20201 `BIZ_WORKER_NOT_FOUND`
- 20203 `BIZ_WORKER_IN_USE` — `t_part.current_holder_id = worker_id` 且
  `status IN ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')` 仍有非软删引用 → 拒
- 40901 `VERSION_CONFLICT`

### `POST /api/v2/workers/{id}/reactivate`

权限：已登录（MANAGER）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工人 ID |

Request：空 body

Response 200 `data`：`WorkerOut`

错误码：

- 20201 `BIZ_WORKER_NOT_FOUND` — 行不存在
- 20104 `BIZ_INVALID_VALUE` — 行已是激活态，无需重启
- 40901 `VERSION_CONFLICT`

---

## DTO 字段参考

`WorkerOut`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（JSON 字符串，防 JS 精度截断） |
| `badge_code` | string | 工牌扫码值 |
| `name` | string | |
| `id_card_no` | string? | 身份证号 |
| `phone` | string? | 手机号 |
| `is_active` | bool | |
| `work_type_id` | string (i64)? | 工种 id；NULL = 未分配 |
| `work_type_name` | string? | service 层补全；work_type_id 为 NULL 时为 NULL；verify-badge 端点不补 |
| `version` | i32 | 乐观锁；每次写操作 +1 |
| `created_at` | naive datetime | Asia/Shanghai |
| `updated_at` | naive datetime | Asia/Shanghai |

---

## 维护约定

1. `work_type_name` 在 `list_workers` / `get_worker` / `reactivate_worker` 三处由
   `WorkTypeRepo::list_by_ids` / `get_by_id` 批量补全；`verify_badge` 故意**不**补，
   保持扫码响应轻量。
2. `id_card_no` 撞唯一索引统一映射到 `40901 VERSION_CONFLICT`（与 Python 行为一致）；
   不引入额外的 `BIZ_WORKER_ID_CARD_DUPLICATE` 业务码。
3. `deactivate` 与 `reactivate` 互为逆操作，但走两条独立 SQL（前者限制
   `deleted_at IS NULL`；后者限制 `deleted_at IS NOT NULL`）。OCC 失败统一转 40901。
