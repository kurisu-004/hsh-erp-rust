# outsource-companies 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service.rs,model.rs,repo.rs,statemachine.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：外协公司 CRUD + 工序映射 + 对账页 sent-parts（company **8 端点**）。
> 2026-09-13 Phase 2 落地；2026-10-03 补读侧 `sent-parts`。
> 关联域：[`./outsource-quotes.md`](./outsource-quotes.md) / [`./outsource-shipments.md`](./outsource-shipments.md) / [`./outsource-sendable.md`](./outsource-sendable.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-companies` | Manager / Clerk / Inspector / CncProgrammer | 列表（按 L1 + 名称过滤 + 分页） |
| POST | `/api/v2/outsource-companies` | Manager / Clerk | 新建公司（含初始工序映射） |
| GET | `/api/v2/outsource-companies/{id}` | Manager / Clerk / Inspector / CncProgrammer | 公司详情（含工序映射） |
| GET | `/api/v2/outsource-companies/{id}/sent-parts` | Manager / Clerk | **对账页**：该公司已发出的零件一览（2026-10-03 新增） |
| POST | `/api/v2/outsource-companies/{id}/update` | Manager / Clerk | 部分更新（OCC） |
| POST | `/api/v2/outsource-companies/{id}/soft-delete` | Manager | 软删（被 part 引用 / 仍映射工序时拒） |
| GET | `/api/v2/outsource-companies/by-process/{process_id}` | Manager / Clerk / Inspector / CncProgrammer | 按工序反查活跃公司 |
| POST | `/api/v2/outsource-companies/{id}/processes` | Manager / Clerk | 整体替换工序映射（先软删旧 + 写新，单事务） |

> 路由顺序：`/by-process/{process_id}` 静态段必须在 `/{id}` catch-all 之前注册。
> `/{id}/sent-parts` 是 **2 段**路径，与 1 段的 `/{id}` 无 matchit 冲突，注册序不限。

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

### OutsourceSentPartListQuery 字段（`GET /{id}/sent-parts`）

| 字段 | 类型 | 说明 |
|---|---|---|
| `keyword` | string? | part 的 `drawing_no` / `name` ILIKE `%needle%`（复用 `part_keyword_search` 语义）；trim 后空串视为无过滤。**零命中返回 `items: []` / `total: 0`**（service 层兜住：SQL 的 `AND (cardinality($2::bigint[]) = 0 OR part_id = ANY($2))` 里空 id 数组会让整个 keyword 条件短路，不兜就会返回该公司的**全部** shipment） |
| `sent_from` | naive datetime? | `sent_at` 闭区间下界（含），ISO 串如 `2026-09-01T00:00:00` |
| `sent_to` | naive datetime? | `sent_at` 闭区间上界（含） |
| `received_from` | naive datetime? | `received_at` 闭区间下界（含） |
| `received_to` | naive datetime? | `received_at` 闭区间上界（含） |
| `sort_by` | string? | 白名单 `PRICE` / `SENT_AT` / `RECEIVED_AT`，**大小写不敏感**（`price` 与 `PRICE` 等价）；**非法值回落 `SENT_AT`（不报错）** |
| `sort_dir` | string? | `ASC` / `DESC`，**大小写不敏感**；**非法值回落 `DESC`** |
| `limit` | i64? | 默认 50，clamp(1, 200) |
| `offset` | i64? | 默认 0，max(0) |

> ⚠️ `sort_by` / `sort_dir` **绝不拼进 SQL 文本**：service 先 `trim` +
> `to_ascii_uppercase`，再把用户输入映射成上表 3 个 + 2 个白名单 token 之一，
> bind 进 SQL 的 `CASE WHEN $7::text = 'PRICE' …`。传 `'; DROP TABLE --` 一类
> 注入串只会静默回落到默认值（见 `tests/outsource/shipment.rs` 的
> `sent_parts_illegal_sort_by_falls_back_to_sent_at_desc_without_injection`
> —— 该用例断言**行序**，不只是「没报错」）。

> **已知限制（不在本轮修）**：`keyword` 不转义 SQL LIKE 通配符。
> `service::keyword_pattern` 直接 `%{kw}%`，不拒 `%` / `_` / `\`。注入面为 0
> （纯 bind），但 `?keyword=%` 等价于「不过滤」、`?keyword=_` 匹配任意单字符。
> 仓库正在形成「service 层拒通配符」的约定，本端点尚未跟进。

### OutsourceSentPartOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `shipment_id` | string (i64) | `t_outsource_shipment.id`。**字段名不是 `id`** —— 前端行编辑（`reconcile-update`）入参按此名取 |
| `version` | i32 | shipment 行 OCC（`reconcile-update` 必传） |
| `quote_id` | string (i64) | 关联报价 |
| `part_id` | string (i64) | 关联 part |
| `part_drawing_no` | string? | |
| `part_name` | string? | |
| `customer_path` | string? | 客户路径：有 L1 拼 `L1 / L2`，仅 L2 时给 L2 名，无客户 `null`。**2026-10-03 修复**：此前恒 `null` |
| `batch_no` | i32? | 历史行可能 `null`（shipment 未绑批次） |
| `process_id` | string (i64) | 外协加工的工序 |
| `process_name` | string? | |
| `quantity` | i32 | 本次发货数量 |
| `unit_price` | string | Decimal 字符串（2 位小数） |
| `total_price` | string | Decimal 字符串 = `unit_price × quantity`（Rust `Decimal` 算，非 SQL） |
| `sent_at` | naive datetime | |
| `received_at` | naive datetime? | 未签收为 `null` |
| `status` | string | `OUTSOURCING` / `RECEIVED` |
| `is_billed` | bool | |
| `is_urgent` | bool | 所属零件的加急标记（前端加急红底用） |

### OutsourceSentPartListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `OutsourceSentPartOut[]` | |
| `total` | i64 | 全量命中行数（与 items 同 WHERE 口径） |
| `limit` | i64 | 回显（clamp 后） |
| `offset` | i64 | 回显 |

---

## 端点契约要点

### 列表 + 详情

- `GET /` 走 `OutsourceService::list_companies`（含分页）。
- `GET /{id}` 返回 `OutsourceCompanyWithProcessesOut`（含工序数组）。
- `GET /by-process/{process_id}` 返回 `OutsourceCompanyOut[]`（无分页，调用方一般用于下拉）。
- `GET /{id}/sent-parts` 返回 `OutsourceSentPartListOut`。行范围 = 该公司
  `status IN ('OUTSOURCING','RECEIVED')` 且未软删的 shipment（对账页只看这两态）。
  配套写端点 [`POST /{id}/reconcile-update`](./outsource-shipments.md) 原地改
  `unit_price` / `quantity` / `is_billed`。

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
- `GET /{id}/sent-parts` 是**读端点**：`pool.acquire()` **不开事务**，service 借 `&mut *conn` 跑两条查询。
- WS 广播在 `tx.commit().await?` **之后**（对齐 Python 延迟广播模式）；本域**无** WS 事件（只读资产）。

### 防 N+1

- `list_companies` 单 SQL 取所有行（无 JOIN 工序）；调用方需工序数组时走详情 / by-process。
- `list_companies_for_process` 走 `t_outsource_company_process` 反向 JOIN，O(1)。
- `list_company_sent_parts` **一条 SQL** 把 part / 客户(L2+L1) / 工序 / 批次号 / 加急标记
  全部 JOIN 出来，service 只做 Decimal 乘法与 VO 组装；`total` 走第二条同 WHERE 的
  `COUNT(*)`。**不带 `keyword` 时全程 2 次往返**（list + count），与 `limit`/`offset`
  无关；**带 `keyword` 时是 3 次** —— `part_keyword_search`（`t_part` 上
  `drawing_no OR name ILIKE`，取回命中 part 的 id 列表）+ list + count。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_companies / create_company / get_company / list_company_sent_parts / update_company / soft_delete_company / list_companies_by_process / set_company_processes` + `company_router()`
- service：`src/modules/outsource/service/{company,shipment}.rs::OutsourceService::list_company_sent_parts`（其余在 `company.rs`）
- repo：`src/modules/outsource/repo/{mod,sql}.rs`（`OutsourceShipmentRepo::list_for_company` / `count_for_company`）
- dto：`src/modules/outsource/dto.rs`
- vo：`src/modules/outsource/vo/shipment.rs::OutsourceSentPartOut / OutsourceSentPartListOut`
- model：`src/modules/outsource/model.rs::TOutsourceCompany`
- 路由挂载：`/outsource-companies`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

- `tests/outsource/company.rs`（6+ 用例：CRUD happy / OCC 40901 / 软删被引用 21205 / by-process / set-processes 替换语义）
- `tests/outsource/shipment.rs`（9 用例：`sent-parts` happy path / keyword 命中 /
  **keyword 零命中返 0 行**（带「无 keyword 返全量」对照组，证明断言没写死）/
  日期窗 / 3 种 sort_by / **sort 白名单大小写不敏感** / 注入串回落**并验行序**（3 行，
  `sent_at` 升 / `received_at` 降 / `unit_price` 与 `sent_at` 不同向，4 种可能的
  排序结果两两不同，单行或同向数据都验不出来）/ 分页 total+offset，以及 in-flight 2 用例）
