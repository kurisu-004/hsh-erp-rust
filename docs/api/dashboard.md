# dashboard 域 API

> WebSocket 大屏的 HTTP 首取 + WS 订阅双入口。HTTP 提供「一次拉全量快照」，
> WS 提供「握手首推 + 持续事件推送 + 心跳 + 周期 re-auth」。

通用响应信封 / 认证 / 角色 / 主键 / 错误码见 [`./index.md`](./index.md)（待补）。

<!-- 同步检查清单（review 时逐项核对）：
  1. handler.rs 的 2 个 handler 路径与方法
  2. dto.rs::DeliveryBasis 枚举变体
  3. vo/snapshot.rs 的 4 个 snapshot 子结构 + 3 个 WS 信封
  4. service::DashboardService::build_snapshot_with_workers 形参
  5. shared/error.rs 的 UNAUTHORIZED / SESSION_REVOKED / INTERNAL 错误码
  6. infra/config.rs 的 WS_HEARTBEAT_INTERVAL_SECONDS 等 4 项 env
-->

## 端点总览

| 方法 | 路径 | 一句话 | 权限 |
|---|---|---|---|
| WS `GET` | `/ws/dashboard` | WS 大屏：握手首推 snapshot + 订阅事件 + 30s 心跳 | 任意已登录（`*`） |
| HTTP `GET` | `/api/v2/dashboard/snapshot` | HTTP 全量首取大屏快照 | 任意已登录（`*`） |

> WS 端点不带 `/api/v2` 前缀，由 `modules::ws_router()` 在 `/ws` 下挂；
> HTTP 端点挂在 `modules::v2_router` 的 `/dashboard` nest 下，鉴权走
> `v2_router` 末尾的 `authenticate_middleware`（Bearer JWT + Redis session）。

---

## HTTP 端点：`GET /api/v2/dashboard/snapshot`

### 权限

