# wx 域 API（微信小程序 BFF）

> 本文件是 `/api/v2/wx/*` 的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码侧对应 `src/modules/wx/`（模块 doc 在 `mod.rs`，子模块 doc 在各自 `mod.rs`）。
>
> **消费方只有一个**：`wx-app` 微信小程序。Web 前端（`frontend/`）**不经本域**，走
> 各业务域的 `/api/v2/part/*` `/api/v2/prod/*` 等端点。

## 0. 2026-10-11 变更摘要（按小程序页面切子模块）

wx BFF 重构 **B2 + B3 两步**已完成（重构收官）。原先 `/api/v2/wx/*` 是 **10 个平铺
文件、零 service 层**，且**跨域复用** `iam::vo::CurrentUserOut` /
`iam::vo::LoginResponse` / `part::statemachine::PartStatus`。重构后：

1. **按页面切子模块**：`login/`（登录页）+ `part_list/`（零件一览页，B2）+
   `production/`（生产页，B3）。B3 之后 `wx/mod.rs` 下只剩这 3 个子目录 +
   `wecom_client.rs`，4 个旧平铺文件（`batches.rs` / `worker.rs` / `repo.rs` /
   `vo.rs`）全部删除。
2. **URL 跟页面名走**，全部**硬切、无 alias**（见 §5）。
3. **VO 不复用任何他域结构**：`WxLoginOut` 自建；`PartCardOut` /
   `ProductionBatchCardOut` 逐字对齐前端卡片模型并改为 **camelCase**。
4. 端点模型改为「**每页 1 个首屏聚合端点 + 1 个上拉增量端点**」。端点总数
   **10 → 4**（`/login/wecom` + `/part-list` + `/part-list/page` +
   `/production` + `/production/page`）。
5. 顺带修掉三个既有 bug（详见 §3.3 / §9）：`status` 参数口径错位（前端传 tab 值
   被当 DB 状态白名单校验）、`delivered` 角标与列表口径不一致（198 vs 126）、
   ★ `GET /wx/worker/stats` 把 `t_user.id` 当 `t_worker.id` 查而恒返 0。
6. 补 `deliveredQty` **真实值**（前端原先硬编码 0）。
7. ⚠️ `VO 逐字对齐前端卡片模型` 这句话**有一处例外**：字段名全部对齐，但
   `batchNo` 的**类型**没对齐（后端 number / 前端 TS `string`，转换在小程序映射层），
   另 `serialNo` 可空而前端声明非可选。登记在 §8.11，**不是**本域的 bug。
8. ⚠️ review 第 1 轮（2026-10-11）又修掉两处：`counts` 标量查询缺工单软删闸门
   （§3.8 / §8.10）、`?period=` 年份段未校验数字导致 LIKE 通配符穿透（§3.8 前的
   `resolve_period`，见 `production/service.rs`）。

## 1. 端点表

| # | 方法 | 路径 | 权限 | Query | 响应 `data` |
|---|---|---|---|---|---|
| 1 | POST | `/api/v2/wx/login/wecom` | **公开**（白名单） | 无（body `{code}`） | `WxLoginOut` |
| 2 | GET | `/api/v2/wx/part-list` | 登录即可（**无角色闸门**） | `status?` `page?` `size?` | `PartListHomeOut` |
| 3 | GET | `/api/v2/wx/part-list/page` | 登录即可 | `status?` `page?` `size?` | `PartListPageOut` |
| 4 | GET | `/api/v2/wx/production` | 登录即可 | `tab`（**必填**）`period?` `page?` `size?` | `ProductionHomeOut` |
| 5 | GET | `/api/v2/wx/production/page` | 登录即可 | `tab`（**必填**）`period?` `page?` `size?` | `ProductionPageOut` |

- **端点总数 5 条**（重构前 10 条，见 §5 的硬切表）。端点 4/5 是 B3 合并三个旧端点
  （`/worker/stats` + `/batches/counts` + `/batches/`）的产物。
- 全部 HTTP 端点返回统一信封 `R { code, message, data }`（`data` 成功时非 null）。
- 端点 2/3 的 `counts`（仅端点 2 有）是**全局口径**：不带 `?status=` 过滤。
  小程序 4 个 tab 的角标是固定的，不随当前选中的 tab 变。
  端点 4 的 `counts` 同理：**不带 `?tab=` 过滤**。
- 端点 2/3 的 `?status=`、端点 4/5 的 `?tab=` / `?period=` 非法取值 →
  `AppError::validation`（**40001** / HTTP 422，走 `R<T>` 信封）；
  `?page=abc` / `?size=abc` 由 axum `Query` 提取器拒绝，返 **HTTP 400 纯文本**
  （**不走** `R<T>` 信封）。
- ⚠️ 端点 4/5 的 **`?tab=` 是必填字段**：缺字段是 serde 缺字段 ⇒ **HTTP 400 纯文本**
  （body 形如 `Failed to deserialize query string: missing field 'tab'`），与
  「传了但非法 → 422 + 40001」是**两种不同的失败**。钉死：
  `tests/wx/production.rs::tab_is_required_and_bad_page_size_clamps`。
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

### 1.3 ★ `ProductionQuery`（端点 4 / 5 共用）

| 字段 | 类型 | 必填 | 缺省 / 约束 |
|---|---|---|---|
| `tab` | string | **是** | 白名单 `in_progress` / `done`。**缺字段 → HTTP 400 纯文本**；传了但非法 → **40001 / 422** |
| `period` | string | 否 | 缺省 = 当前月（`chrono::Local::now() %Y-%m`）；格式恒 `YYYY-MM`（长度 7、第 5 字节 `-`、月份 `01..=12`），否则 **40001 / 422** |
| `page` | number | 否 | 缺省 1，`max(1)`（0 与负数都归 1） |
| `size` | number | 否 | 缺省 10，`clamp(1, 50)` |

`tab` 白名单在 `production::service::tab_to_db_statuses`（私有），**刻意不用**
`part::statemachine::PartStatus` 做校验 —— 那正是本次要消灭的跨域复用。

