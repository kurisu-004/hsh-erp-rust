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
- **唯一约束**：同 `(part_id, outsource_company_id, process_id)` 仅一个活跃报价（DB partial unique 兜底 → 21303 DUPLICATE）。实际是**两条谓词互斥的 partial unique**（2026-10-03 核对）：
  - `uq_t_outsource_quote_approved_part_process (part_id, process_id) WHERE deleted_at IS NULL AND status='APPROVED' AND is_direct=false` —— **审批报价**：每 (零件, 工序) 至多一条；
  - `uq_t_outsource_quote_direct_part_company_process (part_id, outsource_company_id, process_id) WHERE deleted_at IS NULL AND status='APPROVED' AND is_direct=true`（migration 008）—— **DIRECT 占位报价**：每 (零件, 公司, 工序) 至多一条。

  `is_direct = true` 的是 `prod::batch::send-to-outsource` 直发路径自动建的 **0 元占位报价**（`price=0` / `note` 写明 DIRECT 来源），**不是被人审批过的报价**。读侧的可发送闸门与写侧的 `requires_approval` 守卫（见 [`./outsource-sendable.md`](./outsource-sendable.md)）都以 `is_direct = false` 定义「真实审批报价」；漏掉这一维就会让 0 元占位报价冒充审批价发货。

---

## 共享错误码（213xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21301 | BIZ_OUTSOURCE_QUOTE_NOT_FOUND | 404 | 报价不存在 / 已软删 |
| 21302 | BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION | 400 | 当前状态不允许此操作（如 SUBMITTED 调 update） |
| 21303 | BIZ_OUTSOURCE_QUOTE_DUPLICATE | 409 | 同 `(part, company, process)` 已存在活跃报价 |
| 21307 | BIZ_OUTSOURCE_QUOTE_NOT_APPROVED | 400 | 报价非 `APPROVED`、或 `is_direct=true` 的 DIRECT 占位报价（`send-to-outsource` 拒收）、或 DIRECT 占位价并发回查失败 |

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
| `customer_id` | string (i64)? | 客户**子树**过滤：展开成「该客户自身 ∪ 其直接子客户」名下的 `part_id` 集合，再走 `part_id = ANY($4)`。**无需同时给 `keyword`**（2026-10-04 起；此前 service 解析完即丢弃，且「给了 `customer_id` 没给 `keyword`」直接返回空列表）。与 `keyword` 同时给时两者**取交集**；零命中返回 `items: []` / `total: 0` |
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

### `list` 的 `keyword` / `customer_id`：两个 part_id 集合求交

两个过滤维度各自在 service 层展开成一个 `part_id` 集合，再交给**同一条** repo SQL
（`quote_list_with_filters` / `quote_count_with_filters`）：

| 给了什么 | `part_ids` 取值 |
|---|---|
| 都没有 | 空数组（SQL 侧 `cardinality($4::bigint[]) = 0` 表示「不过滤」） |
| 只有 `keyword` | `part_keyword_search` 的结果（`drawing_no` / `name` ILIKE `%needle%`） |
| 只有 `customer_id` | `part_ids_by_customer` 的结果（客户子树） |
| 两个都有 | **交集**（`HashSet` 求交；两个源查询各自 `LIMIT 10000`，不能用嵌套循环） |

- `customer_id` 的子树形状与 `GET /outsource-sendable` 的 `customer_id` 谓词**逐字
  同形**（`customer_id = $1 OR customer_id IN (parent_id = $1 且未软删)`）：零件恒挂
  在 L2 客户上，前端选中的常是 L1，只判等值时 L1 必然零命中。等值那一支保留 ⇒
  传 L2 id 的请求行为不变。「展开一层即完整」依赖客户树严格两层，将来引入 L3 需改成
  递归 CTE。
