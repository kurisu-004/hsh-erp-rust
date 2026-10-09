# prod::scan 域 API（报工台：扫工牌 + 取件 / 放回列表 + 放回送检 + 手动领取）

> 本文件是 `prod::scan` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本域同批改动的契约见 [`batch.md`](batch.md)（worker-scan / pick-up 的旧出处）、
> [`queue.md`](queue.md)（refill 与同事务编排）、[`shelves.md`](shelves.md)（自动选架）、
> [`iam.md`](iam.md)（货架实体）。
>
> ⚠️ part 域**没有** `docs/api/` 契约文档（`docs/api/` 清单里没有它），两条 list
> 端点的旧出处只能从代码追：`src/modules/part/handler/lifecycle.rs`（旧 handler
> 所在文件，2026-10-10 起已无这两条路由）与 `src/modules/prod/scan/listing/`
> （新址）。完整的旧 → 新对照见 §6.1 移除记录表。

## 0. 变更摘要

### 0.1 2026-10-11：worker-scan 支持部分数量

`WorkerScanRequest` 新增 `quantity`（**JSON 字符串**），让放回 / 送检都能指定
「这次只处理这么多件」。逐字段与三种落法见 §2.4，拆批落点见 §2.4.1。

- **向后兼容**：缺省即整批，行为与该字段引入前**逐字一致**；老客户端不多发这个键
  照常工作（生产代码零 `deny_unknown_fields`）。
- **拆批复用**已有的 `split_batch_for_partial_pass`（`prod::batch` 的共用拆批 SQL，
  pick-up / to_ship / to_process / to_inspection 四处已在用），不新写 SQL。
- **余量的去向与 pick-up 相反**：worker-scan 的余量继承 `current_holder_id`
  **留在工人手上**（继续出现在「已持有」列表）；pick-up 的余量留在原处（架上）。
- **响应** `scan.batch_id` 改为返回「**本次实际被处理的那一批**」（拆批场景 = 新
  批次）；JSON 形状仍是那 6 个键。

### 0.2 2026-10-10：报工台立域

把**报工台（工人扫码台）的 5 个端点**从原先散落在三个域的状态收拢进 `prod::scan`
一个域，URL 全部挂 `/api/v2/prod/scan/*`，**硬切、无 alias**。

1. **立域**：`prod::scan`（URL `/api/v2/prod/scan/*`，5 端点）。判定依据是**前端
   消费方** —— 5 条端点的唯一消费方是 `views/production/scan/` 三页（取件 / 放回 /
   送检）+ 扫工牌弹窗 + 队列看板的一条动作。后端看它们分属三域、依赖完全不同的
   模块；从工厂现场看它们是**一台机器的五个按钮**。
2. **行 VO 收敛**：`PartListItem`（40 字段，其余 3 个域的 4 个端点仍在下发）→
   `ScanListItem`（17 字段）。
   报工台**零消费**的 23 个占位字段（`applicant_name` 写死空串、`customer_id` 写死
   `0`、`status` 写死 `"IN_PROCESS"`、4 个审计字段写死 epoch、`unit_price` /
   `total_price` 写死 `"0"`…）不再下发。逐字段证据见 §2.2。
3. **出参收敛**：`WorkerOut`（11 字段）→ `ScanWorkerBrief`（4 字段）。报工台合计只读
   `id` / `badge_code` / `name` / `work_type_id`。`WorkerOut` **不删** ——
   `GET /prod/workers/{id}` 与 worker 列表端点仍在用它。
4. **入参形态变更**：两条 list 端点的过滤键由 **path 参数改 query 参数**
   （`work_type_id` / `worker_id`，必填）；`?shelf_id=` **删除**（批次 B 已让选架
   完全自动，这个入参零消费方）。
5. **域隔离护栏**：`listing/` 子模块（2 条只读聚合）单独装
   `shared::domain_guard::assert_no_foreign_domain`。整域**不适用**该护栏 ——
   `worker_scan` 是转发型用例，必然 import 5 个域。

## 1. 端点表

| # | 方法 | 新路径 | 旧路径（**无 alias**） | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|---|
| 1 | POST | `/api/v2/prod/scan/verify-badge` | `POST /api/v2/prod/workers/verify-badge` | **任意已登录**（含 SHELF_ACCOUNT） | `{ badge_code }` | `ScanWorkerBrief` |
| 2 | GET | `/api/v2/prod/scan/pickable?work_type_id=&limit=&offset=` | `GET /api/v2/parts/pickable-by-work-type/{work_type_id}` | Manager + Clerk + Inspector + ShelfAccount | `work_type_id` **必填**（字符串）、`limit?`（缺省 50 / clamp 1..200）、`offset?` | `ScanListOut` |
| 3 | GET | `/api/v2/prod/scan/held?worker_id=&limit=&offset=` | `GET /api/v2/parts/by-worker/{worker_id}` | Manager + Clerk + Inspector + ShelfAccount | `worker_id` **必填**（字符串）、`limit?`、`offset?` | `ScanListOut` |
| 4 | POST | `/api/v2/prod/scan/worker-scan` | `POST /api/v2/prod/batches/worker-scan` | Manager + ShelfAccount | `WorkerScanRequest`（6 字段，`quantity?` 为 **JSON 字符串**） | `WorkerScanOut` |
| 5 | POST | `/api/v2/prod/scan/batches/{batch_id}/pick-up` | `POST /api/v2/prod/batches/{batch_id}/pick-up` | Manager + Clerk + ShelfAccount | path `batch_id` + `PickUpRequest` | `R<PartOut>` |

- 全部返回统一信封 `R { code, message, data }`。
- 端点 2 / 3 是**纯读**（`pool.acquire()` 不开事务、不发 WS 广播）；端点 1 / 4 / 5
  开事务，**广播在 commit 之后**。