`resolve_period`（旧 `wx::resolve_period`，`pub(crate)`）于 2026-10-11 随 B3 搬进
`production::service` 并**降级为私有函数**（连同它的 3 个单测）。B3 之后 wx 域只剩
本域一个消费者，继续挂在聚合层只是徒增耦合面。

## 2. 逐字段（端点 2 / 3 / 4 / 5）

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

小程序两张页面（零件一览 / 生产）都**从未读取** `total`。故端点 2/3/4/5 的响应里
**没有** `total`、`page`、`size` 三个字段，只回 `list` + `hasMore`。

⚠️ 旧 `WxPage<T> = { items, total, page, size, has_more }` 外壳（`wx/vo.rs`，B3
随该文件一起删除）**不再被任何端点使用**。它的 3 个单测
（`has_more_true_when_more_pages_exist` / `has_more_false_on_last_page` /
`has_more_false_when_empty`）随之消失 —— 那三条断言的是 `has_more = total > page *
size` 这个公式，而新算法是「取 `size + 1` 条判超」（见 §3.4），公式不再存在。
新算法的钉死方式改成了 4 条：lib 单测 `counts_keys_stay_snake_case_tab_names` 不涉及
它，由集成测试 `pagination_has_no_overlap_or_gap`（7 行 / size 3 → 3 页，累计 7 行、
无重复、末页 `hasMore == false`）与端点 2/3 的同款用例共同承担。

### 2.6 `ProductionHomeOut`（端点 4）

**实测 wire 形态**（`worker` **未绑定**）：

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "worker": null,
    "stats": { "batchCount": 0, "workHours": 0.0 },
    "counts": { "in_progress": 1, "done": 1 },
    "list": [
      {
        "id": "896273840442003456",
        "serialNo": "F25226240",
        "name": "法兰盘 DN80",
        "code": "FL-25226240",
        "dueDate": "2026-10-20",
        "batchNo": 1,
        "batchQty": 7,
        "status": "in_progress",
        "assignedTo": "李润",
        "workHours": 12.0,
        "finishedDate": null
      }
    ],
    "hasMore": false
  }
}
```

**实测 wire 形态**（`worker` **已绑定**，与上面同一个数据集，只改了
`t_user.worker_id`）：

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "worker": { "name": "李润", "workType": "CNC 车工", "avatar": null },
    "stats": { "batchCount": 1, "workHours": 12.0 },
    "counts": { "in_progress": 1, "done": 1 },
    "list": [ /* 与上面逐字相同 */ ],
    "hasMore": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker` | object \| **null** | `WorkerOut`；`t_user.worker_id` 未绑定时是 `null`（见 §8.7） |
| `stats` | object | `WorkerStatsOut`，**按绑定工人**过滤（见 §9） |
| `counts` | object | `BatchCountsOut`，**全局口径**（不受 `?tab=` 影响） |
| `list` | array | 当前页卡片，**至多 `size` 条** |
| `hasMore` | boolean | 见 §3.4 |

`WorkerOut`（3 字段）：

| 字段 | 类型 | 来源 | 可空 |
|---|---|---|---|
| `name` | string | `t_worker.name` | 否 |
| `workType` | string | `t_work_type.name`（经 `t_worker.work_type_id`） | 否（未绑定时为**空串 `""`**，见 §8.7） |
| `avatar` | string \| null | **恒 `null`**（`t_worker` 无头像列，见 §8.7） | ✅ |

`WorkerStatsOut`（2 字段）：

| 字段 | 类型 | 口径 |
|---|---|---|
| `batchCount` | number | 该工人当月发生过事件的**不同 `batch_id`** 数 |
| `workHours` | number | 该工人当月 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`（**工作量估算**，DB 无 `work_hours` 列） |

`BatchCountsOut`（2 字段）：

| 字段 | 类型 | 口径 |
|---|---|---|
| `in_progress` | number | 当月 `t_part_batch.status='IN_PROCESS'` **且** `updated_at` 落当月 |
| `done` | number | 当月 `status IN ('DELIVERED','COMPLETED')` **且**存在当月 `DELIVERED` 事件 |

⚠️⚠️ `counts` 的两个键**保持 snake_case**（`in_progress` / `done`），**不**转
camelCase。它们是**前端的 tab 名**（前端 `mock/production.ts` 声明为
`Record<BatchStatus, number>`，`BatchStatus = 'in_progress' | 'done'`），不是卡片字段
—— 与同响应里 camelCase 的 `workHours` / `hasMore` / `batchCount` 是两类东西。
转成 `inProgress` 会直接打断小程序角标渲染。钉死：lib 单测
`counts_keys_stay_snake_case_tab_names`。

### 2.7 `ProductionPageOut`（端点 5）

**实测 wire 形态**：

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "list": [
      {
        "id": "896273840458780672",
        "serialNo": "F54586368",
        "name": "法兰盘 DN80",
        "code": "FL-54586368",
        "dueDate": "2026-10-20",
        "batchNo": 2,
        "batchQty": 4,
        "status": "done",
        "assignedTo": null,
        "workHours": null,
        "finishedDate": "2026-10-05"
      }
    ],
    "hasMore": false
  }
}
```

与端点 4 的**唯一**结构差异：**没有** `worker` / `stats` / `counts` —— 上拉翻页不该
每次重查 `t_user` / `t_part_event` 聚合、也不该每次重算 4 个 COUNT。

`list` 与端点 4 是**同一个查询路径**（`production::service::list_cards`），故同一
`?tab=&period=&page=&size=` 下逐字相同。

### 2.8 `ProductionBatchCardOut`（端点 4 / 5 共用）

逐字对齐前端 `ProductionBatchCardData extends BatchPartCard`
（`wx-app/miniprogram/mock/production.ts`）。**恰好 11 个键**。

