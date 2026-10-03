# production 域 API — 生产管理

> 本目录须与 `src/modules/prod/*` 保持同步（2026-10-03 订正：各模块文件形态不一致，不适用统一通配）：
> - 扁平三件套 `{handler.rs,dto.rs,service.rs}`：`work_type` / `process` / `worker_pool` / `worker` / `programming` / `shelf_process`
> - `process_chain`：`{handler.rs,dto.rs}` + `service/` `repo/` `vo/` 目录
> - `batch`：`{dto.rs,vo.rs,mod.rs,model.rs,status_gate.rs}` + `handler/` `service/` `repo/` 目录（见 [`./batches.md`](./batches.md)）
>
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**生产管理**菜单（前端 `production_group` 一级 + `process_work_type` / `part_process_chain` / `pending_programming` / `worker_queue` / `inspection_pending` / **`workers_list`（2026-10-04 自权限管理 `auth_group` 迁入）** 子菜单）下挂的全部后端域。本目录按前端菜单 + 工人档案 + 待下发批次拆分 **9 个子页 + 1 个入口**（⚠️ 2026-10-02 订正：原写「7 个文件」，实为 9 个子页）。
>
> 实施阶段：part-worker-pool-federated-rocket（2026-09-11 起），整合自 settings/process_chain/worker_pool 三批 PR，2026-09-12 落盘为 production/ 子目录。2026-09-19 prod 容器聚合：worker / work_type / process / process_chain / worker_pool 五个支撑域平移至 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`，旧 nest 下线无 alias；工人档案 worker（7 端点）一并并入。2026-09-29 新增 `prod::batch`（车间 PENDING 待下发批次列表 + 单条 / 批量 / 自动下发 4 端点，URL `/api/v2/prod/batches/*`）。2026-09-30 prod 域 9 端点重构：worker-pool → pool 路径收敛（`/pool/*` 单 nest，5 端点；新增通用 move 取代 assign+remove；移除 admin nest）；batches dispatch 统一 bulk-only（`/dispatch` 单端点，`targets` 数组；移除 bulk-dispatch）；auto-dispatch 改为只读 preview。2026-10-01 新增 `prod::programming`（待编程一览 1 只读端点，URL `/api/v2/prod/programming/pending`，part 状态白名单闸门 + 三规则并集口径；前端「待编程一览」页从 part 域 `GET /api/v2/parts/pending-programming` 切过来，part 域旧端点保留兼容）。2026-10-02 新增 `prod::shelf_process`（货架 ↔ 工序映射 `t_shelf_process` 3 端点，URL `/api/v2/prod/shelf-processes/*`）：自 `src/modules/shelf/process_mapping/` 整体搬入 prod 域，旧路径 `GET|POST /api/v2/shelves/{id}/processes` + `GET /api/v2/shelves/processes` **404（无 alias）**，请求 / 响应契约逐字不变。**前端配套改动不止改 URL，清单见 [`shelf-process-mapping.md#前端配套改动清单`](./shelf-process-mapping.md#前端配套改动清单)**（3 个 URL + `account_count` 出参删除引发的 4 处 Zod/类型/表格落点）。

## 目录

> **导航**：[**`index.md`**](./index.md) · [`work-types.md`](./work-types.md) · [`processes.md`](./processes.md) · [`work-type-process-mapping.md`](./work-type-process-mapping.md) · [`process-chain.md`](./process-chain.md) · [`shelf-process-mapping.md`](./shelf-process-mapping.md) · [`worker-pool.md`](./worker-pool.md) · [`workers.md`](./workers.md) · [`batches.md`](./batches.md) · [`pending-programming.md`](./pending-programming.md)

---

## 端点列表（60 个 = 7 worker + 7 工种 + 5 工序 + 3 工艺链 + 6 pool + 28 batch + 1 programming + 3 货架映射）

> 2026-09-19 prod 聚合：原 19 端点 + worker 7 端点（详见 [`workers.md`](./workers.md)）+ worker_pool 的 admin/assign 1 端点。2026-09-29 新增 4 端点（详见 [`batches.md`](./batches.md)）：1 个 PENDING 列表 + 3 个下发（单条 / 批量 / 自动）。
> 2026-09-30 prod 域 9 端点重构（worker-pool + batches 合并）：
> - worker-pool → pool 路径收敛：原 `/worker-pool/*` + `/admin/worker-pool/*` 双 nest 合并为 `/pool/*` 单 nest（6 → 5 端点；移除 admin/assign 1 端点 + 新增通用 move 1 端点）。
> - batches 端点合并：原 `dispatch`（单条）+ `bulk-dispatch`（批量）合并为统一 bulk-only `dispatch`（4 → 3 端点；移除 bulk-dispatch）。
> - batches auto-dispatch 改为只读 preview（不再真下发）。
> - 净变化：原 31 → 30 端点（-1 net）。
> 2026-10-01 新增 1 端点（详见 [`pending-programming.md`](./pending-programming.md)）：`prod::programming` 待编程一览 → 净变化 30 → 31 端点（+1）。
> 2026-10-02 新增 3 端点（详见 [`shelf-process-mapping.md`](./shelf-process-mapping.md)）：`prod::shelf_process` 货架 ↔ 工序映射，自 `src/modules/shelf/process_mapping/` 搬入本域（旧路径 404，无 alias）→ 净变化 32 → 35 端点（+3）。⚠️ 2026-10-02 订正：上文两处旧账均系计数残留 —— ①「worker-pool 5 端点」实为 6（见「工人池」节），故 **master 上的「31」实为 32**；② 本节标题原按「17 + 5 + 3 + 6 + 3 + 1」记账，17 是「主数据表实列行数」（= 7 worker + 5 工种 CRUD + 5 工序），而按子模块求和应是 7 + 7 + 5 = 19，两者相差的 2 条正是工种↔工序映射端点（记在下一节的 5 条里）。现标题改为**按子模块求和**的显式公式，逐项可加：7 + 7 + 5 + 3 + 6 + 3 + 1 + 3 = 35（订正时点为 35，2026-10-02 batch 子资源迁入后为 60，见下条）。
> **2026-10-02 订正**：25 条 `t_part_batch` 子资源路由自 part 域迁入后，本节标题的
> 35 改为 **60**（35 + 25）。子资源逐条清单见
> [`batches.md`](./batches.md#2026-10-02-t_part_batch-子资源迁入)。

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
| POST | `/api/v2/prod/process-chains/by-part/{part_id}` | MANAGER | 整组 upsert 工艺链 + steps（PENDING 守卫 20705；2026-09-29 改 PUT → POST 统一全仓库惯例） | [`process-chain.md`](./process-chain.md#post-apiv2prodprocess-chainsby-partpart_id) |
| GET | `/api/v2/prod/process-chains/{chain_id}` | 已登录（任意角色，不含 ShelfAccount） | 按链 id 读工艺链（2026-09-16 FK 翻转新增） | [`process-chain.md`](./process-chain.md#get-apiv2prodprocess-chainschain_id) |

### 货架 ↔ 工序映射（3 端点，2026-10-02 自 shelf 域搬入）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/shelf-processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 所有 active shelf 的 mapping 批量查询（防 N+1） | [`shelf-process-mapping.md`](./shelf-process-mapping.md#get-apiv2prodshelf-processes) |
| GET | `/api/v2/prod/shelf-processes/{shelf_id}` | 已登录（M/C/CNC/SHELF/INSPECTOR）+ scope 校验 | 该货架的工序映射列表（按 sort_order） | [`shelf-process-mapping.md`](./shelf-process-mapping.md#get-apiv2prodshelf-processesshelf_id) |
| POST | `/api/v2/prod/shelf-processes/{shelf_id}` | MANAGER | 整组替换货架工序映射 | [`shelf-process-mapping.md`](./shelf-process-mapping.md#post-apiv2prodshelf-processesshelf_id) |

> **归属说明**：2026-10-02 前这 3 个端点在 `src/modules/shelf/`（`GET|POST
> /api/v2/shelves/{id}/processes` + `GET /api/v2/shelves/processes`）。`t_shelf_process`
> 关联的是本域实体 `t_process`，按域规约搬入 prod。旧路径已从 router 删除（**无
> alias**），请求 / 响应契约逐字不变。同期删除的还有 `ShelfOut.account_count`
> （账号绑定数，绑定真源本来就在 iam 域 `t_user_role`）—— **本目录零 iam 改动**。
> 20504~20508 数字不动（20507 被 `part/worker_scan` + `prod/worker_pool` 判定，
> 20508 被 `prod::batch` 判定），只改归属说明。
> ⚠️ `GET /api/v2/shelves/processes` 例外：现在落到 shelf 域 `/{id}` 路由，
> `processes` 非 i64 被 axum 拒为 400 纯文本（非 `R` 信封），而非 404。
> ⚠️ **前端配套改动 ≠ 只改 URL**：mapping 端点本身确实只改 URL，但同 commit 删掉的
> `account_count` 出参会打爆前端 Zod 必填字段，须配套改 4 处 + 1 条回归用例 ——
> 完整清单见 [`shelf-process-mapping.md#前端配套改动清单`](./shelf-process-mapping.md#前端配套改动清单)。

### 工人池（6 端点，2026-09-30 重构：worker-pool → pool 路径收敛）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/pool/state` | 已登录（无 role guard） | worker 当前持有 + 工序池候选数 | [`worker-pool.md`](./worker-pool.md#get-apiv2prodpoolstate) |
| GET | `/api/v2/prod/pool/counts` | Manager+Clerk+Inspector | **2026-09-30 新增**：全工序候选批次聚合计数（dashboard 快照） | [`worker-pool.md`](./worker-pool.md#get-apiv2prodpoolcounts) |
| GET | `/api/v2/prod/pool/{process_id}` | Manager+Clerk+Inspector | 按工序返回候选池详情（admin 视角） | [`worker-pool.md`](./worker-pool.md#get-apiv2prodpoolprocess_id) |
| POST | `/api/v2/prod/pool/refill` | MANAGER | 为指定 worker 抢满 `max_held_batches` | [`worker-pool.md`](./worker-pool.md#post-apiv2prodpoolrefill) |
| POST | `/api/v2/prod/pool/move` | MANAGER | **2026-09-30 新增**：通用移动端点（POOL ↔ WORKER + WORKER ↔ WORKER 三方向） | [`worker-pool.md`](./worker-pool.md#post-apiv2prodpoolmove2026-09-30-新增) |
| POST | `/api/v2/prod/pool/auto-allocate` | MANAGER | 按 process + shelf 自动为多个 worker 抢批次/工时（COUNT/TIME × fill_ratio） | [`worker-pool.md`](./worker-pool.md#post-apiv2prodpoolauto-allocate) |

> 旧 `/api/v2/prod/worker-pool/*` 与 `/api/v2/prod/admin/worker-pool/*` 路径 404（router 层不再挂载）。
>
> ⚠️ **2026-10-02 顺带订正**：本节标题原写「5 端点」、`prod` 总数原写「31」，但下方
> 表格实列 6 行且 `src/modules/prod/worker_pool/mod.rs` router 确有 6 条 route
> （`/state` `/counts` `/{process_id}` `/refill` `/move` `/auto-allocate`）—— 是
> 2026-09-30 重构时的旧计数残留（`/counts` 新增时只加了行没改标题）。故 **master 上的
> 「31」实为 32**；本任务 +3 后 **实际 35**。

### 待下发批次（3 端点，2026-09-29 新增 + 2026-09-30 重构；2026-10-02 扩为 28）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/batches/pending` | Manager+Clerk+Inspector | 车间 PENDING 批次列表（JOIN 4 表扁平投影） | [`batches.md`](./batches.md#get-apiv2prodbatchespending) |
| POST | `/api/v2/prod/batches/dispatch` | Manager+Clerk | bulk-only 下发（targets 数组；单批即 targets.length==1；任一失败 → 全回滚） | [`batches.md`](./batches.md#post-apiv2prodbatchesdispatch2026-09-30-重构bulk-only) |
| POST | `/api/v2/prod/batches/auto-dispatch` | Manager+Clerk | **2026-09-30 改为只读预览**：返回每个 batch 的首道工序 + 首货架 + skip_reason；前端据此构造 dispatch 请求 | [`batches.md`](./batches.md#post-apiv2prodbatchesauto-dispatch2026-09-30-重构只读-preview) |

> 旧 `/api/v2/prod/batches/bulk-dispatch` 端点 404（router 层不再挂载；bulk 走统一 dispatch 的 targets 数组）。
>
> **2026-10-02 扩充**：`/api/v2/prod/batches/*` 前缀下新增 25 条 `t_part_batch`
> 子资源路由（自 part 域迁入，锚点 `part_id` → `batch_id`）—— 单批流转
> （`to-inspection` / `to-ship` / `to-process` / `deliver` / `complete` / `start-repair` /
> `place-on-shelf` / `recall-to-pending` / `release-from-programming` / 外协 3 条 /
> 返修 3 条 / `scan-inspect` / `pick-up` / `split` / 单批 `cancel` / `scan/deliver`）+
> 静态批量与事件 3 条（`to-ship` / `to-inspection` / `worker-scan`）+ 集合读 3 条
> （`inspection` / `repair` / `repairing`）。本域端点总数 35 → 60。逐条清单与文档章节索引见
> [`batches.md#2026-10-02-t_part_batch-子资源迁入`](./batches.md#2026-10-02-t_part_batch-子资源迁入)。

### 待编程一览（1 端点，2026-10-01 新增）

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/prod/programming/pending` | Manager+Clerk+Inspector+CNC_PROGRAMMER | 待编程工单列表（part 状态白名单闸门 + 三规则并集，part 级去重：⓿ `status IN ('PENDING','IN_PROCESS','PROGRAMMING')`（约束全部三规则） ① `status='PROGRAMMING'` ② 链含 `is_cnc` 工序 ③ 批次 `current_process_id` 指向 `is_cnc` 工序） | [`pending-programming.md`](./pending-programming.md#get-apiv2prodprogrammingpending) |

> 归属说明：前端「待编程一览」页 2026-10-01 从 part 域 `GET /api/v2/parts/pending-programming`
> 切到本端点 —— part 域旧端点走「批次货架 → 工序」间接链路（开发库恒返空），本端点规则 3
> 直接读 migration 004 确立的权威列 `t_part_batch.current_process_id`。**part 域一行未改**，
> 旧端点保留兼容（仅 [`../parts/lifecycle.md`](../parts/lifecycle.md) 追加弃用说明）。
> ⚠️ `CNC_PROGRAMMER` 必须在角色白名单内（编程员账号进本页的主路径）。

---

## 生产全流程端点地图

> 2026-09-19 prod 聚合新增：本节索引生产全流程所涉及的端点（含 part 域的报工切片），
> 不重复列文档——只点出调用顺序 + 端点跳转，方便前端 / 运维一站查询。
> 2026-09-29 新增「待下发」一列：车间 `prod::batch` 4 端点负责 PENDING 批次 → IN_PROCESS 的下发。

```
[排工序]                       [待下发]                       [下发]                            [报工]                          [完成]
   |                              |                              |                                |                                |
   ▼                              ▼                              ▼                                ▼                                ▼
prod/process-chains/by-part/*   prod/batches/pending          prod/pool/{process_id}          prod/batches/worker-scan     prod/batches/{batch_id}/complete
prod/process-chains/{chain_id}  prod/batches/dispatch         prod/pool/refill                prod/batches/{batch_id}/pick-up
                               prod/batches/auto-dispatch    prod/pool/move                  prod/batches/{batch_id}/to-process
                                                              prod/pool/auto-allocate         prod/batches/{batch_id}/to-inspection
                                                              prod/pool/state                 prod/batches/{batch_id}/scan-inspect
                                                              prod/pool/counts
```

**核心入口**：
- **车间下发台**：浏览器打开「待下发 Tab」先 `GET /api/v2/prod/batches/pending` 拿列表，UI 自动 `POST /api/v2/prod/batches/auto-dispatch {batch_ids: [...]}` 预览每批的首道工序 + 首货架，用户确认后 `POST /api/v2/prod/batches/dispatch {targets: [{batch_id, target_process_id}, ...]}` 真正下发。提交后 batch 状态 `PENDING → IN_PROCESS`，自动触发 `BATCH_PLACED_ON_SHELF` WS 广播让其他客户端刷新候选池。
- **扫码台**（工人报工唯一入口）：`POST /api/v2/prod/batches/worker-scan`。扫描成功后同事务触发 `refill_for_worker`（联动事件见 [`worker-pool.md` WS 事件清单](./worker-pool.md#ws-事件清单worker-pool-相关)）。
- **大屏候选池**：`GET /api/v2/prod/pool/state`（无 role guard，worker 自查 + admin 监控共用）。
- **管理员拖拽 / 转移批次**：UI 走 `POST /api/v2/prod/pool/move {from, to}`（POOL ↔ WORKER + WORKER ↔ WORKER 三方向通用移动端点）；批量按 COUNT/TIME 走 `pool/auto-allocate`。

**为何报工端点归 prod 域**：
`worker-scan` / `pick-up` / `to-*` / `complete` / 返修闭环 共享 `t_part_batch` OCC +
`status_gate` rollup + 状态机本体 —— 这三者（`PartBatchRepo` / `status_gate` /
批次 SQL）已**整体搬到 prod 域**，路由因而与实现同域；part 域收窄为
「多批次动作 + 非批次动作」（`POST /parts/{part_id}/cancel` / `force-complete` /
CRUD / 文件 / 列表 / `GET /parts/{part_id}/batches`）。
路径锚点也据此从 `part_id` 改为 `batch_id`：`t_part_batch.id` 全局唯一即锚点，
`part_id` 形参本就冗余（`status_gate` 的 part 派生由 `RETURNING part_id` 反推，
不依赖调用方传值），且「批次不属于该 part」这一场景在 `batch_id` 必填后不再存在。

**与 2026-09-29 那次 parts 集合迁移的边界**（勿与本节混为一谈）：
2026-09-29 迁的是 **parts 集合**（工单列表 `/prod/parts` → 移回 `/parts`），
`GET /api/v2/parts`、`GET /api/v2/parts/by-serial/*` 等至今在 part 域；
2026-10-02 迁的是 **batch 子资源**（`/parts/*/{action}` → `/prod/batches/{batch_id}/{action}`）。
两者方向相反、对象不同，不冲突。

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
   │
   │                                      ↑
   │                                      │ status=PENDING 列表 + 下发
   │                                      │ (OCC UPDATE → IN_PROCESS)
   │                                      │
   │                                    prod::batch (batches.md)
   │                                    /api/v2/prod/batches/*
   │
   │ （auto-dispatch 也读 t_part.process_chain_id 解析首道 step）
```

