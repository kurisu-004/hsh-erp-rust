# outsource-pool 域 API（外协看板，2026-10-03 新增）

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,repo/,service/,vo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：按「外协工序」切 tab 的看板三件套（**3 个只读端点**）。
> 关联域：[`./outsource-sendable.md`](./outsource-sendable.md)（候选侧同源 SQL）/
> [`./outsource-shipments.md`](./outsource-shipments.md)（在途一览）/
> [`./outsource-companies.md`](./outsource-companies.md)（右列公司）/
> [`./production/worker-pool.md`](./production/worker-pool.md)（形态模板 `prod::pool`）。
>
> **写入方不在本域**：`send-to-outsource` / `receive-from-outsource` 在 `prod::batch`
> 域（见 [`./production/batches.md`](./production/batches.md)），本域**只读**。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-pool/counts` | **Manager + Clerk + Inspector** | 跨所有货架，按外协工序聚合「可发 / 在途」双徽标 |
| GET | `/api/v2/outsource-pool/{process_id}` | **Manager + Clerk + Inspector** | 单个 tab 的全部内容：左列候选批次 + 右列全部活跃公司 |
| GET | `/api/v2/outsource-pool/state` | **Manager + Clerk** | 某公司在某工序在外协的全部批次（看板右列的卡片来源） |

> 路由挂载：`/api/v2/outsource-pool`（独立顶层前缀，见 `src/modules/mod.rs::v2_router`）。
>
> ⚠️ **`/counts` / `/state` 必须注册在 `/{process_id}` 之前**
> （`src/modules/outsource/handler.rs::pool_router`）。matchit 里参数段
> `/{process_id}` 会兜住任何未命中静态段的单段路径，`Path<i64>` 反序列化
> `counts` / `state` 会失败 → **400** 而不是 200/404。同坑在
> `quote_router()` 的 `quotable-parts` 上已踩过一次。

### 为什么是独立前缀（不扩 `/outsource-sendable`）

看板要求「每个外协工序一个 tab、tab 内不分页拿全」，而既有
`GET /outsource-sendable` 与 `GET /outsource-shipments/in-flight` **都不接受
`process_id` 且都分页**。给它们加可选 `process_id` 会同时改变两个已上线端点的
分页契约（`total` 语义 / 翻页器行为），故另起前缀。

形态**刻意完全照抄 `/api/v2/prod/pool/*`**（`state` / `counts` / `{process_id}`），
让前端复用同一套 queryKey 命名、失效编排与测试范本。

---

## 业务模型

看板 = 「左列一叠候选卡片」+「右列 N 个公司列」。以**外协工序**为 tab 维度：

```
GET /outsource-pool/counts          → tab 列表 + 每个 tab 的「可发 / 在途」徽标
GET /outsource-pool/{process_id}    → 选中 tab：左列 items + 右列 companies
GET /outsource-pool/state?...       → 点某公司列：那一列的批次卡片
```

### 一行 = 什么

| 端点 | 行粒度 |
|---|---|
| `counts[].*` | 一道**外协工序**（`t_process.category = 'OUTSOURCE'` 语义域） |
| `{process_id}.items[]` | 一个 `(活跃批次, 该批次所在货架上、且在该零件工艺链内的 OUTSOURCE 工序)` 组合 —— **与 `/outsource-sendable` 的行粒度逐行一致** |
| `{process_id}.companies[]` | 该工序映射的一条**活跃外协公司**（含 `held_count = 0` 的空列） |
| `state.items[]` | 一个 `(批次, 外协公司, 外协工序)` 三元组，批次满足 `status='OUTSOURCE' AND location='OUTSOURCE_COMPANY'` |

### 候选侧判定（与 `/outsource-sendable` 逐条一致）

- **批次范围**：`t_part_batch.deleted_at IS NULL` 且
  （`status = 'PENDING'` 或（`status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF'`)）
  → `source_status = PENDING / IN_PROCESS`。
- **OUTSOURCE 工序来源**：`t_shelf_process`（`sp.shelf_id = pb.current_holder_id`）
  JOIN `t_process`（`category = 'OUTSOURCE'`）**并与该 part 的
  `t_process_chain_step` 求交** —— 少了链内交集会给出工艺链上不存在的工序，
  发送时 `resolve_step_id_by_process` 会 404。
- **`send_mode` 二选一**（LEFT JOIN `t_outsource_quote`，`status='APPROVED'`）：
  命中 → `APPROVAL`（`quote_id` / `outsource_company_id` / `price` 取报价，
  `company_options` 恒 `[]`）；未命中 → `DIRECT`（三者恒 `null`，`company_options`
  = 该工序映射的**全部活跃公司**）。
- **DIRECT 且 `company_options` 为空的行仍要返回**且计入 `total` /
  `sendable_count`（前端据 `can_send` 把它置灰，**不要在 SQL 里滤掉**）。
- **多 APPROVED 报价**：`DISTINCT ON (batch_id, next_process_id)` +
  `ORDER BY … quote_id ASC NULLS LAST` 取最早批准的那条（语义是「先批准的报价优先」
  且结果稳定，不随查询计划变化）。

### 在途侧判定

`t_part_batch`：`status = 'OUTSOURCE' AND location = 'OUTSOURCE_COMPANY'
AND deleted_at IS NULL`，归属锚是批次自身的
`current_holder_id`（外协公司）+ `current_process_id`（外协工序）—— 写侧
`send_to_outsource` 就是这么落的（`prod::batch::service::outsource.rs` 的
`mark_batch_with_status_and_meta(..., Some(company_id), Some(step_id), Some(process_id))`）。

> ⚠️ **在途侧两处口径刻意不对称**（都只可能在途侧触发，候选侧不涉及）：
> 1. `counts[].in_flight_count` 只按 `current_process_id` 聚合，**不 JOIN
>    `t_process`**；而 `{process_id}.companies[]` 的 `held_count` 要求公司
>    `is_active` **且**已映射该工序。因此
>    **`in_flight_count` 可能 `>` `Σ companies[].held_count`** —— 差值来自
>    「公司被停用 / 映射被删 / 工序已软删」的批次。同理
>    `counts[].process_name` 的 `(deleted#<id>)` 占位也只会在途侧出现
>    （候选侧 INNER JOIN `t_process` 已排除软删工序）。
> 2. `in_flight_count` 不做货架 scope 过滤（`counts` 是 admin 视角的全厂聚合），
>    与 SHELF 账号无关 —— `counts` 对 SHELF 账号是 403。

### 下一道工序的派生（`state.items[*].receive_next_process_*`）

**锚链取 `COALESCE(p.process_chain_id, cur.chain_id)`**：`cur` =
`pb.current_process_step_id` 指向的 step，只用它取 `sort_order`；取下一 step 时
在**锚链**内找 `sort_order = 当前 + 1` 且未软删的 step。DB 有唯一索引
`uq_chain_step_chain_order (chain_id, sort_order) WHERE deleted_at IS NULL`
⇒ **唯一无歧义**。锚链软删时同样落到「无下一 step」分支。

**锚链必须与写侧同源**：写侧 `receive-from-outsource` 走
`require_process_chain(part_id)`（读 `t_part.process_chain_id`）+ 
`resolve_step_id_by_process(chain_id, next_process_id)`。锚 `p.process_chain_id`
⇒ 本端点给出的 process_id 必然是**锚链内的活跃 step 的工序**，写侧一定能解析到。

`chain_resolvable = receive_next_process_id != 0`，等价于下面三条同时成立：

1. `current_process_step_id` 存在且非 `"0"`；
2. 锚链存在且**未软删**；
3. 锚链内存在下一 step（未软删）。

**业务含义**（前端据此决定接收时要不要弹对话框让用户填工序）：
`true` ⇒ 工序链已知，可免填「下一道工序」；`false` ⇒ 工序链缺失或指针漂移，
必须让用户填。

> 「下一 step 的工序已软删」时：`receive_next_process_id` 仍有值、
> `receive_next_process_name` 为 `null`、`chain_resolvable` 仍为 `true` ——
> 判据按步骤 1–3 判定，工序名取不到不影响「下一 step 存在」这个事实。
> 这也是 `receive_next_process_name` 声明为可空的原因。

> ⚠️ **`chain_resolvable == true` 仍可能收到写侧 404（`20702`）**：若 part 在外协
> 期间被改绑工艺链、且**新链内与 `current_process_step_id` 的 `sort_order + 1`
> 位置没有 step**（例如新链只有一道工序），本端点算出的下一 step 缺失 ⇒
> `chain_resolvable` 为 `false`，前端走既有交互路径（弹对话框让用户手填）；
> 但若 part 在**本端点返回之后、用户点接收之前**被再次改绑 / 该 step 被软删，
> 写侧会 **404 `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`**（`chain {} 内找不到
> process_id={} 的活跃 step`）。这类竞态无法在读侧消除，**前端必须处理 `20702`
> 兜底**：与 `chain_resolvable == false` 一样弹对话框让用户手填下一道工序，
> 重试一次即可。`p.process_chain_id IS NULL`（脏数据）时锚链回落到
> `cur.chain_id`，此时写侧会先撞 `20706 BIZ_PROCESS_CHAIN_REQUIRED`（409）——
> 同属「读侧已登录才能看到、但写侧不保证接受」的情形。

---

## 共享 DTO

### OutsourcePoolCountsOut 字段

`GET /outsource-pool/counts` 顶层响应。

| 字段 | 类型 | 说明 |
|---|---|---|
| `counts` | `OutsourcePoolProcessCount[]` | 只含 `sendable_count + in_flight_count > 0` 的工序；按 `process_id ASC` 稳定排序 |
| `sendable_total` | i64 | 各工序 `sendable_count` 之和 |
| `in_flight_total` | i64 | 各工序 `in_flight_count` 之和 |
| `total` | i64 | `sendable_total + in_flight_total` |

### OutsourcePoolProcessCount 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 工序雪花 ID |
| `process_code` | string | `t_process.code`；工序已软删（仅在途侧可能）时为空串 |
| `process_name` | string | `t_process.name`；工序已软删时为 `"(deleted#<id>)"` 占位 |
| `sendable_count` | i64 | 可发送候选批次数。**行粒度与 `{process_id}.items` 完全一致**（同源 SQL，含 DIRECT 空 options 行） |
| `in_flight_count` | i64 | 在该工序在外协的批次数 |

### OutsourcePoolDetailOut 字段

`GET /outsource-pool/{process_id}` 顶层响应（一个 tab 的全部内容）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 回显路径参数 |
| `process_code` | string | `t_process.code` |
| `process_name` | string | `t_process.name` |
| `companies` | `OutsourcePoolCompanyOut[]` | 该工序映射的全部**活跃**公司（含 `held_count = 0` 的空列）；按 `MIN(sort_order) ASC, company_id ASC` |
| `total` | i64 | `items.len()`（不分页） |
| `items` | `OutsourcePoolCandidate[]` | 候选批次；排序见下文「排序」 |

### OutsourcePoolCompanyOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `company_id` | string (i64) | `t_outsource_company.id` |
| `name` | string | 公司名 |
| `held_count` | i64 | 该公司在该工序在外协的批次数（`COUNT(pb.id)::bigint`，LEFT JOIN ⇒ 无在途批次时为 `0`） |

### OutsourcePoolCandidate 字段

看板左列的一个卡片。

| 字段 | 类型 | 说明 |
|---|---|---|
| `version` | i32 | **`t_part_batch.version`**（批次级 OCC）。前端发送时原样回传 |
| `send_mode` | string | `"APPROVAL"` / `"DIRECT"` |
| `source_status` | string | `"PENDING"` / `"IN_PROCESS"`（批次来源状态） |
| `part_id` | string (i64) | |
| `part_serial_no` | string? | `t_part.serial_no` |
| `part_drawing_no` | string? | `t_part.drawing_no` |
| `part_name` | string? | `t_part.name` |
| `quantity` | i32 | 可发送数量（行 = 批次，恒等于 `batch_quantity`） |
| `batch_id` | string (i64) | `t_part_batch.id` —— 发送 / 接收端点的路径锚点 |
| `batch_no` | i32 | 批次号（per-part 递增） |
| `batch_quantity` | i32 | `t_part_batch.quantity` |
| `planned_delivery_date` | string? | `t_part.planned_delivery_date`，`YYYY-MM-DD` |
| `is_urgent` | bool | |
| `customer_path` | string? | 有 L1 拼 `L1 / L2`，仅 L2 时给 L2 名，无客户 `null` |
| `shelf_code` | string? | 批次所在货架 code |
| `outsource_company_id` | string (i64)? | APPROVAL 有值（取报价的公司）/ DIRECT `null` |
| `outsource_company_name` | string? | 同上 |
| `quote_id` | string (i64)? | APPROVAL 有值 / DIRECT `null`。前端靠它决定发送时传哪个报价 |
| `company_options` | `{ id: string (i64), name: string }[]` | DIRECT 列出候选活跃公司；APPROVAL 恒 `[]` |
| `price` | string? | APPROVAL 的 Decimal 字符串 / DIRECT `null` |
| `can_send` | bool | `send_mode == "APPROVAL" \|\| !company_options.is_empty()`（服务端算好，前端不必各写一遍） |
| `status_label` | string | 恒为 `"sendable"` |

> **不复用 `OutsourceSendableItem`**：后者多 `next_process_id` / `next_process_name`
> （分页一览里「这一行是哪道工序」是必须的），而看板视角工序已提到顶层
> `process_id`。保留两组字段会让前端 Zod 守门时出现两个真相源。两者的其余字段
> 逐字段一致，由集成测试
> `pool_detail_items_match_sendable_endpoint_field_by_field` 逐字段比对守住。

### OutsourcePoolStateOut 字段

`GET /outsource-pool/state` 顶层响应。

| 字段 | 类型 | 说明 |
|---|---|---|
| `outsource_company_id` | string (i64) | 回显入参 |
| `outsource_company_name` | string? | 公司不存在 / 已软删时为 `null`（本端点不做存在性拒绝，见「端点契约要点」） |
| `process_id` | string (i64) | 回显入参 |
| `current_held` | i64 | `items.len()` |
| `items` | `OutsourceHeldBatchItem[]` | 按 `t_part_batch.id ASC` |

### OutsourceHeldBatchItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | string (i64) | `t_part_batch.id` —— `receive-from-outsource` 的路径锚点 |
| `part_id` | string (i64) | |
| `batch_no` | i32 | |
| `quantity` | i32 | **当前余量**（`t_part_batch.quantity`），**不是** `shipment.quantity` —— 前端拿它做「部分接收」输入框的 max 值（口径与 [`GET /outsource-shipments/in-flight`](./outsource-shipments.md#端点列表) 一致） |
| `serial_no` | string? | |
| `drawing_no` | string | |
| `name` | string | `t_part.name` |
| `system_delivery_date` | date? | `t_part.system_delivery_date` |
| `planned_delivery_date` | date? | `t_part.planned_delivery_date` |
| `is_urgent` | bool | |
| `customer_name` | string? | L2 叶子客户名 |
| `parent_customer_name` | string? | L1 一级集团名 |
| `applicant_name` | string? | `t_part.applicant_name` LEFT JOIN `t_applicant.name`（非 FK，字符串匹配）。`t_applicant` 的唯一索引是 `(name, customer_id)`，**name 单独不唯一**，故 JOIN 走 `LEFT JOIN LATERAL (… ORDER BY id ASC LIMIT 1)` 收敛到一行 —— 否则同名申请人跨客户并存时一个批次会扇出成多行，破坏 `current_held == items.len()` |
| `location` | string | 恒为 `"OUTSOURCE_COMPANY"` |
| `note` | string? | 工单级备注（`t_part.note`；DB 无 batch 级 remark 字段） |
| `version` | i32 | **`t_part_batch.version`** —— `receive-from-outsource` 的 OCC 锚 |
| `sent_at` | string? | `t_outsource_shipment.sent_at`（ISO 串）。正常流恒有值，可空是 `LEFT JOIN` 的诚实映射 |
| `price` | string? | `t_outsource_shipment.unit_price` 的 Decimal 字符串。同上；**刻意不用空串兜底**（空串会被前端当成「0 元 / 格式错误的数」渲染，比 `null` 难排查） |
| `receive_next_process_id` | string (i64) | 下一道工序 id；**无下一 step 时为字符串 `"0"`**（0 兜底口径，见下） |
| `receive_next_process_name` | string? | 下一道工序名 |
| `chain_resolvable` | bool | `receive_next_process_id != 0` |

> **0 兜底口径**：`receive_next_process_id` 是后端 `i64` + `serialize_i64`，NULL
> 走 `.unwrap_or(0)`（SQL 侧 `COALESCE(nx.next_process_id, 0)`）⇒ JSON 里
> **非 nullable** 的字符串 `"0"`，语义为「未设」。同一口径见
> `prod::batch::vo::PendingBatchItem.current_process_step_id`。

### OutsourcePoolStateQuery 字段

`GET /outsource-pool/state` 的入参（两个都必填）。

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `outsource_company_id` | i64 | ✓ | 外协公司雪花 ID（= `t_part_batch.current_holder_id`） |
| `process_id` | i64 | ✓ | 外协工序雪花 ID（= `t_part_batch.current_process_id`） |

> 两个 query 参数在 wire 上都是**字符串**（雪花 ID 超 i64 JS 安全整数），由
> `serde_urlencoded` 按串解析成 `i64`；类型列写 `i64` 是与同域
> `outsource-sendable.md` / `outsource-shipments.md` 的 query 参数标注保持一致。
> **响应体**里的雪花 ID 则序列化成字符串（`serialize_i64`），两者不要混。

---

## 端点契约要点

### 权限（逐条登记）

| 端点 | 权限 | 依据 |
|---|---|---|
| `GET /outsource-pool/counts` | **Manager + Clerk + Inspector** | 照抄 `GET /api/v2/prod/pool/counts`（`docs/api/production/worker-pool.md`「端点列表」行 + `service::pool_counts_all_shelves` 的 `require_any_role(&[Manager, Clerk, Inspector])`）—— admin 视角但不止 Manager。外协候选与在途本来就是业务/跟单视角，Clerk 必须能看 |
| `GET /outsource-pool/{process_id}` | **Manager + Clerk + Inspector** | 照抄 `GET /api/v2/prod/pool/{process_id}`（同文档；`service::pool_by_process` 内 `require_any_role`）。与 `counts` 同集合：两者是同一个看板的 tab 列表与 tab 内容，权限必须一致，否则会出现「徽标看得见、点进去 403」 |
| `GET /outsource-pool/state` | **Manager + Clerk** | 对齐同域等价数据端点 `GET /outsource-shipments/in-flight`（`docs/api/outsource-shipments.md`，Manager / Clerk）。**不放宽到「已登录」**：本端点除批次元数据外还吐 `price`（`t_outsource_shipment.unit_price`）与 `customer_name` / `parent_customer_name` / `applicant_name`，敏感级别与 `/in-flight` 同档；而 `GET /api/v2/prod/pool/state` 之所以能做到「已登录即可读」，是因为它只吐内部批次元数据 —— **不能把 prod 侧的宽松口径照抄到外协域**，否则 SHELF scope 账号被 `counts` / `{process_id}` 双双 403，却能经 `/state` 枚举任意外协公司的在外协批次、单价与客户 |

三处守卫都在 **service 层**（`current.require_any_role`），handler 不重复校验
（与 work_type / assembly 域惯例一致）。负向回归网：`tests/outsource/pool.rs` 的
`pool_counts_forbidden_for_shelf_account` / `pool_by_process_forbidden_for_shelf_account`
/ `pool_state_forbidden_for_shelf_account`（三个端点各一条，SHELF scope 账号
必须 403 + `code=40300`）。

### 前端如何用本域输出驱动写端点

本域**纯读**；发送 / 接收走
[`prod::batch` 域](./production/batches.md#外协流转send--receive)：

```
POST /api/v2/prod/batches/{batch_id}/send-to-outsource
POST /api/v2/prod/batches/{batch_id}/receive-from-outsource
```

| 本域给出的值 | 写端点要传什么 |
|---|---|
| `items[*].send_mode == "APPROVAL"` | `send-to-outsource` 传 `quote_id`（= 本行的 `quote_id`）+ `outsource_company_id` |
| `items[*].send_mode == "DIRECT"` | `send-to-outsource` 传 `direct: true` + 用户在 `company_options` 里选出的 `outsource_company_id` |
| `items[*].can_send == false` | 卡片置灰，不发起请求（后端也会因守卫拒绝） |
| 部分发送 / 部分接收 | 传 `quantity`（≤ `batch_quantity` / ≤ `quantity`）；等于全量时传 `null` 走全量语义 |
| `items[*].version` / `state.items[*].version` | 两种端点都必传的 OCC 锚（批次级），原样回传 |
| `process_id` | `send-to-outsource` 的 `process_id` |
| `state.items[*].chain_resolvable` | `true` ⇒ 接收时免填「下一道工序」；`false` ⇒ 弹对话框让用户填 |
| `state.items[*].receive_next_process_id` | `chain_resolvable == true` 时可作为对话框的默认值 |
| 写侧 404 `20702` / 409 `20706` | 与 `chain_resolvable == false` **同一条交互路径**：弹对话框让用户手填下一道工序后重试（成因见「下一道工序的派生」的 caveat） |

### 排序

`items`：`is_urgent DESC, planned_delivery_date ASC NULLS LAST, part_id ASC,
batch_no ASC, next_process_id ASC` —— 与 `/outsource-sendable` 逐字一致
（同源 `SENDABLE_DISPLAY_ORDER` 常量）。

`companies`：`MIN(t_outsource_company_process.sort_order) ASC, company_id ASC`
—— 与 `company_options` 的 `array_agg(... ORDER BY cp2.sort_order, c2.id)` 同序，
右列的列序与候选下拉的可选项序一致。

`counts`：`process_id ASC`。

`state.items`：`t_part_batch.id ASC`（与
`prod::worker_pool::WorkerPoolRepo::list_held_by_worker_with_part` 惯例一致，
按 batch_id 稳定展示）。

### 工序不存在时的行为（刻意不对称）

- `GET /outsource-pool/{process_id}` 工序不存在 / 已软删 → **404 `20801
  BIZ_PROCESS_NOT_FOUND`**（口径同 `/prod/pool/{process_id}`）：响应里有
  `process_code` / `process_name` 两个 tab 标题位，拿不到元数据就只能给空串，
  前端会渲染出一个无名 tab。
- `GET /outsource-pool/state` **不校验工序存在性**，工序 id 无对应在途批次时返回
  **200 + 空 `items` + `current_held = 0`**。理由：该 VO 里没有工序元数据位可承载
  404 语义，且「某公司在这道工序上没有在外协的批次」本身就是看板要如实展示的
  合法状态（该列就是空的），不是错误。
- 公司同理：`outsource_company_name` 为 `null` 而非 404。

### 路由注册顺序（硬约束）

`pool_router()` 里 `/counts`、`/state` 必须先于 `/{process_id}` 注册，否则
matchit 会把 `counts` / `state` 当成 `process_id` 交给 `Path<i64>` 解析 →
**400**。这是跨端 404/400 语义里最容易被当成「端点没上线」的坑，故
`tests/outsource/pool.rs::pool_counts_and_state_are_static_routes_not_process_id`
单独守它。

### 事务边界

三个端点都是**纯读**：`pool.acquire()` **不开事务**，service 借 `&mut *conn`
跑查询，连接用完即 drop（与 `CLAUDE.md`「读端点不开事务」一致）。

### WS 广播

**无**。本域不产生任何业务流转。发送 / 接收动作的 WS 事件由 `prod::batch` 域在
commit 后广播（`SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` 对应的 part_event +
dashboard 事件），见 [`./websocket.md`](./websocket.md)。

### 防 N+1

- 候选侧 `company_options` 用**标量子查询 + `array_agg(json_build_object(...))`
  一条 SQL 拿完**（APPROVAL 行走 `CASE … THEN '[]'::jsonb` 短路，不触发该子查询）。
  ⚠️ 必须包一层 `to_jsonb(...)` —— sqlx 解不了 `json[]`，只能解单个 `jsonb` 值。
- `counts` 的工序元数据**一次 `process_map_short(&all_ids)` 取齐**（不是按工序逐个查）。
- `{process_id}` 的 `companies[].held_count` 与 `state.items` 全部在各自**一条 SQL**
  内 JOIN / `LEFT JOIN LATERAL` 解析完毕，service 层零回查。
- 查询条数：`counts` 3 条 SQL（候选 / 在途两条 `GROUP BY` + 一次
  `process_map_short` 元数据，并集与排序在内存里做）、`{process_id}` 3 条、
  `state` 2 条 —— **与行数、工序数无关**。

### 与 `/outsource-sendable` 的 SQL 共享（防分叉）

`repo/sql.rs` 把候选侧的「产出行」部分抽成常量 + 参数化投影：

| 常量 / 函数 | 作用 |
|---|---|
| `SENDABLE_INNER_X_SQL` | 内层 JOIN + WHERE —— **判定谓词的唯一落点** |
| `SENDABLE_DISTINCT_D_SQL` | `DISTINCT ON (batch_id, next_process_id)` 收敛 |
| `SENDABLE_PROJECTION_FULL` / `SENDABLE_DEDUP_PROJECTION_FULL` | list / `list_by_process` 的投影 |
| `SENDABLE_PROJECTION_COUNT` / `SENDABLE_DEDUP_PROJECTION_COUNT` | count / counts 的精简投影（不重算 `company_options`） |
| `SENDABLE_OUTER_COLS` / `SENDABLE_DISPLAY_ORDER` | 外层列清单 / 展示序 |
| `sendable_dedup_sql(inner, dedup)` | 拼出 `x → d` 两层 |

四个查询（`list` / `count` / `list_by_process` / `group_sendable_counts`）只换投影与
外层过滤，**谓词一行都不重复**。`format!` 只填编译期常量、用户输入一律走 bind，
故 4 个查询用 `sqlx::AssertSqlSafe(sql)` 包裹动态 SQL 文本是安全的（口径同
`com::union_list::repo::sql`）。

`tests/outsource/pool.rs::pool_detail_items_match_sendable_endpoint_field_by_field`
逐字段比对两个端点的输出，是这条约束的回归网。

---

## 实现位置

| 层 | 位置 |
|---|---|
| handler | `src/modules/outsource/handler.rs::pool_counts / pool_by_process / pool_state` + `pool_router()` |
| service | `src/modules/outsource/service/pool.rs::OutsourceService::{pool_counts, pool_by_process, pool_state}` |
| repo | `src/modules/outsource/repo/sql.rs::OutsourcePoolRepo`（`group_sendable_counts` / `group_in_flight_counts` / `list_companies_with_held` / `list_held`）与 `OutsourceSendableRepo::list_by_process` |
| 共享 SQL 常量 | `src/modules/outsource/repo/sql.rs::SENDABLE_*`（见上表） |
| 行结构 | `src/modules/outsource/repo/mod.rs::OutsourcePoolCompanyRow / OutsourceHeldBatchRow / OutsourceSendableRow` |
| dto | `src/modules/outsource/dto.rs::OutsourcePoolStateQuery` |
| vo | `src/modules/outsource/vo/pool.rs`（`OutsourcePoolCountsOut` / `OutsourcePoolDetailOut` / `OutsourcePoolStateOut` 等 7 个类型） |
| 路由挂载 | `src/modules/mod.rs::v2_router`（`.nest("/outsource-pool", outsource::pool_router())`） |

---

## 集成测试

`tests/outsource/pool.rs`（17 用例，与本文件验收标准逐条对应）：

| 用例 | 守的不变量 |
|---|---|
| `pool_counts_returns_200_sorted_and_totals_match` | counts 只含 `sendable+in_flight>0` 的工序、按 `process_id ASC`；`total == sendable_total + in_flight_total`；工序元数据正确；0+0 的工序不出现 |
| `pool_counts_allows_clerk_role` | 权限正向：CLERK 可读 counts / state |
| `pool_counts_forbidden_for_shelf_account` | **权限负向回归**：SHELF scope 账号读 counts → 403 + `code=40300`（删掉守卫该用例会红） |
| `pool_by_process_forbidden_for_shelf_account` | 同上，`{process_id}` 403 |
| `pool_state_forbidden_for_shelf_account` | 同上，`state` 403（`state` 带单价 + 客户名，守卫不能被放宽回「已登录」） |
| `pool_counts_empty_returns_zeroed_totals` | 空库返 200 + 空数组 + 全零（不是 500） |
| `pool_counts_and_state_are_static_routes_not_process_id` | **注册顺序守卫**：`/counts`、`/state` 不被 `/{process_id}` 吞成 400 |
| `pool_detail_lists_all_mapped_companies_including_empty_column` | `companies` 恰为映射的活跃公司（停用的不出现）；`held_count` 正确；**无在途批次的公司也在列且 `= 0`**；`items` 不含其它工序的行；`total == items.len()` |
| `pool_detail_items_match_sendable_endpoint_field_by_field` | **防 SQL 分叉核心断言**：21 个字段逐字段比对 `/outsource-pool/{pid}` 与 `/outsource-sendable`（同 seed、过滤 `next_process_id == pid`）；停用公司不进 `company_options` |
| `pool_detail_keeps_direct_row_with_empty_company_options` | DIRECT 空 options 行仍返回、计入 `total`，且 `counts.sendable_count` 也计入；`can_send == false` |
| `pool_detail_unknown_process_returns_404` | 工序不存在 → 404 / `code=20801` |
| `pool_state_returns_held_batches_with_shipment_fields` | 该公司在该工序的全部在外协批次（含 `sent_at` / `price` / `version`）；`current_held == items.len()`；别的公司 / 别的工序的批次不出现 |
| `pool_state_chain_resolvable_when_next_step_exists` | 有下一 step ⇒ `chain_resolvable == true` 且 id / name = 下一 step 的工序 |
| `pool_state_chain_unresolvable_when_no_step_or_chain_tail` | 链尾 / 无 `current_process_step_id` 两种情形 ⇒ `chain_resolvable == false`、`receive_next_process_id == "0"`、`_name == null` |
| `pool_state_derives_next_step_from_parts_current_chain_after_rebind` | **读侧锚链与写侧同源**：part 改绑到链 B 后，`receive_next_process_id` 指向链 B 的下一道工序（不是旧链 A 的），且写侧 `resolve_step_id_by_process` 在链 B 内确实能解析到该 step |
| `pool_state_does_not_fan_out_on_duplicate_applicant_name` | `t_applicant` 同名跨客户并存时 `items` **不扇出**（一个批次恒一行、无重复 `batch_id`）、`current_held == items.len()`、`applicant_name` 仍取到 |
| `pool_state_rejects_missing_query_params_with_400` | 缺任一 query 参数 → **400**（非 500、非静默默认值） |

单测：

- `src/modules/outsource/service/pool.rs::mod tests` 3 个（`can_send` 判定口径：
  APPROVAL 恒可发 / DIRECT 空 options 不可发 / DIRECT 有 options 可发）
- `src/modules/outsource/vo/pool.rs::mod tests` 3 个（`receive_next_process_id` 的
  0 兜底序列化成字符串 `"0"`、雪花字段是字符串）

---

## 前端对接要点

1. **queryKey 建议**：与 `/prod/pool` 同形 ——
   `['outsourcePool','counts']` / `['outsourcePool','detail', processId]` /
   `['outsourcePool','state', companyId, processId]`。
2. **失效时机**：发送 / 接收成功（`prod::batch` 的 WS 事件）后，三个 queryKey
   全部 invalidate —— 候选数、在途数、公司列会同时变。
3. **不传 `process_id` 时不要退化**：三个端点里只有 `state` 允许「某工序无数据」，
   `{process_id}` 不存在会 404（见「工序不存在时的行为」）。
