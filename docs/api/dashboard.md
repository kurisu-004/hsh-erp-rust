# dashboard 域 API（HTTP 首取 + WS 大屏）

> 本文件是 dashboard 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/dashboard/snapshot` | 登录即可（**无角色闸门**） | 无 | `DashboardSnapshot` |
| 2 | GET | `/api/v2/dashboard/upcoming-delivery` | 登录即可 | `days?`（string-or-number，缺省 14，clamp 1..60）、`basis?`（`planned` / `system`，缺省 `system`） | `UpcomingDeliveryBuckets` |
| 3 | GET | `/api/v2/dashboard/delivery-orders` | 登录即可 | `date`（**必填** `YYYY-MM-DD`）、`statuses`（**必填**，逗号分隔）、`basis?`（缺省 `system`） | `DeliveryOrderDetailOut` |
| 4 | GET | `/ws/dashboard` | `?token=` JWT + Redis session | `token`（必填 query 参数） | WS：首帧 snapshot + 增量事件 + text 心跳 |

- 三个 HTTP 端点都是**只读聚合**，返回统一信封 `R { code, message, data }`。
- 端点 1 **不接受**任何 query 参数（分桶已拆到端点 2）。传旧参数 `upcoming_days` / `basis` 不会报错，但被忽略。
- 端点 2 / 3 的 `basis` 非法取值（如 `?basis=xxx`）由 axum `Query` 提取器返 **HTTP 400 纯文本**，**不走 `R<T>` 信封**。
- 端点 3 的 `date` 缺失 / 非法格式、`statuses` 缺失 / 全空白一律走 `AppError::validation`（**40001** / HTTP 422，走 `R<T>` 信封）。
- 端点 3 的 `statuses` 有**限长闸门**：原始串 ≤ **256 字节**、元素数 ≤ **16**（`STATUSES_MAX_RAW_LEN` / `STATUSES_MAX_ITEMS`），超限同样走 40001。超限是防误传巨串——整份参数会绑进 `status = ANY($2::varchar[])`。
- 端点 3 的 `statuses` **不校验元素是否属白名单**：不属于 `DELIVERY_STATUSES` 的字面量一律**查 0 行**（不是 400），前端因此可以先于后端上线新图层状态。

## 2. `DashboardSnapshot` 逐字段

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `overdue_count` | number | `repo/delivery.rs::count_overdue`：`t_part`（`assembly_id IS NULL`）+ `t_assembly` 的 `UNION ALL` 后 `COUNT(*)`，两侧各带一个「无已交批次」的 `NOT EXISTS`（见 §4.4） |
| `in_inspection_count` | number | `repo/sql.rs::count_inspection_batches`：`t_part_batch` JOIN `t_part`，`status='INSPECTION'` + 双软删闸门 + `current_holder_id IN (品检区 active 货架)` |
| `in_process[]` | array | `repo/sql.rs::fetch_worker_rows`：`t_part_batch` JOIN `t_part`，`status='IN_PROCESS' AND location='WORKER'` |
| `system_delivery_orders` | object | `repo/delivery.rs::list_system_delivery_orders`（见 §2.2） |
| `ts` | string | 服务端时间戳（`infra::clock::now_shanghai_iso()`，格式见 §7） |

### 2.1 `in_process[]`（`WorkerHeldBatch`，7 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_part.id`（雪花 ID 字符串化，防 JS 精度截断） |
| `batch_id` | string | `t_part_batch.id`。**必须保留**：前端列表以它做 `:key`（`t_part` 无唯一约束，同一工单的多个 IN_PROCESS 批次会产生多行） |
| `serial_no` | string \| null | `t_part.serial_no` |
| `quantity` | number | `t_part_batch.quantity`（**批次量**，不是 `t_part.quantity`） |
| `is_urgent` | boolean | `t_part.is_urgent` |
| `current_holder_id` | string \| null | `t_part_batch.current_holder_id` |
| `worker_name` | string \| null | `t_worker.name`（按 holder_id 批量查，非逐行） |

### 2.2 `system_delivery_orders`（`SystemDeliveryOrders`）

2026-10-10 拆成**三桶**（原 `{urgent, partial}` 已下线）。三桶**全部是工单级**：
`t_part`（`assembly_id IS NULL`，散件）与 `t_assembly`（装配件）各出一行，
**装配件行替换其子件行** —— 子件只作为装配件已交量的计算中间量，不再单独出行。

| 字段 | 类型 | 说明 |
|---|---|---|
| `upcoming` | object | `system_delivery_date >= today` + 一件没交过，按 `system_delivery_date ASC NULLS LAST, id ASC` |
| `overdue` | object | `system_delivery_date < today` + 一件没交过，同上排序。**与端点 1 的 `overdue_count` 严格对数**（见 §4.4） |
| `partial` | object | **已交过一部分，无任何时间窗口**，同上排序。含 `system_delivery_date IS NULL` 的工单 |

