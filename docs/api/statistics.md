# statistics 域 API（生产统计）

> 域：`statistics`（源码在 `src/modules/statistics/`）。本文件是该域的**唯一**契约来源，
> 任何字段 / 端点变更必须同步本文件。
>
> **只读域**：3 个端点全部 `pool.acquire()` 不开事务、不发 WS 广播。

## 0. 变更摘要

| 日期 | 变更 |
|---|---|
| 2026-09-15 | takeover-fill，完整化 5 端点，权限 MANAGER-only |
| 2026-10-12 | **跳序取件域清空**：数据源表 `t_pickup_skip_event` 删除，`GET /pickup-skips` / `GET /pickup-skips/{worker_id}` 两条端点下线（**无 alias**，旧路径 404）⇒ 端点 **5 → 3** |
| 2026-10-12 | **新增有效工时维度**：`workers` / `workers/{worker_id}` 两个响应体加字段（见 §4.1）。**端点集合与入参逐字未变**，是纯响应体扩展 |
| 2026-10-12 | **`process_breakdown[].process_name` 口径收紧**：`LEFT JOIN t_process` 去掉 `deleted_at` 闸门 ⇒ 软删过的工序在历史统计里**仍显原名**，不再退 `(已删除)`（见 §4.9）。字段集合与类型不变 |