| 字段 | 类型 | 来源 | 可空 | 口径 |
|---|---|---|---|---|
| `id` | string | `t_part_batch.id` | 否 | ⚠️ 是**批次** id 不是零件 id；雪花 → JSON string |
| `serialNo` | string \| null | `t_part.serial_no` | ✅ | 手工工单为 null |
| `name` | string | `t_part.name` | 否 | |
| `code` | string | `t_part.drawing_no` | 否 | 前端叫 `code`，DB 叫 `drawing_no`（图号） |
| `dueDate` | string | `t_part.planned_delivery_date` | 否 | 恒 `YYYY-MM-DD`（该列 NOT NULL） |
| `batchNo` | number | `t_part_batch.batch_no` | 否 | **JSON number**；前端自己 `padStart(2,'0')` 补零展示 |
| `batchQty` | number | `t_part_batch.quantity` | 否 | ⚠️⚠️ **本批次**件数，**不是**工单总件数 —— 见 §8.8 |
| `status` | string | 折叠 | 否 | **只有 2 类**：`in_progress` / `done`（见 §3.6） |
| `assignedTo` | string \| null | `t_worker.name` | ✅ | 批次挂在货架上（`location ≠ 'WORKER'`）时为 null |
| `workHours` | number \| null | `SUM(quantity)` | ✅ | ⚠️ **无值是 `null` 不是 `0`** —— 见 §8.6 |
| `finishedDate` | string \| null | `DELIVERED` 事件日 | ✅ | `YYYY-MM-DD`；从未送车过则 null |

❌ **没有 `drawingUrl`**（§8.2）、❌ **没有 `part_id`**（前端声明但从不读）。

⚠️ 旧 VO 的 `drawing_no` / `serial_no` / `batch_no` / `quantity` / `assigned_to` /
`work_hours` / `finished_date` / `due_date`（snake_case）与 `part_id` / `drawing_url`
**全部删除或改名**，见 §6.2。

## 3. 口径表

### 3.1 排序（端点 2 / 3）

`ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC`
（加急 + 交期近优先）。⚠️ `is_urgent` **只参与排序、不进 VO** —— 前端按 `dueDate`
自己在组件里算紧急度，从不读该字段。**不要**顺手把它从排序里删掉。

端点 4 / 5 的排序**另有一套**，见 §3.7。

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
因此**没有**为了算 `hasMore` 而额外打一条 `SELECT COUNT(*)` 的理由。端点 4/5 与
端点 2/3 同款。

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

### 3.6 ★ 2 类 `status` 折叠与 `?tab=` 语义（端点 4 / 5）

| 前端传 `?tab=` | SQL 状态集 | `counts` 归桶（period 闸门） |
|---|---|---|
| `in_progress` | `['IN_PROCESS']` | `updated_at::text LIKE 'YYYY-MM%'` |
| `done` | `['DELIVERED','COMPLETED']`（**两个**） | 存在当月 `DELIVERED` 事件 |
| **缺 `?tab=`** | **HTTP 400 纯文本**（serde 缺字段） | — |
| **其它一切值** | **40001 / HTTP 422** | — |

- ⚠️ 本域的 tab 值是 `in_progress` / `done`，**与端点 2/3 的
  `inProduction` / `pendingProduction` …不同**。跨域传 tab 名（如
  `?tab=inProduction`）在本域是**非法**的。
- 前端传的是**前端的 tab 值**，**不是** DB 状态值。传 DB 原值（`?tab=IN_PROCESS`）
  是**非法**的。
- ⚠️ **2026-10-11 review 第 1 轮订正**：此处原写「`list` 与 `counts` 的 period 闸门
  **逐字一致**（本域**没有**角标 / 列表口径分叉）」，其中**「没有口径分叉」是事实性
  错误**，已改。准确表述与本次修复见 §3.8。
- 折叠方向是过滤表的**逆映射**：`DELIVERED` / `COMPLETED` → `done`，其余 → `in_progress`。
  lib 单测 `display_status_is_inverse_of_the_filter_table` 用一组交叉断言钉死。
- 与 §3.2 相反，本域**没有**静默兜底偏差：能在列表里出现的 DB 状态必然落在某个 tab
  集合内（`part_list` 的 `PROGRAMMING` / `OUTSOURCE` / `CANCELLED` 不落任何 tab，
  所以那边需要兜底）。`_ => "in_progress"` 臂只为「将来新增 tab 时忘了改这里」兜底。

### 3.7 `production` 的排序

`ORDER BY b.updated_at DESC, b.id DESC`（最近变更优先，`id DESC` 兜底保证同秒变更的
行之间顺序稳定）。逐字沿用旧实现。

⚠️ 与端点 2/3 的 `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC`
（加急 + 交期近优先）是**两套不同的排序** —— 旧 `/wx/batches/` 本来就是这样，本次
只搬不改。

### 3.8 ★ `counts` 与 `list` 的口径对齐（2026-10-11 review 第 1 轮修复）

`counts` 是**两条独立的标量查询**（`ProductionRepo::batch_counts_by_period`），
`list` 是**一条带分页的列表查询**（共用 `repo.rs` 的 4 个 SQL 片段常量）。两者
**不能**共用同一份 WHERE（标量聚合套不上列表的 `FROM` + `ORDER BY` + 分页），
所以「口径一致」是**靠约定 + 单测钉住的，不是靠结构保证的**。

| 维度 | 旧实现（`BatchCountsAgg::by_period`，逐字继承） | 现在 |
|---|---|---|
| **period 闸门**（`updated_at::text LIKE 'YYYY-MM%'` / 存在当月 `DELIVERED` 事件） | 与 list 侧**逐字一致** | 不变，仍逐字一致 |
| 批次软删（`b.deleted_at IS NULL`） | 一致 | 不变 |
| ⚠️ **工单软删（`p.deleted_at IS NULL`）** | **counts 侧缺**（标量查询不 JOIN `t_part`）⇒ **父工单软删、批次未软删**时 `counts` 计入、`list` 不出现 | **已补**（两条 count 各加 `EXISTS(SELECT 1 FROM t_part p WHERE p.id = b.part_id AND p.deleted_at IS NULL)`） |

