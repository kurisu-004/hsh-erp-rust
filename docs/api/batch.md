# prod::batch 域 API（批次流转）—— 剥离中

> 本文件是 `prod::batch` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本域 2026-10-08 同期改动的 `prod::queue` 域契约见 [`queue.md`](queue.md)。

## 1. 本域定位：逐域剥离的**中间态**

batch 域（`t_part_batch` 的批次流转）正在被**逐个端点**拆走。拆分的判据是**前端消费方**（哪个页面的哪个按钮在调它），不是后端逻辑相似度 —— 后端按表 / 状态机聚在一起，前端按页面聚在一起。

**剥离策略**：

1. batch 域先做 `prod::queue` 那一份的剥离（4 个端点，2026-10-08），再做 outsource 那一份（3 个端点，2026-10-09，见 §3）。
2. 后续每一轮某个域重构时**重复这个动作**：认领 §4 表里属于自己目标域的端点，连同 handler / service / repo / VO / DTO 一起搬。
3. 直到 §4 表里的端点被全部分走之后，**再删除本域**。

本文件的存在目的：让下一轮重构的 agent（以及前端）**不翻 Rust 源码**就能知道 batch 域还剩什么、哪些已被认领。契约只从 `docs/api/` 读（前端 CLAUDE.md 规定）。

## 2. 剩余路由表（20 条）

全部挂 `/api/v2/prod/batches`。`/{batch_id}` 是 path 段；`to-ship` / `to-inspection` / `worker-scan` / `repair` / `repairing` / `scan/deliver` 是静态段（**无 Path extractor**）。

| # | 方法 | 路径（相对 `/api/v2/prod/batches`） | 权限 |
|---|---|---|---|
| 1 | POST | `/to-ship` | Manager + Inspector |
| 2 | POST | `/to-inspection` | Manager + Inspector |
| 3 | POST | `/worker-scan` | Manager + ShelfAccount |
| 4 | GET | `/repair` | Manager + Inspector |
| 5 | GET | `/repairing` | Manager + Inspector |
| 6 | POST | `/scan/deliver` | Manager + Inspector |
| 7 | POST | `/{batch_id}/to-inspection` | Manager + Inspector |
| 8 | POST | `/{batch_id}/to-ship` | Manager + Inspector |
| 9 | POST | `/{batch_id}/to-process` | Manager + Inspector |
| 10 | POST | `/{batch_id}/scan-inspect` | Manager + Inspector |
| 11 | POST | `/{batch_id}/deliver` | Manager + Inspector |
| 12 | POST | `/{batch_id}/complete` | Manager + Inspector |
| 13 | POST | `/{batch_id}/start-repair` | Manager + Inspector |
| 14 | POST | `/{batch_id}/place-on-shelf` | Manager + Clerk |
| 15 | POST | `/{batch_id}/release-from-programming` | Manager + Clerk |
| 16 | POST | `/{batch_id}/complete-repair` | Manager + Clerk |
| 17 | POST | `/{batch_id}/repair-dispatch` | Manager + Clerk |
| 18 | POST | `/{batch_id}/split` | Manager + Clerk |
| 19 | POST | `/{batch_id}/cancel` | Manager + Clerk |
| 20 | POST | `/{batch_id}/pick-up` | Manager + Clerk + ShelfAccount |