任意已登录用户（MANAGER / SHELF_ACCOUNT / CLERK / INSPECTOR / CNC_PROGRAMMER），
与 [`GET /ws/dashboard`](./websocket.md#get-wsdashboard--websocket-升级) 对齐。

### Request

- Header：`Authorization: Bearer <access_token>`（必填，走 `v2_router` 中间件）
- Query（可选）：
  - `upcoming_days`（2026-09-30 新增）— 未来 N 天交付分桶的天数，i64 字符串形式
    （沿仓内 part 域 DTO `deserialize_i64_opt` 解析规则）；缺省 / 非法 →
    service 层兜底为 14；取值范围 `1..=60`（service 层 `clamp` 防御恶意大数 /
    拼写错把日期塞成 10000）。
  - `basis`（2026-10-04 新增）— `upcoming_delivery[]` 分桶的**交期口径**：
    - `planned`（缺省）— 按 `t_part.planned_delivery_date`（计划交期）分桶
    - `system` — 按 `t_part.system_delivery_date`（系统交期）分桶
    - 缺省 → service 层 `unwrap_or_default()` = `planned`；其它取值
      （如 `?basis=xxx`）走 axum `Query` 反序列化自动 4xx，不自写错误码。
- 无 body

### 调用链

```
HTTP request
  → v2_router authenticate_middleware (Bearer JWT + Redis session)
  → CurrentUser extractor
  → handler::get_snapshot
    → Query<SnapshotQuery> 解析 upcoming_days / basis（缺省 None）
    → state.pool.begin()
    → state.dashboard_service.build_snapshot_with_workers(
         &mut *tx, None, q.upcoming_days, q.basis
       )
       // service 层 days.unwrap_or(14).clamp(1, 60)、basis.unwrap_or_default() 兜底
    → tx.commit()
  → Json(R<DashboardSnapshot>)
```

> 与 WS 端点共用 `DashboardService::build_snapshot_with_workers`：service
> 调用代码块逐字相同（handler 三形态 ①：snapshot 单次只读聚合，开 tx 仅作
> 边界，commit 即结束；不引入新 repo / 新 SQL）。WS 端点的握手 snapshot 推送
> 保留不变（向后兼容），前端新 dashboard 视图可走「HTTP 首取 + WS 事件
> invalidate」模式。

### 响应 200

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "on_production_shelves": [
      {
        "shelf_id": "<stringified i64>",
        "shelf_code": "S-001",
        "shelf_name": "一号架",
        "total_count": 1,
        "items": [
          {
            "id": "<stringified i64>",
            "batch_id": "<stringified i64 | null>",
            "batch_no": 1,
            "serial_no": null,
            "name": "p-dash",
            "drawing_no": "DWG-D",
            "quantity": 5,
            "is_urgent": false,
            "planned_delivery_date": "2026-09-15",
            "picked_up_at": "2026-09-15T10:00:00",
            "current_holder_id": "<stringified i64 | null>",
            "current_holder_kind": "SHELF",
            "shelf_code": "S-001",
            "customer_id": "<stringified i64 | null>",
            "customer_name": "l2",
            "customer_path": "l1 / l2",
            "next_process_id": "<stringified i64 | null>",
            "next_process_name": "下道工序",
            "worker_name": "张三"
          }
        ]
      }
    ],
    "on_inspection_shelves": [...],
    "in_process": [...],
    "upcoming_delivery": [
      {
        "date": "2026-09-15",
        "count": 6,
        "by_status": { "DELIVERED": 1, "INSPECTION": 2, "PENDING": 3 }
      }
    ],
    "ts": "2026-09-28T15:00:00+08:00"
  }
}
```

`upcoming_delivery[]` 固定 N 条（N = `?upcoming_days`；缺省 14，service 层
`clamp(1, 60)`）；每条都含 `by_status`（必填，空对象 `{}` = 当日 0 件）。

### 响应字段表（对齐 `vo/snapshot.rs::DashboardSnapshot`）

#### 顶层 5 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data.on_production_shelves` | array | 是 | 生产区货架分组（每架 `OnProductionShelfGroup`） |
| `data.on_inspection_shelves` | array | 是 | 待品检货架上的 part 项（`DashboardItem`） |
| `data.in_process` | array | 是 | 加工中的 part（`DashboardItem`，holder 类别为 `WORKER` / `WORKER_POOL`） |
| `data.upcoming_delivery` | array | 是 | 未来 N 天交付分桶（`UpcomingDeliveryBucket`），固定 N 条；N 来自 `?upcoming_days=`，缺省 14，service 层 `clamp(1, 60)` 兜底（2026-09-30 新增）。**分桶所依的交期口径由 `?basis=` 决定**（2026-10-04 新增，缺省 `planned`） |
| `data.ts` | string | 是 | 快照构建本地时间戳（`YYYY-MM-DDTHH:MM:SS.fff+08:00`） |

#### `OnProductionShelfGroup`（`on_production_shelves[]` 元素）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `shelf_id` | string | 是 | 货架 snowflake id（i64 → 字符串） |
| `shelf_code` | string | 是 | 货架代号 |
| `shelf_name` | string | 是 | 货架名 |
| `total_count` | integer | 是 | 该架 item 数（top-10 截流后，可能小于 SQL 实际数） |
| `items` | array | 是 | `DashboardItem` 列表 |

#### `DashboardItem`（`items[]` / `on_inspection_shelves[]` / `in_process[]` 元素）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | part snowflake id |
| `batch_id` | string \| null | 否 | 批次 snowflake id |
| `batch_no` | integer \| null | 否 | 批内序号 |
| `serial_no` | string \| null | 否 | 序列号 |
| `name` | string | 是 | part 名 |
| `drawing_no` | string | 是 | 图号 |
| `quantity` | integer | 是 | 数量 |
| `is_urgent` | boolean | 是 | 是否加急 |
| `planned_delivery_date` | string \| null | 否 | 计划交付日期 `YYYY-MM-DD` |
| `picked_up_at` | string \| null | 否 | 工人领取时间（`NaiveDateTime`，无时区） |
| `current_holder_id` | string \| null | 否 | 当前持有者 id（i64 → 字符串） |
| `current_holder_kind` | string \| null | 否 | 持有者类别（`SHELF` / `WORKER` / `WORKER_POOL` 等） |
| `shelf_code` | string \| null | 否 | 货架代号（重复字段，便于前端按 shelf_code 索引） |
| `customer_id` | string \| null | 否 | 客户 id（i64 → 字符串） |
| `customer_name` | string \| null | 否 | L2 客户名 |
| `customer_path` | string \| null | 否 | L1 / L2 客户路径 |
| `next_process_id` | string \| null | 否 | 下一道工序 id（deprecated 标记保留，2026-09-27 part 域字段对齐影响）。**2026-09-30 改直读 `t_part_batch.current_process_id`**（migration 004）——原先经 `LEFT JOIN t_process_chain_step` 取 `s.process_id`，新下发批次（step 为 NULL）会显示 `null` 工序；**字段名不变** |
| `next_process_name` | string \| null | 否 | 下一道工序名 |
| `worker_name` | string \| null | 否 | 当前持有工人姓名 |

