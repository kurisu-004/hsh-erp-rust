# wx 域 API（微信小程序 BFF）

> 本文件是 `/api/v2/wx/*` 的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码侧对应 `src/modules/wx/`（模块 doc 在 `mod.rs`，子模块 doc 在各自 `mod.rs`）。
>
> **消费方只有一个**：`wx-app` 微信小程序。Web 前端（`frontend/`）**不经本域**，走
> 各业务域的 `/api/v2/part/*` `/api/v2/prod/*` 等端点。

## 0. 2026-10-11 变更摘要（按小程序页面切子模块）

wx BFF 重构 B2 步。原先 `/api/v2/wx/*` 是 **10 个平铺文件、零 service 层**，
且**跨域复用** `iam::vo::CurrentUserOut` / `iam::vo::LoginResponse` /
`part::statemachine::PartStatus`。重构后：

1. **按页面切子模块**：`login/`（登录页）+ `part_list/`（零件一览页）。
   `batches` / `worker` 本步**原样保留**（B3 步会换成 `/wx/production/*`）。
2. **URL 跟页面名走**，全部**硬切、无 alias**（见 §5）。
3. **VO 不复用任何他域结构**：`WxLoginOut` 自建；`PartCardOut` 逐字对齐前端卡片
   模型并改为 **camelCase**。
4. 端点模型改为「**每页 1 个首屏聚合端点 + 1 个上拉增量端点**」。
5. 顺带修掉两个既有 bug（详见 §3.2）：`status` 参数口径错位（前端传 tab 值被当 DB
   状态白名单校验）、`delivered` 角标与列表口径不一致（198 vs 126）。
6. 补 `deliveredQty` **真实值**（前端原先硬编码 0）。

## 1. 端点表

| # | 方法 | 路径 | 权限 | Query | 响应 `data` |
|---|---|---|---|---|---|
| 1 | POST | `/api/v2/wx/login/wecom` | **公开**（白名单） | 无（body `{code}`） | `WxLoginOut` |
| 2 | GET | `/api/v2/wx/part-list` | 登录即可（**无角色闸门**） | `status?` `page?` `size?` | `PartListHomeOut` |
| 3 | GET | `/api/v2/wx/part-list/page` | 登录即可 | `status?` `page?` `size?` | `PartListPageOut` |
| 4 | GET | `/api/v2/wx/batches/counts` | 登录即可 | `period?` | `BatchCounts` |
| 5 | GET | `/api/v2/wx/batches` | 登录即可 | `tab`（必填）`period?` `page?` `size?` | `WxPage<WxBatchSummary>` |
| 6 | GET | `/api/v2/wx/worker/stats` | 登录即可 | `period?` | `MonthlyStats` |

- 端点 4~6 是 **B3 步的迁移对象**（→ `/api/v2/wx/production/*`），本文件在 B3 后
  会重写对应章节；本版如实登记其现状契约。
- 全部 HTTP 端点返回统一信封 `R { code, message, data }`（`data` 成功时非 null）。
- 端点 2/3 的 `counts`（仅端点 2 有）是**全局口径**：不带 `?status=` 过滤。
  小程序 4 个 tab 的角标是固定的，不随当前选中的 tab 变。
- 端点 2/3 的 `?status=` 非法取值 → `AppError::validation`（**40001** / HTTP 422，
  走 `R<T>` 信封）；`?page=abc` / `?size=abc` 由 axum `Query` 提取器拒绝，返
  **HTTP 400 纯文本**（**不走** `R<T>` 信封）。
- ⚠️ **端点 1 是公开路径**，加白名单必须**同步改两处**：
  `src/auth/middleware.rs::is_public_path`（免 Bearer 校验）+
  `src/middleware/idempotency.rs::is_public_idempotency_path`（不缓存响应）。
  第二处漏改 ⇒ 登录响应里的 JWT 被 idempotency 缓存，复用同一个 `Idempotency-Key`
  即可劫持他人 session。回归：`tests/wecom_login.rs::wx_login_idempotency_key_does_not_cache_jwt_response`。

### 1.1 `WxLoginOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `token` | string | access token（JWT，RS256） |
| `refresh_token` | string | refresh token（JWT，RS256） |
| `user` | object | `WxLoginUserOut`（下表） |

