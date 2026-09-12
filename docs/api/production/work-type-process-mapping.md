# work_type ↔ process 工序映射 域 API

> 本文件须与 `src/modules/work_type/{handler.rs,process_mapping.rs,service.rs,repo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：`t_work_type_process` 多对多映射的读写。工种 CRUD 见 [`./work-types.md`](./work-types.md)；工序 CRUD 见 [`./processes.md`](./processes.md)。

> **导航**：[`← index`](./index.md) · [`work-types`](./work-types.md) · [`processes`](./processes.md) · [`process-chain`](./process-chain.md) · [`worker-pool`](./worker-pool.md) · **work-type-process-mapping**

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/work-types/{id}/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 该工种已映射工序列表 |
| POST | `/api/v2/work-types/{id}/processes` | MANAGER | 整组替换工种工序映射 |

挂载点：`/api/v2/work-types`（见 `src/modules/mod.rs::v2_router`）。

---

## 业务模型

`t_work_type_process` 是无业务软删的 mapping 表（worker-pool / worker-scan 校验依赖），
记录「某工种可执行哪些工序」及其显示顺序。

- **无外键**：`work_type_id` / `process_id` 为 bigint 逻辑引用，DB 层无 FK 约束
- **无软删**：mapping 行无 `deleted_at` 字段；update 走「整组替换」语义（先清空再 bulk_insert）
- **业务软删检测**：删除工种时（见 [work-types.md](./work-types.md) `POST /work-types/{id}/soft-delete`），
  service 用 `count_work_type_references` 查 `t_work_type_process.work_type_id` 任一 > 0 ⇒ 20903 拒
- **`process_ids` 出参**：工种 list / detail 接口会通过 `WorkTypeProcessRepo::list_by_work_types_batch`
  单条 SQL 批量补全 `WorkTypeOut.process_ids`（防 N+1）

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| `GET /processes` | 工种不存在 → 20901；按 `sort_order ASC, id ASC` 返回 |
| `POST /processes` | 工种不存在 → 20901；items 里有 process_id 不存在或已软删 → 20801；整组替换语义 |

---

### `GET /api/v2/work-types/{id}/processes`

权限：已登录（M/C/CNC/SHELF/INSPECTOR；service 层 `require_any_role`）

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工种 ID |

Response 200 `data`：`WorkTypeProcessMappingOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [WorkTypeProcessMappingItem](#worktypeprocessmappingitem-字段) | 按 `sort_order ASC, id ASC` |

### `POST /api/v2/work-types/{id}/processes`

权限：MANAGER

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工种 ID |

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [SetWorkTypeProcessesItem](#setworktypeprocessesitem-字段) | — | 空数组 = 清空全部 mapping；每个 `{process_id, sort_order}` 的 `process_id` 必须现存 |

`SetWorkTypeProcessesItem`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `process_id` | string (i64) | 雪花 ID 字符串；不存在 → 20801 `BIZ_PROCESS_NOT_FOUND` |
| `sort_order` | i32 | 显示顺序 |

Response 200 `data`：`null`

错误码：

- 20901 `BIZ_WORK_TYPE_NOT_FOUND`
- 20801 `BIZ_PROCESS_NOT_FOUND` — items 里有 process_id 不存在或已软删

---

## DTO 字段参考

### WorkTypeProcessMappingItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `work_type_id` | string (i64) | |
| `process_id` | string (i64) | |
| `process_code` | string | JOIN t_process 取 |
| `sort_order` | i32 | |

### SetWorkTypeProcessesItem 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | string (i64) | ✓ | 雪花 ID 字符串；不存在 → 20801 |
| `sort_order` | i32 | ✓ | 显示顺序 |

### WorkTypeProcessMappingOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [WorkTypeProcessMappingItem](#worktypeprocessmappingitem-字段) | 按 `sort_order ASC, id ASC` |

---

## 维护约定

1. `process_ids` 在 `list_work_types` / `get_work_type` 两处由
   `WorkTypeProcessRepo::list_by_work_types_batch` 单条 SQL 批量补全；
   `create_work_type` 故意**不**补（创建时不传 mapping，留空数组即可）。
2. `set_work_type_processes` 走「整组替换」语义：先 `soft_delete_all_for_work_type` →
   `bulk_insert`。空 `items` = 清空全部 mapping（仍走事务）。
3. 软删引用计数（`count_work_type_references`）单条 `UNION ALL` 查
   `t_worker.work_type_id`（活跃行）+ `t_work_type_process.work_type_id`（mapping 表
   无业务软删，不筛 `deleted_at`）；任一分支 > 0 ⇒ 20903 拒（**该逻辑在 work_types 域 soft-delete 端点，详见 [`./work-types.md`](./work-types.md#post-apiv2work-typesidsoft-delete)**）。