- 全部返回统一信封 `R { code, message, data }`。
- 写端点的事务边界在 handler（`state.pool.begin()` → service → `tx.commit()`），**WS 广播在 commit 之后**。
- 读端点（`/repair` `/repairing`）走 `pool.acquire()` 不开事务。
- `/{batch_id}` 抽不出数字时走 axum 的 `PathRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**（全仓 `Path<i64>` 端点的统一行为）。
- **权威路由清单**是 `src/modules/prod/batch/handler/mod.rs::ROUTES`，§4 剥离登记表
  按同序同长度给出目标域（`STRIP_TARGETS`）。单测
  `modules::prod::batch::handler::tests::routes_declared_in_router` 从 `router()`
  源码里解析出全部 `.route("…", get|post(…))`，断言与 `ROUTES` 逐条一致 ——
  往 `router()` 加一条路由而忘了更新 `ROUTES` / `STRIP_TARGETS`，该单测立刻红。
  本表与 §4 是给人读的摘要，**改路由时以那条单测为准**。

## 3. 已被认领并移走的端点

| 原路径 | 新路径 | 变更 | 剥离轮次 |
|---|---|---|---|
| `GET /api/v2/prod/batches/inspection` | `GET /api/v2/prod/inspection/queue` | 路径 | 2026-10-07 |
| `GET /api/v2/prod/batches/pending` | `GET /api/v2/prod/queue/pending` | 路径 | 2026-10-08 |
| `POST /api/v2/prod/batches/dispatch` | `POST /api/v2/prod/queue/dispatch` | 路径 | 2026-10-08 |
| `POST /api/v2/prod/batches/auto-dispatch` | `POST /api/v2/prod/queue/auto-dispatch` | 路径 | 2026-10-08 |
| `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` | `POST /api/v2/prod/queue/recall` | 路径 + `batch_id` 改入 body + 出参 `PartOut` → `RecallOut` | 2026-10-08 |
| `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` | `POST /api/v2/outsource-queue/move` | **三合一**：`from`/`to` 结构体 + `batch_id` 改入 body + 去 `quantity` / `process_id` + 出参 `PartOut` → `OutsourceMoveResult` | 2026-10-09 |
| `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource` | `POST /api/v2/outsource-queue/move` | 同上（`to.kind = PRODUCTION_SHELF` 臂；`next_process_id` 可省略，后端按工序链推导） | 2026-10-09 |
| `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` | `POST /api/v2/outsource-queue/move` | 同上（`to.kind = INSPECTION_SHELF` 臂） | 2026-10-09 |

**全部无 alias**，旧路径 404。契约细节见 [`queue.md`](queue.md) §1 与 §5.1。

2026-10-07 一行迁走的是待品检队列读的整条链路（`dto` / `vo` / `model` 行结构 / `repo/list.rs` / `service/list.rs`）→ `prod::inspection`，契约见 [`inspection.md`](inspection.md)。

2026-10-09 外协三端点合并为 `POST /api/v2/outsource-queue/move`（`outsource` 域，硬切无 alias）。一并搬走的代码：`prod::batch/service/outsource.rs`（整文件，含三个 service 方法 + DIRECT 占位报价解析 + 开口 shipment 关闭两个自由函数）、`handler/lifecycle.rs` 的三个 handler、`dto.rs` 的三个入参（`SendToOutsourceRequest` / `ReceiveFromOutsourceRequest` / `ReceiveFromOutsourceToInspectionRequest`）。同时删掉的三个 WS 事件名（`PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED`）合并为 `OUTSOURCE_MOVE_DONE`；`t_part_event` 的三个审计字面量（`SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` / `RECEIVED_TO_INSPECTION`）逐字保留。契约见 `src/modules/outsource/{dto.rs,vo/queue.rs,service/move.rs,handler/move.rs}` 的模块 doc（outsource 域暂无 `docs/api/` 文件）。

2026-10-08 一并搬走的代码：`service/dispatch.rs`、`handler/dispatch.rs`、`repo/mod.rs`（ZST `BatchRepo` → `queue/repo/dispatch.rs` 的 `QueueDispatchRepo`）、`service/shelf.rs` 的 `recall_to_pending` → `queue/service/recall.rs`、`vo.rs` 的 5 类下发流出参 → `queue/vo/queue.rs`、`dto.rs` 的 5 个下发流入参 → `queue/dto.rs`。

## 4. 剥离登记表（路由 → 目标域）

**目标域按前端消费方判定**。一条端点被多个页面消费时归「多域共用」，由后续某一轮自行认领。

| 端点 | 目标域 |
|---|---|
| `POST /to-ship`、`POST /to-inspection`、批量同形两条 | 多域共用（`views/inspection/` + `views/delivery/`） |
| `POST /worker-scan` | `views/scan/`（扫码台） |
| `POST /scan/deliver` | `views/scan/`（扫码台） |
| `POST /{batch_id}/to-inspection` / `to-ship` / `to-process` | 多域共用（`views/inspection/` + `views/delivery/`） |
| `POST /{batch_id}/scan-inspect` | `views/scan/`（扫码台） |
| `POST /{batch_id}/deliver` | `views/delivery/`（送货单域） |
| `POST /{batch_id}/complete` | 多域共用（`views/parts/` + `views/assemblies/` + `views/statistics/`） |
| `POST /{batch_id}/start-repair`、`complete-repair`、`repair-dispatch`、`GET /repair`、`GET /repairing` | `views/repair/` |
| `POST /{batch_id}/place-on-shelf` | 待定（消费方是零件列表页，非队列页） |
| `POST /{batch_id}/release-from-programming` | `views/cnc/` |
| ~~`POST /{batch_id}/send-to-outsource`、`receive-from-outsource`、`receive-from-outsource-to-inspection`~~ | ~~`views/outsource/`~~（2026-10-09 已剥离，三合一为 `outsource::queue`） |
| `POST /{batch_id}/split`、`POST /{batch_id}/cancel` | `views/parts/detail/` |
| `POST /{batch_id}/pick-up` | `views/scan/`（扫码台） |
| ~~`GET /inspection`~~ | ~~`prod::inspection`~~（2026-10-07 已剥离） |
| ~~`GET /pending` `POST /dispatch` `POST /auto-dispatch` `POST /{batch_id}/recall-to-pending`~~ | ~~`prod::queue`~~（2026-10-08 已剥离） |
| ~~`POST /{batch_id}/send-to-outsource`、`receive-from-outsource`、`receive-from-outsource-to-inspection`~~ | ~~`outsource::queue`~~（2026-10-09 已剥离，三合一） |

⚠️ 「多域共用」的几条是后续某一轮的**决策点**：搬之前需要先决定它归哪个域（取决于哪个页面先重构），不要两边都搬。

## 5. 域边界与公共设施

### 5.1 已上移到 `shared::batch` 的四处（2026-10-08）

| 原位置 | 新位置 | 谁在用 |
|---|---|---|
| `prod::batch::status_gate.rs` | `shared::batch::status.rs` | **全仓所有碰批次的域**（写 `t_part_batch.status` 的唯一入口 + 派生链） |
| `prod::batch::service::guard.rs` | `shared::batch::guards.rs` | `prod::batch` / `prod::queue` / `prod::shelf_process` / part / outsource |
| `prod::batch::model::TPartBatch` | `shared::batch::model.rs` | 十个域直接引用（prod / part / delivery_note / outsource / wx / statistics / admin / dashboard …） |
| `PartBatchRepo::get_by_id` / `list_active_by_part_id` | `shared::batch::read.rs` | prod::batch / prod::queue / delivery_note / part / 派生链 |

迁移动机：这四处都是**所有碰批次的域都要用**的公共设施，与「批次有哪些业务用例」无关。留在 batch 域意味着每剥离一个新域就多一条指向 batch 域的反向依赖。上移后依赖方向与派生图方向一致（上层域 → shared）。

**边界记档**：`shared::batch` 是本仓唯一经域 repo **写**库的 shared 模块（`PartRepo::update_part_rollup` / `PartRepo::insert_part_event` / `AssemblyService::sync_assembly_status`），也是唯一依赖 **4 个域** 的 shared 模块 —— 另两个是**只读**单表查询：`shelf::ShelfRepo::get_by_id_zone`（`guards::validate_shelf_zone` 的存在 / 停用 / zone 三谓词）与 `prod::process_chain::ProcessChainRepo::resolve_step_id_by_process`（`guards::optional_step_id`）。写库那 3 处对应「状态派生契约」三层派生图（`t_part_batch.status` → `t_part.status` / `next_process_id` → `t_assembly.status`）的实现本身，该契约天然跨域、无域可归属。详见 `src/shared/batch/mod.rs` 与 `src/shared/mod.rs`。

### 5.2 仍留在 batch 域的

- `repo/queries.rs`（ZST `PartBatchRepo`，`t_part_batch` 通用 SQL 真源）
- `repo/sql.rs`（inspection / lifecycle 流转的 19 个定位 + 写点，全部是写入口之上的薄包装）
- `repo/trait.rs`（胖 trait `PartBatchRepoTrait` + `impl for &mut PgConnection`）
- `model.rs`（2 个**窄投影**行结构：`RecentBatchRow` / `PartBatchScanRow`）
- `service/`（除已剥离的 `dispatch.rs`、`outsource.rs` 与 `shelf.rs::recall_to_pending` 外的全部用例；返修两条集合读的 SQL 在 `service/repair.rs::list_batches_matching` 内联自建）
- `dto.rs`（除已剥离的 5 个下发流入参与 3 个外协入参外的全部）
- `vo.rs`（除已剥离的 5 类下发流出参外的全部）

## 6. 状态派生契约（未变，搬域不搬契约）

```
t_part_batch.status            ← 唯一真源
   │  shared::batch::status::rollup_part_derived（min-progress）
   ▼
