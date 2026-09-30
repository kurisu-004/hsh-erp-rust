# prod::batch 域 API —— 车间 PENDING 批次列表 + 下发（2026-09-30 重构）

> 本文件须与 `src/modules/prod/batch/{handler.rs,dto.rs,service.rs,repo.rs,vo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**车间下发** PENDING 批次专用域 —— UI「待下发队列」展示 + 一键 / 批量 / 自动预览 3 路径。
> 2026-09-29 新增 + 2026-09-30 重构：
> - dispatch 统一 bulk-only（单条下发即 `targets.length == 1`）
> - auto-dispatch 改为只读 preview（不再真下发，返回首道工序 + 首货架 + skip_reason）
> - bulk-dispatch 端点删除（路由层不再挂载）

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/batches/pending` | **Manager+Clerk+Inspector** | 车间 PENDING 批次列表（JOIN 工单 + 客户 L1+L2 + 申请人） |
| POST | `/api/v2/prod/batches/dispatch` | **Manager+Clerk** | bulk-only 下发：`targets` 数组顺序执行，单批即 `targets.length==1`；任一失败 → 全回滚 |
| POST | `/api/v2/prod/batches/auto-dispatch` | **Manager+Clerk** | **只读预览**：返回每个 batch 的首道工序 + 首货架 + `skip_reason`；前端据此构造 dispatch 请求 |

> 路由挂载：`prod::mod::router().nest("/batches", batch::router())` —— 见 `src/modules/prod/mod.rs`。
> 旧 `/batches/bulk-dispatch` 端点 404（router 层不再挂载）。

---

## 共同设计要点

### 货架解析（零 schema 变更）
`target_process_id` → service 查 `t_shelf_process WHERE process_id = $1 AND deleted_at IS NULL ORDER BY sort_order ASC, id ASC LIMIT 1` 解析货架。多结果取 `sort_order` 最小者；0 结果 → `20508 BIZ_SHELF_PROCESS_NOT_FOUND`。

### 状态机与事件
dispatch 路径：`PENDING → IN_PROCESS`，`location='PRODUCTION_SHELF'`，`current_holder_id=shelf_id`，**`current_process_id=target_process_id`**（2026-09-30 新增；工序候选池归属的权威依据），`current_process_step_id=NULL`（**有意的**：dispatch 路径不解析 step，该列已降级为可选的进度指针，NULL 不影响入池；由 worker-scan / 后续流转写入）。同事务写 `t_part_event.kind='PLACED_ON_SHELF'`（from='PENDING', to='IN_PROCESS'）。

> **2026-09-30 bug 修复说明**：此前 dispatch 只写 `current_process_step_id=NULL`，而 `GET /prod/pool/{process_id}` / `/prod/pool/counts` / `take_one_from_pool` 三条 SQL 全部 `INNER JOIN t_process_chain_step ON s.id = pb.current_process_step_id` —— `s.id = NULL` 匹配不到任何行，下发成功的批次对所有工序池查询隐身（前端表现为「下发成功但工序池里没有」），且因唯一推进 step 的 worker-scan 路径又要求批次先在池里，形成死状态。现三条 SQL 均改为按 `current_process_id` 普通过滤，并新增 `t_part_batch.current_process_id`（逻辑 FK → `t_process.id`）写入。**目标**：让没有工序链的工单，其批次也能正常入池。

### 事务 + WS 广播（沿 worker_pool 范本）
- 读（pending）：`pool.acquire()` 不开事务
- 写（dispatch）：handler `state.pool.begin()` → service → handler `tx.commit()` → 成功 commit 后 broadcast `BATCH_PLACED_ON_SHELF`（payload 含 batch_id / target_process_id / shelf_id / version）
- 只读（auto-dispatch）：`pool.acquire()` 不开事务，**不发** WS 广播（无业务流转）

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

### `POST /api/v2/prod/batches/dispatch`（2026-09-30 重构：bulk-only）

权限：**Manager + Clerk**（service 内守卫）

