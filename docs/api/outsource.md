# outsource 域 API（外协公司 / 报价 / 发货记录 + 外协看板）

> 本文件是 `outsource` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 2026-10-09 本域三合一写端点的来源域（`prod::batch` 的外协三端点）契约见 [`batch.md`](batch.md) §3；
> 看板前端接线要与 `prod::queue` 看板协同，对照 [`queue.md`](queue.md)。

## 0. 2026-10-09 变更摘要

本文件描述的域本轮一次做完三件事（全部**硬切无 alias**）：

1. **看板读端点收敛**：`/outsource-pool/{counts,state,{process_id}}` 三条旧读 → `/outsource-queue/{snapshot,processes/{id}}` 两条新读（内联 `held_batches` 消灭 N+1）。
2. **可发送一览下线**：`GET /outsource-sendable` 的行是看板候选列的**分页子集**，端点删除。
3. **写端点三合一**：`prod::batch` 的 `send-to-outsource` / `receive-from-outsource` / `receive-from-outsource-to-inspection` → 单条 `POST /outsource-queue/move`。

router 工厂 **5 → 4**（删 `sendable_router()` 与 `pool_router()`），端点 **23 → 22**（companies 8 + quotes 9 + shipments 2 + queue 3）。

## 1. 端点表

四个 router 工厂，一个前缀一个工厂（禁止合并成一个大 router，否则 matchit 的注册顺序约束会跨前缀纠缠 —— 见 `CLAUDE.md` 路由声明规约第 9 条）。全部返回统一信封 `R { code, message, data }`。

### 1.1 `/outsource-companies`（8 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-companies/` | Manager + Clerk + CncProgrammer + Inspector | `name_like?` / `is_active?` / `limit?`（缺省 50，clamp 1..500）/ `offset?` | `OutsourceCompanyListOut` |
| 2 | POST | `/api/v2/outsource-companies/` | Manager + Clerk | `{ name, contact_name?, contact_phone?, address?, is_active?=true, process_ids?: string[] }` | **201** `OutsourceCompanyWithProcessesOut` |
| 3 | GET | `/api/v2/outsource-companies/{id}` | Manager + Clerk + CncProgrammer + Inspector | path `id` | `OutsourceCompanyWithProcessesOut` |
| 4 | POST | `/api/v2/outsource-companies/{id}/update` | Manager + Clerk | `{ name?, contact_name?, contact_phone?, address?, is_active?, version }` | `OutsourceCompanyWithProcessesOut` |
| 5 | POST | `/api/v2/outsource-companies/{id}/soft-delete` | Manager + Clerk | path `id` | `R<()>`（`data: null`） |
| 6 | GET | `/api/v2/outsource-companies/by-process/{process_id}` | Manager + Clerk + CncProgrammer + Inspector | path `process_id` | `OutsourceCompanyOut[]`（**不分页**） |
| 7 | POST | `/api/v2/outsource-companies/{id}/processes` | Manager + Clerk | `{ process_ids: string[] }` | `OutsourceCompanyWithProcessesOut` |
| 8 | GET | `/api/v2/outsource-companies/{id}/sent-parts` | Manager + Clerk | `keyword?` / `sent_from?` / `sent_to?` / `received_from?` / `received_to?` / `sort_by?` / `sort_dir?` / `limit?`（缺省 50，clamp 1..200）/ `offset?` | `OutsourceSentPartListOut` |

### 1.2 `/outsource-quotes`（9 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-quotes/` | Manager + Clerk + Inspector | `status?` / `part_id?` / `outsource_company_id?` / `customer_id?` / `keyword?` / `sort_by?`（缺省 `CREATED_AT`）/ `sort_dir?`（缺省 `DESC`）/ `limit?`（缺省 50，clamp 1..500）/ `offset?` | `OutsourceQuoteListOut` |
| 2 | POST | `/api/v2/outsource-quotes/` | Manager + Clerk | `{ part_id: string, outsource_company_id: string, process_id: string, price: string, note? }` | **201** `OutsourceQuoteOut`（恒为 `DRAFT`） |
| 3 | GET | `/api/v2/outsource-quotes/quotable-parts` | Manager + Clerk | `keyword?` / `limit?` / `offset?` | `QuotablePartListOut` |
| 4 | GET | `/api/v2/outsource-quotes/{id}` | Manager + Clerk + Inspector | path `id` | `OutsourceQuoteOut` |
| 5 | POST | `/api/v2/outsource-quotes/{id}/update` | Manager + Clerk | `{ price?, note?, version }` | `OutsourceQuoteOut` |
| 6 | POST | `/api/v2/outsource-quotes/{id}/submit` | Manager + Clerk | path `id` | `OutsourceQuoteOut` |
| 7 | POST | `/api/v2/outsource-quotes/{id}/approve` | **Manager 独占** | `{ review_note?, version }` | `OutsourceQuoteOut` |
| 8 | POST | `/api/v2/outsource-quotes/{id}/reject` | **Manager 独占** | `{ review_note: string, version }` | `OutsourceQuoteOut` |
| 9 | POST | `/api/v2/outsource-quotes/{id}/soft-delete` | Manager + Clerk | path `id` | `R<()>` |

### 1.3 `/outsource-shipments`（2 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-shipments/in-flight` | Manager + Clerk + Inspector | `keyword?` / `limit?`（缺省 50，clamp 1..200）/ `offset?` | `OutsourceInFlightListOut` |
| 2 | POST | `/api/v2/outsource-shipments/{id}/reconcile-update` | Manager + Clerk | `{ unit_price?, quantity?, is_billed?, version }` | `OutsourceShipmentOut` |

### 1.4 `/outsource-queue`（3 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-queue/snapshot` | Manager + Clerk + Inspector | **无** | `OutsourceQueueSnapshot` |
| 2 | GET | `/api/v2/outsource-queue/processes/{process_id}` | Manager + Clerk + Inspector | path `process_id`（雪花 ID 字符串） | `OutsourceQueueProcessDetail` |
| 3 | POST | `/api/v2/outsource-queue/move` | Manager + Clerk + Inspector | `{ batch_id: string, version: number, from, to, quote_id?, direct?, note? }` | `OutsourceMoveResult` |