- **零命中必须早返回空**：SQL 谓词 `AND (cardinality($4::bigint[]) = 0 OR
  part_id = ANY($4))` 里，空数组让 `cardinality = 0` 成立、整个过滤条件被短路掉。
  不在 service 层兜住，「一个零件都没有的客户」会返回**全量**报价。守卫的判定条件是
  「`keyword` 或 `customer_id` **任一**已给出且结果集为空」，两个维度都必须算进去。

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
- repo：`src/modules/outsource/repo/{mod,sql}.rs`（`OutsourceQuotableRepo::list / count`；`customer_id` 展开走 `OutsourceRepoTrait::part_ids_by_customer`，实现是 `repo/mod.rs` 里一行 `sqlx::query_as` 跨表 SELECT）
- dto：`src/modules/outsource/dto.rs`
- vo：`src/modules/outsource/vo/quote.rs`（生命周期出参）+ `src/modules/outsource/vo/quotable.rs`（picker 读模型）
- model：`src/modules/outsource/model.rs::TOutsourceQuote + OutsourceQuoteStatus`
- 状态机：`src/modules/outsource/statemachine.rs`
- 路由挂载：`/outsource-quotes`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

- `tests/outsource/quote.rs`（12 用例：create DRAFT / 唯一性 21303 / update DRAFT happy / update SUBMITTED 21302 / submit / approve MANAGER-only / reject review_note 必填 / soft-delete 仅 DRAFT/REJECTED / **list keyword 零命中返 0 行**（带「无 keyword 返全量」对照组）+ 4 条 `customer_id`）
  - `list_quotes_customer_id_l1_expands_to_children_without_keyword` — 只给 L1
    `customer_id` 即命中其全部 L2 子客户的报价，**不需 keyword**（曾恒返空）
  - `list_quotes_customer_id_l2_returns_only_its_own_quotes` — 传 L2 只回该 L2 的
  - `list_quotes_customer_id_and_keyword_intersection` — 两维度取交集；交集为空 → `total=0`
  - `list_quotes_customer_id_zero_match_returns_empty_not_all_rows` — 零命中的
    `customer_id` → `total=0` 而非全量（守 `cardinality($4)=0` 短路陷阱）
- `tests/outsource/quotable.rs`（8 用例：happy path（含 4 个已删字段的缺席断言）/ 无 PENDING 批次排除 / **无工艺链也出现** / 多 PENDING 批次去重 / PENDING+IN_PROCESS 混合 / 软删零件与软删批次排除 / keyword+分页 / 路由不被 `/{id}` 吞掉）

---

## 前端配套改动清单

> 口径与部署顺序同
> [`./outsource-sendable.md#前端配套改动清单`](./outsource-sendable.md#前端配套改动清单)
> （第 2 批 5 项硬切 + 「后端与前端必须同批上线」）。本域落在第 2 批的是这两项：

| # | 后端契约变更 | 前端必须同步改的点 | 漏改症状 |
|---|---|---|---|
| 1 | `GET /quotable-parts` 行粒度收成「一零件一行」，出参**删 4 个字段**：`shelf_id` / `shelf_code` / `next_process_id` / `next_process_name` | `QuotablePartOut` 类型 + Zod schema 去掉这 4 个字段（**必填声明也要一起去掉**，否则后端不返回时 `parse` 抛错）+ 表格去掉「货架 / 工序」两列 | 旧 schema 把已删字段声明为必填 ⇒ `parse` 抛错，报价一览页的 picker 整块白屏 |
| 2 | 报价生效判定改按 `is_direct = false`（0 元 DIRECT 占位报价不再满足审批闸门） | 无需改代码：DIRECT 行的 `send_mode` 由后端给出，前端不再自行按「有无 APPROVED 报价」推断模式 | 若前端仍自行推断，会对「只有占位报价」的场景误判成「已审批」 |

> 第 2 批其余 3 项（`next_process_*` → `current_process_*` 更名、`shelf_code` 可空、
> `send_mode` 语义 + 写侧 `requires_approval` 守卫）都在
> [`./outsource-sendable.md`](./outsource-sendable.md) 一侧，清单与部署顺序以该文件为
> 准，本节不重复。
