# outsource-quotes 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service.rs,model.rs,repo.rs,statemachine.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 域覆盖：外协报价 lifecycle + 报价 picker（quote **9 端点**）。2026-09-13 Phase 2 落地；
> 2026-10-03 补读侧 `quotable-parts`。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-shipments.md`](./outsource-shipments.md) / [`./outsource-sendable.md`](./outsource-sendable.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-quotes` | Manager / Clerk / Inspector / CncProgrammer | 列表（part / company / status 过滤 + 分页） |
| POST | `/api/v2/outsource-quotes` | Manager / Clerk | 新建 DRAFT 报价 |
| GET | `/api/v2/outsource-quotes/quotable-parts` | Manager / Clerk | **报价 picker**：还没下发的零件（**一行 = 一个零件**，2026-10-03 新增） |
| GET | `/api/v2/outsource-quotes/{id}` | Manager / Clerk / Inspector / CncProgrammer | 报价详情 |
| POST | `/api/v2/outsource-quotes/{id}/update` | Manager / Clerk | DRAFT 部分更新（OCC） |
| POST | `/api/v2/outsource-quotes/{id}/submit` | Manager / Clerk | DRAFT → SUBMITTED |
| POST | `/api/v2/outsource-quotes/{id}/approve` | **Manager** | SUBMITTED → APPROVED（需 `version` + 可选 `review_note`） |
| POST | `/api/v2/outsource-quotes/{id}/reject` | **Manager** | SUBMITTED → REJECTED（需 `version` + `review_note`） |
| POST | `/api/v2/outsource-quotes/{id}/soft-delete` | Manager | DRAFT/REJECTED 软删（OCC） |

> ⚠️ **路由顺序**：`/quotable-parts` 静态段**必须在 `/{id}` catch-all 之前注册**。
> 此前该端点未注册，`quotable-parts` 被 `/{id}`（`Path<i64>`）吞掉 →
> `PathRejection` → 恒 400（前端报价一览页每次进都报错，「新建报价」picker 恒空）。

---

## 业务模型

- **报价表** `t_outsource_quote`：`id` / `part_id` / `outsource_company_id` / `process_id` / `price` (numeric) / `status` / `quantity`? / `is_direct` / `is_billed` / `version` / 审计字段 + 软删。
- **状态机**：`DRAFT → SUBMITTED → APPROVED | REJECTED`（单向前进，不可回退；详见 `src/modules/outsource/statemachine.rs::can_transition_to`）。
- **唯一约束**：同 `(part_id, outsource_company_id, process_id)` 仅一个活跃报价（DB partial unique 兜底 → 21303 DUPLICATE）。

---

## 共享错误码（213xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21301 | BIZ_OUTSOURCE_QUOTE_NOT_FOUND | 404 | 报价不存在 / 已软删 |
| 21302 | BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION | 400 | 当前状态不允许此操作（如 SUBMITTED 调 update） |
| 21303 | BIZ_OUTSOURCE_QUOTE_DUPLICATE | 409 | 同 `(part, company, process)` 已存在活跃报价 |
| 21307 | BIZ_OUTSOURCE_QUOTE_NOT_APPROVED | 404 | 找不到 `(part, company, process)` 的 APPROVED 报价 |

> 21304–21306 预留（业务未触发的中间状态码）。

---

## 共享 DTO

### OutsourceQuoteOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `part_id` | string (i64) | 关联 part |
| `outsource_company_id` | string (i64) | 关联外协公司 |
| `process_id` | string (i64) | 关联工序 |
| `price` | decimal | 报价单价 |
| `quantity` | i32? | 可选（部分模式下不指定数量） |
| `status` | string | "DRAFT" / "SUBMITTED" / "APPROVED" / "REJECTED" |
| `is_direct` | bool | 是否直接派外协（不经仓库） |
| `is_billed` | bool | 是否已开票 |
| `review_note` | string? | Manager 审批 / 拒绝备注 |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

### OutsourceQuoteListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `OutsourceQuoteOut[]` | |
| `total` | i64 | 全量命中行数 |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### OutsourceQuoteCreateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | 关联 part |
| `outsource_company_id` | string (i64) | ✓ | 关联外协公司 |
| `process_id` | string (i64) | ✓ | 关联工序（必须是公司已映射的 OUTSOURCE 工序） |
| `price` | decimal | ✓ | 报价单价（> 0） |
| `quantity` | i32? | — | 可选 |
| `is_direct` | bool? | — | 缺省 false |

### OutsourceQuoteUpdateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `price` | decimal? | — | None = 不改 |
| `quantity` | i32? | — | None = 不改；Some(null) 清空 |
| `is_direct` | bool? | — | None = 不改 |

> 仅 DRAFT 可 update；SUBMITTED 及以后返回 21302。

### OutsourceQuoteApproveRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `review_note` | string? | — | 审批备注 |

### OutsourceQuoteRejectRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `review_note` | string | ✓ | 拒绝理由（> 0 字符） |

### OutsourceQuoteListQuery 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64)? | 按 part 过滤 |
| `outsource_company_id` | string (i64)? | 按 company 过滤 |
| `customer_id` | string (i64)? | **2026-10-03 订正**：代码 `OutsourceQuoteListQuery` 里一直有这个字段，但本文档此前漏记。当前实现只在「同时给了 `customer_id`」时生效，且**必须同时给 `keyword`**：只给 `customer_id` 不给 `keyword` 时端点直接返回空列表（`total=0`）——已知简化实现，见 service `list_quotes` 注释 |
| `status` | string? | 单状态过滤 |
| `statuses` | string? | 多状态过滤（逗号分隔）；**2026-10-03 订正**：DTO 里没有该字段，service 恒传空数组给 repo（等价不过滤） |
| `keyword` | string? | **2026-10-03 订正**：代码里已有（走 `part_keyword_search` 展开成 `part_id = ANY(...)`），本文档此前漏记。trim 后空串视为无过滤；**零命中返回 `items: []` / `total: 0`**（service 层兜住，与 `sent-parts` 同源 —— SQL 的 `AND (cardinality($N::bigint[]) = 0 OR part_id = ANY($N))` 里空 id 数组会让整个 keyword 条件短路） |
| `sort_by` | string? | **2026-10-03 订正**：代码里已有。`PRICE` / `REVIEWED_AT` / `CREATED_AT`，默认 `CREATED_AT`；非法值静默回落 |
| `sort_dir` | string? | **2026-10-03 订正**：代码里已有。`ASC` / `DESC`，默认 `DESC` |
| `limit` | i64? | 默认 50，clamp(1, 500) |
| `offset` | i64? | 默认 0，max(0, …) |

### OutsourceQuotablePartListQuery 字段（`GET /quotable-parts`）

| 字段 | 类型 | 说明 |
|---|---|---|
| `keyword` | string? | part 的 `drawing_no` / `name` ILIKE `%needle%`；trim 后空串视为无过滤 |
| `limit` | i64? | 默认 50，clamp(1, 500) |
| `offset` | i64? | 默认 0，max(0) |

### QuotablePartOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | `t_part.id` |
| `serial_no` | string? | |
| `drawing_no` | string | |
| `name` | string | |
| `is_urgent` | bool | 零件加急标记 |
| `unit_price` | string | 零件下单单价 Decimal 字符串（供与报价对比谈判空间） |
| `customer_id` | string (i64) | `t_part.customer_id` |
| `customer_name` | string? | L2（零件直属客户） |
| `l1_customer_name` | string? | L1（`t_customer.parent_id`） |
| `customer_path` | string? | 有 L1 拼 `L1 / L2`，仅 L2 时给 L2 名，无客户 `null` |

> **2026-10-03 简化**：原 VO 还带 `shelf_id` / `shelf_code` / `next_process_id` /
> `next_process_name` 四个字段（行粒度是「零件 × OUTSOURCE 工序」，靠「货架绑了哪些
> 外协工序」枚举）。行粒度收成「一零件一行」后这 4 个字段一并删除：报价工序由用户在
> 建报价时从 `category = 'OUTSOURCE'` 的工序列表里选，那个下拉本来就独立存在。

### QuotablePartListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `QuotablePartOut[]` | |
| `total` | i64 | 全量命中行数（去重后行粒度，见下） |
| `limit` | i64 | 回显（clamp 后） |
| `offset` | i64 | 回显 |

---

## 端点契约要点

### `quotable-parts` 的行粒度与筛选

**行 = 一个零件**（只要它有 PENDING 批次），同一零件无论几个批次都只出一行。
SQL 用 `DISTINCT ON (p.id)` 去重（内层 `ORDER BY p.id, pb.batch_no ASC` 取 batch_no
最小的批次做代表行；外层按展示序 `is_urgent DESC, planned_delivery_date ASC NULLS LAST,
id ASC` 排序）。`total` 的口径与 list 的 WHERE + `DISTINCT ON` 逐条一致。