- 端点 4.1 / 4.2 是**纯读**（`pool.acquire()` 不开事务、不发 WS 广播）；其余写端点开事务，**广播在 commit 之后**（仅端点 4.3 广播）。
- `{id}` / `{process_id}` 抽不出数字时走 axum 的 `PathRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**（全仓 `Path<i64>` 端点的统一行为，非本域特例）。
- `process_id` 不存在或已软删（端点 4.2）→ `20801 BIZ_PROCESS_NOT_FOUND`（**HTTP 404**）。
- i64 雪花主键一律序列化为 JSON **string**；Decimal（`price` / `unit_price` / `total_price`）一律**字符串**。
- 端点 4.3 的 `version` **无 `#[serde(default)]`**：缺失 → HTTP **422 纯文本**（axum `Json` 提取器），不是业务信封。
- 端点 1.1-#1 的 `is_active` 与 `limit` 之外的 query 参数被忽略（不报错）。

### 1.5 路由注册顺序（硬约束，见 handler 源码注释）

`company_router()` 的 `/by-process/{process_id}` 与 `quote_router()` 的 `/quotable-parts` **必须注册在同前缀的 `/{id}` catch-all 之前**：它们与 `/{id}` 同段位，matchit 按注册序匹配，被 `Path<i64>` 兜住会返 **400**（不是 404）。`quote_router()` 的 `quotable-parts` 就是这个坑的实际受害者（此前恒 400，前端 picker 恒空）。

`queue_router()` 的三条 route 段数不同（`/snapshot` 与 `/move` 1 段、`/processes/{process_id}` 2 段）⇒ **无同段位争用**，注册顺序不影响匹配。被取代的旧 `pool_router()` 三条全是 1 段，那里静态段必须先注册。

## 2. 逐字段

### 2.1 `OutsourceCompanyOut`（端点 1.1-#1 / #6 元素、1.1-#3 的基础形状）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` | string | `t_outsource_company.id`（`serialize_i64`） |
| `name` / `contact_name` / `contact_phone` / `address` | string \| null | 同名列 |
| `is_active` | boolean | 同名列 |
| `version` | number | 同名列（**OCC 锚**，update 必传） |
| `created_at` / `updated_at` | string | 同名列（naive timestamp，无时区后缀） |

`OutsourceCompanyWithProcessesOut` = 上表全部字段 + `processes: OutsourceCompanyProcessLinkOut[]`。

`OutsourceCompanyProcessLinkOut`：`process_id`（string）/ `process_code` / `process_name` / `category`（`t_process`，取自 `t_outsource_company_process` JOIN）。工序链接为**整组替换**（端点 1.1-#7），`process_ids` 传空数组即清空。

### 2.2 `OutsourceSentPartOut`（端点 1.1-#8 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `shipment_id` / `quote_id` / `part_id` / `process_id` | string | `t_outsource_shipment` 同名列 |
| `version` | number | `s.version`（**shipment 行 OCC**，reconcile-update 必传） |
| `part_drawing_no` / `part_name` / `is_urgent` | string \| null / boolean | `LEFT JOIN t_part p` |
| `customer_path` | string \| null | `service::join_customer_path`（L2 `c.name` + L1 `cp.name`，有 L1 拼 `L1 / L2`） |
| `batch_no` | number \| null | `LEFT JOIN t_part_batch pb`（shipment 未绑批次的历史行为 null） |
| `process_name` | string \| null | `LEFT JOIN t_process pr` |
| `quantity` | number | `s.quantity`（**发出时的全量**，不是批次当前余量） |
| `unit_price` / `total_price` | string | `s.unit_price::text` / `unit_price × quantity`（Decimal 字符串） |
| `sent_at` / `received_at` | string | `s.sent_at` / `s.received_at`（后者可空） |
| `status` | string | `OUTSOURCING` / `RECEIVED` |
| `is_billed` | boolean | 同名列 |

行粒度 = **一行一个 shipment**（`list_for_company` 的 WHERE 带 `status IN ('OUTSOURCING','RECEIVED')`）。**刻意不复用 `OutsourceShipmentOut`**：后者主键字段叫 `id`，本 VO 叫 `shipment_id`（前端行编辑端点入参按此名取）。

### 2.3 `OutsourceQuoteOut`（端点 1.2 全部返回）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` / `part_id` / `outsource_company_id` / `process_id` | string | `t_outsource_quote` 同名列 |
| `version` | number | 同名列（**OCC 锚**，update / approve / reject 必传） |
| `price` | string | `price::text`（Decimal 字符串；DIRECT 占位报价为 `"0.00"`） |
| `note` / `review_note` | string \| null | 同名列 |
| `status` | string | 同名列，5 态：`DRAFT` / `SUBMITTED` / `APPROVED` / `REJECTED` / `USED` |
| `submitted_at` / `reviewed_at` | string \| null | 同名列 |
| `created_at` / `updated_at` | string | 同名列 |
| `part_serial_no` / `part_drawing_no` / `part_name` / `is_urgent` | string \| null / boolean | `t_part`（service 补全） |
| `outsource_company_name` / `process_code` / `process_name` | string \| null | `t_outsource_company` / `t_process`（service 补全） |
| `customer_path` | string \| null | service 拼 L1 / L2 |
| `part_unit_price` | string \| null | `t_part.unit_price::text`（供与报价对比） |

`is_direct` **不在出参里**（内部列，谓词用途见 §4.2）。

### 2.4 `QuotablePartOut`（端点 1.2-#3 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` | string | `t_part.id` |
| `serial_no` / `drawing_no` / `name` | string \| null / string | `t_part` 同名列 |
| `is_urgent` | boolean | `t_part` |
| `unit_price` | string | `t_part.unit_price::text`（Decimal 字符串，**下单单价**供与报价对比） |
| `customer_id` | string | `t_part.customer_id` |
| `customer_name` / `l1_customer_name` / `customer_path` | string \| null | `t_customer` L2 / L1 / service 拼 |

行粒度 = **一行一个零件**（该零件存在 `PENDING` 批次）。刻意不复用 `PartListItem`：后者刻意不声明任何派生工序字段，本 VO 要带单价 / 加急 / 客户路径供 picker 直接渲染。

### 2.5 `OutsourceInFlightItem`（端点 1.3-#1 元素）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `part_id` / `batch_id` | string | `t_part.id` / `t_part_batch.id` |
| `batch_no` / `quantity` | number | `pb.batch_no` / **`pb.quantity`（当前剩余待收量，不是 shipment.quantity**） |
| `serial_no` / `drawing_no` / `name` | string \| null | `t_part` |
| `is_urgent` | boolean | `t_part` |
| `customer_path` | string \| null | service 拼 L1 / L2 |
| `next_process_id` | string \| null | `s.process_id`（外协加工的工序，`serialize_i64_opt`） |
| `next_process_name` | string \| null | `LEFT JOIN t_process pr` |
| `outsource_company_id` / `outsource_company_name` | string / string \| null | `s.outsource_company_id` / `LEFT JOIN t_outsource_company` |
| `sent_at` | string | `s.sent_at` |
| `version` | number | **`pb.version`（不是 `s.version`）** |

