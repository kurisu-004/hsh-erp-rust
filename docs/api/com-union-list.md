# `GET /api/v2/com/union-list` — 跨表合并视图

**域**：`com`（Customer Order Management）
**稳定版本**：2026-09-29 新增（`feat(com): 新增 union-list 端点`）
**URL**：`/api/v2/com/union-list`
**Handler**：`src/modules/com/union_list/handler.rs::list_union_items`
**Service**：`src/modules/com/union_list/service/crud.rs::UnionListService::list_union_items`

## 概述

跨 `t_part` + `t_assembly` 两表的合并视图端点。替代原 `GET /api/v2/parts` 的三态
`row_type` 矩阵（ALL / PART / ASSEMBLY），下沉到 com 域并修复原分页 bug。

**为什么独立端点而非 part 域内嵌**：
- part 域只查 `t_part`，跨 `t_assembly` 合并越界
- 原 ALL 模式 `segment_limit.clamp(1,200)` 在 deep offset 返回空集（plan §3）
- ALL 模式 SQL UNION ALL + 每段 pushdown 修分页

## 路径

```
GET /api/v2/com/union-list
```

## 鉴权

| 角色 | 允许 |
|---|---|
| Manager | ✅ |
| Clerk | ✅ |
| Inspector | ✅ |
| CncProgrammer | ✅ |
| Worker | ❌ 40300 |

权限字面值定义在 `handler.rs::LIST_UNION_ROLES`（4 角色全开放，与 `part/handler/crud.rs::LIST_PART_ROLES` 同形）。

## Query 参数

