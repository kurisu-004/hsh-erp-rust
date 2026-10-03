# wx 域 API（微信小程序 BFF + 企业微信登录）

> 本文件须与 `src/modules/wx/{mod,auth,dashboard,parts,batches,worker,repo,vo,dto,wecom_client}.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
> IAM 域端点见 [`./iam.md`](./iam.md)

> **2026-09-28 新增**：7 个 BFF 只读聚合端点（首页 / 工单 / 批次 / 工人）。
> **2026-09-29 新增**：`POST /wx/iam/wx-login`（企业微信小程序登录），
> 以及 iam 域的 3 个绑定管理端点（见 [`./iam.md`](./iam.md)）。此前 wx 域
> **完全无文档**，本文件一次性补齐全部 8 个端点。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/wx/iam/wx-login` | **公开** | 企业微信小程序登录（`wx.login()` code → JWT） |
| GET | `/api/v2/wx/dashboard/home` | 已登录 | 首页聚合（me + 4 个计数 + 2 个今日事件） |
| GET | `/api/v2/wx/parts/counts` | 已登录 | 工单 4 tab 计数 |
| GET | `/api/v2/wx/parts` | 已登录 | 工单卡片分页 |
| GET | `/api/v2/wx/parts/by-serial/{serial_no}` | 已登录 | 扫码定位单个工单 |
| GET | `/api/v2/wx/batches/counts` | 已登录 | 当月批次 2 tab 计数 |
| GET | `/api/v2/wx/batches` | 已登录 | 批次卡片分页 |
| GET | `/api/v2/wx/worker/stats` | 已登录 | 当月工人工作量 |

## 域特性

- **瘦 DTO**：本域响应剔除 `version` / 审计字段 / children 嵌套，控制在 4KB 内
  （单卡片列表 10 条实测 ~1.5KB JSON），保证小程序首屏秒开。
- **雪花 ID 序列化为 string**：与全栈契约一致（`"id": "9000000000000001"`）。
- **BFF 聚合**：`/dashboard/home` 一次拉完首页全部卡片，减少小程序 HTTP 请求数。
- **分页结构**统一为 `WxPage<T>`（`items` / `total` / `page` / `size` / `has_more`），
  与通用 `Page<T>`（`items` / `total`）不同：小程序端按页码翻页而非 offset 游标。

---

## `POST /api/v2/wx/iam/wx-login`

权限: **公开**（白名单端点，调用方尚无 token）

### 身份源与前提

- 身份源是**企业微信 userid**（不是微信 openid）——小程序只在企业微信客户端内打开。
- 换取接口是企业微信的 `GET /cgi-bin/miniprogram/jscode2session`，
  **不是**微信的 `api.weixin.qq.com/sns/jscode2session`。
- 应用类型必须是**自建应用**（返回明文 userid）。第三方应用返回加密 userid，
  需 `suite_access_token` + `auth/getuserinfo3rd` 二次解密，本方案不适用。
- **仅预绑定**：userid 必须在 `t_wx_identity` 中已绑定到某个 `t_user.id`，
  否则直接拒绝（`40107`），**不会自动开户**。
- `session_key` 拿到即丢（**不落库**、不进日志）；本端点不返回手机号 / 头像昵称。

### Request

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | 小程序 `wx.login()` 返回的临时凭证；**一次性、5 分钟过期**；trim 后非空且 ≤512 字节 |

### Response 200 `data`

结构与 `POST /api/v2/iam/login` **完全同构**（复用 `iam::vo::LoginResponse`），
小程序端可直接复用账号密码登录的登录态处理逻辑。

