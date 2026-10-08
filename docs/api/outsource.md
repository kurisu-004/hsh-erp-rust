# outsource 域 API（外协公司 / 报价 / 发货记录 + 外协看板）

> 本文件是 `outsource` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 2026-10-09 本域三合一写端点的来源域（`prod::batch` 的外协三端点）契约见 [`batch.md`](batch.md) §3；
> 看板前端接线要与 `prod::queue` 看板协同，对照 [`queue.md`](queue.md)。

## 0. 2026-10-09 变更摘要

本文件描述的域本轮分两批做完，全部**硬切无 alias**。

**第一批 —— 看板与写端点**（端点 23 → 22）：

1. **看板读端点收敛**：`/outsource-pool/{counts,state,{process_id}}` 三条旧读 → `/outsource-queue/{snapshot,processes/{id}}` 两条新读（内联 `held_batches` 消灭 N+1）。
2. **可发送一览下线**：`GET /outsource-sendable` 的行是看板候选列的**分页子集**，端点删除。
3. **写端点三合一**：`prod::batch` 的 `send-to-outsource` / `receive-from-outsource` / `receive-from-outsource-to-inspection` → 单条 `POST /outsource-queue/move`。

**第二批 —— 公司 / 报价两域收敛**（端点 22 → **19**）：

4. **`POST /outsource-companies/{id}/processes` 下线**，工序能力清单的整体替换吸收进 `POST /{id}/update` 的 `process_ids`（三态：`None` 不动 / `Some([])` 清空 / `Some([..])` 替换），同事务内与公司字段一起提交。
5. **`GET /outsource-quotes/{id}` 与 `POST /outsource-quotes/{id}/update` 下线**（前端零消费）。
6. **三条写端点补必填 body `version`**：公司 `soft-delete`、报价 `submit` 与 `soft-delete`。此前这三条都是 service 内部自读 version，等于用自己读到的值守自己的乐观锁（见 §4.7）。
7. **两个列表端点的 `keyword` 拆成 `drawing_no` / `name` 直连 ILIKE**，另加 `is_urgent`（报价）与 `customer_id` / `process_id` / `is_billed`（对账页）三个精确维度（见 §4.8）。
8. **报价一览的 `statuses` 多状态筛选接线**（本轮最重要的修复，见 §3.3）。
9. **VO 瘦身与端点收窄**：见 §2 的逐字段表与 §6 的移除记录。

router 工厂始终是 **4 个**（`company_router` / `quote_router` / `shipment_router` / `queue_router`）。

## 0b. 2026-10-10 变更摘要：移动端点只剩两个方向

1. **`OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向整条下线**：
   `OutsourceLocation::InspectionShelf` 变体随之删除（该变体只有 `shelf_id` 一个
   字段，而品检架上线自动选架后它没有任何角色可留）。发这个 kind 的请求现在在
   **反序列化阶段**被拒 → **HTTP 422 纯文本**（`unknown variant \`INSPECTION_SHELF\``）、
   不进 `R<T>` 信封、批次零改动。
2. **`to = PRODUCTION_SHELF` 删掉 `shelf_id`**：目标架由服务端按
   `current_load / capacity` 升序选（[`shelves.md`](shelves.md) §4），选不出 →
   `20508 BIZ_SHELF_PROCESS_NOT_FOUND`。
3. **`from = PRODUCTION_SHELF` 不再比对 `current_holder_id`**：只校验
   `batch.location == 'PRODUCTION_SHELF'`。「批次在某个生产架上」与「它是不是在生产架
   上」是两件事；保留比对会让「同一批货只是换了个架」被拒，而看板卡片本来就不承诺
   自己知道批次此刻在哪一格。`20122` 因此只剩 `from = OUTSOURCE_COMPANY` 且
   `company_id` 不符这一条可达路径。

## 1. 端点表

四个 router 工厂，一个前缀一个工厂（禁止合并成一个大 router，否则 matchit 的注册顺序约束会跨前缀纠缠 —— 见 `CLAUDE.md` 路由声明规约第 9 条）。全部返回统一信封 `R { code, message, data }`。

### 1.1 `/outsource-companies`（7 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-companies/` | Manager + Clerk + CncProgrammer + Inspector | `name_like?` / `is_active?` / `limit?`（缺省 50，clamp 1..500）/ `offset?` | `OutsourceCompanyListOut` |
| 2 | POST | `/api/v2/outsource-companies/` | Manager + Clerk | `{ name, contact_name?, contact_phone?, address?, is_active?=true, process_ids?: string[] }` | **201** `R<()>`（`data: null`） |
| 3 | GET | `/api/v2/outsource-companies/{id}` | Manager + Clerk + CncProgrammer + Inspector | path `id` | `OutsourceCompanyWithProcessesOut` |
| 4 | GET | `/api/v2/outsource-companies/{id}/sent-parts` | Manager + Clerk | `drawing_no?` / `name?` / `customer_id?` / `process_id?` / `is_billed?` / `sent_from?` / `sent_to?` / `received_from?` / `received_to?` / `sort_by?` / `sort_dir?` / `limit?`（缺省 50，clamp 1..200）/ `offset?` | `OutsourceSentPartListOut` |
| 5 | POST | `/api/v2/outsource-companies/{id}/update` | Manager + Clerk | `{ name?, contact_name?, contact_phone?, address?, is_active?, version, process_ids?: string[] }` | `OutsourceCompanyWithProcessesOut` |
| 6 | POST | `/api/v2/outsource-companies/{id}/soft-delete` | Manager + Clerk | path `id` + **`{ version }`** | `R<()>`（`data: null`） |
| 7 | GET | `/api/v2/outsource-companies/by-process/{process_id}` | Manager + Clerk + CncProgrammer + Inspector | path `process_id` | `OutsourceCompanyOptionOut[]`（**不分页**） |

- 端点 2 / 5 / 6 的 `version`（端点 2 无）**必填**，无 `#[serde(default)]`：缺省 → HTTP **422 纯文本**（axum `Json` 提取器），不是业务信封。
- 端点 5 的 `process_ids` 三态可分：缺省 / `null` = 不动、`[]` = 清空、`[..]` = 整体替换（保序）。**「清空」必须能与「不动」区分**，否则「取消全选并保存」会被静默丢弃。目标有序集合与当前有序集合完全相同时 service 跳过重写映射表（见 §4.9）。
- 端点 6 的守卫顺序是契约：**先 OCC（`40901`），再工序映射非空（`21205`）**（见 §4.7）。
- 端点 1 之外的 query 参数被忽略（不报错）。
- 端点 7 的出参是窄 VO（只有 `id` + `name`）：能出现在本列表里的公司恒为启用（service 已 `filter(is_active)`），故不返 `is_active`。

### 1.2 `/outsource-quotes`（7 条）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/outsource-quotes/` | Manager + Clerk + Inspector | `status?` / `statuses?`（**逗号分隔**）/ `part_id?` / `outsource_company_id?` / `customer_id?` / `drawing_no?` / `name?` / `is_urgent?` / `sort_by?`（缺省 `CREATED_AT`）/ `sort_dir?`（缺省 `DESC`）/ `limit?`（缺省 50，clamp 1..500）/ `offset?` | `OutsourceQuoteListOut` |
| 2 | POST | `/api/v2/outsource-quotes/` | Manager + Clerk | `{ part_id: string, outsource_company_id: string, process_id: string, price: string, note? }` | **201** `OutsourceQuoteOut`（恒为 `DRAFT`） |
| 3 | GET | `/api/v2/outsource-quotes/quotable-parts` | Manager + Clerk | `keyword?` / `limit?` / `offset?` | `QuotablePartListOut` |
| 4 | POST | `/api/v2/outsource-quotes/{id}/submit` | Manager + Clerk | **`{ version }`** | `OutsourceQuoteOut` |
| 5 | POST | `/api/v2/outsource-quotes/{id}/approve` | **Manager 独占** | `{ review_note?, version }` | `OutsourceQuoteOut` |
| 6 | POST | `/api/v2/outsource-quotes/{id}/reject` | **Manager 独占** | `{ review_note: string, version }` | `OutsourceQuoteOut` |
| 7 | POST | `/api/v2/outsource-quotes/{id}/soft-delete` | Manager + Clerk | **`{ version }`** | `R<()>` |