#### `UpcomingDeliveryBucket`（`upcoming_delivery[]` 元素）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `date` | string | 是 | 日期 `YYYY-MM-DD`。**口径由 `?basis=` 决定**（2026-10-04 新增）：`planned` = 计划交期日、`system` = 系统交期日；桶本身（日期序列 + 补零规则）两口径一致 |
| `count` | integer | 是 | 当天预计交付件数（i64，JSON wire 保留 number；非 snowflake ID 故不走字符串化）。**口径 = `t_part` 行数**：**含**装配件子件（`assembly_id IS NOT NULL`，每个子件各计 1），**不含**装配件父行（`t_assembly` 在本端点全模块零引用，`t_part.assembly_id` 只是个指向 `t_assembly.id` 的逻辑外键）。下钻列表须用 `GET /api/v2/com/union-list?row_type=PART_FLAT`（同一 `t_part` 口径） |
| `by_status` | object\<string, integer\> | 是 | 当天按 `OrderStatus` 细分的件数（2026-09-30 新增；供 dashboard 分层堆叠柱状图用），口径与同桶 `count` 完全一致（含子件、不含装配件父行）。**COMPLETED / CANCELLED 已 WHERE 排除，by_status 不会含这两个 key**；空对象 `{}` 表示当日 0 件。key 字母序排列（BTreeMap 序列化保证），但前端按 key 直接查，不依赖顺序 |

> `by_status` 与 `count` 的关系：`count = by_status 所有 value 之和`（service 端求和）。

> ⚠️ **`count` 的统计单元与下钻口径对齐（2026-10-05 写明）**：本字段统计的是
> `t_part` 行，**含装配件子件**、**不含装配件父行**。前端由本字段触发的下钻列表
> 必须用 `GET /api/v2/com/union-list?row_type=PART_FLAT`（同一 `t_part` 口径），
> 不得用 `row_type=PART`（子件被 `assembly_id IS NULL` 守卫排除，会出现
> 「柱状图 9 条 / 抽屉 5 条」的不一致）或 `row_type=ALL`（会多出装配件父行）。
>
> **`count` == 下钻 `total` 的三个前置条件（三条都满足才可能相等）**：
> 1. **交期口径对齐**：`?basis=system` 时下钻用
>    `system_delivery_date_from` / `system_delivery_date_to`；`?basis=planned`
>    （**缺省值**）时桶日取自 `planned_delivery_date`，下钻必须改用
>    `planned_delivery_date_from` / `_to`。用错列则两条统计落在不同日期集合上。
> 2. **状态口径对齐**：dashboard 的 count SQL 恒带
>    `status NOT IN ('COMPLETED','CANCELLED')`。PART_FLAT 不传 `statuses` 时
>    这两个状态**会被计入**下钻 `total` ⇒ `total ≥ count`，且桶内确有该两态行时
>    严格 `total > count`。下钻须显式传 `statuses=` 排除它们。
> 3. **时间窗精确等于桶日**：下钻的 `_from` / `_to` 须都取该桶的 `date`
>    （闭区间同日），不得用「整个窗口」或「跨多个桶」的范围。
>
> 三条都满足时，该桶 `count` 才等于下钻 `total`。任一条不满足时，前端不得把
> 两者做等值校验或据此提示「数据不一致」。