- 端点 2 / 3 的 `limit` **缺省 50**：想要全部必须显式传 `limit=200`（clamp 上限），
  否则静默截断。`offset` 缺省 0。
- `work_type_id` / `worker_id` 走 `deserialize_i64`（**只接受 JSON 字符串**），
  发 JSON number → axum `QueryRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**。
  漏传 → 同样 400 纯文本（无 `#[serde(default)]`）。
- body 里的两个数量 / 消歧入参（`WorkerScanRequest.batch_id` /
  `WorkerScanRequest.quantity`、`PickUpRequest.shelf_id` /
  `PickUpRequest.quantity`）走 `deserialize_i64_opt`，**同样只接受 JSON 字符串**；
  发数字 → axum `JsonRejection` → **HTTP 422 纯文本**。
- i64 雪花主键一律序列化为 JSON **string**，防 JS `Number` 精度截断。
- 端点 1 的两个业务出口：工牌不存在 → `20201 BIZ_WORKER_NOT_FOUND`（**HTTP 404**）；
  存在但停用 → `20202 BIZ_WORKER_INACTIVE`（**HTTP 400**）。两者靠
  `include_deleted=true` 分流。

### 1.1 路由注册顺序（有硬约束）

`/batches/{batch_id}/pick-up`（2 段，首段静态 `batches`）与任何 2 段首段动态的路由
**段数相同**，靠 matchit 的「静态段优先」消解。本域当前没有 2 段首段动态路由，故
注册顺序不影响结果；将来新增时静态组**必须在前**（`src/modules/prod/batch/handler/mod.rs`
的 `/scan/deliver` 是同款先例）。

### 1.2 旧路径的实际失效形态（实跑结论，勿凭直觉改写）

**5 条里 4 条是 404、1 条是 405，没有一条是 400。**

| 旧路径 | 实得 | 成因 |
|---|---|---|
| `GET /parts/pickable-by-work-type/{id}` | **404** | part 域的 2 段路由（`/{part_id}/update` 等）第二段是**字面量**，`matchit` 匹配不上 `{id}` ⇒ 无路由命中。`Path<i64>` 提取器**根本没机会**拒绝 |
| `GET /parts/by-worker/{id}` | **404** | 同上 |
| `POST /prod/workers/verify-badge` | **405** | 落进 worker 域的 `/{id}`，但那条**只注册了 GET** ⇒ 方法不匹配 |
| `POST /prod/batches/worker-scan` | **404** | 整个路由段已不存在 |
| `POST /prod/batches/{id}/pick-up` | **404** | 同上 |

⚠️ **教训登记**：跨域硬切时「落进 catch-all ⇒ 400」这个直觉**不总成立**。`/shelves/*`
是干净 404、`/iam/shelves/for-return` 因落进 catch-all 才是 400 —— 差别在「被
catch-all 吃掉的那条路径，它的动态段是不是唯一一段」。part 域 `/{part_id}` 是
**1 段**路由，2 段路径根本够不着它。

回归测试：`tests/production/scan_listing.rs::old_scan_paths_are_gone`（4 条）+
`tests/production/scan_badge.rs::old_verify_badge_path_is_gone`（1 条）。

## 2. 逐字段

### 2.1 `ScanWorkerBrief`（端点 1，4 字段）

| 字段 | 类型 | 来源 | 报工台的用途 |
|---|---|---|---|
| `id` | string | `t_worker.id`（`serialize_i64`） | 发 `GET /scan/held?worker_id=` |
| `badge_code` | string | `t_worker.badge_code` | 顶栏显示 + worker-scan 的 `badge_code` 入参 |
| `name` | string | `t_worker.name` | 顶栏显示 |
| `work_type_id` | string \| null | `t_worker.work_type_id`（`serialize_i64_opt`） | 发 `GET /scan/pickable?work_type_id=` |

**砍掉 7 个字段的 grep 证据**（前端 `views/scan/` 四个组件 + 三个 composable 全量
grep `worker?.<字段>`，命中仅这 4 个）。⚠️ 下列路径是**迁移前路径**：报工台视图目录
2026-10-10 自 `src/views/scan/` 搬进 `src/views/production/scan/`，本块是迁移**当时**
跑出来的原始输出，保留原样以存证：

```
$ grep -rno 'worker??\.\(id\|name\|badge_code\|work_type_id\|work_type_name\|id_card_no\|phone\|is_active\|version\|created_at\|updated_at\)' src/views/scan/ src/composables/ | sort | uniq -c
   1 src/views/scan/ScanReturnParts.vue:77:worker?.id
   1 src/views/scan/ScanReturnParts.vue:54:worker?.id
   1 src/views/scan/ScanReturnParts.vue:47:worker?.badge_code
   1 src/views/scan/ScanReturnParts.vue:45:worker?.name
   1 src/views/scan/ScanPickParts.vue:80:worker?.work_type_id
   1 src/views/scan/ScanPickParts.vue:59:worker?.id
   1 src/views/scan/ScanPickParts.vue:52:worker?.badge_code
   1 src/views/scan/ScanPickParts.vue:50:worker?.name
   1 src/views/scan/ScanInspectParts.vue:66:worker?.id
   1 src/views/scan/ScanInspectParts.vue:43:worker?.id
   1 src/views/scan/ScanInspectParts.vue:36:worker?.badge_code
   1 src/views/scan/ScanInspectParts.vue:34:worker?.name
   1 src/views/scan/ScanActionPicker.vue:33:worker?.badge_code
   1 src/views/scan/ScanActionPicker.vue:31:worker?.name
```

被砍的 7 个：`id_card_no` / `phone` / `is_active` / `work_type_name` / `version` /
`created_at` / `updated_at`。其中 `work_type_name` 在 `verify_badge` 路径上本就恒
`null`（service 不做工种名回填，只 `GET /workers/{id}` 与列表端点才填）。