`WxLoginUserOut` **恰好 4 个字段**，逐字断言见
`tests/wx/part_list.rs::wx_login_response_only_exposes_six_fields`：

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_user.id`（雪花 → JSON string） |
| `username` | string | `t_user.username` |
| `full_name` | string | `t_user.full_name`（NOT NULL，可能是空串） |
| `roles` | array\<string\> | 扁平角色名；恒非空（无角色账号在签 token 前就被 20606 拒） |

⚠️ **字段名是 snake_case**（与端点 2/3 的 camelCase 刻意不同）：前端
`wx-app/miniprogram/services/auth.ts:78-87` 的 `applyLoginResponse` 逐字读
`token` / `refresh_token` / `user.id` / `user.username` / `user.full_name` /
`user.roles`，改名即打断登录态。

### 1.2 `WxLoginRequest`

| 字段 | 类型 | 约束 |
|---|---|---|
| `code` | string | 小程序 `wx.login()` 的一次性 code。trim 后非空、≤ **512** 字节，否则 40001 |

校验在 service 层显式做（**不**放 `#[serde(deserialize_with)]`）：code 不落库、
无 schema 约束可依赖，提前拒绝比让企微返一个不可归因的 40029 更有排查价值。

### 1.3 `BatchCounts` / `WxBatchSummary` / `MonthlyStats`（B3 迁移对象）

| 类型 | 字段 | 类型 | 说明 |
|---|---|---|---|
| `BatchCounts` | `in_progress` / `done` | number | 当月；`done` 走 `t_part_event` 的 `DELIVERED` 事件口径 |
| `WxBatchSummary` | `id` / `part_id` | string | 雪花 → JSON string |
| | `serial_no` / `name` / `drawing_no` / `status` | string | 直出 `t_part` / `t_part_batch` |
| | `batch_no` / `quantity` | number | 当前批次的 `t_part_batch` 值 |
| | `assigned_to` | string \| null | `t_worker.name`（holder 且 `location='WORKER'`） |
| | `work_hours` | number \| null | 该批次 `PICKED_UP + RETURNED` 事件的 SUM(quantity)（**工作量估算**，DB 无工时列） |
| | `finished_date` / `due_date` | string \| null | `YYYY-MM-DD` |
| | `drawing_url` | string \| null | **恒 `null`**（本仓未接文件服务；与端点 2/3 的「不出该字段」是不同处置，见 §8.2） |
| `MonthlyStats` | `batch_count` / `work_hours` | number | 当月；`batch_count` = 有事件的不同 `batch_id` 数 |

分页外壳 `WxPage<T> = { items, total, page, size, has_more }`，`has_more = total >
page * size`。⚠️ **端点 2/3 不用这个外壳**（见 §2.5）。

## 2. 逐字段（端点 2 / 3）

### 2.1 `PartCardOut`（判别联合，`#[serde(tag = "kind")]`）

JSON 顶层多一个 `kind` 键，取值 `"workOrder"` 或 `"batch"`。

**共同字段（两个变体都有）**：

| 字段 | 类型 | 来源 | 可空 | 口径 |
|---|---|---|---|---|
| `kind` | string | 后端判定 | 否 | `assembly_id IS NULL → "workOrder"`；**非空 → `"batch"`** |
| `id` | string | `t_part.id` | 否 | 雪花 → JSON string |
| `serialNo` | string \| null | `t_part.serial_no` | ✅ | 手工工单为 null |
| `name` | string | `t_part.name` | 否 | |
| `code` | string | `t_part.drawing_no` | 否 | 前端叫 `code`，DB 叫 `drawing_no`（图号） |
| `dueDate` | string | `t_part.planned_delivery_date` | 否 | 恒 `YYYY-MM-DD`（该列 NOT NULL） |

**`kind = "workOrder"` 变体**（对应前端 `WorkOrderPartCard`）：

| 字段 | 类型 | 来源 | 可空 | 口径 |
|---|---|---|---|---|
| `customer` | string \| null | `t_customer.name`（LEFT JOIN） | ✅ | |
| `deliveredQty` | number | 子查询 | 否 | `SUM(t_part_batch.quantity)` where `status IN ('DELIVERED','COMPLETED')` 且 `deleted_at IS NULL`，无命中为 `0` |
| `totalQty` | number | `t_part.quantity` | 否 | 工单**总**件数 |
| `status` | string | 折叠 | 否 | 4 类 tab 值之一，见 §3.2 |

**`kind = "batch"` 变体**（对应前端 `BatchPartCard`）：