| 参数 | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `row_type` | `string` | 否 | `"ALL"` | 行类型过滤：`"ALL"` / `"PART"` / `"ASSEMBLY"`。非法值 → `40001 VALIDATION_ERROR` |
| `customer_id` | `i64` (string) | 否 | — | 客户 id（雪花 ID 字符串）；service 层展开 L1+L2 ids |
| `status` | `string` | 否 | — | 单状态筛选（如 `"PENDING"`） |
| `statuses` | `string` | 否 | — | 多状态逗号分隔（如 `"PENDING,IN_PROCESS"`） |
| `is_urgent` | `bool` | 否 | — | 是否加急 |
| `keyword` | `string` | 否 | — | 模糊匹配（name / drawing_no / serial_no 三列 ILIKE OR） |
| `locations` | `string` | 否 | — | 逗号分隔位置白名单（OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF / OUTSOURCE_COMPANY）。**PART / ALL 模式生效**，ASSEMBLY 模式忽略（`t_assembly` 无 batch 派生字段） |
| `holder_ids` | `string` | 否 | — | 逗号分隔雪花 ID。**PART / ALL 模式生效**；ASSEMBLY 模式忽略 |
| `planned_delivery_date_from` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：日期窗口下界 `planned_delivery_date >= $from`。**PART / ALL / ASSEMBLY 三模式全部生效**。非法格式 → `40001 VALIDATION_ERROR` |
| `planned_delivery_date_to` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：日期窗口上界 `planned_delivery_date <= $to`。**PART / ALL / ASSEMBLY 三模式全部生效**。非法格式 → `40001 VALIDATION_ERROR`。任一端缺失 → 对应 NULL 短路 |
| `sort_by` | `string` | 否 | `CREATED_AT` | 排序键白名单：`CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `DRAWING_NO` / `NAME` / `SYSTEM_DELIVERY_DATE`。**注意**：`SERIAL_NO` 仅 `t_part` 独有 → ALL 模式降级 `CREATED_AT` |
| `drawing_no` | `string` | 否 | — | 2026-09-30 新增：图号 ILIKE 模糊（`%x%`，已 trim + 预格式化）。**PART / ALL / ASSEMBLY 三模式全部生效**。空串 / 纯空白 → 不参与过滤 |
| `name` | `string` | 否 | — | 2026-09-30 新增：名称 ILIKE 模糊（`%x%`）。**PART / ALL / ASSEMBLY 三模式全部生效** |
| `order_no` | `string` | 否 | — | 2026-09-30 新增：订单号 ILIKE 模糊（`%x%`，t_part.order_no / t_assembly.order_no 均为 nullable varchar(30)）。**PART / ALL / ASSEMBLY 三模式全部生效** |
| `serial_no` | `string` | 否 | — | 2026-09-30 新增：序列号 ILIKE 模糊（`%x%`，t_part.serial_no / t_assembly.serial_no 均为 nullable varchar(15)）。**PART / ALL / ASSEMBLY 三模式全部生效** |
| `request_date_from` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：请求日期窗口下界 `request_date >= $from`。**PART / ALL / ASSEMBLY 三模式全部生效**（t_part.request_date / t_assembly.request_date 均 NOT NULL）。非法格式 → `40001 VALIDATION_ERROR` |
| `request_date_to` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：请求日期窗口上界 `request_date <= $to`。**PART / ALL / ASSEMBLY 三模式全部生效**。任一端缺失 → 对应 NULL 短路 |
| `system_delivery_date_from` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：系统交期窗口下界 `system_delivery_date >= $from`。**PART / ALL / ASSEMBLY 三模式全部生效**（t_part.system_delivery_date / t_assembly.system_delivery_date 均为 nullable date，普通 `>=`/`<=` 对 NULL 直接 false） |
| `system_delivery_date_to` | `string` (`YYYY-MM-DD`) | 否 | — | 2026-09-30 新增：系统交期窗口上界 `system_delivery_date <= $to`。**PART / ALL / ASSEMBLY 三模式全部生效**。任一端缺失 → 对应 NULL 短路 |
| `order_no_is_null` | `bool` | 否 | — | 2026-09-30 新增：订单号 IS NULL 三态过滤。`true` → `order_no IS NULL OR order_no = ''`（含空串语义对齐 PR-F 2026-08-11『空串视为未填』）；`false` → `order_no IS NOT NULL AND order_no <> ''`；省略 → 不参与。**PART / ALL / ASSEMBLY 三模式全部生效** |
| `system_delivery_date_is_null` | `bool` | 否 | — | 2026-09-30 新增：系统交期 IS NULL 三态过滤。`true` → `system_delivery_date IS NULL`；`false` → `system_delivery_date IS NOT NULL`；省略 → 不参与。**PART / ALL / ASSEMBLY 三模式全部生效** |
| `sort_dir` | `string` | 否 | `DESC` | `"ASC"` / `"DESC"` |
| `limit` | `i64` | 否 | `50` | `[1, 200]` |
| `offset` | `i64` | 否 | `0` | `>= 0` |

### `row_type` 语义矩阵

| `row_type`         | 行为 |
|--------------------|-------------------------------------------------|
| `"PART"`           | 仅 `t_part WHERE assembly_id IS NULL`（装配体子件被守卫排除） |
| `"ASSEMBLY"`       | 仅 `t_assembly`（投影为 `PartListItem` 形态） |
| `"ALL"` / 缺省 / `""` | `t_part` UNION ALL `t_assembly` + 每段 `LIMIT (offset+limit)` pushdown |
| 其它非空字符串      | `40001 VALIDATION_ERROR`（HTTP 422） |

## 响应

`PartListOut`（与 `GET /api/v2/parts` 同形 VO；见 `src/modules/part/vo/part.rs`）。

```json
{
  "code": 0,
  "data": {
    "items": [
      {
        "id": "1234567890123456789",      // 雪花 ID（字符串）
        "serial_no": "P0001234",
        "name": "test part",
        "drawing_no": "D-TEST-001",
        "applicant_name": "张三",
        "quantity": 5,
        "request_date": "2026-09-29",
        "planned_delivery_date": "2026-10-15",
        "customer_id": "9876543210987654321",
        "assembly_id": null,             // PART 行 None / 装配件子件 None
        "status": "PENDING",
        "is_urgent": false,
        "order_no": null,
        "system_delivery_date": null,
        "note": null,
        "unit_price": "0.00",
        "total_price": "0.00",
        "version": 0,
        "created_at": "2026-09-29T10:30:00",
        "created_by": "1111111111111111111",
        "updated_at": "2026-09-29T10:30:00",
        "updated_by": null,
        "deleted_at": null,
        "process_chain_id": null,
        "customer_name": "客户 L2",
        "l1_customer_name": "客户 L1",
        "location": "PRODUCTION_SHELF",   // PART 行派生 / 其它 None
        "holder_name": "A1",             // PART 行派生 / 其它 None
        "row_type": "PART",              // "PART" / "ASSEMBLY"（与请求 row_type 一致）
        "has_children": false,           // PART 行 false / 装配件按 child_count
        "child_count": null,             // PART 行 null / 装配件子件数
        "delivered_quantity": 0          // PART 行已送数量 / 装配件已送套数
      }
    ],
    "total": 5,
    "limit": 50,
    "offset": 0
  }
}
```

**派生字段规则**：

| 字段 | PART 行 | ASSEMBLY 行 |
|---|---|---|
| `location` | min-progress 活跃批次 location；无活跃批次 → `None` | `None` |
| `holder_name` | 按 location 分桶解析（`t_shelf.code` / `t_worker.name` / `t_outsource_company.name`） | `None` |
| `has_children` | `false` | `child_count.unwrap_or(0) > 0` |
| `child_count` | `None` | 子件数（`t_part WHERE assembly_id = $1 AND deleted_at IS NULL` 的 COUNT） |
| `customer_name` / `l1_customer_name` | 派生 | 派生 |
| `assembly_id` | 装配件子件 → `Some(id)`；顶层零件 → `None` | `None`（顶层装配件无父） |
| `process_chain_id` | 直接搬 | `None`（t_assembly 无此列） |
| `batch_id` / `batch_version` | **`None`（恒 `null`，不填）** | **`None`（恒 `null`，不填）** |
| `delivered_quantity` | 未软删批次中 `status ∈ ('DELIVERED', 'COMPLETED')` 的 `quantity` 之和（零批次 → `0`） | 可凑齐的套数 `MIN(子件已送件数 × 装配件套数 / 子件总量)`，整数除法截断；子件总量为 0 者不参与，无子件 → `0` |

> ⚠️ `delivered_quantity`（2026-10-03 新增）：真相源是 `t_part_batch.status`
> （批次级「已交」的唯一依据，**不**从派生缓存 `t_part.status` 反推 —— 后者在
> min-progress 规则下只有全部活跃批次都 DELIVERED 才等于 DELIVERED，会把「部分已交」
> 一律压成 0）。两种 row_type 模式都填该字段（三态矩阵里 PART / ASSEMBLY / ALL
> 各接一次聚合查询），故 `delivered_quantity` 恒为非 null 数字。
> 与 `GET /api/v2/parts` 的同名字段口径逐字一致（同一对 helper）。
> 完整字段表见 [`./parts/index.md#partlistitem-字段`](./parts/index.md#partlistitem-字段)。

