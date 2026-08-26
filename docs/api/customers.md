# customers 域 API

> 本文件须与 `src/modules/customer/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/customers` | 已登录（M/C/INSPECTOR/CNC） | 列表（L1+L2 + 过滤） |
| POST | `/api/v2/customers` | 已登录（M/C） | 创建客户（L1 或 L2） |
| GET | `/api/v2/customers/{id}` | 已登录（M/C/INSPECTOR/CNC） | 客户详情 |
| POST | `/api/v2/customers/{id}/update` | 已登录（M/C） | 部分更新（OCC） |
| POST | `/api/v2/customers/{id}/soft-delete` | 已登录（M/C） | 软删（OCC，被 part/assembly 引用时拒） |

挂载点：`/api/v2/customers`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型：L1 / L2 两层结构

- **L1**（一级集团）：`parent_id IS NULL`，`serial_prefix` 必须为单个大写字母（A-Z），
  对应 `t_customer` 上 `uq_t_customer_root_prefix`（活跃 L1 的 prefix 唯一）。
- **L2**（叶子客户）：`parent_id` 非 NULL（指向 L1），`serial_prefix` 必须 NULL。
  `serial_prefix` 由所属 L1 派生，自身不持有。

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| 创建 L1 | `parent_id` 必须缺省 / 空串；`serial_prefix` 必须为单大写字母 |
| 创建 L2 | `parent_id` 必填且指向现存 L1；`serial_prefix` 必须缺省 / NULL |
| 更新 `serial_prefix` | 仅 L1 可改；L2 改则 20104 |
| 显式清空 `serial_prefix` | 仅 L1 可清；L2 清则 20104（语义上不应发生） |
| 更新 `parent_id` | **本接口不支持**（20104）。移级走 soft-delete + 重建 |
| 软删 | `t_part.customer_id` 或 `t_assembly.customer_id` 有非软删引用 → 20113 拒 |

---

### `GET /api/v2/customers`

权限：已登录（M/C/INSPECTOR/CNC；service 层 `require_any_role`）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name_like` | string | — | `ILIKE '%needle%'`；trim 后空串视为无过滤 |
| `parent_id` | string (i64) | — | 精确匹配父客户；与 `is_root` 互斥，同时传以 `parent_id` 为准 |
| `is_root` | bool | — | `true` ⇒ `parent_id IS NULL`（L1）；`false` ⇒ `parent_id IS NOT NULL`（L2）；缺省 = 不过滤 |
| `limit` | i64 | — | 默认 50，clamp(1, 500) |
| `offset` | i64 | — | 默认 0，max(0, …) |

Response 200 `data`：`CustomerListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [CustomerOut] | |
| `total` | i64 | 全量命中行数（无视 limit/offset） |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### `POST /api/v2/customers`

权限：已登录（M/C；service 层 `require_any_role([M, C])`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | trim 后非空；1..=100 字符 |
| `parent_id` | string (i64)? | — | 创建 L2 时必填；指向 L1 id |
| `serial_prefix` | string? | — | 创建 L1 时必填（单大写字母）；创建 L2 时必须缺省 |

Response 201 `data`：`CustomerOut`

错误码：

- 20104 `BIZ_INVALID_VALUE` — name 空 / parent_id 非整数 / L1 缺 prefix / L2 传了 prefix / prefix 非单大写字母
- 20104 `BIZ_INVALID_VALUE` — `serial_prefix` 与已有活跃 L1 撞唯一索引（uk_t_customer_root_prefix）

### `GET /api/v2/customers/{id}`

权限：已登录（M/C/INSPECTOR/CNC）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 客户 ID |

Response 200 `data`：`CustomerOut`

错误码：

- 20102 `BIZ_CUSTOMER_NOT_FOUND`

### `POST /api/v2/customers/{id}/update`

权限：已登录（M/C）

Request（部分更新；与 Python `exclude_unset` 语义一致）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| `serial_prefix` | string? \| null? | — | 字段缺省 = 不改；`Some(null)` = 显式清空（仅 L1）；`Some(value)` = 改值（仅 L1） |
| `parent_id` | — | — | **不支持**。本字段若传一律 20104；移级走 soft-delete + 重建 |

Response 200 `data`：`CustomerOut`（回读最新版本）

错误码：

- 20102 `BIZ_CUSTOMER_NOT_FOUND`
- 20104 `BIZ_INVALID_VALUE` — name 空 / L2 改 prefix / L2 清 prefix / 试图改 parent_id / prefix 非单大写字母
- 40901 `VERSION_CONFLICT` — 乐观锁冲突（前端需重新 GET 后再试）

### `POST /api/v2/customers/{id}/soft-delete`

权限：已登录（M/C）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 客户 ID |

Request：空 body

Response 200 `data`：`null`

错误码：

- 20102 `BIZ_CUSTOMER_NOT_FOUND`
- 20113 `BIZ_CUSTOMER_IN_USE` — `t_part.customer_id` 或 `t_assembly.customer_id` 仍有非软删引用 → 拒
- 40901 `VERSION_CONFLICT`

---

## DTO 字段参考

`CustomerOut`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（JSON 字符串，防 JS 精度截断） |
| `name` | string | |
| `parent_id` | string (i64)? | L1 时为 `null`；L2 时为所属 L1 的 id 字符串 |
| `parent_name` | string? | service 层补全；孤立父节点（已软删）时为 `null` |
| `serial_prefix` | string? | L1 单大写字母；L2 为 `null` |
| `version` | i32 | 乐观锁；每次写操作 +1 |
| `created_at` | naive datetime | Asia/Shanghai |
| `updated_at` | naive datetime | Asia/Shanghai |