**闸门加在 counts 侧**，用 `EXISTS`（半连接）而不是 `JOIN t_part`：这两条是标量
`COUNT(*)`，加 `JOIN` 会改变聚合形状、且 `JOIN` 会在无匹配时把行直接滤掉（虽然这里
语义等价，但 `EXISTS` 更贴近 list 侧 `INNER JOIN` 的「存在性」语义而不动聚合）。

⚠️ **该偏差不是本次重构引入的回归** —— 它从旧 `BatchCountsAgg::by_period` **逐字
继承**。本 worktree 的 dev 库实测该差值 **0 行**（库里 0 条软删工单关联批次），
所以是**潜伏**偏差而非现网故障；本次对齐是**主动收口**，登记在 §8.10。

**剩下的结构性事实（不修）**：`counts` 与 `list` 仍是**两次独立查询**，首屏聚合
（`worker` / `stats` / `counts` / `list`）在 handler 层**不开事务**，跑在
read-committed 下**可以跨快照**（例如统计查询期间有人改了批次状态，`counts` 与
`list` 反映的是两个时刻）。这符合本仓「读端点不开事务」的既定约定（端点 2/3 的
`counts` + `list` 同款），本轮**刻意不引入事务** —— 为纯读端点开事务会占住连接、
并与 WS 广播的锁序纠缠。**别把这里的「口径一致」理解成「同一快照内一致」**：口径
（SQL 谓词）一致，快照（隔离级别）不保证。

### 3.9 `page` 无上限（已知限制，继承自旧实现）

`OFFSET (page - 1) * size` 可任意深，**没有** `page` 上限，深翻页会让 DB 扫掉并丢弃
前 N 行。`size` 本身是 `clamp(1, 50)`（端点 2/3/4/5 一致）。

本轮**不修**：这是从旧 `/wx/batches/` / `/wx/parts/` 逐字继承的行为，且新实现少了
`COUNT(*)`（`hasMore` 改「取 `size + 1` 条」，见 §3.4）**严格更优**；给它加上限会引入
「页码超限时返回空列表 vs 报错」的产品语义问题，属产品决议。实测小程序两个页面都是
首屏 + 上拉增量，`page` 到不了几十。要真限制，应连同「超限时怎么办」一起定。

## 4. 错误码

| code | HTTP | 端点 | 触发条件 |
|---|---|---|---|
| `0` | 200 | 全部 | 成功 |
| `40001` | 422 | 1 / 2 / 3 / 4 / 5 | `code` 为空或 > 512 字节；`?status=` 不在白名单（2/3）；`?tab=` 不在白名单（4/5）；`?period=` 格式非法（4/5） |
| — | 400（纯文本） | 2 / 3 / 4 / 5 | `?page=abc` / `?size=abc`（axum `Query` 提取器层 rejection，**不走 `R<T>`**）；⚠️ 4/5 **缺 `?tab=`** 也是 400 纯文本（serde 缺字段），body 形如 `Failed to deserialize query string: missing field 'tab'` |
| `40100` | 401 | 2 / 3 / 4 / 5 | 无 / 无效 access token |
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
| `POST /api/v2/wx/iam/wx-login` | `POST /api/v2/wx/login/wecom` | 公开端点；两处白名单同步改（B2） |
| `GET /api/v2/wx/parts/counts` | 并入 `GET /api/v2/wx/part-list` | 首屏聚合（B2） |
| `GET /api/v2/wx/parts/?status=&page=&size=` | `GET /api/v2/wx/part-list/page?…` | （B2） |
| `GET /api/v2/wx/parts/by-serial/{serial_no}` | **删除** | 前端 `fetchPartBySerial` 零消费者（B2） |
| `GET /api/v2/wx/dashboard/home` | **删除** | 前端 `fetchHomeDashboard` 零消费者，且小程序无 dashboard 页；随该域一起消失的还有跨域复用的 `iam::vo::CurrentUserOut`（B2） |
| `GET /api/v2/wx/worker/stats?period=YYYY-MM` | 并入 `GET /api/v2/wx/production` | ⚠️ **顺带修掉了恒返 0 的 bug**，见 §9（**B3**） |
| `GET /api/v2/wx/batches/counts?period=YYYY-MM` | 并入 `GET /api/v2/wx/production` | 两个 tab 的角标一次性返回（**B3**） |
| `GET /api/v2/wx/batches/?tab=&period=&page=&size=` | `GET /api/v2/wx/production/page?tab=&period=&page=&size=` | ⚠️ **尾斜杠去掉了**（**B3**） |

**实测（2026-10-11，wire 形态逐字）**：B3 的三条旧路径与 `/wx/production/` 带尾斜杠
形态**完全一致** —— **HTTP 404 + 空 body**（axum 默认 fallback，不走 `R<T>` 信封）：

| 请求 | 实测 |
|---|---|
| `GET /api/v2/wx/batches/counts` | 404，body `""` |
| `GET /api/v2/wx/batches/` | 404，body `""` |
| `GET /api/v2/wx/worker/stats` | 404，body `""` |
| `GET /api/v2/wx/production/` | 404，body `""` |
| `GET /api/v2/wx/production/page/` | 404，body `""` |

⚠️ **小程序侧必须同步改**：`wx-app/miniprogram/services/production.ts` 现在发的是
`/wx/worker/stats`、`/wx/batches/counts`、`/wx/batches/`（**带**尾斜杠，见该文件
2026-09-28 注释），三个都要换成 `/wx/production?tab=…&period=…` 与
`/wx/production/page?tab=…&period=…&page=…&size=…`（**无**尾斜杠），并把
snake_case 的响应字段映射改成直接吃 camelCase。

### 5.1 ⚠️ 尾斜杠（2026-10-11 实测钉死）

**实测结论**：本仓 axum 版本下，`nest("<prefix>")` + 内层 `route("/")` **只匹配
无尾斜杠**的路径。