Request：`DispatchRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `targets` | `Vec<DispatchTarget>` | ✓ | 每条 target 一个 `batch_id + target_process_id`；空数组 → 40001 |
| `note` | string? | ✗ | 落到所有 `t_part_event.note`（bulk 共享 note） |

```jsonc
{
  "targets": [
    { "batch_id": "1001", "target_process_id": "2001" },
    { "batch_id": "1002", "target_process_id": "2002" }
  ],
  "note": "批量下发"
}
```

业务流转（service `dispatch_batch` bulk-only，handler tx 边界）：

1. 角色守卫：Manager + Clerk
2. 校验 `targets` 非空 → 否则 `40001 VALIDATION_ERROR`（HTTP 422）
3. 顺序循环执行 `dispatch_single` 内部 helper；任一硬失败 → **service 抛 AppError**，handler 的 `Transaction` Drop 自动回滚全部 succeeded 写入
4. 全成功 commit → 广播 `BATCH_PLACED_ON_SHELF`（payload = `{ batches: [...] }`，含 succeeded 列表）

`dispatch_single` 内部 helper 步骤：

1. 取 batch（`find_batch_by_id(include_deleted=false)`）→ `None` → `20121 BIZ_BATCH_NOT_FOUND`
2. 校验 `batch.status == 'PENDING'` → 否则 `20120 BIZ_BATCH_INVALID_STATUS`
3. 解析货架（`find_first_shelf_for_process`）→ `None` → `20508 BIZ_SHELF_PROCESS_NOT_FOUND`
4. `update_batch_dispatched`（OCC，WHERE `version = current_version AND status='PENDING'`；SET 写 `current_process_id = target_process_id`、`current_process_step_id = NULL`）→ 0 行 → `40901 VERSION_CONFLICT`
5. 写 `t_part_event(kind='PLACED_ON_SHELF', from='PENDING', to='IN_PROCESS')`
6. 返回 `DispatchSuccessItem { batch_id, current_process_step_id=None, current_process_id=Some(target_process_id), target_process_id, shelf_id, version=batch.version+1 }`

Response 200 `data`：[`DispatchResult`](#dispatchresult-字段)

错误码（任一硬失败顶层响应）：

- 40001 VALIDATION_ERROR —— `targets` 为空
- 20120 BIZ_BATCH_INVALID_STATUS —— 批次当前 status 非 PENDING
- 20121 BIZ_BATCH_NOT_FOUND —— batch_id 不存在 / 已软删
- 20508 BIZ_SHELF_PROCESS_NOT_FOUND —— `target_process_id` 在 `t_shelf_process` 无任何 active 货架映射
- 40901 VERSION_CONFLICT —— 并发事务已成功提交过本批次（OCC）
- 40300 FORBIDDEN —— 非 Manager/Clerk

WS 广播（commit 后下发；仅 succeeded 时广播）：

- `BATCH_PLACED_ON_SHELF`（payload = `{ batches: [{ batch_id, target_process_id, shelf_id, version }, ...] }`）

---

### `POST /api/v2/prod/batches/auto-dispatch`（2026-09-30 重构：只读 preview）

权限：**Manager + Clerk**（service 内守卫）

Request：`AutoDispatchRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_ids` | `Vec<i64>` (字符串数组) | ✗ | 待预览的 batch_id 列表；空数组 / `null` → 40001；`deserialize_i64_vec_opt` 反序列化（前端可发字符串数组） |

业务流转（service `auto_dispatch_preview`，**只读不开事务**）：

1. 角色守卫：Manager + Clerk
2. 校验 `batch_ids` 非空 → 否则 `40001 VALIDATION_ERROR`（HTTP 422）
3. 调 `preview_auto_dispatch` **单 SQL**（`BatchRepo::preview_auto_dispatch`）拉所有 PENDING batch 的 preview 元数据
4. 对每个 preview 行计算 `skip_reason`：
   - `process_chain_id` 为 None → `"NO_PROCESS_CHAIN"`
   - `first_process_id` 为 None → `"NO_PROCESS_STEP"`
   - `first_shelf_id` 为 None → `"NO_SHELF"`
   - 全有 → `None`（可下发）
5. 对不在 preview 结果里的 `batch_id`（已软删 / 非 PENDING / 不存在）→ 兜底查 `part_id` + `skip_reason='NOT_FOUND'`
6. 按 `batch_ids` 入参顺序排序返回（保持 caller 视角稳定）

> **不写库、不发 WS**（只读查询，无业务流转）。

Response 200 `data`：[`AutoDispatchResult`](#autodispatchresult-字段)

错误码：

- 40001 VALIDATION_ERROR —— `batch_ids` 为空
- 40300 FORBIDDEN —— 非 Manager/Clerk

### 前端使用流

1. `GET /batches/pending` 拿到 PENDING 列表
2. `POST /batches/auto-dispatch {batch_ids: [...]}` 拿到每个 batch 的 `first_process_id` / `first_shelf_id` / `skip_reason`
3. 用户确认后 `POST /batches/dispatch {targets: [{batch_id, target_process_id}, ...]}` 真正下发

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

### `DispatchResult` 字段（2026-09-30 重构：bulk-only 形态）

```jsonc
{
  "succeeded": [DispatchSuccessItem, ...],  // 顺序与 req.targets 一致
  "failed": []                              // 当前实现「任一失败 → 全回滚」（service 抛 AppError），
                                            //   failed 字段恒空；预留 partial commit 未来扩展
}
```

### `DispatchSuccessItem` 字段

```jsonc
{
  "batch_id": "1001",
  "current_process_step_id": null,    // Option<i64>，dispatch 路径不解析 step → null（有意，可选进度指针）
  "current_process_id": "2001",       // Option<i64>，2026-09-30 新增：下发后写入的工序池归属（= target_process_id）
  "target_process_id": "2001",
  "shelf_id": "3001",                  // string(i64)，t_shelf_process 解析
  "version": 4                        // i32，batch.version + 1
}
```

### `AutoDispatchResult` 字段（2026-09-30 重构：只读 preview）

```jsonc
{
  "items": [AutoDispatchItem, ...]    // 按 req.batch_ids 入参顺序稳定排序
}
```

### `AutoDispatchItem` 字段

```jsonc
{
  "batch_id": "1001",
  "part_id": "2001",
  "process_chain_id": "3001",         // 0 表示 part 无 chain
  "first_process_id": "4001",         // 0 表示无可用 step
  "first_process_code": "PROC-A",
  "first_process_name": "工序A",
  "first_shelf_id": "5001",           // 0 表示首道工序无货架映射
  "skip_reason": null                 // Option<String>：NOT_FOUND / NO_PROCESS_CHAIN /
                                      //   NO_PROCESS_STEP / NO_SHELF；null 表示可下发
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
| 40001 | VALIDATION_ERROR | 422 | dispatch targets / auto-dispatch batch_ids 为空 |

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