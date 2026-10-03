# outsource-sendable 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service/,vo/,repo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：可发送外协的一览（**1 端点**）。2026-10-03 新增。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-quotes.md`](./outsource-quotes.md) /
> [`./outsource-shipments.md`](./outsource-shipments.md) / [`./production/batches.md`](./production/batches.md) /
> [`./outsource-pool.md`](./outsource-pool.md)（外协看板，2026-10-03 新增 —— **候选侧与本端点共用同一份核心 SQL**，见本文件「与 `/outsource-pool` 的 SQL 共享」节）。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-sendable` | Manager / Clerk / Inspector | 可发送外协的活跃批次一览（**一行 = 一个批次**），APPROVAL / DIRECT 双模式 |

> **独立顶层前缀**：判定横跨 company（候选公司）+ quote（APPROVED 价）+ batch（批次状态 / 货架 / OCC），
> 不属于任何单一域的子资源，故不 nest 进 `outsource-quotes` / `outsource-shipments`。
> 命名沿用旧的 `/parts/outsource-sendable`，便于前端对照迁移。

---

## 业务模型

- **一行 = 一个活跃批次**。判据是 **`t_part_batch.current_process_id` 指向一道
  OUTSOURCE 工序**（那一列是工序候选池归属的权威依据 —— 2026-09-30 新增，
  见 [`production/batches.md`](./production/batches.md#post-apiv2prodbatchesdispatch2026-09-30-重构bulk-only)
  的写入不变式）。
  2026-10-03 改判据前是「(活跃批次 × 货架上的 OUTSOURCE 工序 × 零件工艺链内)」的
  组合粒度，且要求该工序出现在零件的 `process_chain_id` 链内；生产库里绝大多数零件
  没有链（且 `t_process_chain_step` 里没有 OUTSOURCE 类的 step），交集恒空 ⇒
  **端点恒返回空列表**。同类问题在 `prod::worker_pool` 的候选池 SQL 上已于 2026-09-30
  以同样方式修过。
- **批次范围**：`t_part_batch.deleted_at IS NULL` 且
  （`status = 'PENDING'` 或（`status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF'`)）
  → `source_status` = `PENDING` / `IN_PROCESS`。
  `status = 'IN_PROCESS'` 但 `location = 'WORKER'` 的批次**不出现**（零件在工人手上，不在架上）。
  `current_process_id IS NULL`（PENDING 未派工）也不出现。
- **OUTSOURCE 工序来源**：`JOIN t_process pr ON pr.id = pb.current_process_id`
  （`deleted_at IS NULL AND category = 'OUTSOURCE'`）。**工艺链不参与判定** ——
  写侧 `send-to-outsource` / `receive-from-outsource` 已于 2026-10-03 同样放开链的必须性
  （`current_process_step_id` 是可选的显示用定位信息，无链时落 NULL）。
- **审批闸门（`t_process.requires_approval`，DEFAULT true）**：
  - `requires_approval = false` → 免审批直发，直接出行。
  - `requires_approval = true` → **必须**已有该 `(part, process)` 的 APPROVED 报价，
    否则不出行（`NOT pr.requires_approval OR EXISTS (… APPROVED quote …)`）。
    这是 `requires_approval` 第一次被真正读取（此前 process CRUD 只写不读）。
- **`send_mode` 二选一**（LEFT JOIN `t_outsource_quote`：
  `q.part_id = p.id AND q.process_id = pr.id AND q.status = 'APPROVED'
  AND q.deleted_at IS NULL AND pr.requires_approval`）：
  - `requires_approval = true`（⇒ 报价必然存在）→ `send_mode = "APPROVAL"`，
    `quote_id` / `outsource_company_id` / `price` 三件套取自报价，
    `company_options` 恒为空数组。
  - `requires_approval = false` → `send_mode = "DIRECT"`，`quote_id` /
    `outsource_company_id` / `price` 恒 `null`（**即使历史上存在已批准报价也不回传** ——
    JOIN 条件里的 `AND pr.requires_approval` 就是为了守住这条），
    `company_options` = 该 OUTSOURCE 工序映射的**全部活跃公司**
    （`t_outsource_company_process` JOIN `t_outsource_company` where `is_active AND deleted_at IS NULL`）。
  - **DIRECT 且 `company_options` 为空的行仍要返回**（前端 `canSend()` 据
    `company_options.length >= 1` 把它置灰），`total` 同样计入 —— 不要在 SQL 里滤掉。