每个桶是同一个结构（`DeliveryBucket`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[]` | array | 最多 **30** 行（`DELIVERY_BUCKET_LIMIT`） |
| `total` | number | **匹配总数，不受 `items` 截断影响**（SQL 侧 `COUNT(*) OVER ()`）。裸 JSON number，与 `overdue_count` 一致。零命中时为 `0`（无行 ⇒ 窗口函数无从求值） |

行字段（`SystemDeliveryOrder`，10 个）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | `t_part.id` / `t_assembly.id`（雪花 ID 字符串形态，防 JS 精度截断） |
| `serial_no` | string \| null | 两侧同名列 |
| `name` | string | 两侧同名列 |
| `quantity` | number | `t_part.quantity`（**件数**）/ `t_assembly.quantity`（**套数**）—— 随 `row_type` 变 |
| `status` | string | 两侧共用同一份 6 态白名单；`t_assembly` 侧天然只落 4 态（见 §4.2） |
| `system_delivery_date` | string \| null | `YYYY-MM-DD`；`partial` 桶内可为 null |
| `customer_name` | string \| null | 二级客户名。两侧 `customer_id` 同指 `t_customer.id`，**一次**批量查 |
| `is_urgent` | boolean | 两侧同名列 |
| `delivered_quantity` | number | 件级：已交**件数**；装配件级：已交**套数**（min 公式，见 §4.4） |
| `row_type` | string | `"PART"` \| `"ASSEMBLY"`。前端据此切换「件 / 套」单位与下钻目标 |

判据（是否已交）**全部在 SQL 里**（`repo/delivery.rs` 的三个 `SQL_ORDERS_*` 常量），
不拉全量回 Rust 分桶 —— 否则「前 30 条」会变成「先截断再分桶」的错误口径。
三桶各 1 条主查询 + 3 次批量聚合（散件已交件数 / 装配件已交套数 / 客户名），
**SQL 条数上界 6 条且与命中行数无关**（聚合入参集为空时各跳过 1 条）。

## 3. 交期分桶与抽屉

### 3.1 `UpcomingDeliveryBuckets`

| 字段 | 类型 | 说明 |
|---|---|---|
| `today` | string | **后端**判定的今天（`YYYY-MM-DD`，口径 `infra::clock::now_naive()` = Asia/Shanghai）。与 `buckets[0].date` 是**同一个值** |
| `buckets[]` | array | 恒为请求的 `days` 条，缺失日期已在 Rust 侧零填充；每条 `{ date, count, by_status }` |
| `ts` | string | 服务端时间戳（格式见 §7） |

`by_status` 是 `OrderStatus → 件数` 的 map（`BTreeMap`，key 字母序确定）。SQL 排除了 `COMPLETED` / `CANCELLED`。

**窗口下界来自传入的 `today`，不是 SQL 的 `CURRENT_DATE`**：SQL 窗口是 `[today, today + days)`，两个端点都用 `$2` 绑定 service 取的那一个 `today`（`infra::clock::now_naive()`，Asia/Shanghai）。`CURRENT_DATE` 是 DB **会话时区**的今天，与前者是两个独立时钟；不一致时（Asia/Shanghai 00:00–08:00 共 8 小时窗口）当天行会落进一个不生成的桶被静默丢弃、末桶恒 0。集成测试 `snapshot_counters_window_anchors_on_passed_today_not_current_date` 钉住这条。

### 3.2 `DeliveryOrderDetailOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `date` | string | 回显请求的日期 |
| `basis` | string | 回显请求的口径（`planned` / `system`） |
| `total` | number | 匹配总数，**不受 `items` 截断影响**（前端据此显示「共 N 件」）。裸 JSON number，不字符串化 |
| `items[]` | array | 最多 **200** 行（`DELIVERY_DETAIL_LIMIT`） |
| `ts` | string | 服务端时间戳（格式见 §7） |

行字段（`DeliveryOrderDetail`）：`id`（字符串）/ `serial_no` / `drawing_no` / `name` / `l1_customer_name` / `customer_name` / `status` / `planned_delivery_date` / `system_delivery_date`。

**两列交期恒同时返回**（`planned_delivery_date` 是 NOT NULL 列），前端倒计列按自己的 basis 选列渲染。客户名两级批量查（`l1_customer_name` 取 `t_customer.parent_id` 对应客户名；叶子无 parent 时 L1 退化为叶子名，保证该列非空）。

## 4. 口径表

`DELIVERY_STATUSES`（`repo/delivery.rs`，6 态）是「未交付」的**唯一**判据，被四处共用：