⚠️ 与看板右列 `held_batches` 的**重叠度很高但不是同一端点**：`in-flight` 是按 shipment 的**全局在途列表**（不分工序，paged），`held_batches` 是**单工序板内联**（不分页，见 §2.7）。前端对账页的「在途」tab 用前者。

### 2.6 `OutsourceQueueSnapshot`（端点 4.1）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `processes[]` | array | SQL 1 + 2 + 3（见下） |
| `sendable_total` | number | SQL 1 求和（服务层累加） |
| `in_flight_total` | number | SQL 2 求和（服务层累加） |
| `ts` | string | `infra::clock::now_shanghai_iso()`（RFC 3339 `+08:00`，小数秒位数自适应） |

`processes[]` 元素（`OutsourceQueueProcess`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `process_id` | string | SQL 1/2 的 `current_process_id`（装配处 `.to_string()`） |
| `process_code` / `process_name` | string | SQL 3 `t_process.code` / `.name` |
| `color` | string \| null | `t_process.color`（`#RRGGBBAA`，历史行为 null；**不做格式收窄**，前端直接喂 CSS `border-left-color`） |
| `category` | string | `t_process.category`（DB CHECK `INHOUSE` / `OUTSOURCE`） |
| `sendable_count` | number | SQL 1 `COUNT(*)::bigint` |
| `in_flight_count` | number | SQL 2 `COUNT(*)::bigint` |

**只返 `sendable + in_flight > 0` 的工序**（`GROUP BY` 不产 0 行组）。工序元数据查不到（已软删）的行以 `process_code = ""` + `process_name = "(deleted#{id})"` 占位返回，`category` 兜底 `"OUTSOURCE"`，**计数仍显示**。

### 2.7 `OutsourceQueueProcessDetail`（端点 4.2）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `process` | object | SQL 1 `OutsourceQueueProcessMeta`（`process_id` / `process_code` / `process_name` / `color`，**无 `category`**） |
| `companies[]` | array | SQL 2 + SQL 3（见下） |
| `items[]` | array | SQL 4（见下） |
| `total` | number | **`items.len()`**（服务层从 Vec 算，不分页，与 `items` 恒等） |
| `ts` | string | `infra::clock::now_shanghai_iso()` |

`companies[]` 元素（`OutsourceQueueCompany`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `company_id` / `name` | string | SQL 2 `t_outsource_company.id` / `.name`（经 `t_outsource_company_process` JOIN） |
| `held_count` | number | **服务层内存分组行数**（`== held_batches.len()`，见 §4.1） |
| `held_batches[]` | array | SQL 3（**无公司谓词**，见下） |

`items[]` 元素（`OutsourceQueueCandidate`，25 字段）：取自 `SENDABLE_PROJECTION_FULL` 的全投影，字段与后端 SQL 列一一对应 —— `version`（`pb.version`，**OCC 锚**）、`send_mode`、`batch_id` / `part_id`（string）、`batch_no`、`quantity`（`pb.quantity`，行 = 批次故与旧 VO 的 `batch_quantity` 同值）、`part_serial_no` / `part_drawing_no` / `part_name`、`planned_delivery_date` / `system_delivery_date`（`to_char(…,'YYYY-MM-DD')`，**字符串不是日期对象**）、`is_urgent`、`customer_name` / `parent_customer_name`、`applicant_name`、`note`、`shelf_code`（string \| null）、`shelf_id`（string，**承重字段**，见 §8.4）、`outsource_company_id` / `outsource_company_name`、`quote_id`、`company_options: OutsourceCompanyOption[]`（`id` string + `name`）、`price`（Decimal 字符串）、`can_send`（**服务层算** `send_mode == "APPROVAL" || !company_options.is_empty()`）、`has_cnc_program`（`EXISTS (t_part_file kind='G_CODE')`）。

`held_batches[]` 元素（`OutsourceQueueHeldBatch`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `batch_id` / `part_id` | string | SQL 3 `pb.id` / `pb.current_holder_id AS company_id`（分组键）/ `pb.part_id` |
| `batch_no` / `quantity` | number | `pb.batch_no` / **`pb.quantity`（当前余量，前端「部分接收」输入框的 max）** |
| `serial_no` / `drawing_no` / `name` | string \| null / string | `t_part` |
| `system_delivery_date` / `planned_delivery_date` | string \| null | `t_part`（**native date，非 `to_char` 字符串** —— 与候选卡不同） |
| `is_urgent` | boolean | `t_part` |
| `customer_name` / `parent_customer_name` / `applicant_name` | string \| null | `t_customer` L2 / L1 / `LEFT JOIN LATERAL t_applicant` |
| `location` | string | `pb.location`，恒为 `"OUTSOURCE_COMPANY"`（在途谓词保证） |
| `note` | string \| null | `t_part.note` |
| `version` | number | `pb.version`（**移动写端点的 OCC 锚**） |
| `has_cnc_program` | boolean | `EXISTS (t_part_file kind='G_CODE')` |
| `sent_at` | string \| null | `LEFT JOIN t_outsource_shipment.sent_at`（`status='OUTSOURCING'`） |
| `price` | string \| null | `s.unit_price::text`（Decimal 字符串，**刻意不用空串兜底**） |
| `receive_next_process_id` | string | `COALESCE(nx.next_process_id, 0)` ⇒ **推不出时是字符串 `"0"`**（0 兜底口径，非 nullable） |
| `receive_next_process_name` | string \| null | `nx.next_process_name` |
| `chain_resolvable` | boolean | **`receive_next_process_id != "0"`**（服务层算） |

`chain_resolvable` 的业务含义：`true` ⇒ 工序链已知，前端可免填 `to.next_process_id`；`false` ⇒ 工序链缺失或指针漂移，前端必须让用户手填（否则写端点返 `20706 BIZ_PROCESS_CHAIN_REQUIRED`）。

### 2.8 `OutsourceMoveResult`（端点 4.3）

