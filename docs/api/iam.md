# iam 域 API（认证 + 账号 + 企业微信绑定 + 货架实体）

> 本文件是 iam 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 覆盖域：`src/modules/iam/`（`handler` / `dto` / `vo` / `service` / `repo`）+
> 嵌套子模块 `src/modules/iam/shelf/`（**货架子模块**，见 §1.4）。
> 域外有一处开口：`modules/wx/auth.rs` 的 `POST /api/v2/wx/iam/wx-login` 经
> `AccountService::resolve_wx_login_user` 反查本域的 `t_wx_identity`（见 §5）。

## 1. 端点表

全部端点挂在 `/api/v2/iam`（`modules::v2_router()` 的一处顶层 nest）。信封统一为
`R { code, message, data }`。**本域零 WS 事件**（见 §6）。

### 1.1 session（5）

| # | 方法 | 路径 | 权限 | 事务 | 入参 | 成功码 | 响应 `data` |
|---|---|---|---|---|---|---|---|
| 1 | POST | `/iam/login` | 公开 | begin/commit | `{ username, password }` | 200 | `LoginResponse` |
| 2 | GET | `/iam/me` | 登录即可 | acquire（不开事务） | — | 200 | `CurrentUserOut` |
| 3 | POST | `/iam/logout` | 登录即可 | 无 DB | — | 200 | `LogoutResponse` |
| 4 | POST | `/iam/change-password` | 本人或 Manager | begin/commit | `{ old_password, new_password }` | 200 | `null` |
| 5 | POST | `/iam/refresh` | 公开（凭 refresh token） | begin/commit | `{ refresh_token }` | 200 | `LoginResponse` |

### 1.2 users（12）

| # | 方法 | 路径 | 权限 | 事务 | 入参 | 成功码 | 响应 `data` |
|---|---|---|---|---|---|---|---|
| 6 | GET | `/iam/users` | Manager | acquire | `?username_like=&is_active=&limit=&offset=` | 200 | `UserListOut` |
| 7 | POST | `/iam/users` | Manager | begin/commit | `{ username, password, full_name, phone? }` | **201** | `UserOut` |
| 8 | GET | `/iam/users/{id}` | Manager | acquire | — | 200 | `UserOut` |
| 9 | POST | `/iam/users/{id}/update` | Manager | begin/commit | `{ version, full_name?, phone?, password?, is_active? }` | 200 | `UserOut` |
| 10 | POST | `/iam/users/{id}/reset-password` | Manager | begin/commit + commit 后清 Redis | **无 body** | 200 | `UserOut` |
| 11 | POST | `/iam/users/{id}/deactivate` | Manager | begin/commit | `{ version }` | 200 | `UserOut` |
| 12 | GET | `/iam/users/{id}/roles` | Manager | acquire | — | 200 | `UserRoleOut[]` |
| 13 | POST | `/iam/users/{id}/roles` | Manager | begin/commit | `{ role, scope_type?, scope_id? }` | **201** | `UserRoleOut` |
| 14 | POST | `/iam/users/{id}/roles/{role_id}/remove` | Manager | begin/commit | `{ version }` | 200 | `null` |
| 15 | GET | `/iam/users/{id}/wx-bind` | Manager | acquire | — | 200 | `WxIdentityOut \| null` |
| 16 | POST | `/iam/users/{id}/wx-bind` | Manager | begin/commit | `{ wx_user_id }` | 200 | `WxIdentityOut` |
| 17 | POST | `/iam/users/{id}/wx-bind/unbind` | Manager | begin/commit | `{ version }` | 200 | `null` |

**读端点 vs 写端点的事务口径**：`GET` 端点用 `pool.acquire()`（不开事务），写端点
显式 `state.pool.begin()` / `tx.commit()`，错误路径靠 `tx` drop 隐式回滚。事务边界
一律在 handler，service 不知事务（`CLAUDE.md` 架构约定第 1 条）。

### 1.3 本次的 4 条破坏性变更（2026-10-10）

