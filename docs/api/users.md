# users 域 API

> 本文件须与 `src/modules/user/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/users` | 已登录（service 层强制 Manager） | 账号列表（带过滤分页） |
| POST | `/api/v2/users` | Manager | 创建账号 |
| GET | `/api/v2/users/{id}` | 已登录 | 账号详情（含角色） |
| POST | `/api/v2/users/{id}/update` | Manager | 部分更新（含乐观锁） |
| POST | `/api/v2/users/{id}/reset-password` | Manager | 重置密码为默认 `"changeme"` |
| POST | `/api/v2/users/{id}/deactivate` | Manager | 停用账号 |
| GET | `/api/v2/users/{id}/roles` | 已登录 | 该用户的角色列表 |
| POST | `/api/v2/users/{id}/roles` | Manager | 给用户添加角色 |
| POST | `/api/v2/users/{id}/roles/{role_id}/remove` | Manager | 移除用户角色 |

---

### `GET /api/v2/users`

权限: 已登录（**service 层强制 Manager**）

Query：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `username_like` | string? | — | 模糊匹配 username |
| `is_active` | bool? | — | 过滤活跃状态 |
| `limit` | i64? | 50 | clamp [1, 500] |
| `offset` | i64? | 0 | |

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [UserDetail] | |
| `total` | i64 | |
| `limit` | i64 | 回显请求值 |
| `offset` | i64 | 回显请求值 |

错误码：

- 40300 FORBIDDEN — 非 Manager

### `POST /api/v2/users`

权限: **Manager**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `username` | string | ✓ | 唯一 |
| `password` | string | ✓ | |
| `full_name` | string | ✓ | |
| `phone` | string? | — | |

Response 201 `data`：[`UserDetail`](#userdetail-字段)

错误码：

- 20602 BIZ_USER_DUPLICATE_USERNAME — username 重复
- 40001 VALIDATION_ERROR — 角色 scope 用法错误等

### `GET /api/v2/users/{id}`

权限: 已登录

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 用户雪花 ID |

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND

### `POST /api/v2/users/{id}/update`

权限: **Manager**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `full_name` | string? | — | None/缺省 = 不改 |
| `phone` | string? | — | None/缺省 = 不改 |
| `password` | string? | — | None/缺省 = 不改 |
| `is_active` | bool? | — | None/缺省 = 不改 |

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 40901 VERSION_CONFLICT — 数据已被他人修改
- 40001 VALIDATION_ERROR

### `POST /api/v2/users/{id}/reset-password`

权限: **Manager**

Request: **无 body**（重置为默认密码 `"changeme"`，对齐 Python `DEFAULT_RESET_PASSWORD`）

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 40300 FORBIDDEN

### `POST /api/v2/users/{id}/deactivate`

权限: **Manager**

Request: 无

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 20603 BIZ_USER_INACTIVE — 已停用

### `GET /api/v2/users/{id}/roles`

权限: 已登录

Response 200 `data`：`[RoleAssignment]`（见下文 `UserDetail.roles[]` 节点）

### `POST /api/v2/users/{id}/roles`

权限: **Manager**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `role` | string | ✓ | 角色枚举（`MANAGER`/`CLERK`/...） |
| `scope_type` | string? | — | `SHELF_ACCOUNT` 必填 `"shelf"`；其它角色留空 |
| `scope_id` | i64? | — | `SHELF_ACCOUNT` 必填货架 ID；其它角色留空 |

Response 201 `data`：`RoleAssignment`

错误码：

- 20604 BIZ_USER_ROLE_DUPLICATE — 同一用户已有该角色
- 40001 VALIDATION_ERROR — scope 用法错误
- 40400 NOT_FOUND — 货架不存在 / 已停用 / 非合法 zone

### `POST /api/v2/users/{id}/roles/{role_id}/remove`

权限: **Manager**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 用户 ID |
| `role_id` | string (i64) | 角色分配 ID（不是角色枚举！） |

Request: 无

Response 200 `data`: `null`

错误码：

- 20605 BIZ_USER_ROLE_NOT_FOUND — 角色分配记录不存在
- 40300 FORBIDDEN — 非 Manager

---

## 共享 DTO

### UserDetail 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `version` | i32 | 乐观锁 |
| `username` | string | |
| `full_name` | string | |
| `phone` | string? | |
| `is_active` | bool | |
| `last_login_at` | naive datetime? | |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |
| `roles` | [RoleAssignment] | 见下 |

### RoleAssignment 节点字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 角色分配 ID（雪花） |
| `version` | i32 | 乐观锁 |
| `role` | string | 角色枚举 |
| `scope_type` | string? | `SHELF_ACCOUNT` 时固定 `"shelf"`，否则 `null` |
| `scope_id` | string (i64)? | 绑定的货架 ID（仅 `SHELF_ACCOUNT` 非空） |
| `shelf_code` | string? | 货架编号（仅 `SHELF_ACCOUNT` 非空） |
| `shelf_name` | string? | 货架名（仅 `SHELF_ACCOUNT` 非空） |