t_part.status / next_process_id   ← 派生缓存
   │  assembly::compute_assembly_target
   ▼
t_assembly.status               ← 派生缓存
```

- **写 `t_part_batch.status` 只能走 `shared::batch::status::apply_batch_status_change`**。它一函数内完成「写批次（OCC + SQL 层源状态白名单）→ 回填 part 派生列 → 级联 assembly → 终态序列号归档 / 释放」，caller 没有「要不要顺手调 sync」这个选项。
- **CI 强制**：`cargo test --lib` 的 `shared::batch::status::write_guard_tests::no_outside_file_writes_batch_status` 扫全 `src/**/*.rs`，除 `src/shared/batch/status.rs` 外任何文件写 `UPDATE t_part_batch SET status …` 即失败。该测试的允许路径是一处字面量常量，**写入口再搬家时必须同步改它**。
- **派生层不得覆盖主操作**：`t_part` 已由主操作写成终态时，min-progress 不许写回去（SQL 层 `AND status NOT IN ('COMPLETED','CANCELLED')` 兜底）。
- 兜底对账端点 `POST /api/v2/admin/recompute-rollup`（Manager）复用同一套 rollup 函数，**幂等**。

## 7. 移除记录

| 被移除项 | 原因 |
|---|---|
| `GET /api/v2/prod/batches/inspection` | 迁往 `GET /api/v2/prod/inspection/queue`（2026-10-07） |
| `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` / `receive-from-outsource` / `receive-from-outsource-to-inspection` | 合并为 `POST /api/v2/outsource-queue/move`（2026-10-09，硬切无 alias）。**部分收发能力随之下线**（入参不再有 `quantity`），部分流转改走 `POST /api/v2/prod/batches/{batch_id}/split` |
| `service/outsource.rs`（整文件）+ `dto.rs` 的三个外协入参 | 随三端点迁往 `outsource::service::move_svc` / `outsource::dto`（2026-10-09） |
| WS 事件名 `PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED` | 合并为 `OUTSOURCE_MOVE_DONE`（2026-10-09）。`t_part_event` 的三个审计字面量不变 |
| `GET /api/v2/prod/batches/pending` / `POST /dispatch` / `POST /auto-dispatch` | 迁往 `prod::queue`（2026-10-08） |
| `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` | 迁往 `POST /api/v2/prod/queue/recall`（2026-10-08），`batch_id` 改入 body、出参改 `RecallOut` |
| `repo/list.rs` + `service/list.rs` | 随待品检队列读迁往 `prod::inspection`（2026-10-07） |
| `status_gate.rs` / `service/guard.rs` / `model::TPartBatch` | 上移 `shared::batch`（2026-10-08），见 §5.1 |
| `repo/mod.rs` 的 ZST `BatchRepo` | 迁 `prod::queue::repo::dispatch` 并改名 `QueueDispatchRepo`（2026-10-08） |

## 8. 已知偏差登记

- **§2 路由表与 §4 剥离登记表是手写摘要，权威源是 `handler/mod.rs::ROUTES` + `STRIP_TARGETS`。** 两者由 `modules::prod::batch::handler::tests` 两条单测与 `router()` 源码比对，但**那两条单测不校验本文件**。改路由时若忘了同步本文件，本文件会静默过期 —— 以单测为准。
- **§4 登记表里「多域共用」的几条尚未决定归属。** 搬之前需要先决定它归哪个域（取决于哪个页面先重构），不要两边都搬。
- **`{batch_id}` 非数字段返 400 纯文本而非 `R<T>` 信封**（`PathRejection` 的默认行为，全仓一致）。前端若按 `code` 分支解析错误，需要对 400 的纯文本单独兜底。