> ⚠️ **两口径的缺失语义（2026-10-04 新增）**
> - `?basis=system` 下 `system_delivery_date IS NULL` 的工单**整件不计入**：WHERE 的两处
>   范围比较（`>= CURRENT_DATE` / `< CURRENT_DATE + N days`）对 NULL 恒为 false，NULL 行
>   天然不命中——与 union-list 端点「NULL 交期不被命中」的既有语义一致，未额外写
>   `IS NOT NULL`。
> - `planned_delivery_date` 是 `t_part` 的 **`NOT NULL` 列**（`system_delivery_date` 可空），
>   planned 口径无此缺失。
> - 两口径的**桶总数恒为 N**（缺失日期补 0），差异只体现在 `count` / `by_status` 上。
> - 两口径的**合计之间无可比大小关系**：同一工单的两列可能分别落在窗口内外不同侧
>   （如计划交期已逾期、仅系统交期顺延到窗口内），方向可以反转，前端不得依赖任一方向。

### 错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 40100 | UNAUTHORIZED | 401 | 缺失 `Authorization` Header / Bearer JWT 验签失败 |
| 40105 | SESSION_REVOKED | 401 | Redis session 不存在（登出 / 改密 / 管理员停用） |
| 50001 | DATABASE | 500 | DB / SQL 透传错误（`build_snapshot_with_workers` 返回 `sqlx::Error`，由 `AppError::Database(#[from] sqlx::Error)` 映射） |

> 错误码段说明：本端点不引入新业务错误码；DB / SQL 失败走 `AppError::from(sqlx::Error)`
> 映射成 `50001 DATABASE`（与现有 HTTP handler 风格一致）。`50000 INTERNAL`
> 在本端点不会出现（handler 内无 `AppError::Internal(_)` 路径）。
>
> query 反序列化失败（`?upcoming_days=abc` / `?basis=xxx`）由 axum `Query` 直接返
> **400**（纯文本 body，**不走 `R<T>` 信封**），与上表的业务错误码段无关。

### 数据来源（7 张只读表）

| 表 | 用途 |
|---|---|
| `t_part` | snapshot 主表（`on_inspection_shelves` / `in_process` / `upcoming_delivery` 统计单元） |
| `t_part_batch` | `t_part` 的批次行（产线 + 品检 + 工人持有） |
| `t_part_event` | `PICKED_UP` 最近时间戳（`picked_up_at` 字段） |
| `t_shelf` | 生产区（`zone='PRODUCTION'`）和品检区（`zone='INSPECTION'`）货架 |
| `t_customer` | 客户路径（`parent_id` + 二级 parent 查表） |
| `t_process` | `next_process_id` → `next_process_name` 查表 |
| `t_worker` | `current_holder_id`（kind=`WORKER`） → `worker_name` 查表 |

> 货架分组聚合（按 `current_holder_id` 分桶）抽离到
> `src/shared/analytics/shelf_grouping.rs::group_by_shelf`，本端点不展开。

### i64 字段约定

- **snowflake ID 类**（`shelf_id` / `id` / `batch_id` / `customer_id` /
  `current_holder_id` / `next_process_id`）在 JSON 中序列化为**字符串**，
  与现有 HTTP `R<T>` 风格一致（避免 JS `Number.MAX_SAFE_INTEGER` 精度截断）。
  其中 `next_process_id` 的**取值来源**为 `t_part_batch.current_process_id`
  （2026-09-30 起直读，migration 004），字段名与序列化形态均不变。
- **数值类**（`data.upcoming_delivery[].count`）保留 JSON integer（小整数，
  不会触发精度问题）。

### 与 `/ws/dashboard` 的关系