- **多 APPROVED 报价的处理**：DB 有 partial unique
  `uq_t_outsource_quote_approved_part_process` 兜底（撞了 → 21303 DUPLICATE），但并发审批 /
  历史数据仍可能出现多条。SQL 用 `DISTINCT ON (batch_id, current_process_id)` + `ORDER BY … quote_id ASC NULLS LAST`
  **取 id 最小的那条**：语义是「先批准的报价优先」，且结果稳定（不随查询计划变化）。
  `current_process_id` 由 `batch_id` 单值决定，故这两列的分组键语义等价；保留两列是为了
  让 count 侧精简投影与全投影共享同一组键列名。
- **货架可空**：`t_shelf` 是 `LEFT JOIN`（`PENDING` 且未上架的批次没有
  `current_holder_id`）⇒ `shelf_code` 为 `null`。

---

## 共享 DTO

### OutsourceSendableListQuery 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `keyword` | string? | part 的 `drawing_no` / `name` ILIKE `%needle%`；trim 后空串视为无过滤 |
| `customer_id` | i64? | 按 `t_part.customer_id` 精确过滤 |
| `limit` | i64? | 默认 50，clamp(1, 200) |
| `offset` | i64? | 默认 0，max(0) |

### OutsourceSendableItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `version` | i32 | **`t_part_batch.version`**（批次级 OCC）。前端发送时原样回传 |
| `send_mode` | string | `"APPROVAL"`（工序需审批 + 已命中批准报价）/ `"DIRECT"`（工序免审批直发） |
| `source_status` | string | `"PENDING"` / `"IN_PROCESS"`（批次来源状态） |
| `part_id` | string (i64) | |
| `part_serial_no` | string? | |
| `part_drawing_no` | string? | |
| `part_name` | string? | |
| `quantity` | i32 | 可发送数量（行 = 批次，恒等于 `batch_quantity`） |
| `batch_id` | string (i64) | `t_part_batch.id` —— 发送 / 接收端点的路径锚点 |
| `batch_no` | i32 | 批次号（per-part 递增） |
| `batch_quantity` | i32 | `t_part_batch.quantity` |
| `planned_delivery_date` | string? | `t_part.planned_delivery_date`，`YYYY-MM-DD` |
| `is_urgent` | bool | |
| `customer_path` | string? | 有 L1 拼 `L1 / L2`，仅 L2 时给 L2 名，无客户 `null` |
| `current_process_id` | string (i64) | 批次当前所在的外协工序（= `t_part_batch.current_process_id`）—— 发送时当 `process_id` 回传 |
| `current_process_name` | string? | |
| `shelf_code` | string? | 批次所在货架 code（如 `C2`）；`PENDING` 未上架的批次为 `null` |
| `outsource_company_id` | string (i64)? | APPROVAL 有值（取报价的公司）/ DIRECT `null` |
| `outsource_company_name` | string? | 同上 |
| `quote_id` | string (i64)? | **APPROVAL 有值 / DIRECT `null`**。前端靠它决定发送时传哪个报价 |
| `company_options` | `{ id: string (i64), name: string }[]` | DIRECT 列出候选活跃公司；APPROVAL 恒 `[]` |
| `price` | string? | APPROVAL 的 Decimal 字符串 / DIRECT `null` |
| `status_label` | string | 恒为 `"sendable"`（前端按它筛可发送集合） |

> `quote_id` 是 2026-10-03 新增字段。此前前端的 `OutsourceSendableItem` 类型里没有它，
> APPROVAL 模式发送时只能靠 `outsource_company_id` 反查报价。本 VO 补上后前端可直接回传。

### OutsourceSendableListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `OutsourceSendableItem[]` | |
| `total` | i64 | 全量命中行数（与 items 同 WHERE + DISTINCT ON 口径，**含 DIRECT 空 options 行**） |
| `limit` | i64 | 回显（clamp 后） |
| `offset` | i64 | 回显 |

---

## 端点契约要点

### 前端如何用本端点的输出驱动写端点

本端点是**纯读**的；实际发送 / 接收走
[`prod::batch` 域](./production/batches.md) 的批次级端点：

```
POST /api/v2/prod/batches/{batch_id}/send-to-outsource
POST /api/v2/prod/batches/{batch_id}/receive-from-outsource
```

| 本端点给出的值 | 写端点要传什么 |
|---|---|
| `send_mode == "APPROVAL"` | `send-to-outsource` 传 `quote_id`（= 本行的 `quote_id`）+ `outsource_company_id` |
| `send_mode == "DIRECT"` | `send-to-outsource` 传 `direct: true` + 用户在 `company_options` 里选出的 `outsource_company_id`（无报价，故无 `quote_id`） |
| 部分发送 / 部分接收 | 传 `quantity`（≤ `batch_quantity`）；`quantity == batch_quantity` 时传 `null` 走全量语义 |
| `version` | 两种端点都必传的 OCC 锚（批次级），原样回传 |
| `current_process_id` | `send-to-outsource` 的 `process_id` |
| `canSend()` 判定 | `status_label === 'sendable'` 且（`send_mode === 'APPROVAL'` 或 `company_options.length >= 1`） |