| 用途 | SQL 状态条件 |
|---|---|
| 逾期计数（端点 1 `overdue_count`） | `status = ANY(DELIVERY_STATUSES)` **且** 无已交批次（`NOT EXISTS`） |
| 交期面板 `upcoming` / `overdue` 桶（端点 1） | 同上（**与逾期计数逐字同谓词**，见 §4.4） |
| 交期面板 `partial` 桶（端点 1） | `status = ANY(DELIVERY_STATUSES)` **且** 有已交批次（`EXISTS`） |
| 柱状图下钻抽屉（端点 3） | `status = ANY(DELIVERY_STATUSES)`（允许前端按层传子集）。**不含**已交量守卫 |
| 柱状图分桶（端点 2）`by_status` | top(4) + middle(2) 正是这 6 态；**bottom 层额外含 `DELIVERED`** |

「已交批次」的判据三处一致：`t_part_batch.status IN ('DELIVERED','COMPLETED')` + `deleted_at IS NULL`；
装配件侧经 `t_part`（`assembly_id = a.id`，未软删）找其子件的批次。

### 4.1 行单位（跨端对数前必读）

| 用途 | 行单位 | 装配件处理 |
|---|---|---|
| 逾期计数 | **工单级** | 算 1 条（查 `t_assembly`；`t_part` 侧用 `assembly_id IS NULL` 排除子件） |
| 交期面板三桶（端点 1） | **工单级** | 算 1 条并**替换其子件行**（子件不再单独出行，只作为已交套数的计算中间量） |
| 柱状图 / 下钻抽屉（端点 2 / 3） | **件级** | 不出现（`t_part` 全表含子件，子件各算 1 件） |

⚠️ **「时间窗口不重叠」这条不变式已于 2026-10-10 作废**：`partial` 桶刻意**无时间窗口**
（产品决议：「部分已交不应该限制时间范围，应该扫出全部的部分已交工单」），故它与 `overdue`
在 `< today` 区间上**必然重叠** —— 一条已交一部分且已逾期的工单会同时进 KPI、overdue 排除集
（被 NOT EXISTS 排除，故不出现在 overdue 桶）与 partial 桶。这是产品决议，不是缺陷；
登记见 §8.3。

仍然成立的对数关系只有一条：**`overdue.total == overdue_count`**（两者同谓词），
`overdue.items.len()` 则还要再受 30 条上限截断，故只能说 `<=`。

### 4.2 `t_assembly` 侧只命中 4 态

`DELIVERY_STATUSES` 在逾期计数与三桶的 `t_assembly` 侧复用同一份白名单，但 `AssemblyStatus` 只有 7 态（**无 `PROGRAMMING` / `OUTSOURCE`），故天然只命中 `PENDING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP`。这是有意的：白名单按 part 状态域取全集，不需要第二份常量。

### 4.3 两份 `count_overdue`（不要误判为「违反分叉约定」）

仓里有**两份**口径完全不同的逾期计数，别把它们混读：

| | dashboard 的 `SQL_COUNT_OVERDUE` | `statistics::repo::sql::count_overdue_undelivered` |
|---|---|---|
| 消费方 | 大屏 `snapshot.overdue_count` | 生产统计页（前端 `OverviewTab.vue`） |
| 交期列 | `system_delivery_date` | `planned_delivery_date` |
| 已交量守卫 | ✅ 2026-10-10 新增：两侧各一个 `NOT EXISTS` | ❌ 无。改用 `NOT EXISTS (… DELIVERED 事件)` 兜底 |
| 事件兜底 | ❌ **刻意不加**：派生状态滞后窗口只影响一次刷新，事件表兜底是过度设计 | ✅ 有，且有事件口径测试 |
| 改动约束 | 改它必须同步 `SQL_ORDERS_OVERDUE`（对数要求） | **不要动它** |

「不要动它」那条禁令**只针对右列**。左列 2026-10-10 加的已交量守卫是产品口径决策，
与右列的有意分叉并不冲突 —— 分叉点是「交期列 + 事件兜底」两处，右列两处都没动。

### 4.4 装配件整套交付不变式 ⇒ 两个谓词等价（2026-10-10）

**业务不变式：装配件只能整套交付，不允许单独交子件。** 送货单路径经 `entry_max_sets` 闸门
（`com::delivery_note::service::scan_entry`，错误码 **21405**）强制这一点。

**不变式下的等价推导**：装配件总套数 N、交 k 套 ⇒ 每个子件 c 交 `k × c.quantity / N` 件，于是

```
per_set(c)     = (子件已交件数 × N) / NULLIF(c.quantity, 0) = k
delivered_sets = MIN over c (per_set(c)) = LEAST(k, N) = k
⇒ delivered_sets == 0 ⟺ k == 0 ⟺ 无任何子件被交付 ⟺ NOT EXISTS(子件有已交批次)
```

⇒ 「一件没交过」这件事有**两个等价的判据**：SQL 里的 `NOT EXISTS(已交批次)`，与
Rust 侧算出来的 `delivered_sets == 0`。逾期 KPI（`SQL_COUNT_OVERDUE`）与
`upcoming` / `overdue` 两桶在**散件与装配件两侧都对齐**，即
`overdue.total == overdue_count`。

