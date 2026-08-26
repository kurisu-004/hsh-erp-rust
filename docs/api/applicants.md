# applicants 域 API

> 本文件须与 `src/modules/applicant/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/applicants` | 已登录（**service 层强制 Manager / Clerk**） | 申请人列表（过滤 + 分页） |
| POST | `/api/v2/applicants` | 已登录（**service 层强制 Manager / Clerk**） | 新增申请人（`customer_id` 必须指向 L1） |
| GET | `/api/v2/applicants/{id}` | 已登录（**service 层强制 Manager / Clerk**） | 申请人详情（含 `customer_name`） |
| POST | `/api/v2/applicants/{id}/update` | 已登录（**service 层强制 Manager / Clerk**） | 部分更新（含乐观锁） |
| POST | `/api/v2/applicants/{id}/soft-delete` | 已登录（**service 层强制 Manager / Clerk**） | 软删（被 `t_part.applicant_name` 引用则拒） |

---

### `GET /api/v2/applicants`

权限: 已登录（**service 层强制 Manager / Clerk** —— 非该角色返 40300 FORBIDDEN）

Query：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `customer_id` | string? | — | 所属 L1 客户 id（雪花 ID 字符串） |
| `name_like` | string? | — | 姓名模糊匹配（`ILIKE %name%`） |
| `limit` | i64? | 100 | clamp [1, 500] |
| `offset` | i64? | 0 | |

Response 200 `data`：[`ApplicantListOut`](#applicantout-字段)

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [ApplicantOut] | 按 `id DESC` |
| `total` | i64 | 命中总数（与 `limit` / `offset` 无关） |
| `limit` | i64 | 实际生效的 limit |
| `offset` | i64 | 实际生效的 offset |

错误码：

- 40300 FORBIDDEN — 非 Manager / Clerk

### `POST /api/v2/applicants`

权限: 已登录（**service 层强制 Manager / Clerk**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | 申请人姓名（≤50 字符，trim 后非空） |
| `customer_id` | string | ✓ | 所属 **L1 客户** 的雪花 ID（必须 `parent_id IS NULL`） |

Request 示例：

```json
{ "name": "张三", "customer_id": "1891234567890123456" }
```

Response 201 `data`：[`ApplicantOut`](#applicantout-字段)（`version` 初始为 0）

错误码：

- 21003 BIZ_APPLICANT_BAD_CUSTOMER — `customer_id` 不存在 / 不是 L1
- 21002 BIZ_APPLICANT_DUPLICATE_NAME — 同一客户下姓名重复（DB partial unique 兜底）
- 40001 VALIDATION_ERROR — 姓名为空 / 非数字 ID
- 40300 FORBIDDEN — 非 Manager / Clerk

### `GET /api/v2/applicants/{id}`

权限: 已登录（**service 层强制 Manager / Clerk**）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 申请人雪花 ID |

Response 200 `data`：[`ApplicantOut`](#applicantout-字段)

错误码：

- 21001 BIZ_APPLICANT_NOT_FOUND — 申请人不存在 / 已软删
- 40300 FORBIDDEN — 非 Manager / Clerk

### `POST /api/v2/applicants/{id}/update`

权限: 已登录（**service 层强制 Manager / Clerk**）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string? | — | 缺省 / `null` = 不修改；trim 后非空 |
| `customer_id` | string? | — | 缺省 / `null` = 不修改；必须指向 L1 |

Request 示例（仅改名）：

```json
{ "name": "李四" }
```

Request 示例（同时改客户）：

```json
{ "name": "李四", "customer_id": "1891234567890123456" }
```

Response 200 `data`：[`ApplicantOut`](#applicantout-字段)（`version` +1）

错误码：

- 21001 BIZ_APPLICANT_NOT_FOUND — 申请人不存在 / 已软删
- 21003 BIZ_APPLICANT_BAD_CUSTOMER — 新 `customer_id` 不是 L1 / 不存在
- 40901 VERSION_CONFLICT — 数据已被并发修改（service 内部 OCC 撞 0 行）
- 40001 VALIDATION_ERROR — `name` 显式传空字符串 / `customer_id` 非数字
- 40300 FORBIDDEN — 非 Manager / Clerk

> 乐观锁版本号由后端内部管理（请求体**不**接受 `version` 字段），冲突由 service 在
> UPDATE `WHERE id=$1 AND version=$2` 撞 0 行时返回 `40901`。

### `POST /api/v2/applicants/{id}/soft-delete`

权限: 已登录（**service 层强制 Manager / Clerk**）

Request: 无 body。

Response 200 `data`：`null`

错误码：

- 21001 BIZ_APPLICANT_NOT_FOUND — 申请人不存在 / 已软删
- 21004 BIZ_APPLICANT_IN_USE — 被 `t_part.applicant_name` 引用（同一 `(name, customer_id)` 下有未软删零件）
- 40901 VERSION_CONFLICT — 数据已被并发修改
- 40300 FORBIDDEN — 非 Manager / Clerk

---

## 共享 DTO

### ApplicantOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 申请人雪花 ID |
| `name` | string | 姓名 |
| `customer_id` | string (i64) | 所属 L1 客户雪花 ID |
| `customer_name` | string? | L1 客户名称（service 一次性 join `t_customer` 补，避免 N+1） |
| `version` | i32 | 乐观锁版本号；创建=0，每次成功 UPDATE +1 |
| `created_at` | naive datetime | 服务端写入时间（Asia/Shanghai） |
| `updated_at` | naive datetime | 服务端写入时间（Asia/Shanghai） |

### ApplicantListOut 字段

见 [GET /applicants](#get-api-v2applicants) 响应小节。

---

## 业务规则（与 Python myERP 对齐）

- **角色**：Manager + Clerk 可读写；service 层 `require_any_role(&[Manager, Clerk])` 守卫
- **L1 校验**：`customer_id` 必须指向 L1（`parent_id IS NULL`），L2 拒收
- **重名**：(name, customer_id) 在 active 行上唯一（DB partial unique `uq_t_applicant_name_customer_active` 兜底；前置 service 层先查再写以给更友好错误码）
- **乐观锁**：所有写路径带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，0 行影响 → `40901 VERSION_CONFLICT`
- **软删**：被 `t_part.applicant_name` 引用（同一 `(name, customer_id)`）时拒软删，避免遗留孤悬引用
- **无状态机**：本域无状态流转