| 变更 | 旧 | 新 | 前端影响 |
|---|---|---|---|
| **解绑路由 + 返回值** | `DELETE /iam/users/{id}/wx-bind` → `R<Vec<WxIdentityOut>>` | `POST /iam/users/{id}/wx-bind/unbind` + body `{version}` → `R<()>`（旧路径 405，**无 alias**） | 换 method + 换 URL + 加 body；返回值从数组改 `null` |
| **查询绑定返回值** | `R<Vec<WxIdentityOut>>`（未绑定 = `[]`） | `R<Option<WxIdentityOut>>`（未绑定 = `data: null`） | Zod schema 从数组改可空对象 |
| **绑定入参** | `{ wx_user_id, corp_id? }`（`corp_id` 是「保留字段、一律忽略」） | `{ wx_user_id }` | 旧客户端继续传 `corp_id` **不报错**（serde 忽略未知字段），落库值不变 |
| **OCC 锚点** | 4 个写端点 body 无 `version` | 端点 9 / 11 / 14 / 17 的 body **必填** `version` | 缺失 → **HTTP 422 纯文本** |

端点 10（`reset-password`）是 OCC 豁免的幂等端点、端点 13（`add_role`）是纯 INSERT
（新行没有 `version`），两者**不收** `version`。

### 1.4 shelves（5，2026-10-10 自独立的 shelf 域迁入）

| # | 方法 | 路径 | 权限 | 事务 | 入参 | 成功码 | 响应 `data` |
|---|---|---|---|---|---|---|---|
| 18 | GET | `/iam/shelves` | Manager + Clerk + CncProgrammer + ShelfAccount + Inspector | acquire | `?code_like=&zone=&is_active=&limit=&offset=` | 200 | `ShelfListOut` |
| 19 | POST | `/iam/shelves` | **Manager 独占** | begin/commit | `{ code, name, zone, location?, capacity?, display_order? }` | **201** | `ShelfOut` |
| 20 | GET | `/iam/shelves/{id}` | 同端点 18 | acquire | path `id` | 200 | `ShelfOut` |
| 21 | POST | `/iam/shelves/{id}/update` | **Manager 独占** | begin/commit | `{ name?, location?, capacity?, display_order?, version }` | 200 | `ShelfOut` |
| 22 | POST | `/iam/shelves/{id}/deactivate` | **Manager 独占** | begin/commit | 无 body | 200 | `null` |

⚠️ **硬切无 alias**：旧前缀 `/api/v2/shelves/*` 整体下线，5 条端点全部 404
（不带 `/iam` 前缀时连 `/{id}` catch-all 都不存在，故是干净的 404 而非 400）。
请求 / 响应 / 错误码 / OCC 语义**逐字未变**，只有 URL 前缀变了。

**归属缘由**：账号与货架同属「谁能碰什么」的权限资源 —— `t_user_role` 里
`SHELF_ACCOUNT` 角色的 `scope_id` 指向某个货架，货架实体是这套权限体系的落点；
5 条写端点又全部 MANAGer 独占，与账号 / 角色管理同一批授权动作。

不动的东西：工序映射（`prod::shelf_process`，`t_shelf_process` 关联的是 prod 域实体
`t_process`）与选架设施（`shared::shelf`，跨域设施层、无域归属）**均不搬**。

`t_shelf` 迁入后行结构收口为一份（`iam::shelf::model::TShelf`，14 列含 `capacity`）：
原先 iam 账号侧那份 13 列投影已删除，`IamRepoTrait::get_shelf_by_id` 改为委托
`ShelfRepo::get_by_id`（两条 SQL 谓词逐字相同，都只过滤 `deleted_at`、不过滤
`is_active`）。**wire 契约不受影响** —— `ShelfOut` 的列集由 VO 决定。

货架实体这一块的**逐字段、口径表、负载聚合与选架算法、前端配套清单**见
[`shelves.md`](shelves.md)（本文件只登记归属与端点表，不重复字段口径）。

## 2. 逐字段

### 2.1 `UserOut`（端点 6/7/8/9/10/11）