| 请求 | 实测 |
|---|---|
| `GET /api/v2/wx/part-list` | **200**，走 `handler::home` |
| `GET /api/v2/wx/part-list/` | **404**，axum 默认 fallback，**空 body**（不走 `R<T>` 信封） |
| `GET /api/v2/wx/part-list/page` | **200**，走 `handler::page` |
| `GET /api/v2/wx/part-list/page/` | **404**（同上） |
| `GET /api/v2/wx/production` | **200**，走 `handler::home` |
| `GET /api/v2/wx/production/` | **404 空 body**（同上） |
| `GET /api/v2/wx/production/page` | **200**，走 `handler::page` |
| `GET /api/v2/wx/production/page/` | **404 空 body**（同上） |

小程序侧曾按**相反**的假设发请求并踩过 404（旧 `/wx/parts/?…`、旧 `/wx/batches/?…`
同因）。**新契约全部无尾斜杠。** 回归：`tests/wx/part_list.rs::trailing_slash_form_is_pinned`
与 `tests/wx/production.rs::trailing_slash_form_is_pinned`（各把四种形态全钉死，
防 axum 升级 / nest 改写后行为漂移）。

### 5.2 路由顺序硬约束

`part_list::handler::router()` 与 `production::handler::router()` 里
`.route("/page", …)` **必须先于** `.route("/", …)` 注册（matchit 静态段优先于兜底）。
顺序反了静态路径会被兜底路由抢走。`wx/mod.rs` 顶层三个 nest 的段名互不相同、
无 catch-all，顶层顺序无硬约束。

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

### 6.2 2026-10-11（B3）

| 被移除项 | 原因 |
|---|---|
| `GET /wx/worker/stats` 端点 | 并入首屏聚合端点 `GET /wx/production`（少 2 次 HTTP；且顺带修掉恒返 0 的 bug） |
| `GET /wx/batches/counts` 端点 | 并入 `GET /wx/production` |
| `GET /wx/batches/` 端点 | 迁到 `/wx/production/page` |
| `src/modules/wx/batches.rs` / `worker.rs` | 搬进 `production/{handler,service}.rs` 后删除 |
| `src/modules/wx/repo.rs` / `vo.rs` | 搬进 `production/{repo,model,vo}.rs` 后删除（B2 已把 part 相关搬进 `part_list/`，B3 把剩下的搬完） |
| `wx::resolve_period`（`pub(crate)`，带 3 个单测） | 降级为 `production::service::resolve_period`（私有，3 个单测同搬）；B3 之后 wx 域只剩本域一个消费者 |
| `repo::BatchList::count` | `hasMore` 改「取 `size + 1` 条」判定，前端不读 `total` ⇒ 少一次 COUNT |
| `repo::BatchList::list` / `repo::BatchCountsAgg::by_period` 的两份重复 WHERE | 合成 `repo.rs` 的 4 个 SQL 常量（`SELECT_COLS` / `FROM_SQL` / `WHERE_CLAUSE` / `ORDER_BY_CLAUSE`），`count` 直接删除 |
| VO 字段 `part_id` | 前端 `ProductionBatchCardData` 声明了但**从不读取**（小程序无「跳零件详情」跳转） |
| VO 字段 `drawing_url`（**恒 `null` 的占位**） | `t_part` 无图纸列，后端无数据源；与端点 2/3 的「不出该字段」**统一处置**，见 §8.2 |
| VO `WxPage<T>`（`items` / `total` / `page` / `size` / `has_more`）+ 3 个单测 | 前端从不读 `total` / `page` / `size`；改为只回 `list` + `hasMore`，与端点 2/3 同款 |
| VO `BatchStatus`（`type = String`） | 无消费方：`list[].status` 只可能是 `in_progress` / `done` 两个字面量，折叠逻辑在 service 层 |
| 卡片字段 `drawing_no`（snake_case） | 逐字对齐前端 → 改名 `code` |
| 卡片字段 `serial_no` / `batch_no` / `quantity` / `assigned_to` / `work_hours` / `finished_date` / `due_date`（snake_case） | 同上 → `serialNo` / `batchNo` / `batchQty` / `assignedTo` / `workHours` / `finishedDate` / `dueDate`（camelCase） |
| VO `BatchCounts` / `MonthlyStats` / `WxBatchSummary` | 被 `production::vo::{BatchCountsOut, WorkerStatsOut, ProductionBatchCardOut}` 取代 |

## 7. 表依赖

| 表 | 用途 | 端点 |
|---|---|---|
| `t_part` | 卡片主体（`serial_no` / `name` / `drawing_no` / `planned_delivery_date`） | 2 / 3 / 4 / 5 |
| `t_part_batch` | ① 端点 2/3：当前活跃批次的 `batch_no`（`LEFT JOIN LATERAL`）+ `deliveredQty` 子查询 ② 端点 4/5：卡片主体（`batch_no` / `quantity` / `status` / `location` / `current_holder_id`）+ `in_progress` 角标 | 2 / 3 / 4 / 5 |
| `t_customer` | `customer` 字段（`LEFT JOIN`） | 2 / 3 |
| `t_part_event` | `finished_date` / `work_hours` / `done` 角标（端点 4/5）；端点 2/3 的 `deliveredQty` 间接来源 | 4 / 5 |
| `t_worker` | ① 端点 4/5：`assignedTo`（`LEFT JOIN … AND location='WORKER'`）② 端点 4/5：`worker.name` + `worker.work_type_id` | 4 / 5 |
| `t_work_type` | `worker.workType`（`LEFT JOIN … AND deleted_at IS NULL`） | 4 |
| `t_user` | `worker_id`（解 `CurrentUser.id → t_worker.id` 的唯一一跳，见 §9） | 4 |

**wx 域对以下表零 SQL**（跨域只读 / 开口消费）：