筛选（2026-10-03 起 2 条）：

1. **`t_part.deleted_at IS NULL`**。
2. **存在 PENDING 批次**：`t_part_batch`（`pb.part_id = p.id AND pb.deleted_at IS NULL
   AND pb.status = 'PENDING'`）。业务口径是「报价是给**还没下发**的零件提前锁价」，
   所以已下发（在产 / 在外协）的零件不出现。

**不再参与筛选的三项**（2026-10-03 全部移除）：

- **货架 ↔ 工序映射**（`t_shelf_process` ⋈ `t_process`）：它要求批次已上架并绑定
  外协工序，而 picker 的目标恰恰是**还没下发**的零件。
- **工艺链求交**（`t_process_chain_step`）：`PENDING` 批次的
  `current_process_id` / `current_holder_id` 实测全为 NULL，且生产库里绝大多数零件
  没有 `process_chain_id`；叠加这条筛选后 picker 长期恒空。
- **`t_process.requires_approval`**：提前锁价与该工序是否需要审批无关。

> **与 `/outsource-sendable` 不再同源**：sendable 的判据是「批次停在某道外协工序上」，
> 两者谓词不同，不共享 SQL 常量，也不再要求口径一致。

### Lifecycle 守卫

- **create**：唯一性校验 → INSERT DRAFT。
- **update**：仅 DRAFT 可改价 / 数量；其它状态 → 21302。
- **submit**：DRAFT → SUBMITTED；状态机迁移由 `statemachine.rs` 守卫。
- **approve**：SUBMITTED → APPROVED；Manager-only（service 层 `require_role`）。
- **reject**：SUBMITTED → REJECTED；Manager-only；`review_note` 必填。
- **soft-delete**：DRAFT / REJECTED 可软删；SUBMITTED / APPROVED 拒绝（防审计丢失）。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901。
- `update` / `submit` / `approve` / `reject` / `soft-delete` 强制 OCC；`create` / `list` / `get` 不要求。

### 事务边界

- handler 层开 tx → 传 `&mut tx` 给 service → 显式 `tx.commit()`；失败时 `Transaction::drop` 自动回滚。
- `GET /quotable-parts` 是**读端点**：`pool.acquire()` **不开事务**。
- WS 广播在 `tx.commit().await?` **之后**；本域**无** WS 事件（同步经由 part 域 `send-to-outsource` 等端点）。

### 防 N+1

- `list_quotes` 单 SQL JOIN `t_outsource_company` + `t_process` 一次拿齐 company_name / process_name。
- `list_quotable_parts` **一条 SQL** 一次拿齐 part / 客户(L2+L1) 展示字段，
  `total` 走第二条同口径 `COUNT(*)`；service 层不循环查询。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_quotes / list_quotable_parts / create_quote / get_quote / update_quote / submit_quote / approve_quote / reject_quote / soft_delete_quote` + `quote_router()`
- service：`src/modules/outsource/service/quote.rs::OutsourceService::list_quotes / list_quotable_parts / …`
- repo：`src/modules/outsource/repo/{mod,sql}.rs`（`OutsourceQuotableRepo::list / count`）
- dto：`src/modules/outsource/dto.rs`
- vo：`src/modules/outsource/vo/quote.rs`（生命周期出参）+ `src/modules/outsource/vo/quotable.rs`（picker 读模型）
- model：`src/modules/outsource/model.rs::TOutsourceQuote + OutsourceQuoteStatus`
- 状态机：`src/modules/outsource/statemachine.rs`
- 路由挂载：`/outsource-quotes`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

- `tests/outsource/quote.rs`（9+ 用例：create DRAFT / 唯一性 21303 / update DRAFT happy / update SUBMITTED 21302 / submit / approve MANAGER-only / reject review_note 必填 / soft-delete 仅 DRAFT/REJECTED / **list keyword 零命中返 0 行**（带「无 keyword 返全量」对照组））
- `tests/outsource/quotable.rs`（8 用例：happy path（含 4 个已删字段的缺席断言）/ 无 PENDING 批次排除 / **无工艺链也出现** / 多 PENDING 批次去重 / PENDING+IN_PROCESS 混合 / 软删零件与软删批次排除 / keyword+分页 / 路由不被 `/{id}` 吞掉）
