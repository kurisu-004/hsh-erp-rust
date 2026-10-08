# prod::batch 域 API（批次流转）—— 剥离中

> 本文件是 `prod::batch` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本域 2026-10-08 同期改动的 `prod::queue` 域契约见 [`queue.md`](queue.md)；
> 2026-10-09 新增的顶层共用端点与外协三合一写端点见 [`outsource.md`](outsource.md)；
> 2026-10-10 的自动选架口径见 [`shelves.md`](shelves.md)（§4 选架算法）。

## 0. 2026-10-10 变更摘要：货架入参全部删除 + 链尾自动送检

1. **本域 7 条写路径不再收货架字段**：`to-inspection`（单件 + 批量）、
   `to-process`、`scan-inspect`、`place-on-shelf` / `release-from-programming`、
   `complete-repair` / `repair-dispatch`、`worker-scan`。目标架改由服务端按
   `current_load / capacity` 升序选（[`shelves.md`](shelves.md) §4）。
2. **worker-scan 新增「链尾自动送检」**：批次在工序链上是最后一道时，放回 = 做完了，
   直接走送检写入路径（响应 `event_type` 会与请求的不同 —— 见 §2.2）。
3. **worker-scan 的 refill 跨架取料**：不再有架锚，跨全部映射该工种工序的活跃生产架
   取料（[`queue.md`](queue.md) §4.1）。
4. **`zone = <其它值> → 20104` 这条错误码消失**（`complete-repair` / `repair-dispatch`
   原先读 `shelf.zone` 分流，现在由 `next_process_id` 有无决定）。
5. **`20507 BIZ_SHELF_PROCESS_NOT_MAPPED` 在本域不再触发**：映射校验已被选架覆盖。

## 1. 本域定位：逐域剥离的**中间态**

batch 域（`t_part_batch` 的批次流转）正在被**逐个端点**拆走。拆分的判据是**前端消费方**（哪个页面的哪个按钮在调它），不是后端逻辑相似度 —— 后端按表 / 状态机聚在一起，前端按页面聚在一起。

**剥离策略**：

1. batch 域先做 `prod::queue` 那一份的剥离（4 个端点，2026-10-08），再做 outsource 那一份（3 个端点，2026-10-09，见 §3）。
2. 后续每一轮某个域重构时**重复这个动作**：认领 §4 表里属于自己目标域的端点，连同 handler / service / repo / VO / DTO 一起搬。
3. 直到 §4 表里的端点被全部分走之后，**再删除本域**。

本文件的存在目的：让下一轮重构的 agent（以及前端）**不翻 Rust 源码**就能知道 batch 域还剩什么、哪些已被认领。契约只从 `docs/api/` 读（前端 CLAUDE.md 规定）。

## 2. 剩余路由表（19 条）

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
| 18 | POST | `/{batch_id}/cancel` | Manager + Clerk |
| 19 | POST | `/{batch_id}/pick-up` | Manager + Clerk + ShelfAccount |

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

### 2.1 入参里的货架字段（2026-10-10 全部删除）

| 端点 | 被删字段 | 去向 |
|---|---|---|
| `POST /{batch_id}/to-inspection` | `target_inspection_shelf_id` | 服务端选品检架（`zone='INSPECTION'`） |
| `POST /to-inspection`（批量） | `target_inspection_shelf_id` | 同上，**循环外选一次**、这批 item 共用 |
| `POST /{batch_id}/to-process` | `shelf_id` | 服务端选生产架（按 `next_process_id` 筛候选） |
| `POST /{batch_id}/scan-inspect` | `target_inspection_shelf_id`、`shelf_id`、`next_process_id` | 三者全删（后两个**本端点从 2026-10-04 起就从不消费**） |
| `POST /{batch_id}/place-on-shelf` / `release-from-programming` | `shelf_id` | 服务端选生产架（`next_process_id` 保留必填） |
| `POST /{batch_id}/complete-repair` / `repair-dispatch` | `shelf_id` | 由 `next_process_id` **有无**决定去向：有 → 生产架、无 → 品检架 |
| `POST /worker-scan` | `shelf_id`、`target_inspection_shelf_id` | 见 §2.2 |

