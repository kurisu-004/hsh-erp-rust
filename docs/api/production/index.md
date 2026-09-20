# production 域 API — 生产管理

> 本目录须与 `src/modules/prod/{work_type,process,process_chain,worker_pool,worker}/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**生产管理**菜单（前端 `production_group` 一级 + `process_work_type` / `part_process_chain` / `worker_queue` 三个子菜单）下挂的全部后端域。本目录按前端菜单 + 工人档案拆分 6 个文件 + 1 个入口。
>
> 实施阶段：part-worker-pool-federated-rocket（2026-09-11 起），整合自 settings/process_chain/worker_pool 三批 PR，2026-09-12 落盘为 production/ 子目录。2026-09-19 prod 容器聚合（PR-N）：worker / work_type / process / process_chain / worker_pool 五个支撑域平移至 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`，旧 nest 下线无 alias；工人档案 worker（7 端点）一并并入。

## 目录

> **导航**：[**`index.md`**](./index.md) · [`work-types.md`](./work-types.md) · [`processes.md`](./processes.md) · [`work-type-process-mapping.md`](./work-type-process-mapping.md) · [`process-chain.md`](./process-chain.md) · [`worker-pool.md`](./worker-pool.md) · [`workers.md`](./workers.md)

---

## 端点列表（27 个 = 19 + 7 worker + 1 admin/assign）

> 2026-09-19 prod 聚合：原 19 端点 + worker 7 端点（详见 [`workers.md`](./workers.md)）+ worker_pool 的 admin/assign 1 端点。