- 端点 4 / 5 / 6 / 7 的 `version` 全部必填。守卫顺序统一为：**状态机 → OCC**（所以对一条已 `SUBMITTED` 的报价再 `submit`，无论 `version` 对不对都是 `21302`）。
- `statuses` 的 wire format 是**逗号分隔单值**（`?statuses=DRAFT,SUBMITTED`），**不是**重复 key —— 理由见 §3.3。
- ⚠️ 本 router **没有 1 段动态路由**了（`GET /{id}` 已下线），故 §1.5 的注册顺序约束在本前缀不再生效。

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
| 3 | POST | `/api/v2/outsource-queue/move` | Manager + Clerk + Inspector | `{ batch_id: string, version: number, from, to, quote_id?, direct?, note? }`；`from` / `to` 的 `kind` ∈ `{PRODUCTION_SHELF, OUTSOURCE_COMPANY}`（**`INSPECTION_SHELF` 已于 2026-10-10 删除**） | `OutsourceMoveResult` |

- 端点 4.1 / 4.2 是**纯读**（`pool.acquire()` 不开事务、不发 WS 广播）；其余写端点开事务，**广播在 commit 之后**（仅端点 4.3 广播）。
- `{id}` / `{process_id}` 抽不出数字时走 axum 的 `PathRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**（全仓 `Path<i64>` 端点的统一行为，非本域特例）。
- `process_id` 不存在或已软删（端点 4.2）→ `20801 BIZ_PROCESS_NOT_FOUND`（**HTTP 404**）。
- i64 雪花主键一律序列化为 JSON **string**；Decimal（`price` / `unit_price` / `total_price`）一律**字符串**。
- 端点 4.3 的 `version` **无 `#[serde(default)]`**：缺失 → HTTP **422 纯文本**（axum `Json` 提取器），不是业务信封。

### 1.5 路由注册顺序（硬约束，见 handler 源码注释）

`company_router()` 的 `/by-process/{process_id}` **必须注册在同前缀的 `/{id}` catch-all 之前**：两者同段位，matchit 按注册序匹配，被 `Path<i64>` 兜住会返 **400**（不是 404）。

`quote_router()` 原先的 `quotable-parts` 也是同一个坑的实际受害者（此前恒 400、前端 picker 恒空），但 `GET /{id}` 已在 2026-10-09 下线 ⇒ 该前缀已无 1 段动态路由，注册顺序不再有硬约束。这条约束留给「将来谁想加回一条 1 段动态路由」的人。

`queue_router()` 的三条 route 段数不同（`/snapshot` 与 `/move` 1 段、`/processes/{process_id}` 2 段）⇒ **无同段位争用**，注册顺序不影响匹配。`shipment_router()` 同理（`/in-flight` 1 段、`/{id}/reconcile-update` 2 段）。

## 2. 逐字段

### 2.1 公司域出参（端点 1.1-#1 / #3 / #5 / #7）

`OutsourceCompanyOut`（端点 1.1-#1 的元素；端点 #3/#5 是它的超集 + `processes`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` | string | `t_outsource_company.id`（`serialize_i64`） |
| `name` / `contact_name` / `contact_phone` / `address` | string \| null | 同名列 |
| `is_active` | boolean | 同名列 |
| `version` | number | 同名列（**OCC 锚**，端点 1.1-#5 / #6 必传） |

⚠️ **无 `created_at` / `updated_at`**（2026-10-09 删）：公司一览是对账 / 报价 / 看板三处的公司下拉数据源，前端只渲染「名称 + 联系人 + 启停用」，两列时间戳无任何消费方，而每次写端点都会让它们变化 ⇒ 纯粹的缓存抖动。

`OutsourceCompanyWithProcessesOut`（端点 1.1-#3 / #5）= 上表全部字段 + `processes: OutsourceCompanyProcessLinkOut[]`。

`OutsourceCompanyProcessLinkOut`（**3 字段**）：`process_id`（string）/ `process_code` / `process_name`。
⚠️ **无 `category` / `sort_order`**（2026-10-09 删）：`category` 的消费方是前端勾选框，而候选集来自独立的 `GET /proc/processes?category=OUTSOURCE`，本字段与那份候选集恒等；`sort_order` 只被写侧 `replace_processes` 赋值、被看板公司列的 `ORDER BY MIN(cp.sort_order)` 读（`board/repo.rs::SQL_COMPANIES_BY_PROCESS`），**两条都不经过本 VO** —— 映射的展示顺序由请求数组顺序决定。`processes[]` 的顺序 = `t_outsource_company_process` 的 `sort_order ASC, id ASC`。

`OutsourceCompanyOptionOut`（端点 1.1-#7 的元素，**2 字段**）：`id`（string）/ `name`。
⚠️ **无 `is_active`**：service 层已在 Rust 里 `filter(|c| c.is_active)` 掉了停用公司，能出现在本列表里的行恒为启用 —— 再返一列 `is_active` 等于把「已被后端消掉的事实」交给前端重新判断。

### 2.2 `OutsourceSentPartListOut` / `OutsourceSentPartOut`（端点 1.1-#4）

信封（`data`）：

| 字段 | 类型 | 来源 |
|---|---|---|
| `outsource_company_id` | string | 请求 path `id` 回显（`serialize_i64`） |
| `outsource_company_name` | string \| null | `t_outsource_company.name`（仅未软删）。**公司不存在 / 已软删时为 `null`** —— 端点本身**不**因公司缺失而 404，所以标题位需要能显示「未知公司」 |
| `items` | array | 见下 |
| `total` / `limit` / `offset` | number | `limit` 缺省 50、clamp 1..200 |

2026-10-09 加 `outsource_company_id` / `outsource_company_name` 的理由：前端对账页原先要**额外发一次** `GET /outsource-companies/{id}` 才能拿到公司名渲染页头。

`OutsourceSentPartOut`（**16 字段**，行粒度 = 一行一个 shipment，WHERE 带 `status IN ('OUTSOURCING','RECEIVED')`）：

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `shipment_id` | string | `t_outsource_shipment.id`（**主键字段名是 `shipment_id`，不是 `id`** —— 前端行编辑端点入参按此名取） |
| `version` | number | `s.version`（**shipment 行 OCC**，reconcile-update 必传） |
| `part_drawing_no` / `part_name` / `is_urgent` | string \| null / boolean | `LEFT JOIN t_part p` |
| `customer_path` | string \| null | `service::join_customer_path`（L2 `c.name` + L1 `cp.name`，有 L1 拼 `L1 / L2`） |
| `batch_no` | number \| null | `LEFT JOIN t_part_batch pb`（shipment 未绑批次的历史行为 null） |
| `process_id` / `process_name` | string / string \| null | `s.process_id` / `LEFT JOIN t_process pr` |
| `quantity` | number | `s.quantity`（**发出时的全量**，不是批次当前余量） |
| `unit_price` / `total_price` | string | `s.unit_price::text` / `unit_price × quantity`（Decimal 字符串） |
| `sent_at` / `received_at` | string | `s.sent_at` / `s.received_at`（后者可空） |
| `status` | string | `OUTSOURCING` / `RECEIVED` |
| `is_billed` | boolean | 同名列 |

