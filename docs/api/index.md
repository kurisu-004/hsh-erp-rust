# Backend API v2 — 前端对接参考

> ⚠️ **本目录文件须与 `src/modules/*/{handler,dto}.rs` 保持同步**
>
> 后端代码变更（**新增 / 修改 / 删除端点**，或**修改 DTO 字段 / 错误码**）后，必须**立即**更新对应模块文件：
> - [`./iam.md`](./iam.md) — iam 域（auth + user 合并，2026-09-19；旧 alias `/auth` + `/users` 保留至 PR-4）
> - [`./auth.md`](./auth.md) — ⚠️ 已废弃，保留为兼容期参考
> - [`./users.md`](./users.md) — ⚠️ 已废弃，保留为兼容期参考
> - [`./applicants.md`](./applicants.md) — applicant 域（申请人 CRUD，2026-08-26）
> - [`./customers.md`](./customers.md) — customers 域（L1/L2 CRUD，2026-08-26）
> - [`./shelves.md`](./shelves.md) — shelves 域（CRUD + picker + mapping，2026-08-26）
> - [`./workers.md`](./workers.md) — workers 域（CRUD + verify-badge + deactivate/reactivate，2026-08-26）
> - [`./production/index.md`](./production/index.md) — **生产管理** 域（工种/工序/工序映射/工艺链/工人候选池；按前端 `production_group` 菜单整合为子目录，2026-09-12）
> - [`./parts/index.md`](./parts/index.md) — part 域（49 端点：to-inspection / to-ship 批量+单件 / to-process / **worker-scan** + Phase 1/2 全套批量与单件状态机扩展，2026-09-14）
> - [`./assemblies/index.md`](./assemblies/index.md) — assembly 域（8 端点：装配体 CRUD + multipart PDF + 子件自动生成 + start + 子件 auto-rollup，2026-09-14 Phase 3）
> - [`./cnc-programs.md`](./cnc-programs.md) — cnc_program 域（2 端点：配对上传 + 列表，2026-09-14 Phase 3）
> - [`./files.md`](./files.md) — part_file 域（multipart 上传 + 列表 + 下载 URL + confirm 绑定，2026-09-14 Phase 3；2026-09-18 删除 upload-intents）
> - [`./upload_session.md`](./upload_session.md) — upload_session 域（7 端点：共享 STS 凭证的 Redis 会话机制，2026-09-18 新增；替代原 `POST /part-files/upload-intents`）
> - [`./outsource-companies.md`](./outsource-companies.md) — 外协公司（7 端点，2026-09-13 Phase 2）
> - [`./outsource-quotes.md`](./outsource-quotes.md) — 外协报价（8 端点，2026-09-13 Phase 2）
> - [`./outsource-shipments.md`](./outsource-shipments.md) — 外协发货（1 端点：reconcile-update，2026-09-13 Phase 2）
> - [`./delivery-notes/index.md`](./delivery-notes/index.md) — delivery_notes 域（已拆为子目录：[queries](./delivery-notes/queries.md) / [drafts](./delivery-notes/drafts.md) / [workflow](./delivery-notes/workflow.md) / [print](./delivery-notes/print.md)）
> - [`./delivery-groups.md`](./delivery-groups.md) — delivery_groups 域
> - [`./_e2e.md`](./_e2e.md) — e2e seed hook（11 端点；dev/test 默认启用，release profile 硬关，2026-09-14）
> - [`./websocket.md`](./websocket.md) — WebSocket（含 **WORKER_SCAN_* / WORKER_POOL_* / 12+ 业务事件**）
>
> **同步流程**：
> 1. 修改 `src/modules/<mod>/{handler.rs,dto.rs,service.rs}`
> 2. 在 `src/shared/error.rs::code` 加新错误码（如适用）
> 3. **同步更新 `docs/api/<mod>.md`**（或拆分后的 `docs/api/<mod>/index.md`）对应小节
> 4. 在 PR 描述里点出 `docs/api/<mod>.md` 有变更

## 目录

