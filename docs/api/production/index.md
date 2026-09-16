# production 域 API — 生产管理

> 本目录须与 `src/modules/{work_type,process,process_chain,worker_pool}/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**生产管理**菜单（前端 `production_group` 一级 + `process_work_type` / `part_process_chain` / `worker_queue` 三个子菜单）下挂的全部后端域。本目录按前端菜单的 5 个子项拆为 5 个文件 + 1 个入口。
>
> 实施阶段：part-worker-pool-federated-rocket（2026-09-11 起），整合自 settings/process_chain/worker_pool 三批 PR，2026-09-12 落盘为 production/ 子目录。

## 目录

> **导航**：[**`index.md`**](./index.md) · [`work-types.md`](./work-types.md) · [`processes.md`](./processes.md) · [`work-type-process-mapping.md`](./work-type-process-mapping.md) · [`process-chain.md`](./process-chain.md) · [`worker-pool.md`](./worker-pool.md)

---

## 端点列表（19 个）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/work-types` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（`code_like` 过滤 + 分页） | [`work-types.md`](./work-types.md#get-apiv2work-types) |
| POST | `/api/v2/work-types` | MANAGER | 创建工种 | [`work-types.md`](./work-types.md#post-apiv2work-types) |
| GET | `/api/v2/work-types/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工种详情（含 `process_ids`） | [`work-types.md`](./work-types.md#get-apiv2work-typesid) |
| POST | `/api/v2/work-types/{id}/update` | MANAGER | 部分更新（OCC） | [`work-types.md`](./work-types.md#post-apiv2work-typesidupdate) |
| POST | `/api/v2/work-types/{id}/soft-delete` | MANAGER | 软删（OCC） | [`work-types.md`](./work-types.md#post-apiv2work-typesidsoft-delete) |
| GET | `/api/v2/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（过滤 + 分页） | [`processes.md`](./processes.md#get-apiv2processes) |
| POST | `/api/v2/processes` | MANAGER | 创建工序（INHOUSE/OUTSOURCE） | [`processes.md`](./processes.md#post-apiv2processes) |
| GET | `/api/v2/processes/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工序详情 | [`processes.md`](./processes.md#get-apiv2processesid) |
| POST | `/api/v2/processes/{id}/update` | MANAGER | 部分更新（OCC） | [`processes.md`](./processes.md#post-apiv2processesidupdate) |
| POST | `/api/v2/processes/{id}/soft-delete` | MANAGER | 软删（OCC，被引用时拒） | [`processes.md`](./processes.md#post-apiv2processesidsoft-delete) |
| GET | `/api/v2/work-types/{id}/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 该工种已映射工序列表 | [`work-type-process-mapping.md`](./work-type-process-mapping.md#get-apiv2work-typesidprocesses) |
| POST | `/api/v2/work-types/{id}/processes` | MANAGER | 整组替换工种工序映射 | [`work-type-process-mapping.md`](./work-type-process-mapping.md#post-apiv2work-typesidprocesses) |
| GET | `/api/v2/process-chains/by-part/{part_id}` | 已登录（任意角色，不含 ShelfAccount） | 读 part 绑定的工艺链（header + steps） | [`process-chain.md`](./process-chain.md#get-apiv2process-chainsby-partpart_id) |
| PUT | `/api/v2/process-chains/by-part/{part_id}` | MANAGER | 整组 upsert 工艺链 + steps（PENDING 守卫 20705） | [`process-chain.md`](./process-chain.md#put-apiv2process-chainsby-partpart_id) |
| GET | `/api/v2/process-chains/{chain_id}` | 已登录（任意角色，不含 ShelfAccount） | 按链 id 读工艺链（2026-09-16 FK 翻转新增） | [`process-chain.md`](./process-chain.md#get-apiv2process-chainschain_id) |
| GET | `/api/v2/worker-pool/state` | 已登录（无 role guard） | worker 当前持有 + 工序池候选数 | [`worker-pool.md`](./worker-pool.md#get-apiv2worker-poolstate) |
| GET | `/api/v2/worker-pool/{process_id}` | Manager+Clerk+Inspector | 按工序返回候选池详情（admin 视角） | [`worker-pool.md`](./worker-pool.md#get-apiv2worker-poolprocess_id) |
| POST | `/api/v2/admin/worker-pool/refill` | MANAGER | 为指定 worker 抢满 `max_held_batches` | [`worker-pool.md`](./worker-pool.md#post-apiv2adminworker-poolrefill) |
| POST | `/api/v2/admin/worker-pool/remove` | MANAGER | 把 worker 持有批次按 RETURNED 语义放回候选池 | [`worker-pool.md`](./worker-pool.md#post-apiv2adminworker-poolremove) |
| POST | `/api/v2/admin/worker-pool/auto-allocate` | MANAGER | 按 process + shelf 自动为多个 worker 抢批次/工时（COUNT/TIME × fill_ratio） | [`worker-pool.md`](./worker-pool.md#post-apiv2adminworker-poolauto-allocate) |

---

## 模块关系

```
                    ┌──────────────────┐
                    │   t_process      │  ← process 域（processes.md）
                    │  (INHOUSE/       │
                    │   OUTSOURCE)     │
                    └────┬────────┬────┘
                         │        │
        ┌────────────────┘        └────────────────┐
        │                                          │
        │ process_id                               │ process_id (next_process_id 候选池)
        │ (t_work_type_process                     │
        │  多对多 mapping)                         │
        │                                          │
┌───────▼───────────┐                    ┌─────────▼─────────┐
│ t_work_type       │◄────work_type_id───┤  t_worker_pool    │  ← worker_pool 域（worker-pool.md）
│ (work-types.md)   │   (worker 持有    │  + t_part_batch   │
│                   │    反查工种)      │  候选批次         │
│ max_held_batches  │                    │  (FOR UPDATE      │
│ max_held_minutes  │                    │   SKIP LOCKED)    │
└───────────────────┘                    └───────────────────┘
        │
        │ process_chain_step.process_id
        │ 逻辑引用
        ▼
┌────────────────────────┐
│ t_process_chain_step   │  ← process_chain 域（process-chain.md）
│ t_part_process_chain   │     1:1 绑 part（uk_t_part_process_chain_part_id）
│ (header + 多步)        │     step 含 estimated_minutes
└────────────────────────┘
```

### 关键关系

- **work_type ↔ process**（多对多）：`t_work_type_process(work_type_id, process_id, sort_order)`，
  详见 [`work-type-process-mapping.md`](./work-type-process-mapping.md)
- **process ↔ process_chain**（1:N）：`t_process_chain_step.process_id` 逻辑指向 `t_process.id`，
  无 FK；详见 [`process-chain.md`](./process-chain.md)
- **process ↔ worker_pool**（按 process_id 维度）：`t_part_batch.next_process_id` 决定候选池范围，
  worker 持 `work_type` 决定可抢范围；详见 [`worker-pool.md`](./worker-pool.md)
- **worker ↔ work_type**（N:1）：`t_worker.work_type_id` 决定该 worker 可抢的工种能力

### menuCode 映射

| 前端 menuCode | 前端路由 | 前端组件 | 后端域 | 后端文档 |
|---|---|---|---|---|
| `process_work_type` | `/production/process-work-type` | `ProcessWorkTypePage.vue`（tabbed shell: work-types / processes / mapping） | `work_type` + `process` | [`work-types.md`](./work-types.md) + [`processes.md`](./processes.md) + [`work-type-process-mapping.md`](./work-type-process-mapping.md) |
| `part_process_chain` | `/parts/process-chains` | （**前端页面待建**，menuCode 已 INSERT） | `process_chain` | [`process-chain.md`](./process-chain.md) |
| `worker_queue` | `/workers/queue` | `WorkerQueueBoard.vue`（**实际挂在生产管理菜单下**） | `worker_pool` | [`worker-pool.md`](./worker-pool.md) |

> 上述 3 个 menuCode 的授权（MANAGER + CLERK + INSPECTOR）由后端 migration 018 + 021 写入 `t_role_menu`。
> 前端 `process_work_type` tabbed shell 替代了原 `settings_root` 下的 3 个旧菜单（`work_types_list` / `processes_list` / `work_type_processes_list`，migration 021 软删）。

---

## 端点约束（5 个域共享）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → 40901 `VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → 2xxxx `_NOT_FOUND` 错误码
- **事务边界在 handler**：handler `state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；repo 用 `impl PgExecutor<'_>` 以同时接受 pool/conn/tx
- **WS 广播在 commit 之后**：避免慢 WS 拖慢 HTTP 响应；本目录 5 个域内，**仅 `worker_pool` 域有 WS 事件**（详见 [`worker-pool.md#ws-事件清单`](./worker-pool.md#ws-事件清单worker-pool-相关)）

---

## 关键错误码速查（本目录相关段位）

| 段 | 域 | 常见码 |
|---|---|---|
| 201xx | 通用业务值 | 20104 `BIZ_INVALID_VALUE`（参数校验）/ 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER`（worker-pool 持有校验） |
| 202xx | 工人 | 20201 / 20202 / 20206 `NO_WORK_TYPE`（worker-pool 依赖） |
| 207xx | 工艺链 | 20701 / 20702 / 20703 `MAX_HELD_MINUTES_NOT_SET` / 20704 `AUTO_ALLOCATE_INVALID_RATIO` / 20705 `PART_NOT_PENDING` / **20706 `PROCESS_CHAIN_REQUIRED`** |
| 208xx | 工序 | 20801 `NOT_FOUND` / 20802 `DUPLICATE_CODE` / 20803 `IN_USE` |
| 209xx | 工种 | 20901 / 20902 / 20903 / 20904 `MAX_HELD_NOT_SET` / 20905 `NO_PROCESS_MAPPING` |

> 完整错误码定义见 [`../index.md#跨域错误码速查`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实施状态

- ✅ **work_type 域**：CRUD + 工序 mapping 整组替换 + 三态更新 + 引用校验（2026-08-26）
- ✅ **process 域**：CRUD + INHOUSE/OUTSOURCE + 引用校验（2026-08-26；color 字段 2026-09-11）
- ✅ **process_chain 域**：1:1 part 工艺链 + 整组 upsert（2026-09-11）
- ✅ **worker_pool 域**：state + admin refill/remove + auto-allocate COUNT/TIME 模式（2026-09-11）
- ✅ **菜单整合**：migration 018 建 `production_group` + `part_process_chain` + 迁移 `worker_queue`；migration 021 软删 settings_root + 3 子菜单 + 新增 `process_work_type`（2026-09-12）
- ✅ **API 文档整合**：本目录（2026-09-12）

## 参考

- 集成测试：`tests/work_type_api.rs` / `tests/work_type_process_mapping_api.rs` / `tests/process_api.rs` / `tests/process_chain_api.rs` / `tests/worker_pool_api.rs` / `tests/worker_pool_auto_allocate_api.rs`
- 模块 README：见各子模块顶层
- 错误码：`src/shared/error.rs::code`
- 前端模块文档：`frontend/docs/03-modules/production/README.md`
- 前端视图目录：`frontend/src/views/production/`