⚠️ **无 `quote_id` / `part_id`**（2026-10-09 删）：前端对账页两列都不存在（行编辑端点的入参按 `shipment_id` 取，零件列展示的是 `part_drawing_no` / `part_name` 两个可读字段），后端留着的后果是同一份零件标识序列化两次且分叉。**刻意不复用 `OutsourceShipmentOut`**：后者主键字段叫 `id`。

### 2.3 `OutsourceQuoteOut`（端点 1.2 的写端点返回）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` / `part_id` / `outsource_company_id` / `process_id` | string | `t_outsource_quote` 同名列 |
| `version` | number | 同名列（**OCC 锚**，submit / approve / reject / soft-delete 必传） |
| `price` | string | `price::text`（Decimal 字符串；DIRECT 占位报价为 `"0.00"`） |
| `note` / `review_note` | string \| null | 同名列 |
| `status` | string | 同名列，5 态：`DRAFT` / `SUBMITTED` / `APPROVED` / `REJECTED` / `USED` |
| `submitted_at` / `reviewed_at` | string \| null | 同名列 |
| `created_at` / `updated_at` | string | 同名列 |
| `part_serial_no` / `part_drawing_no` / `part_name` / `is_urgent` | string \| null / boolean | `t_part`（service 补全） |
| `outsource_company_name` / `process_code` / `process_name` | string \| null | `t_outsource_company` / `t_process`（service 补全） |
| `customer_path` | string \| null | service 拼 L1 / L2 |
| `part_unit_price` | string \| null | `t_part.unit_price::text`（供与报价对比） |

`is_direct` **不在出参里**（内部列，谓词用途见 §4.4）。

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

`items[]` 元素（`OutsourceQueueCandidate`，26 字段）：取自 `SENDABLE_PROJECTION_FULL` 的全投影，字段与后端 SQL 列一一对应 —— `version`（`pb.version`，**OCC 锚**）、`send_mode`、`batch_id` / `part_id`（string）、`batch_no`、`quantity`（`pb.quantity`，行 = 批次故与旧 VO 的 `batch_quantity` 同值）、`part_serial_no` / `part_drawing_no` / `part_name`、`planned_delivery_date` / `system_delivery_date`（`to_char(…,'YYYY-MM-DD')`，**字符串不是日期对象**）、`is_urgent`、`customer_name` / `parent_customer_name`、`applicant_name`、`note`、`shelf_code`（string \| null）、`shelf_id`（string，**2026-10-10 起只是展示字段**、写端点不再消费，见 §8.4 与 §0b）、`outsource_company_id` / `outsource_company_name`、`quote_id`、`company_options: OutsourceCompanyOption[]`（`id` string + `name`）、`price`（Decimal 字符串）、`can_send`（**服务层算** `send_mode == "APPROVAL" || !company_options.is_empty()`）、`has_cnc_program`（`EXISTS (t_part_file kind='G_CODE')`）、`has_process_chain`（判据见 §8.4）。

⚠️ `has_process_chain` **只加在候选卡上，同屏的在途卡 `OutsourceQueueHeldBatch` 没有这一列** —— 外协收发阶段的批次在厂外，不存在「按工序链顺推到下一道」的语义，故在途卡恒按无链渲染（不画绿框）。这是**有意的不对称**，见 §8.4。

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
| ~~`has_process_chain`~~ | — | **刻意没有这一列**（候选卡有、在途卡无，见上）。写 zod 时**不要**声明它 |
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
| `from_kind` / `to_kind` | string | 请求 `from.kind` / `to.kind` 字面（`PRODUCTION_SHELF` / `OUTSOURCE_COMPANY` 两值） |
| `new_location` | string | **恒等于 `to_kind`**（写入口直接用 `to.kind` 作 `location` 值） |
| `version` | number | **写后读回 `t_part_batch.version` 的真实值**（不在 Rust 里算 `+1`，见 §8.4） |
| `shipment_id` | string，**仅发送方向存在** | 本次新建的 `t_outsource_shipment.id`；回收方向**键不存在**（不是 `null`） |
| `new_process_id` | string，**仅回收生产方向存在** | `t_part_batch.current_process_id`；发送方向不产生它 ⇒ 键不存在 |

`shipment_id` / `new_process_id` 带 `skip_serializing_if = "Option::is_none"`：前端的判定是「这个方向有没有这个东西」（`"shipment_id" in payload` / 挂 shipment 卡片），`null` 与「这个方向不产生它」语义不同。两个字段的序列化回归由 `vo/queue.rs::tests` 的 `move_result_omits_direction_specific_keys_when_absent` 钉住。

⚠️ 出参**取代旧三端点的 `PartOut`**：`PartOut` 是 part 级 VO（一次返回整个工单的聚合视图），对批次级看板毫无用处（移动的是**一个批次**）。这条是破坏性变更。

## 3. 报价与对账（第二块能力）

外协域除看板外还有两块能力。2026-10-09 对这两块做了端点收敛与入参拆分，**状态机本身未改动**。

### 3.1 报价生命周期（端点 1.2 的 7 条）

状态机（`statemachine.rs`，**纯内存迁移表，不写 DB**）：

```
DRAFT ──submit──▶ SUBMITTED ──approve──▶ APPROVED
                     │
                     └──reject──▶ REJECTED

REJECTED ──▶ （软删；或重新建一条 DRAFT）
```

- `USED` 是**占位态**：`from_str` 接受它以读存量行，但 service 当前**不自动迁**（approve 后发送时未写 USED 事件）。
- 历史兼容词汇（`OUTSOURCING` / `RECEIVED` / `BILLED`）同样只读不写。
- 每个写端点的状态前置闸门：`soft-delete` 要求 `DRAFT`（另允许 `REJECTED`）；`submit` 要求 `DRAFT`；`approve` / `reject` 要求 `SUBMITTED`。违反 → `21302 BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION`。
- **守卫顺序统一为「状态机 → OCC」**：对一条已 `SUBMITTED` 的报价再 `submit`，无论 `version` 对不对都是 `21302`，不是 `40901`。
- `approve` 会把同 `(part_id, process_id)` 的 `SUBMITTED` / `APPROVED` 报价批量置 `REJECTED`（`reject_competitors`，被新批准报价取代）。
- `approve` / `reject` **Manager 独占**（service 第一行 `require_role(Role::Manager)`）。
- **`update` 端点已下线**（2026-10-09）：DRAFT 报价的价格 / 备注改法只剩「软删后重建一条 DRAFT」。前端本就零消费该端点。

### 3.2 对账与在途

- **对账页**（`GET /outsource-companies/{id}/sent-parts` + `POST /outsource-shipments/{id}/reconcile-update`）：行 = shipment；`reconcile-update` 三态回填（`unit_price` / `quantity` / `is_billed`，`None` = 不改）+ 必传 `version`，OCC 冲突 → `40901`。`quantity <= 0` → `20104`。
- **对账页的页头信息内联在 sent-parts 信封里**（`outsource_company_id` / `outsource_company_name`），前端不必再单独 `GET /outsource-companies/{id}`。
- **在途**（`GET /outsource-shipments/in-flight`）：行 = shipment，`status='OUTSOURCING'`。该端点的 `keyword` **未**拆成 `drawing_no` / `name`（它是单一 `keyword_pat` 绑进一个 `($1::text IS NULL OR p.drawing_no ILIKE $1 OR p.name ILIKE $1)` 谓词，本来就直连 ILIKE，无中间查询与截断风险）。
- `shipment` 状态：`OUTSOURCING`（已发出）→ `RECEIVED`（已收回）。移动写端点在两个回收方向上自动把开口 shipment 标 `RECEIVED` 并写 `received_at`。
- DB 上有 partial unique `uq_t_outsource_shipment_open_batch`：一个批次最多一张开口 shipment。

