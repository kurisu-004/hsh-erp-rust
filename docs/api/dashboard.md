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
| `overdue_count` | number | `repo/delivery.rs::count_overdue`：`t_part`（`assembly_id IS NULL`）+ `t_assembly` 的 `UNION ALL` 后 `COUNT(*)` |
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

| 字段 | 类型 | 说明 |
|---|---|---|
| `urgent[]` | array | `delivered_quantity == 0`（一件没交过），按 `system_delivery_date ASC` |
| `partial[]` | array | `delivered_quantity > 0`（已交过一部分），同上 |

每桶独立截断到 **30** 行（`DELIVERY_BUCKET_LIMIT`）；窗口 `[today, today + 7)`（`DELIVERY_WINDOW_DAYS`）。分桶判据 / 窗口 / 截断**全部在服务端**，前端不再自己过滤。

行字段（`SystemDeliveryOrder`）：`id` / `serial_no` / `name` / `quantity` / `status` / `system_delivery_date` / `customer_name` / `is_urgent` / `delivered_quantity`。

`delivered_quantity` 来自 `t_part_batch` 的 `SUM(quantity)`（`status IN ('DELIVERED','COMPLETED')`），一条批量聚合 SQL，**防 N+1**：客户名同理（`t_customer` 一条 `id = ANY($1)` 批量查）。

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
| 逾期计数（端点 1 `overdue_count`） | `status = ANY(DELIVERY_STATUSES)` |
| 最紧急 + 部分已交面板（端点 1 `system_delivery_orders`） | `status = ANY(DELIVERY_STATUSES)` |
| 柱状图下钻抽屉（端点 3） | `status = ANY(DELIVERY_STATUSES)`（允许前端按层传子集） |
| 柱状图分桶（端点 2）`by_status` | top(4) + middle(2) 正是这 6 态；**bottom 层额外含 `DELIVERED`** |

### 4.1 行单位差异（跨端对数前必读）

| 用途 | 行单位 | 装配件处理 |
|---|---|---|
| 逾期计数 | **工单级** | 算 1 条（查 `t_assembly`；`t_part` 侧用 `assembly_id IS NULL` 排除子件） |
| 面板 / 柱状图 | **件级** | 不出现（`t_part` 全表含子件，子件各算 1 件） |

两者**不冲突**：逾期窗口是 `< today`，面板 / 图的窗口是 `>= today`，**时间窗口不重叠**，同一条工单不会同时出现在两处。

### 4.2 `t_assembly` 侧只命中 4 态

逾期查询的 `t_assembly` 侧复用同一份 `DELIVERY_STATUSES`，但 `AssemblyStatus` 只有 7 态（**无 `PROGRAMMING` / `OUTSOURCE`），故天然只命中 `PENDING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP`。这是有意的：白名单按 part 状态域取全集，不需要第二份常量。

### 4.3 与 `statistics` 域的有意分叉

`statistics::repo::sql::count_overdue_undelivered` 服务生产统计页（前端 `OverviewTab.vue`），走 `planned_delivery_date` 口径且带 `NOT EXISTS (… DELIVERED 事件)` 兜底，**不要动它**。dashboard 的 `count_overdue` 走 `system_delivery_date` 口径、**刻意不加事件兜底**（派生状态滞后窗口只影响一次刷新）。

## 5. 状态域约定（无编译期保障）

- 后端常量：`repo/delivery.rs::DELIVERY_STATUSES`
- 前端对应：柱状图 `UpcomingDeliveryChart.vue` 的 `LAYERS[].statuses`——`top`（PENDING /
  PROGRAMMING / IN_PROCESS / OUTSOURCE）+ `middle`（INSPECTION / READY_TO_SHIP）合起来
  正是本常量的 6 态，`bottom`（DELIVERED）额外多一个。
  2026-10-07 那两块交期面板的 urgent / partial 判定改为服务端按 `delivered_quantity`
  判定后，6 态在前端**只剩 `LAYERS[].statuses` 这一个镜像**（见 §2.2）。

两者是**人工同步**关系：Rust 常量与 TS 字面量之间没有任何编译期约束，漂了不会编译失败，
只会让「逾期数」与「柱状图层数」互相矛盾（且现象是数字对不上、极难定位）。改任一侧必须同步另一侧；
集成测试 `overdue_accepts_all_six_delivery_statuses` 把后端常量的字面值钉死，也只会抓到这一种症状。

## 6. 移除记录（2026-10-07）

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

  `ASSEMBLY_CANCELLED` / `ASSEMBLY_CREATED` / `ASSEMBLY_DELETED` / `ASSEMBLY_UPDATED` / `BATCH_TO_INSPECTION` / `BATCH_TO_SHIP` / `PART_BATCH_WITH_PDFS_CREATED` / `PART_COMPLETED` / `PART_DELIVERED` / `PART_SOFT_DELETED` / `PART_TO_INSPECTION` / `PART_TO_PROCESS` / `PART_TO_SHIP` / `ROLLUP_RECOMPUTED` / `WORKER_POOL_AUTO_ALLOCATE_DONE` / `WORKER_POOL_EMPTY` / `WORKER_POOL_MOVE_DONE` / `WORKER_POOL_REFILL_DONE` / `DELIVERY_NOTE_CREATED` / `DELIVERY_NOTE_PARTS_ADDED` / `DELIVERY_NOTE_SUBMITTED` / `DELIVERY_NOTE_PICKED_UP` / `DELIVERY_NOTE_SCAN_ADD` / `DELIVERY_NOTE_BATCHES_ATTACHED`

  前端 `AFFECTS_DASHBOARD` 白名单与之人工对应（关系是**子集**：并非每个 kind 都影响大屏）。
- 心跳：text 帧 `{type:"heartbeat", ts:<unix 秒>}` + 协议层 `Message::Ping`（前端 JS 不可见，服务端判活用）。
- 慢消费方：广播队列（容量 1024）溢出 → 服务端发 `4003 lagged` Close 帧，前端重连 + 全量 HTTP 重取。

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
4. **窗口过滤、`delivered_quantity == 0` 判定、两桶 30 条截断全在服务端**（前端不再自己过滤）。
5. **逾期数来源变更**：原为调 `GET /statistics/overview` 取 1 个数字（后端跑 9 条 SQL、返 16 标量 + 2 数组 + 2 嵌套结构，且口径是 `planned_delivery_date`，与同页右栏面板的 `system_delivery_date` 互相矛盾）→ 现直接读 `snapshot.overdue_count`。
6. **「在制」标签改名「在加工」**：`in_process` 的 SQL 硬约束是 `location='WORKER'`，语义是「已从货架/品检区出池、压在工人手上」，叫「在制」是误导。

### 8.3 已知偏差登记

柱状图三层只覆盖 7 态（PENDING / PROGRAMMING / IN_PROCESS / OUTSOURCE / INSPECTION / READY_TO_SHIP / DELIVERED），而 `by_status` 可能含 `REPAIRING`（存量环境里 DB 仍有该字面，2026-10-01 起 `REPAIRING` 已降级为 `t_part_batch.is_repairing` boolean 列、新数据不再产生该 status）。该状态既不进柱也不进 tooltip total ⇒ **KPI 数字可能大于图上总和**。

产品决议（2026-10-07）：**不处理**。前端若要消除这个偏差，可在 `by_status` 里显式排除 `REPAIRING`。