| 字段 | 类型 | 后端 SQL 来源 / 说明 |
|---|---|---|
| `batch_id` / `part_id` / `new_holder_id` | string | 批次 id / `batch.part_id` / `to` 的那个 id 字段（前端拿它当**下一次** move 的 `from` 侧 id） |
| `from_kind` / `to_kind` | string | 请求 `from.kind` / `to.kind` 字面（`PRODUCTION_SHELF` / `OUTSOURCE_COMPANY` / `INSPECTION_SHELF`） |
| `new_location` | string | **恒等于 `to_kind`**（写入口直接用 `to.kind` 作 `location` 值） |
| `version` | number | **写后读回 `t_part_batch.version` 的真实值**（不在 Rust 里算 `+1`，见 §8.4） |
| `shipment_id` | string，**仅发送方向存在** | 本次新建的 `t_outsource_shipment.id`；回收方向**键不存在**（不是 `null`） |
| `new_process_id` | string，**仅回收生产方向存在** | `t_part_batch.current_process_id`；回收到品检架时按出池不变式清 NULL ⇒ 键不存在 |

`shipment_id` / `new_process_id` 带 `skip_serializing_if = "Option::is_none"`：前端的判定是「这个方向有没有这个东西」（`"shipment_id" in payload` / 挂 shipment 卡片），`null` 与「这个方向不产生它」语义不同。两个字段的序列化回归由 `vo/queue.rs::tests` 的 `move_result_omits_direction_specific_keys_when_absent` 钉住。

⚠️ 出参**取代旧三端点的 `PartOut`**：`PartOut` 是 part 级 VO（一次返回整个工单的聚合视图），对批次级看板毫无用处（移动的是**一个批次**）。这条是破坏性变更。

## 3. 报价与对账（第二块能力）

外协域除看板外还有两块能力，本轮**未改动**，按现状记录。

### 3.1 报价生命周期（端点 1.2 的 8 条）

状态机（`statemachine.rs`，**纯内存迁移表，不写 DB**）：

```
DRAFT ──submit──▶ SUBMITTED ──approve──▶ APPROVED
                     │
                     └──reject──▶ REJECTED

REJECTED ──▶ （软删；或重新建一条 DRAFT）
```

- `USED` 是**占位态**：`from_str` 接受它以读存量行，但 service 当前**不自动迁**（approve 后发送时未写 USED 事件）。
- 历史兼容词汇（`OUTSOURCING` / `RECEIVED` / `BILLED`）同样只读不写。
- 每个写端点的状态前置闸门：`update` / `soft-delete` 要求 `DRAFT`（软删另允许 `REJECTED`）；`submit` 要求 `DRAFT`；`approve` / `reject` 要求 `SUBMITTED`。违反 → `21302 BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION`。
- `approve` 会把同 `(part_id, process_id)` 的 `SUBMITTED` / `APPROVED` 报价批量置 `REJECTED`（`reject_competitors`，被新批准报价取代）。
- `approve` / `reject` **Manager 独占**（service 第一行 `require_role(Role::Manager)`）。

### 3.2 对账与在途

- **对账页**（`GET /outsource-companies/{id}/sent-parts` + `POST /outsource-shipments/{id}/reconcile-update`）：行 = shipment；`reconcile-update` 三态回填（`unit_price` / `quantity` / `is_billed`，`None` = 不改）+ 必传 `version`，OCC 冲突 → `40901`。`quantity <= 0` → `20104`。
- **在途**（`GET /outsource-shipments/in-flight`）：行 = shipment，`status='OUTSOURCING'`。
- `shipment` 状态：`OUTSOURCING`（已发出）→ `RECEIVED`（已收回）。移动写端点在两个回收方向上自动把开口 shipment 标 `RECEIVED` 并写 `received_at`。
- DB 上有 partial unique `uq_t_outsource_shipment_open_batch`：一个批次最多一张开口 shipment。

## 4. 口径表

### 4.1 候选侧三处的行粒度一致性（`sendable_count` / `items` / 旧 `sendable` 端点）

三个消费方共用**同一个谓词常量** `repo/sql.rs::SENDABLE_INNER_X_SQL` + 同一个 `x → d` 收敛层（`DISTINCT ON (batch_id, current_process_id)`）：

| 消费方 | SQL 落点 | 行粒度 | 收敛 |
|---|---|---|---|
| `snapshot.processes[].sendable_count` | `board/repo.rs::SQL_SENDABLE_COUNT_BY_PROCESS`（用 `*_PROJECTION_COUNT` 精简投影） | 一批次一行 | 有 |
| `detail.items[]` | `board/repo.rs::SQL_CANDIDATES_BY_PROCESS`（用 `*_PROJECTION_FULL`） | 一批次一行 | 有 |
| 旧 `GET /outsource-sendable` | `repo/sql.rs::OutsourceSendableRepo::list_by_process` | 一批次一行 | 有 |

⇒ **`snapshot.processes[].sendable_count == detail.items.len()` 恒成立**（看板侧集成测试 `tests/outsource/pool.rs::detail_items_count_matches_snapshot_sendable_count` 钉住）。看板只换**投影**（`sendable_dedup_sql` 的两个投影形参 + 外层列清单），**JOIN 与 WHERE 一行都不重写** —— 改谓词只改一个常量。

`GET /outsource-sendable` 已于 2026-10-09 硬切下线。它的行是 `items[]` 的**分页子集**（看板不分页），分页 + 关键字 + 客户过滤那套入参（`OutsourceSendableListQuery`）与 VO 一并删除。候选侧谓词 SQL 与三个共用纯函数（`service/sendable.rs` 的 `send_mode_of` / `can_send_of` / `decode_company_options`）**保留** —— 看板候选列仍消费它们。

### 4.2 `held_count == held_batches.len()` 不变量

| 端点 | 消费方 | 保证方式 |
|---|---|---|
| `detail.companies[].held_count` | 前端列头徽标 | **服务层内存分组行数**（`board/service.rs::to_companies`），SQL 侧**不做 `COUNT`** |
| `detail.companies[].held_batches[].version` | 移动写端点 OCC 锚 | `pb.version`（行级真源） |

要求恒等的理由：前端按 `held_count` 渲染列头徽标、按 `held_batches` 渲染卡片，两者不一致时表现为「徽标写 3、列里只有 2 张卡」，运营无法判断是漏件还是显示 bug，而这类不一致是**静默**的。