### 3.3 🔴 `statuses` 多状态筛选（本轮最重要的修复）

**症状**：报价一览的状态筛选**恒不生效**，且没有任何报错。

**完整故事**：

1. `repo/sql.rs` 的 `quote_list_with_filters` / `quote_count_with_filters` **早就支持**多状态 —— 形参 `statuses: &[String]`，SQL 里有 `AND (cardinality($2::text[]) = 0 OR status = ANY($2))`（空数组 = 不过滤）。
2. 但 `dto.rs` 的 `OutsourceQuoteListQuery` **没有这个字段**，而 `service/quote.rs` 里两处都硬编码传 `&[]`。SQL 与 repo 完好，DTO 与 service 断在中间。
3. 前端一直发状态筛选参数；axum 的 `Query` 走 `serde_urlencoded`，它**忽略未知 query 参数且不报错** ⇒ 参数被静默丢弃 ⇒ 筛选恒不生效。
4. 前端的角色默认筛选（`defaultStatusesForRole`：MANAGER → `['SUBMITTED']`、CLERK → `['DRAFT']`）本来就走这个参数，所以**两条路径同时失效**：MANAGER 打开报价一览看到的是**全量报价而非待审核报价**，而表头因 `statusFilterActive` 判定为「有筛选」变蓝加粗，**视觉上在说筛选已生效** —— 这是最坏的一种失效形态。

**修复**：只补 DTO 字段 + service 接线，SQL 与 repo 零改动。

**wire format 是逗号分隔单值，不是重复 key** —— 这是本轮实测得出的硬约束，值得单独记：

| 写法 | `Option<Vec<String>>` 的结果 |
|---|---|
| `?statuses=DRAFT&statuses=SUBMITTED` | **400**，纯文本 `invalid type: string "DRAFT", expected a sequence` |
| `?statuses[]=DRAFT&statuses[]=SUBMITTED` | 静默 `None`（键名带 `[]` 不匹配字段名） |
| `?statuses=DRAFT,SUBMITTED` | **400**（同上，`serde_urlencoded` 不按逗号拆序列） |
| `?statuses=DRAFT` + `Option<String>` + service `split(',')` | ✅ 正确表单值 |

即：axum 的 `Query`（`serde_urlencoded`）的 `Part` 反序列化器**不支持序列**，所以「多值 query 参数」在本仓一律写成逗号分隔的 `Option<String>`，由 service 展开（对照 `prod::part::dto_crud::PartListQuery.statuses` 与 `delivery_note` 的列表入参）。DTO 侧字段名保持 `statuses`，所以前端的 `paramsSerializer` 只要把它列进「CSV 单值」白名单即可（它**已经**在了）。

⚠️ 重复 key 形态在 `Option<String>` 上是**取最后一个**（query 被解析成 `HashMap<key, String>`，后写覆盖先写），**不是 OR**。

## 4. 口径表

### 4.1 候选侧两处的行粒度一致性（`sendable_count` / `items`）

两个消费方共用**同一个谓词常量** `repo/sql.rs::SENDABLE_INNER_X_SQL` + 同一个 `x → d` 收敛层（`DISTINCT ON (batch_id, current_process_id)`）：

| 消费方 | SQL 落点 | 行粒度 | 收敛 |
|---|---|---|---|
| `snapshot.processes[].sendable_count` | `board/repo.rs::SQL_SENDABLE_COUNT_BY_PROCESS`（用 `*_PROJECTION_COUNT` 精简投影） | 一批次一行 | 有 |
| `detail.items[]` | `board/repo.rs::SQL_CANDIDATES_BY_PROCESS`（用 `*_PROJECTION_FULL`） | 一批次一行 | 有 |

⇒ **`snapshot.processes[].sendable_count == detail.items.len()` 恒成立**（看板侧集成测试 `tests/outsource/pool.rs::detail_items_count_matches_snapshot_sendable_count` 钉住）。看板只换**投影**（`sendable_dedup_sql` 的两个投影形参 + 外层列清单），**JOIN 与 WHERE 一行都不重写** —— 改谓词只改一个常量。

`GET /outsource-sendable` 已于 2026-10-09 硬切下线。它的行是 `items[]` 的**分页子集**（看板不分页），分页 + 关键字 + 客户过滤那套入参（`OutsourceSendableListQuery`）与 VO 一并删除。候选侧谓词 SQL 与三个共用纯函数（`service/sendable.rs` 的 `send_mode_of` / `can_send_of` / `decode_company_options`）**保留** —— 看板候选列仍消费它们。

### 4.2 `held_count == held_batches.len()` 不变量

| 端点 | 消费方 | 保证方式 |
|---|---|---|
| `detail.companies[].held_count` | 前端列头徽标 | **服务层内存分组行数**（`board/service.rs::to_companies`），SQL 侧**不做 `COUNT`** |
| `detail.companies[].held_batches[].version` | 移动写端点 OCC 锚 | `pb.version`（行级真源） |

要求恒等的理由：前端按 `held_count` 渲染列头徽标、按 `held_batches` 渲染卡片，两者不一致时表现为「徽标写 3、列里只有 2 张卡」，运营无法判断是漏件还是显示 bug，而这类不一致是**静默**的。

批次数由 SQL 3（在途批次一次取齐）后在内存 `HashMap` 分组得到，**不依赖 SQL 的 `COUNT`**（那条 SQL 与明细 SQL 的谓词一旦漂移就会分叉）。守卫测试 `board::held_count_guard_tests::held_count_matches_held_batches_len` 直接对生产路径上的纯函数断言恒等式（含交错输入，顺带证明分组不依赖 SQL 的 ORDER BY）。

**在途侧两条 SQL 共用同一个 `current_holder_id IS NOT NULL` 谓词**：`snapshot` 的在途分组计数（`COUNT(*)`）与 `detail` 的在途明细。缺了它两边会分叉 —— `COUNT(*)` 会数到一批 `detail` 取不出来的行（tab 徽标虚高），而 `detail` 侧若也缺，`current_holder_id`（`bigint` 可空，无 DB 约束）会以 SQL NULL 落进 `HeldBatchRow::company_id`（`i64`）⇒ sqlx `error decoding column` ⇒ **整个 `process_detail` 返 500**。服务层的 `HashMap` 分组**不含谓词**（只按 `company_id` 建键），不是第三个落点。异常行（`location='OUTSOURCE_COMPANY'` 却没 holder）本仓判为数据异常，登记在下面的偏差表里，不在读侧兜底。

⚠️ **两条 SQL 谓词同形只保证「holder 为 NULL 的行两侧一致」，不保证 tab 徽标（`in_flight_count`）等于各公司列卡片数之和。** `detail` 的在途明细**不带公司谓词**（一次取齐全部公司的在途批次），而公司列只渲染「活跃 + 已映射」白名单 ⇒ holder 指向已停用 / 已解映射公司的批次照旧计入 tab 徽标，却在服务层被整组丢弃。tab 徽标与列内卡片数的差**只可能**来自这一处丢弃，见 §8.4 的登记条目（注意区分：单列内部的 `held_count == held_batches.len()` 恒等不受影响）。

### 4.3 snapshot 与 process detail 的关系