| 字段 | 类型 | 来源 | 口径 |
|---|---|---|---|
| `id` | string | `t_user.id` | i64 → string（防 JS 精度截断） |
| `version` | number | `t_user.version` | **OCC 锚点**，端点 9/11 原样回传 |
| `username` | string | `t_user.username` | 建号时 trim + 转小写，唯一 |
| `full_name` | string | `t_user.full_name` | 建号必填非空；更新传空串 → 40001 |
| `phone` | string \| null | `t_user.phone` | 传空串 = **显式清空**（`None` 才是「不修改」） |
| `is_active` | boolean | `t_user.is_active` | 端点 11 软删后恒 `false` |
| `last_login_at` | string \| null | `t_user.last_login_at` | 每次 login / refresh / wx-login 戳一次（不动 `version`） |
| `created_at` / `updated_at` | string | 对应列 | naive timestamp（Asia/Shanghai），序列化为 RFC3339 无时区后缀 |
| `roles` | `UserRoleOut[]` | `t_user_role` LEFT JOIN `t_shelf` | 端点 6 走**一次**批量查询（`WHERE user_id = ANY($1)`）后按 `user_id` 分桶，非逐行查 |

### 2.2 `UserRoleOut`（嵌在 `UserOut.roles` / 端点 12 / 端点 13）

| 字段 | 类型 | 来源 | 口径 |
|---|---|---|---|
| `id` | string | `t_user_role.id` | i64 → string |
| `version` | number | `t_user_role.version` | **OCC 锚点**，端点 14 原样回传 |
| `role` | string | `t_user_role.role` | 5 个大写字面量之一（见 §3） |
| `scope_type` | string \| null | `t_user_role.scope_type` | 仅 `SHELF_ACCOUNT` 非空，值恒为 `"shelf"` |
| `scope_id` | string \| null | `t_user_role.scope_id` | i64 → string；`null` = 共享 HMI 通配（见 §3） |
| `shelf_code` / `shelf_name` | string \| null | `t_shelf.code` / `.name` | `LEFT JOIN`（含 `s.deleted_at IS NULL` 闸门）；非货架角色恒 `null` |

### 2.3 `WxIdentityOut`（端点 15 的 `data` / 端点 16 的 `data`）

| 字段 | 类型 | 来源 | 口径 |
|---|---|---|---|
| `id` | string | `t_wx_identity.id` | i64 → string |
| `corp_id` | string | `t_wx_identity.corp_id` | **恒取后端配置 `WECOM_CORPID`**（trim 后），请求体无法指定 |
| `wx_user_id` | string | `t_wx_identity.wx_user_id` | 入参 trim + **转小写**后落库（依据见 §8.3） |
| `user_id` | string | `t_wx_identity.user_id` | i64 → string，指向 `t_user.id` |
| `version` | number | `t_wx_identity.version` | **OCC 锚点**，端点 17 原样回传 |
| `created_at` | string | `t_wx_identity.created_at` | 软删不改它 |

⚠️ 本表**从不**存 `corpsecret` / `session_key`（`session_key` 企微侧拿到即丢）。

### 2.4 `CurrentUserOut`（端点 1/2/5 的 `data.user`）

| 字段 | 类型 | 来源 | 口径 |
|---|---|---|---|
| `id` | string | `t_user.id` | i64 → string |
| `username` / `full_name` / `is_active` | string / string / boolean | `t_user` | 与 `UserOut` 同源 |
| `roles` | string[] | `t_user_role.role` | 大写；未知值 warn 后**跳过**（宁可不识别也不 panic） |
| `shelf_ids` | string[] | SHELF_ACCOUNT 行的 `scope_id` | 只保留**货架 active 且 zone ∈ 白名单**的；i64 → string |
| `menus` | `MenuNodeOut[]` | `t_menu` JOIN `t_role_menu` | 已组树（见 §2.6） |

### 2.5 `LoginResponse`（端点 1/5）

`{ token, refresh_token, user: CurrentUserOut }`。access / refresh 双 token 均带
`jti`（UUID v4），服务端在 Redis 写两条 session 条目。**双 token 轮转由 rust 端
处理**：每次 refresh 都轮转 `t_user.refresh_token_version` + 换 jti，并把旧 refresh
jti 写入黑名单（TTL 对齐旧 token 剩余有效期），旧 jti 再次使用 → 40105 并连带清空
该用户全部 session（reuse detection）。

### 2.6 `MenuNodeOut`（递归 `children`）

`id`(string) / `version` / `parent_id`(string\|null) / `code` / `title` /
`path`(null) / `icon`(null) / `sort_order` / `children[]`。

组树规则：根与每层 children 按 `(sort_order, code)` 升序；`parent_id` 指向**不在
可见集合内**的父节点时该节点提升为根（孤儿兜底）。菜单与角色的对应在
`t_role_menu`，`seeds/menu.sql` 是其权威源。