**`WorkerOut` 不删**：`GET /api/v2/prod/workers/{id}` 与 worker 列表端点仍在用它，
只是不再被 `verify_badge` 引用。

形状护栏：`vo/worker.rs::tests::keys_are_exactly_four` +
`tests/production/scan_badge.rs::verify_badge_response_has_exactly_four_keys`。

### 2.2 `ScanListItem`（端点 2 / 3，17 字段）

行单位是**批次**（取行 SQL 从 `t_part_batch` 起），锚点是 `batch_id` /
`batch_version`。

| 字段 | 类型 | `pickable` | `held` | 来源 |
|---|---|---|---|---|
| `id` | string | 有值 | 有值 | `p.id` |
| `serial_no` | string \| null | 有值 | 有值 | `p.serial_no`（nullable：手工工单无序列号） |
| `name` | string | 有值 | 有值 | `p.name`（真实值，非图号副本） |
| `drawing_no` | string | 有值 | 有值 | `p.drawing_no` |
| `quantity` | number | 有值 | 有值 | **`pb.quantity`**（批次数量，不是 `p.quantity`） |
| `is_urgent` | boolean | 有值 | 有值 | `p.is_urgent` |
| `planned_delivery_date` | string | 有值 | 有值 | `p.planned_delivery_date`（**真实值**，非 `1970-01-01`） |
| `system_delivery_date` | string \| null | 有值 | 有值 | `p.system_delivery_date`（可空列） |
| `process_chain_id` | string \| null | **恒 null** | 有值 | `held` 侧投影 `p.process_chain_id`；`pickable` 侧投影 `NULL::bigint` |
| `has_process_chain` | boolean | 有值 | 有值 | `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR` |
| `chain_state` | string | **恒 `"NONE"`** | 有值 | `COALESCE(nx.chain_state, 'NONE')` |
| `chain_next_process_id` | string | **恒 `"0"`** | 有值 | `COALESCE(nx.next_process_id, 0)` + `serialize_i64` |
| `chain_next_process_name` | string \| null | **恒 null** | 有值 | `np.name` |
| `chain_current_process_name` | string \| null | **恒 null** | 有值 | `cp.name` |
| `batch_id` | string | 有值 | 有值 | `pb.id`（pick-up 路径参数 + OCC 锚；worker-scan 消歧入参） |
| `batch_version` | number \| null | 有值 | 有值 | `pb.version`（批次 OCC，**不可与 part 级 version 混用**） |
| `location` | **恒 null** | 恒 null | 恒 null | 两条端点不做 batch enrichment。**键必须保留**（见下） |

**为什么 `pickable` 侧的链四字段是占位**：链位置是**批次级**事实，而 `pickable` 侧
那行「还没被领走」，报工台不需要知道它的下一道工序（放回分流只在放回页做，放回页读
的是 `held`）。取保守默认（`NONE` / `"0"` / `null` / `null`）而不是编一个值 ——
`NONE` 的语义是「让用户手填下一道工序」，与「不知道」同向；给一个可能错的下一道会
让写侧照单全收。

**⚠️ `location` 值恒 null 但键必须保留**：前端 `BatchPickerDialog.holderText` 用
「键存在性」（`'location' in p`）判断要不要渲染 holder 行，删键 / 加
`skip_serializing_if` 会让报工台卡片静默少掉「未知位置」这一行，且仓内没有测试能
提前发现（前端 fixture 自己显式带上了这个键）。

**被砍掉的 23 个字段**（全部恒为占位值且报工台零消费，按仓内既有先例
`has_cnc_program` 的处理留档）：`applicant_name`（写死空串）/ `request_date`
（写死 `1970-01-01`）/ `customer_id`（写死 `0`）/ `assembly_id`（恒 null）/
`status`（写死 `"IN_PROCESS"`）/ `order_no` / `note` / `unit_price` +
`total_price`（写死 `"0"`）/ `version`（**part 级** OCC，写死 `0`；取行 SQL 从不投影
`p.version`，批次 OCC 只认 `batch_version`）/ `created_at` + `created_by` +
`updated_at` + `updated_by`（写死 epoch）/ `deleted_at` / `customer_name` /
`l1_customer_name` / `holder_name` / `row_type` / `has_children` / `child_count` /
`has_cnc_program` / `delivered_quantity`（`From<TPart>` 是 part 级投影、不含批次
聚合量 ⇒ 恒 `None`；报工台的行单位是批次，「已送数量」对它是批次级量，取行 SQL
从 `t_part_batch` 起也不投影该聚合）。

> `request_date` 这一条对前端有连带义务：它原本带一条 `'1970-01-01' → null` 的字段级
> transform。键不再下发后 Zod 会因 `undefined` 抛错（整份信封 parse 失败），那条
> transform 必须**一并删掉**。`planned_delivery_date` 的同名 transform 保留无害
> （它投影的是真实值，transform 只对恰好等于哨兵的串生效）。

形状护栏：`vo/listing.rs::tests::scan_list_item_keys_are_pinned` +
`tests/production/scan_listing.rs::scan_list_item_key_set_is_pinned`（断言
`keys(sort())` 逐字等于那 17 个）。

### 2.3 `ScanListOut`（端点 2 / 3 的分页信封）

| 字段 | 类型 | 备注 |
|---|---|---|
| `items` | `ScanListItem[]` | 见 §2.2 |
| `total` | number | 裸 i64（**不是** string） |
| `limit` | number | 裸 i64，缺省 50 / clamp 1..200 |
| `offset` | number | 裸 i64 |