### 排序

`is_urgent DESC, planned_delivery_date ASC NULLS LAST, part_id ASC, batch_no ASC, current_process_id ASC`
—— 加急件永远在前；同零件多批次按 `batch_no` 升序，保证翻页稳定。

### 事务边界

- **读端点**：`pool.acquire()` **不开事务**，service 借 `&mut *conn` 跑两条查询。
- WS 广播：**本域无**。发送 / 接收动作的 WS 事件由 `prod::batch` 域在 commit 后广播。

### 防 N+1

- `company_options` 用**标量子查询 + `array_agg(json_build_object(...))` 一条 SQL 拿完**
  （APPROVAL 行走 `CASE ... THEN '[]'::jsonb` 短路，不触发该子查询）。
  ⚠️ 实现细节：必须包一层 `to_jsonb(...)` —— sqlx 解不了 `json[]`，只能解单个 `jsonb` 值。
- list + count 两条查询搞定；service 层**不循环查公司**。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_sendable` + `sendable_router()`
- service：`src/modules/outsource/service/sendable.rs::OutsourceService::list_sendable`
- repo：`src/modules/outsource/repo/sql.rs::OutsourceSendableRepo::list / count`
  （行结构 `repo/mod.rs::OutsourceSendableRow`；核心 SQL 常量 `SENDABLE_INNER_X_SQL` /
  `SENDABLE_DISTINCT_D_SQL` / `SENDABLE_PROJECTION_*` / `SENDABLE_OUTER_COLS` /
  `SENDABLE_DISPLAY_ORDER` 供 `list_by_process` / `pool_group_sendable_counts` 复用）
- dto：`src/modules/outsource/dto.rs::OutsourceSendableListQuery`
- vo：`src/modules/outsource/vo/sendable.rs::OutsourceSendableItem / OutsourceSendableListOut / OutsourceCompanyOption`
- 路由挂载：`/outsource-sendable`（见 `src/modules/mod.rs::v2_router`）

> **2026-10-03：`/outsource-pool/{process_id}` 的 `items` 与本端点同源**。看板按外协
> 工序切 tab，需要「不分页拿全 + 按工序过滤」，本端点的分页契约不能动，故另起
> `/outsource-pool` 前缀而不是给它加 `process_id`。两端点的行粒度、字段口径、
> 排序**必须保持一致** —— 谓词抽到共享 SQL 常量
> `repo/sql.rs::SENDABLE_INNER_X_SQL`（唯一落点），
> 由 `tests/outsource/pool.rs::pool_detail_items_match_sendable_endpoint_field_by_field`
> 逐字段比对守住。**改本端点的判定谓词时，必须同查那批常量。**
>
> **与 `quotable-parts` 不同源**（2026-10-03 解除约束）：报价 picker 的谓词是
> 「该零件有 PENDING 批次」（一零件一行，给还没下发的零件提前锁价），
> 与本端点的「批次停在某道外协工序上」是两回事。两者**不共享任何 SQL 常量**，
> 也不再要求口径一致 —— 唯一交集是都要对 `t_part.deleted_at IS NULL` 过滤。

---

## 集成测试

`tests/outsource/sendable.rs`（15 用例）：

- `sendable_approval_mode_when_approved_quote_exists` — APPROVAL 三件套 + `company_options` 空数组 + `version == batch.version`
- `sendable_direct_mode_lists_active_company_options` — DIRECT 正确列出**活跃**公司（停用的不得出现）
- `sendable_direct_mode_even_with_approved_quote_when_approval_not_required` — 免审批工序即使有已批准报价也判 DIRECT，且报价三件套为 `null`
- `sendable_requires_approval_without_quote_excluded` — 需审批但无已批准报价 → **不出行**
- `sendable_requires_approval_with_draft_quote_excluded` — DRAFT 报价不算已批准 → **不出行**
- `sendable_direct_row_kept_when_no_active_company` — 空 options 行仍返回且计入 `total`
- `sendable_pending_without_holder_has_null_shelf_code` — 未上架的 PENDING 批次仍出行，`shelf_code` 为 `null`
- `sendable_excludes_non_outsource_current_process` — `current_process_id` 指向 INHOUSE 工序 → 不出现
- `sendable_excludes_batch_without_current_process` — `current_process_id IS NULL` → 不出现
- `sendable_source_status_and_batch_version` — `source_status` 区分 PENDING / IN_PROCESS，WORKER 上的批次不出现
- `sendable_customer_id_filter_and_keyword` — 两个 query 过滤生效
- `sendable_total_matches_items_and_pagination` — `total` 与实际行数一致 + 分页（**不含 DIRECT 空 options 行** —— 本用例的 2 个 DIRECT 行各有 1 个 option；空 options 行由 `sendable_direct_row_kept_when_no_active_company` 单独覆盖）
- `sendable_orders_urgent_first_then_planned_delivery` — 排序
- `sendable_includes_outsource_process_when_part_has_no_chain` — 零件**完全没有**工艺链 → 仍出行（取代旧的 `sendable_excludes_process_not_in_part_chain`）
- `sendable_includes_process_outside_part_chain` — 工艺链内没有该外协工序、货架也没映射该工序 → 仍出行
- `sendable_one_row_per_batch_even_with_many_batches` — 行粒度是「批次」（同一零件多批次出多行）

单测：`service/sendable.rs::mod tests` 6 个（`send_mode_of` 三分支 + `company_options`
JSON 解码：空数组 / 正常 / 畸形降级不 500）。

---

## 前端配套改动清单

> 口径同
> [`./production/shelf-process-mapping.md#前端配套改动清单`](./production/shelf-process-mapping.md#前端配套改动清单)。
> **前端配套改动不止改 URL**：本批 4 个读端点里，2 个换了前缀、2 个换了返回形状
> （分页信封），另有 1 个出参新增字段、1 个类型声明与实际返回不符。只改 URL 的话
> 页面仍会「能请求但不显示 / 显示错」。