批次数由 SQL 3（在途批次一次取齐）后在内存 `HashMap` 分组得到，**不依赖 SQL 的 `COUNT`**（那条 SQL 与明细 SQL 的谓词一旦漂移就会分叉）。守卫测试 `board::held_count_guard_tests::held_count_matches_held_batches_len` 直接对生产路径上的纯函数断言恒等式（含交错输入，顺带证明分组不依赖 SQL 的 ORDER BY）。

### 4.3 snapshot 与 process detail 的关系

| | `snapshot`（4.1） | `detail`（4.2） |
|---|---|---|
| 只含有货工序 | ✅ `sendable + in_flight > 0` | ❌ 工序不存在即 404（不判有无货） |
| 行粒度 | 一工序一行（计数） | 一公司一列 + 一批次一张卡 |
| 分页 | 无 | 无 |
| SQL 条数 | **3**（候选分组 / 在途分组 / 工序元数据 `ANY`） | **4**（工序元数据 / 公司白名单 / 在途一次取齐 / 候选卡） |

⚠️ **`snapshot.processes[]` 只含有货工序 ⇒ tab 集合必须由前端 join 全量 OUTSOURCE 工序列表**，否则「该工序的货被发完 / 收完」的那一瞬间 tab 会从界面上消失，正在操作的用户被踢出当前页。工序的全量列表来自 `prod::process` 域（`category='OUTSOURCE'`）。

`snapshot` 的 SQL 条数恒定 3、`detail` 恒定 4，由 `cargo test --lib` 的 `outsource::board::sql_count_guard_tests::{no_sqlx_query_inside_loop_body, detail_queries_are_pinned}` 钉死（源码级护栏：循环体内禁 `sqlx::query` + 调用点数钉死）。改这两条聚合 SQL 必须同步 `board/repo.rs` 的方法 doc、`board/mod.rs` 的条数陈述与那两个常量。

### 4.4 审批闸门 `t_process.requires_approval`

`t_process.requires_approval` 是候选侧谓词的一部分（`SENDABLE_INNER_X_SQL` 的 WHERE 末段）：

- `false`（免审批直发）⇒ 直接出行，`send_mode = "DIRECT"`，`quote_id` / `price` / `outsource_company_id` / `outsource_company_name` **恒为 `null`**（SQL 的 `AND pr.requires_approval` 把报价 LEFT JOIN 短路掉），`company_options` 列出该工序映射的全部**活跃**公司。
- `true`（需审批）⇒ 必须已有该 `(part_id, process_id)` 的**真实审批报价**（`status='APPROVED' AND is_direct=false`）否则**不出行**；出行时 `send_mode = "APPROVAL"`，`company_options` 恒为 `[]`。

`is_direct = false` 这一维是「被人审批过的报价」的定义：DIRECT 直发会**自动建** `status='APPROVED' AND is_direct=true AND price=0` 的占位报价，只判 `status='APPROVED'` 会把那种组合命中，用户以为按审批价发货实际用 0 元占位价。

**读侧与写侧同时守**（这是闭环的前提）：读侧候选列的 `can_send` 决定「看不看得见」，写端点 `resolve_send_quote` 的守卫 6 决定「发不发得成」——`requires_approval=true` 的工序传 `direct=true` → `20104`。

### 4.5 排序白名单

| 端点 | `sort_by` 白名单 | 非法值回落 | `sort_dir` | 非法值回落 |
|---|---|---|---|---|
| `GET /outsource-quotes/` | `PRICE` / `REVIEWED_AT` / `CREATED_AT` | `CREATED_AT` | `ASC` / `DESC` | `DESC` |
| `GET /outsource-companies/{id}/sent-parts` | `PRICE` / `SENT_AT` / `RECEIVED_AT` | `SENT_AT` | `ASC` / `DESC` | `DESC` |

两者都以**归一化后的白名单 token 走 bind** + CASE 表达式选列 —— 用户输入永远不进 SQL 文本。

### 4.6 `customer_id` 的客户子树展开（报价列表）

`GET /outsource-quotes/` 的 `customer_id` 在 service 层展开成 part_id 集合（自身 ∪ **直接**子客户），与 `keyword` 展开出的集合**取交集**。

展开只下潜**一层** —— 依据是生产库实测（零件全挂 L2、L3 数量 0），而该结构 API 层不强制（`create_customer` 不校验 `parent_id` 是否指向根客户）。出现 L3 后本字段需改成递归 CTE，且漏报**是静默的**（`total` 偏小、不报错）。

## 5. 状态域约定（无编译期保障）

### 5.1 move 的守卫白名单（逐条，顺序本身是契约）

守卫顺序：**角色 → 同 kind 40001 → 批次存在 → OCC → 状态机 → from 锚点 → to 侧分方向**。

| # | 守卫 | 错误码 |
|---|---|---|
| 1 | 角色 Manager + Clerk + Inspector（service 入口第一行） | `40301` |
| 2 | `from.kind == to.kind` → 拒绝（**在查批次之前判**，同 kind 是请求形状错误） | `40001 VALIDATION_ERROR` |
| 3 | 批次存在（软删视为不存在） | `20109 BIZ_PART_BATCH_NOT_FOUND` |
| 4 | OCC：`validate_batch_version(batch.id, req.version, batch.version)` | `40901 VERSION_CONFLICT` |
| 5 | 状态机 `ensure_transition`；两个回收方向**额外**显式要求源为 `OUTSOURCE` | `20103 BIZ_INVALID_TRANSITION` |
| 6 | `from` 必须等于批次真实 `(location, current_holder_id)` | `20122 BIZ_BATCH_LOCATION_MISMATCH` |
| 6b | 源为 `IN_PROCESS` 时 location 必须是 `PRODUCTION_SHELF`（在 6 之前判） | `20103` |
| 6c | `from.kind = INSPECTION_SHELF` 一律拒（品检架上的批次不走外协看板） | `20122` |
| 6d | `quote_id` / `direct` 出现在非发送方向 | `20104 BIZ_INVALID_VALUE` |
| 7 | 发送方向：公司存在 → 启用 → 工序存在 → 工序类别 `OUTSOURCE` → 公司映射该工序 → `direct` / `quote_id` 恰给一个 → `requires_approval` 工序不许 `direct` | `21201` / `21205` / `20801` / `20104` / `20104` / `20104` / `20104` |
| 7b | 报价存在 → 状态 `APPROVED` → `(part, company, process)` 三元组一致 → APPROVAL 路径拒 DIRECT 占位价 | `21301` / `21307` / `21302` / `21307` |
| 8 | 回收生产：目标货架存在 / 启用 / `zone='PRODUCTION'` / 映射该工序；下一道工序推导 | `20501` / `20512` / `20104` / `20507` / `20706` |
| 9 | 回收品检：目标货架存在 / 启用 / `zone='INSPECTION'` | `20501` / `20512` / `20104` |