## 3. 角色与货架 scope 口径

5 个角色（大写常量，`Role` ↔ 字符串的唯一真源是 `src/auth/rbac.rs`）：

| role | 含义 | scope |
|---|---|---|
| `MANAGER` | 超级权限（各域自行判断是否豁免） | 不接受 scope |
| `CLERK` | 文员 | 不接受 scope |
| `INSPECTOR` | 品检员 | 不接受 scope |
| `CNC_PROGRAMMER` | CNC 程序员 | 不接受 scope |
| `SHELF_ACCOUNT` | 货架一体机专用账号 | **必须** `scope_type="shelf"` + `scope_id` |

- 授予非 `SHELF_ACCOUNT` 角色时带 `scope_type` / `scope_id` → **40001**。
- 授予 `SHELF_ACCOUNT` 时缺 `scope_type="shelf"` 或 `scope_id` → **40001**。
- `scope_id` 指向的货架必须存在、`zone ∈ ALLOWED_SHELF_ZONES`
  （`PRODUCTION` / `INSPECTION`）、`is_active = true`，任一不满足 → **40400**。
- **`scope_id IS NULL` 的 SHELF_ACCOUNT 行 = 共享 HMI 通配**（登录态里的
  `shelf_wildcard = true`，可访问全部货架）。⚠️ **本仓写端点写不出这种行**：
  `validate_role_scope` 对 SHELF_ACCOUNT 强制要求 `scope_id`，缺了直接 40001。
  库里若存在，是 Python 端或人工写入的存量数据，本仓只读兼容（§8.1）。
- **scope / 角色变更不在请求即刻生效于鉴权层**：`CurrentUser` 的 `shelf_ids` /
  `roles` 来自 **Redis session 缓存**，不是请求时读 DB。管理端改完角色后最长滞后
  一个 session TTL（`REDIS_SESSION_TTL_SECONDS`，代码缺省 900 秒；**现网 `.env` 实配
  43200 秒 = 12 小时**）；执行一次
  `/iam/refresh` 会按 DB 重算并重写缓存，因而立即生效。`/iam/me` 每次都重读 DB，
  但它**不**改写缓存 —— 界面上看到的角色列表与鉴权实际生效的角色在 TTL 内可能不一致。

## 4. 错误码表

| code | 名称 | HTTP | 触发场景 |
|---:|---|---:|---|
| 20601 | `BIZ_USER_ACCOUNT_NOT_FOUND` | 404 | 目标账号不存在（含 `get_user` / `update` / `deactivate` / 绑定的目标校验） |
| 20602 | `BIZ_USER_DUPLICATE_USERNAME` | 409 | `username` 已被占用（显式查重 + 唯一索引兜底同码） |
| 20603 | `BIZ_USER_INACTIVE` | 400 | 账号已停用（本域当前无端点返回它，预留） |
| 20604 | `BIZ_USER_ROLE_DUPLICATE` | 409 | 同一 `(role, scope)` 重复授予（含 NULL scope） |
| 20605 | `BIZ_USER_ROLE_NOT_FOUND` | 404 | 角色行不存在，**或**不属于该用户（不泄露他人角色是否存在） |
| 20606 | `BIZ_USER_NO_ROLE` | 403 | 登录成功但未分配任何角色 |
| 40001 | `VALIDATION_ERROR` | 422 | 入参校验失败（空 userid、超长、空密码、scope 形态错……） |
| 40901 | `VERSION_CONFLICT` | 409 | **只**表达 OCC：客户端传的 `version` 与 DB 不符 |
| 40100 | `UNAUTHORIZED` | 401 | 缺 / 非法 Bearer，或 Redis session 不存在 |
| 40101 | `BIZ_AUTH_INVALID` | 401 | 登录失败（用户不存在 / 已软删 / 已停用 / 密码错**统一**此码与文案）；wx-login 的「绑定指向的账号已软删」也走此码 |
| 40103 | `REFRESH_INVALID` | 401 | refresh token 失效 / 版本不匹配 |
| 40104 | `OLD_PASSWORD_MISMATCH` | 401 | 自助改密的旧密码错 |
| 40105 | `SESSION_REVOKED` | 401 | session 不存在（已 logout / 改密 / 吊销）或 refresh jti 命中黑名单 |
| 40107 | `BIZ_WX_NOT_BOUND` | 403 | 企业微信 userid 未预绑定（**仅预绑定，不自动开户**），或返回 corpid 与配置不符 |
| 40108 | `BIZ_WX_BINDING_DUPLICATE` | 409 | 该 `(corp_id, wx_user_id)` 已绑到**另一个**系统账号（wx → system 方向） |
| 40109 | `BIZ_WX_NOT_CONFIGURED` | 503 | 后端 `WECOM_CORPID` / `WECOM_CORPSECRET` 未配置（服务未就绪，非调用方无权） |
| 40110 | `BIZ_WX_USER_ALREADY_BOUND` | 409 | 该系统账号已绑**另一个**企业微信 userid（system → wx 方向） |
| 40300 | `FORBIDDEN` | 403 | 角色不满足（`require_role`） |
| 40400 | `NOT_FOUND` | 404 | 角色 scope 校验里的货架缺失 / zone 不合法 / 货架停用 |
| （无 code） | 方法不允许 | 405 | 请求方法不被该路径支持（例：已下线的 `DELETE /iam/users/{id}/wx-bind`） |