**向后兼容性**：本仓生产代码零 `deny_unknown_fields`，serde 对 struct **默认忽略未知
字段** ⇒ 老客户端多发的 `shelf_id` 会被静默丢弃，「老前端 + 新后端」不破。但
**新客户端发老版本服务端会得 422**（字段必填缺失）⇒ **部署顺序必须后端先上**。

⚠️ **唯一一处移除后语义会漂**：`complete-repair` / `repair-dispatch` 的 `shelf_id`
曾是「去向」的唯一载体（读 `shelf.zone` 分流），老服务端对四种 body 的处理是：

| 老 body | 老服务端行为 | 新服务端行为 |
|---|---|---|
| `{shelf_id: 生产架, next_process_id: X}` | `IN_PROCESS`（回生产） | `IN_PROCESS` ✅ 一致 |
| `{shelf_id: 生产架, 无 next_process_id}` | `20104` 报错 | **`INSPECTION`**（静默改去向） |
| `{shelf_id: 品检架, next_process_id: X}` | `INSPECTION`（该字段被**忽略**） | **`IN_PROCESS`**（静默改去向）⚠️ |
| `{shelf_id: 品检架, 无 next_process_id}` | `INSPECTION` | `INSPECTION` ✅ 一致 |

第三行是更危险的一侧：老服务端在品检分支**完全忽略** `next_process_id`，所以「回品检时
顺带把下一道工序一起发过去」在老系统里是合法且无感的（发不发一个样），新服务端会把它
当成「回生产」—— 把本该返修完送检的货放回产线重跑，而且返回 200。

⇒ **这两个端点的调用方必须改**：回生产时发 `next_process_id`，**回品检时不得发**
`next_process_id`。

#### 2.1.1 选架失败时的错误码

| 目标区 | 错误码 | 语义 |
|---|---|---|
| `PRODUCTION` | `20508 BIZ_SHELF_PROCESS_NOT_FOUND` | 该工序无可用生产货架（没配映射 / 映射的架全停用或软删 / 全非 PRODUCTION / 都不在当前账号 scope 内） |
| `INSPECTION` | `40301 SHELF_MISMATCH` | 当前账号 scope 内**没有**任何可用的 INSPECTION 架 |

`40301` 而不是 `20501` 的理由：典型成因是一个只绑了生产架的 `SHELF_ACCOUNT`，语义是
**无权**而不是「架不存在」。这是**既有约束的延续** —— 自动选架之前，操作员在 UI 上选
品检架时同样会被 `can_access_shelf(target)` 拒（`worker-scan` 的 INSPECTED 分支就是
这条守卫）。只是**触发时机**从「选了一个越权的架」变成「scope 内没有品检架」。

### 2.2 worker-scan 的三条分流（2026-10-10）

请求 `event_type` 只有 `RETURNED` / `INSPECTED` 两个值，但服务端在 `RETURNED` 下还有
第三种去向：

| 请求 `event_type` | 批次在链上的位置 | 实际动作 | 响应 `event_type` | `next_process_id` |
|---|---|---|---|---|
| `RETURNED` | `TAIL`（链内最后一道，指针一致） | **自动送检**（不落生产架） | **`WORKER_SCAN_INSPECTED`** | 可省 |
| `RETURNED` | `NEXT`（指针一致） | 放回服务端选出的生产架并推进工序 | `WORKER_SCAN_RETURNED` | 可省 |
| `RETURNED` | 其余（非顺应：无链 / 链软删 / 指针漂移 / 链内 `process_id` 重复） | 放回服务端选出的生产架 | `WORKER_SCAN_RETURNED` | **必填**，缺 → `40001` |
| `INSPECTED` | 无关 | 送检 | `WORKER_SCAN_INSPECTED` | 无关 |

⚠️ **响应 `event_type` 可能与请求的不同** —— 前端**必须按响应里的 `event_type` 分支**，
不能按自己发的那一个。服务端比前端更清楚批次做完了没有。