`ensure_transition` 依赖 `PartStatus::can_transition_to`（`part::statemachine` 的内存迁移表）。发送方向用到的两条边是 `PENDING → OUTSOURCE` 与 `IN_PROCESS → OUTSOURCE`；⚠️ `IN_PROCESS → OUTSOURCE` 这条边**曾经缺失**，导致「可发送一览的行（几乎全是 `IN_PROCESS` 源）发一单就被 `20103` 拒」，端到端实测下外协发送 100% 不可用。状态机补边后守卫 6b 才真正承担 location 不变式 —— 别因为「守卫 6 已经能拒」就把它删掉。

### 5.2 出池不变式（回收品检方向）

`OUTSOURCE → INSPECTION` 是**出池**：`t_part_batch.current_process_id` 与 `current_process_step_id` 两列**同时清 NULL**。所以回收到品检架时 `new_process_id` 恒缺席（`OutsourceMoveResult` 的键不存在），且该批次从此不再出现在任何候选池查询里（候选谓词要求 `current_process_id` 指向一道 `OUTSOURCE` 工序）。

### 5.3 `t_part_batch.status` 的写入口

**唯一写入口是 `shared::batch::status::apply_batch_status_change`**，由 `cargo test --lib` 的 `shared::batch::status::write_guard_tests::no_outside_file_writes_batch_status` 扫全 `src/**/*.rs` 强制。本域的 move 端点经 `mark_batch_with_status_and_meta` 薄包装调用，**不自写任何 `UPDATE t_part_batch SET status …`**。

### 5.4 报价状态机

`DRAFT / SUBMITTED / APPROVED / REJECTED / USED` 五态与 `statemachine.rs::can_transition_to` 迁移表**无编译期约束**（字符串列 + Rust enum 校验），改一侧必须改另一侧。

### 5.5 `company_options` 的 SQL ↔ Rust 双重解码

`company_options` 是 SQL 侧 `to_jsonb(array_agg(json_build_object(...)))` 的结果（**单个 JSONB 值，不是 `json[]`** —— 后者 sqlx 解不进 `serde_json::Value`）。`decode_company_options` 解不出来时**降级为空数组**而不是让整个端点 500（少一个下拉选项远好过整页不可用）。APPROVAL 行在 SQL 侧 `CASE WHEN q.id IS NOT NULL` 短路成 `'[]'::jsonb`。

## 6. 移除记录（不得省）

**全部硬切、旧路径 404、无 alias。**

| 被移除项 | 原因与新路径 |
|---|---|
| `GET /api/v2/outsource-pool/counts` | → `GET /api/v2/outsource-queue/snapshot`（工序元数据 + `sendable_total` / `in_flight_total`） |
| `GET /api/v2/outsource-pool/{process_id}` | → `GET /api/v2/outsource-queue/processes/{process_id}`（右列从「只给 `held_count`」变成「内联全部在途批次卡片」） |
| `GET /api/v2/outsource-pool/state?company_id=&process_id=` | 被上一条的 `companies[].held_batches` 取代（**不再需要逐公司请求**） |
| `GET /api/v2/outsource-sendable` | 被 `GET /outsource-queue/processes/{id}` 的候选列取代（它是同一批行的分页子集）。连带删除 `sendable_router()` 工厂与 `OutsourceSendableListQuery` |
| `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` | 三合一为 `POST /api/v2/outsource-queue/move`（`from=PRODUCTION_SHELF` + `to=OUTSOURCE_COMPANY`） |
| `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource` | 同端点（`OUTSOURCE_COMPANY` → `PRODUCTION_SHELF`） |
| `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` | 同端点（`OUTSOURCE_COMPANY` → `INSPECTION_SHELF`） |
| `GET /api/v2/parts/outsource-in-flight` | 2026-10-03 硬切 → `GET /outsource-shipments/in-flight`（旧端点返回错形状的通用 `PartListItem`；旧 URL 实际返 **400** 而非 404 —— part 域 `/{part_id}` catch-all 兜住未注册的 1 段静态路径后由 `Path` extractor 拒绝） |
| `GET /api/v2/parts/outsource-sendable` | 同上，2026-10-03 硬切（`OUTSOURCING` 端点同款错形状） |
| WS 事件名 `PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED` | 合并为 `OUTSOURCE_MOVE_DONE`（见 §7） |
| `repo/sql.rs` 的 `OutsourcePoolRepo`（4 方法） | 三条端点下线后全部无调用方，SQL 按「去公司谓词」的口径搬进 `board/repo.rs` |
| `vo/pool.rs` / `service/pool.rs` | 出参合并进 `vo/queue.rs`；service 实现被 `board/service.rs` 取代 |
| `OutsourceRepoTrait` 的 5 个方法（`pool_*` 4 + `sendable_list_by_process` 1） | 同上，无调用方 |
| `vo/shipment.rs` 的 `ApprovedForSendItem` / `ApprovedForSendListOut` | 死 VO，零调用方（表达不了 DIRECT 模式） |
| `GET /api/v2/prod/batches/{batch_id}/split` | 2026-10-09 提升为共用顶层端点 `POST /api/v2/batches/split`（`batch_id` 入 body），见 [`batch.md`](batch.md) §2.1 |

### 6.1 因 move 端点收窄而**部分下线**的能力

| 能力 | 状态 | 替代路径 |
|---|---|---|
| 部分发送（`quantity` < 批次全量） | **下线**（三合一端点恒整批） | 先拆批：`POST /api/v2/batches/split`，再发子批次 |
| 部分回收 | 同上 | 同上 |
| 从未上架的 `PENDING` 批次直接发外协 | **下线**（`from.kind` 只有 `PRODUCTION_SHELF`，而未上架批次的 `current_holder_id IS NULL`） | 先上架：`POST /api/v2/prod/batches/{batch_id}/place-on-shelf`，再发 |
| 调用方自选外协工序（`process_id` 入参） | **删除** | 后端自推 = 批次当前所属工序（`current_process_id`） |

收窄的理由：旧三端点的 `quantity` 会让「一个 move 写两行批次」的记账与 shipment 的开口 / 关闭口径分叉（shipment 记的是**发出时的全量**，部分回收只拆批次、源批次继续持有开口 shipment）—— 那是三合一要消灭的分歧。

