# shelves 域 API

> 本文件须与 `src/modules/shelf/{handler.rs,dto.rs,service.rs,process_mapping.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/shelves` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（过滤 + 分页，含 `account_count`） |
| POST | `/api/v2/shelves` | MANAGER | 创建货架（PRODUCTION / INSPECTION） |
| GET | `/api/v2/shelves/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 货架详情（SHELF_ACCOUNT scope 校验） |
| POST | `/api/v2/shelves/{id}/update` | MANAGER | 部分更新（OCC） |
| POST | `/api/v2/shelves/{id}/deactivate` | MANAGER | 软删 + 停用（OCC，被 IN_PROCESS/INSPECTION/REPAIRING 零件引用时拒） |
| GET | `/api/v2/shelves/{id}/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 该货架的工序映射列表（按 sort_order） |
| GET | `/api/v2/shelves/for-return?next_process_id=` | 已登录（M/C/CNC/SHELF） | PRODUCTION 区 picker（按 current_load 升序，标 `is_recommended`） |
| GET | `/api/v2/shelves/for-inspection` | 已登录（M/C/CNC/SHELF/INSPECTOR） | INSPECTION 区 picker（仅 `zone='INSPECTION' AND is_active=true`） |
| GET | `/api/v2/shelves/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 所有 active shelf 的 mapping 批量查询（防 N+1） |
| POST | `/api/v2/shelves/{id}/processes` | MANAGER | 整组替换 mapping（先软删全部旧 → INSERT 新） |

挂载点：`/api/v2/shelves`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型

- **zone**: `'PRODUCTION'` / `'INSPECTION'`（DB varchar，应用层 enum 校验；其他值 → 20104）。
- **location**: 物理位置描述（货架所在通道/楼层），由 MANAGER 创建/更新时填写，可空。
- **is_active**: 是否启用。`deactivate` 同时把 `is_active = false` + `deleted_at = now()`
  写回（同事务）；`activate`（re-enable）不在本域实现（Python 没有此端点）。
- **display_order**: 物理顺序（0 = 未设置；manager 在 ShelfList 后台手填）。
- **account_count**: 绑定的 SHELF_ACCOUNT 角色数；`list_shelves` 单条 GROUP BY 批量补全。

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| 创建 | `code` 业务唯一（uk_t_shelf_code 活跃行唯一）；`code`/`name` 非空；`zone` ∈ {PRODUCTION, INSPECTION} |
| 更新 `code` | **不支持**（业务唯一键不可变；update 不暴露此字段） |
| 更新 `zone` | **不支持**（不暴露此字段；改 zone 走 soft-delete + 重建） |
| 更新 `name` | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| 更新 `location` | 三态：`None` 不改；`Some(null)` 清空；`Some(v)` 改 |
| 更新 `display_order` | None = 不改；Some(v) = 改 |
| 软删 | `t_part.current_holder_id = shelf_id` 且 `status IN ('IN_PROCESS','INSPECTION','REPAIRING')` 仍有非软删引用 → 20503 拒 |
| 映射 `set_shelf_processes` | 整组替换：先软删全部旧 mapping → INSERT 新（带 sort_order） |

---

### `GET /api/v2/shelves`

权限：已登录（M/C/CNC/SHELF/INSPECTOR；service 层 `require_any_role`）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code_like` | string | — | `ILIKE '%needle%'`；trim 后空串视为无过滤 |
| `zone` | string | — | 精确匹配（PRODUCTION / INSPECTION）；trim 后空串视为无过滤 |
| `is_active` | bool | — | 精确匹配；缺省 = 不过滤 |
| `limit` | i64 | — | 默认 50，clamp(1, 500) |
| `offset` | i64 | — | 默认 0，max(0, …) |

Response 200 `data`：`ShelfListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [ShelfOut] | 含 `account_count`（GROUP BY 单条批量算，防 N+1） |
| `total` | i64 | 全量命中行数 |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### `POST /api/v2/shelves`

权限：MANAGER（service 层 `require_role`）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | ✓ | trim 后非空；撞 `uk_t_shelf_code` → 20502 |
| `name` | string | ✓ | trim 后非空 |
| `zone` | string | ✓ | PRODUCTION 或 INSPECTION（大小写不敏感，规范化大写） |
| `location` | string? | — | trim 后空串视为 NULL |
| `display_order` | i32? | — | 默认 0 |

Response 201 `data`：`ShelfOut`

错误码：

- 20104 `BIZ_INVALID_VALUE` — code/name 空 / zone 不在 {PRODUCTION, INSPECTION}
- 20502 `BIZ_SHELF_DUPLICATE_CODE` — `uk_t_shelf_code` 撞唯一索引

### `GET /api/v2/shelves/{id}`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 货架 ID |

Response 200 `data`：`ShelfOut`

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND`
- 40301 `SHELF_MISMATCH` — SHELF_ACCOUNT 用户访问不在自己 shelf_ids 列表里的货架

### `POST /api/v2/shelves/{id}/update`

权限：MANAGER

Request（部分更新；与 Python `exclude_unset` 语义一致）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string? | — | None = 不改；Some(空串) = 20104；Some(非空) = 改 |
| `location` | string? \| null? | — | `None` 不改；`Some(null)` 清空；`Some(value)` 改值（trim 后写） |
| `display_order` | i32? | — | None = 不改；Some(v) = 改 |