⚠️ **两个谓词字面不同、语义等价。不要把其中任何一条当成漏判「修」掉。**

**不变式被破坏后的实际现象**：三桶的归属判据是 SQL 里的 `NOT EXISTS` / `EXISTS`
（**不是** `delivered_quantity` 是否为 0），所以 KPI 与三桶的**行集合**在不变式被破坏时
**仍然互斥**（`NOT EXISTS` ⟹ `delivered_sets` 恒为 0，反向不成立）。真正错位的是
**展示值**：一个「子件 A 交满、子件 B 一件没交」的半套装配件会落 `partial` 桶
（`EXISTS` 命中），`delivered_quantity` 却是 min 公式给出的 **0 套** ⇒ 前端会看到
「部分已交 / 0 套」。只有把分桶挪回 Rust 按 `delivered_quantity > 0` 判定，才会退化成
真正的「KPI 不计但面板有行」反向差 —— **不要那样改**。

**唯一破坏路径**：`POST /api/v2/prod/batches/{id}/deliver`
（`prod::batch::repo::sql::mark_batch_delivered` 只查
`allowed_from: &["READY_TO_SHIP"]`，**无装配件套数校验**，能单独交子件）。
送货单路径维持不变式。存量违规数据由人工清理。

**装配件行的 `delivered_quantity` = 已交套数**（不是件数），公式
`LEAST(COALESCE(MIN((子件已送件数 × a.quantity) / NULLIF(c.quantity, 0)), 0), a.quantity)`
与 `part::service::list_enrichment::fetch_delivered_sets` **逐字同源**，且在 dashboard 域
**复刻**了一份 —— 域隔离护栏（`cargo test --lib` 的
`modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain`）禁止 import 他域
实现，只读跨域聚合是本仓既定 pattern。**改动时两处必须同步。**

`COALESCE` 必须在 `LEAST` **里面**：PG 的 `LEAST` 忽略 NULL 实参，写成
`COALESCE(LEAST(MIN(...), a.quantity), 0)` 会在「子件总量全为 0、`MIN` 为 NULL」时返回
`a.quantity`（整套全交），与口径正好相反。

**无子件的装配件**不产生聚合结果行（SQL 以子件表为驱动表），调用方 `.unwrap_or(0)` 兜底
⇒ `delivered_quantity = 0`，落 `upcoming` / `overdue` 桶。这与逾期 KPI 计它的行为一致，
**是刻意的**。

## 5. 状态域约定（无编译期保障）