货架端点（18~22）的错误码 `20501` / `20502` / `20503` / `20512` 由**调用方 service 层**
返回（`shared::error::code` 常量），逐条口径见 [`shelves.md`](shelves.md) 与
`src/modules/iam/shelf/service/crud.rs`；它们不进本表是因为货架的判序与文案与账号
CRUD 完全独立 —— 本表只覆盖 session / users 两段。

⚠️ **422 是纯文本，不是错误码**：body 是 axum 提取器的拒绝文本（如
`Failed to deserialize the JSON body into the target type: missing field \`version\``），
**没有** `{code, message, data}` 信封。断言这类响应必须用
`test_support::http::send_raw`（`send` 会在 JSON 解析处 panic）。端点表里带 `Json`
提取器的 10 个端点（1/4/5/7/9/11/13/14/16/17）的提取失败都属此类；其余 7 个端点没有
`Json` body：2 取 `CurrentUser`、3 取 `CurrentUser` + `SessionJti`、6 取
`Query<UserListQuery>`、8/10/12/15 只取 `Path<i64>`。`Path` 与 `Json` 并存的是 9/11/13/17
（`Path<i64>`）与 14（`Path<(i64, i64)>`）；`SessionJti` 仅端点 3。17 个端点都带 `State`，
`CurrentUser` 覆盖除公开端点 1/5 外的 15 个（它是 middleware 注入 extensions 的薄壳，
验签与 session 校验都在 middleware）。

## 5. 移除记录（2026-10-10）

| 移除项 | 替代 | 备注 |
|---|---|---|
| `/api/v2/shelves/*`（5 条端点的旧前缀，自独立的 shelf 域继承） | `/api/v2/iam/shelves/*` | **旧路径 404，无 alias**。域归属迁移，契约逐字不变 |
| `DELETE /iam/users/{id}/wx-bind` | `POST /iam/users/{id}/wx-bind/unbind` | **旧路径 405，无 alias**。本仓只用 GET + POST，不留全后端唯一的 DELETE 路由 |
| `WxBindRequest.corp_id`（保留字段，一律忽略） | 无（`corp_id` 恒取 `WECOM_CORPID`） | 旧客户端继续传不报错（serde 忽略未知字段） |
| `GET /iam/users/{id}/wx-bind` 的数组返回 | 单对象 / `null` | 业务上双向一对一 |
| `GET /iam/users/{id}/wx-bind` 的「账号存在性校验」 | 无 | 查不存在的账号返 `data: null` 而非 404（端点语义就是「查绑定」） |
| 3 个 wx service 方法的 `&mut PgConnection` 签名 | 泛型 `R: IamRepoTrait` | 进 trait 后才拿到单测 mock 面；`t_wx_identity` 的 SQL 真源同步从 `modules/wx/repo.rs` 搬进 `modules/iam/repo/sql/wx_identity.rs` |