| | `snapshot`（4.1） | `detail`（4.2） |
|---|---|---|
| 只含有货工序 | ✅ `sendable + in_flight > 0` | ❌ 工序不存在即 404（不判有无货） |
| 行粒度 | 一工序一行（计数） | 一公司一列 + 一批次一张卡 |
| 分页 | 无 | 无 |
| SQL 条数 | **3**（候选分组 / 在途分组 / 工序元数据 `ANY`） | **4**（工序元数据 / 公司白名单 / 在途一次取齐 / 候选卡） |

⚠️ **`snapshot.processes[]` 只含有货工序 ⇒ tab 集合必须由前端 join 全量 OUTSOURCE 工序列表**，否则「该工序的货被发完 / 收完」的那一瞬间 tab 会从界面上消失，正在操作的用户被踢出当前页。工序的全量列表来自 `prod::process` 域（`category='OUTSOURCE'`）。

`snapshot` 的 SQL 条数恒定 3、`detail` 恒定 4，由 `cargo test --lib` 的 `outsource::board::sql_count_guard_tests::{no_sqlx_query_inside_loop_body, detail_queries_are_pinned}` 钉死（源码级护栏：**循环体内禁任何数据库往返调用点**（判定见 `board/mod.rs::QUERY_TERMINALS` / `QUERY_TERMINALS_UFCS`）+ 调用点数钉死）。改这两条聚合 SQL 必须同步 `board/repo.rs` 的方法 doc、`board/mod.rs` 的条数陈述与那两个常量。

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

`GET /outsource-quotes/` 的 `customer_id` 在 service 层展开成 part_id 集合（自身 ∪ **直接**子客户），落成 SQL 的一个 WHERE 段（`part_id = ANY($4)`）。

展开只下潜**一层** —— 依据是生产库实测（零件全挂 L2、L3 数量 0），而该结构 API 层不强制（`create_customer` 不校验 `parent_id` 是否指向根客户）。出现 L3 后本字段需改成递归 CTE，且漏报**是静默的**（`total` 偏小、不报错）。

⚠️ 这是报价一览里**唯一**还需要「展开成中间 id 集合」的维度（客户子树无法写成对 `t_outsource_quote` 单表 + `t_part` LEFT JOIN 的谓词），也因此它是**唯一**需要「客户子树零命中 → service 早返回空列表」守卫的维度 —— 空数组会让 `cardinality($4) = 0` 成立、整个客户条件被短路掉，不兜住就会「选了一个零件都没有的客户」返回**全量**报价。零件侧维度（`drawing_no` / `name` / `is_urgent`）不需要对应守卫：谓词形如 `($N::text IS NULL OR col …)`，NULL 时短路、给值时正常求值，零命中天然就是零行。

⚠️ **`GET /outsource-companies/{id}/sent-parts` 的 `customer_id` 语义不同** —— 那边**只判等值**（`p.customer_id = $4`），不做子树展开。理由：对账页筛的是「零件本身的归属客户」（这家外协厂供过哪个客户的货），与 batch / 工序域的 `customer_id` 谓词同形；而报价一览筛的是「客户视角的报价归属」，前端给的常是 L1 客户，故需展开。两者不要混用同一个 DTO 字段名当同义。

### 4.7 写端点的 OCC 锚（必填 `version`）

全仓约定：**写端点 body 的 `version` 必须必填**（无 `#[serde(default)]`），缺失 → HTTP **422 纯文本**，不是业务信封。

本域 2026-10-09 补齐的三条曾长期破例 —— service 内部 `quote_get_by_id` / `company_get_by_id` 读到当前 version 再喂给 repo 的 `UPDATE … WHERE id = $1 AND version = $2`。同一行上「刚读到的 version」必然等于「当前 version」，所以 `rows_affected = 0` 的分支**永不可达**，守卫形同虚设。

| 端点 | 补齐前 | 现在 |
|---|---|---|
| `POST /outsource-quotes/{id}/submit` | 无 body，`quote_submit(id, q.version, …)` | `{ version }`，先比 `q.version` 再守 |
| `POST /outsource-quotes/{id}/soft-delete` | 无 body，`quote_soft_delete(id, q.version, …)` | `{ version }` |
| `POST /outsource-companies/{id}/soft-delete` | 无 body，`company_soft_delete(id, company.version, …)` | `{ version }` |

**公司 `soft-delete` 的守卫顺序是契约：先 OCC（`40901`），再工序映射非空（`21205`）。** version 过期意味着整个对话框看到的公司状态已失效（可能是别人刚改的联系人 / 启停用 / 工序），此时报「仍映射 N 项工序，请先清空」会把用户引向错误的排查方向 —— 他会去清工序，而真正的原因是数据已被他人改动。

报价写端点的守卫顺序与之相反，是**状态机 → OCC**（见 §3.1）：状态不对时先说状态不对，因为状态流转失败与版本无关。

### 4.8 零件侧筛选：`drawing_no` / `name` 直连 ILIKE（取代 `keyword` 预搜索）

2026-10-09 两个列表端点的 `keyword` 都拆成 `drawing_no` + `name`（外加精确维度），**省掉三样东西**：

1. **`part_keyword_search` 预搜索**：`SELECT id FROM t_part WHERE … (drawing_no ILIKE $1 OR name ILIKE $1) LIMIT 10000`，**无 `ORDER BY`** ⇒ 一旦触顶返回的是**任意 10000 条**（非确定性子集、同一请求两次可能不同），`total` 偏小且零命中守卫不触发 ⇒ **静默少报**。
2. **「给了关键词却零命中要早返回」的 service 兜底分支**：那是第 1 条的补偿逻辑（空数组会让 `cardinality(...) = 0` 成立、整个条件被短路，从而返回全量）。直连谓词下零命中就是零行，这个易漏的分支一并消失。
3. **`part_ids_in` 中间数组**及其在 list / count 两条 SQL 里的 `part_id = ANY($N)` 谓词。

**SQL 侧的改动量**：报价一览为了 `p.drawing_no` / `p.name` / `p.is_urgent` 新加了一条 `LEFT JOIN t_part p`（并给 `t_outsource_quote` 起别名 `q`，否则两表的 `id` / `deleted_at` 同名列会判 ambiguous）；对账页的 `LEFT JOIN t_part p` 本就存在（取 `drawing_no` / `name` / `is_urgent`），新增谓词是**零 JOIN 改动**。

`LEFT` 而非 `INNER`：不传任何零件侧筛选时它必须恒等空操作（零件已软删 / `part_id` 悬空的存量报价仍要出现在一览里）；传了筛选时谓词在 NULL 行上求值为 NULL、不成立，行为与 `INNER JOIN` 一致。

### 4.9 工序映射的「有序集合」语义与 diff 守卫

`POST /outsource-companies/{id}/update` 的 `process_ids` 吸收了原 `POST /{id}/processes` 的职责，映射的**有序**集合有两处依赖：

- 写侧 `replace_processes` 按数组下标写 `sort_order`（首次出现位置去重保序）；
- 看板公司列的 `ORDER BY MIN(cp.sort_order)`（`board/repo.rs::SQL_COMPANIES_BY_PROCESS`）决定公司列内的公司顺序。

**diff 守卫**：目标有序集合 == 当前有序集合（`junction_list_by_company` 按 `sort_order ASC, id ASC`）时**跳过重写**。合并对话框之后每次保存都会走到这条路径，而 `replace_processes` 是「软删全部 + 逐条重建」—— 无脑执行会把整张 `t_outsource_company_process` churn 一遍（换一批雪花 id、`sort_order` 重排），而内容一字未变。去重保序的实现收在一个共享 helper 里（`dedup_keep_order`），因为「重复项在前还是在后」若在两处各写一遍，两份实现一旦漂移，diff 守卫会把「仅顺序不同」误判成「有变化」。