### 关键关系

- **work_type ↔ process**（多对多）：`t_work_type_process(work_type_id, process_id, sort_order)`，
  详见 [`work-type-process-mapping.md`](./work-type-process-mapping.md)
- **process ↔ process_chain**（1:N）：`t_process_chain_step.process_id` 逻辑指向 `t_process.id`，
  无 FK；详见 [`process-chain.md`](./process-chain.md)
- **process ↔ worker_pool**（按 process_id 维度）：`t_part_batch.next_process_id` 决定候选池范围，
  worker 持 `work_type` 决定可抢范围；详见 [`worker-pool.md`](./worker-pool.md)
- **worker ↔ work_type**（N:1）：`t_worker.work_type_id` 决定该 worker 可抢的工种能力
- **worker ↔ part_batch**（N:持有）：`t_part_batch.current_holder_id`（part 域；2026-09-16 后从 `t_part` 真相源迁出）
- **worker.deactivate** 反查 `t_part_batch.current_holder_id = worker_id AND status IN (IN_PROCESS, INSPECTION, RETURNED)` → 20203 拒（**2026-10-01**：REPAIRING 已降级为 `is_repairing` 标记，返修批次 status 即 IN_PROCESS，守卫强度不变）

### menuCode 映射

| 前端 menuCode | 前端路由 | 前端组件 | 后端子模块 | 后端文档 |
|---|---|---|---|---|
| `process_work_type` | `/production/process-work-type` | `ProcessWorkTypePage.vue`（tabbed shell: work-types / processes / mapping） | `prod::work_type` + `prod::process` | [`work-types.md`](./work-types.md) + [`processes.md`](./processes.md) + [`work-type-process-mapping.md`](./work-type-process-mapping.md) |
| `part_process_chain` | `/parts/process-chains` | （**前端页面待建**，menuCode 已 INSERT） | `prod::process_chain` | [`process-chain.md`](./process-chain.md) |
| `worker_queue` | `/production/worker-queue` | `production/WorkerQueueBoard.vue` | `prod::worker_pool` | [`worker-pool.md`](./worker-pool.md) |
| 工人档案管理（**2026-10-04 菜单归属改为 `production_group`**） | `/production/worker-list` | （前端视图目录 `frontend/src/views/production/`） | `prod::worker` | [`workers.md`](./workers.md) |
| （**前端 menuCode 待定 2026-09-29**） | `/production/pending-dispatch` | `ProductionPendingDispatchPage.vue`（**新前端页面 2026-09-29**） | `prod::batch` | [`batches.md`](./batches.md) |
| （**前端 menuCode 待定 2026-10-01**） | 「待编程一览」页（沿用 part 域路由） | 待编程 tab（`has_cnc_program` 三态） | `prod::programming` | [`pending-programming.md`](./pending-programming.md)（口径：状态白名单闸门 + 三规则并集，与 part 域旧端点的差异见该页「过滤谓词」段） |