### worker / work_type / process 主数据（17 端点）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/workers` | MANAGER | 工人列表（`name_like` / `is_active` 过滤 + 分页） | [`workers.md`](./workers.md#get-apiv2prodworkers) |
| POST | `/api/v2/prod/workers` | MANAGER | 创建工人 | [`workers.md`](./workers.md#post-apiv2prodworkers) |
| GET | `/api/v2/prod/workers/{id}` | MANAGER | 工人详情 | [`workers.md`](./workers.md#get-apiv2prodworkersid) |
| POST | `/api/v2/prod/workers/{id}/update` | MANAGER | 部分更新（OCC） | [`workers.md`](./workers.md#post-apiv2prodworkersidupdate) |
| POST | `/api/v2/prod/workers/{id}/deactivate` | MANAGER | 停用（OCC，被 part_batch 持有时拒） | [`workers.md`](./workers.md#post-apiv2prodworkersiddeactivate) |
| POST | `/api/v2/prod/workers/{id}/reactivate` | MANAGER | 重启（OCC） | [`workers.md`](./workers.md#post-apiv2prodworkersidreactivate) |
| POST | `/api/v2/prod/workers/verify-badge` | 已登录（含 SHELF_ACCOUNT） | 按 badge_code 定位工人（扫码台入口） | [`workers.md`](./workers.md#post-apiv2prodworkersverify-badge) |
| GET | `/api/v2/prod/work-types` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（`code_like` 过滤 + 分页） | [`work-types.md`](./work-types.md#get-apiv2prodwork-types) |
| POST | `/api/v2/prod/work-types` | MANAGER | 创建工种 | [`work-types.md`](./work-types.md#post-apiv2prodwork-types) |
| GET | `/api/v2/prod/work-types/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工种详情（含 `process_ids`） | [`work-types.md`](./work-types.md#get-apiv2prodwork-typesid) |
| POST | `/api/v2/prod/work-types/{id}/update` | MANAGER | 部分更新（OCC） | [`work-types.md`](./work-types.md#post-apiv2prodwork-typesidupdate) |
| POST | `/api/v2/prod/work-types/{id}/soft-delete` | MANAGER | 软删（OCC） | [`work-types.md`](./work-types.md#post-apiv2prodwork-typesidsoft-delete) |
| GET | `/api/v2/prod/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 列表（过滤 + 分页） | [`processes.md`](./processes.md#get-apiv2prodprocesses) |
| POST | `/api/v2/prod/processes` | MANAGER | 创建工序（INHOUSE/OUTSOURCE） | [`processes.md`](./processes.md#post-apiv2prodprocesses) |
| GET | `/api/v2/prod/processes/{id}` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 工序详情 | [`processes.md`](./processes.md#get-apiv2prodprocessesid) |
| POST | `/api/v2/prod/processes/{id}/update` | MANAGER | 部分更新（OCC） | [`processes.md`](./processes.md#post-apiv2prodprocessesidupdate) |
| POST | `/api/v2/prod/processes/{id}/soft-delete` | MANAGER | 软删（OCC，被引用时拒） | [`processes.md`](./processes.md#post-apiv2prodprocessesidsoft-delete) |

### 工序映射 + 工艺链（5 端点）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/work-types/{id}/processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 该工种已映射工序列表 | [`work-type-process-mapping.md`](./work-type-process-mapping.md#get-apiv2prodwork-typesidprocesses) |
| POST | `/api/v2/prod/work-types/{id}/processes` | MANAGER | 整组替换工种工序映射 | [`work-type-process-mapping.md`](./work-type-process-mapping.md#post-apiv2prodwork-typesidprocesses) |
| GET | `/api/v2/prod/process-chains/by-part/{part_id}` | 已登录（任意角色，不含 ShelfAccount） | 读 part 绑定的工艺链（header + steps） | [`process-chain.md`](./process-chain.md#get-apiv2prodprocess-chainsby-partpart_id) |
| PUT | `/api/v2/prod/process-chains/by-part/{part_id}` | MANAGER | 整组 upsert 工艺链 + steps（PENDING 守卫 20705） | [`process-chain.md`](./process-chain.md#put-apiv2prodprocess-chainsby-partpart_id) |
| GET | `/api/v2/prod/process-chains/{chain_id}` | 已登录（任意角色，不含 ShelfAccount） | 按链 id 读工艺链（2026-09-16 FK 翻转新增） | [`process-chain.md`](./process-chain.md#get-apiv2prodprocess-chainschain_id) |

### 工人池（5 端点）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/worker-pool/state` | 已登录（无 role guard） | worker 当前持有 + 工序池候选数 | [`worker-pool.md`](./worker-pool.md#get-apiv2prodworker-poolstate) |
| GET | `/api/v2/prod/worker-pool/{process_id}` | Manager+Clerk+Inspector | 按工序返回候选池详情（admin 视角） | [`worker-pool.md`](./worker-pool.md#get-apiv2prodworker-poolprocess_id) |
| POST | `/api/v2/prod/admin/worker-pool/refill` | MANAGER | 为指定 worker 抢满 `max_held_batches` | [`worker-pool.md`](./worker-pool.md#post-apiv2prodadminworker-poolrefill) |
| POST | `/api/v2/prod/admin/worker-pool/remove` | MANAGER | 把 worker 持有批次按 RETURNED 语义放回候选池 | [`worker-pool.md`](./worker-pool.md#post-apiv2prodadminworker-poolremove) |
| POST | `/api/v2/prod/admin/worker-pool/auto-allocate` | MANAGER | 按 process + shelf 自动为多个 worker 抢批次/工时（COUNT/TIME × fill_ratio） | [`worker-pool.md`](./worker-pool.md#post-apiv2prodadminworker-poolauto-allocate) |
| POST | `/api/v2/prod/admin/worker-pool/assign` | MANAGER | 单 batch 拖拽分配（不循环触顶 max_held；用于 UI 单 batch 拖拽场景） | [`worker-pool.md`](./worker-pool.md#post-apiv2prodadminworker-poolassign) |

---

## 生产全流程端点地图

> 2026-09-19 prod 聚合新增：本节索引生产全流程所涉及的端点（含 part 域的报工切片），
> 不重复列文档——只点出调用顺序 + 端点跳转，方便前端 / 运维一站查询。

```
[排工序]                       [下发]                            [报工]                          [完成]
   |                              |                                |                                |
   ▼                              ▼                                ▼                                ▼
prod/process-chains/by-part/*   prod/worker-pool/{process_id}   parts/worker-scan (part域)       parts/{part_id}/complete (part域)
prod/process-chains/{chain_id}  prod/admin/worker-pool/refill    parts/{part_id}/pick-up (part域)  parts/{id}/to-process (part域)
                                prod/admin/worker-pool/assign    parts/{part_id}/to-process (part域) parts/{id}/to-inspection (part域)
                                prod/admin/worker-pool/auto-allocate                                 parts/{id}/scan-inspect (part域)
                                prod/admin/worker-pool/remove
                                prod/worker-pool/state
```

**核心入口**：
- **扫码台**（工人报工唯一入口）：`POST /api/v2/parts/worker-scan`（part 域）。扫描成功后同事务触发 `refill_for_worker`（→ 详见 [`worker-pool.md#worker-scan-与-refill-的联动`](./worker-pool.md#worker-scan-与-refill-的联动)）。
- **大屏候选池**：`GET /api/v2/prod/worker-pool/state`（无 role guard，worker 自查 + admin 监控共用）。
- **管理员拖拽分配**：UI 走 `POST /api/v2/prod/admin/worker-pool/assign`（单 batch，不循环）；批量按 COUNT/TIME 走 `auto-allocate`。

**为何报工端点留在 part 域**：
part / assembly 是 ERP 核心实体（跨生产 + 编程 + 外协 + 质检 + 交付 + 返修），CLAUDE.md 明确标记为「跨域枢纽」。
`worker-scan` / `pick-up` / `to-*` / `complete` / 返修闭环 共享 `t_part_batch` OCC + 事件 rollup + 状态机本体，
强行从 part 域拆出会掏空 part 域并制造双向依赖。本节只负责文档串联，**代码侧零迁移**。

---

## 模块关系

```
                     ┌──────────────────┐
                     │   t_process      │  ← prod::process（processes.md）
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
 │ t_work_type       │◄────work_type_id───┤  t_worker_pool    │  ← prod::worker_pool（worker-pool.md）
 │ (work-types.md)   │   (worker 持有    │  + t_part_batch   │
 │                   │    反查工种)      │  候选批次         │
 │ max_held_batches  │                    │  (FOR UPDATE      │
 │ max_held_minutes  │                    │   SKIP LOCKED)    │
 └───────────────────┘                    └───────────────────┘
         │
         │ process_chain_step.process_id
         │ 逻辑引用
         ▼
 ┌────────────────────────┐         ┌────────────────────────┐
 │ t_process_chain_step   │         │ t_worker               │  ← prod::worker（workers.md）
 │ t_part_process_chain   │  FK     │ (badge_code / work_type_id)│   工人档案主数据
 │ (header + 多步)        │ ◄─────► │ is_active / deleted_at │
 └────────────────────────┘  1:1    └────────────────────────┘
   ↑ prod::process_chain                            │
   │ (1:1 绑 part)                                 │ current_holder_id
   │                                              ▼
   │                                    t_part_batch (part域)
```

### 关键关系

- **work_type ↔ process**（多对多）：`t_work_type_process(work_type_id, process_id, sort_order)`，
  详见 [`work-type-process-mapping.md`](./work-type-process-mapping.md)
- **process ↔ process_chain**（1:N）：`t_process_chain_step.process_id` 逻辑指向 `t_process.id`，
  无 FK；详见 [`process-chain.md`](./process-chain.md)
- **process ↔ worker_pool**（按 process_id 维度）：`t_part_batch.next_process_id` 决定候选池范围，
  worker 持 `work_type` 决定可抢范围；详见 [`worker-pool.md`](./worker-pool.md)
- **worker ↔ work_type**（N:1）：`t_worker.work_type_id` 决定该 worker 可抢的工种能力
- **worker ↔ part_batch**（N:持有）：`t_part_batch.current_holder_id`（part 域；PR-2 后从 `t_part` 真相源迁出）
- **worker.deactivate** 反查 `t_part_batch.current_holder_id = worker_id AND status IN (IN_PROCESS, INSPECTION, REPAIRING, RETURNED)` → 20203 拒

### menuCode 映射

| 前端 menuCode | 前端路由 | 前端组件 | 后端子模块 | 后端文档 |
|---|---|---|---|---|
| `process_work_type` | `/production/process-work-type` | `ProcessWorkTypePage.vue`（tabbed shell: work-types / processes / mapping） | `prod::work_type` + `prod::process` | [`work-types.md`](./work-types.md) + [`processes.md`](./processes.md) + [`work-type-process-mapping.md`](./work-type-process-mapping.md) |
| `part_process_chain` | `/parts/process-chains` | （**前端页面待建**，menuCode 已 INSERT） | `prod::process_chain` | [`process-chain.md`](./process-chain.md) |
| `worker_queue` | `/workers/queue` | `WorkerQueueBoard.vue`（**实际挂在生产管理菜单下**） | `prod::worker_pool` | [`worker-pool.md`](./worker-pool.md) |
| 工人档案管理（**菜单归属非 production_group**） | `/workers` 等 | （前端视图目录 `frontend/src/views/workers/`） | `prod::worker` | [`workers.md`](./workers.md) |

> 上述 `process_work_type` / `part_process_chain` / `worker_queue` 3 个 menuCode 的授权（MANAGER + CLERK + INSPECTOR）由后端 migration 018 + 021 写入 `t_role_menu`。
> 前端 `process_work_type` tabbed shell 替代了原 `settings_root` 下的 3 个旧菜单（`work_types_list` / `processes_list` / `work_type_processes_list`，migration 021 软删）。

---

## 端点约束（5 个子模块共享）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → 40901 `VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → 2xxxx `_NOT_FOUND` 错误码
- **事务边界在 handler**：handler `state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；repo 用 `impl PgExecutor<'_>` 以同时接受 pool/conn/tx
- **WS 广播在 commit 之后**：避免慢 WS 拖慢 HTTP 响应；本目录 5 个子模块内，**仅 `prod::worker_pool` 有 WS 事件**（详见 [`worker-pool.md#ws-事件清单`](./worker-pool.md#ws-事件清单worker-pool-相关)）

---

## 关键错误码速查（本目录相关段位）

| 段 | 子模块 | 常见码 |
|---|---|---|
| 201xx | 通用业务值 | 20104 `BIZ_INVALID_VALUE`（参数校验）/ 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER`（worker-pool 持有校验） |
| 202xx | `prod::worker` / `prod::worker_pool` | 20201 / 20202 / 20203 `WORKER_IN_USE`（deactivate 反查 part_batch 持有）/ 20204 `WORKER_HOLD_LIMIT_EXCEEDED` / 20205 `WORKER_POOL_EMPTY` / 20206 `NO_WORK_TYPE` |
| 207xx | `prod::process_chain` | 20701 / 20702 / 20703 `MAX_HELD_MINUTES_NOT_SET` / 20704 `AUTO_ALLOCATE_INVALID_RATIO` / 20705 `PART_NOT_PENDING` / **20706 `PROCESS_CHAIN_REQUIRED`** |
| 208xx | `prod::process` | 20801 `NOT_FOUND` / 20802 `DUPLICATE_CODE` / 20803 `IN_USE` |
| 209xx | `prod::work_type` | 20901 / 20902 / 20903 / 20904 `MAX_HELD_NOT_SET` / 20905 `NO_PROCESS_MAPPING` |

> 完整错误码定义见 [`../index.md#跨域错误码速查`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实施状态

- ✅ **`prod::work_type`**：CRUD + 工序 mapping 整组替换 + 三态更新 + 引用校验（2026-08-26；2026-09-19 移至 `src/modules/prod/work_type/`）
- ✅ **`prod::process`**：CRUD + INHOUSE/OUTSOURCE + 引用校验（2026-08-26；color 字段 2026-09-11；2026-09-19 移至 `src/modules/prod/process/`）
- ✅ **`prod::process_chain`**：1:1 part 工艺链 + 整组 upsert（2026-09-11；2026-09-16 FK 翻转；2026-09-19 移至 `src/modules/prod/process_chain/`）
- ✅ **`prod::worker_pool`**：state + admin refill/remove + auto-allocate COUNT/TIME 模式 + admin/assign（2026-09-11 + 2026-09-14；2026-09-19 移至 `src/modules/prod/worker_pool/`）
- ✅ **`prod::worker`**（2026-09-19 聚合新增）：CRUD + verify-badge + deactivate/reactivate（2026-08-26；移至 `src/modules/prod/worker/`，URL `/api/v2/prod/workers`）
- ✅ **菜单整合**：migration 018 建 `production_group` + `part_process_chain` + 迁移 `worker_queue`；migration 021 软删 settings_root + 3 子菜单 + 新增 `process_work_type`（2026-09-12）
- ✅ **API 文档整合**：本目录（2026-09-12）
- ✅ **prod 容器聚合**（2026-09-19）：5 支撑域平移至 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`，旧 nest 下线无 alias，前端配套 PR 锁步

## 参考

- 集成测试：`tests/work_type_api.rs` / `tests/work_type_process_mapping_api.rs` / `tests/process_api.rs` / `tests/process_chain_api.rs` / `tests/worker_pool_api.rs` / `tests/worker_pool_auto_allocate_api.rs` / `tests/worker_api.rs` / `tests/worker_shelf_deactivate_api.rs`
- 模块 README：见各子模块顶层
- 错误码：`src/shared/error.rs::code`
- 前端模块文档：`frontend/docs/03-modules/production/README.md`
- 前端视图目录：`frontend/src/views/production/`
- 报工入口（part 域）：`docs/api/parts/`