## 7. 与 WS 的关系

- **outsource 域不订阅 WS。** 本域的读端点都是 HTTP `pool.acquire()` 拉取，前端靠 TanStack Query 的 staleTime + 写端点成功后的失效编排来刷新。
- **但本域唯一的写端点会发广播**（`WsEvent::DashboardEvent { kind, payload }`，**commit 之后**）：

| 写端点 | `kind` | payload |
|---|---|---|
| `POST /outsource-queue/move` | `OUTSOURCE_MOVE_DONE` | **整个 `OutsourceMoveResult` 的序列化**（前端按 `from_kind` / `to_kind` 自行推断方向） |

其余写端点（公司 / 报价 / shipment 的 CRUD 与状态流转）**不发任何 WS 广播**。

### 7.1 ⚠️ 传输层事件名 与 `t_part_event.event_type` 审计字面量是两件事

| 层 | 值 | 性质 |
|---|---|---|
| WS 事件名（传输层） | `OUTSOURCE_MOVE_DONE` | 一次移动在网络上完成，前端按它做卡片归位 |
| `t_part_event.event_type`（业务事实） | `SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` / `RECEIVED_TO_INSPECTION` | **记录发生了什么业务事实**（哪个方向、去了哪），前端在工单时间线上按它分组 |

三个审计字面量**逐字不变**，与 WS 事件名的合并无关。`t_part_event.event_type` 是 `varchar(30)`，字面量超 30 字符会让 PG 返 22001 并把**整个事务**回滚（三个字面量分别 19 / 26 / 22 字符，均在限内）。

## 8. 表依赖与前端配套

### 8.1 读的表

| 用途 | 表 |
|---|---|
| 批次（候选 / 在途 / 状态 / OCC） | `t_part_batch` |
| 工单展示字段 | `t_part` |
| 工序元数据与 `requires_approval` | `t_process` |
| 外协公司与工序能力映射 | `t_outsource_company` / `t_outsource_company_process` |
| 报价 | `t_outsource_quote` |
| 发货记录（开口 / 已收 / 单价） | `t_outsource_shipment` |
| 客户两级名 | `t_customer`（L2 + `parent_id` L1） |
| 申请人 | `t_applicant`（`LEFT JOIN LATERAL`） |
| 货架（候选卡的 `shelf_code` / 回收目标） | `t_shelf` |
| 工序链（下一道工序推导） | `t_process_chain_step` / `t_part.process_chain_id` |
| CNC 程序存在性 | `t_part_file`（`EXISTS` 子查询） |
| 事件日志 | `t_part_event`（写） |
| 派生列回填 | `t_part` / `t_assembly`（经 `shared::batch::status`） |

### 8.2 跨域依赖登记

本域**整体不适用**域隔离护栏：它继承既定的「经本域 trait 转发其它域单表查询」pattern（`OutsourceRepoTrait` 转发 `t_part` / `t_part_batch` / `t_process`），并调用 `shared::batch::guards`（批次状态写入口的薄包装）。

### 8.3 前端配套改动清单（外协看板接线）

1. **URL 全量替换**：`/api/v2/outsource-pool/*` → `/api/v2/outsource-queue/{snapshot,processes/{id}}`；`/api/v2/outsource-sendable` **删除**（改用 `processes/{id}` 的 `items[]`）；`/api/v2/prod/batches/{id}/{send-to-outsource,receive-from-outsource,receive-from-outsource-to-inspection}` → `/api/v2/outsource-queue/move`。**无 alias**，旧路径 404。
2. **N+1 消除**：原「进程序列板 1 次 + 每公司 1 次 state」的组合应合并为**一次** `processes/{id}` 请求。
3. **`move` 入参形态变更**（**破坏性**）：
   - `batch_id` 从 path 参数移到 body，且**必须是 JSON 字符串**（`"1590000000000000001"`）。`shared::types::deserialize_i64` 只接受字符串，发 JSON number → **HTTP 422 纯文本**（响应里没有 `code` 字段，勿按 `40001` 分支解析）。
   - `version` **必填**，值取卡片上的 `items[].version` 或 `held_batches[].version`。
   - `outsource_company_id` / `shelf_id` 移进 `to` / `from` 对象（`{ kind, company_id | shelf_id }`）。
   - `process_id` **删除**；`next_process_id` 改为 `to.next_process_id` 且**可省略**（后端按工序链推导，`chain_resolvable = true` 时可省）。
   - `quantity` **删除**（整批语义）。
   - DIRECT 传 `direct: true` 且 `quote_id: null`；APPROVAL 传 `quote_id` 且 `direct: null`（两者互斥，恰给一个）。
4. **`move` 出参变更**：`PartOut`（part 级）→ `OutsourceMoveResult`（批次级）。读 part_id 改读 `out.part_id`；OCC 版本号改读 `out.version`（**写后读回的真实值**，不是请求的 `version + 1`）。`shipment_id` / `new_process_id` 按「键是否存在」判定方向，不要按 `null` 判定。
5. **候选卡 `shelf_id` 是承重字段**：拖拽发送时必须原样回传给 `from.shelf_id`（候选池跨货架，不能用「用户当前激活货架」凑 —— 激活货架对 MANAGER / CLERK / INSPECTOR 恒为空）。填错被写端点按 `20122` 拒收。
6. **候选卡的 `shelf_id` 为空串的行走不通**：那是 `PENDING` 且未上架的批次（本来就在生产架之外），要先 `place-on-shelf`。
7. **新增 2 个看板 composable** + **删 3 个旧 composable**（`/counts` 计数、`/state` 每公司一次、`/pool/{id}` 详情）。
8. **zod schema 同步**：新增 `outsourceQueueSnapshotSchema` / `outsourceQueueProcessDetailSchema` / `outsourceQueueCandidateSchema` / `outsourceQueueCompanySchema` / `outsourceQueueHeldBatchSchema` / `outsourceMoveResultSchema`。**注意 zod 默认 strip 模式**会让漏声明的字段静默丢失，数组元素必须全字段声明（候选卡 25 字段）。
9. **日期字段类型不一致**（勿写同一个 schema 复用）：候选卡 / `quotable` 的日期是 `YYYY-MM-DD` **字符串**（`to_char`）；`held_batches` 的是 ISO 日期串（native date）。
10. **`receive_next_process_id` 是字符串 `"0"`** 而非数字 0、亦非 `null`；配合 `chain_resolvable` 判定要不要弹手填对话框。
11. **`snapshot.processes[]` 的 tab 集合必须 join 全量 OUTSOURCE 工序列表**（见 §4.3）。