**域外开口**（不是移除，是收口）：`modules/wx/auth.rs` 的 wx-login 过去直接调
`WxIdentityRepo::get_by_corp_and_user` + `iam::repo::sql::user::get_user_by_id`，
现在两步合进 `AccountService::resolve_wx_login_user`。**错误码语义逐字未变**
（未绑定 40107、账号已软删 40101）：`tests/wecom_login.rs` 的 10 个 `wx_login_*`
场景零改动通过；同文件另 2 个 bind/unbind 场景按新契约改写（bind 新增 system → wx
冲突检查后需先建新账号再绑、unbind 改 `POST .../unbind` + body `version`，
且 `GET wx-bind` 返单对象）。

## 6. 与 WS 的关系

**iam 域零 WS 事件。** 依据：

- 全仓唯一的 WS 端点是 `GET /ws/dashboard`，其广播源在 `modules/dashboard/`；
- iam 的写端点（含改密 / 角色变更 / 解绑）没有任何 `state.ws_hub.broadcast(...)`
  调用 —— 鉴权上下文走 Redis session，轮转即刻生效，不需要推送；
- 账号 / 角色的变化由前端在下一次请求（或一次 refresh）时重新拉 `/iam/me` 得到。

推论：前端**不要**指望 WS 推「你的权限变了」。权限相关的实时性靠 §3 说的
session TTL / refresh。

## 7. 表依赖

| 表 | 读 | 写 | 说明 |
|---|---|---|---|
| `t_user` | 登录、账号 CRUD、`/me`、refresh | 建号、更新、软删停用、密码、戳 `last_login_at`、轮转 `refresh_token_version` | 无物理外键，存在性由 service 校验 |
| `t_user_role` | 角色解析、账号角色组装、查重 | 授予、软删撤销 | 唯一约束**非 partial**，见 §8.3 |
| `t_menu` + `t_role_menu` | 按角色取可见菜单并组树 | — | `seeds/menu.sql` 是菜单的权威源 |
| `t_shelf` | SHELF_ACCOUNT 的 scope 校验 + 登录态货架范围解析 + 端点 18~22 的 CRUD | 建架、改架、软删停用 | **2026-10-10 起写端点在本域内**（货架子模块自独立 shelf 域迁入，`ShelfRepo` 是本域自有 repo）；字段口径见 [`shelves.md`](shelves.md) |
| `t_wx_identity` | 绑定查询、wx-login 反查、system → wx 一对一判定（读该账号全部活跃行） | 绑定、解绑软删 | 唯一索引 `uk_wx_identity_corp_user` 是 **partial**（软删行不参与） |

另读 Redis（session 条目 + refresh jti 黑名单），键前缀由 `RedisConfig::key_prefix`
隔离（测试期按进程隔离）。

### 7.1 前端配套改动清单（本次破坏性变更）

1. **端点 9/11/14/17 的 body 加 `version`**，值分别来自 `UserOut.version`（9/11）、
   `UserRoleOut.version`（14）、`GET /iam/users/{id}/wx-bind` 的 `data.version`（17）。
   漏传得到 **422 纯文本**（不是 `{code: 40001}`），错误处理分支要与业务信封分开写。
2. **解绑换 `POST /iam/users/{id}/wx-bind/unbind` + body `{version}`**；旧 DELETE
   请求会拿到 **405**。
3. **端点 15 的 Zod schema 从 `array` 改 `object().nullable()`**；「未绑定」判据从
   `data.length === 0` 改成 `data === null`。
4. **新增 40110 分支**：绑定时报「该系统账号已绑其它 userid，请先解绑」——与 40108
   方向相反，UI 文案要区分（40108 = 这个 userid 已名花有主；40110 = 这个账号已占）。
5. `POST /iam/users/{id}/wx-bind` 的 `corp_id` 字段删掉（继续传也不报错）。
6. `/iam/users/{id}/reset-password`、`/iam/users/{id}/roles`（授予）**不需要**
   `version`。

## 8. 已知偏差登记

### 8.1 `t_user_role` 的唯一约束不是 partial

`uk_t_user_role_user_role_scope` 是**普通 UNIQUE 约束**（建表语句里没有
`WHERE deleted_at IS NULL` 谓词）。PostgreSQL 的 UNIQUE 视 NULL 与 NULL 互不相等，
故 `(user_id, role, NULL, NULL)` 这种含 NULL 的组合**可以被重复插入**：

- 现状（能正常工作）：「软删后可重新添加同一角色」成立；「软删前不重复」由 service
  的 `has_user_role_with_scope` 用 `IS NOT DISTINCT FROM` 预检兜住，撞唯一索引时
  翻成 20604。