| 维度 | `GET /api/v2/dashboard/snapshot`（本端点） | `GET /ws/dashboard` |
|---|---|---|
| 协议 | HTTP（`R<T>` 信封） | WebSocket（`WsSnapshotMsg` envelope） |
| 路径前缀 | `/api/v2/dashboard` | `/ws/dashboard`（不带 `/api/v2`） |
| 鉴权方式 | `authenticate_middleware` | `verify_session_token`（WS upgrade 不能被 HTTP middleware 拦截） |
| 鉴权凭证 | `Authorization: Bearer <jwt>` | `?token=<jwt>` query 参数 |
| 用途 | 视图首屏加载（HTTP 全量首取） | 实时增量（snapshot envelope + 业务事件 + 心跳） |
| 共用 service | `DashboardService::build_snapshot_with_workers` | 同 |
| 交期口径（2026-10-04 新增） | 由 `?basis=` 指定（`planned` / `system`） | **恒为 `planned`**（WS 无 query 参数，service 收 `None` → `Planned`） |
| 调用方前端 | dashboard 视图组件 mount 时 1 次 | dashboard 视图组件 mount 后维持连接 |

---

## WebSocket 端点：`GET /ws/dashboard`

> 详见 [`./websocket.md`](./websocket.md)（待补，含 14 条退出路径全景 +
> 4 个 Close code `1011` / `1012` / `4001` / `4003` + 30s text 心跳 +
> 协议层 Ping/Pong 存活检测 + 周期性 re-auth +
> 3 种消息帧 `WsSnapshotMsg` / `WsHeartbeatMsg` / `WsEventMsg`）。
>
> 当前文件不重复展开。

---

## 反向事件来源（dashboard 域被哪些模块影响）

`WsEvent::DashboardEvent` 由以下源在各自事务 `commit` 后广播，前端收事件后
触发 HTTP `GET /api/v2/dashboard/snapshot?basis=…` 全量重取（不是增量 patch）：

| 域 / 子模块 | 触发动作 |
|---|---|
| `task::auto_complete` | 自动完工定时任务触发批次状态切换 |
| `prod::worker_pool` | 工人池候选池分配 / 释放 |
| `prod::batch::dispatch` | 批次下发（拆批 / 派工） |
| `prod::batch::transition` | 批次流转（IN_PROCESS / COMPLETED / DELIVERED 等） |
| `prod::batch::lifecycle` | 批次生命周期（撤回 / 召回 / 强制完工） |
| `assembly` | 装配件状态变化（拆 / 合 / 改状态） |
| `admin` | 兜底对账 `POST /api/v2/admin/recompute-rollup`（Manager） |
| `part::batch` | 工单批量改状态 / 批量取消 |
| `part::crud` | 工单 CRUD（创建 / 修改 / 软删） |

> `WsEvent::DashboardSnapshot` 当前**无**业务方主动广播——快照由客户端在
> WS 握手时拉取 + 在 HTTP 端点首屏拉取；服务端不主动重推。

---

## 版本与变更日志

- **2026-09-15**：`GET /ws/dashboard` takeover-fill（接手 v1 Python 端 WS）；WS 走
  `verify_session_token`（不走 HTTP middleware）；
- **2026-09-22**：dashboard 域对齐 iam 事务分层范式（Group E 重构），拆
  `repo/` + `service/{mod,snapshot}.rs`，handler 三形态 ①；
- **2026-09-28**：新增 HTTP 端点 `GET /api/v2/dashboard/snapshot`（前端走
  「HTTP 首取 + WS 事件 invalidate」模式）；
- **2026-09-30**：HTTP 端点新增 `?upcoming_days=` query 形参（默认 14，`clamp(1, 60)`）；
  `BatchLite.next_process_id` 改直读 `t_part_batch.current_process_id`（migration 004）；
  `upcoming_delivery[].by_status` 按状态细分（BTreeMap 保 key 顺序）；
- **2026-10-01**：WS 健壮性加固 4 项（B1 `Lagged` 不静默 / B2 补 Close 帧 /
  B3 接真连接表 / B4 协议层 Ping + pong 超时 + 周期 re-auth）；
- **2026-10-02**：WS review 2 轮修复（re-auth 失败原因分流 / `select!` 分支顺序
  / `close_with` 加超时护栏 / 写侧失败路径裸 break）；
- **2026-10-04**：HTTP 端点新增 `?basis=` query 形参（`planned` / `system`，
  缺省 `planned`）；WS 路径恒传 `None` → `Planned`。