| 字段 | 类型 | 来源 | 可空 | 口径 |
|---|---|---|---|---|
| `batchNo` | number \| null | 当前活跃批次的 `t_part_batch.batch_no` | ✅ | 「活跃」= `deleted_at IS NULL` 且 `status NOT IN ('COMPLETED','CANCELLED')`；多个时取 `batch_no ASC` 第一条；无活跃批次为 null |
| `batchQty` | number | `t_part.quantity` | 否 | ⚠️ 是**工单总件数**，**不是**当前批次量（与前端映射层原实现一致） |

⚠️ `batch` 变体**不含** `status` / `customer` / `deliveredQty` / `totalQty`
（前端 `BatchPartCard` 就没这些字段）。⇒ **批次卡片上拿不到 tab 归属**。

❌ **两个变体都没有 `drawingUrl`**，见 §8.2。

### 2.2 `PartListHomeOut`（端点 2）

| 字段 | 类型 | 说明 |
|---|---|---|
| `counts` | object | `PartCountsOut`，**全局口径**（不受 `?status=` 影响） |
| `list` | array | 当前页卡片，**至多 `size` 条** |
| `hasMore` | boolean | 见 §3.4 |

### 2.3 `PartListPageOut`（端点 3）

| 字段 | 类型 | 说明 |
|---|---|---|
| `list` | array | 与端点 2 的 `list` **同一查询路径**：同 `?status=&page=&size=` 下逐字相同 |
| `hasMore` | boolean | 同上 |

与端点 2 的**唯一**结构差异：**没有 `counts`**（上拉翻页不该每次重算 4 个 COUNT）。

### 2.4 `PartCountsOut`

| 字段 | 类型 | 归桶（DB 状态） |
|---|---|---|
| `all` | number | **任何**未软删 `t_part.status`（含 `CANCELLED`） |
| `pendingProduction` | number | `PENDING` |
| `inProduction` | number | `IN_PROCESS` |
| `pendingInspection` | number | `INSPECTION` |
| `delivered` | number | `READY_TO_SHIP` + `DELIVERED`（**两个**状态合并） |

### 2.5 ❌ 没有 `total` / `page` / `size` 回显

小程序两张页面（零件一览 / 生产）都**从未读取** `total`。故端点 2/3 的响应里
**没有** `total`、`page`、`size` 三个字段，只回 `list` + `hasMore`。端点 5 的
`WxPage<T>` 仍带 `total`（那套外壳本步不动，B3 迁 `production` 时再议）。

## 3. 口径表

### 3.1 排序

`ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC`
（加急 + 交期近优先）。⚠️ `is_urgent` **只参与排序、不进 VO** —— 前端按 `dueDate`
自己在组件里算紧急度，从不读该字段。**不要**顺手把它从排序里删掉。

### 3.2 4 类 tab 折叠与 `?status=` 语义

| 前端传 `?status=` | SQL 状态集 | 归桶（counts） |
|---|---|---|
| 缺省 / `all` | 不过滤（`$1::text[] IS NULL`） | — |
| `pendingProduction` | `['PENDING']` | `PENDING` |
| `inProduction` | `['IN_PROCESS']` | `IN_PROCESS` |
| `pendingInspection` | `['INSPECTION']` | `INSPECTION` |
| `delivered` | `['READY_TO_SHIP','DELIVERED']` | `READY_TO_SHIP` + `DELIVERED` |
| **其它一切值** | **40001 / HTTP 422** | — |

- 前端传的 `status` 是**前端的 tab 值**，**不是** DB 状态值。传 DB 原值
  （`status=PENDING`）是**非法**的 —— 2026-10-11 前它反而是「唯一合法」的形态，
  这正是旧 bug 之一。
- 白名单在 `part_list::service::status_to_db_statuses`（私有），**刻意不用**
  `part::statemachine::PartStatus` 做校验 —— 那正是本次要消灭的跨域复用。
- `REPAIRING` **不在表里**（2026-10-01 起降级为 `t_part_batch.is_repairing` 标记列，
  DB 不再产生该 status；返修中的工单 status 就是 `IN_PROCESS`，自动计入
  `in_production`）。**别加回 `REPAIRING` 分支**。

### 3.3 `counts` 与 `list` 同口径（2026-10-11 修掉的 bug）

| | 旧实现 | 新实现 |
|---|---|---|
| `counts.delivered` | `READY_TO_SHIP + DELIVERED`（本地库实测 72 + 126 = **198**） | 同左 |
| `list` 的 `delivered` tab | 只收**单值** `DELIVERED`（最多 **126**） | `['READY_TO_SHIP','DELIVERED']`（**198**） |

