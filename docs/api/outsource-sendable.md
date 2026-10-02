# outsource-sendable 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service/,vo/,repo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：可发送外协的一览（**1 端点**）。2026-10-03 新增。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-quotes.md`](./outsource-quotes.md) /
> [`./outsource-shipments.md`](./outsource-shipments.md) / [`./production/batches.md`](./production/batches.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-sendable` | Manager / Clerk / Inspector | 可发送外协的（活跃批次 × OUTSOURCE 工序）一览，APPROVAL / DIRECT 双模式 |

> **独立顶层前缀**：判定横跨 company（候选公司）+ quote（APPROVED 价）+ batch（批次状态 / 货架 / OCC），
> 不属于任何单一域的子资源，故不 nest 进 `outsource-quotes` / `outsource-shipments`。
> 命名沿用旧的 `/parts/outsource-sendable`，便于前端对照迁移。

---

## 业务模型

- **一行 = 一个组合**：`(活跃批次, 该批次所在货架上、且在该零件工艺链内的 OUTSOURCE 工序)`。
  同一批次若货架上绑了 2 个符合条件的 OUTSOURCE 工序，出 **2 行**。
- **批次范围**：`t_part_batch.deleted_at IS NULL` 且
  （`status = 'PENDING'` 或（`status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF'`)）
  → `source_status` = `PENDING` / `IN_PROCESS`。
  `status = 'IN_PROCESS'` 但 `location = 'WORKER'` 的批次**不出现**（零件在工人手上，不在架上）。
- **OUTSOURCE 工序来源**：`t_shelf_process`（`sp.shelf_id = pb.current_holder_id`，
  `deleted_at IS NULL`）JOIN `t_process`（`deleted_at IS NULL AND category = 'OUTSOURCE'`），
  **并与该 part 的 `t_process_chain_step` 求交**（`deleted_at IS NULL`）——
  与 [`quotable-parts`](./outsource-quotes.md#quotable-parts-的行粒度与筛选) 同理：
  少了链内交集会给出工艺链上不存在的工序，发送时
  `resolve_step_id_by_process` 会 404。
- **`send_mode` 二选一**（LEFT JOIN `t_outsource_quote`：
  `q.part_id = p.id AND q.process_id = pr.id AND q.status = 'APPROVED' AND q.deleted_at IS NULL`）：
  - 命中 → `send_mode = "APPROVAL"`，`quote_id` / `outsource_company_id` / `price` 三件套
    取自报价，`company_options` 恒为空数组。
  - 未命中 → `send_mode = "DIRECT"`，`quote_id` / `outsource_company_id` / `price` 恒 `null`，
    `company_options` = 该 OUTSOURCE 工序映射的**全部活跃公司**
    （`t_outsource_company_process` JOIN `t_outsource_company` where `is_active AND deleted_at IS NULL`）。
  - **DIRECT 且 `company_options` 为空的行仍要返回**（前端 `canSend()` 据
    `company_options.length >= 1` 把它置灰），`total` 同样计入 —— 不要在 SQL 里滤掉。
- **多 APPROVED 报价的处理**：DB 有 partial unique
  `uq_t_outsource_quote_approved_part_process` 兜底（撞了 → 21303 DUPLICATE），但并发审批 /
  历史数据仍可能出现多条。SQL 用 `DISTINCT ON (batch_id, next_process_id)` + `ORDER BY … quote_id ASC NULLS LAST`
  **取 id 最小的那条**：语义是「先批准的报价优先」，且结果稳定（不随查询计划变化）。
  同一层 `DISTINCT ON` 也顺手吃掉 `t_shelf_process` / `t_process_chain_step` 的重复行
  （两者都没有 `(shelf, process)` / `(chain, process)` 唯一约束）。

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
| `send_mode` | string | `"APPROVAL"` / `"DIRECT"` |
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
| `next_process_id` | string (i64) | 该货架上的 OUTSOURCE 工序 —— 发送时当 `process_id` 回传 |
| `next_process_name` | string? | |
| `shelf_code` | string? | 批次所在货架 code（如 `C2`） |
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
| `next_process_id` | `send-to-outsource` 的 `process_id` |
| `canSend()` 判定 | `status_label === 'sendable'` 且（`send_mode === 'APPROVAL'` 或 `company_options.length >= 1`） |

### 排序

`is_urgent DESC, planned_delivery_date ASC NULLS LAST, part_id ASC, batch_no ASC, next_process_id ASC`
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
  （行结构 `repo/mod.rs::OutsourceSendableRow`）
- dto：`src/modules/outsource/dto.rs::OutsourceSendableListQuery`
- vo：`src/modules/outsource/vo/sendable.rs::OutsourceSendableItem / OutsourceSendableListOut / OutsourceCompanyOption`
- 路由挂载：`/outsource-sendable`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

`tests/outsource/sendable.rs`（9 用例）：

- `sendable_approval_mode_when_approved_quote_exists` — APPROVAL 三件套 + `company_options` 空数组 + `version == batch.version`
- `sendable_direct_mode_lists_active_company_options` — DIRECT 正确列出**活跃**公司（停用的不得出现）
- `sendable_direct_row_kept_when_no_active_company` — 空 options 行仍返回且计入 `total`
- `sendable_source_status_and_batch_version` — `source_status` 区分 PENDING / IN_PROCESS，WORKER 上的批次不出现
- `sendable_customer_id_filter_and_keyword` — 两个 query 过滤生效
- `sendable_total_matches_items_and_pagination` — `total` 与实际行数一致（含 DIRECT 空 options 行）+ 分页
- `sendable_orders_urgent_first_then_planned_delivery` — 排序
- `sendable_excludes_process_not_in_part_chain` — 工序不在工艺链内不出现
- `sendable_one_row_per_batch_process_even_with_many_batches` — 行粒度是「批次 × 工序」（多批次出多行）

单测：`service/sendable.rs::mod tests` 3 个（`company_options` JSON 解码：空数组 / 正常 / 畸形降级不 500）。
