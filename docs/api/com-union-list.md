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
| `sort_by` | `string` | 否 | `CREATED_AT` | 排序键白名单：`CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `DRAWING_NO` / `NAME`。**注意**：`SERIAL_NO` 仅 `t_part` 独有 → ALL 模式降级 `CREATED_AT` |
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
        "child_count": null              // PART 行 null / 装配件子件数
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

## 引用

- 前端对应：`src/api/com/unionList.ts`（前端子模块另开 PR 接入；本端点路由
  自身即可工作）
- 集成测试：`tests/com/union_list.rs`（7 用例覆盖 PART / ASSEMBLY / ALL /
  SERIAL_NO 降级 / 非法值 / 缺省默认值 / deep offset 分页）
- API 设计文档：`docs/api/com-union-list.md`（本文件）
- 实现参考：`docs/plans/com-get-part-union-all-t-assembly-t-par-graceful-mitten.md`