⇒ 旧实现里「角标 198、列表翻到底只有 126」的自相矛盾已消除。新实现由 service 层
的**同一张映射表**同时驱动过滤谓词与 counts 归桶；lib 单测
`counts_buckets_match_the_filter_table` 用一组交叉断言钉死这条不变量，集成测试
`delivered_tab_list_total_equals_counts_delivered` 走 HTTP 再验一遍。

### 3.4 `hasMore` 算法（★ 不多打 count 查询）

取 `size + 1` 条，**超出** `size` 即 `true`。响应里没有 `total`，前端不读，
因此**没有**为了算 `hasMore` 而额外打一条 `SELECT COUNT(*)` 的理由。

`page` 缺省 1、`max(1)`；`size` 缺省 10、`clamp(1, 50)`。

### 3.5 `deliveredQty` 口径

```sql
COALESCE((
    SELECT SUM(b.quantity) FROM t_part_batch b
    WHERE b.part_id = p.id
      AND b.deleted_at IS NULL
      AND b.status IN ('DELIVERED', 'COMPLETED')
), 0)::int
```

2026-10-11 之前的响应里**没有**这个字段，前端映射层硬编码 `deliveredQty: 0`。
本次后端补真值。口径实测（dev 库 1901 条未软删工单全量）：

| 观测 | 结果 |
|---|---|
| `SUM(所有未软删批次 quantity) == t_part.quantity` | 零例外 |
| `DELIVERED` 桶 126 条中 `deliveredQty == totalQty` | 126 / 126 |
| `IN_PROCESS` 桶部分交付（`0 < deliveredQty < totalQty`） | 5 条 |
| `PENDING` 桶部分交付 | 1 条 |

⇒ 该字段真的有信息量，不是恒 0。⚠️ 该子查询**只 SELECT**（与 CI 护栏
`no_outside_file_writes_batch_status` 无关，那条只拦 `UPDATE t_part_batch SET status`）。

## 4. 错误码

| code | HTTP | 端点 | 触发条件 |
|---|---|---|---|
| `0` | 200 | 全部 | 成功 |
| `40001` | 422 | 1 / 2 / 3 | `code` 为空或 > 512 字节；`?status=` 不在白名单；`?period=` 格式非法（端点 4/5/6） |
| — | 400（纯文本） | 2 / 3 / 5 | `?page=abc` / `?size=abc`（axum `Query` 提取器层 rejection，**不走 `R<T>`**） |
| `40100` | 401 | 2~6 | 无 / 无效 access token |
| `40101` | 401 | 1 | 绑定指向的用户已软删 / 已停用 |
| `40106` | 401 | 1 | 企微 `40029`（code 失效）重取 token 后仍失败；或企微返回 userid 为空 |
| `40107` | 403 | 1 | corpid 与本地配置不符（防跨企业串号）；或 userid 未在 `t_wx_identity` 预绑定 |
| `40108` | 409 | —（iam 域 `/iam/users/{id}/wx-bind`） | 改绑到已被其它账号占用的 userid |
| `40109` | 503 | 1 | 后端未配置 `WECOM_CORPID` / `WECOM_CORPSECRET`（`NoopWeComClient`） |
| `20606` | 403 | 1 | 绑定用户未分配任何角色 |
| `50000` / `50001` | 500 | 全部 | DB / 内部错误 |

安全硬约束（端点 1）：`corpsecret` / `access_token` / `session_key` **绝不**出现在
日志、错误消息或响应结构里。`session_key` 在 `HttpWeComClient` 反序列化瞬间即被
丢弃，**不落库**、不返回。本域对 `t_wx_identity` **零 SQL**（绑定表的 SQL 真源属
iam 域，经 `AccountService::resolve_wx_login_user` 开口）。

## 5. URL 硬切记录（2026-10-11，**无 alias**，旧路径一律 404）

| 旧 | 新 | 备注 |
|---|---|---|
| `POST /api/v2/wx/iam/wx-login` | `POST /api/v2/wx/login/wecom` | 公开端点；两处白名单同步改 |
| `GET /api/v2/wx/parts/counts` | 并入 `GET /api/v2/wx/part-list` | 首屏聚合 |
| `GET /api/v2/wx/parts/?status=&page=&size=` | `GET /api/v2/wx/part-list/page?…` | |
| `GET /api/v2/wx/parts/by-serial/{serial_no}` | **删除** | 前端 `fetchPartBySerial` 零消费者 |
| `GET /api/v2/wx/dashboard/home` | **删除** | 前端 `fetchHomeDashboard` 零消费者，且小程序无 dashboard 页；随该域一起消失的还有跨域复用的 `iam::vo::CurrentUserOut` |