### 2.4 `WorkerScanRequest`（端点 4，6 字段）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | **是** | 工单序列号；定位 part 的主键锚 |
| `badge_code` | string | **是** | 工人工牌码；未注册 → `20201`、停用 → `20202`、未分配工种 → `20206` |
| `event_type` | string | **是** | `RETURNED`（放回）/ `INSPECTED`（送检），**大写**，反序列化枚举无小写别名 |
| `next_process_id` | string | **条件** | 仅**非顺应工序**时必填（缺失 → `40001`）；`chain_state='NEXT'` 与 `TAIL` 时可省，后端按链推导 / 自动送检 |
| `batch_id` | string | 否 | 多批次歧义消歧（`find_worker_held_batch_for_part` 的 `expected_batch_id`）；只有一批持有件时缺省 |
| `quantity` | string | 否 | **JSON 字符串**，本次实际操作量；缺省 = 整批。口径见 §2.4.1 |

`batch_id` / `quantity` 都是 **JSON 字符串**（雪花 id 精度 + 与本域既有约定一致）：
`"batch_id": "1234"`、`"quantity": "4"`。发 JSON number → **HTTP 422 纯文本**，
不进 `R<T>` 信封。

### 2.4.1 `quantity` 的三种落法（部分放回 / 部分送检）

| 请求 | 结果 | 批次变化 | 事件日志 | 响应 `scan.batch_id` |
|---|---|---|---|---|
| 缺省 / `null` | **整批** | 源批次直接流转，不拆 | `RETURNED_TO_SHELF` 或 `SENT_TO_INSPECTION`，`quantity` = `batch.quantity` | 源批次 |
| `>= batch.quantity` | **整批**（`==` 是「显式整批」的合法写法） | 同上，不拆 | 同上 | 源批次 |
| `0 < q < batch.quantity` | **部分** | 拆出 `quantity = q` 的新批次走本次流转；**源批次余量继承 `current_holder_id` 留在工人手上**（`location='WORKER'`、`current_holder_id` 不变，只是 `quantity` 被扣减、`version` +1） | `SPLIT`（`quantity = q`，`batch_id` = 新批次）+ 原事件（`quantity = q`，`batch_id` = 新批次） | **新批次** |
| `<= 0` / `> batch.quantity` / 超出 i32 | 拒收 | **零写入**（事务回滚，不拆批） | 无 | — |

- 错误码 `20111 BIZ_PART_BATCH_INVALID_QUANTITY`（HTTP **400**），与 pick-up 的
  部分领取、`POST /api/v2/batches/split` 的非法数量同码同状态。
- ⚠️ **`==` 合法、`>` 才非法**，与本域 `PickUpRequest::quantity` 同款；与
  `POST /api/v2/batches/split` 的「必须严格小于」**不同** —— 那条端点里「等于」
  是「白拆一次」，故判非法。
- ⚠️ **拆批的落点**：`RETURNED` 分支里是「`resolve_chain_position` 之后、TAIL 判定
  与 `mark_batch_returned` 之前」；`INSPECTED` 分支里是「分支入口」。两条都保证
  拆批早于该分支的**任何**批次写入。链位置可以在拆前判定 ——
  `resolve_chain_position` 只按 `current_process_id` / `current_process_step_id`
  定位，而拆批的 `INSERT ... SELECT` 原样继承这两列、`UPDATE` 只动 `quantity`。
- ⚠️ **TAIL（链尾自动送检）分支同样返回新批次**：它是 `RETURNED` 臂里的
  early-return（chain_state = `TAIL`），若拆批落在它之后，那条路径送检的就是整批、
  本次指定的数量被静默吞掉。
- ⚠️ **新批次能满足下游守卫**：`_split_batch_inner` 的 `INSERT ... SELECT` 继承
  `location`（`'WORKER'`）与 `status`（`'IN_PROCESS'`），`version` 恒 0，而
  `mark_batch_returned` 的 WHERE 是
  `id=$1 AND version=$2 AND status='IN_PROCESS' AND location='WORKER' AND
  deleted_at IS NULL`、`mark_batch_inspected` 走 `shared::batch::status` 的
  `allowed_from = {PENDING, PROGRAMMING, IN_PROCESS}` + `expected_version` ⇒
  逐条命中。
- 拆批复用 `prod::batch::repo::PartBatchRepo::split_batch_for_partial_pass`（薄包装
  `_split_batch_inner`），与 pick-up / to_ship / to_process / to_inspection 共用同一段
  SQL，**本域不新写拆批 SQL**。
- 余量继续满足 `GET /scan/held` 的行判据（`status='IN_PROCESS'` +
  `location='WORKER'` + `current_holder_id = worker_id`），仍出现在报工台「已持有」
  列表，可被下一次扫（放回 / 送检）处理。

### 2.5 `WorkerScanOut` / `WorkerScanCoreOut`（端点 4）

JSON 形状逐字不变（本轮只改 URL 与归属）：

| 字段 | 类型 | 备注 |
|---|---|---|
| `scan.worker_id` | string | |
| `scan.part_id` | string | |
| `scan.batch_id` | string | ⚠️ **本次实际被处理的那一批**：拆批场景（§2.4.1）= 拆出来的**新批次**，整批场景 = 工人手上那批 |
| `scan.event_type` | string | ⚠️ **可能与请求的不同**，见 §4.1 |
| `scan.synced_assembly_id` | string \| null | 父装配件真变了才有值（handler 据此发 `ASSEMBLY_UPDATED`） |
| `refill` | `RefillResult` | 同事务 refill，见 [`queue.md`](queue.md) §3 |

`work_type_id` / `badge_code` / `split` 是**内部管道字段**（`#[serde(skip)]`）：前两个
把 `worker_scan_event` 已 fetch 过的 worker 信息透传给同事务的
`QueueService::refill_for_worker_with_work_type`，避免重复查询；`split` 承载
「部分数量有没有拆批」，供 handler 在 commit 之后补发 `PART_BATCH_SPLIT`。三者都不
出现在 JSON 里。