Response 200 `data`：`ShelfOut`（回读最新版本）

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND`
- 20104 `BIZ_INVALID_VALUE` — name 空 / zone CHECK（理论 service 已 catch）
- 40901 `VERSION_CONFLICT` — 乐观锁冲突

### `POST /api/v2/shelves/{id}/deactivate`

权限：MANAGER

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 货架 ID |

Request：空 body

Response 200 `data`：`null`

语义：等同 soft-delete —— `is_active = false` 同时 `deleted_at = now()`（Python pattern）。
软删前查 `t_part.current_holder_id = shelf_id` 且 `status IN ('IN_PROCESS','INSPECTION','REPAIRING')`
引用数（单条 `UNION ALL` 累加 3 个 sub-SELECT）。

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND`
- 20503 `BIZ_SHELF_IN_USE` — 仍有非软删引用 → 拒
- 40901 `VERSION_CONFLICT`

### `GET /api/v2/shelves/for-return?next_process_id=`

权限：已登录（M/C/CNC/SHELF）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `next_process_id` | string (i64) | — | 候选的下一道工序 id；若传必须现存（否则 20801） |

Response 200 `data`：`ShelfForReturnOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].id` | string (i64) | |
| `items[].code` | string | |
| `items[].name` | string | |
| `items[].zone` | string | |
| `items[].location` | string? | |
| `items[].current_load` | i64 | LEFT JOIN t_part_batch 聚合（status IN (PENDING/IN_PROCESS/INSPECTION/REPAIRING/OUTSOURCE) 的批次 quantity 总和） |
| `items[].is_recommended` | bool | `current_load` 最小的第一条 = `true`，其余 `false` |

业务规则：

- 仅返回 `zone='PRODUCTION' AND is_active=true AND deleted_at IS NULL`
- SHELF_ACCOUNT 用户仅看到 `user.shelf_ids` 绑定的架（用 `can_access_shelf`）；Manager 见全集
- `current_load` LEFT JOIN 聚合：空载货架 = 0（保留在结果中）

错误码：

- 20104 `BIZ_INVALID_VALUE` — next_process_id 非整数
- 20801 `BIZ_PROCESS_NOT_FOUND` — next_process_id 不存在

### `GET /api/v2/shelves/for-inspection`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）

Response 200 `data`：`ShelfForInspectionOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[]` | `ShelfForInspectionItem` | `id` / `code` / `name` / `zone` / `location` / `is_active` |

业务规则：

- 仅 `zone='INSPECTION' AND is_active=true AND deleted_at IS NULL`
- 不过滤 SHELF_ACCOUNT scope（品检架全员可见）

### `GET /api/v2/shelves/processes`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）

Response 200 `data`：`AllShelfProcessMappingOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].shelf_id` | string (i64) | |
| `items[].shelf_code` | string | |
| `items[].process_id` | string (i64) | |
| `items[].process_code` | string | |

业务规则：

- 单条 JOIN 返回所有 active shelf ↔ process 行（防 N+1）
- SHELF_ACCOUNT 用户仅看到 `user.shelf_ids` 命中的映射
- 用途：part_batch / worker_pool 创建批次/工人时一次性拿全货架工序映射

### `GET /api/v2/shelves/{id}/processes`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）+ SHELF_ACCOUNT scope 校验

Response 200 `data`：`ShelfProcessMappingOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].shelf_id` | string (i64) | |
| `items[].shelf_code` | string | |
| `items[].process_id` | string (i64) | |
| `items[].process_code` | string | |
| `items[].sort_order` | i32 | |

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND`
- 40301 `SHELF_MISMATCH` — SHELF_ACCOUNT 越界

### `POST /api/v2/shelves/{id}/processes`

权限：MANAGER

Request：`SetShelfProcessesRequest`

```jsonc
{
  "items": [
    { "process_id": "1001", "sort_order": 0 },
    { "process_id": "1002", "sort_order": 1 }
  ]
}
```

语义：整组替换 —— 事务内：

1. 校验 shelf 存在（20501）+ items 里所有 process_id 存在（20505）
2. 软删该 shelf 的全部 active mapping（清 deleted_at）
3. `bulk_insert` 新 mapping（带 sort_order）

`items` 可为 `[]`（清空映射）。

Response 200 `data`：`null`

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND` —— shelf 不存在 / 已软删
- 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
- 20104 `BIZ_INVALID_VALUE` —— process_id 非整数

---

## DTO 字段参考

`ShelfOut`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（JSON 字符串） |
| `code` | string | 业务唯一键；创建后不可改 |
| `name` | string | |
| `zone` | string | PRODUCTION / INSPECTION |
| `location` | string? | 物理位置；可空 |
| `is_active` | bool | 启用 / 停用 |
| `display_order` | i32 | 物理顺序 |
| `account_count` | i64 | 绑定的 SHELF_ACCOUNT 角色数；`get_shelf` 单条默认 0，list 批量 GROUP BY 补 |
| `version` | i32 | 乐观锁；每次写操作 +1 |
| `created_at` | naive datetime | Asia/Shanghai |
| `updated_at` | naive datetime | Asia/Shanghai |

---

## 跨模块引用

- `part::service` 用 `ShelfRepo::get_by_id` / `get_active_by_id` / `get_by_id_zone`
  做品检架/生产架校验 —— `TShelf` 已扩展 `location` 字段，但 part 域不读，不影响。
- `process::service::soft_delete_process` 的 `count_process_references` 用 `t_shelf_process` 查引用。
- `user::repo::ShelfRepo::get_by_id` 是 user 域自带的只读投影，与本域并存；不共享代码。