### 5.1 ⚠️ 尾斜杠（2026-10-11 实测钉死）

**实测结论**：本仓 axum 版本下，`nest("/part-list")` + 内层 `route("/")` **只匹配
无尾斜杠**的路径。

| 请求 | 实测 |
|---|---|
| `GET /api/v2/wx/part-list` | **200**，走 `handler::home` |
| `GET /api/v2/wx/part-list/` | **404**，axum 默认 fallback，**空 body**（不走 `R<T>` 信封） |
| `GET /api/v2/wx/part-list/page` | **200**，走 `handler::page` |
| `GET /api/v2/wx/part-list/page/` | **404**（同上） |

小程序侧曾按**相反**的假设发请求并踩过 404（旧 `/wx/parts/?…` 同因）。
**新契约全部无尾斜杠。** 回归：`tests/wx/part_list.rs::trailing_slash_form_is_pinned`
（把四种形态全钉死，防 axum 升级 / nest 改写后行为漂移）。

### 5.2 路由顺序硬约束

`part_list::handler::router()` 里 `.route("/page", …)` **必须先于** `.route("/", …)`
注册（matchit 静态段优先于兜底）。顺序反了静态路径会被兜底路由抢走。

## 6. 移除记录

### 6.1 2026-10-11（B2）

| 被移除项 | 原因 |
|---|---|
| `GET /wx/parts/counts` 端点 | 与列表合并进首屏聚合端点 `GET /wx/part-list`（少一次 HTTP） |
| `GET /wx/parts` 端点 | 迁到 `/wx/part-list/page` |
| `GET /wx/parts/by-serial/{serial_no}` 端点 | 前端 `fetchPartBySerial` 零消费者 |
| `/wx/dashboard/*` 整域（`dashboard.rs`） | 前端 `fetchHomeDashboard` 零消费者，且小程序无 dashboard 页 |
| `HomeDashboard` VO | 随 dashboard 域删除 |
| `wx::vo::CountsByStatus` | 被 `part_list::vo::PartCountsOut`（camelCase）取代 |
| `wx::vo::WxPartSummary` / `WxPartKind` | 被 `part_list::vo::PartCardOut` 判别联合取代 |
| `wx::dto::WxLoginRequest`（旧平铺文件） | 搬进 `login/dto.rs` |
| 卡片字段 `is_urgent` | 前端自己按 `dueDate` 算紧急度，从不读该字段（**排序仍用该列**） |
| 卡片字段 `current_holder_label` | 前端从未读取；它同时是 `t_shelf` / `t_worker` / `t_outsource_company` 三条 LEFT JOIN 的唯一理由 ⇒ 三条 JOIN 一并删 |
| 卡片字段 `current_batch_id` | 小程序无「跳批次详情」跳转；`batchNo` 够用 |
| 卡片字段 `drawing_no`（snake_case） | 逐字对齐前端 → 改名 `code` |
| 卡片字段 `quantity` / `planned_delivery_date` / `customer_name` / `serial_no` | 同上 → 改名 `totalQty` / `dueDate` / `customer` / `serialNo`（camelCase） |
| `?customer_id=` query 参数 | 零消费者的预留参数，SQL 里恒真；留着会变成「本端点支持按客户筛选」的假承诺 |
| `WxPage` 的 `total` / `page` / `size` 回显（端点 2/3） | 前端从不读取；改为只回 `list` + `hasMore`，`hasMore` 用「取 `size + 1` 条」判定 |
| 响应字段 `expires_in` / `is_active` / `shelf_ids` / `menus`（端点 1） | 小程序零消费；`menus` 是整棵菜单树、`shelf_ids` 是 Web 端货架权限模型，对小程序是多余负载 |
| SQL 里 3 条 LEFT JOIN（`t_shelf` / `t_worker` / `t_outsource_company`）+ `COALESCE(sh.code, w.name, oc.name)` | 只为 `current_holder_label` 而存在 |
| `repo::DailyEventCounts` | 唯一消费者是已删除的 `/wx/dashboard/home` |

### 6.2 待 B3 处理