1. [通用约定](#通用约定)
   - [响应信封 R<T>](#响应信封-rt)
   - [认证](#认证)
   - [五角色 RBAC](#五角色-rbac)
   - [主键与时间字段](#主键与时间字段)
   - [分页](#分页)
   - [错误码分段](#错误码分段)
2. [模块 API](#模块-api)
3. [未上线域（前端勿调用）](#未上线域前端勿调用)
4. [跨域错误码速查](#跨域错误码速查)
5. [维护约定](#维护约定)

---

## 通用约定

### 响应信封 R<T>

所有 HTTP 响应统一为 `R<T> { code, message, data }`：

```json
// 成功
{ "code": 0, "message": "ok", "data": <T> }

// 失败（普通）
{ "code": 21421, "message": "batch 123 is in invalid state: DELIVERED", "data": null }

// 失败（带失败明细 — 装配件整套拒绝 21418）
{
  "code": 21418,
  "message": "整套拒绝：含 2 个不可入单子件",
  "data": {
    "failures": [
      { "serial_no": "B01", "name": "fala-A", "reason": "status=IN_PROCESS" },
      { "serial_no": "B02", "name": "fala-B", "reason": "on note DN-20260821-0001" }
    ]
  }
}
```

HTTP 状态码：

| 类型 | HTTP | code |
|---|---|---|
| 成功 | 200 / 201 / 204 | 0 |
| 通用校验失败 | 422 | 40001 |
| 未授权 | 401 | 40100 / 40101 / 40102 / 40103 |
| 权限不足 | 403 | 40300 / 40301 / 20606 |
| 资源不存在 | 404 | 40400 + 2xxxx_NOT_FOUND |
| 状态冲突 | 409 | 40901 + 2xxxx_DUPLICATE / _IN_USE / _LOCKED |
| 请求体过大 | 413 | 41301 |
| 系统错误 | 500 | 50000 / 50001 |

> 详情见 `src/shared/error.rs::status_from_code`。

### 认证

- Header：`Authorization: Bearer <access_token>`
- 各端点小节里**标注"权限: 公开"**的端点无需登录；其余均需 Bearer JWT
- access token 默认 12h 过期；refresh token 默认 7d
- JWT 载荷含：`sub`（用户 i64）/ `roles`（[Role]）/ `shelf_ids`（[i64]）/ `shelf_wildcard`（bool）/ `ver`（用户版本号，用于 refresh 校验）
- **服务端 session**：每个 access / refresh token 必须对应 Redis 一条 `session:tok:<sha256_hex>`
  条目才视为有效。`CurrentUser` extractor 在 JWT 验签后额外查 Redis；
  查不到 → 40105 SESSION_REVOKED。登出（删当前 token 条目）、改密 / 管理员停用
  （清整个用户 Set `sessions:user:<id>`）都会触发吊销。前端拿到 40105 应清本地
  token 并跳回登录页。session 条目默认 TTL 12h，每次成功访问会 EXPIRE 续期（滑动窗口）。

> ⚠️ 当 `REDIS_SESSION_CHECK_ENABLED=false` 时（迁移过渡期），extractor 不查 Redis，
> 40105 SESSION_REVOKED 不会再触发；session 写入也走 no-op store。
> 见 `docs/api/auth.md` 末段。

### 五角色 RBAC

来源：`src/auth/rbac.rs::Role`

| 角色 | 字符串 | 说明 |
|---|---|---|
| Manager | `MANAGER` | 超级权限；后端 service 层校验 |
| Clerk | `CLERK` | 文员 |
| Inspector | `INSPECTOR` | 品检员 |
| CncProgrammer | `CNC_PROGRAMMER` | CNC 程序员 |
| ShelfAccount | `SHELF_ACCOUNT` | 货架一体机专用；必须 scope 到具体 shelf_id |

`ShelfAccount` 货架范围控制：

- `shelf_ids: [i64]` — 可访问的具体货架列表
- `shelf_wildcard: bool` — 是否对所有货架放行（仅 Manager）

### 主键与时间字段

- **雪花 ID** 在 JSON 里序列化为**字符串**（避免 JS `Number.MAX_SAFE_INTEGER` 精度截断）；输入时也接受字符串
- 时间字段为 `chrono::NaiveDateTime`（无时区，统一 Asia/Shanghai 输入输出）
- 日期字段为 `chrono::NaiveDate`（格式 `YYYY-MM-DD`）

### 分页

列表接口统一使用 query 参数：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `limit` | i64 | 50 | 用户域默认 50，送货单默认 50，clamp [1, 500]（用户域）/ [1, 200]（共享） |
| `offset` | i64 | 0 | |

响应体（含 `items, total, limit, offset` 四个字段，部分接口叫别的名字，详见各域）。

### 错误码分段

详见 `src/shared/error.rs::code` 模块顶部注释 + 常量定义。

| 段 | 含义 |
|---|---|
| `0` | 成功 |
| `4xxxx` | HTTP 语义（400/401/403/404/409/413） + 401xx Auth 业务码 |
| `5xxxx` | 系统（50000 INTERNAL / 50001 DATABASE） |
| `2xxxx` | 业务域错误 |

完整 2xxxx 业务码见 [跨域错误码速查](#跨域错误码速查)。

---

## 模块 API

| 模块 | 文件 | 端点数 | 状态 |
|---|---|---|---|
| auth + users | [`./iam.md`](./iam.md) | 14 | ✅ 完全上线（2026-09-19 合并为单一 iam 域） |
| applicants | [`./applicants.md`](./applicants.md) | 5 | ✅ 完全上线（CRUD + L1 customer 校验 + OCC，2026-08-26） |
| customers | [`./customers.md`](./customers.md) | 5 | ✅ 完全上线（CRUD + L1/L2 + OCC，2026-08-26） |
| shelves | [`./shelves.md`](./shelves.md) | 11 | ✅ 完全上线（CRUD + picker + mapping，2026-08-26） |
| workers | [`./workers.md`](./workers.md) | 7 | ✅ 完全上线（CRUD + verify-badge + deactivate/reactivate + id_card_no 40901，2026-08-26） |
| **生产管理** | [`./production/index.md`](./production/index.md) | **19** | ✅ 完全上线（工种/工序/工序映射/工艺链/工人候选池；按前端 `production_group` 菜单整合为子目录，2026-09-12） |
| part | [`./parts/index.md`](./parts/index.md) | **49** | ✅ 完全上线（Phase 1+2 全部状态机 / 批量 / 扫码 / pick-up 端点落地，2026-09-14） |
| assembly | [`./assemblies/index.md`](./assemblies/index.md) | **8** | ✅ 完全上线（Phase 3 加 /start + /files，2026-09-14） |
| cnc-programs | [`./cnc-programs.md`](./cnc-programs.md) | 2 | ✅ 完全上线（2026-09-14，Phase 3） |
| part-files | [`./files.md`](./files.md) | 4 | ✅ 完全上线（2026-09-14，Phase 3；2026-09-18 删除 upload-intents → 改由 upload_session 域承载 STS 共享机制） |
| upload-sessions | [`./upload_session.md`](./upload_session.md) | 7 | ✅ 完全上线（2026-09-18 新增：共享 STS 凭证 Redis 会话机制，替代原 upload-intents 一次性签发） |
| outsource-companies | [`./outsource-companies.md`](./outsource-companies.md) | 7 | ✅ 完全上线（2026-09-14，Phase 2） |
| outsource-quotes | [`./outsource-quotes.md`](./outsource-quotes.md) | 8 | ✅ 完全上线（2026-09-14，Phase 2） |
| outsource-shipments | [`./outsource-shipments.md`](./outsource-shipments.md) | 1 | ✅ 完全上线（2026-09-14，Phase 2） |
| delivery-notes | [`./delivery-notes/index.md`](./delivery-notes/index.md) | 18 | ✅ 完全上线（P1–P4，按功能拆为子目录） |
| delivery-groups | [`./delivery-groups.md`](./delivery-groups.md) | 4 | ✅ 完全上线（P1） |
| process-chains | [`./production/process-chain.md`](./production/process-chain.md) | 3 | ✅ 完全上线（2026-09-12，process_chain 域；2026-09-16 FK 翻转 + `GET /{chain_id}`） |
| _e2e | [`./_e2e.md`](./_e2e.md) | 11 | ✅ 完全上线（2026-09-14，e2e seed hook，dev/test profile） |
| websocket | [`./websocket.md`](./websocket.md) | 1 | 🟡 WS stub（handler 已搭骨架，待握手实现） |
| 其他 1 域 | — | 0 | ⚪ 仅占位（见下） |

---

## 未上线域（前端勿调用）

> 📋 Rust vs Python myERP 接口差距清单：[`./inconsistencies.md`](./inconsistencies.md)（1 域整域缺失 + 大部分 part gap 已补齐 + WS 路径差异）

| 域 | 路由前缀 | 状态 |
|---|---|---|
| `statistics` | `/api/v2/statistics` | 同上（5 个聚合读端点占位，纯查询域） |
| `part_batch` | （未挂载独立路由） | 内部复用，无独立前端 API |

> 调用任何"未上线域"会得到 `404 Not Found`（不是 panic / 500）。
> 后续域实施后，需**在本目录新增对应文件**（参照现有 `<module>.md` 模板）。

---

## 跨域错误码速查

> 完整定义在 `src/shared/error.rs::code`。新增错误码时务必同步该模块顶部注释。

### 通用

| code | 名称 | HTTP |
|---|---|---|
| 40000 | BAD_REQUEST | 400 |
| 40001 | VALIDATION_ERROR | 422 |
| 40100 | UNAUTHORIZED | 401 |
| 40300 | FORBIDDEN | 403 |
| 40301 | SHELF_MISMATCH | 403 |
| 40400 | NOT_FOUND | 404 |
| 40901 | VERSION_CONFLICT | 409 |
| 41301 | REQUEST_TOO_LARGE | 413 |
| 50000 | INTERNAL | 500 |
| 50001 | DATABASE | 500 |

### Auth 业务码（401xx）

| code | 名称 | HTTP |
|---|---|---|
| 40101 | BIZ_AUTH_INVALID（用户不存在/已删/已停用/密码错） | 401 |
| 40102 | TOKEN_EXPIRED | 401 |
| 40103 | REFRESH_INVALID（refresh 失效/版本不匹配/用户停用） | 401 |
| 40104 | OLD_PASSWORD_MISMATCH | 401 |
| 40105 | SESSION_REVOKED（Redis 中 session 不存在 / 已失效） | 401 |

### 业务域（2xxxx）

| 段 | 域 |
|---|---|
| 200xx | 用户/订单（USER_NOT_FOUND 20001 / USER_DUPLICATE 20002 / ORDER_NOT_FOUND 20003） |
| 201xx | 零件/客户/序列号（PART_NOT_FOUND 20101 / CUSTOMER_NOT_FOUND 20102 / INVALID_TRANSITION 20103 / INVALID_VALUE 20104 / SERIAL_EXHAUSTED 20105 / ... / PART_BATCH_NOT_FOUND 20109 / PRICE_LOCKED_BY_ASSEMBLY 20110 / PART_BATCH_INVALID_QUANTITY 20111 / PART_QUANTITY_LOCKED 20112 / CUSTOMER_IN_USE 20113 / **PART_BATCH_NOT_HELD_BY_WORKER 20114 / PART_ALREADY_CANCELLED 20115 / PART_NOT_DELIVERED 20116 / PART_NOT_READY_TO_SHIP 20117 / PART_REPAIR_NOT_TRIGGERED 20118 / PART_NOT_DELETABLE 20119**） |
| 202xx | 工人（WORKER_NOT_FOUND 20201 / WORKER_INACTIVE 20202 / WORKER_IN_USE 20203 / WORKER_HOLD_LIMIT_EXCEEDED 20204 / **WORKER_POOL_EMPTY 20205 / NO_WORK_TYPE 20206**） |
| 203xx | 装配体（ASSEMBLY_NOT_FOUND 20301 / BAD_CUSTOMER 20302 / TOO_MANY_CHILDREN 20303 / **PDF_INVALID 20305 / CHILD_PRICE_LOCKED 20306 / HAS_SHIPMENT 20307 / CUSTOMER_NO_SERIAL_PREFIX 20308**） |
| 204xx | 图纸文件（DRAWING_FILE_NOT_FOUND 20401 / BAD_TYPE 20402 / TOO_LARGE 20403 / UPLOAD_FAILED 20404） |
| 205xx | 货架（SHELF_NOT_FOUND 20501 / DUPLICATE_CODE 20502 / IN_USE 20503 / **PROCESS_SHELF_NOT_FOUND 20504 / PROCESS_PROCESS_NOT_FOUND 20505 / NO_MATCH_FOR_PROCESS 20506** / PROCESS_NOT_MAPPED 20507 / NOT_INSPECTION_ZONE 20511 / INACTIVE 20512） |
| 206xx | 账号（USER_ACCOUNT_NOT_FOUND 20601 / DUPLICATE_USERNAME 20602 / INACTIVE 20603 / ROLE_DUPLICATE 20604 / ROLE_NOT_FOUND 20605 / NO_ROLE 20606） |
| 207xx | 工艺链（**PROCESS_CHAIN_NOT_FOUND 20701 / PROCESS_CHAIN_STEP_NOT_FOUND 20702 / WORK_TYPE_MAX_HELD_MINUTES_NOT_SET 20703 / AUTO_ALLOCATE_INVALID_RATIO 20704 / PROCESS_CHAIN_PART_NOT_PENDING 20705 / PROCESS_CHAIN_REQUIRED 20706**） |
| 208xx | 工序（PROCESS_NOT_FOUND 20801 / DUPLICATE_CODE 20802 / IN_USE 20803） |
| 209xx | 工种（WORK_TYPE_NOT_FOUND 20901 / DUPLICATE_CODE 20902 / IN_USE 20903 / **MAX_HELD_NOT_SET 20904 / NO_PROCESS_MAPPING 20905**） |
| 210xx | 申请人（APPLICANT_NOT_FOUND 21001 / DUPLICATE_NAME 21002 / BAD_CUSTOMER 21003 / IN_USE 21004） |
| 211xx | 零件文件 / 模板（PART_FILE_NOT_FOUND 21101 / BAD_TYPE 21102 / TOO_LARGE 21103 / UPLOAD_FAILED 21104 / OWNER_NOT_FOUND 21105 / DUPLICATE 21108 / DELIVERY_TEMPLATE_NOT_CONFIGURED 21109 / DELIVERY_PART_STATUS_INVALID 21111 / TEMPLATE_TOO_MANY_PARTS 21112 / PRINT_BAD_ORDER 21113 / **PART_FILE_TMP_OBJECT_MISSING 21114 / PART_FILE_SIZE_MISMATCH 21115 / STS_ISSUE_FAILED 21116**）⚠️ 21110 已 deprecated 别名 = 21407 |
| 212xx | 外协公司（OUTSOURCE_COMPANY_NOT_FOUND 21201 / DUPLICATE 21202 / BAD_PROCESS 21203 / PROCESS_NOT_MAPPED 21204 / IN_USE 21205 / **PART_NOT_OUTSOURCEABLE 21206 / DIRECT_REQUIRES_C2_SHELF 21207 / NO_SHELF 21208**） |
| 213xx | 外协报价（OUTSOURCE_QUOTE_NOT_FOUND 21301 / INVALID_TRANSITION 21302 / DUPLICATE 21303 / NOT_APPROVED 21307） |
| 214xx | 送货单（NOT_FOUND 21401 / INVALID_TRANSITION 21402 / NOT_DRAFT 21403 / NOT_SUBMITTED 21404 / PART_NOT_READY 21405 / PART_ALREADY_ASSIGNED 21406 / PARTS_MULTIPLE_CUSTOMERS 21407 / SCAN_MISMATCH 21408 / DRIVER_INVALID 21409 / SCAN_INCOMPLETE 21410 / INVALID_VALUE 21411 / PARTS_LOCKED 21412 / GROUP_NOT_FOUND 21413 / GROUP_DUPLICATE_NAME 21414 / GROUP_MEMBER_CONFLICT 21415 / SCOPE_MISMATCH 21416 / SCAN_UNKNOWN_CODE 21417 / ASSEMBLY_PARTS_NOT_READY 21418 / DRAFT_SCOPE_CONFLICT 21419 / LOCKED_PART 21420 / **BATCH_STATE_INVALID 21421**） |
| 215xx | 外协发货（OUTSOURCE_SHIPMENT_NOT_FOUND 21501 / **INVALID_TRANSITION 21502 / NO_OPEN 21503 / QUANTITY_EXCEEDS 21504**） |

---

## 维护约定

1. **每次修改 handler / dto / service 都要同步本目录对应文件**。在 PR 描述里点出 `docs/api/<mod>.md` 有变更。
2. **新增业务错误码**时，先在 `src/shared/error.rs::code` 注册（含 HTTP 状态推导），再在本文件 [跨域错误码速查](#跨域错误码速查) 追加。
3. **新增域**时：在 `src/modules/mod.rs` 注册路由 → 在本目录下新建 `<module>.md`（参照现有模板；如预估端点 ≥15 或文件预估 ≥600 行，直接以 `<module>/index.md` 子目录形式建立，模板见 `docs/api/delivery-notes/`）→ 在本文件"未上线域"表格里删除该行 / 在"模块 API"表格里新增一行。
4. **BizWithFailures**（如 21418）走 `R.data.failures`；前端要按 `code` 判断是否解析 `data.failures`。
5. **通用约定不要在模块文件里重复**（认证、响应信封、错误码分段都只在 index.md）。模块文件只写自己的端点和共享 DTO。
6. **模块拆分约定**：当 `<module>.md` 超过 ~15 端点或 ~600 行时，应当拆为 `<module>/` 子目录，结构如下：
   - `<module>/index.md` — 入口（同步声明 + 范围说明 + 子文件导航 + 端点总览表 + 共享 DTO）
   - `<module>/queries.md` / `<module>/drafts.md` / `<module>/workflow.md` / `<module>/print.md` — 按访问语义分组（视域而定；查询、草稿变更、状态流转、打印是最常见的四类）

   子文件顶部须有 sibling-nav 链回入口与同级文件；共享 DTO 锚点统一指向 `./index.md#<dto>-字段`。实施示例：`docs/api/delivery-notes/`。
