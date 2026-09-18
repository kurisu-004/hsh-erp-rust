# iam 域 API

> 本文件须与 `src/modules/iam/{handler.rs,dto.rs,service/{session,account}.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

> ⚠️ **2026-09-19 IAM 域合并（PR-1）**：原 `auth.md` + `users.md` 已合并为本文档。
> 新路径 `/api/v2/iam/*`；保留旧 alias `/api/v2/auth/*` + `/api/v2/users/*` 至 PR-4 删除。
> - [`./auth.md`](./auth.md) — 已废弃，保留为兼容期旧 alias 参考
> - [`./users.md`](./users.md) — 已废弃，保留为兼容期旧 alias 参考

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/iam/login` | 公开 | 用户名密码登录，返回 access+refresh token |
| GET | `/api/v2/iam/me` | 已登录 | 当前用户信息 + 菜单树 |
| POST | `/api/v2/iam/logout` | 已登录 | 删除当前 token 的 Redis session，立即生效；后续 `/me` 返回 40105 |
| POST | `/api/v2/iam/change-password` | 已登录 | 改自己密码 |
| POST | `/api/v2/iam/refresh` | 公开 | refresh token 换新 access+refresh pair |
| GET | `/api/v2/iam/users` | 已登录（service 层强制 Manager） | 账号列表（带过滤分页） |
| POST | `/api/v2/iam/users` | Manager | 创建账号 |
| GET | `/api/v2/iam/users/{id}` | 已登录 | 账号详情（含角色） |
| POST | `/api/v2/iam/users/{id}/update` | Manager | 部分更新（含乐观锁） |
| POST | `/api/v2/iam/users/{id}/reset-password` | Manager | 重置密码为默认 `"changeme"` |
| POST | `/api/v2/iam/users/{id}/deactivate` | Manager | 停用账号 |
| GET | `/api/v2/iam/users/{id}/roles` | 已登录 | 该用户的角色列表 |
| POST | `/api/v2/iam/users/{id}/roles` | Manager | 给用户添加角色 |
| POST | `/api/v2/iam/users/{id}/roles/{role_id}/remove` | Manager | 移除用户角色 |

---

## Session 端点（auth 域原 5 端点）

### `POST /api/v2/iam/login`

权限: **公开**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `username` | string | ✓ | 用户名 |
| `password` | string | ✓ | 密码 |

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `token` | string | JWT access token（默认 12h） |
| `refresh_token` | string | JWT refresh token（默认 7d） |
| `user` | object | 见 [`GET /iam/me`](#get-apiv2iamme) |

错误码：

- 40101 BIZ_AUTH_INVALID — 用户不存在 / 已删 / 已停用 / 密码错
- 20606 NO_ROLE — 账号未分配角色
- 403 FORBIDDEN — 未分配任何已知角色

### `GET /api/v2/iam/me`

权限: 已登录

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `username` | string | |
| `full_name` | string | |
| `is_active` | bool | |
| `roles` | [string] | 角色枚举（`MANAGER`/`CLERK`/`INSPECTOR`/`CNC_PROGRAMMER`/`SHELF_ACCOUNT`） |
| `shelf_ids` | [string (i64)] | 货架一体机可访问的货架 ID 列表 |
| `menus` | [object] | 菜单树（递归 `children`），见下 |

`menus[]` 节点字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `version` | i32 | |
| `parent_id` | string (i64)? | |
| `code` | string | |
| `title` | string | |
| `path` | string? | 前端路由 |
| `icon` | string? | Element Plus 图标名 |
| `sort_order` | i32 | |
| `children` | [object] | 递归子节点 |

### `POST /api/v2/iam/logout`

权限: 已登录

Request: 无

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `ok` | bool | 始终 `true` |

> 后端从 Bearer token 解析 sha256，删除 Redis 中 `session:tok:<hash>` 条目与
> 用户 Set 索引中的对应成员；当前 token 立即失效，后续 `/me` 返回 40105 SESSION_REVOKED。
> 其他 token（同一用户的其他设备）不受影响。

### `POST /api/v2/iam/change-password`

权限: 已登录（改自己的密码）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `old_password` | string | ✓ | |
| `new_password` | string | ✓ | |

Response 200 `data`: `null`

错误码：

- 40104 OLD_PASSWORD_MISMATCH — 旧密码错误

### `POST /api/v2/iam/refresh`

权限: **公开**（带 `refresh_token`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `refresh_token` | string | ✓ | |

Response 200 `data`：同 [`/iam/login`](#post-apiv2iamlogin)

错误码：

- 40103 REFRESH_INVALID — refresh 失效 / 版本不匹配 / 用户已停用

### Session 域错误码补充

- 40105 SESSION_REVOKED — 会话已被吊销（Redis 中不存在 / 已失效）。前端应清除本地 token 并跳回登录页。

### 服务端 session 校验开关

环境变量 `REDIS_SESSION_CHECK_ENABLED`（默认 `true`）控制 Rust 后端是否在每次请求中校验 Redis 服务端 session：

- `true`（默认）：登录/refresh 时把 token hash 写入 Redis；每次请求查 Redis 校验；
  logout/change_password 删 Redis 条目强制吊销。40105 SESSION_REVOKED 仍会触发。
- `false`：不建 Redis 连接池；所有 session 写入走 NoopSessionStore（silent 成功但不持久化）；
  extractor 直接从 JWT Claims 构造 CurrentUser，**不再返回 40105**。
  仅当 Rust 后端在借 Python 后端签发的 JWT 时使用——切回 `true` 后所有已发 token 必须重新登录。

---

## Account 端点（users 域原 9 端点）

### `GET /api/v2/iam/users`

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

### `POST /api/v2/iam/users`

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

### `GET /api/v2/iam/users/{id}`

权限: 已登录

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 用户雪花 ID |

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND

### `POST /api/v2/iam/users/{id}/update`

权限: **Manager**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `full_name` | string? | — | None/缺省 = 不改 |
| `phone` | string? | — | None/缺省 = 不改 |
| `password` | string? | — | None/缺省 = 不改（不轮转 refresh_token_version） |
| `is_active` | bool? | — | None/缺省 = 不改 |

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 40901 VERSION_CONFLICT — 数据已被他人修改
- 40001 VALIDATION_ERROR

### `POST /api/v2/iam/users/{id}/reset-password`

权限: **Manager**

Request: **无 body**（重置为默认密码 `"changeme"`，对齐 Python `DEFAULT_RESET_PASSWORD`）

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 40300 FORBIDDEN

### `POST /api/v2/iam/users/{id}/deactivate`

权限: **Manager**

Request: 无

Response 200 `data`：`UserDetail`

错误码：

- 20601 BIZ_USER_ACCOUNT_NOT_FOUND
- 20603 BIZ_USER_INACTIVE — 已停用

### `GET /api/v2/iam/users/{id}/roles`

权限: 已登录

Response 200 `data`：`[RoleAssignment]`（见下文 `UserDetail.roles[]` 节点）

### `POST /api/v2/iam/users/{id}/roles`

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

### `POST /api/v2/iam/users/{id}/roles/{role_id}/remove`

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

---

## 旧 alias（PR-1 兼容期，PR-4 删除）

PR-1 兼容期内保留以下旧 alias path（同组路由函数挂到不同 nest 上，前端 / 第三方客户端可继续使用）：

| 旧 alias 新 path | 等价于 |
|---|---|
| `POST /api/v2/auth/login` | `POST /api/v2/iam/login` |
| `GET /api/v2/auth/me` | `GET /api/v2/iam/me` |
| `POST /api/v2/auth/logout` | `POST /api/v2/iam/logout` |
| `POST /api/v2/auth/change-password` | `POST /api/v2/iam/change-password` |
| `POST /api/v2/auth/refresh` | `POST /api/v2/iam/refresh` |
| `GET /api/v2/users` | `GET /api/v2/iam/users` |
| `POST /api/v2/users` | `POST /api/v2/iam/users` |
| `GET /api/v2/users/{id}` | `GET /api/v2/iam/users/{id}` |
| `POST /api/v2/users/{id}/update` | `POST /api/v2/iam/users/{id}/update` |
| `POST /api/v2/users/{id}/reset-password` | `POST /api/v2/iam/users/{id}/reset-password` |
| `POST /api/v2/users/{id}/deactivate` | `POST /api/v2/iam/users/{id}/deactivate` |
| `GET /api/v2/users/{id}/roles` | `GET /api/v2/iam/users/{id}/roles` |
| `POST /api/v2/users/{id}/roles` | `POST /api/v2/iam/users/{id}/roles` |
| `POST /api/v2/users/{id}/roles/{role_id}/remove` | `POST /api/v2/iam/users/{id}/roles/{role_id}/remove` |

PR-4 删除：`src/modules/iam/handler.rs::auth_router()` / `users_router()` 工厂函数 + 旧 alias 路由表。