⚠️ 本域**不发 WS 事件、不消费 WS 事件**。前端的新鲜度策略与其它域一致（页面自身
mutation 后显式 invalidate + 可选轮询），见 `docs/api/queue.md` §8.3 第 8 条对全仓
策略的登记。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/statistics/overview` | **Manager 独占** | `date_from` / `date_to`（必填） | `OverviewOut` |
| 2 | GET | `/api/v2/statistics/workers` | **Manager 独占** | `date_from` / `date_to`（必填） | `WorkerStatsListOut` |
| 3 | GET | `/api/v2/statistics/workers/{worker_id}` | **Manager 独占** | path `worker_id` + `date_from` / `date_to`（必填） | `WorkerDetailOut` |

- **端点总数 3 条**。路由注册链：`src/modules/mod.rs` 的 `/api/v2` nest →
  `src/modules/statistics/mod.rs::router()` 转发 → 本域 `handler.rs::router()`。
- 统一信封 `R { code, message, data }`。
- ⚠️ 角色守卫**下沉在 handler 第一行**（`current.require_role(Role::Manager)`），
  三个 handler 逐字一致。非 Manager 账号一律 `40300 FORBIDDEN`。
- ⚠️ `date_from` / `date_to` 是**必填**的 `NaiveDate`（`%Y-%m-%d`）：缺字段或非日期
  字面量走 axum `Query` 提取器 → **HTTP 400 纯文本，不进 `R<T>` 信封**。
- ⚠️ 端点 3 的 `worker_id` 是 **path 上的 `String`**，由 service 自己 `parse::<i64>()`：
  非数字 → `20104 BIZ_INVALID_VALUE`（**HTTP 400，走信封**，与端点 1/2 的裸提取器
  行为不同）。**不要**把它当 `Path<i64>` 端点。

### 1.1 `DateRangeQuery` 逐参数

| 参数 | 类型 | 语义 | 缺省 | 非法值处理 |
|---|---|---|---|---|
| `date_from` | date | 统计区间**含**当日的起始日 | 无，**必填** | 缺字段 / 非 `%Y-%m-%d` → axum `Query` 拒绝 → HTTP 400 纯文本 |
| `date_to` | date | 统计区间**含**当日的结束日 | 无，**必填** | 同上 |

⚠️ **`date_from > date_to` 是 `20104 BIZ_INVALID_VALUE`（HTTP 400，走信封）**，由
service 的 `validate_date_range` 判，不是提取器判 ⇒ 端点 1/2/3 三个都返这一个码。

## 2. `OverviewOut` 逐字段

| 字段 | 类型 | SQL 来源 | 口径 |
|---|---|---|---|
| `date_from` / `date_to` | string | 请求回显 | `NaiveDate` 原生序列化 |
| `created_count` | number | `t_part` | 期内新建工单数（`deleted_at IS NULL`，`created_at ∈ [date_from, date_to+1)`） |
| `completed_count` | number | `t_part_event` | `event_type='COMPLETED' AND batch_id IS NULL`，distinct `part_id` |
| `in_process_count` | number | `t_part` | 期末在制：`date_to` 24:00 前已创建，且**任何时刻**不存在 `COMPLETED` / `CANCELLED` 工单级事件。⚠️ 判定**不带** `e.created_at < date_to+1` 过滤（那是 2026-09-15 修掉的期末口径偏差） |
| `delivered_count` | number | `t_part_event` | 由 DELIVERED 事件派生实际交付日（**不是** `t_part.actual_delivery_date`，该列已删） |
| `delivered_value` | string | `t_part.total_price` | 期内交付总产值，`Decimal` → JSON string |
| `late_orange_count` | number | 同上 | orange = 实际 > 计划 且（系统交期为 NULL 或 实际 ≤ 系统交期） |
| `late_red_count` | number | 同上 | red = 系统交期非空 且 实际 > 系统交期 |
| `overdue_undelivered_count` | number | `t_part` + NOT EXISTS DELIVERED | **当前**（不是 date_to）超期未交付数 |
| `repair_part_count` | number | `t_part_event` | `REPAIR_STARTED`，distinct `part_id` |
| `daily_created[]` / `daily_completed[]` | array | 同上 | `{ date, count }`，**零填充**到整个区间（缺日期补 0） |
| `delivery_performance` | object | 同上 | `{ on_time, orange, red }`，`on_time = delivered - orange - red`（互斥拆分） |
| `status_distribution[]` | array | `t_part` | `{ status_value, count }`，**含** `CANCELLED`，便于前端看分布 |

⚠️ **本端点不含任何工人维度**。有效工时只出现在端点 2 / 3。

## 3. `WorkerStatsListOut` / `WorkerDetailOut` 逐字段

### 3.1 `WorkerStatsItem`（端点 2 的 `items[]` 元素）

| 字段 | 类型 | SQL 来源 | 口径 |
|---|---|---|---|
| `worker_id` | string | `t_worker.id` | 雪花 ID 字符串化（`serialize_i64`） |
| `worker_name` / `badge_code` | string | `t_worker` | |
| `work_type_id` | string \| null | `t_worker.work_type_id` | 雪花 ID 字符串化（`serialize_i64_opt`），未分配工种为 `null` |
| `work_type_name` | string \| null | `t_work_type` | |
| `is_active` | boolean | `t_worker` | |
| `pickup_count` | number | `t_part_event` | `PICKED_UP` 事件条数 |
| `pickup_quantity` | number | 同上 | `PICKED_UP` 的 `quantity` 之和 |
| `participated_part_count` | number | 同上 | distinct `part_id` |
| `contribution_pct` | number \| null | `shared::analytics::worker_contribution` | 贡献度百分比 0-100；无工种或 0 总量 → `null` |
| **`effective_minutes`** | number | 见 §4.1 | **有效工时，分钟整数**。口径细则见 §4 |
| **`worked_process_count`** | number | `COUNT(DISTINCT e.process_id)` | 期内完成过的**不同工序**数。⚠️ SQL 的 `COUNT(DISTINCT)` 天然忽略 `process_id IS NULL` 的行 —— 「链外工序计 0 分钟但不被丢弃」指的是 `effective_minutes` 的求和侧，不是这个 distinct 侧 |
| **`worked_batch_count`** | number | `COUNT(DISTINCT e.batch_id)` | 期内作业过的**不同批次**数 |

⚠️ **行单位是「工人」，不是事件**：一个工人一行（无论他做了多少事件），未软删工人
**全部列出**，区间内无任何事件的工人三个新字段都是 0（零填充，不是缺行）。

### 3.2 `WorkerDetailOut`（端点 3）

| 字段 | 类型 | 口径 |
|---|---|---|
| `worker` | object | `WorkerBrief`：`{ id, name, badge_code, work_type_name, is_active }`；⚠️ **不含 `work_type_id`**（详情页只展示名称） |
| `pickup_count` / `pickup_quantity` | number | 单工人 `PICKED_UP` 条数 / 数量 |
| `participated_part_count` | number | distinct `part_id`，⚠️ **即便 part 已软删也计数**（与 `parts[]` 只列未软删件是两条不同口径） |
| `return_count` | number | `RETURNED_TO_SHELF` 事件条数（2026-10-12 由 `'RETURNED'` 修正为 Rust 侧实际写的字面量） |
| `daily_pickups[]` | array | `{ date, count }`，零填充 |
| `parts[]` | array | `WorkerPartItem`：`{ part_id, serial_no, name, drawing_no, status, pickup_count, last_pickup_at }`，按 `last_pickup_at DESC` |
| **`effective_minutes`** | number | 该工人区间内的有效工时**总额**（分钟整数） |
| **`process_breakdown[]`** | array | 按工序拆分，见 §3.3 |

### 3.3 `WorkerProcessBreakdown`（`process_breakdown[]` 元素）

| 字段 | 类型 | SQL 来源 | 口径 |
|---|---|---|---|
| `process_id` | string \| null | `e.process_id` | 雪花 ID 字符串化（`serialize_i64_opt`）。⚠️ **可空**：写侧只在 `prod::scan` 的 6 个构造点落 `process_id`（2026-10-15 起：worker_scan 3 + pickup 2 + `TRANSFERRED` 1），其余构造点恒 `NULL`；这类事件在此汇成一个「无工序」组，**对 `effective_minutes` 总额的贡献恒为 0** |
| `process_name` | string | `pr.name`（`LEFT JOIN t_process`） | 取不到名时退 **`(已删除)`** 字符串。⚠️ 该 JOIN **刻意不过滤 `t_process.deleted_at`** —— 事后软删是对当前工序字典的行政操作，不追溯抹掉历史报表里的名字（理由见 §4.9）；工序行被软删**仍显原名**。真正落到 `(已删除)` 的只剩「`process_id` 是 NULL」与「工序行物理缺失」 |
| `step_count` | number | `COUNT(*)` | 该工序在该区间内被「完成」的**次数**（⚠️ 不是 distinct） |
| `effective_minutes` | number | §4.1 | 该工序贡献的有效工时（分钟整数） |

⚠️ **`process_id: null` 与 `process_name: "(已删除)"` 是两种不同的「无名」**，前端
不要混用同一个占位文案：前者是「这条作业事件没记工序」（本域绝大多数行的成因），
后者是「记了工序 id 但工序行物理缺失」。⚠️ **工序行被软删不属于后者** —— §4.9 的
JOIN 不过滤 `deleted_at`，软删工序仍显原名。

⚠️ **服务端已排序**（`effective_minutes DESC, step_count DESC, process_id ASC`），
前端不要再排一遍。

## 4. 有效工时口径表

### 4.1 定义

> 有效工时 = 统计区间内，该工人**完成了一道工序**的次数 × 该工序在**工单锚链**
> （`t_part.process_chain_id`）上配置的 `t_process_chain_step.estimated_minutes`，
> 求和。

**单位是分钟整数（`i64` / JSON number），不是小时，也不是浮点。** 本仓所有时长类
字段都是整数；`f64` 在前端累加会出现 `0.1 + 0.2` 类误差。

### 4.2 计入 / 不计入哪些事件

| `t_part_event.event_type` | 计入？ | 理由 |
|---|---|---|
| `RETURNED_TO_SHELF` | ✅ | 工人放回货架 = 做完当前工序，顺工序链推进（写点 `prod::scan::service::worker_scan`） |
| `SENT_TO_INSPECTION` | ✅ | 工人主动送检 = 做完当前工序（同一写点） |
| `PICKED_UP` | ❌ | **开始**取件。一进一出都计则同一道工序被计两次 |
| `TAKEN_FROM_POOL` | ❌ | 同上，从候选池领取是「开始」 |
| `MOVED` | ❌ | 同上；且它有三个方向，只有 POOL→WORKER 是「开始加工」，另两个是放回与转手 |
| `INSPECTED` | ❌ | **品检员**动作，不是工人作业（写点在 `prod::batch` 的批次扫码与状态流转两处） |
| `FORCED_SENT_TO_INSPECTION` | ❌ | 2026-10-17 新增：品检页对「**压在工人手上**的批次」的强制送检（`POST /prod/batches/{batch_id}/to-inspection` 带 `force=true`）。它是**运营代工人做的**一次越权送检，不是工人完成了一道工序 ⇒ 与 `SENT_TO_INSPECTION`（工人**主动**送检、计入）严格区分。**两道独立保险**：① `effective_minutes::WHERE_SQL` 的 `event_type IN ('RETURNED_TO_SHELF','SENT_TO_INSPECTION')` 白名单天然排除本值；② 写侧该事件的 `worker_id` / `process_id` 恒 `None`（`JOIN t_worker w` 是 INNER + `worker_id IS NOT NULL`，见 §4.8）。⇒ **SQL 一字未改**，本行是纯登记 |
| `DELIVERED` | ❌ | 无工序语义 |
| 其余全部 | ❌ | 非「工人完成一道工序」语义 |

**判据是「一进一出只认『出』的那一次」**，不是「凡有工人的事件都算」。

### 4.3 链外工序：**计 0 分钟，不是丢弃该行**

满足以下任一条时，该事件的工时贡献为 **0**：

| 情形 | 判据 |
|---|---|
| 事件没记工序 | `e.process_id IS NULL` |
| 工单没配工序链 | `p.process_chain_id IS NULL` |
| 链内找不到该工序的 step | 锚链内无 `process_id` 匹配的未软删 step |

⚠️ **计 0 ≠ 不统计**：这些行照常计入 `worked_process_count` / `worked_batch_count`
的统计集合（在按工序 breakdown 里体现为一个无工序名的组），只是不贡献工时。

### 4.4 ⚠️ 为什么必须用 `LEFT JOIN LATERAL … LIMIT 1`

`t_process_chain_step` **没有** `(chain_id, process_id)` 唯一约束 —— 唯一的唯一
索引是 `uq_chain_step_chain_order (chain_id, sort_order) WHERE deleted_at IS NULL`。
写侧 `prod::process_chain::service::upsert_chain` 也**只校验链内 `sort_order` 互不
重复、不校验 `process_id` 重复** ⇒ **同名工序在一条链里出现多次是后端照收的合法
形态**（前端工序链编辑页连续「添加工序」且不改工序即是一条）。

因此取工时**必须**是：

```sql
LEFT JOIN LATERAL (
    SELECT s.estimated_minutes
      FROM t_process_chain_step s
     WHERE s.chain_id = p.process_chain_id
       AND s.process_id = e.process_id
       AND s.deleted_at IS NULL
     ORDER BY s.sort_order ASC, s.id ASC
     LIMIT 1
) cs ON TRUE
```

直接写 `LEFT JOIN t_process_chain_step cs ON cs.chain_id = … AND cs.process_id = …`
会让该事件**按命中条数翻倍计入工时**，而且**不报错、不抛异常** —— 只是一个更大的数字。

⚠️ `ORDER BY sort_order ASC, id ASC` 的 `id ASC` 是**兜底排序键**：`uq_chain_step_chain_order`
保证同一链内 `sort_order` 互不重复，故 `sort_order` 已足够确定；补 `id` 是为了在
PG 改执行计划时也不至于顺序不定。

这与 `shared::batch::chain` 把 `hit_count > 1` 判成 `chain_state = 'NONE'` 是同一
事实的两种处置：那边「下一道是谁」有歧义必须**拒答**，这边「这一步的预计工时」取
链上**第一次**出现的那一步即可。

### 4.5 ⚠️ 锚链取 `p.process_chain_id` 单来源

LATERAL 子查询的 `chain_id` **只取** `p.process_chain_id`，**没有**
`COALESCE(p.process_chain_id, step.chain_id)` 之类的回退。

`shared::batch::chain` 里的 `COALESCE(p.process_chain_id, cur.chain_id)` 是**运行时
链定位**纪律 —— 它要兼容「批次 step 指针指向的链与工单当前锚链不一致」的老数据。
**统计侧不需要兼容老数据**：统计的是「历史事件发生时该工单的链上配置」，回退只会让
「无链工单」被误判成有链。

### 4.6 ⚠️ 数据覆盖受工序链配置率限制（**不是 bug**）

`effective_minutes` 只在**已配工序链**的工单上有值。未配链的工单、链内没配该工序的
工单，一律 0 分钟。

⇒ **看到大面积 0，先查工序链的配置率，不要当回归排查。** 链配得越少，这个数字越接近 0
是口径的必然结果。要让该指标有意义，前提是工单普遍配了工序链**且**链上配了
`estimated_minutes`。

（开发库实测：绝大多数工单无 `process_chain_id`，链上 step 的 `estimated_minutes`
多为夹具值 ⇒ 该指标在开发库几乎恒为 0，这是预期形态。）

### 4.7 三条 SQL 共用同一份骨架

「按工人聚合」「单工人总额」「按工序拆分」三条 SQL 共用
`src/modules/statistics/repo/sql.rs` 的 `effective_minutes` 模块常量
（`FROM_SQL` / `WHERE_SQL` / `EFFECTIVE_MINUTES_SQL` / `PROCESS_JOIN_SQL`），
由 `build()` 拼装。**改口径必须改那一处**，三条各抄一遍必然漂移，而漂移的表现是
数字静默错。

⚠️ 拼装用的是 `format!`，返回值经 `AssertSqlSafe` 包裹 —— 注入面为 0：填进去的
四个片段全是模块私有常量或本文件字面量，**用户输入一律走 `bind`**。

### 4.8 `JOIN t_worker w` 是 INNER（不要改）

新聚合 `JOIN t_worker w ON w.id = e.worker_id` 是 **INNER**，与既有的
`worker_pickup_rows` 同款：本维度只统计「真有工人身份」的作业事件，`worker_id`
指向已消失的工人时该行不该进任何统计。**不要**改成 LEFT。

### 4.9 `LEFT JOIN t_process pr` 刻意**不过滤 `deleted_at`**（不要顺手对齐）

按工序拆分那条的显示名来自 `LEFT JOIN t_process pr ON pr.id = e.process_id`，
ON 上**没有** `pr.deleted_at IS NULL` ⇒ **软删过的工序在历史统计里仍显原名**。

理由与 `part::service::phase1::events` 对同一派生的取舍一致：被聚合的行是
`t_part_event` —— 一张**没有 `deleted_at` 列**的不可变审计流水，而统计窗口是**已
过去的闭区间**。事后软删一道工序是对**当前工序字典**的行政操作，不该追溯抹掉历史
报表里的名字；否则一次软删就让整段历史行集体退化成 `(已删除)`。

旁证两条：① 加不加这道闸门都**不改分组**（分组键是 `e.process_id`，事件侧），
`LEFT JOIN` 至多命中一行也不会放大行数；② 仓内「`LEFT JOIN` 取显示名」一律不过滤
（`prod::inspection::repo` / `outsource::repo::sql` / `shared::shelf::pool_priority`
同款），带 `AND pr.deleted_at IS NULL` 的是 **INNER JOIN 筛候选**的场景
（`prod::programming` / `prod::work_type` / `prod::shelf_process`）。

⚠️ 本域**不**与「当前工序字典」口径的那些 SQL 互相对齐 —— 两者场景不同。

### 4.10 ⚠️ 三条聚合没有编译期列名校验

三条有效工时 SQL 走运行时 `sqlx::query()` + `format!` 拼装（见 §4.7），**不进
`query!` 宏** ⇒ `.sqlx/` 里**没有**它们的条目 ⇒ 列名写错 / 列被改名 / `GROUP BY`
漏列都只在运行时暴露，且 `GROUP BY` 漏列这类还可能只在特定数据下才报错。改这三处
口径时改 `effective_minutes` 模块常量，并自己实跑一次。

## 5. 错误码表

| 码 | HTTP | 触发条件 | 出处 |
|---|---|---|---|
| `20104` `BIZ_INVALID_VALUE` | 400 | `date_from > date_to`（三个端点都会） | `service.rs::validate_date_range` |
| `20104` `BIZ_INVALID_VALUE` | 400 | path `{worker_id}` 不是数字字符串（端点 3 专有） | `service.rs::parse_worker_id` |
| `20201` `BIZ_WORKER_NOT_FOUND` | 404 | 工人不存在**或已软删**（端点 3 专有） | `service.rs::worker_detail` |
| `40300` `FORBIDDEN` | 403 | 角色不是 `MANAGER`（三个端点都会） | `handler.rs` 的 `require_role` |
| `40100` / `40102` / `40105` | 401 | 缺 token / token 过期 / session 被吊销 | 全域 auth 中间件 |
| **HTTP 400 纯文本**（无 `R` 信封） | 400 | `date_from` / `date_to` 缺字段或非 `%Y-%m-%d` | axum `Query` 提取器，**不经 `AppError`** |
| `40800` `REQUEST_TIMEOUT` | 408 | 请求级超时中间件到点（全域，非本域特有） | `src/middleware/timeout.rs` |
| 500 | 500 | SQL 失败（`sqlx::Error` → `AppError` 映射） | repo 层 |

⚠️ **本域不产生其它业务码**：统计域是纯只读报表，没有「写入被拒」的语义。

## 6. 移除记录

### 6.1 跳序取件域清空（2026-10-12）

数据源表 `t_pickup_skip_event` 随 migration 删除（写入点早已随 Python v1 退役消失），
连带删除：

- 端点 `GET /api/v2/statistics/pickup-skips` / `GET /api/v2/statistics/pickup-skips/{worker_id}`
  —— **硬切，旧路径 404，无 alias**；
- VO `PickupSkipSummaryOut` / `PickupSkipSummaryItem` / `PickupSkipDetailOut` /
  `PickupSkipDetailItem`（含 `PickupSkipSummaryOut` / `PickupSkipDetailOut` 的分页三件套）；
- DTO `PickupSkipDetailQuery`；
- repo 3 方法 + 2 row struct；
- `tests/statistics/api.rs` 的 2 条对应用例。

⇒ 端点 **5 → 3**。本文件是清空后的**当前态**描述，历史细节见 `CLAUDE.md`
「已清掉的存量基线红」一节的登记。

## 7. 与 WS 的关系

- **本域不发 WS 广播**：3 个端点全是只读，无写操作。
- **本域不消费 WS 事件**：没有失效通道。新鲜度靠前端页面自身的 query 策略
  （挂载 refetch + 可选轮询 + 写操作后显式 invalidate），与全仓其它只读域一致。

## 8. 表依赖与前端配套

### 8.1 读的表

| 用途 | 表 |
|---|---|
| 事件流水（本域的一切统计都从它派生） | `t_part_event` |
| 工单与锚链（`process_chain_id` / `deleted_at` 闸门） | `t_part` |
| 工人身份与工种（INNER JOIN） | `t_worker` / `t_work_type` |
| **工序预计工时**（有效工时唯一来源） | `t_process_chain_step` |
| 工序显示名（breakdown 用，LEFT JOIN） | `t_process` |

⚠️ **本域零跨域模块依赖**：跨域只读聚合是本仓既定 pattern（同 `dashboard` / `admin`），
SQL 直接 JOIN 别的域的表，不 import 别的域的 service / repo —— 例外是
`worker_stats` / `worker_detail` 经 `repo.conn_mut()` 借位调
`prod::worker::repo::WorkerRepo` 与 `prod::work_type::repo::WorkTypeRepo`（与
`DeliveryNoteRepoTrait::conn_mut` 同形）。

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 路径 | 备注 |
|---|---|---|
| api 层 | `src/api/statistics.ts` | 三个函数 `fetchOverview` / `fetchWorkerStats` / `fetchWorkerDetail`，**零 `.parse`** —— 只发请求 + 用 schema 的 `z.infer` 派生类型（`*Data`）标注返回类型 |
| TS 类型 | `src/types/statistics.ts` | 只剩入参类型 `StatisticsQuery`（`{ date_from, date_to }`）；三个端点的出参镜像**已删除**（消费侧全部走 schema 推导类型，避免同一份 wire 契约两个真源） |
| Zod 守门 schema | `src/views/statistics/composables/statisticsSchema.ts` | 三条：`overviewOutSchema` / `workerStatsListOutSchema` / `workerDetailOutSchema`（与三个 query hook 同居域内）。**守门在 queryFn**，不在 api 层 |
| query hook | `src/views/statistics/composables/useStatistics{Overview,Workers,WorkerDetail}Query.ts` | 每条 queryFn 里 `xxxSchema.parse(await fetchXxx())`；键取自 `qk.statistics*` 工厂 |
| 页面 | `src/views/statistics/WorkerStatsTab.vue` | 加「有效工时」列 |
| 页面 | `src/views/statistics/WorkerDetailTab.vue` | 加工时数字卡 + 按工序表 |

⚠️ **雪花 ID 是 JSON string**：本域所有 id 字段（`worker_id` / `part_id` /
`process_id` / `work_type_id`）都是 string，不要按 `number` 声明。

⚠️ **Zod 是 strip 模式**：后端加字段不会让 Zod 报错，只会被静默丢弃 ⇒ 新增字段时
schema 必须同步声明（`workerProcessBreakdownSchema` 的 4 个键、三个新工时字段都
是本轮这样补进去的）。

⚠️ **`effective_minutes` 是 JSON number**（分钟整数），不是 string —— 它不是雪花
ID，不要误用 `z.string()`。

### 8.3 已知偏差登记

1. **`effective_minutes` 在开发库几乎恒为 0**：口径要求工单配了工序链且链上配了
   该工序，而开发库绝大多数工单无链（§4.6）。**不是回归**，也不要为了「让它有数」
   而放宽口径（如给无链工单兜一个默认工时）—— 那会让这个指标失去意义。
2. **`StatisticsService::overview` / `worker_stats` / `worker_detail` 无 happy-path
   集成测试**：原有的 3 条因查询窗口硬编码绝对日期（跨月即红）已被删除。本轮新增的
   有效工时 SQL 因此也**没有**集成测试覆盖，只由 §4 的 SQL 形态与开发库实测保证，
   且三条聚合**没有编译期列名校验**（§4.10）。补测试时遵守「插入行用 `now_naive()`
   时查询窗口不得硬编码绝对日期」的约定。