| 字段 | 类型 | 说明 |
|---|---|---|
| `token` | string | JWT access token |
| `refresh_token` | string | JWT refresh token |
| `user` | object | 与 [`GET /iam/me`](./iam.md#get-apiv2iamme) 同结构 |

### 错误码

| 码 | HTTP | 场景 |
|---|---|---|
| 40001 | 422 | `code` 为空 / 超过 512 字节 |
| 40101 | 401 | 绑定指向的用户已软删 / 不存在 |
| 40106 | 401 | 企微侧失败：`40029`（code 失效）/ access_token 失效重取后仍失败 |
| 40107 | 403 | userid 未预绑定；或企微返回 corpid 与后端 `WECOM_CORPID` 不符（防跨企业串号） |
| 40109 | 503 | 后端未配置 `WECOM_CORPID` / `WECOM_CORPSECRET`（服务未就绪） |
| 20606 | 403 | 绑定用户未分配任何角色 |

### 白名单说明（安全）

本端点在**两处**中间件白名单里，改动路径时必须同步：

- `src/auth/middleware.rs::is_public_path` —— 免 Bearer 校验
- `src/middleware/idempotency.rs::is_public_idempotency_path` —— **不缓存响应**

第二处尤其关键：登录响应含 JWT，若被 idempotency 中间件缓存，攻击者复用同一个
`Idempotency-Key` 即可劫持他人 session。

---

## `GET /api/v2/wx/dashboard/home`

权限: 已登录

Response 200 `data`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `me` | object | 当前用户视图（结构同 `/iam/me`；`full_name` 为空串以省一次 DB 往返） |
| `part_counts` | object | 工单计数，见下 |
| `batch_counts` | object | 当月批次计数，见下 |
| `today_picked` | i64 | 今日 `PICKED_UP` 事件数 |
| `today_delivered` | i64 | 今日 `DELIVERED` 事件数 |

`part_counts` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `all` | i64 | 全部非软删工单 |
| `pending_production` | i64 | `PENDING` |
| `in_production` | i64 | `IN_PROCESS` |
| `pending_inspection` | i64 | `INSPECTION` |
| `delivered` | i64 | `READY_TO_SHIP` + `DELIVERED`（"已完工可出货"二合一） |

`batch_counts` 字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `in_progress` | i64 | 本月进入 `IN_PROCESS` 的批次数 |
| `done` | i64 | 本月有 `DELIVERED` 事件的批次数 |

错误码：DB 失败 → 50001。

---

## `GET /api/v2/wx/parts/counts`

权限: 已登录

Response 200 `data`：结构同 `dashboard/home` 的 `part_counts`（`all` /
`pending_production` / `in_production` / `pending_inspection` / `delivered`）。

> `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 仅计入
> `all`，不单独 tab 化。
>
> **2026-10-01**：`REPAIRING` 从该列表移除（降级为 `t_part_batch.is_repairing`
> 标记列）。返修中的工单 `status` 为 `IN_PROCESS`，**自动计入
> `in_production`**。

---

## `GET /api/v2/wx/parts`

权限: 已登录

### Query

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `status` | string? | 全部 | 精确过滤；`all` / 省略 = 不过滤；其它非法值 → 40001（HTTP 422）。**2026-10-01**：`REPAIRING` 已不在取值域内（降级为 `t_part_batch.is_repairing` 标记列）—— 返修中的工单按 `IN_PROCESS` 过滤 |
| `customer_id` | string (i64)? | — | 按客户过滤 |
| `page` | i64? | 1 | 页码（从 1 起） |
| `size` | i64? | 10 | 每页条数，上限 50 |

### Response 200 `data`（`WxPage<WxPartSummary>`）

分页字段：`items` / `total` / `page` / `size` / `has_more`。

`items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工单 ID |
| `serial_no` | string? | 序列号 |
| `name` | string | 名称 |
| `drawing_no` | string | 图号 |
| `quantity` | i32 | 数量 |
| `status` | string | 工单状态枚举 |
| `is_urgent` | bool | 是否急件 |
| `planned_delivery_date` | string | 计划交期（`YYYY-MM-DD`） |
| `customer_name` | string? | 客户名 |
| `current_batch_id` | string (i64)? | 当前活跃批次 ID |
| `current_batch_no` | i32? | 当前活跃批次号 |
| `current_holder_label` | string? | 当前持有者标签（货架 code / 工人名 / 外协公司名） |
| `kind` | string | `workOrder`（`assembly_id IS NULL`）/ `batch`（有装配体归属） |

排序：`is_urgent DESC, planned_delivery_date ASC, id ASC`。

---

## `GET /api/v2/wx/parts/by-serial/{serial_no}`

权限: 已登录

Path 参数：

| 字段 | 类型 | 说明 |
|---|---|---|
| `serial_no` | string | 序列号 |

Response 200 `data`：**单个** `WxPartSummary`（字段同上一节）。
0 行 → 40400 `NOT_FOUND`（HTTP 404）。

---

## `GET /api/v2/wx/batches/counts`

权限: 已登录

### Query

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `period` | string? | 当前月 | 严格 `YYYY-MM`；非法 → 40001（HTTP 422） |

### Response 200 `data`

| 字段 | 类型 | 说明 |
|---|---|---|
| `in_progress` | i64 | 本月进入 `IN_PROCESS` 的批次数 |
| `done` | i64 | 本月有 `DELIVERED` 事件的批次数（`DELIVERED` + `COMPLETED`） |

---

## `GET /api/v2/wx/batches`

权限: 已登录

### Query

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `tab` | string | — | **必填**；`in_progress`（→ `IN_PROCESS`）或 `done`（→ `DELIVERED`/`COMPLETED`）；其它值 → 40001（HTTP 422） |
| `period` | string? | 当前月 | 严格 `YYYY-MM`；非法 → 40001（HTTP 422） |
| `page` | i64? | 1 | 页码 |
| `size` | i64? | 10 | 每页条数，上限 50 |

### Response 200 `data`（`WxPage<WxBatchSummary>`）

`items[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 批次 ID |
| `part_id` | string (i64) | 所属工单 ID |
| `serial_no` | string? | 工单序列号 |
| `name` | string | 工单名称 |
| `drawing_no` | string | 图号 |
| `batch_no` | i32 | 批次号 |
| `quantity` | i32 | 批次数量 |
| `status` | string | 批次状态 |
| `assigned_to` | string? | 持有工人名 |
| `work_hours` | float? | 加工件数累计（`PICKED_UP` + `RETURNED` 的 `SUM(quantity)`，非真实工时） |
| `finished_date` | string? | 完工日期（最近一条 `DELIVERED` 事件的 `created_at::date`） |
| `due_date` | string? | 计划交期 |
| `drawing_url` | string? | 图纸 URL（当前恒为 `null`） |

排序：`updated_at DESC, id DESC`。

---

## `GET /api/v2/wx/worker/stats`

权限: 已登录

### Query

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `period` | string? | 当前月 | 严格 `YYYY-MM`；非法 → 40001（HTTP 422） |

### Response 200 `data`

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_count` | i64 | 该工人在该月发生过事件的不同 batch 数 |
| `work_hours` | float | 该工人在该月 `PICKED_UP` + `RETURNED` 事件的 `SUM(quantity)`（以「加工件数」作为工作量估算，**不是**真实工时） |

统计对象固定为**当前登录用户**（`CurrentUser.id`）。

> ⚠️ 已知口径：`t_part_event.worker_id` 语义上是 `t_worker.id`，当前与
> `t_user.id` **共享同一雪花 ID 空间**（migration 071 起的统一雪花策略）。
> 若将来两个 ID 空间分裂，本端点需要补一层中间映射。