**TAIL 判定要求 step 指针一致**（`is_pointer_consistent(&batch) && chain_state == "TAIL"`），
与 NEXT 分支同款：缺了指针一致性这一半，一个指针已漂移的批次只要恰好落进一条单 step
链的锚链里就会被判成「做完了」直接送检，绕过「非顺应 ⇒ 必须显式指定下一道工序」这道
闸门。而指针漂移恰恰说明链信息已不可信。

**WS 广播链路成立且无需改动**：handler 按 `scan_out.event_type`（**响应**里那个值）广播
`WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`，dashboard 两条事件都监听。

**同事务的 refill 跨架取料**：worker-scan 路径给 `refill_for_worker_with_work_type` 传
`shelf_id = None` ⇒ 候选池不限架。链尾自动送检时批次**根本没落任何生产架**，若 refill
仍按架过滤必然查空池。

### 2.3 本域的第二处挂载：`POST /api/v2/batches/split`（1 条，域外）

拆批由 `POST /api/v2/prod/batches/{batch_id}/split` 提升为**顶层共用端点**，旧路径 404 **无 alias**（见 §3）。它**不在** §2 表内、也不在 `ROUTES` 内（`ROUTES` 只描述域内 `router()`），契约如下：

| 方法 | 路径 | 权限 | 入参 | 出参 |
|---|---|---|---|---|
| POST | `/api/v2/batches/split` | Manager + Clerk | `{ batch_id: string, version: number, quantity: number, note? }` | `BatchSplitOut` |

**入参形态逐字段**（2026-10-09 订正）——同一份请求体里字符串 / 数字**混用**，不是笔误：

| 字段 | 线上形态 | 反序列化 |
|---|---|---|
| `batch_id` | **JSON 字符串**（`"1590000000000000001"`） | `shared::types::deserialize_i64`（只吃 `str`）；发数字 → **HTTP 422 纯文本、不进信封**（响应无 `code` 字段） |
| `version` | 裸 JSON 数字，**必填**（无 `#[serde(default)]`，缺字段 → 422 纯文本） | `i32` 原生 |
| `quantity` | **裸 JSON 数字** | `i32` 原生；发字符串 `"4"` → 422 纯文本 |

`version` 是 `t_part_batch.version` 的 OCC 锚；过期 → `40901 VERSION_CONFLICT`。`quantity ∈ [1, batch.quantity - 1]`，越界（`<= 0` 或 `>= batch.quantity`）→ `20111`（HTTP 400）。`batch` 指请求体 `batch_id` 命中的**源批次**行（代码里的局部变量名，见 `service/batch_ops.rs::split_batch`）。

⚠️ `quantity` 是 i32 量级的计数，**不能**挂 `deserialize_i64`（那个 helper 是给雪花 ID 防 JS 精度截断的）：挂在计数上，前端按常规发数字就会吃提取器层 422 纯文本，响应里没有 `code` 字段可供提示。回归由 `tests/part/batch.rs::split_batch_numeric_quantity_with_string_batch_id_succeeds`（数字必通 + 出参三 ID 字符串）与 `split_batch_string_quantity_rejects_with_422_plaintext`（字符串必 422）双锁。

`BatchSplitOut` 五字段：`batch_id` / `new_batch_id` / `part_id`（**三者均为 JSON 字符串**，雪花 ID 走 `serialize_i64`）、`quantity`（数字，实际拆走量）、`source_version`（数字，源批次写入后的 version = 请求 `version + 1`）。

⚠️ 出参形状是**破坏性变更**：旧端点返 `R<i64>`（裸数字）。裸数字在 JS 侧落到 `Number`（2^53-1 之上即失真），而本仓雪花 ID ≈ 9.0e18 比该上限大三个数量级 —— 前端拿到的 `new_batch_id` 会与库里那一行对不上。回归由 `prod::batch::vo::tests::batch_split_out_ids_serialize_as_strings`（lib 单测）与 `tests/part/batch.rs::split_batch_happy_path`（集成）双锁。

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
| `POST /api/v2/prod/batches/{batch_id}/split` | `POST /api/v2/batches/split` | **提为共用顶层端点**：`batch_id` 改入 body + 出参 `R<i64>` → `BatchSplitOut`（见 §2.1） | 2026-10-09 |