> ⚠️ `batch_id` / `batch_version` 是 2026-10-03 给 `PartListItem` 加的两个可选字段
> （全仓**仅** `GET /parts/pickable-by-work-type/{work_type_id}` 填）。本端点
> **刻意不填**：union-list 的行单位是 part（ASSEMBLY 行连 part 都不是），一个 part
> 的活跃批次可能不止一个，填任一活跃批次都是错锚点，故两字段恒为 `null`。
> 完整字段表见 [`./parts/index.md#partlistitem-字段`](./parts/index.md#partlistitem-字段)。

## 状态码

| HTTP | code | 触发 |
|---|---|---|
| 200 | 0 | 成功 |
| 422 | 40001 | 非法 `row_type` / `holder_ids` 雪花 ID 解析失败 |
| 401 | 40105 | JWT 缺失 / 失效 / 吊销 |
| 403 | 40300 | 角色不在 4 角色白名单 |
| 5xx | 50001+ | DB 错误 / 其它 server-side failure |

## SQL 策略（plan §3）

### ALL 模式 pushdown + UNION

```sql
WITH
  part_seg AS (
    SELECT <22 列 + next_process_id + 'PART'::text AS row_type>
    FROM t_part
    WHERE deleted_at IS NULL AND assembly_id IS NULL
      /* 共用筛选：customer_ids / status / statuses / is_urgent / keyword */
      /* 额外：locations / holder_ids 走 EXISTS t_part_batch */
    ORDER BY <sort_col> NULLS LAST, id DESC
    LIMIT $pushdown_limit OFFSET 0
  ),
  asm_seg AS (
    SELECT <22 列 + NULL::bigint AS assembly_id + 'ASSEMBLY'::text AS row_type>
    FROM t_assembly
    WHERE deleted_at IS NULL
      /* 共用筛选（与 part 段同）*/
    ORDER BY <sort_col> NULLS LAST, id DESC
    LIMIT $pushdown_limit OFFSET 0
  )
SELECT *
FROM (
  SELECT * FROM part_seg
  UNION ALL
  SELECT * FROM asm_seg
) AS u
ORDER BY <sort_col> NULLS LAST, id DESC
LIMIT $limit OFFSET $offset;
```

**正确性论证**：每段内部按各自 sort_key 取前 `pushdown_limit = offset + limit` 行；
外层 UNION ALL 后再做全局排序 + 分页，位置 `[offset, offset+limit)` 内的任何行必
然来自某段的前 `(offset+limit)` 行（否则它不会进入 top-N），所以结果正确。

**修分页 bug**：原 ALL 模式 `segment_limit.clamp(1,200)` 在 deep offset 返回空集
（拉回的 200+200=400 行内存排序后 `skip(offset).take(limit)` 跳过数据）；pushdown
保证每段拉够 `offset+limit` 行再外层切片。

**索引命中**：
- `customer_id` / `status` 同时命中 `ix_t_part_customer_status_delivery` /
  `ix_t_assembly_customer_status`（DDL 见
  `migrations/20260811100005_005_create_part_tables.sql`）
- `keyword` 走 ILIKE 三列 OR（name / drawing_no / serial_no），DDL 上无
  trigram 索引，单段可能 seq scan；客户筛选缩窄后命中索引覆盖

### PART / ASSEMBLY 单段模式

直走现成 repo：
- PART：`part/repo/sql/part_sql.rs::list_with_filters`（`part_only=true` 强写
  守卫）+ `count_with_filters`
- ASSEMBLY：`assembly/repo/sql.rs::list_with_filters` + `count_with_filters`

不需 pushdown（本身就是单表）。

### count 策略

- ALL：`part_total + asm_total`（两次 `count_with_filters`），不查 union 表
  （union 计数代价高）
- PART：`PartRepo::count_with_filters(part_only=true)`
- ASSEMBLY：`AssemblyRepo::count_with_filters`

## 示例

### ALL 模式 + 客户筛选

```bash
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=ALL" \
  --data-urlencode "customer_id=9876543210987654321" \
  --data-urlencode "limit=50" \
  --data-urlencode "offset=0"
```

响应：
```json
{
  "code": 0,
  "data": {
    "items": [
      {"id": "1111", "row_type": "PART", ...},
      {"id": "2222", "row_type": "ASSEMBLY", "has_children": true, "child_count": 3, ...}
    ],
    "total": 12,
    "limit": 50,
    "offset": 0
  }
}
```

### PART 模式 + 位置过滤

```bash
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=PART" \
  --data-urlencode "locations=PRODUCTION_SHELF,WORKER"
```

响应：
```json
{
  "code": 0,
  "data": {
    "items": [
      {"id": "1111", "row_type": "PART", "location": "PRODUCTION_SHELF", ...}
    ],
    "total": 3,
    "limit": 50,
    "offset": 0
  }
}
```

### ASSEMBLY 模式

```bash
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=ASSEMBLY"
```

响应：
```json
{
  "code": 0,
  "data": {
    "items": [
      {"id": "5555", "row_type": "ASSEMBLY", "has_children": true, "child_count": 2, ...}
    ],
    "total": 1,
    "limit": 50,
    "offset": 0
  }
}
```

### 非法 `row_type`

```bash
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=BAD"
```

响应（HTTP 422）：
```json
{
  "code": 40001,
  "message": "row_type 非法: BAD（必须是 PART / ASSEMBLY / ALL 或省略）"
}
```

### 日期窗口过滤（2026-09-30 新增）

修前端 dashboard UpcomingDeliveryListDrawer 隐藏 bug —— 该 Drawer 早传 `planned_delivery_date_from/to`
但本端点 DTO 之前无对应字段，参数被静默丢弃。本切片把两字段正式纳入 DTO + service
parse（`YYYY-MM-DD` → `NaiveDate`，非法 → 40001）+ repo SQL 段内 `>=`/`<=` 过滤。

```bash
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=ALL" \
  --data-urlencode "planned_delivery_date_from=2026-09-30" \
  --data-urlencode "planned_delivery_date_to=2026-10-07"
```

响应（仅返回 `planned_delivery_date ∈ [2026-09-30, 2026-10-07]` 的行）：
```json
{
  "code": 0,
  "data": {
    "items": [
      {"id": "1111", "row_type": "PART", "planned_delivery_date": "2026-10-01", ...},
      {"id": "2222", "row_type": "ASSEMBLY", "planned_delivery_date": "2026-10-05", "has_children": true, ...}
    ],
    "total": 2,
    "limit": 50,
    "offset": 0
  }
}
```

非法日期格式（HTTP 422）：
```json
{
  "code": 40001,
  "message": "planned_delivery_date_from 非法: not-a-date（必须是 YYYY-MM-DD: ...）"
}
```

### 文本+日期+IS NULL 三态筛选（2026-09-30 新增）

修零件一览页面（frontend `PartsTable.vue` / `usePartsListQuery.ts::buildParams()`）
隐藏 bug —— 该页面照常发出 10 个字段（4 文本 ILIKE + 4 日期窗口 + 2 IS NULL
三态），但本端点 DTO 之前无对应字段，参数被 axum `Query<T>` 静默丢弃；表现：
用户在图号/名称/订单号/序列号筛选框输入值、请求日期/系统交期选区间、『订单号
是否为空』『系统交期是否为空』下拉切换 —— 全部失效。

本切片把 10 字段正式纳入 DTO + service parse + repo SQL 段内 WHERE，三层修复：
1. **DTO 声明**（`src/modules/com/union_list/dto.rs::UnionListQuery`）：加 10 字段
   `#[serde(default)]` 防止 axum `Query<T>` 静默丢弃（这部分就是 bug 根因）。
2. **service 解析**（`parse_filters`）：4 文本走 `parse_optional_ilike_pattern`
   helper（None / Some("") / 纯空白 → None；其它 → `Some(format!("%{}%", raw.trim()))`）；
   4 日期复用 `parse_optional_date` helper（YYYY-MM-DD → NaiveDate，非法 →
   40001）；2 IS NULL bool 直传。
3. **repo SQL 段内消费**（UNION ALL SQL format!）：4 文本扩 `$13..$16` 占位 +
   段内 `WHERE <col> ILIKE $N`；4 日期扩 `$17..$20` 占位 + 段内 `WHERE <col> >= / <= $N`；
   2 IS NULL 三态条件预生成 SQL 字符串片段（`""` / `" AND (...)"`）拼到 format!
   字符串里，**不增加 `$N` 占位**（避免 `$N::bool` 多占位污染 plan cache）。

字段语义：
- **4 文本 ILIKE 模糊**：`drawing_no` / `name` / `order_no` / `serial_no`。
  t_part.drawing_no / name NOT NULL；t_assembly 同列 NOT NULL；
  t_part.order_no / serial_no 与 t_assembly 同列均为 nullable。
  None / 空串 / 纯空白 → 不参与过滤。
- **4 日期窗口**：`request_date_from/to` + `system_delivery_date_from/to`。
  t_part.request_date / t_assembly.request_date 均 NOT NULL（SQL `>=`/`<=`
  直接生效）；t_part.system_delivery_date / t_assembly.system_delivery_date 均为
  nullable date，普通 `>=`/`<=` 对 NULL 直接 false 故 NULL 被短路排除；如要命中
  NULL 行用 `system_delivery_date_is_null=true` 显式筛。
- **2 IS NULL 三态**：`order_no_is_null` + `system_delivery_date_is_null`。
  - `None`：不参与过滤
  - `Some(true)`：`order_no` → `IS NULL OR = ''`（含空串语义对齐 PR-F
    2026-08-11『空串视为未填/与 NULL 同义』）；`system_delivery_date` → `IS NULL`
  - `Some(false)`：`order_no` → `IS NOT NULL AND <> ''`；
    `system_delivery_date` → `IS NOT NULL`

破坏性变更（仅内域）：
- `PartListFilters` / `AssemblyListFilters` 各加 10 字段（与日期窗口同位置追加）；
  所有 caller（part 域 list / 外协 / assembly 域 trait impl）固定传 `None` 维持
  旧行为，零破坏。
- `UnionListRepo::list_union_all_with_filters` / `UnionListRepoTrait::list_union_all_with_filters`
  各加 10 扁平形参；仅 com::union_list 端点直接调用，零破坏。

PART / ALL / ASSEMBLY 三模式全部生效（UNION ALL SQL `part_seg` / `asm_seg` 两段
都追加同 `$13..$20` 守卫，外层 SQL 不消费这两个 placeholder 故不影响 `$9`/`$10`）。

测试覆盖：11 个新增 union-list 用例（4 文本 + 2 日期 + 4 IS NULL + 1 combined smoke）+ 1
非法日期格式 + 2 老端点兼容回归（`/parts` + `/assemblies`）= 共 14 个新增测试。
2026-10-03 再加 10 个 `delivered_quantity_*` 用例（PART 行 5 个 + ASSEMBLY 行 4 个
+ `GET /parts` 口径一致 1 个）。

```bash
# 4 文本 + 4 日期 + 2 IS NULL 三态全开（11 个新增用例之一：combined smoke）
curl -G "http://localhost:3000/api/v2/com/union-list" \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode "row_type=ALL" \
  --data-urlencode "drawing_no=D-COMB" \
  --data-urlencode "name=P-COMB" \
  --data-urlencode "order_no=ORD-COMB" \
  --data-urlencode "serial_no=SN-COMB" \
  --data-urlencode "request_date_from=2026-09-30" \
  --data-urlencode "request_date_to=2026-10-03" \
  --data-urlencode "system_delivery_date_from=2026-09-30" \
  --data-urlencode "system_delivery_date_to=2026-10-03" \
  --data-urlencode "order_no_is_null=false" \
  --data-urlencode "system_delivery_date_is_null=false"
```

## 引用

- 前端对应：`src/api/com/unionList.ts`（前端子模块另开 PR 接入；本端点路由
  自身即可工作）
- 集成测试：`tests/com/union_list.rs`（20 用例覆盖 PART / ASSEMBLY / ALL /
  SERIAL_NO 降级 / 非法 row_type / 缺省默认值 / deep offset 分页 / 日期窗口过滤
  / **10 字段筛选（4 文本 ILIKE + 4 日期 + 2 IS NULL）+ combined smoke + 非法
  日期格式**；2026-10-03 新增 10 个 `delivered_quantity_*` 用例覆盖 PART 行
  口径（只累加 DELIVERED / COMPLETED、排除软删与 CANCELLED、零批次为 0 且键恒在）、
  ASSEMBLY 行套数 min 公式（含子件总量 0 不参与、无子件为 0、补交后 min 变化）
  与 `GET /parts` 端点的口径一致性）
  + `tests/part/crud.rs::list_parts_old_endpoint_ignores_new_union_list_fields`
  + `tests/assembly/api.rs::list_assemblies_with_compat_union_list_fields` 兼容回归
- API 设计文档：`docs/api/com-union-list.md`（本文件）
- 实现参考：`docs/plans/com-get-part-union-all-t-assembly-t-par-graceful-mitten.md`