| 项 | 说明 |
|---|---|
| `/wx/batches/*` + `/wx/worker/stats` → `/wx/production/*` | B3 步的 URL 硬切；本步刻意保留，线上不断 |
| `wx/repo.rs` + `wx/vo.rs` 剩余内容 → `production/repo.rs` + `production/vo.rs` | 这两个文件本步被裁到只剩 batch / worker（part 相关已搬进 `part_list/`），B3 整体搬走后删除 |

## 7. 表依赖

| 表 | 用途 | 端点 |
|---|---|---|
| `t_part` | 卡片主体（12 列投影） | 2 / 3 |
| `t_part_batch` | ① 当前活跃批次的 `batch_no`（`LEFT JOIN LATERAL`）② `deliveredQty` 子查询 | 2 / 3 |
| `t_customer` | `customer` 字段（`LEFT JOIN`） | 2 / 3 |
| `t_part_event` | 批次 `finished_date` / `work_hours` / 工人工作量 | 4 / 5 / 6 |
| `t_worker` | `assigned_to`（`LEFT JOIN`） | 5 |

**wx 域对以下表零 SQL**（跨域只读 / 开口消费）：

| 表 | 归属域 | 开口 |
|---|---|---|
| `t_wx_identity` | iam | `AccountService::resolve_wx_login_user` |
| `t_user` / `t_user_role` / `t_menu` / `t_role_menu` | iam | `AccountService` / `SessionService` |
| `t_shelf` | iam / shelf | `SessionService`（`shelf_ids`，仅 Web 端权限模型用） |

依赖方向单向：`wx → 他域`。**禁止**反向 import `modules::wx::*`。唯一跨域类型是
`state.wecom: Arc<dyn WeComApiClient>`（由 `AppState` 持有，不属于 wx 域私有类型）。

## 8. 已知偏差登记

### 8.1 ★ 4 类 `status` 折叠有静默兜底（`counts` 与 `list` 的归属不完全对齐）

| 项 | 内容 |
|---|---|
| 偏差 | DB 的 `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 不映射到任何 tab 值 |
| 现象 | 它们**只计入 `counts.all`**，却**不会出现在任何单个 tab 的列表里**（列表按 DB 状态集过滤，它们不在任何集合内）。在「全部」列表里 `list[].status` 填什么？**本轮决定：填 `"pendingProduction"`** |
| 决定理由 | 与旧前端 `services/parts.ts::mapStatus` 的 catch-all 分支（`return 'pendingProduction'`）**逐字对齐**，避免小程序渲染行为突变。若改成 `COMPLETED → delivered` 之类「更贴近语义」的映射，一批工单会在改版后从「待生产」跳到别的 tab，是**面向用户的行为变化**，不该由一次后端重构悄悄引入 |
| 处置 | **静默兜底，产品不决议**。前端可自行处理：4 类折叠是**展示口径**，不是完整的生产阶段机。要消除只能给前端加第 5 个 tab（产品决议，不在本轮范围） |
| 钉死 | lib 单测 `display_status_silently_falls_back_to_pending_production`；文档两处（本文 §8.1 + `part_list::vo` 模块 doc） |

### 8.2 `drawingUrl` 有意缺字段

| 项 | 内容 |
|---|---|
| 偏差 | 前端 `BasePartCard` 有 `drawingUrl`，端点 2/3 **不产出该字段** |
| 原因 | `t_part` **无图纸列**，后端没有可信数据源。**刻意不加恒 `null` 的占位字段**（那只会让前端多一条永假的分支） |
| 处置 | 小程序侧在自己的映射层用 `/asset/drawing/{code}.png` 本地兜底。等 COS 文件服务接入后**单独 PR** 补 |
| ⚠️ 对照 | 端点 5 的 `WxBatchSummary.drawing_url` 是**恒 `null` 的占位字段**（历史形态，本步未动）。两处处置不同：B3 迁 `production` 时请统一 |

### 8.3 旧路径 404 无 alias

见 §5。硬切即 404，**没有**兼容层、没有重定向。小程序侧必须同步切 URL，否则
表现为「接口突然全挂」。

### 8.4 `by-serial` 与 `dashboard/home` 已删除（零消费者）

见 §5 / §6.1。两条端点在前端均无调用方（`rg` 确认），删除不产生功能回归。

### 8.5 `counts` 是全局口径，与 `list` 的过滤条件独立

`?status=delivered` 时：`counts` 仍是全部 5 个 tab 的数字（固定不变），只有 `list`
被过滤。这是**设计如此**（小程序 4 个 tab 的角标固定），不是 bug。若把过滤套到
counts 上，切 tab 时角标会集体塌成 0。