**全部无 alias**，旧路径 404。契约细节见 [`queue.md`](queue.md) §1 与 §5.1、[`outsource.md`](outsource.md)。

2026-10-07 一行迁走的是待品检队列读的整条链路（`dto` / `vo` / `model` 行结构 / `repo/list.rs` / `service/list.rs`）→ `prod::inspection`，契约见 [`inspection.md`](inspection.md)。

2026-10-09 外协三端点合并为 `POST /api/v2/outsource-queue/move`（`outsource` 域，硬切无 alias）。一并搬走的代码：`prod::batch/service/outsource.rs`（整文件，含三个 service 方法 + DIRECT 占位报价解析 + 开口 shipment 关闭两个自由函数）、`handler/lifecycle.rs` 的三个 handler、`dto.rs` 的三个入参（`SendToOutsourceRequest` / `ReceiveFromOutsourceRequest` / `ReceiveFromOutsourceToInspectionRequest`）。同时删掉的三个 WS 事件名（`PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED`）合并为 `OUTSOURCE_MOVE_DONE`；`t_part_event` 的审计字面量 `SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` 逐字保留（第三个 `RECEIVED_TO_INSPECTION` 随 2026-10-10 的 `OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向下线而不再被写入，见 §0b）。契约见 [`outsource.md`](outsource.md)。

2026-10-09 拆批端点提升为顶层共用端点。它的消费方有**三处**（生产队列看板 / 外协看板 / 零件详情页），按「目标域按前端消费方判定」的规约它属多域共用 —— 挂在 `/prod/batches/{batch_id}/…` 这条「批次子资源」路径下既不贴切、也拿不掉路径参数（顶层前缀下 `/{id}/…` 会与别的 `/batches/*` 端点争 matchit 段位）。代码侧 `handler::split_router` 是**第二个** router 工厂（不动 `router()`），经 `prod::split_router()` 转发、由 `src/modules/mod.rs::v2_router()` 的 `.nest("/batches", …)` 挂载。service / repo / WS 事件名 `PART_BATCH_SPLIT` 与 payload **一行未改**，只有入参形状（`batch_id` 入 body）与出参（`BatchSplitOut`）变了。

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
| `POST /{batch_id}/cancel` | `views/parts/detail/` |
| `POST /{batch_id}/pick-up` | `views/scan/`（扫码台） |
| ~~`POST /{batch_id}/split`~~ | ~~`views/parts/detail/`~~（2026-10-09 已提为共用顶层端点 `POST /batches/split`，三处消费） |
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
| 批次在工序链上的位置派生（锚链两步定位 / 「下一道」/ 顺应工序判据） | `shared::batch::chain.rs`（2026-10-09 新增） | `prod::batch::service::worker_scan`（RETURNED）/ `prod::queue::service::dispatch` / part 读侧 `list_by_worker` / 4 处卡片 DTO 的 `has_process_chain` |

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
| `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` / `receive-from-outsource` / `receive-from-outsource-to-inspection` | 合并为 `POST /api/v2/outsource-queue/move`（2026-10-09，硬切无 alias）。**部分收发能力随之下线**（入参不再有 `quantity`），部分流转改走 `POST /api/v2/batches/split` |
| `POST /api/v2/prod/batches/{batch_id}/split` | 提为共用顶层端点 `POST /api/v2/batches/split`（2026-10-09，硬切无 alias，`batch_id` 入 body，出参 `R<i64>` → `BatchSplitOut`）。见 §2.1 |
| `service/outsource.rs`（整文件）+ `dto.rs` 的三个外协入参 | 随三端点迁往 `outsource::service::move_svc` / `outsource::dto`（2026-10-09） |
| WS 事件名 `PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED` | 合并为 `OUTSOURCE_MOVE_DONE`（2026-10-09）。`t_part_event` 的 `SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE` 不变（`RECEIVED_TO_INSPECTION` 随 2026-10-10 的 `OUTSOURCE_COMPANY → INSPECTION_SHELF` 方向下线而不再被写入，见 §0b） |
| `GET /api/v2/prod/batches/pending` / `POST /dispatch` / `POST /auto-dispatch` | 迁往 `prod::queue`（2026-10-08） |
| `POST /api/v2/prod/batches/{batch_id}/recall-to-pending` | 迁往 `POST /api/v2/prod/queue/recall`（2026-10-08），`batch_id` 改入 body、出参改 `RecallOut` |
| `repo/list.rs` + `service/list.rs` | 随待品检队列读迁往 `prod::inspection`（2026-10-07） |
| `status_gate.rs` / `service/guard.rs` / `model::TPartBatch` | 上移 `shared::batch`（2026-10-08），见 §5.1 |
| `repo/mod.rs` 的 ZST `BatchRepo` | 迁 `prod::queue::repo::dispatch` 并改名 `QueueDispatchRepo`（2026-10-08） |
| 7 条写路径的货架入参（`shelf_id` / `target_inspection_shelf_id`，`scan-inspect` 的前兼容 `shelf_id` + `next_process_id`） | 2026-10-10：目标架改由服务端按负载自动选。逐条见 §2.1 |
| `service::guard` / `shared::batch::guards::assert_shelf_maps_process` | 2026-10-10：「架必须映射该工序」这条守卫已被选架的候选集覆盖，零调用方 ⇒ 删除。选架 SQL 里带 `t_shelf_process` 的 `EXISTS` + 软删闸门 |
| `_validate_inspection_shelf` / `_validate_production_shelf_and_process` | 2026-10-10：纯守卫函数，被选架覆盖 ⇒ 删除 |
| `ShelfRepo` 直查（`complete-repair` / `repair-dispatch` 读 `shelf.zone` 分流） | 2026-10-10：去向改由 `next_process_id` 有无决定 |

## 8. 已知偏差登记

- **§2 路由表与 §4 剥离登记表是手写摘要，权威源是 `handler/mod.rs::ROUTES` + `STRIP_TARGETS`。** 两者由 `modules::prod::batch::handler::tests` 两条单测与 `router()` 源码比对，但**那两条单测不校验本文件**。改路由时若忘了同步本文件，本文件会静默过期 —— 以单测为准。
- **`ROUTES` 只描述域内 `router()`，不含 §2.1 的顶层 `/batches/split`。** 加顶层挂载不会让那两条单测红，反过来也一样：改 `split_router()` 时没有编译期/单测层面的登记表兜底，只有 §2.1 这段手写摘要与集成测试 `tests/part/batch.rs::split_batch_*`。
- **§4 登记表里「多域共用」的几条尚未决定归属。** 搬之前需要先决定它归哪个域（取决于哪个页面先重构），不要两边都搬。
- **`{batch_id}` 非数字段返 400 纯文本而非 `R<T>` 信封**（`PathRejection` 的默认行为，全仓一致）。前端若按 `code` 分支解析错误，需要对 400 的纯文本单独兜底。
- **`zone = <其它值> → 20104` 这条错误码已不存在**（2026-10-10）。`complete-repair` /
  `repair-dispatch` 原先读 `shelf.zone` 分流，而 `t_shelf.zone` 只有
  `PRODUCTION` / `INSPECTION` 两个合法值（DB 层有 CHECK）⇒ 那条分支**不可达**。现在
  去向由 `next_process_id` 有无决定、`zone` 根本不参与判定，错误码改由「该 zone 无可用
  货架」承担（生产 `20508` / 品检 `40301`）。
- **`20507 BIZ_SHELF_PROCESS_NOT_MAPPED` 在本域不再触发**（2026-10-10）。它原先守的是
  「你指定的这个架没有映射这道工序」；选架的候选集本身就只含映射了该工序的架，判据
  没被削弱、只是换了个更前置的形态（没有映射 ⇒ 候选为空 ⇒ 选不出）。
  ⚠️ **2026-10-10 起该码在全仓零触发点** —— 最后一个触发点（`prod::queue::move` 的
  WORKER→POOL 映射校验）随 `to` 侧货架入参移除一并退场，只剩 `shared::error` 里的
  常量与码名测试。前端若还有分支 20507 的代码，那是死分支。