### 8.4 已知偏差登记（不得省）

- **`companies[].held_count` 与 `held_batches.len()` 的一致性由服务层保证（集成测试 + lib 单测 `held_count_matches_held_batches_len` 锁），但若未来有人在 SQL 侧重新加 `COUNT` 会静默分叉。** SQL 侧已刻意不做 `COUNT`（`SQL_COMPANIES_BY_PROCESS` 的 doc 逐字写了这一点）—— 恢复 `COUNT` 的诱惑来自「顺手」，代价是两条 SQL 的谓词一旦漂移就静默不一致。
- **候选卡 `shelf_id` 对「`PENDING` 且未上架」的批次序列化为空串（不是 `null`）。** 这类行本来就在生产架之外，拖拽发送会被 `from` 守卫以 `20122` 拒收。选空串而非 `null` 是因为 `null` 会让前端的必填字符串校验炸在**整页渲染**上。
- **`snapshot.processes[]` 只含 `sendable + in_flight > 0` 的工序 ⇒ tab 集合必须由前端 join 全量 OUTSOURCE 工序列表，否则操作到一半 tab 会消失。** 后端不返「零货工序」是刻意的（序列板的语义是「现在有活要干的工序」），但这意味着 tab 集合不是后端给的单一真源。
- **`OutsourceMoveResult.version` 是写后读回的真实值，不是在 Rust 里算的 `batch.version + 1`。** 写入口的 OCC 守卫与源状态白名单都可能让 UPDATE 命中 0 行，让「算出来的 +1」与真实值分叉；而分叉的症状是「刚拖完就冲突」，极难定位。代价是多一次读（同一事务内）。
- **`OutsourceMoveResult.new_location` 与 `to_kind` 恒等**（冗余字段）。让「归位键」是显式字段而不是「推导得出」的东西，理由是 WS payload 会被缓存重放（前端刷新后先补事件再拉列表）。
- **`GET /outsource-quotes/` 的 `customer_id` 只下潜一层客户子树**，出现 L3 后漏报是**静默的**（`total` 偏小、不报错）。
- **`snapshot` 的工序元数据查不到（已软删）时 `category` 兜底为 `"OUTSOURCE"`**，而候选侧不可能命中软删工序（它 INNER JOIN 了 `t_process`）—— 只有在途侧会。兜底而非返 `null` 是因为前端要按 `category` 分组渲染。
- **`detail.process` 无 `category` 字段**（单工序详情不展示类别，与 `prod::queue` 的 `QueueProcessMeta` 对齐），而 `snapshot.processes[]` 有。
- **§1.5 的「静态段必须先于 catch-all 注册」是硬约束，没有编译期保障。** 加一条新的 1 段静态路由而放到 `/{id}` 之后 ⇒ 该路径返 **400**（不是 404），症状与「路由没注册」不同，极易误判。

## 9. 错误码分段（`src/shared/error.rs::code`）

本域用到的业务码（`2xxxx` 段，外协自有 `21xxx`）：

| 码 | 符号 | 触发 |
|---|---|---|
| `20101` | `BIZ_PART_NOT_FOUND` | 批次关联的 part 不存在 / 已软删 |
| `20103` | `BIZ_INVALID_TRANSITION` | move 状态机守卫 |
| `20104` | `BIZ_INVALID_VALUE` | 分方向业务规则（工序类别 / 公司映射 / direct 互斥 / 货架 zone / `quantity <= 0` / `review_note` 空） |
| `20109` | `BIZ_PART_BATCH_NOT_FOUND` | 批次不存在 / 已软删 |
| `20122` | `BIZ_BATCH_LOCATION_MISMATCH` | `from` 与批次真实位置不符 |
| `20501` / `20507` / `20512` | `BIZ_SHELF_NOT_FOUND` / `BIZ_SHELF_PROCESS_NOT_MAPPED` / `BIZ_SHELF_INACTIVE` | 回收目标货架三条守卫 |
| `20706` | `BIZ_PROCESS_CHAIN_REQUIRED` | 下一道工序推不出（需手填 `to.next_process_id`） |
| `20801` | `BIZ_PROCESS_NOT_FOUND` | 工序不存在 / 已软删（HTTP 404） |
| `21201` | `BIZ_OUTSOURCE_COMPANY_NOT_FOUND` | 公司不存在 / 已软删（HTTP 404） |
| `21202` / `21214` | `BIZ_OUTSOURCE_COMPANY_DUPLICATE` / `..._DUPLICATE_NAME` | 公司重名（应用层预检 / DB 唯一索引兜底） |
| `21203` / `21204` | `BIZ_OUTSOURCE_COMPANY_BAD_PROCESS` / `BIZ_OUTSOURCE_PROCESS_NOT_MAPPED` | 工序类别不对 / 公司未映射该工序 |
| `21205` | `BIZ_OUTSOURCE_COMPANY_IN_USE` | 公司已停用（发送方向）或仍被引用（软删） |
| `21301` | `BIZ_OUTSOURCE_QUOTE_NOT_FOUND` | 报价不存在 / 已软删（HTTP 404） |
| `21302` | `BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION` | 报价状态机守卫 / 三元组与本次 move 不一致 |
| `21303` | `BIZ_OUTSOURCE_QUOTE_DUPLICATE` | 同 `(part, company, process)` 已存在活跃报价 |
| `21307` | `BIZ_OUTSOURCE_QUOTE_NOT_APPROVED` | 报价状态非 `APPROVED` / 是 DIRECT 占位价 / 并发回查失败 |
| `21501` | `BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND` | shipment 不存在 / 已软删（HTTP 404） |
| `21502` | `BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION` | shipment 状态机守卫 |
| `21503` / `21504` | `BIZ_OUTSOURCE_SHIPMENT_NO_OPEN` / `..._QUANTITY_EXCEEDS` | 找不到开口 shipment / 接收数量超开口量 |
| `40901` | `VERSION_CONFLICT` | OCC 冲突 |
| `40001` | `VALIDATION_ERROR` | 请求形状错误（同 kind 移动） |
| `40301` | — | 角色守卫 |

⚠️ `40901 VERSION_CONFLICT` 只表达 OCC，不得用于业务唯一键冲突（全仓规约见 `CLAUDE.md` §8）。本域的公司重名 / 报价三元组重复都有专用码（`21202`/`21214` / `21303`）。