> **状态：5 项已全部落地**（前端仓 `hsh-erp/frontend`，2026-10-03 逐个打开
> 核对当前实现）。本节从「待办清单」转为「已完成的改动记录」—— 保留是为了
> ① 记录硬切前后的 URL 对照，便于日后排查旧路径残留；② 记录返回形状变更的
> 前端同步面（`listQuotableParts` / `useOutsourceReceivingList` 两处若漏改，
> 症状是「表格空白 / 翻页恒 1 页」且**不报错**，最难自查）。

| 前端位置 | 改前现状（2026-10-03 前） | 改后新契约（前端已完成适配） |
|---|---|---|
| `frontend/src/api/outsource.ts` 的 `listOutsourceInFlight` | 打 `/parts/outsource-in-flight`，返回按 `OutsourceInFlightItem[]` 消费 | 打 `/outsource-shipments/in-flight`（旧路径已下线，实际 400），返回按 `OutsourceInFlightListResult`（`{items,total,limit,offset}`）消费并经 Zod 守门 |
| `frontend/src/api/parts/crud.ts` 的 sendable helper | 函数在 `api/parts/crud.ts`，打 `/parts/outsource-sendable` | 函数已迁到 `frontend/src/api/outsource.ts::listOutsourceSendable`，打 `/outsource-sendable`（旧路径已下线，实际 400）—— 读侧属外协域，与写的 `send-to-outsource`（prod/batches 域）不同域 |
| `frontend/src/api/outsource.ts` 的 `listQuotableParts` 返回类型 | 声明 `Promise<PartListItem[]>`，按数组消费 | 返回 `QuotablePartListResult`（分页信封）；消费方 `OutsourceQuoteList.vue` 改读 `r.items` |
| `frontend/src/views/outsource/composables/useOutsourceReceivingList.ts` 的 `receivingFetcher` | `return { items, total: items.length }` | `return { items: r.items, total: r.total }`（`total: items.length` 会让翻页器只有 1 页） |
| `frontend/src/types/outsource.ts` 的 `OutsourceSendableItem` | 无 `quote_id` | 有 `quote_id: string \| null`（APPROVAL 有值 / DIRECT `null`）—— `send-to-outsource` 要求 `quote_id` 与 `direct` 必传其一 |

### 已知限制（不在本轮修）

- **`keyword` 不转义 SQL LIKE 通配符**：`service/mod.rs::keyword_pattern` 直接
  `%{kw}%`，不拒 `%` / `_` / `\`。注入面为 0（纯 bind 参数），但
  `?keyword=%` 等价于「不过滤」、`?keyword=_` 匹配任意单字符。仓库正在形成
  「service 层拒通配符」的约定（见 [`./parts/lifecycle.md`](./parts/lifecycle.md)
  里 `pending-programming` 的转义说明），本端点尚未跟进。