### 2.6 `PickUpRequest`（端点 5）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | number | **是** | `t_part_batch.version` OCC 锚 |
| `worker_id` | string | **是** | 持有件工人（雪花 ID 字符串） |
| `shelf_id` | string | 否 | 传了才走 `validate_shelf_zone`（`20501` / `20512` / `20104`），缺省**完全不校验** |
| `quantity` | string | 否 | **JSON 字符串**；缺省 = 整批（口径与 §2.4.1 同款，**只有余量的去向不同**） |
| `note` | string | 否 | 事件日志备注 |

`shelf_id` 为什么可以缺省：pick-up 路径上它只进 `validate_shelf_zone`（零写），而
本路径 `t_part_batch` 的全部写入点的 SET / WHERE 均无货架列或货架条件，
`t_part_event` 无货架列，响应 `PartOut` 无 shelf 字段 ⇒ 那条校验是**防呆断言**而非
安全边界。⚠️ pick-up 路径**从不**校验货架↔工序映射（`20507`
`assert_shelf_maps_process` 在这条路径一次都没被调用）。

⚠️ pick-up 部分领取的**余量留在原处**（源批次 `location` / `current_holder_id` 一行
不改），与 worker-scan 的 §2.4.1 相反 —— 现场语义不同：领取时余量应回到架上等下一个
工人领，放回 / 送检时余量应留在**当前这个工人**手上等他下次处理。

## 3. 口径表

### 3.1 `pickable` 的行判据

批次 `status='IN_PROCESS'` + `location='PRODUCTION_SHELF'` + `deleted_at IS NULL`，
挂在 `zone='PRODUCTION'` 且 `is_active=true` 且**未软删**的货架上，且批次
`current_process_id` 命中 `t_work_type_process` 里该工种的**活跃**映射（带
`wtp.deleted_at IS NULL` 闸门）；另需 part 本身 `deleted_at IS NULL`。**货架范围再
按当前账号 scope 收窄**（见 §3.3）。

排序：`ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, pb.id ASC` ——
排的是 **DB 真实列**（这也是 2026-10-04 补投影 `is_urgent` / 两个交期的动因：列表已
按加急排好、工件上却看不出任何标记）。

`t_process_chain_step cs` 走 **LEFT JOIN** —— 指针为 NULL 的批次（无链工单的常态）
必须照样出现在可领列表里。

### 3.2 `held` 的行判据

批次 `status='IN_PROCESS'` + `location='WORKER'` + `deleted_at IS NULL` 且
`current_holder_id = worker_id`；另需 part 本身 `deleted_at IS NULL`。
排序 `pb.id DESC`。**不做货架 scope 收窄**（行已在工人手上，不是架上的候选池）。

### 3.3 货架 scope 收窄（`pickable` 专有）

`pickable_shelf_scope(current)` 只有两条分支：`shelf_wildcard || has_role(Manager)`
→ `None`（SQL 不加谓词）；其余 → `Some(shelf_ids)`，谓词 `sh.id = ANY($n)`。
空数组返回 `Some(vec![])` 而**不是** `None`（`ANY('{}')` 对任何货架都假）。

⚠️ **与 `shared::shelf::select::shelf_scope_for` 的分歧是刻意的，不要合并**：
写侧那条多一条「非货架账号不限」的分支，因为 `shelf_ids` 只对 `Role::ShelfAccount`
填 —— 一个从未被授予货架范围的 `Inspector`，`shelf_ids` 恒为 `[]`，照读侧收窄的话
它的选架 scope 恒空、送检一律 `40301`，而送检恰恰是品检员的主业。读侧不受影响：
把无货架范围的 Clerk / Inspector 收窄成空列表在「工人能取哪些件」的语境下无害。

⚠️ **Clerk / Inspector 的行为后果**：本端点的角色白名单含 Clerk / Inspector，而这两
类角色按惯例不配 `t_user_role` 的 SHELF_ACCOUNT 行 ⇒ `shelf_ids` 为空且
`shelf_wildcard = false` ⇒ 收口后**返回空列表**。写侧 `worker-scan` 的
`require_any_role(&[Manager, ShelfAccount])` 只放行 Manager / ShelfAccount，故对这
两类账号不存在「列表给出但提交被拒」的落差。若业务上要放开，唯一经产品 API 可达的
办法是给它们**逐架**配 `scope_id` 的 SHELF_ACCOUNT 行。

分歧的单测：`listing/service.rs::tests::diverges_from_write_side_shelf_scope_on_purpose`。

### 3.4 链位置派生（`held` 侧，唯一填充路径）

`chain_state` 四件套由 `shared::batch::chain::CHAIN_POSITION_LATERAL_SQL` 派生，
**两步定位**（纪律与完整论证见该常量的模块 doc，本域是它的**第一个消费方**）：