| 表 | 归属域 | 开口 |
|---|---|---|
| `t_wx_identity` | iam | `AccountService::resolve_wx_login_user` |
| `t_user_role` / `t_menu` / `t_role_menu` | iam | `AccountService` / `SessionService` |
| `t_shelf` | iam::shelf | **无**。⚠️ 2026-10-11 review 第 1 轮订正：B2 之前这里写「`SessionService`（`shelf_ids`）」是**误导** —— 那是 `iam::vo::LoginResponse` / `CurrentUserOut` 的出参字段，B2 已随「不复用他域 VO」一起从 wx 响应里删掉（见 `login/vo.rs`）。`login_by_user_id` **内部仍会解析货架范围**（iam 的登录流水线第 ⑤ 步，`SessionService::resolve_roles_and_scope`），故 `t_shelf` 仍会被读，但结果对 wx 域**只算不用**（算完即丢弃），前端零消费方 |

⚠️ **`t_user` 是唯一的例外**（B3 起）：`production` 域**在本域 SQL 里只读**
`t_user.worker_id` 一列。这符合本仓既定 pattern（`statistics` / `admin` /
`dashboard` 都在本域 SQL 里只读聚合），**不是**跨域 import —— 护栏
`production::production_domain_depends_on_no_other_domain` 钉死「代码区零他域
import」。⚠️ `t_user` 其余列（用户名 / 角色 / session）的 SQL 真源仍属 iam 域，
本域**不碰**。

依赖方向单向：`wx → 他域`。**禁止**反向 import `modules::wx::*`。唯一跨域类型是
`state.wecom: Arc<dyn WeComApiClient>`（由 `AppState` 持有，不属于 wx 域私有类型）。

## 8. 已知偏差登记

### 8.1 ★ 4 类 `status` 折叠有静默兜底（`part_list`，端点 2/3）

| 项 | 内容 |
|---|---|
| 偏差 | DB 的 `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 不映射到任何 tab 值 |
| 现象 | 它们**只计入 `counts.all`**，却**不会出现在任何单个 tab 的列表里**（列表按 DB 状态集过滤，它们不在任何集合内）。在「全部」列表里 `list[].status` 填什么？**本轮决定：填 `"pendingProduction"`** |
| 决定理由 | 与旧前端 `services/parts.ts::mapStatus` 的 catch-all 分支（`return 'pendingProduction'`）**逐字对齐**，避免小程序渲染行为突变。若改成 `COMPLETED → delivered` 之类「更贴近语义」的映射，一批工单会在改版后从「待生产」跳到别的 tab，是**面向用户的行为变化**，不该由一次后端重构悄悄引入 |
| 处置 | **静默兜底，产品不决议**。前端可自行处理：4 类折叠是**展示口径**，不是完整的生产阶段机。要消除只能给前端加第 5 个 tab（产品决议，不在本轮范围） |
| 钉死 | lib 单测 `display_status_silently_falls_back_to_pending_production`；文档两处（本文 §8.1 + `part_list::vo` 模块 doc） |

⚠️ **`production` 域（端点 4/5）没有这个偏差**：2 个 tab 与过滤集一一对应，不存在
「只计入 counts 却不在任何 tab 里」的状态。见 §3.6。

### 8.2 `drawingUrl` 有意缺字段

| 项 | 内容 |
|---|---|
| 偏差 | 前端 `BasePartCard` 有 `drawingUrl`，端点 2/3/4/5 **都不产出该字段** |
| 原因 | `t_part` **无图纸列**，后端没有可信数据源。**刻意不加恒 `null` 的占位字段**（那只会让前端多一条永假的分支） |
| 处置 | 小程序侧在自己的映射层用 `/asset/drawing/{code}.png` 本地兜底。等 COS 文件服务接入后**单独 PR** 补 |
| ⚠️ B3 前的对照 | 旧 `WxBatchSummary.drawing_url` 是**恒 `null` 的占位字段**（旧 `repo.rs` 里 `drawing_url: None, // 本 PR 不拉图`）。B3 已把它**连同占位一起删掉**，与端点 2/3 的处置统一 |

### 8.3 旧路径 404 无 alias

见 §5。硬切即 404，**没有**兼容层、没有重定向。小程序侧必须同步切 URL，否则
表现为「接口突然全挂」。

### 8.4 `by-serial` 与 `dashboard/home` 已删除（零消费者）

见 §5 / §6.1。两条端点在前端均无调用方（`rg` 确认），删除不产生功能回归。

### 8.5 `counts` 是全局口径，与 `list` 的过滤条件独立

`?status=delivered`（端点 2/3）/ `?tab=done`（端点 4/5）时：`counts` 仍是全部
tab 的数字（固定不变），只有 `list` 被过滤。这是**设计如此**（小程序 tab 角标固定），
不是 bug。若把过滤套到 counts 上，切 tab 时角标会集体塌成 0。

### 8.6 ★ `workHours` 无值时是 `null`，不是 `0`（B3 的行为变更）

| 项 | 内容 |
|---|---|
| 旧行为 | `repo::BatchList::list` 的子查询写的是 `COALESCE(SUM(quantity), 0)::float8` ⇒ **本月零事件的批次拿到 `0`** |
| 新行为 | 去掉 `COALESCE`：`SUM(quantity)::float8` 在无匹配行时是 NULL ⇒ VO 里是 `null` |
| 为什么改 | 前端 `<part-card>` 用 `wx:if="{{item.workHours != null}}"` 守门决定是否渲染工时行；给 `0` 会把「本月没干过活」渲染成「干了 0 小时」 |
| 影响面 | 只影响**响应值**，不影响 `counts`（角标走另外两条 SQL，与此无关） |
| 钉死 | lib 单测 `null_fields_serialize_as_null_not_zero` / `row_projection_keeps_missing_work_hours_as_null`；集成测试 `missing_work_hours_and_finished_date_are_null` |

### 8.7 ★ 恒缺 / 可空字段登记（`production` 域）