> 上述 `process_work_type` / `part_process_chain` / `worker_queue` 3 个 menuCode 的授权（MANAGER + CLERK + INSPECTOR）由 `seeds/menu.sql` 第 4 节（角色授权 `t_role_menu`）按扁平 code 列表写入。
> 2026-10-04：`workers_list`（工人档案）随前端视图搬到 `/production/worker-list`，菜单从 `auth_group` 迁到 `production_group`（`seeds/menu.sql` 改父分组 + path + sort_order 25）—— `t_role_menu` 仍是扁平 code 列表，**授权范围一行未变**（仍只有 MANAGER 有 `workers_list`）。
> `prod::batch` 4 端点走 service 内 `require_any_role(...)` 守卫（list=Manager+Clerk+Inspector；写=Manager+Clerk），无需新增 `t_role_menu` 行（沿用 production_group 既有授权）。
> `prod::programming` 1 端点同样走 service 内守卫（Manager+Clerk+Inspector+**CNC_PROGRAMMER**），同样无需新增 `t_role_menu` 行。
> 前端 `process_work_type` tabbed shell 替代了原 `settings_root` 下的 3 个旧菜单（`work_types_list` / `processes_list` / `work_type_processes_list`，migration 021 软删）。

---

## 端点约束（8 个子模块共享）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → 40901 `VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → 2xxxx `_NOT_FOUND` 错误码
- **事务边界在 handler**：handler `state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；repo 用 `impl PgExecutor<'_>` 以同时接受 pool/conn/tx
- **WS 广播在 commit 之后**：避免慢 WS 拖慢 HTTP 响应；本目录 8 个子模块内，
  - `prod::worker_pool` —— 5 个 `WORKER_*` 事件（详见 [`worker-pool.md#ws-事件清单`](./worker-pool.md#ws-事件清单worker-pool-相关)）
  - `prod::batch` —— 1 个 `BATCH_PLACED_ON_SHELF` 事件（详见 [`batches.md` 事务 + WS 广播](./batches.md#事务--ws-广播沿-worker_pool-范本)）
  - `prod::programming` —— 纯读端点，**不发**任何 WS 事件
  - `prod::shelf_process` —— 3 端点（1 写 2 读），**不发**任何 WS 事件

---

## 关键错误码速查（本目录相关段位）

| 段 | 子模块 | 常见码 |
|---|---|---|
| 201xx | 通用业务值 | 20104 `BIZ_INVALID_VALUE`（参数校验）/ 20114 `BIZ_PART_BATCH_NOT_HELD_BY_WORKER`（worker-pool 持有校验）/ **20120 `BIZ_BATCH_INVALID_STATUS`**（prod::batch dispatch 时 batch.status 非 PENDING） / **20121 `BIZ_BATCH_NOT_FOUND`**（prod::batch dispatch 时按 batch_id 查不到；与 part 域 20109 `BIZ_PART_BATCH_NOT_FOUND` 独立槽位，按调用方区分场景） |
| 202xx | `prod::worker` / `prod::worker_pool` | 20201 / 20202 / 20203 `WORKER_IN_USE`（deactivate 反查 part_batch 持有）/ 20204 `WORKER_HOLD_LIMIT_EXCEEDED` / 20205 `WORKER_POOL_EMPTY` / 20206 `NO_WORK_TYPE` |
| 205xx | 货架（本目录 `prod::shelf_process` + `prod::batch` 引用） | **20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND`**（货架映射 items 里有 process_id 不存在）/ 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`（货架未映射该工序，worker_pool move 复用）/ **20508 `BIZ_SHELF_PROCESS_NOT_FOUND`**（prod::batch dispatch 时 target_process_id 在 t_shelf_process 0 结果） |
| 207xx | `prod::process_chain` | 20701 / 20702 / 20703 `MAX_HELD_MINUTES_NOT_SET` / 20704 `AUTO_ALLOCATE_INVALID_RATIO` / 20705 `PART_NOT_PENDING` / **20706 `PROCESS_CHAIN_REQUIRED`**（2026-10-03 起生产流端点改用 `optional_process_chain` 后**无端点返回**，常量保留在 `shared::error::code` 注册表） |
| 208xx | `prod::process` | 20801 `NOT_FOUND` / 20802 `DUPLICATE_CODE` / 20803 `IN_USE` |
| 209xx | `prod::work_type` | 20901 / 20902 / 20903 / 20904 `MAX_HELD_NOT_SET` / 20905 `NO_PROCESS_MAPPING` |

> 完整错误码定义见 [`../index.md#跨域错误码速查`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

> **20109 / 20121 区分依据**（2026-09-29）：两者都是「批次不存在」，但分属不同调用方：
> - `20109 BIZ_PART_BATCH_NOT_FOUND` —— part 域（inspection / lifecycle 等）按 part_id 或 (part_id, batch_id) 反查批次时找不到
> - `20121 BIZ_BATCH_NOT_FOUND` —— prod 域 batch 子模块按 batch_id 单独查批次时找不到（dispatch / auto-dispatch 路径）
>
> 前端按错误码区分处理路径；message 字段携带具体 batch_id 便于排查。槽位独立而非合并，避免 part / prod 两域后续各自扩码互相挤占。

---

## 实施状态

- ✅ **`prod::work_type`**：CRUD + 工序 mapping 整组替换 + 三态更新 + 引用校验（2026-08-26；2026-09-19 移至 `src/modules/prod/work_type/`）
- ✅ **`prod::process`**：CRUD + INHOUSE/OUTSOURCE + 引用校验（2026-08-26；color 字段 2026-09-11；2026-09-19 移至 `src/modules/prod/process/`）
- ✅ **`prod::process_chain`**：1:1 part 工艺链 + 整组 upsert（2026-09-11；2026-09-16 FK 翻转；2026-09-19 移至 `src/modules/prod/process_chain/`，2026-09-29 PUT→POST 统一惯例）
- ✅ **`prod::worker_pool`**（**2026-09-30 重构 → pool**）：state + counts + refill + auto-allocate COUNT/TIME + 通用 move 取代 admin_remove + admin_assign（POOL ↔ WORKER + WORKER ↔ WORKER 三方向，5 端点）；路径收敛为 `/pool/*` 单 nest
- ✅ **`prod::worker`**（2026-09-19 聚合新增）：CRUD + verify-badge + deactivate/reactivate（2026-08-26；移至 `src/modules/prod/worker/`，URL `/api/v2/prod/workers`）
- ✅ **`prod::batch`**（2026-09-29 新增 + 2026-09-30 重构）：PENDING 批次列表 + bulk-only dispatch（`targets` 数组，单批即 `targets.length==1`）+ 只读 auto-dispatch preview（首道工序 + 首货架 + skip_reason），3 端点（URL `/api/v2/prod/batches/*`）；复用既有 `t_shelf_process` 解析货架，零 schema 变更；commit 后广播 `BATCH_PLACED_ON_SHELF` WS 事件
- ✅ **`prod::programming`**（2026-10-01 新增）：待编程一览 1 只读端点（part 状态白名单闸门 + 三规则并集 + part 级去重；`has_cnc_program` 三态 Tab；`keyword`（`%`/`_` 已转义）/ `serial_no` 过滤；`limit` / `offset` 空串走缺省 + `clamp(1,500)` / `max(0)`；角色 Manager+Clerk+Inspector+CNC_PROGRAMMER），URL `/api/v2/prod/programming/pending`；零 schema 变更；集成测试 `tests/production/pending_programming.rs` **14 场景**
- ✅ **`prod::shelf_process`**（2026-10-02 新增）：货架 ↔ 工序映射 3 端点（全集查询 / 单架查询 / 整组替换），`t_shelf_process` SQL 真源收口到 `ShelfProcessRepo`（6 个静态方法：平移 4 + 从 `prod::batch` / `prod::worker_pool` 各收 1 处）；URL `/api/v2/prod/shelf-processes/*`，旧路径 404 无 alias；零 schema 变更；集成测试 `tests/production/shelf_process.rs` **4 场景**
- ✅ **菜单整合**：migration 018 建 `production_group` + `part_process_chain` + 迁移 `worker_queue`；migration 021 软删 settings_root + 3 子菜单 + 新增 `process_work_type`（2026-09-12）
- ✅ **API 文档整合**：本目录（2026-09-12；2026-09-29 增 `batches.md`；2026-09-30 增 move + 重构 pool/batches；2026-10-01 增 `pending-programming.md`；2026-10-02 增 `shelf-process-mapping.md`）
- ✅ **prod 容器聚合**（2026-09-19）：5 支撑域平移至 `src/modules/prod/*`，URL 硬切换 `/api/v2/prod/*`，旧 nest 下线无 alias，前端配套 PR 锁步

## 参考

- 集成测试：`tests/work_type_api.rs` / `tests/work_type_process_mapping_api.rs` / `tests/process_api.rs` / `tests/process_chain_api.rs` / `tests/worker_pool_api.rs` / `tests/worker_pool_auto_allocate_api.rs` / `tests/worker_api.rs` / `tests/worker_shelf_deactivate_api.rs` / `tests/production/batch.rs`（2026-09-29 新增）/ `tests/production/pending_programming.rs`（2026-10-01 新增）/ `tests/production/shelf_process.rs`（2026-10-02 新增）
- 模块 README：见各子模块顶层
- 错误码：`src/shared/error.rs::code`
- 前端模块文档：`frontend/docs/03-modules/production/README.md`
- 前端视图目录：`frontend/src/views/production/`
- 报工入口（part 域）：`docs/api/parts/`