`company_update` 与 `replace_processes` 在**同一事务**内（handler `pool.begin()` 包两步），任一步失败整体回滚 —— 所以映射的校验错误（工序不存在 / 非 OUTSOURCE 类别 → `20801` / `21203`）不会留下「公司字段已改、映射没改」的半截状态。

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
| 6c | （2026-10-10 删除：`from.kind = INSPECTION_SHELF` 变体已不存在） | — |
| 6d | `quote_id` / `direct` 出现在非发送方向 | `20104 BIZ_INVALID_VALUE` |
| 7 | 发送方向：公司存在 → 启用 → 工序存在 → 工序类别 `OUTSOURCE` → 公司映射该工序 → `direct` / `quote_id` 恰给一个 → `requires_approval` 工序不许 `direct` | `21201` / `21205` / `20801` / `20104` / `20104` / `20104` / `20104` |
| 7b | 报价存在 → 状态 `APPROVED` → `(part, company, process)` 三元组一致 → APPROVAL 路径拒 DIRECT 占位价 | `21301` / `21307` / `21302` / `21307` |
| 8 | 回收生产：下一道工序推导；然后**自动选目标架**（候选集自带 `zone='PRODUCTION'` / 启用 / 未软删 / 「映射了该工序」四个谓词） | `20706` / `20508` |
| 9 | （2026-10-10 删除：回收直送品检方向整条下线。替代路径 = 先收进生产架，再走 `POST /api/v2/prod/batches/{batch_id}/to-inspection`，品检架同样由服务端自动选） | — |

`ensure_transition` 依赖 `PartStatus::can_transition_to`（`part::statemachine` 的内存迁移表）。发送方向用到的两条边是 `PENDING → OUTSOURCE` 与 `IN_PROCESS → OUTSOURCE`；⚠️ `IN_PROCESS → OUTSOURCE` 这条边**曾经缺失**，导致「可发送一览的行（几乎全是 `IN_PROCESS` 源）发一单就被 `20103` 拒」，端到端实测下外协发送 100% 不可用。状态机补边后守卫 6b 才真正承担 location 不变式 —— 别因为「守卫 6 已经能拒」就把它删掉。

### 5.2 出池不变式（回收直送品检方向已下线）

该方向的 `OUTSOURCE → INSPECTION` 是**出池**（`current_process_id` 与
`current_process_step_id` 两列同时清 NULL）。**2026-10-10 该方向整条下线**
（见 §0b），本节记录的是它下线前的不变式，供追溯历史行的口径：这条不变式本身仍然
成立 —— 走常规送检链路（`to-inspection` / worker-scan `INSPECTED`）落到品检架时，
出池清两列的行为完全相同。

`current_process_step_id` 指针的链尾自动送检衔接见 [`batch.md`](batch.md) §2.2。

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
| `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` | 2026-10-09 并入 `POST /api/v2/outsource-queue/move`（`OUTSOURCE_COMPANY` → `INSPECTION_SHELF`）；**2026-10-10 该方向本身也下线**（§0b），无替代端点。发 `to.kind = "INSPECTION_SHELF"` 现在得 **422 纯文本** |
| `GET /api/v2/parts/outsource-in-flight` | 2026-10-03 硬切 → `GET /outsource-shipments/in-flight`（旧端点返回错形状的通用 `PartListItem`；旧 URL 实际返 **400** 而非 404 —— part 域 `/{part_id}` catch-all 兜住未注册的 1 段静态路径后由 `Path` extractor 拒绝） |
| `GET /api/v2/parts/outsource-sendable` | 同上，2026-10-03 硬切（`OUTSOURCING` 端点同款错形状） |
| WS 事件名 `PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED` | 合并为 `OUTSOURCE_MOVE_DONE`（见 §7） |
| `repo/sql.rs` 的 `OutsourcePoolRepo`（4 方法） | 三条端点下线后全部无调用方，SQL 按「去公司谓词」的口径搬进 `board/repo.rs` |
| `vo/pool.rs` / `service/pool.rs` | 出参合并进 `vo/queue.rs`；service 实现被 `board/service.rs` 取代 |
| `OutsourceRepoTrait` 的 5 个方法（`pool_*` 4 + `sendable_list_by_process` 1） | 同上，无调用方 |
| `vo/shipment.rs` 的 `ApprovedForSendItem` / `ApprovedForSendListOut` | 死 VO，零调用方（表达不了 DIRECT 模式） |
| `POST /api/v2/prod/batches/{batch_id}/split` | 2026-10-09 提升为共用顶层端点 `POST /api/v2/batches/split`（`batch_id` 入 body），入参形态见 [`batch.md`](batch.md) §2.1 |
| `POST /api/v2/outsource-companies/{id}/processes` | 2026-10-09 硬切，功能吸收进 `POST /api/v2/outsource-companies/{id}/update` 的 `process_ids`（三态，见 §1.1）。连带删除 `SetOutsourceCompanyProcessRequest` 与 `OutsourceService::set_company_processes`。旧 URL 返 **404**（本 router 无其它 2 段 POST 会匹配它） |
| `GET /api/v2/outsource-quotes/{id}` | 2026-10-09 硬切（前端零消费）。连带删除 `OutsourceService::get_quote`。旧 URL 返 **404**（quote router 已无 1 段路由） |
| `POST /api/v2/outsource-quotes/{id}/update` | 2026-10-09 硬切（前端零消费）。连带删除 `OutsourceQuoteUpdateRequest` / `OutsourceService::update_quote` / `OutsourceQuoteRepo::update`。DRAFT 报价改价格 / 备注的路径改为「软删后重建一条 DRAFT」 |
| `OutsourceRepoTrait` 的 `part_keyword_search` / `quote_update` / `process_map_full` | 端点下线 + `keyword` 拆 `drawing_no` / `name`（见 §4.8）+ `OutsourceCompanyProcessLinkOut` 删 `category`（使 `process_map_full` 与 `process_map_short` 逐字同形），三者均无调用方 |
| 两个列表端点的 `keyword` 入参 | 拆成 `drawing_no` + `name` 直连 ILIKE（报价侧另有 `is_urgent`，对账页另有 `customer_id` / `process_id` / `is_billed`） |

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
| `t_part_event.event_type`（业务事实） | `SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` | **记录发生了什么业务事实**（哪个方向、去了哪），前端在工单时间线上按它分组 |

两个审计字面量**逐字不变**，与 WS 事件名的合并无关。`t_part_event.event_type` 是 `varchar(30)`，字面量超 30 字符会让 PG 返 22001 并把**整个事务**回滚（两个字面量分别 19 / 26 字符，均在限内）。