- 残留风险：预检与 INSERT 之间仍有窗口（与 §8.2 同源问题），且含 NULL 的组合
  索引层完全拦不住。要彻底堵住需改约束为 partial unique（**独立决策**，涉及数据
  迁移，本轮未做）。
- ⚠️ 别把它读成「`scope_id IS NULL` 的 SHELF_ACCOUNT 行在本仓能写进去」：索引层
  拦不住 ≠ 服务层放行。`validate_role_scope` 对 SHELF_ACCOUNT 强制
  `scope_id.is_some()`，缺了直接 40001 ⇒ Rust 端**写不进**这种行。库里的通配行
  （登录态 `shelf_wildcard = true`，见 §3）是 Python 端或人工写入的存量数据，
  本仓只读兼容。

### 8.2 system → wx 一对一的 TOCTOU 窗口

端点 16 的 40110 检查是「先读该账号的全部活跃绑定行、再 `INSERT`」，**没有**
`uk_wx_identity_user_id` 这样的 partial unique 索引兜底，故两步之间的并发绑定会
穿透检查。本仓的处理是「靠管理端低并发兜住」（该端点只有 Manager 能调，是人工操作
界面，不存在高并发自动调用方）。要彻底堵住需追加一个 partial unique 索引
（**独立决策**）。wx → system 方向不受影响：`uk_wx_identity_corp_user` 是真索引，
并发插入撞它翻成 40108。

### 8.3 `wx_user_id` 归一化成小写的依据

归一化（trim + `to_lowercase`）的**官方依据是「发送应用消息」文档
（`https://developer.work.weixin.qq.com/document/path/90236`）**：返回包中的
userid 不区分大小写、统一转为小写。⚠️ **不是**登录流程（`jscode2session`）文档 ——
后者只说返回 `userid` 字面量。引用时不要引错文档。不归一会让 `ZhangSan` 与
`zhangsan` 落成两条绑定行。

### 8.4 `corp_id` 参与唯一索引的依据

`uk_wx_identity_corp_user` 把 `corp_id` 放进唯一键，依据是官方「userid **企业内**
唯一」——不同企业的同名 userid 是不同的人。当前**单企业部署**下 `corp_id` 是常量
（`WECOM_CORPID`），属为多企业预留。因此：

- 落库值恒取后端配置，请求体无法指定（避免写出永远登不进来的死行）；
- 真做多企业时，登录侧的 corpid 比对（`wx/auth.rs`）与绑定侧的常量取值都要改成
  按请求企业取值，本轮的 40107/40109 分支也要重新审一遍。

### 8.5 存量多行绑定账号

`GET` 只返 `created_at ASC, id ASC` 的**第一行**，解绑**全清**该账号所有活跃行
（无 `uk_wx_identity_user_id`，历史上可能留下多行）。这两条口径不一致是刻意的：
读端点要单值（UI 只显示一个），写端点要幂等安全（一次解绑让该账号彻底失去所有
企微身份）。多行数据只可能来自 8.2 描述的窗口。

**多行解绑的 OCC 锚点只落在首行**：端点 17 的 `version` 来自 `GET wx-bind` 返回的
那一个值，解绑时**只**把它打在 `created_at ASC, id ASC` 的首行上，其余行用同一事务
里读到的 `r.version`。否则一个可能与各行已分化的 version 会把非首行全判成 409，
而仓里没有 force-unbind 端点可救。
⚠️ **当前前提**：所有活跃行的 `version` 恒为 0 —— `create_wx_identity` 恒插
`version = 0`，而唯一推进它的 `soft_delete_wx_identity` 同时置 `deleted_at`，
故不存在「保留行被推进 version」的状态，首行 / 非首行的取值差异今天不可观测。
若将来新增「改绑定而不删行」的写入口（从而推进保留行的 version），本节口径要跟着复核。

### 8.6 权限上下文滞后一个 session TTL

角色 / 货架 scope 的变更不在请求即刻生效于鉴权层（走 Redis 缓存，
`REDIS_SESSION_TTL_SECONDS` 代码缺省 900 秒、**现网 `.env` 实配 43200 秒 = 12 小时**）。
详见 §3。要立即生效需执行一次 `/iam/refresh`。