- 后端常量：`repo/delivery.rs::DELIVERY_STATUSES`
- 前端对应：柱状图 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses`——`top`（PENDING /
  PROGRAMMING / IN_PROCESS / OUTSOURCE）+ `middle`（INSPECTION / READY_TO_SHIP）合起来
  正是本常量的 6 态，`bottom`（DELIVERED）额外多一个。
  2026-10-07 起交期面板的分桶判定改为服务端按「已交批次」判定（2026-10-10 又把面板从
  两桶拆成三桶），6 态在前端**只剩 `LAYERS[].statuses` 这一个镜像**（见 §2.2）。

两者是**人工同步**关系：Rust 常量与 TS 字面量之间没有任何编译期约束，漂了不会编译失败，
只会让「逾期数」与「柱状图层数」互相矛盾（且现象是数字对不上、极难定位）。改任一侧必须同步另一侧；
集成测试 `overdue_accepts_all_six_delivery_statuses` 把后端常量的字面值钉死，也只会抓到这一种症状。

## 6. 移除记录

### 6.1 2026-10-10（交期面板拆三桶）

| 被移除项 | 原因 |
|---|---|
| `system_delivery_orders.urgent[]`（裸数组） | 「最紧急（`>= today` 且一件没交过）」这一块拆成 `upcoming`（时间窗）+ `overdue`（过期窗），前者保留、后者新增；两块都不是裸数组而是 `{items, total}` 对象 |
| `system_delivery_orders.partial[]`（裸数组，**有时间窗** `[today, today+7)`） | 产品决议：「部分已交不应该限制时间范围」⇒ 该桶**取消时间窗口**并改名保留为 `partial` 对象 |
| 面板的行单位 `件级` → `工单级` | 装配件开始作为面板行出场并**替换**其子件行；行单位随之从「件」变「工单」，新增 `row_type` 供前端区分 |
| `repo::DELIVERY_WINDOW_DAYS`（`i64 = 7`） | 面板唯一的窗口上界。`upcoming` 桶不再有上界（`>= today` 全收），`partial` 桶本就无窗口 ⇒ 常量零引用 |
| `SQL_SYSTEM_DELIVERY_ORDERS`（单条主查询 + Rust 侧分桶） | 三桶各自一条主查询（`SQL_ORDERS_UPCOMING` / `_OVERDUE` / `_PARTIAL`），判据与截断全在 SQL 内 |

**破坏性变更**：前端拿到的 JSON 形状变了（键名 + 数组→对象 + 新增 `row_type`），
按 §8.2 第 4 条同步。

### 6.2 2026-10-07

| 被移除项 | 原因 |
|---|---|
| `snapshot.on_production_shelves`（整棵嵌套树） | 前端零渲染（货架轮播区已下线），后端仍在全额计算整棵树 + 5 条附表 SQL |
| `snapshot.on_inspection_shelves`（行类型 `DashboardItem`，19 字段） | 前端只取 `.length` 喂「在检」KPI → 改为后端 `COUNT(*)` |
| `snapshot.upcoming_delivery` | 分桶数据量与刷新频率都与快照主体不同（`days` / `basis` 可变）→ 拆到端点 2 |
| `GET /snapshot?upcoming_days=` / `?basis=` | 随分桶一起迁到端点 2（快照本身无口径概念） |
| `shared::analytics::shelf_grouping` | 唯一调用方是给 `on_production_shelves` 分桶；随该字段下线后成为死码 |

`snapshot.in_process` 由 19 字段收窄到 7 字段（`WorkerHeldBatch`）：12 个字段零消费，其中 5 个各自对应一条额外 SQL（客户路径 / 工序名 / PICKED_UP 时间等）。原「每 holder top-N」限流整体移除（工厂规模用不上，截流只会让在制清单莫名缺行）。

## 7. 与 WS 的关系

- **首帧 snapshot 仅作连接就绪信号**：`GET /ws/dashboard` 握手后推一帧 `{type:"snapshot", data:<DashboardSnapshot>, ts}`，前端据此确认连接可用。数据主体请走 HTTP 端点 1（语义是「WS 事件 → invalidate → HTTP 重取」，不是增量 patch）。
- **`ts` 时间戳格式（全域唯一口径）**：所有 message 的 `ts` 都是 **RFC 3339 / ISO 8601 带固定偏移**字符串，恒为 `YYYY-MM-DDTHH:MM:SS[.小数秒]+08:00`，由 `infra::clock::now_shanghai_iso()` 产生。小数秒位数按纳秒有效位自适应（0 / 3 / 6 / 9 位），**不保证逐字等长**——JS `new Date(...)` 两种都能解析，前端不要按固定小数位数做字符串截取比较。外层 envelope 的 `ts` 与嵌套 `data.ts` 同格式、同为 Asia/Shanghai（宿主时区不影响）。
  - 唯一例外是心跳帧：`{type:"heartbeat", ts:<unix 秒整数>}`（非字符串，见下）。
- **`WsEvent::DashboardSnapshot` 已删**（零生产方）。`WsEvent` 现在只有 `DashboardEvent { kind, payload }` 一个变体。
- **`kind` 事件集**（后端全量 `ws_hub.broadcast` 生产方）：

  `ASSEMBLY_CANCELLED` / `ASSEMBLY_CREATED` / `ASSEMBLY_DELETED` / `ASSEMBLY_UPDATED` / `BATCH_TO_INSPECTION` / `BATCH_TO_SHIP` / `PART_BATCH_WITH_PDFS_CREATED` / `PART_COMPLETED` / `PART_DELIVERED` / `PART_SOFT_DELETED` / `PART_TO_INSPECTION` / `PART_TO_PROCESS` / `PART_TO_SHIP` / `ROLLUP_RECOMPUTED` / `WORKER_POOL_AUTO_ALLOCATE_DONE` / `WORKER_POOL_EMPTY` / `WORKER_POOL_MOVE_DONE` / `WORKER_POOL_REFILL_DONE` / `DELIVERY_NOTE_CREATED` / `DELIVERY_NOTE_PARTS_ADDED` / `DELIVERY_NOTE_SUBMITTED` / `DELIVERY_NOTE_PICKED_UP` / `DELIVERY_NOTE_SCAN_ADD` / `DELIVERY_NOTE_BATCHES_ATTACHED` / `DELIVERY_NOTE_DRIVER_SET`

  前端 `AFFECTS_DASHBOARD` 白名单与之人工对应（关系是**子集**：并非每个 kind 都影响大屏）。

  2026-10-08 两条补充：
  - **新增** `DELIVERY_NOTE_DRIVER_SET`（`com::delivery_note` 的 `POST /{id}/driver`，指定送货司机）。
  - `DELIVERY_NOTE_CREATED` / `DELIVERY_NOTE_PARTS_ADDED` / `DELIVERY_NOTE_BATCHES_ATTACHED`
    三个 kind **已无生产方**（对应端点随「入单收敛为扫码单一入口」下线），仍留在本清单里作为
    历史全集；当前 `DELIVERY_NOTE_*` 只有 4 个在用：`SCAN_ADD` / `SUBMITTED` /
    `DRIVER_SET` / `PICKED_UP`。逐条写端点对照见
    [`delivery_note.md` §7](delivery_note.md#7-与-ws-的关系)。
- 心跳：text 帧 `{type:"heartbeat", ts:<unix 秒>}` + 协议层 `Message::Ping`（前端 JS 不可见，服务端判活用）。

### 7.1 关闭码表（前后端联合契约）

**关闭码只表达「这条连接必须终止」，处置方式由 reason 文案区分。** 前端按 `(code, reason)`
二元组分流，不要只看 code。「发出点」列给出**触发该关闭的位置**：由服务端发出的 7 行标的是
`src/modules/dashboard/handler.rs` 内的具体分支 / 时机；`1000` / `1001` / `1006` 三行没有
服务端发出点（分别是前端主动 `close()`、本仓从不使用的规范码、网络层裸断）。

| code | reason | 发出点 | 含义 | 前端应做什么 |
|---|---|---|---|---|
| `1000` | （浏览器不发 reason） | 前端主动 `close()`（VueUse `useWebSocket` 的 `open()` 会先关旧连接） | 正常收摊，不是故障 | 自己发起的照常无需处置（不记错误、不弹提示）。⚠️ **code 1000 也可能由服务端 / 中间层主动发出**，那属于真实断开、必须照常记一次失败，故前端另有一道兜底判定：按 **socket 实例同一性**（不是 code）识别「这次 close 是自己发起的」，非自己发起的 1000 一律走失败记账 |
| `1001` | （后端不发） | **无服务端发出点**，保留仅作对照 | 规范里的「端点离开」；本仓不用它 | 与 `1000` 同属「正常收摊」，照常继续无限重试。**别按 code 分流**：写侧失败路径一律裸断（见下「写侧已失败的路径不发 Close 帧」），浏览器侧落的是 `1006` 而不是 1001 |
| `1006` | — | 无 Close 帧：裸 TCP 断（网络抖动 / 代理掐 / 进程被 kill / 写侧失败后的裸断） | 连接非正常终止，看不到任何服务端信号 | 照常退避重连；40105 一类的会话信号只能靠 HTTP 侧感知 |
| `1011` | `snapshot build failed` | `run_socket` 进主循环前的首帧快照构建失败 | 服务端算不出大屏数据（DB 故障） | 退避重连，不要登出 |
| `1011` | `pong timeout` | 超过 `WS_PONG_TIMEOUT_SECONDS` 未收到任何入站帧 | 对端已死 / 半开 TCP | 退避重连，不要登出 |
| `1011` | `re-auth unavailable` | 周期 re-auth 失败**且**失败码不是 40100/40102/40105（`reauth_close_code` 兜底段，主要是 Redis 故障的 `50000`） | **服务端**不可用，与用户会话无关 | 退避重连，不要登出（切勿据此清本地 token） |
| `1012` | `server restart` | 服务优雅退出（`state.shutdown` 被 cancel：Ctrl-C / 部署） | 服务端要重启了 | 立刻重连 |
| `4001` | `auth expired` | 周期 re-auth 命中 `UNAUTHORIZED`(40100) 或 `SESSION_REVOKED`(40105) | **会话真被吊销**（登出 / 改密 / 管理员停用 / refresh reuse detection） | 终止会话：清本地 token + refresh token，跳登录页。**不要**重连 |
| `4001` | `access token expired` | 周期 re-auth 命中 `TOKEN_EXPIRED`(40102) | **只是这枚 access JWT 过期**，Redis session 通常还活着 | 先用 refresh token 续期再重连；续期失败（refresh 也过期 / 被吊销）才终止会话跳登录页 |
| `4003` | `lagged` | 广播队列（容量 1024）溢出，tokio 已永久丢弃 n 条事件 | 慢消费方，本连接漏事件 | 重连 + **全量 HTTP 重取**（事件是「invalidate → 重取」语义，不补发增量） |

**4001 的两段 reason 是本表的核心**（2026-10-09 拆分，线上缺陷修复）：大屏页开着时用户零
HTTP 流量，access token（`JWT_ACCESS_TOKEN_EXPIRE_SECONDS`，缺省 900s）自然过期是**常规
现象**，与「会话被吊销」完全不同。二者共用一个 reason 时，前端只能一律清掉**仍然有效**的
refresh token 并跳登录页 ⇒ 空闲用户被踢下线。决策点收在纯函数 `reauth_close_code`
（`src/modules/dashboard/handler.rs`）。

**上表 10 行的测试覆盖并不齐整**（`tests/dashboard_ws_api.rs` 的真实 e2e 清单，逐行如实登记）：

| 覆盖形态 | 涉及的行 |
|---|---|
| 端到端（WS 集成用例） | `1011 pong timeout`（`ws_e2e_pong_timeout_closes_dead_peer`）、`1012 server restart`（`ws_e2e_server_shutdown_sends_1012`）、`4001 auth expired`（`ws_e2e_reauth_failure_sends_4001_close`）、`4001 access token expired`（`ws_e2e_access_token_expiry_sends_4001_with_access_token_expired`）、`4003 lagged`（`ws_e2e_lagged_client_gets_4003_close`） |
| 仅 lib 单测（`reauth_close_code` 纯函数，无 IO） | `1011 re-auth unavailable`：`reauth_infra_failure_maps_to_1011_not_4001` + `reauth_unknown_code_defaults_to_1011`；两条 `4001` 另有 `reauth_token_expired_maps_to_4001_with_distinct_reason` / `reauth_revoked_session_maps_to_4001_auth_expired` 钉死 reason 串 |
| 无自动化覆盖 | `1000`（由前端自己发起，与后端无关）、`1001`（后端从不发出，无可构造的触发路径）、`1006`（网络层裸断，无法稳定构造）、`1011 snapshot build failed`（需制造 DB 故障）、`1011 re-auth unavailable` 的**端到端**（需自定义 `SessionStore` 才能造 Redis 故障） |

「无自动化覆盖」里的后两项——`1011 snapshot build failed` 与 `1011 re-auth unavailable` 的
端到端形态——是**已知缺口，不追求补齐**：造 DB 故障与自定义 `SessionStore` 的代价远大于收益，
且这两条的前端处置（退避重连、不登出）与已在 e2e 里验过的 `1011 pong timeout` 同形，
前端逻辑不会因此漏分支。改上表任一行时同步核对这张覆盖表。

**三个必须知道的边界**：

- **`4001` 只在已建立的连接上有效**。握手阶段的鉴权失败不会产生 Close 帧——服务端在
  upgrade 前就返 HTTP 401，浏览器 WS API 不暴露握手期状态码，前端只会看到 `1006`。
  所以「会话真死」的最终兜底在 HTTP 侧（40105 → 登出），不是 WS 侧。
- **re-auth 周期** = `WS_HEARTBEAT_INTERVAL_SECONDS` × `WS_REAUTH_EVERY_N_HEARTBEATS`
  （缺省 30s × 10 ≈ 5min），所以上表里 4001 类关闭**最晚滞后一个周期**才到达。re-auth
  调用套 5s 超时：Redis 卡住只跳过本轮、不判死（此时问题在服务端，不在用户会话）。
- **写侧已失败的路径不发 Close 帧**（共 5 条：初始快照写失败 / 回 Pong 写失败 / 广播事件写
  失败 / 心跳写失败 / Ping 写失败）。此时 socket 已不可用，`send(Close)` 必然再失败一次，
  只会多一条噪音日志；前端侧表现为连接直接结束（等效 `1006`）。因此前端**不能**假设
  「服务端要断就一定给 Close 帧」，重连逻辑必须能兜住「无理由断流」。

## 8. 表依赖与前端配套

### 8.1 读的 5 张表

`t_part` / `t_part_batch` / `t_assembly` / `t_customer` / `t_worker`。

dashboard 是**只读跨域聚合域**——这是本仓既定 pattern（`statistics` / `admin` 同形）：不 import 其它域的 service / repo，而是在本域 SQL 里直接聚合。`cargo test --lib` 的 `modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain` 把这条边界变成 CI 强制（扫 `src/modules/dashboard/**/*.rs`，除本域外任何 `crate::modules::<他域>` import 即失败）。

### 8.2 前端配套改动清单

1. **composable 增删**（对照 `views/dashboard/composables/` 实际落地）
   - 删：`useDashboardUrgentList` / `useDashboardOverdue` / `useDashboardUpcomingList` 三个
     composable，及承载其全部过滤逻辑的 `utils/systemDeliveryOrders.ts`（整文件）
   - 留：`useDashboardSnapshot` —— 新增 `overdue_count` / `in_inspection_count` /
     `system_delivery_orders`；`in_process` 行类型收窄到 7 字段 `WorkerHeldBatch`
     （原 `DashboardItem` 19 字段）；不再返回 `on_production_shelves` /
     `on_inspection_shelves` / `upcoming_delivery`
   - 新增：`useDashboardUpcoming` —— 端点 2 `GET /api/v2/dashboard/upcoming-delivery`
     的 `today` + `buckets[]`
   - 新增：`useDashboardDeliveryOrders` —— 端点 3 `GET /api/v2/dashboard/delivery-orders`
     的抽屉明细（`date` + `statuses` + `basis`），取代原 `useDashboardUpcomingList`
   - 视图侧：`SystemDeliveryOrdersPanel.vue` 数据源自 `com/union-list` 切到快照的
     `system_delivery_orders`；`DashboardKpiTiles.vue` 读 `overdue_count` /
     `in_inspection_count`；`UpcomingDeliveryChart.vue` + `UpcomingDeliveryListDrawer.vue`
     接端点 2 / 3
2. **`deliveryBasis` 缺省改为 `system`**（原 `planned`）。后端 `DeliveryBasis::Default` 已同步改为 `System`。
3. **`gcTime: POSITIVE_INFINITY` 例外的适用 query 集合变化**：原来只需对 `snapshot` 长缓存（WS 事件驱动 invalidate）；现在 `upcoming-delivery` 与 `delivery-orders` 也应进例外集合（`today` 锚点 + 窗口下限决定它们天然按天变化，不该在跨零点时被旧数据卡住）。
4. **窗口过滤、已交判据、三桶 30 条截断全在服务端**（前端不再自己过滤）。⚠️ 2026-10-10
   面板从 `{urgent, partial}` 改成 `{upcoming, overdue, partial}` **三个对象**，每个对象是
   `{items[], total}` 而**不再是裸数组** —— 前端两处消费点（`SystemDeliveryOrdersPanel.vue`
   与其所在的交期面板容器）都要跟着改，且 `urgent` 这个键**已不存在**。
   新增行字段 `row_type`（`"PART"` / `"ASSEMBLY"`）：装配件行的 `quantity` 与
   `delivered_quantity` 的单位是**套**而非件，展示单位必须按 `row_type` 切换。
   `DELIVERY_WINDOW_DAYS`（原 7 天上界）已删除：`upcoming` 桶不再有窗口上界。
5. **逾期数来源变更**：原为调 `GET /statistics/overview` 取 1 个数字（后端跑 9 条 SQL、返 16 标量 + 2 数组 + 2 嵌套结构，且口径是 `planned_delivery_date`，与同页右栏面板的 `system_delivery_date` 互相矛盾）→ 现直接读 `snapshot.overdue_count`。
6. **「在制」标签改名「在加工」**：`in_process` 的 SQL 硬约束是 `location='WORKER'`，语义是「已从货架/品检区出池、压在工人手上」，叫「在制」是误导。
7. **WS 关闭处理按 `(code, reason)` 二元组分流**（2026-10-09，与 §7.1 同批）：`4001` 按 reason 拆两段——`auth expired` ⇒ 终止会话并跳登录页；`access token expired` ⇒ 先 refresh 再重连。判别依据只有 reason 串本身，**不要**改写/规范化它（大小写、前后空格都算契约）。

### 8.3 已知偏差登记

柱状图三层只覆盖 7 态（PENDING / PROGRAMMING / IN_PROCESS / OUTSOURCE / INSPECTION / READY_TO_SHIP / DELIVERED），而 `by_status` 可能含 `REPAIRING`（存量环境里 DB 仍有该字面，2026-10-01 起 `REPAIRING` 已降级为 `t_part_batch.is_repairing` boolean 列、新数据不再产生该 status）。该状态既不进柱也不进 tooltip total ⇒ **KPI 数字可能大于图上总和**。

产品决议（2026-10-07）：**不处理**。前端若要消除这个偏差，可在 `by_status` 里显式排除 `REPAIRING`。

### 8.4 交期面板三桶的已知偏差登记（2026-10-10）

| 偏差 | 现象 | 处置 |
|---|---|---|
| `partial` 与 `overdue_count` 的时间窗口重叠 | 一条「已交一部分 + 交期已过」的工单**不在** `overdue_count` 里（被 `NOT EXISTS` 排除），但**在** `partial` 桶里。用户若把「逾期数」与「已交一部分的逾期工单数」相加去核对总逾期，会对不上 | **产品决议，不处理**：需求原文即「部分已交不应该限制时间范围，应该扫出全部的部分已交工单」。前端不得拿 `partial` 的交期分布去推断逾期 |
| `partial` 含 `system_delivery_date IS NULL` 的工单 | 「全部的部分已交工单」包含**没填系统交期**的工单（排序 `NULLS LAST`） | **产品决议，不处理**：既然不限时间范围，就不该把「没填交期」排除掉。前端需容忍该列 `null` 并给占位展示 |
| `upcoming.total` 可能远超 `items.length()` | 每桶上限 30，`total` 是匹配总数 | 设计如此（前端按「共 N 条」展示）。**唯一例外是 0 命中**：`COUNT(*) OVER ()` 在无行时无从求值，此时 `total` 为 `0` 而非 `null` |
| 无子件装配件的 `delivered_quantity` 恒为 0 | 该装配件无论业务上是否已交付，都会被判成「一件没交过」⇒ 落 `upcoming` / `overdue` 桶而非 `partial` | 与逾期 KPI 的行为一致（`NOT EXISTS` 子件路径同样恒真），**刻意保持**。无子件装配件本身是数据问题，不是口径问题 |
| 装配件行落在 `partial` 桶但 `delivered_quantity` 显示 0 套 | 只在**装配件整套交付不变式被破坏**时出现（典型形态：子件 A 交满、子件 B 一件没交）。桶归属看 `EXISTS`（命中），展示值看 min 公式（0 套） | 推导见 §4.4。不变式的唯一破坏路径是 `POST /prod/batches/{id}/deliver`，送货单路径有闸门；存量违规数据人工清理。**代码侧不处理** |