`RECEIVED_TO_INSPECTION` 曾是第三个审计字面量，随 `OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向于 2026-10-10 下线而**不再被写入**（§0b）。历史行里已有的该字面量仍可在时间线上读到。

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

#### 外协看板（第一批）

1. **URL 全量替换**：`/api/v2/outsource-pool/*` → `/api/v2/outsource-queue/{snapshot,processes/{id}}`；`/api/v2/outsource-sendable` **删除**（改用 `processes/{id}` 的 `items[]`）；`/api/v2/prod/batches/{id}/{send-to-outsource,receive-from-outsource,receive-from-outsource-to-inspection}` → `/api/v2/outsource-queue/move`。**无 alias**，旧路径 404。
2. **N+1 消除**：原「进程序列板 1 次 + 每公司 1 次 state」的组合应合并为**一次** `processes/{id}` 请求。
3. **`move` 入参形态变更**（**破坏性**）：
   - `batch_id` 从 path 参数移到 body，且**必须是 JSON 字符串**（`"1590000000000000001"`）。`shared::types::deserialize_i64` 只接受字符串，发 JSON number → **HTTP 422 纯文本**（响应里没有 `code` 字段，勿按 `40001` 分支解析）。
   - `version` **必填**，值取卡片上的 `items[].version` 或 `held_batches[].version`。
   - `outsource_company_id` 移进 `to` / `from` 对象（`{ kind, company_id }`）。
   - 2026-10-10：`shelf_id` **从两个变体里都删掉**（目标架由服务端选）；`INSPECTION_SHELF` 变体删除。
   - `process_id` **删除**；`next_process_id` 改为 `to.next_process_id` 且**可省略**（后端按工序链推导，`chain_resolvable = true` 时可省）。
   - `quantity` **删除**（整批语义）。
   - DIRECT 传 `direct: true` 且 `quote_id: null`；APPROVAL 传 `quote_id` 且 `direct: null`（两者互斥，恰给一个）。
4. **`move` 出参变更**：`PartOut`（part 级）→ `OutsourceMoveResult`（批次级）。读 part_id 改读 `out.part_id`；OCC 版本号改读 `out.version`（**写后读回的真实值**，不是请求的 `version + 1`）。`shipment_id` / `new_process_id` 按「键是否存在」判定方向，不要按 `null` 判定。
5. **候选卡 `shelf_id` 仍要读、但不再回传**（2026-10-10）：写端点的 `from.shelf_id`
   字段已删除，只校验 `batch.location == 'PRODUCTION_SHELF'`。候选卡的 `shelf_id`
   退化为**纯展示**（它仍有助于用户看清这批货在哪一格），**回传它不再有任何作用**
   —— 老客户端继续回传会被 serde 静默忽略。
6. **候选卡的 `shelf_id` 为空串的行走不通**：那是 `PENDING` 且未上架的批次（本来就在生产架之外），要先 `place-on-shelf`。
7. **新增 2 个看板 composable** + **删 3 个旧 composable**（`/outsource-pool/counts` 计数、`/outsource-pool/state` 每公司一次、`/outsource-pool/{process_id}` 详情）。
8. **zod schema 同步**：新增 `outsourceQueueSnapshotSchema` / `outsourceQueueProcessDetailSchema` / `outsourceQueueCandidateSchema` / `outsourceQueueCompanySchema` / `outsourceQueueHeldBatchSchema` / `outsourceMoveResultSchema`。**注意 zod 默认 strip 模式**会让漏声明的字段静默丢失，数组元素必须全字段声明（**候选卡 26 字段**，含 `has_process_chain`；**在途卡刻意无 `has_process_chain`**，勿给它补声明）。
9. **日期字段类型不一致**（勿写同一个 schema 复用）：候选卡 / `quotable` 的日期是 `YYYY-MM-DD` **字符串**（`to_char`）；`held_batches` 的是 ISO 日期串（native date）。
10. **`receive_next_process_id` 是字符串 `"0"`** 而非数字 0、亦非 `null`；配合 `chain_resolvable` 判定要不要弹手填对话框。
11. **`snapshot.processes[]` 的 tab 集合必须 join 全量 OUTSOURCE 工序列表**（见 §4.3）。

#### 公司 / 报价（第二批）

12. **删 3 个 api 函数**：`setOutsourceCompanyProcesses`（→ `updateOutsourceCompany` 的 `process_ids`）、`getOutsourceQuote`、`updateOutsourceQuote`。**无 alias**，旧 URL 404。
13. **`POST /outsource-companies` 出参改 `R<()>`**：`createOutsourceCompany` 的返回类型从 `OutsourceCompanyWithProcesses` 改成 `void`；建号后走列表失效重拉（不要试图从建号响应里取 `id`）。
14. **`POST /{id}/soft-delete`（公司）与报价的 `submit` / `soft-delete` 现在必须带 `{ version }`**。漏传 → **HTTP 422 纯文本**（响应无 `code` 字段，勿按 `40001` 分支解析）。公司 `soft-delete` 的 `version` 取公司 `GET /{id}` 或列表行的 `version`。
15. **公司编辑对话框合并工序勾选**：工序多选与联系人字段在**同一次** `POST /{id}/update` 里提交。`process_ids` 三态：不给 = 不动（只改联系人时**可以省略**，服务端不会重写映射表）；`[]` = 清空；`[..]` = 替换（保序）。⚠️ 要表达「用户什么都没改」时**不要**误传 `[]`。
16. **公司 VO 删字段**：`OutsourceCompany` / `OutsourceCompanyWithProcesses` 不再有 `created_at` / `updated_at`；`OutsourceCompanyProcessLink` 收成 `{ process_id, process_code, process_name }`（无 `category` / `sort_order`）。zod schema 必须同步删（zod strip 模式下「多声明」不报错但会误导，「少声明」会静默丢字段 —— 这里的方向是删，所以要确认删干净）。
17. **`GET /outsource-companies/by-process/{id}` 出参换窄 VO** `OutsourceCompanyOption { id, name }`（无 `is_active` / 联系人 / `version`）。**关键**：这个端点**没有 `version`** —— 若代码把它当成 `OutsourceCompany` 用（比如直接塞进需要 `version` 的保存 payload），会在提交时才炸 `40901`。
18. **对账页页头改读 sent-parts 信封**：`data.outsource_company_id` / `data.outsource_company_name`，可以删掉那次额外的 `GET /outsource-companies/{id}`。公司名可能为 `null`（公司已软删），标题位要能显示「未知公司」。
19. **sent-parts 行删 `quote_id` / `part_id`**：凡是靠 `item.part_id` 做零件跳转 / 定位的地方改用 `part_drawing_no` / `part_name`，或调 `GET /api/v2/parts/{id}` 时从别处拿 id（当前后端**没有**在 sent-parts 行里给零件 id，这是有意的取舍，见 §8.4）。
20. **两个列表的 `keyword` 拆成 `drawing_no` / `name`**：报价一览另有 `is_urgent`，对账页另有 `customer_id` / `process_id` / `is_billed`。⚠️ 对账页的 `customer_id` 是**等值**（零件直属客户），报价一览的 `customer_id` 是**一层子树展开**（见 §4.6），两者语义不同、不要复用同一个筛选组件的语义描述。
21. **`statuses` 现在真的生效了** —— wire format 是**逗号分隔单值**（`?statuses=DRAFT,SUBMITTED`），不是重复 key（详见 §3.3）。前端 `paramsSerializer` 的「CSV 单值」白名单已含 `statuses`，所以 `listOutsourceQuotes({ statuses: [...] })` 无需改动即可生效。⚠️ 顺带把 `statusFilterActive` 的判定复核一遍：后端此前恒不过滤，若前端曾用它做「有没有筛选」的提示，现在它才真正成立。
22. **报价一览的 `status` 与 `statuses` 并存且 AND**：两者同时传时取交集（各占各的 WHERE 段）。

### 8.4 已知偏差登记（不得省）

- **对账页（`sent-parts`）的每一行不再带零件 id（`part_id` 已删）。** 后端判断是「前端对账页只展示 `part_drawing_no` / `part_name`，零件 id 无消费方」；代价是若将来要在这张表里做「点零件跳详情」，得先补回 `part_id` 或另开一个按 shipment 取零件的端点。这是**有意**的字段删减，不是遗漏。
- **`by-process` 端点不返 `version`**，而 `GET /{id}` 与列表端点返。前端若把 `by-process` 的结果直接塞进需要 `version` 的保存 payload，会在提交时 `40901`。
- **`statuses` 的 wire format 与「多值 query 参数」的直觉相反**（逗号单值，不是重复 key），根因是 axum `Query` 走 `serde_urlencoded`、其 `Part` 反序列化器不支持序列（§3.3 有实测表）。**重复 key 形态在本字段上是「取最后一个」而非 OR**，误用它会静默少筛。
- **`GET /outsource-quotes/` 的 `customer_id` 只下潜一层客户子树**，出现 L3 后漏报是**静默的**（`total` 偏小、不报错）。
- **对账页的 `customer_id` 只判等值、不做子树展开**（与报价一览语义不同，见 §4.6）。若产品要求「按 L1 客户看该客户全部零件的外协发货记录」，需要另行扩子树，不是本轮遗漏。
- **`companies[].held_count` 与 `held_batches.len()` 的一致性由服务层保证（集成测试 + lib 单测 `held_count_matches_held_batches_len` 锁），但若未来有人在 SQL 侧重新加 `COUNT` 会静默分叉。** SQL 侧已刻意不做 `COUNT`（`SQL_COMPANIES_BY_PROCESS` 的 doc 逐字写了这一点）—— 恢复 `COUNT` 的诱惑来自「顺手」，代价是两条 SQL 的谓词一旦漂移就静默不一致。
- **`t_part_batch.current_holder_id` 可空，而 `location='OUTSOURCE_COMPANY'` 的批次按业务不变式必有 holder（= 公司 id）；本仓对违约行不兜底。** 在途侧的两条 SQL 统一加 `current_holder_id IS NOT NULL`（见 §4.2），后果是这类批次**在 `snapshot` 的 `in_flight_total` 与 `detail` 的在途卡里都不出现**（静默少算，不是报错）。这是有意的取舍：给它单独一个「holder 缺失」的位置反而要求读侧造一个假的分组键。若将来这类数据真的出现，应该修的是写入侧不变式，不是读侧。
- **tab 的在途徽标可能大于全部公司列的卡片数之和，且这是当前设计的正常表现（不是显示 bug）。** 徽标来自 `snapshot` 的 `COUNT(*)`（**不带公司谓词**），卡片只落在 `detail.companies[]` 的「活跃 + 已映射」白名单列里；holder 指向**已停用**或**已解映射**公司的在途批次会计入徽标却被整组丢弃（批次发出后再停用公司 / 解映射即可复现，症状是「徽标 3、列里 0 张卡」）。两条 SQL 的 `current_holder_id IS NOT NULL` 谓词逐字同形也**只**保证「holder 为 NULL 的行两侧一致」，所以徽标与列内卡片数的差**只可能**来自这一处丢弃；前端不要把它当数据不一致做告警。**该修的是写入侧**：不应允许停用公司持有在途批次（公司停用 / 映射删除的写端点应拒绝「仍有在途批次」的公司），而不是在读侧造一个假分组键把丢弃的批次重新挂回某列。
- **候选卡 `shelf_id` 对「`PENDING` 且未上架」的批次序列化为空串（不是 `null`）。** 这类行本来就在生产架之外，拖拽发送会被 `from` 守卫以 `20122` 拒收。选空串而非 `null` 是因为 `null` 会让前端的必填字符串校验炸在**整页渲染**上。
- **`snapshot.processes[]` 只含 `sendable + in_flight > 0` 的工序 ⇒ tab 集合必须由前端 join 全量 OUTSOURCE 工序列表，否则操作到一半 tab 会消失。** 后端不返「零货工序」是刻意的（序列板的语义是「现在有活要干的工序」），但这意味着 tab 集合不是后端给的单一真源。
- **`OutsourceMoveResult.version` 是写后读回的真实值，不是在 Rust 里算的 `batch.version + 1`。** 写入口的 OCC 守卫与源状态白名单都可能让 UPDATE 命中 0 行，让「算出来的 +1」与真实值分叉；而分叉的症状是「刚拖完就冲突」，极难定位。代价是多一次读（同一事务内）。
- **`OutsourceMoveResult.new_location` 与 `to_kind` 恒等**（冗余字段）。让「归位键」是显式字段而不是「推导得出」的东西，理由是 WS payload 会被缓存重放（前端刷新后先补事件再拉列表）。
- **`snapshot` 的工序元数据查不到（已软删）时 `category` 兜底为 `"OUTSOURCE"`**，而候选侧不可能命中软删工序（它 INNER JOIN 了 `t_process`）—— 只有在途侧会。兜底而非返 `null` 是因为前端要按 `category` 分组渲染。
- **`detail.process` 无 `category` 字段**（单工序详情不展示类别，与 `prod::queue` 的 `QueueProcessMeta` 对齐），而 `snapshot.processes[]` 有。
- **§1.5 的「静态段必须先于 catch-all 注册」是硬约束，没有编译期保障。** 公司 router 里加一条新的 1 段静态路由而放到 `/{id}` 之后 ⇒ 该路径返 **400**（不是 404），症状与「路由没注册」不同，极易误判。（quote router 暂时无此约束，因为 `/{id}` 已下线。）
- **公司 / 报价两域的 `version` 必填是**「422 纯文本」**而不是业务信封**，前端错误处理要按 HTTP 状态码分支，不能假设响应必有 `code` 字段。同理 `Path<i64>` 抽不出数字时的 400。
- **看板两个端点的权限面比旧 `/outsource-pool/state` 宽了半档（2026-10-09 收敛的连带后果）。** 旧 `state` 端点是 **Manager + Clerk**，而它吐的 `unit_price` 与客户 / 申请人名属于外协域的商务敏感读面；收敛后 `held_batches[].price`（同一列的另一种叫法）被内联进 `GET /outsource-queue/processes/{id}` 的 `companies[]`，而该端点为了让 Inspector 能收货/发料给了 **Manager + Clerk + Inspector**。⇒ **`held_batches[].price` 与客户 / 申请人名对 Inspector 打开了**，这是本轮有意接受的暴露面变化（代价：旧端点那点保护没了；收益：收货角色不必再靠两次请求拼看板）。若产品不接受，正确修法是给 `price` 单独加字段级脱敏（按角色置 `null`），而不是把整个端点退回 Manager + Clerk —— 那会让 Inspector 看不到自己经手的在途卡。角色口径与理由记在 `src/modules/outsource/handler/board.rs` 的模块 doc。

## 9. 错误码分段（`src/shared/error.rs::code`）

本域用到的业务码（`2xxxx` 段，外协自有 `21xxx`）：

| 码 | 符号 | 触发 |
|---|---|---|
| `20101` | `BIZ_PART_NOT_FOUND` | 批次关联的 part 不存在 / 已软删 |
| `20103` | `BIZ_INVALID_TRANSITION` | move 状态机守卫 |
| `20104` | `BIZ_INVALID_VALUE` | 分方向业务规则（工序类别 / 公司映射 / direct 互斥 / `quantity <= 0` / `review_note` 空）。**货架 zone 一项已随 2026-10-10 自动选架下线** |
| `20508` | `BIZ_SHELF_PROCESS_NOT_FOUND` | **2026-10-10 新增于本域**：回收生产时该工序无可用生产货架（选架候选为空） |
| `422` | 无信封（axum `JsonRejection`） | **2026-10-10 新增于本域**：`to.kind = "INSPECTION_SHELF"`（变体已删除）|
| `20109` | `BIZ_PART_BATCH_NOT_FOUND` | 批次不存在 / 已软删 |
| `20122` | `BIZ_BATCH_LOCATION_MISMATCH` | `from` 与批次真实位置不符 |
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