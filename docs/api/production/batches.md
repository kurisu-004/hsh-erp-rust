# prod::batch 域 API —— 车间 PENDING 批次列表 + 下发

> 本文件须与 `src/modules/prod/batch/{handler.rs,dto.rs,service.rs,repo.rs,vo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**车间下发** PENDING 批次专用域 —— UI「待下发队列」展示 + 一键 / 批量 / 自动下发 3 路径。
> 2026-09-29 新增；URL 挂 `/api/v2/prod/batches/*`；零 schema 变更（复用既有 `t_shelf_process`）。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/batches/pending` | **Manager+Clerk+Inspector** | 车间 PENDING 批次列表（JOIN 工单 + 客户 L1+L2 + 申请人） |
| POST | `/api/v2/prod/batches/dispatch` | **Manager+Clerk** | 单 batch 下发（PENDING → IN_PROCESS + 上架 + 写事件） |
| POST | `/api/v2/prod/batches/bulk-dispatch` | **Manager+Clerk** | 批量下发（单事务；任一失败 → 全回滚） |
| POST | `/api/v2/prod/batches/auto-dispatch` | **Manager+Clerk** | 自动下发（按 part.process_chain 首道 step 推导 target_process；NO_PROCESS_CHAIN / NO_PROCESS_STEP 跳过不报错） |

> 路由挂载：`prod::mod::router().nest("/batches", batch::router())` —— 见 `src/modules/prod/mod.rs`。

---

## 共同设计要点

### 货架解析（零 schema 变更）
`target_process_id` → service 查 `t_shelf_process WHERE process_id = $1 AND deleted_at IS NULL ORDER BY sort_order ASC, id ASC LIMIT 1` 解析货架。多结果取 `sort_order` 最小者；0 结果 → `40402 BIZ_SHELF_PROCESS_NOT_FOUND`。

### 状态机与事件
dispatch 路径：`PENDING → IN_PROCESS`，`location='PRODUCTION_SHELF'`，`current_holder_id=shelf_id`，`current_process_step_id=NULL`（dispatch 路径不解析 step，由 worker-scan / 后续流转触发）。同事务写 `t_part_event.kind='PLACED_ON_SHELF'`（from='PENDING', to='IN_PROCESS'）。

### 事务 + WS 广播（沿 worker_pool 范本）
- 读（pending）：`pool.acquire()` 不开事务。
- 写（dispatch / bulk / auto）：handler `state.pool.begin()` → service → handler `tx.commit()` → 成功 commit 后 broadcast `BATCH_PLACED_ON_SHELF`（payload 含 batch_id / target_process_id / shelf_id / version）。

### 角色守卫
下沉到 service（沿 `WorkerPoolService::pool_by_process` 范本），service 入口第一行 `current.require_any_role(...)`。

---

### `GET /api/v2/prod/batches/pending`

权限：**Manager + Clerk + Inspector**（service 内守卫）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `limit` | i64 | ✗ | 默认 200；service 层 `clamp(1, 500)` |
| `offset` | i64 | ✗ | 默认 0；service 层 `max(0)` |

Response 200 `data`：[`PendingBatchListOut`](#pendingbatchlistout-字段)

业务流转：

1. 角色守卫：Manager + Clerk + Inspector
2. SQL：`SELECT ... FROM t_part_batch pb JOIN t_part p LEFT JOIN t_customer c / pc / t_applicant a WHERE pb.status='PENDING' AND pb.deleted_at IS NULL AND p.deleted_at IS NULL ORDER BY p.system_delivery_date ASC NULLS LAST, p.is_urgent DESC, pb.created_at ASC, pb.id ASC LIMIT $1 OFFSET $2`
3. 配套 COUNT 走 `count_pending_batches` 同 WHERE 不同 SELECT

错误码：

- 40300 FORBIDDEN —— 非 Manager/Clerk/Inspector

---

### `POST /api/v2/prod/batches/dispatch`

权限：**Manager + Clerk**（service 内守卫）

Request：`DispatchRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64) | ✓ | 批次雪花 ID（`deserialize_i64` 反序列化） |
| `target_process_id` | string (i64) | ✓ | 目标工序 ID（service 按 `t_shelf_process` 解析货架） |
| `note` | string | ✗ | 落到 `t_part_event.note` |

业务流转（service `dispatch_batch`）：

1. 角色守卫：Manager + Clerk
2. 取 batch（`find_batch_by_id(include_deleted=false)`）→ `None` → `20121 BIZ_BATCH_NOT_FOUND`
3. 校验 `batch.status == 'PENDING'` → 否则 `20120 BIZ_BATCH_INVALID_STATUS`
4. 解析货架（`find_first_shelf_for_process`）→ `None` → `20508 BIZ_SHELF_PROCESS_NOT_FOUND`
5. `update_batch_dispatched`（OCC，WHERE `version = current_version AND status='PENDING'`）→ 0 行 → `40901 VERSION_CONFLICT`
6. 写 `t_part_event(kind='PLACED_ON_SHELF', from='PENDING', to='IN_PROCESS')`
7. 返回 `DispatchResult { batch_id, current_process_step_id=None, target_process_id, shelf_id, version=batch.version+1 }`

Response 200 `data`：[`DispatchResult`](#dispatchresult-字段)

错误码：

- 20120 BIZ_BATCH_INVALID_STATUS —— 批次当前 status 非 PENDING（已被下发 / 已 IN_PROCESS）
- 20121 BIZ_BATCH_NOT_FOUND —— batch_id 不存在 / 已软删
- 20508 BIZ_SHELF_PROCESS_NOT_FOUND —— `target_process_id` 在 `t_shelf_process` 无任何 active 货架映射
- 40901 VERSION_CONFLICT —— 并发事务已成功提交过本批次（OCC）
- 40300 FORBIDDEN —— 非 Manager/Clerk
- 40001 VALIDATION_ERROR —— payload shape 错误

WS 广播（commit 后下发）：

- `BATCH_PLACED_ON_SHELF`（payload = `{ batch_id, target_process_id, shelf_id, version }`）

---

### `POST /api/v2/prod/batches/bulk-dispatch`

权限：**Manager + Clerk**（service 内守卫）

Request：`BulkDispatchRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `targets` | `Vec<BulkDispatchTarget>` | ✓ | 每条 target 一个 `batch_id + target_process_id`；元素至少 1 条（空数组 → 40001） |

```jsonc
{
  "targets": [
    { "batch_id": "1001", "target_process_id": "2001" },
    { "batch_id": "1002", "target_process_id": "2002" }
  ]
}
```

业务流转（service `bulk_dispatch`）：

1. 角色守卫：Manager + Clerk
2. 校验 `targets` 非空 → 否则 `40001 VALIDATION_ERROR`（HTTP 422）
3. 顺序循环执行 `dispatch_batch` 核心逻辑；任一失败 → 直接抛 AppError（caller 通过 `code()` 区分 20120 / 20121 / 20508 / 40901 等）
4. handler 的 `Transaction` Drop 自动回滚（事务边界在 handler）

Response 200 `data`：[`BulkDispatchResult`](#bulkdispatchresult-字段)

错误码：

- 40001 VALIDATION_ERROR —— `targets` 为空
- 20120 / 20121 / 20508 / 40901 —— 同 dispatch 端点，任一失败透传
- 40300 FORBIDDEN —— 非 Manager/Clerk

WS 广播（commit 后下发）：

- `BATCH_PLACED_ON_SHELF`（payload = `{ batches: [{ batch_id, target_process_id, shelf_id, version }, ...] }`）

---

### `POST /api/v2/prod/batches/auto-dispatch`

权限：**Manager + Clerk**（service 内守卫）

Request：`AutoDispatchRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_ids` | `Vec<i64>` (字符串数组) | ✗ | 待自动推导下发的 batch_id 列表；空数组 / `null` → 40001；`deserialize_i64_vec_opt` 反序列化（前端可发字符串数组） |

业务流转（service `auto_dispatch`）：

1. 角色守卫：Manager + Clerk
2. 校验 `batch_ids` 非空 → 否则 `40001 VALIDATION_ERROR`（HTTP 422）
3. 对每个 `batch_id`：
   - 取 batch；`None` → `20121 BIZ_BATCH_NOT_FOUND`（硬错误，全回滚）
   - 查 `t_part.process_chain_id`；`NULL` → skipped（reason='NO_PROCESS_CHAIN'，不影响事务）
   - 查 `t_process_chain_step WHERE chain_id = $1 ORDER BY sort_order ASC, id ASC LIMIT 1`；
     `None` → skipped（reason='NO_PROCESS_STEP'，不影响事务）
   - 否则以 `step.process_id` 作为 `target_process_id` 调 `dispatch_batch` 核心逻辑
4. 全成功提交；任一硬错误（非 skipped）→ service 抛 AppError，handler 事务回滚
5. skipped 与 succeeded 互不影响（skipped 是合法的「该 batch 跳过」语义）

Response 200 `data`：[`AutoDispatchResult`](#autodispatchresult-字段)

错误码：

- 40001 VALIDATION_ERROR —— `batch_ids` 为空
- 20120 / 20121 / 20508 / 40901 —— 同 dispatch 端点，硬错误透传
- 40300 FORBIDDEN —— 非 Manager/Clerk

WS 广播（commit 后下发）：

- `BATCH_PLACED_ON_SHELF`（payload = `{ batches: [...] }`，仅 succeeded 部分；skipped 不广播）

---

## 字段定义

### `PendingBatchItem` 字段

```jsonc
{
  "batch_id": "1001",                  // string(i64) 雪花
  "part_id": "2001",
  "batch_no": 1,                       // i32（每个 part 从 0+ 创建）
  "quantity": 5,                       // i32
  "serial_no": "B01",                  // Option<String>，手工工单可空
  "name": "fala-A",                    // String，t_part.name
  "drawing_no": "DWG-001",             // String，t_part.drawing_no
  "planned_delivery_date": "2026-09-30", // String（"YYYY-MM-DD"），DB NOT NULL
  "system_delivery_date": "2026-09-28",  // Option<NaiveDate>
  "customer_name": "ACME L2",          // Option<String>，L2 叶子客户
  "parent_customer_name": "ACME Group", // Option<String>，L1 一级集团
  "applicant_name": "张三",             // Option<String>
  "is_urgent": false,                  // bool，t_part.is_urgent
  "note": "特殊工艺要求",                // Option<String>
  "version": 3,                        // i32，乐观锁
  "current_process_step_id": "0",      // string(i64) —— PENDING 时通常 0（与 NULL 同义）
  "process_chain_id": "5001"           // string(i64) —— t_part.process_chain_id
}
```

### `PendingBatchListOut` 字段

```jsonc
{
  "items": [PendingBatchItem, ...],
  "total": 42,        // i64，配套 COUNT（不受 limit/offset 限制）
  "limit": 200,       // i64，caller 传入（service 层 clamp(1,500)）
  "offset": 0         // i64
}
```

### `DispatchResult` 字段

```jsonc
{
  "batch_id": "1001",
  "current_process_step_id": null,    // Option<i64>，dispatch 路径不解析 step → null
  "target_process_id": "2001",
  "shelf_id": "3001",                  // string(i64)，t_shelf_process 解析
  "version": 4                        // i32，batch.version + 1
}
```

### `BulkDispatchResult` 字段

```jsonc
{
  "succeeded": [DispatchResult, ...], // 全部成功的明细
  "failed": []                        // 当前实现「任一失败 → 全回滚」，失败数组恒空；
                                       //   失败码经 AppError.code() 抛给 caller
}
```

### `AutoDispatchResult` 字段

```jsonc
{
  "succeeded": [DispatchResult, ...],    // 成功下发
  "skipped": [                            // 跳过（不影响 succeeded / 不全回滚）
    { "batch_id": "1002", "reason": "NO_PROCESS_CHAIN" },
    { "batch_id": "1003", "reason": "NO_PROCESS_STEP" }
  ]
}
```

---

## 关键错误码速查（本域相关段位）

| Code | Name | HTTP | 触发场景 |
|---|---|---|---|
| 20120 | BIZ_BATCH_INVALID_STATUS | 409 | dispatch 时 batch.status 非 PENDING |
| 20121 | BIZ_BATCH_NOT_FOUND | 404 | dispatch 时 batch_id 不存在 / 已软删 |
| 20508 | BIZ_SHELF_PROCESS_NOT_FOUND | 404 | target_process_id 在 t_shelf_process 无任何 active 映射 |
| 40901 | VERSION_CONFLICT | 409 | 并发事务抢回本批次（OCC） |
| 40300 | FORBIDDEN | 403 | 角色守卫失败 |
| 40001 | VALIDATION_ERROR | 422 | bulk/auto-dispatch targets / batch_ids 为空 |

> 完整错误码定义见 [`../index.md`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实施状态

- ✅ **`prod::batch`**（2026-09-29 新增）：
  - 4 端点（list pending / dispatch / bulk-dispatch / auto-dispatch）
  - 5 个 repo 静态方法（list_pending_batches / count_pending_batches /
    find_batch_by_id / find_first_shelf_for_process / update_batch_dispatched /
    first_step_of_chain + part_get_process_chain_id）
  - 3 个新错误码（20120 / 20121 / 20508）注册到 status_from_code + 测试
  - in-source 单测：`src/modules/prod/batch/service.rs::tests` —— list_pending /
    dispatch_batch 成功路径 + 二次 dispatch 40903 / 不存在 batch_id 40404 /
    并发冲突 40901 / t_shelf_process 多结果取 LIMIT 1 / Inspector 角色 40300 /
    bulk_dispatch 全回滚 + 空 targets 422 / auto_dispatch 无 chain / 无 step /
    全部无 chain / 有 chain 成功首道 step.id

## 参考

- 模块 README：见 `src/modules/prod/batch/{mod,handler,service,repo,vo,dto}.rs`
- 错误码：`src/shared/error.rs::code`
- 前端模块文档：`frontend/docs/03-modules/production/README.md`
- 前端视图目录：`frontend/src/views/production/`
- 报工入口（part 域）：`docs/api/parts/`