1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`，`cur` =
   `pb.current_process_step_id` 指向的 step（该 JOIN 无行 ⇒ 锚链解析失败 ⇒ 落
   `NONE`）；中间 JOIN `t_part_process_chain` 让「锚链已软删」同样落 `NONE`。
2. **当前 step 在锚链内的位置**：按 `pb.current_process_id` 在锚链内**重新定位**
   （`cur2` inner `JOIN LATERAL`），再取锚链内 `sort_order` 大于它且最小的那个未软删
   step。命中 >1 视作歧义落 `NONE`。

⚠️ **第 2 步绝对不能拿 `pb.current_process_step_id` 的 `sort_order` 直接当位置** ——
step 指针与「当前工序在链内的位置」是两个独立事实，而 worker-scan 的 RETURNED 分支
在**非顺应工序**时只写 `current_process_id`、step 指针留在原处。指针漂移的批次在放回
时按 `sort_order` 推进会把**当前工序自己**当成下一道返回，而 `chain_state` 仍在说
「可免填」⇒ 写侧照单全收，静默错值比拒收更难发现。

⚠️ **链内同一 `process_id` 允许重复，读侧必须自己识别歧义**：`hit_count > 1` 时显式
落 `NONE`（与「未知一律往保守方向降」一致），并同时门控三个派生侧 ⇒ 维持
`NONE` / `"0"` / `null` / `null` 的不变量。

⚠️ **「下一道」按 `sort_order > 当前 ORDER BY ASC LIMIT 1` 取，不按 `= 当前 + 1`**：
与写侧正典 `next_step_in_chain` 逐条同形。`sort_order` 的**密度不由读侧决定** ——
写侧只保证链内互不重复，稠密 0-based 与稀疏 `10/20/30` 都能落库。

### 3.5 `has_process_chain` 判据

`HAS_PROCESS_CHAIN_EXPR` = 「工单已绑链 **且** 批次当前工序能在链内定位」。
与 `prod::queue` 的 `QueuePoolItem.has_process_chain` / `QueueHeldBatch`、
`outsource` 候选卡的同名列**同源**（同一个常量），四处必须同改。

## 4. 与 WS 的关系

| 端点 | 事件 | 触发条件 |
|---|---|---|
| `POST /scan/worker-scan` | `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED` | 无条件，**按响应的 `scan_out.event_type`** |
| `POST /scan/worker-scan` | `PART_BATCH_SPLIT` | 部分数量（§2.4.1，自动拆批） |
| `POST /scan/worker-scan` | `WORKER_POOL_REFILL_DONE` | 同事务 refill 抢到一批 |
| `POST /scan/worker-scan` | `WORKER_POOL_EMPTY` | refill 池空 |
| `POST /scan/worker-scan` | `ASSEMBLY_UPDATED` | 父装配件真变了 |
| `POST /scan/batches/{id}/pick-up` | `PART_PICKED_UP` | 无条件 |
| `POST /scan/batches/{id}/pick-up` | `PART_BATCH_SPLIT` | 部分领取（自动拆批） |

- **事件名一字不改** —— dashboard 与队列页都在监听，改名会静默断链。
- `PART_BATCH_SPLIT` **两条写路径共用**（worker-scan 部分放回 / 部分送检与 pick-up
  部分领取），payload 字段名**逐字同形**：
  `{ part_id, new_batch_id, source_batch_id, quantity }`（雪花 id 为 JSON string，
  `quantity` 为裸 number）。
- ⚠️ **worker-scan 这条必须发**：拆批把源批次 `quantity` 静默扣减、并新建了一个批次
  行，而余量**仍在工人手上**（不像 pick-up 那样留在架上），其它端的批次视图收不到
  这条事件就永远看不到「持有件变多了一件 / 源数量变了」。
- 消费方是 dashboard 域（`/ws/dashboard` + 前端 `AFFECTS_DASHBOARD` 白名单），
  不是报工台自己的实时刷新。
- 广播在 **commit 之后**（对齐 Python 延迟广播模式）。

### 4.1 ⚠️ `worker-scan` 的响应 `event_type` 可能与请求的不同

请求发 `RETURNED` 但批次在工序链上是最后一道时，服务端把它当**送检**处理（链尾自动
送检），响应 `event_type = "WORKER_SCAN_INSPECTED"`。

消费方**必须按响应里的 `event_type` 分支**，不能按自己发的那一个 —— 服务端比前端
更清楚批次做完了没有。handler 按 `scan_out.event_type` 广播，所以 WS 链路自动成立、
无需为它单独改 handler。

### 4.2 `WORKER_POOL_EMPTY` 的 payload

`shelf_id` 键恒为 `null`（refill 已无架锚，worker-scan 也不再收它）；WS payload 与
HTTP 响应的 `refill.shelf_id` 同步。

## 5. SQL 条数

| 端点 | 条数 | 说明 |
|---|---|---|
| `GET /scan/pickable` | **2** | 取行 + COUNT |
| `GET /scan/held` | **2** | 取行 + COUNT |
| `POST /scan/verify-badge` | 1 | 按 `badge_code` 反查 |
| `POST /scan/worker-scan` | 多次 | 状态翻转 + 事件日志 + 选架 + 同事务 refill + 派生；**部分数量另加** 链位置解析 + 拆批 3 条（`max(batch_no)` / `INSERT ... SELECT` / 扣量 `UPDATE`）+ 新批次回读 1 条 + `SPLIT` 事件 1 条 |
| `POST /scan/batches/{id}/pick-up` | 多次 | 拆批（可选）+ 状态翻转 + 事件日志 |

两条 list 端点都**不按行数重复查**：`pickable` 一次 SQL 把「工种→工序映射 → 架 →
批次」全链 JOIN 完；`held` 一次 SQL 把链位置派生挂在 `LEFT JOIN LATERAL` 上。

⚠️ COUNT 的 `$n` 编号**按自身 bind 顺序连续**（`pickable` 的 COUNT 用 `$1`/`$2`，
取行用 `$1`..`$4`）。PG 扩展协议要求 Parse 消息声明的参数类型个数**等于** SQL 里被
引用的参数个数，沿用取行的编号就得 bind 两个没人引用的占位符，Parse 期直接被拒。

## 6. 移除记录（2026-10-10）

### 6.1 端点

| 旧路径 | 新路径 | 迁往 |
|---|---|---|
| `POST /api/v2/prod/workers/verify-badge` | `POST /api/v2/prod/scan/verify-badge` | `prod::worker` |
| `GET /api/v2/parts/pickable-by-work-type/{work_type_id}` | `GET /api/v2/prod/scan/pickable?work_type_id=` | `part` |
| `GET /api/v2/parts/by-worker/{worker_id}` | `GET /api/v2/prod/scan/held?worker_id=` | `part` |
| `POST /api/v2/prod/batches/worker-scan` | `POST /api/v2/prod/scan/worker-scan` | `prod::batch` |
| `POST /api/v2/prod/batches/{batch_id}/pick-up` | `POST /api/v2/prod/scan/batches/{batch_id}/pick-up` | `prod::batch` |

实际失效形态见 §1.2。**无 alias。**

### 6.2 字段

- `ScanWorkerBrief` 相对 `WorkerOut` 删 7 个字段（§2.1）。
- `ScanListItem` 相对 `PartListItem` 删 23 个字段（§2.2）。
- `PickableQuery` 删 `shelf_id`；`ByWorkTypeQuery`（留在 part 域）同步删。

### 6.3 文件 / 类型归属

| 类型 | 原址 | 新址 |
|---|---|---|
| `ChainState`（报工台填充的那份用法） | `part::vo::ChainState` | `prod::scan::vo::ScanChainState` |
| `WorkerScanCoreOut` / `WorkerScanOut` | `prod::batch::vo` | `prod::scan::vo::transition` |
| `WorkerScanEvent` | `prod::queue::dto` | `prod::scan::dto` |
| `WorkerScanRequest` / `PickUpRequest` | `prod::batch::dto` | `prod::scan::dto` |
| `VerifyBadgeRequest` | `prod::worker::dto` | `prod::scan::dto` |
| `ByWorkTypeQuery` / `ByWorkerQuery` | `part::dto_crud` | `prod::scan::dto::PickableQuery` / `HeldQuery` |
| `PickUpOutcome` / `PickUpSplitInfo` | `prod::batch::service::pickup` | `prod::scan::service::pickup` |

`part::vo::ChainState` **保留**（`PartListItem` 仍在用），并从本域 re-export 去掉 ——
让 `part` 反向依赖 `prod::scan` 换来的只是省几行字，代价是一条本不该存在的跨域依赖。

## 7. 表依赖

| 用途 | 表 |
|---|---|
| 批次行 / 批次锚 / OCC | `t_part_batch` |
| 工单展示字段 | `t_part` |
| 工种↔工序映射（`pickable` 过滤） | `t_work_type_process` |
| 货架（zone / active / 软删 / scope） | `t_shelf` |
| 工序名（链派生后取名） | `t_process` |
| 工艺链与 step 定位 | `t_process_chain` / `t_process_chain_step` / `t_part.process_chain_id` |
| 工人（`verify_badge` / worker-scan） | `t_worker` |
| 事件日志 | `t_part_event` |
| 货架↔工序映射（选架候选集） | `t_shelf_process` |

### 7.1 跨域依赖登记

**整域不适用** `shared::domain_guard`：`worker_scan` 是转发型用例，必然 import
`part`（repo / 事件日志 / 状态机）、`assembly`（父件级联）、`prod::queue`（同事务
refill）、`prod::worker`（按工牌反查）四处域。这与 `prod::queue` 的写端点「经本域
trait 转发他域单表查询」同款，是逐域剥离期间的既定 pattern。

**`listing/` 子模块零跨域**：2 条只读聚合端点只需要上表里那几张表，全部在本目录的
SQL 里聚合，一处他域的 service / repo 都不 import（连 `PartRepoTrait` 都不借 ——
那是一个 part 域的胖 trait，借它就等于把 part 域拖进来，本模块直接收
`&mut PgConnection`）。这条由
`modules::prod::scan::listing::tests::listing_aggregation_depends_on_no_other_domain`
守住（扫 `src/modules/prod/scan/listing/**/*.rs`）。

**两个跨域设施层**（`shared` 不是域，护栏放行）：

| 设施 | 用途 |
|---|---|
| `shared::batch::chain` | `HAS_PROCESS_CHAIN_EXPR`（卡片绿边框判据）+ `CHAIN_POSITION_LATERAL_SQL`（链位置两步定位）+ `resolve_chain_position`（写侧，与读侧同源） |
| `shared::shelf::select` | `pick_least_loaded`（放回 / 送检的自动选架）+ `shelf_scope_for`（写侧 scope）+ `no_candidate_in_scope`（40301 统一文案） |

### 7.2 前端配套改动清单

1. **URL 全量替换**（**无 alias**，部署顺序**必须后端先上**）：
   - `/api/v2/prod/workers/verify-badge` → `/api/v2/prod/scan/verify-badge`
   - `/api/v2/parts/pickable-by-work-type/{work_type_id}` →
     `/api/v2/prod/scan/pickable?work_type_id={work_type_id}`
   - `/api/v2/parts/by-worker/{worker_id}` →
     `/api/v2/prod/scan/held?worker_id={worker_id}`
   - `/api/v2/prod/batches/worker-scan` → `/api/v2/prod/scan/worker-scan`
   - `/api/v2/prod/batches/{batch_id}/pick-up` →
     `/api/v2/prod/scan/batches/{batch_id}/pick-up`
2. **api 层函数改名 + 入参形态**：两条 list 函数的过滤键从路径段改 query 参数；删除
   所有 `shelf_id` 入参。
3. **`scanPartRowSchema` 删 23 个键**（§2.2），其中 `request_date` 那条
   `'1970-01-01' → null` 的字段级 transform 必须**一并删**（键不再下发，Zod 会因
   `undefined` 抛错炸掉整份信封）。⚠️ **不要**动 `location` 的声明（键必须在）。
4. **`scanBadgeSchema`（verify-badge 出参）收敛为 4 键**：`id` / `badge_code` /
   `name` / `work_type_id`（§2.1）。前端 `Worker` 类型若同时服务 worker 管理页，
   需拆成两个类型（管理页继续用全字段 `WorkerOut`）。
5. **目录归位**：报工台的 api 函数落到**两级切分的两个文件** ——
   `src/api/productionScan.ts`（请求函数）+ `src/api/productionScan.contract.ts`
   （守门 schema 与派生类型），不要再挂在 `api/parts/crud.ts` / `api/worker.ts`
   下 —— 后者按前端实体扁平放置，不按后端模块分层（见 `api/shelves.ts` 文件头）。
6. **路径形硬切要同步前端契约测试**：旧 URL 逐字写在
   `src/api/parts/__tests__/routes.spec.ts` 与
   `src/api/__tests__/productionScan.contract.spec.ts`（随第 5 条一起搬家，
   原 `src/api/parts/__tests__/scan-list.contract.spec.ts` 是它的旧名）。
7. **`worker-scan` 的成功文案必须按响应的 `event_type` 分支**（§4.1）—— 2026-10-10
   起这条从「链尾边缘场景」变成常规路径。
8. **i64 字符串化**：所有雪花 id 仍是 JSON string，本轮不改变该约定。
9. **部分数量（2026-10-11 新增）**：
   - 请求侧放回页 / 送检页各加一个数量输入，**发 JSON 字符串**（`"quantity": "4"`），
     不发数字（否则 422 纯文本）；缺省不传该键即整批，**老客户端零改动照常工作**。
   - 响应侧 `scan.batch_id` 现在是「本次实际被处理的那一批」。**不要**再拿请求里
     扫到的 `batch_id` 去回显 / 去推后继动作 —— 拆批场景下它指向余量（仍在工人手上）。
   - 成功后的列表刷新：余量批次仍在 `GET /scan/held` 里，拆出来的那批已不在 ⇒
     「已持有」列表**行数不变**、数量变小；放回 / 送检两页都必须重拉列表，不能只
     假定「扫掉一行」。
   - `PART_BATCH_SPLIT` WS 事件与 pick-up 共用，payload 字段名同形
     （§4）—— 若前端已有该事件的处理函数，worker-scan 这条**零改动**即生效。
   - 非法数量回 `20111`（HTTP 400）+ 信封 `{ code, message, data: null }`，按既有
     错误提示路径展示即可。

## 8. 已知偏差登记

---

**`part::vo::PartListItem` 的 7 个字段退场（保留但恒占位）**

2026-10-10 起，`PartListItem` 里的 `batch_id` / `batch_version` / `chain_state` /
`chain_next_process_id` / `chain_next_process_name` / `chain_current_process_name` /
`has_process_chain` 在**本 VO 内已无填充点**（唯一填它们的端点已迁往本域）。

**字段保留**，理由：`PartListItem` 在 wire 上还有 **3 个域的 4 个端点**在用 ——
`part`（`GET /parts` + `GET /parts/by-work-type/{id}` 两条 list）、`assembly`
（`POST /assemblies/{id}/children` 的单条响应）、`com::union_list`（`GET /com/union-list`），
它们的响应形状不能变，删字段是**破坏性 wire 变更**。⚠️ `outsource` 与 `wx` **不在此列**：
两者只在注释里拿 `PartListItem` 做字段对照，各有自己的 VO（`QuotablePartListOut` /
`OutsourceSentPartListOut` 等、`wx::vo` 的窄投影），从不返回它 —— 算域数时别把
「注释提到」当成「wire 上有」。按仓内既有先例（`has_cnc_program` 的处理）补了字段
doc 写明「恒 null / NONE / "0" / false」。它们在那些 part 级行上的正确值本来就是
「不知道」—— 一个 part 的活跃批次可能不止一个，任一批次的链位置都是错锚点。

---

**`WorkerScanEvent` 的归属搬迁**

它原先住在 `prod::queue::dto` —— 它是 worker-scan 的**入参枚举**，queue 只是当初收留
了它（2026-09-30 拆 `MoveLocation` 时与下发流入参混在同一文件）。本轮按**消费方**
归位到 `prod::scan::dto`，queue 侧删除。域归属按消费方判定，与「谁先写出来」无关。

---

**`list_by_work_type`（`GET /parts/by-work-type/{id}`）留在 part 域**

它是 part 域 list 族里唯一的「按工种列工人持有件」读端点，**零前端消费方**但有集成
测试覆盖其 part 侧投影，故未随本轮迁走。它的取行投影已从共用 struct 拆成独立的
`WorkTypeByWorkTypeRow`（只含它自己 SELECT 的 8 列），不再与报工台的行投影共享代码。
`?shelf_id=` 入参同步删除（该端点列的是**工人手上**的件而非架上的候选池，shelf
过滤对它没有语义）。

---

**list 端点 SQL 与队列域候选池口径的关系**

`/scan/pickable` 与 `prod::queue` 的候选池 SQL **是两套独立实现**，不是同一口径的
两个入口：

| | `/scan/pickable` | `prod::queue` 候选池 |
|---|---|---|
| 过滤 | 按**工种**（`t_work_type_process` 映射） | 按**工序** |
| 排序 | `is_urgent DESC, planned_delivery_date ASC, id ASC` | 队列序列 + 已编程优先 |
| 链列 | `has_process_chain` + `chain_*` 四件套（`pickable` 侧为占位） | `has_process_chain` |
| 页面 | 报工台取件页 | 生产队列看板 |

两者共享的是 `HAS_PROCESS_CHAIN_EXPR` 一个常量（判据必须同源），**SQL 其余部分刻意
不同** —— 报工台的「按工种取件」与看板的「按工序排队」是两个不同的现场动作，强行合并
会让其中一边的排序/过滤语义变形。

---

**跨域护栏只覆盖 `listing/`**

`prod::scan` 整域**不适用** `assert_no_foreign_domain`（转发型域，见 §7.1），只有
`listing/` 那两块纯只读聚合单独装护栏。护栏为什么按**目录路径前缀**扫、被扫的代码
必须自成一个目录：这条规则要可被 CI 执行，被扫代码就得是一个能被 `read_dir` 圈出来
的目录。圈出可守的部分比整域不守要强。
