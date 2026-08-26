# processes 域 API

> 本文件须与 `src/modules/process/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（过滤 + 分页） |
| POST | `/api/v2/processes` | MANAGER | 创建工序（INHOUSE/OUTSOURCE） |
| GET | `/api/v2/processes/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工序详情 |
| POST | `/api/v2/processes/{id}/update` | MANAGER | 部分更新（OCC） |
| POST | `/api/v2/processes/{id}/soft-delete` | MANAGER | 软删（OCC，被引用时拒） |

挂载点：`/api/v2/processes`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型

工序是 worker-pool 域与多个下游域（外协公司能力清单、货架工序映射、工种工序白名单、
工单 next_process_id）的核心枢纽。`category` 区分两类业务场景：

- **INHOUSE**（自产）：在厂内加工。`requires_approval` 强制 `false`（与 Python
  `service/process.py:_assert_inhouse_no_approval` 对齐）。INHOUSE 工序一般被
  `t_work_type_process` / `t_shelf_process` / `t_part.next_process_id` 引用。
- **OUTSOURCE**（外协）：发外协公司加工。`requires_approval` 保留请求值（默认 `true`，
  表示 OUTSOURCE 工序进入外协报价流程需要走 `t_outsource_quote.APPROVED`）。OUTSOURCE
  工序一般被 `t_outsource_company_process` / `t_part.next_process_id` 引用。

`code` 是业务唯一键（`uk_t_process_code`，活跃行唯一）：**不可变** —— update 接口
收到任何 `code` 字段一律 20104 `BIZ_INVALID_VALUE`。

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| 创建 | `code` 非空、`name` 非空；`category` ∈ {INHOUSE, OUTSOURCE}；INHOUSE 时 `requires_approval` 强制 `false`；`uk_t_process_code` 撞 → 20802 |
| 更新 `code` | **不允许**（20104） |
| 更新 `category` | **不允许**（20104） |
| 更新 `name` | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| 更新 `description` | 三态：`None` 不改；`Some(null)` 清空；`Some(v)` 改（trim 后写） |
| 更新 `requires_approval` | INHOUSE 强制 false（无视 Some/None 内的任何值）；OUTSOURCE 保留请求值，None = 不改 |
| 更新 `sort_order` | None = 不改；Some(v) = 改 |
| 软删 | `t_work_type_process` / `t_outsource_company_process` / `t_shelf_process` / `t_part.next_process_id` 任何一处有非软删引用 → 20803 拒 |

---

### `GET /api/v2/processes`

权限：已登录（M/C/CNC/SHELF/INSPECTOR；service 层 `require_any_role`）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code_like` | string | — | `ILIKE '%needle%'`；trim 后空串视为无过滤 |
| `category` | string | — | 精确匹配（INHOUSE / OUTSOURCE）；trim 后空串视为无过滤 |
| `limit` | i64 | — | 默认 50，clamp(1, 500) |
| `offset` | i64 | — | 默认 0，max(0, …) |

Response 200 `data`：`ProcessListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [ProcessOut] | |
| `total` | i64 | 全量命中行数（无视 limit/offset） |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### `POST /api/v2/processes`

权限：MANAGER（service 层 `require_role`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后非空；撞 `uk_t_process_code` → 20802 |
| `name` | string | ✓ | trim 后非空 |
| `category` | string | ✓ | INHOUSE 或 OUTSOURCE（大小写不敏感，规范化大写） |
| `sort_order` | i32? | — | 默认 0 |
| `description` | string? | — | trim 后空串视为 NULL |
| `requires_approval` | bool? | — | OUTSOURCE：缺省 = true；INHOUSE：**忽略请求值**，强制 false |

Response 201 `data`：`ProcessOut`

错误码：

- 20104 `BIZ_INVALID_VALUE` — code/name 空 / category 不在 {INHOUSE, OUTSOURCE}
- 20802 `BIZ_PROCESS_DUPLICATE_CODE` — `uk_t_process_code` 撞唯一索引

### `GET /api/v2/processes/{id}`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工序 ID |

Response 200 `data`：`ProcessOut`

错误码：

- 20801 `BIZ_PROCESS_NOT_FOUND`

### `POST /api/v2/processes/{id}/update`

权限：MANAGER

Request（部分更新；与 Python `exclude_unset` 语义一致）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | — | — | **不支持**。本字段若传一律 20104 |
| `category` | — | — | **不支持**。本字段若传一律 20104 |
| `name` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| `sort_order` | i32? | — | None = 不改；Some(v) = 改 |
| `description` | string? \| null? | — | 字段缺省 = 不改；`Some(null)` = 显式清空；`Some(value)` = 改值（trim 后写） |
| `requires_approval` | bool? | — | INHOUSE 强制 false（无视 Some 内的任何值）；OUTSOURCE 保留请求值；None = 不改 |

Response 200 `data`：`ProcessOut`（回读最新版本）

错误码：

- 20801 `BIZ_PROCESS_NOT_FOUND`
- 20104 `BIZ_INVALID_VALUE` — code/category 试图改 / name 空
- 40901 `VERSION_CONFLICT` — 乐观锁冲突（前端需重新 GET 后再试）

### `POST /api/v2/processes/{id}/soft-delete`

权限：MANAGER

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工序 ID |

Request：空 body

Response 200 `data`：`null`

错误码：

- 20801 `BIZ_PROCESS_NOT_FOUND`
- 20803 `BIZ_PROCESS_IN_USE` — 4 张引用表中任一有非软删引用 → 拒
- 40901 `VERSION_CONFLICT`

---

## DTO 字段参考

`ProcessOut`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（JSON 字符串，防 JS 精度截断） |
| `code` | string | 业务唯一键；创建后不可改 |
| `name` | string | |
| `category` | string | INHOUSE / OUTSOURCE |
| `sort_order` | i32 | 显示顺序 |
| `description` | string? | |
| `requires_approval` | bool | INHOUSE 永远 false；OUTSOURCE 由请求决定 |
| `version` | i32 | 乐观锁；每次写操作 +1 |
| `created_at` | naive datetime | Asia/Shanghai |
| `updated_at` | naive datetime | Asia/Shanghai |

---

## 软删引用查询（best-effort）

`ProcessRepo::count_process_references` 一次 `UNION ALL` 累加以下 4 张表的非软删计数：

| 引用表 | 用途 | 是否筛 deleted_at |
|---|---|---|
| `t_work_type_process` | 工种可执行工序白名单 | 否（mapping 表无业务软删） |
| `t_outsource_company_process` | 外协公司能力清单 | 否（mapping 表无业务软删） |
| `t_shelf_process` | 货架支持的工序 | 否（mapping 表无业务软删） |
| `t_part` (`next_process_id`) | 工单下一工序 | **是**（part 表带软删） |

任一总数 > 0 ⇒ 20803 `BIZ_PROCESS_IN_USE`。当前阶段（Phase P2）4 张表都已迁移到位，
无 junction repo 缺口；后续如需按 junction 拆分 repo，可保留 best-effort 注释。