# iam 域 API

> 本文件须与 `src/modules/iam/{handler.rs,dto.rs,service/{session,account}.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

> **2026-09-19 IAM 域合并**：原 `auth.md` + `users.md` 已合并为本文档。
> **2026-09-19 IAM 域收尾**：旧 alias `/api/v2/auth/*` + `/api/v2/users/*` 已下线，
> `/api/v2/iam/*` 成为 IAM 域唯一对外接口。`auth.md` + `users.md` 已删除。

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
| POST | `/api/v2/iam/users/{id}/wx-bind` | Manager | 绑定企业微信 userid（2026-09-29 新增） |
| GET | `/api/v2/iam/users/{id}/wx-bind` | Manager | 查该用户的企业微信绑定列表（2026-09-29 新增） |
| DELETE | `/api/v2/iam/users/{id}/wx-bind` | Manager | 解绑该用户的全部企业微信绑定（2026-09-29 新增） |

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
| `token` | string | JWT access token（默认 15min / 900s） |
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
| `menus` | [object] | 菜单树（递归 `children`），见下；**可见性口径与角色矩阵见 [`菜单可见性（2026-10-05）`](#菜单可见性2026-10-05)** |

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

#### 菜单可见性（2026-10-05）

`menus` 树 = `t_menu JOIN t_role_menu` 的拍平行再组装成树，SQL 真源
`src/modules/iam/repo/sql/menu.rs:8-31`：

```sql
SELECT DISTINCT m.* FROM t_menu m
JOIN t_role_menu rm ON rm.menu_id = m.id
WHERE rm.role = ANY($1)
  AND m.is_active = TRUE
  AND m.deleted_at IS NULL
  AND rm.deleted_at IS NULL
ORDER BY m.sort_order, m.code
```

三条注意：

1. **过滤条件是三段合取**：`m.is_active = TRUE` + `m.deleted_at IS NULL`（菜单侧）
   + `rm.deleted_at IS NULL`（授权侧）。任一为假该菜单即不可见 —— 所以**回收授权
   用软删 `t_role_menu.deleted_at` 就够了**，不必动 `t_menu`。
2. **父分组不会自动可见**：角色必须**显式持有父节点 code**，否则子节点会被
   `build_menu_tree`（`src/modules/iam/service/menu.rs:44-52`）的孤儿兜底提升为
   顶级节点。例如 INSPECTOR 保留了 `inspection_pending`（待品检），就必须同时保留
   它的父节点 `production_group`。
3. **多角色取并集**：一个用户有多个角色时是 `rm.role = ANY(...)` 的并集 + `DISTINCT` 去重，
   不是交集。

**角色 × 菜单规范矩阵**（2026-10-05 收紧后的真源，逐角色列出其可见 menuCode 全集）：

| 角色 | 可见 menuCode 全集 |
|---|---|
| `MANAGER` | `home`、`production_stats`、`customer_management`、`customers_list`、`applicants_list`、`order_group`、`parts_list`、`parts_new`、`assemblies_list`、`delivery_notes_manage`、`inspection_pending`、`delivery_dispatch`、`repair_receive`、`pending_programming`、`production_group`、`process_work_type`、`part_process_chain`、`worker_queue`、`workers_list`、`template_management`、`print_templates_designer`、`auth_group`、`users_list`、`shelves_list`、`outsource_list`、`outsource_companies_list`、`outsource_quotes_list`、`outsource_send_receive_list`、`floor_group` |
| `CLERK` | `home`、`customer_management`、`customers_list`、`applicants_list`、`order_group`、`parts_list`、`parts_new`、`assemblies_list`、`delivery_notes_manage`、`repair_receive`、`production_group`、`worker_queue`、`outsource_list`、`outsource_companies_list`、`outsource_quotes_list`、`outsource_send_receive_list` |
| `INSPECTOR` | `home`、`parts_list`、`assemblies_list`、`delivery_notes_manage`、`inspection_pending`、`delivery_dispatch`、`repair_receive`、`production_group`、`outsource_send_receive_list` |
| `CNC_PROGRAMMER` | `home`、`parts_list`、`pending_programming` |
| `SHELF_ACCOUNT` | `home`、`scan_badge`、`floor_group` |

> ⚠️ 本表按「`t_role_menu` 授权口径」列（= `seeds/menu.sql` 第 4 节白名单 + 4.7 回收段），
> 便于与 seed 对账；**实际渲染到 `menus` 树还要再过 `m.is_active = TRUE` 一关**：
> `assemblies_list` 虽在 MANAGER/CLERK/INSPECTOR 的授权行里，但 `t_menu.is_active = false`
> （seed 第 3.4 段 prod 停用，授权行刻意保留以备重新启用），故**不会**出现在响应里。
> `settings_root` 及其 3 个旧子菜单已被 seed 第 3.1 段软删、授权行由第 4.6 段硬删，故表中均无。

**真源与维护约定**：菜单可见性的唯一真源是 `seeds/menu.sql` —— 第 4 节
`INSERT ... ON CONFLICT (role, menu_id) DO NOTHING` 白名单（写权限）+
**4.7 段显式回收**（收权限）。注意 `t_role_menu` 是 **add-only** 语义：
授权写入走 `DO NOTHING`，从白名单里删掉某个 code **不会**回收生产库已存在的授权行，
菜单也就不会消失。**收紧权限必须在 4.7 段显式软删**（`deleted_at` + `version + 1`）。
`uk_t_role_menu_role_menu` 是 partial（`WHERE deleted_at IS NULL`），故软删后将来若把
某 code 加回白名单，INSERT 会插新行不撞键，旧软删行留作审计。

本次收紧（用户需求：工序工种/制定工序 → MANAGER，生产队列 → MANAGER + CLERK，品检三项全收回）
的逐菜单对照见 [`production/index.md`](./production/index.md#menucode-映射)。

### `POST /api/v2/iam/logout`

权限: 已登录

Request: 无

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `ok` | bool | 始终 `true` |

> 更新于 2026-09-23 重构
>
> 后端从 Bearer token 提取 JWT claims.jwt_id（即 jti，UUID v4），删除 Redis 中
> `session:tok:<jti>` 条目与用户 Set 索引中的对应成员；当前 token 立即失效，
> 后续 `/me` 返回 40105 SESSION_REVOKED。其他 token（同一用户的其他设备）不受影响。

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
- 40105 SESSION_REVOKED — **reuse detection 命中**（见下方 2026-09-23 重构说明）

> 更新于 2026-09-23 重构：refresh token rotation + reuse detection
>
> **轮转（rotation）** —— 每次 refresh 成功后：
>
> - 服务端把旧 refresh jti 写入 Redis 黑名单 `revoked:<old_refresh_jti>`（空值 + `EX <ttl>`），
>   TTL = `max(0, old_refresh_exp - now)`，与旧 refresh 自身剩余有效期对齐（最长 7d）。
> - DB 端 `t_user.refresh_token_version` +1，旧 refresh 因版本不匹配即时作废。
>
> **复用检测（reuse detection）** —— 任何时刻同一 refresh token 被再次使用：
>
> - 后端在 phase 1 解码 refresh 后立刻查黑名单，命中即视为 token 已被轮转过 → 40105 SESSION_REVOKED。
> - 进一步触发 `delete_all_user_sessions(user_id)` 强制下线该用户的所有 session（含
>   access + 其他设备上的 refresh），并打 `tracing::warn!` 日志
>   `ACCOUNT_SECURITY_EVENT refresh_token_reuse_detected`（含 user_id + jti）。
> - 闸位在 DB 版本校验之前，保证 40105 不会先被 40103 拦截而成为 dead branch。
>
> **access 闸** —— 闸位是**防御性的**：rotation 路径**只**黑名单 refresh jti
> （`complete_refresh` 调 `revoke_jti(&old_refresh_jti, ttl)`，access jti 不入黑名单）。
> 因此 `auth::middleware::verify_session_token` 的 `EXISTS revoked:<jti>` 闸
> 实际只对 refresh jti 命中；access token 在自然 TTL（默认 15min）内仍可用，
> 与标准 OAuth 行为一致。access 真要立即失效需新增 `/iam/refresh` 请求携带
> access token 字段并由 rotation 同步黑名单 access jti，本期未做。
>
> **部署注意事项**：
>
> - 黑名单是新引入的 Redis 数据结构，**无需迁移**；旧条目自然过期（最长 7d）。
> - `verify_session_token` 的闸位在 Redis 主条目查询之前，会让已 logout 但仍携带旧
>   access token 的请求直接返 40105（不再走 `cached.user_id != claims.subject` 路径），
>   客户端语义不变（40105 → 清 token → 跳登录）。
> - 上线顺序：先发后端 → Redis 启用黑名单 key（前向兼容：黑名单空时闸不命中）→
>   前端无需配合改动。

> 2026-09-23 重构补充：refresh token 算法同步切到 RS256；与 access token
> 共用同一私钥/公钥对。HS256 fallback 在服务端保留（仅验签端兼容历史 token，
> 签发端不再产出），过渡期结束后 cleanup PR 删除。

### Session 域错误码补充

- 40105 SESSION_REVOKED — 会话已被吊销（Redis 中不存在 / 已失效 / refresh reuse detection 命中）。前端应清除本地 token 并跳回登录页。

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

## 企业微信绑定端点（2026-09-29 新增）

供 [`POST /api/v2/wx/iam/wx-login`](./wx.md#post-apiv2wxiamwx-login) 使用的预绑定表
`t_wx_identity`。**仅预绑定，不自动开户**：管理员先把企业微信 userid 绑到某个系统账号，
该 userid 才能登录成功。

> 三个端点全部要求 **Manager**，权限在 service 层强制（对齐 `list_users` / `add_role`）。
> 唯一键为 `(corp_id, wx_user_id)` 的 **partial unique**（`WHERE deleted_at IS NULL`），
> 因此解绑后可重新绑定同一 userid；同一 userid 绑到**别的**系统账号会返回 40108。

### `POST /api/v2/iam/users/{id}/wx-bind`

权限: **Manager**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 系统账号 ID |

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `wx_user_id` | string | ✓ | 企业微信 userid；服务端会 trim + **转小写**（企微 userid 不区分大小写）；长度 1..=64 |
| `corp_id` | string? | — | **保留字段，当前被忽略**（2026-09-29 起一律以后端 `WECOM_CORPID` 为准；传了不一致的值只打服务端 warn）。后端 `WECOM_CORPID` 为空 → 40109 |

Response 200 `data`：`WxIdentity`（见下文共享 DTO）

幂等语义：

- 绑到**同一个** `user_id` 重复调用 → **200 成功**（返回既有绑定行，不新建）
- 绑到**别的** `user_id` → **40108**（HTTP 409）

错误码：

- 40001 VALIDATION_ERROR — `wx_user_id` 空白 / 超 64 字符；后端 `WECOM_CORPID` 超 64 字符
- 20601 BIZ_USER_ACCOUNT_NOT_FOUND — 目标账号不存在
- 40108 BIZ_WX_BINDING_DUPLICATE — 该 userid 已绑到其它系统账号
- 40109 BIZ_WX_NOT_CONFIGURED — 后端 `WECOM_CORPID` 为空（请求体 `corp_id` 不能绕过）
- 40300 FORBIDDEN — 非 Manager

### `GET /api/v2/iam/users/{id}/wx-bind`

权限: **Manager**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 系统账号 ID |

Response 200 `data`：`[WxIdentity]`。该账号**无绑定**时返回**空数组**（不是 404）。
不校验目标账号是否存在（语义是「列绑定集合」）。

### `DELETE /api/v2/iam/users/{id}/wx-bind`

权限: **Manager**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 系统账号 ID |

Request: 无

Response 200 `data`：`[WxIdentity]` —— 本次**被软删**的绑定行（无绑定时为空数组）。

幂等语义：该账号当前无绑定时重复 DELETE → **200 + 空数组**（不报 404）。
解绑走 `soft_delete`（`deleted_at` + 乐观锁 `version`），**不物理删除**。

错误码：

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

### WxIdentity 字段（`t_wx_identity`，2026-09-29 新增）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 绑定行 ID（雪花） |
| `corp_id` | string | 企业 ID（永远等于后端 `WECOM_CORPID`） |
| `wx_user_id` | string | 企业微信 userid（已 trim + 转小写） |
| `user_id` | string (i64) | 系统账号 ID |
| `version` | i32 | 乐观锁；`DELETE` 响应里是**软删之后**的值（已 +1） |
| `created_at` | naive datetime | 绑定时间 |

> 不返回 `corpsecret` / `session_key` —— 本表也从不存这两样
> （`session_key` 拿到即丢，见 `src/modules/wx/wecom_client.rs`）。
