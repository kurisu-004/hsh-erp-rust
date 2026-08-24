# auth 域 API

> 本文件须与 `src/modules/auth/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/auth/login` | 公开 | 用户名密码登录，返回 access+refresh token |
| GET | `/api/v2/auth/me` | 已登录 | 当前用户信息 + 菜单树 |
| POST | `/api/v2/auth/logout` | 已登录 | no-op，返回 `{ ok: true }` |
| POST | `/api/v2/auth/change-password` | 已登录 | 改自己密码 |
| POST | `/api/v2/auth/refresh` | 公开 | refresh token 换新 access+refresh pair |

---

### `POST /api/v2/auth/login`

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
| `user` | object | 见 [`GET /auth/me`](#get-apiv2authme) |

错误码：

- 40101 BIZ_AUTH_INVALID — 用户不存在 / 已删 / 已停用 / 密码错

### `GET /api/v2/auth/me`

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

### `POST /api/v2/auth/logout`

权限: 已登录

Request: 无

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `ok` | bool | 始终 `true`（no-op） |

> 服务端无法强制吊销短期 access token。"登出"由前端清除本地 token 实现；后端通过 refresh 版本号轮转实现"实质登出"。

### `POST /api/v2/auth/change-password`

权限: 已登录（改自己的密码）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `old_password` | string | ✓ | |
| `new_password` | string | ✓ | |

Response 200 `data`: `null`

错误码：

- 40104 OLD_PASSWORD_MISMATCH — 旧密码错误

### `POST /api/v2/auth/refresh`

权限: **公开**（带 `refresh_token`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `refresh_token` | string | ✓ | |

Response 200 `data`：同 [`/auth/login`](#post-apiv2authlogin)

错误码：

- 40103 REFRESH_INVALID — refresh 失效 / 版本不匹配 / 用户已停用