| 字段 | 形态 | 为什么 | 前端怎么活下来 |
|---|---|---|---|
| `worker` | `object \| null` | `t_user.worker_id IS NULL`（非工人账号 / 未绑定 / 指向已软删工人） | `production-stats.ts` 的 observer 写 `worker?.avatar \|\| ''`；模板直接 `{{worker.workType}}` 渲染，`null` 会在模板里被 `?.` / 默认值挡住 |
| `worker.avatar` | **恒 `null`** | `t_worker` **无头像列** | 组件 `wx:if="{{avatarSrc}}"` 为假 ⇒ 渲染 `t-icon` 用户占位。等头像服务接入后单独 PR 补。**刻意保留字段而不是删掉**（前端已声明并读取它） |
| `worker.workType` | `string`，未绑定工种 / 工种已软删时是**空串 `""`** | `t_worker.work_type_id IS NULL` 或 `t_work_type` 行已软删 | 模板直接 `{{worker.workType}}`：空串渲染成空白，`null` 反而会渲染成字面量 `null`。且前端 `WorkerInfo.workType` 的 TS 类型是 `string`（非可选），空串不破坏类型 |
| `stats` | **不区分**「未绑定」与「已绑定但当月零工作量」 | 两者都是零值 | 判别标志是 `worker` 是否为 `null`。⚠️ 若将来要区分，得在 `t_user` 上再加一个「已绑定」标记位，本轮不做 |
| `card.workHours` / `card.finishedDate` | `number \| null` / `string \| null` | 无事件 | `wx:if="{{item.workHours != null}}"` 守门，见 §8.6 |
| `card.assignedTo` | `string \| null` | 批次挂在货架上（`location ≠ 'WORKER'`） | 卡片可空渲染 |

⚠️ **B1 没有回填 `t_user.worker_id`**：回填脚本
`scripts/sql/20261011_backfill_t_user_worker_id.sql` 需人工确认后手工执行（脚本本身
已用 `BEGIN;` / `COMMIT;` 包裹，dry-run 方式见其文件头「执行方式」；⚠️ 该脚本
**不会**被 `sqlx::migrate!()` 或任何启动钩子自动跑，仓库里没有这样的路径）。
**上线前必须先跑完这个回填脚本** —— 否则不仅多数账号拿到 `worker: null` + 零值
`stats`，更要紧的是 `t_user.worker_id` **至今没有任何 app 写端点**（建账号 / 改账号
的 DTO 都还没有 `worker_id` 入口），意味着**不跑脚本就没有任何其它途径能把工人绑上**。
在那之前**绝大多数账号**（admin / 系统管理员 / `hmi-*` 等非工人账号）都会命中「未绑定」
分支，表现为 `worker: null` + 零值 `stats` + **HTTP 200**。

### 8.8 ⚠️⚠️ 口径陷阱：`production` 的 `batchQty` ≠ `part_list` 的 `batchQty`

| 端点 | 字段 | 取值 | 语义 |
|---|---|---|---|
| 4 / 5（`production`） | `batchQty` | `t_part_batch.quantity` | **本批次**件数 |
| 2 / 3（`part_list`，`kind=batch`） | `batchQty` | `t_part.quantity` | **工单总**件数 |

**两个字段同名、不同义。** 这是**既有行为的延续**（旧 `/wx/batches/` 与旧
`/wx/parts/` 本来就是两套口径），本次**刻意不改**：统一它需要产品先回答「批次卡片上
该显示本批件数还是工单总件数」，那是产品决议不是后端重构的顺带事项。

⚠️ 后人**不要**「顺手统一」—— 会静默改变其中一端小程序卡片的显示值。登记在此以免
被当成写错了。

### 8.9 `production` 的排序与 `part_list` 不同

端点 4/5 是 `ORDER BY b.updated_at DESC, b.id DESC`（最近变更优先）；
端点 2/3 是 `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, p.id ASC`
（加急 + 交期近优先）。**两套排序各自沿用旧实现**，B3 只搬不改。见 §3.7。

### 8.10 ★ `counts` 曾缺工单软删闸门（2026-10-11 review 第 1 轮**已修**）

| 项 | 内容 |
|---|---|
| 偏差 | 两条 count 标量查询只有 `b.deleted_at IS NULL`，**没有** list 侧 `JOIN t_part p` 带的 `p.deleted_at IS NULL` |
| 现象 | 父工单已软删、批次未软删 ⇒ **`counts` 计入、`list` 不出现**（角标 > 列表实际行数） |
| 来源 | ⚠️ **不是本次重构的回归**，从旧 `BatchCountsAgg::by_period` 逐字继承。worktree 的 dev 库实测差值 **0 行**（0 条软删工单关联批次）⇒ 潜伏偏差，非现网故障 |
| 处置 | **已修**（review 第 1 轮）：两条 count 各补 `EXISTS(… p.deleted_at IS NULL)`，闸门加在 **counts 侧** |
| 为什么文档原写法也算错 | 旧文档写「`list` 与 `counts` 口径**逐字一致**、本域**没有**角标 / 列表口径分叉」—— 谓词并不逐字一致（缺一条 `p.deleted_at IS NULL`），把它写成「不存在」是事实性错误。已收窄为「**period 闸门逐字一致**」并同步 §3.6 / §3.8 |
| 钉死 | `docs/api/wx.md` §3.8（口径表 + 闸门加在哪一侧）、`production/repo.rs` 的 `batch_counts_by_period` 与 `WHERE_CLAUSE` 逐字段 doc |

⚠️ 修完之后**仍有**一个结构性事实没变：两者是两次独立查询、首屏聚合不开事务 ⇒
read-committed 下**可能跨快照**。见 §3.8 末段。

### 8.11 ⚠️ `batchNo` 与前端 TS 模型的 number-vs-string 类型差（2026-10-11 review 第 1 轮登记）

| 端点 | 字段 | 后端 JSON 类型 | 前端 `BatchPartCard.batchNo` 的 TS 类型 |
|---|---|---|---|
| 4 / 5（`production`） | `batchNo` | **number**（`i32`，恒非空） | **`string`** |
| 2 / 3（`part_list`，`kind=batch`） | `batchNo` | **number \| null**（`Option<i32>`） | **`string`** |

