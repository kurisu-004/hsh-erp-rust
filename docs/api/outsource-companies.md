# outsource-companies 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service.rs,model.rs,repo.rs,statemachine.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：外协公司 CRUD + 工序映射（company 7 端点）。2026-09-13 Phase 2 落地。
> 关联域：[`./outsource-quotes.md`](./outsource-quotes.md) / [`./outsource-shipments.md`](./outsource-shipments.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-companies` | Manager / Clerk / Inspector / CncProgrammer | 列表（按 L1 + 名称过滤 + 分页） |
| POST | `/api/v2/outsource-companies` | Manager / Clerk | 新建公司（含初始工序映射） |
| GET | `/api/v2/outsource-companies/{id}` | Manager / Clerk / Inspector / CncProgrammer | 公司详情（含工序映射） |
| POST | `/api/v2/outsource-companies/{id}/update` | Manager / Clerk | 部分更新（OCC） |
| POST | `/api/v2/outsource-companies/{id}/soft-delete` | Manager | 软删（被 part 引用 / 仍映射工序时拒） |
| GET | `/api/v2/outsource-companies/by-process/{process_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工序反查活跃公司 |
| POST | `/api/v2/outsource-companies/{id}/processes` | Manager / Clerk | 整体替换工序映射（先软删旧 + 写新，单事务） |

> 路由顺序：`/by-process/{process_id}` 静态段必须在 `/{id}` catch-all 之前注册。

---

## 业务模型

- **公司表** `t_outsource_company`：`id` / `name` / `is_active` / `version` / 审计字段 + 软删。
- **工序映射** `t_outsource_company_process`：`(company_id, process_id)` 主键；`process_id` 必须指向 `t_process` 中 `category ∈ {OUTSOURCE, INHOUSE}` 的工序。
- **公司名唯一约束**：同 L1 下不重名（DB partial unique 兜底 → 21202 应用层预检 / 21214 DB 兜底）。

---

## 共享错误码（212xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21201 | BIZ_OUTSOURCE_COMPANY_NOT_FOUND | 404 | company 不存在 / 已软删 |
| 21202 | BIZ_OUTSOURCE_COMPANY_DUPLICATE | 409 | name 与已有活跃公司撞唯一索引 |
| 21203 | BIZ_OUTSOURCE_COMPANY_BAD_PROCESS | 400 | process_id 不存在 / 不是 OUTSOURCE/INHOUSE 类别 |
| 21204 | BIZ_OUTSOURCE_PROCESS_NOT_MAPPED | 400 | 调用方要求但 company 未映射该工序 |
| 21205 | BIZ_OUTSOURCE_COMPANY_IN_USE | 409 | 仍被 part OUTSOURCE 引用 / 仍映射工序 → 拒软删 |
| 21206 | BIZ_PART_NOT_OUTSOURCEABLE | 400 | 当前 part 状态不允许发送外协 |
| 21207 | BIZ_OUTSOURCE_DIRECT_REQUIRES_C2_SHELF | 400 | 直接发送外协要求 part 位于绑定了外协工序的货架 |
| 21208 | BIZ_OUTSOURCE_NO_SHELF | 400 | 系统无任何绑定了外协工序的货架 |
| 21214 | BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME | 409 | DB `uk_t_outsource_company_name` 部分唯一兜底：pre-check 漏网（并发插入 / 跨事务）→ INSERT 撞唯一索引。与 21202 同义 409，仅 code 不同用以区分应用层预检 vs DB 兜底路径 |

---

## 共享 DTO

### OutsourceCompanyOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `name` | string | 公司名 |
| `is_active` | bool | |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

### OutsourceCompanyWithProcessesOut 字段

`OutsourceCompanyOut` 子集 + `processes: [{ id, code, name, category }]`（LEFT JOIN t_process）。

### OutsourceCompanyListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `OutsourceCompanyOut[]` | |
| `total` | i64 | 全量命中行数 |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### OutsourceCompanyCreateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | trim 后非空；1..=100 字符 |
| `process_ids` | string (i64)[] | — | 初始工序映射；空数组合法 |

### OutsourceCompanyUpdateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `name` | string? | — | None = 不改；空串 40001 |
| `is_active` | bool? | — | None = 不改 |

### SetOutsourceCompanyProcessRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC（company 行 version） |
| `process_ids` | string (i64)[] | ✓ | 整体替换；空数组合法 |

### OutsourceCompanyListQuery 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `name_like` | string? | ILIKE `%needle%`；trim 后空串视为无过滤 |
| `is_active` | bool? | 缺省 = 不过滤 |
| `limit` | i64? | 默认 50，clamp(1, 500) |
| `offset` | i64? | 默认 0，max(0, …) |

---

## 端点契约要点

### 列表 + 详情

- `GET /` 走 `OutsourceService::list_companies`（含分页）。
- `GET /{id}` 返回 `OutsourceCompanyWithProcessesOut`（含工序数组）。
- `GET /by-process/{process_id}` 返回 `OutsourceCompanyOut[]`（无分页，调用方一般用于下拉）。

### 写操作

- `POST /` 在 handler 内开 tx → service 写 company 行 + 初始 process mappings → commit。
- `POST /{id}/update` 仅改 company 行字段；不动工序映射（走 `/processes` 单独端点）。
- `POST /{id}/soft-delete` 检查 `t_part` 引用 + 映射表 → 21205。
- `POST /{id}/processes` 走 `set_company_processes`：先软删旧 mapping（`deleted_at = now()`），再 bulk INSERT 新 mapping。单事务，OCC 锚 `version`。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901。
- `update` / `processes` / `soft-delete` 强制 OCC；`create` / `list` / `get` 不要求。

### 事务边界

- handler 层开 tx → 传 `&mut tx` 给 service → 显式 `tx.commit()`；失败时 `Transaction::drop` 自动回滚。
- WS 广播在 `tx.commit().await?` **之后**（对齐 Python 延迟广播模式）；本域**无** WS 事件（只读资产）。

### 防 N+1

- `list_companies` 单 SQL 取所有行（无 JOIN 工序）；调用方需工序数组时走详情 / by-process。
- `list_companies_for_process` 走 `t_outsource_company_process` 反向 JOIN，O(1)。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_companies / create_company / get_company / update_company / soft_delete_company / list_companies_by_process / set_company_processes` + `company_router()`
- service：`src/modules/outsource/service.rs::OutsourceService::list_companies / create_company / get_company / update_company / soft_delete_company / list_companies_for_process / set_company_processes`
- repo：`src/modules/outsource/repo.rs`
- dto：`src/modules/outsource/dto.rs`
- model：`src/modules/outsource/model.rs::TOutsourceCompany`
- 路由挂载：`/outsource-companies`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

`tests/outsource_company_api.rs`（6+ 用例：CRUD happy / OCC 40901 / 软删被引用 21205 / by-process / set-processes 替换语义）