**转换发生在小程序侧，且是既有映射层，不是本仓**：
- `wx-app/miniprogram/services/parts.ts::toPartCard` → `String(it.current_batch_no ?? 1).padStart(2, '0')`
- `wx-app/miniprogram/services/production.ts::toBatchCard` → `String(it.batch_no).padStart(2, '0')`
- 组件 `part-card.wxml` 只 `{{item.batchNo}}` **直接渲染**，自身不做转换

⇒ 本域 VO 模块 doc 里「逐字对齐前端卡片模型……前端可直接把响应塞进
`ProductionBatchCardData`」这句话**只对字段名成立、对 `batchNo` 不成立**（旧文案
没写这个限定，容易被读成「整条响应直连卡片模型」）。同族未登记项：`serialNo`
后端是 `string | null`，前端 `BasePartCard.serialNo` 声明为 `string`（非可选）——
`null` 会渲染成空白，TS 侧要靠 `?? ''` 兜（`toPartCard` 里写了 `serialNo: it.serial_no`
直传，实际会渲染出 `null` 字面量，属前端既有行为，本轮**不联动改前端**，只登记）。

**处置**：**不改后端**。`t_part_batch.batch_no` 是 `int`；把 JSON 改成 string 会让
端点 4/5 与端点 2/3（同名字段、同源列）类型不一致，是拿一个类型差换另一个。真要消
掉，得改**前端**的 `BatchPartCard` 类型声明或映射层 —— 那是 wx-app 侧的 PR。
**只登记**，以免后人把这当后端 bug「顺手修」。

## 9. ★ `worker` / `stats` 的数据源（2026-10-11 B3 修掉的既有 bug）

### 9.1 旧实现为什么恒返 0

旧 `GET /api/v2/wx/worker/stats` 的 repo 方法签名是
`WorkerStats::by_user_period(executor, user_id, period)`，SQL 是
`WHERE worker_id = $1`，而 handler 传的是 `CurrentUser.id` —— **那是 `t_user.id`**。
但 `t_part_event.worker_id` 的语义是 **`t_worker.id`**，两表之间**没有任何映射**，
只是「碰巧共用同一个雪花 ID 空间」（migration 071 起的统一雪花）。

**实测证据**：dev 库 5750 条 `t_part_event` 的 13 个 `worker_id` **全部只命中
`t_worker`、零命中 `t_user`**。⇒ 该端点对任何真实用户**恒返 `batch_count: 0`**。

旧 `worker.rs` 的注释把这一点写成「如果发现 worker 表的 worker_id 是 user 表的
子集 / 独立空间，这里需要调整」——事实上它**已经是**独立空间。

### 9.2 修复后的链路

```text
CurrentUser.id (t_user.id)
  → t_user.worker_id        ← B1（migration 20261011120000_001）新增的列（无物理 FK）
  → t_worker.id            ← stats 查询的过滤锚点
  → t_work_type.name       ← worker.workType 的来源（LEFT JOIN + deleted_at IS NULL）
```

为什么新增列而不是在查询里现推：`t_user.username = t_worker.badge_code` 这种推断
在真实数据上已被证伪（`13350114794` 的工人工牌是 `13359114794`，号段笔误；另有工人
在 `t_worker` 里查无此人）。绑定关系是**业务事实**，必须由人确认后落库。

### 9.3 「未绑定」的判定与形态

`production::repo::find_worker_by_user` 返回 `None`（⇒ `worker: null` + 零值
`stats`，**HTTP 仍 200**）的情形：

| 情形 | SQL 判据 |
|---|---|
| `t_user.worker_id IS NULL` | `AND u.worker_id IS NOT NULL`（绝大多数账号） |
| `worker_id` 指向的工人行不存在 | `JOIN t_worker w ON w.id = u.worker_id`（仓库**无物理 FK**，允许悬挂） |
| 工人行已软删 | `AND w.deleted_at IS NULL` |

`worker` 是 `Option` 而非「永远有值」：绝大多数系统账号本来就没有对应工人，把它做成
401/403 会让非工人账号**连批次列表都看不了**，而列表与工人身份无关。

### 9.4 `stats` 的口径

| 字段 | SQL |
|---|---|
| `batchCount` | `COUNT(DISTINCT batch_id) FILTER (WHERE batch_id IS NOT NULL)`，限该工人 + 当月 `created_at` |
| `workHours` | `COALESCE(SUM(quantity) FILTER (WHERE event_type IN ('PICKED_UP','RETURNED')), 0)`，限该工人 + 当月 `created_at` |

⚠️ `workHours` 是**工作量估算**（DB schema 无 `work_hours` 列），用「加工件数」顶替
—— 与 `statistics` 域的同形口径一致。

### 9.5 回归测试

| 测试 | 钉住什么 |
|---|---|
| `tests/wx/production.rs::stats_are_filtered_by_the_logged_in_users_worker` | ★ **核心回归**：两个工人各有当月事件（甲 2 批 × 10 件、另加 1 条 `quantity IS NULL` 事件 + 1 条上月事件；乙 3 批 × 100 件），账号绑甲 ⇒ 只统计甲；再把绑定切到乙 ⇒ 只统计乙。旧实现会返 0，漏过滤会返全表量 |
| `tests/wx/production.rs::bound_worker_exposes_name_work_type_and_nonzero_stats` | 已绑定 ⇒ `name` / `workType` / `avatar: null` / `batchCount > 0`；未分配工种 / 工种软删 ⇒ `workType == ""`；绑到已软删工人 ⇒ 收敛成 `worker: null` |
| `tests/wx/production.rs::unbound_worker_yields_null_worker_and_zero_stats` | 未绑定 ⇒ `worker: null` + 零值 stats + **HTTP 200**，且列表数据不受影响 |
| `src/modules/wx/production/mod.rs::production_domain_depends_on_no_other_domain` | 本域**零他域 import**（`t_user` / `t_worker` / `t_work_type` 只能在本域 SQL 里只读聚合） |