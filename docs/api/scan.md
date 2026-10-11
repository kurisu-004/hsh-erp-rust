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

### 0.0 2026-10-15：扫码接管 `POST /scan/take-over` + pick-up PENDING 链首落位（S2）

「扫码找件 · 自助恢复」的第二件（S1 = `GET /scan/locate` 只读诊断）。本轮两半：

**S2-A · 新端点 `POST /scan/take-over`（第 7 条，纯新增）**：批次挂在**别的**工人
名下（`IN_PROCESS + location='WORKER'` + holder≠我），但图纸 / 实物已流转到本人手里
⇒ 扫工牌一键接管：holder 换成我、顺手推进到「链上下一道（可推导时）」或前端**显式
指定**的工序，同一事务原子完成。**不改 `status`、不碰 `is_repairing`、不做部分数量**。

- 角色 Manager + ShelfAccount（与 `worker-scan` 同款）；handler 开事务、commit 后
  广播 **`PART_TRANSFERRED`**（新事件，payload 四键**全部 JSON 字符串**）。
- 判据顺序（9 步）与错误码全表见 §2.8；与 S1 `locate` 的 `TAKE_OVER` 判定**同源**。
- 既有端点请求 / 响应零改动。

**S2-B · `POST /scan/batches/{id}/pick-up` 的 PENDING 起点落链首**：PENDING 批次
此前透传 NULL 工序两列，领到手的新 `IN_PROCESS` 批次工序信息缺失、落不进任何工序
候选池。现在**有链 ⇒ 落链首**；链不可解析 ⇒ `20702`（HTTP 404）、链首 ∉ 工种映射
⇒ `20104`（HTTP 400）—— 两道新判据都排在**容量守卫之后、拆批之前**。

- **行为变更点（前端须知）**：PENDING 领取现在可能在提交后吃到 `20702` / `20104`；
  `PICKED_UP` / `SPLIT` 事件的 `process_id` 从恒 `null` 变为链首工序（无链仍 `null`）。
- 与 `locate` 的 PENDING 闸门逐谓词对齐；`locate` 侧同步订正了「链行已软删但 step
  仍活跃」的旧分叉（见 §8）。

**`locate` 侧的两处配套订正**（以写侧为准，见 §8）：① PENDING 链首可用性改带
**链行软删闸门**（与 `first_step_in_chain` 对齐）；② `location='WORKER'` 但
**holder 为 NULL** 的脏形态不再给 `TAKE_OVER`（写侧同形态拒 `20103`；落 `UNKNOWN`）。

### 0.1 2026-10-15：新增第 6 条端点 `GET /scan/locate`（扫码找件 · 诊断）

工人扫图纸序列号 → 返回该 part **全部未软删批次**（**含终态**）的位置事实；传了
`worker_id` 时再对每批给出「**这个工人**、**这一批**现在能做什么」的 `action` /
不能做的 `disabled_reason`。这是「扫码找件 · 自助恢复」三件套的**第一件（S1，纯只读
诊断）**；**S2（接手 take-over）已于同日落地**（见 §0.0），**S3（外协收回）尚未落地**
—— 本端点给它们预留的 `TAKE_OVER` / `RECEIVE_FROM_OUTSOURCE` 两个 action 中，
前者已接上真实写端点。

- **纯新增**：不改任何既有端点的请求 / 响应 / 错误码，**无历史路径可硬切**。
- **只读**：`pool.acquire()`、不开事务、不发 WS（§4 不登记它）。
- **角色**：Manager + Clerk + Inspector + ShelfAccount（与 `/pickable` / `/held` 同一
  白名单 `SCAN_LIST_ROLES`）。
- 端点专节：§1 第 6 行 / §2.7 逐字段 / §3.6 action 决策表与写侧守卫对照 / §5 SQL
  条数 / §7 表依赖 / §8 三条偏差登记。

### 0.2 2026-10-14：`/held` 容量可见 + pick-up 容量守卫

**三处前端契约变更**：

1. **`GET /scan/held` 出参由 `ScanListOut` 改为 `HeldListOut`**（§2.3.1）—— 新增
   `current_held` / `max_held_batches` 两个**裸 number**。前 4 个分页字段逐字不变。
   `/pickable` **不**携带这两个字段。
2. **`POST /scan/batches/{id}/pick-up` 新增两条错误码**（§2.6.1）：`20204`
   （容量触顶，**HTTP 409**）/ `20904`（工种未配上限，**HTTP 400**）。
3. **`POST /scan/worker-scan` 响应的 `refill` 段多出第三个可选键
   `refill.stopped_reason`**（§2.5）—— `refill` 是内嵌的 `RefillResult`
   （`prod::scan::vo::transition::WorkerScanOut::refill`），与
   `POST /queue/refill` 的响应体**同一个 VO**：refill 一批也没抢到时该键出现，取
   **`"POOL_EMPTY"`（真·池空）/ `"WORKER_FULL"`（工人已拿满硬上限）/
   `"AUTO_LIMIT_REACHED"`（2026-10-15 新增：工种自动上限触顶，工人仍可手动多领）**；
   抢到东西时**整个键不出现**（`skip_serializing_if`）。逐字段、判据与那条「诊断只在没抢到时执行」
   的性能口径见 [`queue.md`](queue.md) §3.2 / §4.8。⇒ **向后兼容**：不读该键的客户端
   零改动照常工作。

   ⚠️ 同一改动还**收窄了本端点的 WS 行为**：refill 因「工人已拿满」停下时**不再**广播
   `WORKER_POOL_EMPTY`（§4.2）—— 前端若靠该事件提示「没抢到料」，现在会收不到，
   改读本响应里的 `refill.stopped_reason`。

**为什么**：报工台取件页此前拿不到「还能不能再取件」，只能在提交后吃一个 `20204`；
而 pick-up 这条路径**完全没有**容量守卫，工人在队列板上被 `held < max` 卡着、从
报工台却能无限往手上加 —— 两条路径的容量契约形同虚设。

**同一轮还有两处非 wire 变更**（属 `prod::queue` / `shared::batch` 域，
逐条见 [`queue.md`](queue.md) §8.4 与 [`batch.md`](batch.md)）：读侧判据对齐 +
终态批次清 `location` / `current_holder_id`。

### 0.3 2026-10-11：worker-scan 支持部分数量

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

### 0.4 2026-10-10：报工台立域

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
| 6 | GET | `/api/v2/prod/scan/locate?serial_no=&worker_id=` | —（**2026-10-15 纯新增，无旧路径**） | Manager + Clerk + Inspector + ShelfAccount | `serial_no` **必填**（字符串）、`worker_id?`（字符串；缺省 ⇒ 不算 action） | `LocateOut`（`part` + `batches[]`） |
| 7 | POST | `/api/v2/prod/scan/take-over` | —（**2026-10-15 纯新增，无旧路径**） | Manager + ShelfAccount | `TakeOverRequest`（5 字段，`batch_id` / `next_process_id?` 为 **JSON 字符串**；`version` 必填 number） | `TakeOverOut`（7 键） |

- 全部返回统一信封 `R { code, message, data }`。
- 端点 2 / 3 / 6 是**纯读**（`pool.acquire()` 不开事务、不发 WS 广播）；端点 1 / 4 / 5 / 7
  开事务，**广播在 commit 之后**。
- 端点 2 / 3 的 `limit` **缺省 50**：想要全部必须显式传 `limit=200`（clamp 上限），
  否则静默截断。`offset` 缺省 0。端点 6 **不分页**（返回该工单全部批次）。
- `work_type_id` / `worker_id` 走 `deserialize_i64`（**只接受 JSON 字符串**），
  发 JSON number → axum `QueryRejection` → **HTTP 400 纯文本，不进 `R<T>` 信封**。
  漏传 → 同样 400 纯文本（无 `#[serde(default)]`）。
  ⚠️ 端点 6 的 `worker_id` 是**可选**（`deserialize_i64_opt` + `#[serde(default)]`）：
  缺省合法（纯位置事实），非数字 / 漏 `serial_no` 才 400。
- body 里的两个**数量**入参（`WorkerScanRequest.quantity` / `PickUpRequest.quantity`）
  与 `PickUpRequest.shelf_id` 走 `deserialize_i64_opt`，**只接受 JSON 字符串**；发数字 →
  axum `JsonRejection` → **HTTP 422 纯文本**。
- ⚠️ `WorkerScanRequest.batch_id` / `TakeOverRequest.next_process_id` **不走**
  `deserialize_i64_opt`：它们是裸 `Option<String>`，只在 service 里 `str::parse`
  （前者走 `find_worker_held_batch_for_part` 调用点、后者解析失败 → `40001`）。
  「发数字也得到 422」是 serde 对 `Option<String>` 的类型不匹配，**与前一条不是同一个
  机制**，别把它当同款先例照抄。`TakeOverRequest.batch_id` 则走 `deserialize_i64`
  （数字 → 422 纯文本）。
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

### 2.3 `ScanListOut`（端点 2 的分页信封）

| 字段 | 类型 | 备注 |
|---|---|---|
| `items` | `ScanListItem[]` | 见 §2.2 |
| `total` | number | 裸 i64（**不是** string） |
| `limit` | number | 裸 i64，缺省 50 / clamp 1..200 |
| `offset` | number | 裸 i64 |

### 2.3.1 `HeldListOut`（端点 3 的分页信封，**2026-10-14 起**）

⚠️ **前端契约变更**：端点 3（`GET /scan/held`）的出参**不再**是 `ScanListOut`，
改为自己的一份。前 4 个字段与 `ScanListOut` **逐字相同、顺序不变** ⇒ 读
`items` / `total` / `limit` / `offset` 的代码不受影响，**新增** 2 个字段。

| 字段 | 类型 | 备注 |
|---|---|---|
| `items` | `ScanListItem[]` | 见 §2.2 |
| `total` | number | 裸 i64（**不是** string） |
| `limit` | number | 裸 i64，缺省 50 / clamp 1..200 |
| `offset` | number | 裸 i64 |
| `current_held` | number | **新增**。裸 i64，与 `total` **恒等**（见下） |
| `max_held_batches` | number | **新增**。裸 i32；`t_work_type.max_held_batches` 为 NULL 时发 **0** |

**为什么新增**（2026-10-14 的 bug 是同一个根因的另一面）：报工台取件页此前**拿不到**
「这个工人还能不能再取件」，只能在提交后吃一个 `20204`。`pick-up` 的容量守卫补上后
（见 §2.6），前端有了「已拿 2/5」就能在触顶前置灰按钮的能力。

**口径三条**：

1. **`current_held` 与 `total` 恒等** —— 两者是**同一次查询**的同一个值（都是「该
   工人当前持有的 `IN_PROCESS + WORKER + 未软删` 批次数」，见 §3.2）。冗余暴露
   `current_held` 是为了让前端不必推断「`total` 在本端点恰好等于容量计数」（`total`
   这个名字本身不承诺这件事）。**不要**为它另发一条查询。
2. **`max_held_batches` 为 NULL 时发 `0` 而不是 `null`** —— 沿用队列板
   `prod::queue::board::service.rs` 的 `w.max_held.unwrap_or(0)` 既有约定。语义正确：
   NULL 的含义是「**未配置**」而非「不限」（见 `docs/api/queue.md` §4.7），工种未配
   max ⇒ 各路径在**服务入口即早退**（`pick-up` / `move` / `refill` 返 `20904`，
   auto-allocate 逐 worker 记 `MAX_BATCHES_NOT_SET` 跳过），`take_one_from_pool`
   自 **2026-10-15** 起不再读工种上限（`max_batches` CTE 已删）；发 `null` 反而有让
   前端渲染成「不限」的风险。
3. 两个计数都是**裸 number**（本仓约定：只有雪花 id 才转 string）。

⚠️ **`/pickable`（端点 2）不携带这两个字段** —— 它按 `work_type_id` 作用域，
**根本不知道是哪个工人**，容量字段对它结构性无意义。回归测试
`tests/production/scan_listing.rs::pickable_does_not_carry_capacity_fields` 钉住
「别顺手也加到 pickable 上」。

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
| `== batch.quantity` | **整批**（`==` 是「显式整批」的合法写法） | 同上，不拆 | 同上 | 源批次 |
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

JSON **形状**逐字不变（键集一字未动）；`scan.batch_id` 的**取值**语义见下表
`scan.batch_id` 行与 §2.4.1：

| 字段 | 类型 | 备注 |
|---|---|---|
| `scan.worker_id` | string | |
| `scan.part_id` | string | |
| `scan.batch_id` | string | ⚠️ **本次实际被处理的那一批**：拆批场景（§2.4.1）= 拆出来的**新批次**，整批场景 = 工人手上那批 |
| `scan.event_type` | string | ⚠️ **可能与请求的不同**，见 §4.1 |
| `scan.synced_assembly_id` | string \| null | 父装配件真变了才有值（handler 据此发 `ASSEMBLY_UPDATED`） |
| `refill` | `RefillResult` | 同事务 refill（`QueueService::refill_for_worker_with_work_type`）。**2026-10-14 起 5 字段** = `worker_id` / `shelf_id`（worker-scan 路径恒 `null`，refill 无架锚）/ `taken[]` / `pool_empty` / **`stopped_reason`**（可选键：refill 一批也没抢到时出现，取 `"POOL_EMPTY"` / `"WORKER_FULL"` / **`"AUTO_LIMIT_REACHED"`**（2026-10-15 新增），抢到东西时**整个键不出现**）。⭐ **2026-10-15：refill 的目标口径改为「补到工种自动上限 `auto_allocate_limit`（未配置兜底 `max_held_batches`）」**（逐条见 [`queue.md`](queue.md) §4.8）。逐字段与判据见 [`queue.md`](queue.md) §3.2；真源 `prod/queue/vo/worker.rs::RefillResult` |

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

#### 2.6.1 pick-up 的容量守卫与两条新错误码（**2026-10-14 新增**）

⚠️ **本端点此前完全没有 `max_held_batches` 守卫** —— 工人在队列板上被 `held < max`
卡着，从报工台取件页却可以无限往手上加，`/queue/move` 与 `take_one_from_pool` 两条
路径的容量契约在这里形同虚设。2026-10-14 起守卫补上，与 `move_batch` 共用**同一个**
守卫函数 `QueueService::ensure_worker_has_capacity` 与**同一份判据**。

| 错误码 | 常量 | HTTP | 触发 |
|---|---|---|---|
| `20204` | `BIZ_WORKER_HOLD_LIMIT_EXCEEDED` | **409** | 工人已持有 `max_held_batches` 批（判据见 `docs/api/queue.md` §4.3） |
| `20904` | `BIZ_WORK_TYPE_MAX_HELD_NOT_SET` | **400** | 工种 `max_held_batches` 为 NULL（= **未配置**，不是「不限」；见 `docs/api/queue.md` §4.7） |

⚠️ **`20904` 的 HTTP 状态是 400 而不是 409** —— 它与 `20204` 是两个不同性质的错：
前者是「配置缺失」（与 `/refill` 遇 NULL 返同一个码、同一个 400），后者是「容量触顶」
（与 `/move` 满员返同一个码、同一个 409）。前端别把两者混成一个分支。

⚠️ **守卫位置是硬约束**：必须排在 `split_batch_for_partial_pass` **之前**。否则
「部分领取」会先把批次拆出一个新批次、随即撞容量守卫失败，留下一个**没人认领的
孤儿新批次**且源批次数量已被扣减。回归测试
`tests/production/pickup.rs::pick_up_when_worker_full_returns_20204_without_splitting`
钉住「满员时 20204 **且** `t_part_batch` 没有多出新行」。

⚠️ **守卫排在 `validate_shelf_zone` 之后**（既有错误码顺序不因本次新增而改变）：
「非法 shelf + 工种未配上限」在改动前返回货架错误（`20501` / `20512` / `20104`），
插队会让它变成 `20904`。

#### 2.6.2 PENDING 起点的链首落位与两条新错误码（**2026-10-15 S2-B 新增**）

**行为变更**：PENDING 起点不再透传 NULL 工序两列。解析顺序（`resolve_pending_chain_head`）：

| 形态 | 结果 |
|---|---|
| **无链**（`t_part.process_chain_id IS NULL`） | 两列维持 `NULL`（已知口径，与改动前逐字一致） |
| **有链，链可解析**（链行未软删 且 链内有活跃 step） | 新批次落**链首** `(step_id, process_id)`（`first_step_in_chain`，`ORDER BY sort_order ASC, id ASC LIMIT 1`） |
| **有链但链行已软删 / 链内无活跃 step** | `20702 BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`（**HTTP 404**，零写入） |
| **链首 ∉ 该工种的活跃工序映射** | `20104 BIZ_INVALID_VALUE`（**HTTP 400**，零写入） |

- **位置硬约束**：解析与校验排在**容量守卫之后、`split_batch_for_partial_pass`
  之前**。前者保证既有错误码优先级不变（「PENDING + 坏链 + 工种未配上限」改动前返回
  `20904`，插队会变成 `20702`；回归
  `tests/production/pickup.rs::pick_up_pending_chain_check_keeps_capacity_error_priority`）；
  后者保证校验失败**不留下拆批孤儿**（部分领取场景）。
- `PICKED_UP` / `SPLIT` 事件的 `process_id` 同步改用解析值（PENDING + 有链 = 链首；
  PENDING 无链仍 `null`；IN_PROCESS 起点仍取源批次现值）。
- **IN_PROCESS 分支一行不动**（池内领取不推进工序，仍只翻 location + holder）。
- 与 `locate` 的 PENDING 闸门**逐谓词对齐**（`has_usable_chain_head` /
  `chain_head_in_work_type` 两个投影），见 §3.6 与 §8。

### 2.7 `LocateOut` / `LocatePartBrief` / `LocateBatchItem`（端点 6，2026-10-15 新增）

#### 入参（`LocateQuery`）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | **是** | 图纸序列号。`trim()` 后为空按「未命中」处理（`20101`）；漏传 → `QueryRejection` **400 纯文本** |
| `worker_id` | string | 否 | 工人雪花 id。**只接受字符串形态**（与 held 同款）；非数字 → 400 纯文本。不传 ⇒ `action` / `disabled_reason` 恒 `null`（纯位置事实） |

#### 错误

| 错误码 | 常量 | HTTP | 触发 |
|---|---|---|---|
| `20101` | `BIZ_PART_NOT_FOUND` | **404** | `serial_no` trim 后为空 / `t_part` 无命中。**不回退 `t_assembly`**，文案提示「如为装配件，请扫子件序列号」（见 §8） |
| `20201` | `BIZ_WORKER_NOT_FOUND` | **404** | `worker_id` 传了但查不到（不存在或已软删） |
| `20202` | `BIZ_WORKER_INACTIVE` | **400** | 工人存在但 `is_active=false`（与写侧 pick-up 的 worker 校验同码，避免诊断出「点了就会失败」的按钮） |

#### `LocateOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | `LocatePartBrief` | 命中的工单 |
| `batches` | `LocateBatchItem[]` | 该 part **全部未软删批次**（**含终态**），`ORDER BY batch_no ASC, pb.id ASC`。**不分页、无上限**（同一工单批次数有限，找件场景要一次看全） |

#### `LocatePartBrief`（7 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | **string**（雪花） | `t_part.id` |
| `serial_no` | string \| null | `t_part.serial_no`（可空列；本端点按序列号命中 ⇒ 实际恒非空） |
| `name` | string | `t_part.name` |
| `drawing_no` | string | `t_part.drawing_no` |
| `is_urgent` | bool | `t_part.is_urgent` |
| `planned_delivery_date` | date | `t_part.planned_delivery_date` |
| `system_delivery_date` | date \| null | `t_part.system_delivery_date`（可空列） |

#### `LocateBatchItem`（16 字段，**键名即网络契约**）

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | **string**（雪花） | `t_part_batch.id`；写请求的批次锚 |
| `batch_no` | number | `t_part_batch.batch_no` |
| `quantity` | number | `t_part_batch.quantity`（批次数量） |
| `status` | string | `t_part_batch.status` **原文透出**（含终态；分类结论在 `action` / `disabled_reason`） |
| `version` | number | `t_part_batch.version`（OCC 锚） |
| `location` | string \| null | `t_part_batch.location` 真实值（**不是** `ScanListItem` 那个恒 null 的占位） |
| `holder_display` | string \| null | 三表 `COALESCE(t_shelf.name, t_worker.name, t_outsource_company.name)`（§8 多态歧义第 7 处） |
| `current_process_name` | string \| null | `t_process.name`（走 `current_process_id` 权威列；工序软删 ⇒ null） |
| `chain_state` | `"NONE"` \| `"NEXT"` \| `"TAIL"` | 与 `/held` 的 `chain_state` **同源**（同一片段），语义逐字相同 |
| `next_process_id` | **string \| null**（雪花） | 链内下一道工序 id。⚠️ **可空**，不是 `/held` 的 `"0"` 兜底 —— 诊断行用 `null` 表达「没有下一道」 |
| `next_process_name` | string \| null | 下一道工序名（下一道工序软删 ⇒ id 仍有值、名字 null） |
| `updated_at` | timestamp | `t_part_batch.updated_at`（位置事实的新鲜度） |
| `company_name` | string \| null | **仅 `status='OUTSOURCE'`**：持有人公司名；非外协批次恒 null |
| `sent_at` | timestamp \| null | **仅 `status='OUTSOURCE'`**：该批次**开口 shipment**（`t_outsource_shipment.status='OUTSOURCING'`）的 `sent_at`；找不到开口 shipment ⇒ null |
| `action` | `LocateAction \| null` | 见 §3.6；未传 `worker_id` ⇒ null |
| `disabled_reason` | `LocateDisabledReason \| null` | 见 §3.6；`action` 非空时恒 null |

`action` 的 4 个 wire 字面量：`"PICK_UP"` / `"MINE"` / `"TAKE_OVER"` /
`"RECEIVE_FROM_OUTSOURCE"`（`SCREAMING_SNAKE_CASE`）。
`disabled_reason` 的 **10 个**：`"PROGRAMMING"` / `"INSPECTION"` / `"TERMINAL"` /
`"WORK_TYPE_NOT_SET"` / `"PROCESS_NOT_IN_WORK_TYPE"` /
`"PROCESS_CHAIN_STEP_NOT_FOUND"` / `"PROCESS_CHAIN_REQUIRED"` /
`"WORKER_HOLD_LIMIT_EXCEEDED"` / `"WORK_TYPE_MAX_HELD_NOT_SET"` / `"UNKNOWN"`。

⚠️ **键集护栏**：`vo/locate.rs` 的单测 `locate_keys_are_pinned` 逐键钉死本模块三个
结构，两个枚举的字面量另有 `locate_action_literals_are_pinned` /
`locate_disabled_reason_literals_are_pinned` 钉住 —— 加 / 删字段或改字面量必须先改
它们，并同步前端 schema。

### 2.8 `POST /scan/take-over` 的入参 / 出参（端点 7，2026-10-15 S2 新增）

#### 入参（`TakeOverRequest`，5 字段）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string | **是** | 批次锚（雪花 id 字符串）。**只接受字符串**（`deserialize_i64`）；发数字 → 422 纯文本 |
| `badge_code` | string | **是** | **接管人**（扫码人）工牌码；旧持有人由批次行 `current_holder_id` 决定，**后端不需要、也不接受**客户端指定 |
| `version` | number | **是** | 批次 OCC 锚（与 `t_part_batch.version` 不一致 → `40901`）；**缺失 → 422 纯文本**（本仓 OCC 规约） |
| `next_process_id` | string | 条件 | 接管后要推进到的工序。**仅「链位置推导不出下一道」时必填**（判据见下）；解析失败 → `40001`。JSON 字符串形态（与 `worker-scan` 同款） |
| `note` | string | 否 | `t_part_event.note` 的附加备注；缺省自动文案见下 |

#### 出参（`TakeOverOut`，7 键，键名即网络契约）

| 字段 | 类型 | 说明 |
|---|---|---|
| `batch_id` | **string**（雪花） | 被接管的批次 |
| `part_id` | **string**（雪花） | 所属工单（批次行反查） |
| `from_worker_id` | **string**（雪花） | **旧**持有人（批次行接管前的 `current_holder_id`，**恒非空** —— holder 为 NULL 的行拒 `20103`） |
| `to_worker_id` | **string**（雪花） | **新**持有人（扫牌反查出的接管人） |
| `next_process_id` | **string**（雪花） | 接管后落到的工序 |
| `next_process_step_id` | **string \| null** | 目标工序在链内的 step；**无链时 `null`**（不是 `"0"` 兜底） |
| `version` | number | 接管**后**的批次锚（客户端传值 + 1），前端继续拿它做下一次动作的 OCC |

#### 判据顺序（**顺序即错误码优先级契约**，实现 = `service/transfer.rs`）

| # | 判据 | 失败码 | HTTP |
|---|---|---|---|
| 1 | 工牌反查 worker（`include_deleted=false`） | 不存在 `20201` / 停用 `20202` / 无工种 `20206` | 404 / 400 / 400 |
| 2 | 批次存在、part 存在 | `20109` / `20101` | 404 / 404 |
| 3 | 客户端 `version` 与批次 OCC 比对 | `40901` | 409 |
| 4 | `status='IN_PROCESS'` 且 `location='WORKER'` 且**有持有人** | `20103` | 400 |
| 5 | 持有人不是扫码人自己 | `20104`（「批次已在你手上，无需接管」） | 400 |
| 6 | 链位置解析：`pointer_consistent && chain_state=='NEXT'` ⇒ 免填；否则 `next_process_id` 必填、且经 `optional_step_id` 链内解析 | 缺 `40001`（**HTTP 422**）/ 链内定位不到 `20702`（**HTTP 404**） | 422 / 404 |
| 7 | 目标工序 ∈ 接管人工种的活跃工序映射（`WorkTypeRepo::list_process_ids`） | `20104` | 400 |
| 8 | 容量守卫（`ensure_worker_has_capacity`，只按 `max_held_batches` 判） | `20904` / `20204` | 400 / 409 |
| 9 | 原子 UPDATE（`transfer_to_worker`）0 行 | `40901` | 409 |

**第 4 步的「持有人存在」是 2026-10-15 的显式订正**：`location='WORKER'` 却没有
持有人的行是数据异常（任何写路径都不会产出）；「接管」的前提是挂在**别人**手上，
无对象可接管 ⇒ 与 `locate` 的 `UNKNOWN` 口径对齐（S1 决策表也同步订正）。

**第 6 步的两条分支**（与 `worker-scan` RETURNED 逐条同源）：

- **顺应且 `NEXT`**：后端按链推导下一道（`resolve_chain_position`），响应回传推导值；
  派生 SQL 内部不自洽（NEXT 但无下一道）按 worker-scan 同款拒 `20702`。
- **其余**（无链 / 链已软删 / 指针漂移 / 链内工序重复 / **链尾 TAIL**）：必须显式指定，
  且**有链时只能是链内工序**（越链 → `20702`；无链 ⇒ step 可为 `null`）。
  ⚠️ TAIL 接管只能指定链内工序 —— 「链尾做完了」在报工台的语义是放回 / 送检，
  不是接管到链外。

#### 写入行为（与 `worker-scan` 的差别）

- **不改 `status`**（`IN_PROCESS` 保持）、**不碰 `is_repairing`**（返修件接管后仍
  挂在返修流；回归 `tests/production/scan_take_over.rs::take_over_keeps_is_repairing_flag`）。
- 只改 `current_holder_id` / `location` / `current_process_id` /
  `current_process_step_id` / `version(+1)` / `updated_*` 六处，WHERE 带旧 holder 条件
  与 OCC（NULL 安全比较）。
- `PartService::sync_from_batch_change` 只调一次（part 派生 + 父装配件级联），随后写
  `TRANSFERRED` 事件：`worker_id` = 接管人、`process_id` = **接管后的目标工序**、
  `from_status` / `to_status` 均 `IN_PROCESS`、`badge_code` = 接管人工牌；
  `note` 两态 —— 无 body note：`"接管自 worker {旧持有人 id}"`；有：拼
  `"接管自 worker {id}：{note}"`（**全角冒号**）。
- **不做部分数量**（整批接管），响应 `batch_id` 恒是被接管的源批次。

⚠️ **键集护栏**：`vo/take_over.rs::take_over_keys_are_pinned` 逐键钉死 7 个 JSON 键
（内部管道字段 `synced_assembly_id` 必须被 `#[serde(skip)]` 挡在 JSON 外）。

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

### 3.6 `locate`（端点 6）的 action 决策表与写侧守卫对照

**判定纪律**（实现 = `listing/service.rs` 的 `decide_action` 一族纯函数）：

1. `worker_id` 未传 ⇒ `action` / `disabled_reason` 均 `null`（纯位置事实）；
2. `MINE`（件已在本工人手上）**不受任何闸门影响** —— 容量触顶 / 工种未绑定都不改变
   「这是我手上的件」；
3. 其余形态的 reason 优先级：**状态类 → 工种未绑定 → 工序 ∉ 工种 → 容量**，与写侧
   「先资格后容量」同序。

**分类表**（`status` + `location` + 持有人）：

| 批次形态 | 候选 `action` | 附加闸门（不满足 ⇒ `NONE` + 对应 reason） |
|---|---|---|
| `PENDING` | `PICK_UP` | ① **part 有链**（`t_part.process_chain_id IS NOT NULL`）时：链行已软删 / 链内**无未软删 step** ⇒ `PROCESS_CHAIN_STEP_NOT_FOUND`（与写侧 `first_step_in_chain` 逐谓词对齐，2026-10-15 S2-B 订正）；链首（`ORDER BY sort_order ASC, id ASC LIMIT 1`）∉ 我工种活跃映射 ⇒ `PROCESS_NOT_IN_WORK_TYPE`。② 容量：工种未配 max ⇒ `WORK_TYPE_MAX_HELD_NOT_SET`；held ≥ max ⇒ `WORKER_HOLD_LIMIT_EXCEEDED` |
| `IN_PROCESS` + `location='PRODUCTION_SHELF'` | `PICK_UP` | **只查容量** —— 写侧 pick-up 的 IN_PROCESS 分支不校验工序归属，读侧也不许校验（防「假阻挡」） |
| `IN_PROCESS` + `location='WORKER'` + holder==我 | `MINE` | 无 |
| `IN_PROCESS` + `location='WORKER'` + holder≠我（**且非空**） | `TAKE_OVER` | ① 当 `chain_state=='NEXT'` **且** `pointer_consistent`（写侧 `is_pointer_consistent` 的纯 SQL 表达）时：推导出的下一道 ∉ 我工种映射 ⇒ `PROCESS_NOT_IN_WORK_TYPE`；推导不出（无链 / 指针漂移 / 链内歧义 / 链尾）⇒ **仍给 `TAKE_OVER`**（前端弹手填，残余失败组合见 §8）。② 容量 |
| `OUTSOURCE` + `location='OUTSOURCE_COMPANY'` | `RECEIVE_FROM_OUTSOURCE` | ① 有链且 `chain_state=='NEXT'`：下一道 ∉ 我工种映射 ⇒ `PROCESS_NOT_IN_WORK_TYPE`；有链但 `chain_state!='NEXT'` ⇒ `PROCESS_CHAIN_REQUIRED`；**无链 ⇒ 放行**（前端收下一道）。② 容量 |
| `PROGRAMMING` | `NONE` | `PROGRAMMING`（指引找 CNC 编程） |
| `INSPECTION` | `NONE` | `INSPECTION`（指引找品检） |
| `READY_TO_SHIP` / `DELIVERED` / `COMPLETED` / `CANCELLED` | `NONE` | `TERMINAL` |
| `IN_PROCESS` + `location='WORKER'` + **holder 为 NULL**（数据异常） | `NONE` | `UNKNOWN`（2026-10-15 S2 订正：写侧 `take-over` 同形态拒 `20103`，读侧不得给可点按钮） |
| 其它（`IN_PROCESS` + 未知位置 / `OUTSOURCE` + 未知位置 / 未知 status） | `NONE` | `UNKNOWN`（保守出口，**出现即需补表**） |

**与写侧守卫对照表**（「写侧会成功的绝不显示 `NONE`、写侧会拒绝的绝不显示可点
按钮」是决策表的验收纪律；下表列出对位关系 —— S2 两个写面**已落地**
（`take-over` 与 pick-up 的 PENDING 链首落位），S3 仍未落地）：

| `action` / `reason` | 对位写侧 | 写侧守卫 / 错误码 |
|---|---|---|
| `PICK_UP`（PENDING / 架上） | `POST /scan/batches/{id}/pick-up` | worker active + `work_type_id`（`20201`/`20202`/`20206`）、容量（`20204`/`20904`）；**IN_PROCESS 分支不校验工序归属** |
| `PICK_UP` 的 `PROCESS_CHAIN_STEP_NOT_FOUND` | `POST /scan/batches/{id}/pick-up`（PENDING 链首落位，S2-B） | `20702`（链行软删 / 链内无活跃 step） |
| `PICK_UP` 的 `PROCESS_NOT_IN_WORK_TYPE` | 同上 | `20104`（链首 ∉ 工种映射） |
| `MINE` | 无写端点，前端引导到放回 / 送检流 | — |
| `TAKE_OVER` | `POST /scan/take-over`（S2-A，**已落地**） | `20702` / `20104` / `20103` / 容量两条；推导不出下一道时前端**必须弹手填**（`40001` / `20702` 兜底） |
| `RECEIVE_FROM_OUTSOURCE` | S3 写端点（未落地）；现状自动推导路径 = `POST /outsource-queue/move` | `20706`（推不出下一道时拒收 —— 现状写侧对「无链」也拒，S3 改为前端显式传 `next_process_id` 接收，故读侧对本形态**放行**，见 §8） |
| 容量两条 reason | 任意取件路径 | 与 `QueueService::ensure_worker_has_capacity` **同一判据**（`20904` 先于 `20204`） |

⚠️ **容量判据逐字同谓词**：held = `t_part_batch WHERE status='IN_PROCESS' AND
location='WORKER' AND current_holder_id=$w AND deleted_at IS NULL`（**复用 `/held` 的
`count_held` 同一条 SQL**）；max = 工人 `work_type_id` 对应的 `t_work_type.
max_held_batches`（`NULL` = 未配置，不是「不限」）。

⚠️ **读侧零跨域 import**：工人 / 工种 / 外协的查询全部内联在 `listing/repo.rs` 的
SQL 里（不 import `prod::worker` / `prod::queue` / `outsource` 任何符号），
由域隔离护栏守住（§7.1）。

## 4. 与 WS 的关系

| 端点 | 事件 | 触发条件 |
|---|---|---|
| `POST /scan/worker-scan` | `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED` | 无条件，**按响应的 `scan_out.event_type`** |
| `POST /scan/worker-scan` | `PART_BATCH_SPLIT` | 部分数量（§2.4.1，自动拆批） |
| `POST /scan/worker-scan` | `WORKER_POOL_REFILL_DONE` | 同事务 refill 抢到一批 |
| `POST /scan/worker-scan` | `WORKER_POOL_EMPTY` | refill **池空**。⚠️ **2026-10-14 起收窄**：`stopped_reason == "POOL_EMPTY"`，工人已拿满时不广播（见 §4.2） |
| `POST /scan/worker-scan` | `ASSEMBLY_UPDATED` | 父装配件真变了 |
| `POST /scan/batches/{id}/pick-up` | `PART_PICKED_UP` | 无条件 |
| `POST /scan/batches/{id}/pick-up` | `PART_BATCH_SPLIT` | 部分领取（自动拆批） |
| `POST /scan/take-over` | `PART_TRANSFERRED` | **无条件**（2026-10-15 S2 新事件，见下） |
| `POST /scan/take-over` | `ASSEMBLY_UPDATED` | 父装配件真变了（`sync_from_batch_change` 返回 `Changed`） |

- **事件名一字不改** —— dashboard 与队列页都在监听，改名会静默断链。
- **`PART_TRANSFERRED`（2026-10-15 S2 新增，本域第 7 条写端点的唯一产出点）**：
  payload 四键**全部 JSON 字符串**（雪花精度约定）：
  `{ part_id, batch_id, from_worker_id, to_worker_id }`。
  语义 = 「这一批换人了」；消费方（dashboard / 队列看板）据此刷新「工人持有」格子。
  ⚠️ 它**没有** `quantity` / `process_id` 等键 —— 需要更多事实的消费方从刷新后的
  列表读，不要指望事件带全量快照。⚠️ 广播**无集成断言**（见 §8）。
- 本域两条拆批写路径（worker-scan 部分放回 / 部分送检、pick-up 部分领取）共用
  `PART_BATCH_SPLIT`，payload 字段名**逐字同形**：
  `{ part_id, new_batch_id, source_batch_id, quantity }`（雪花 id 为 JSON string，
  `quantity` 为裸 number）。
- ⚠️ **同一事件名在本域外还有第三个产出方，payload 只有 2 个键**：顶层共用拆批端点
  `POST /api/v2/batches/split`（`prod::batch` 域）发的是
  `{ part_id, new_batch_id }`（见 `prod/batch/handler/lifecycle.rs::split_batch_by_body`）。
  ⇒ 消费方**只能无条件依赖 `part_id` 与 `new_batch_id` 这两个键**，
  `source_batch_id` / `quantity` 必须写成「缺失即不更新」而不是必填。
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

### 4.2 `WORKER_POOL_EMPTY` 的 payload 与 2026-10-14 的广播条件收窄

| 键 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string | 触发 refill 的工人 |
| `shelf_id` | null | 恒为 `null`（refill 已无架锚，worker-scan 也不再收它）；WS payload 与 HTTP 响应的 `refill.shelf_id` 同步 |
| `pool_empty` | bool | 恒 `true`（能走到这条分支就意味着 `taken` 为空） |
| `stopped_reason` | string | **2026-10-14 新增**。走这条分支时恒为 `"POOL_EMPTY"`（条件即由它推出） |

⚠️ **2026-10-14 有意的协议行为变更：工人拿满时不再广播。**
改造前的条件是 `pool_empty`，而 `pool_empty` 对「池空」与「**工人已拿满
`max_held_batches`**」两种成因**都为 `true`**（`take_one_from_pool` 对两者都返回
`None`，SQL 层失败形态一样）⇒ 工人在报工台放回 / 送检时即便「是自己已经拿满了」，
也会向所有订阅端推一条名为 `WORKER_POOL_EMPTY` 的事件，看板照样把停机原因归给
「池里没货」。现在改读 `refill.stopped_reason == "POOL_EMPTY"`，与
[`queue.md`](queue.md) §7 的 `POST /queue/refill` 逐字同款。

⚠️ **广播条件只认 `"POOL_EMPTY"`（2026-10-15 复核，未变）**：`WORKER_FULL`、
**`AUTO_LIMIT_REACHED`**（自动上限触顶）、`MAX_BATCHES_NOT_SET` 都**不广播**
`WORKER_POOL_EMPTY` —— 事件名表达的是「池里没货」，其余成因广播它会把看板归因带偏；
这些成因只有触发方从 HTTP 响应里看得到。

**两个 WS 产出点，都要读它**（漏一处就重新引入两套判据）：

| 产出点 | 端点 | payload 里的 `worker_id` |
|---|---|---|
| `prod::scan::handler::transition::worker_scan` | `POST /api/v2/prod/scan/worker-scan` | `scan_out.worker_id` |
| `prod::queue::handler::pool::admin_refill` | `POST /api/v2/prod/queue/refill` | 请求体 `worker_id` |

⚠️ **已决策不新增 WS event kind**（2026-10-14）。代价是「工人已拿满」这条状态在 WS 上
**完全没有信号** —— 只有直接看 `POST /scan/worker-scan` 响应的 `refill.stopped_reason`
（或 `POST /queue/refill` 响应体）才知道。接受的理由同 queue 域：拿满是**按工人维度**的
事实，看板每个工人格子各自刷新自己的 `current_held`，不需要一条全局广播。

⚠️ 两处 payload 的 `shelf_id` **形态不同**：本域恒 `null`（worker-scan 的 refill
跨架取料、无架锚），queue 域填请求里的架 id。

⇒ **4 个键两边同名**，只有 `shelf_id` 的值形态不同：`worker_id`（字符串）/ `pool_empty`
（恒 `true`）/ `stopped_reason`（恒 `"POOL_EMPTY"`）三键两边逐字相同。
⚠️ **2026-10-14 review 第 2 轮 MAJOR-N2 订正**：本节此前写「`stopped_reason` /
`pool_empty` / `worker_id` **三个**键两边同形」，而 `prod::queue` 那一侧当时**根本没
有 `stopped_reason` 键**（上一轮只把键加到了 scan 侧）⇒ 同一事件名两个 payload 形状。
现已补齐，本节改为「4 个键同名、3 个键同形、`shelf_id` 值形态不同」。
⚠️ 两处都是**手拼 `json!`、没有共享构造器** —— 刻意的（`shelf_id` 的差异是真实业务
差异，共用 builder 会诱使后人把差异抹平）。代价：改键集合必须**两侧同时改**，纪律
写在两侧代码注释 + [`queue.md`](queue.md) §7 的 payload 列登记里。

## 5. SQL 条数

| 端点 | 条数 | 说明 |
|---|---|---|
| `GET /scan/pickable` | **2** | 取行 + COUNT |
| `GET /scan/held` | **3** | 取行 + COUNT + 工种上限（**2026-10-14 由 2 增至 3**） |
| `GET /scan/locate` | **≤ 4**（未传 `worker_id` 时 2） | 工单命中 + 工人/工种上限 + 持有数（**复用 `/held` 的 `count_held` 同一条 SQL**）+ 批次取行。**2026-10-15 新增** |
| `POST /scan/verify-badge` | 1 | 按 `badge_code` 反查 |
| `POST /scan/worker-scan` | 多次 | 状态翻转 + 事件日志 + 选架 + 同事务 refill + 派生。RETURNED 臂**无条件**先跑 1 条链位置解析（整批路径也跑）；`0 < q < batch.quantity` 的部分数量路径**再加** 5 条：拆批 3 条（`max(batch_no)` / `INSERT ... SELECT` / 扣量 `UPDATE`）+ `SPLIT` 事件 1 条 + 新批次回读 1 条 |
| `POST /scan/batches/{id}/pick-up` | 多次 | **容量守卫 2 条**（工种 + 持有数）+ **PENDING 链首解析 1–3 条**（仅 PENDING 起点，2026-10-15 S2-B：`optional_process_chain` 恒 1 条；有链再 +2 —— `first_step_in_chain` + `list_process_ids`）+ 拆批（可选）+ 状态翻转 + 事件日志 |
| `POST /scan/take-over` | 多次（前置校验 7–9 条） | 工牌 1 + 批次 1 + part 1 + 链位置解析 1 + 链内 step **0–2**（仅非顺应分支：`optional_process_chain` + `optional_step_id`，无链时后者不查库）+ 工种工序 1 + 容量 2；写 1（`transfer_to_worker`）+ 派生 + 事件 1。**2026-10-15 S2 新增** |

三条只读端点都**不按行数重复查**：`pickable` 一次 SQL 把「工种→工序映射 → 架 →
批次」全链 JOIN 完；`held` / `locate` 一次 SQL 把链位置派生挂在 `LEFT JOIN LATERAL`
上，`locate` 另把 PENDING 链首 / 工种工序映射两个 `EXISTS` 与外协在途 shipment 的
`LEFT JOIN LATERAL` 一并 JOIN 完（**批次数不产生额外 SQL**）。

⚠️ **`held` 的第 3 条是 `fetch_max_held_batches`**（`t_worker LEFT JOIN
t_work_type` 取 `max_held_batches`，供 `HeldListOut` 的容量展示，2026-10-14 新增）。
它是「工人维度」的量，与取行 / COUNT 的「批次维度」量**互不覆盖、无法合并** ——
合并就得把 `t_worker` 拖进那条已经 JOIN 了 `t_part` / `t_part_batch` / 链 step 的
大 SQL 里，让分页 COUNT 为一个非分页的常量付出代价。

⚠️ **`held` 的 `total` 与 `current_held` 复用同一次 COUNT 的同一个值**（不是两条
SQL 两次查询）—— 再查一次不仅多一条 SQL，还会让两个字段在并发下可能出现
「`total` 与 `current_held` 差 1」的诡异现象。

⚠️ **`held` 的第 3 条必须裸写 SQL，不许 import `prod::work_type` / `prod::queue` 的
repo**：本域 `listing/` 子模块受域隔离护栏
`listing_aggregation_depends_on_no_other_domain` 扫，任何他域 import 都会让它变红。
读他域的**表**是允许的既定 pattern，import 他域的**符号**才违规。

⚠️ **`pick-up` 的容量守卫 2 条**（`work_type_get_by_id` + `count_held_by_worker`）
与 `move` 共用 `QueueService::ensure_worker_has_capacity`。守卫失败时事务回滚，
**零写副作用**（回归测试断言批次数量 / location / holder / version 一个都不变）。

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
| 工种↔工序映射（`pickable` 过滤；`take-over` 的「下一道 ∈ 工种」校验；pick-up PENDING 链首校验） | `t_work_type_process` |
| 货架（zone / active / 软删 / scope） | `t_shelf` |
| 工序名（链派生后取名） | `t_process` |
| 工艺链与 step 定位（含 `t_part_process_chain` 链行软删闸门） | `t_part_process_chain` / `t_process_chain_step` / `t_part.process_chain_id` |
| 工人（`verify_badge` / worker-scan / `take-over` 的按牌反查与容量） | `t_worker` |
| 工人工种 + 工种容量上限（`locate` 的工人上下文、`/held` 的 `max_held_batches`、`take-over` 的容量守卫） | `t_work_type` / `t_worker` |
| 外协在途公司与开口 shipment（`locate` 的 `company_name` / `sent_at`） | `t_outsource_company` / `t_outsource_shipment` |
| 事件日志（含 `TRANSFERRED`） | `t_part_event` |
| 货架↔工序映射（选架候选集） | `t_shelf_process` |

### 7.1 跨域依赖登记

**整域不适用** `shared::domain_guard`：`worker_scan` / `take_over` 是转发型用例，必然
import `part`（repo / 事件日志 / 状态机）、`assembly`（父件级联）、`prod::queue`
（同事务 refill / 容量守卫）、`prod::worker`（按工牌反查）、`prod::work_type`
（`take-over` 的工种工序校验，经 `WorkTypeRepo::list_process_ids`）等域。这与
`prod::queue` 的写端点「经本域 trait 转发他域单表查询」同款，是逐域剥离期间的既定
pattern。

**`listing/` 子模块零跨域**：3 条只读聚合端点（`pickable` / `held` / 2026-10-15 起的
`locate`）只需要上表里那几张表，全部在本目录的 SQL 里聚合，一处他域的 service /
repo 都不 import（连 `PartRepoTrait` 都不借 —— 那是一个 part 域的胖 trait，借它就
等于把 part 域拖进来，本模块直接收 `&mut PgConnection`）。这条由
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
8. **i64 字符串化**：所有雪花 id 一律序列化为 JSON string（query / body 的入参侧对应
   「只吃字符串」，见 §1 的 deserializer 口径）。
9. **部分数量（2026-10-11 新增）**：
   - 请求侧放回页 / 送检页各加一个数量输入，**发 JSON 字符串**（`"quantity": "4"`），
     不发数字（否则 422 纯文本）；缺省不传该键即整批，**老客户端零改动照常工作**。
   - ⚠️ **部署顺序：后端必须先上**（与第 1 条的 URL 硬切同一个理由，但这条**不报错**）：
     老服务端不认识 `quantity` 这个键，而本仓生产代码零 `deny_unknown_fields`
     ⇒ 键被 serde 静默忽略 ⇒ 请求 200、**整批**流转。现场表现是「工人填了 4 件、
     整批放回了 10 件」，零错误零告警。URL 硬切至少炸在 404 / 405（§1.2 实跑形态），
     这条炸在**静默**，所以发布单上要单独钉一句，别只写「后端先上」就以为都覆盖了。
   - 响应侧 `scan.batch_id` 现在是「本次实际被处理的那一批」。**不要**再拿请求里
     扫到的 `batch_id` 去回显 / 去推后继动作 —— 拆批场景下它指向余量（仍在工人手上）。
   - 成功后的列表刷新：余量批次仍在 `GET /scan/held` 里、拆出来的那批已不在 ⇒ 被处理的
     那一行是**数量变小**而不是消失；行数**不减**（同事务 refill 抢到料时会 **+N** 行，
     refill 结果见本响应的 `refill` 段（`RefillResult`）—— 该 VO 在 `queue.md` 的端点表
     里登记为端点 7 `POST /api/v2/prod/queue/refill` 的响应类型）。⇒ 放回 / 送检两页
     都必须重拉列表，不能只假定「扫掉一行」，也不能按「行数不变」做乐观更新。
   - `PART_BATCH_SPLIT` WS 事件与 pick-up 共用，payload 字段名同形（§4）；⚠️ 顶层
     `POST /api/v2/batches/split` 发的同事件只有 2 个键，消费方只能无条件依赖
     `part_id` / `new_batch_id`。已有该事件的处理函数时，worker-scan 这条**零改动**即生效。
   - ⚠️ **两种失败形态不同源，别用一个 catch 兜**：数量非法（`<= 0` / `> batch.quantity`）
     是业务信封 `20111`（**HTTP 400**，`{ code, message, data: null }`）；
     发数字则是 axum `JsonRejection` 的 **HTTP 422 纯文本**，**不进信封**
     ⇒ 前者走既有 `code != 0` 的错误提示路径，后者得单独处理（否则只会拿到一句裸 422）。
10. **2026-10-14 的三条契约（摘要见 §0.1，逐字段见对应小节）**：
    - `GET /scan/held` 换成 `HeldListOut`，新增 `current_held` / `max_held_batches`
      两个**裸 number**（§2.3.1）—— 取件页要显示「已拿 N / 上限 M」并据此禁用取件按钮；
    - `POST /scan/batches/{id}/pick-up` 新增 `20204`（容量触顶，HTTP 409）/ `20904`
      （工种未配上限，HTTP 400）两条错误码（§2.6.1）—— 都得单独给文案，**别落进
      通用 500 提示**；
    - `POST /scan/worker-scan` 的 `refill` 段新增可选键 `stopped_reason`（§2.5，语义与
      判据见 [`queue.md`](queue.md) §3.2），同时 ⚠️ `WORKER_POOL_EMPTY` 的广播被
      **收窄**为「真·池空」（§4.2）：「工人已拿满」这条状态 WS 上不再有信号，前端只能
      从**本响应体**拿 —— 放回 / 送检两页的「没抢到料」提示必须改读
      `refill.stopped_reason`，不能只依赖 WS 事件。
11. **2026-10-15：`refill.stopped_reason` 扩三值 + refill 目标口径变更**
    （`prod::queue` 的自动分配上限特性，整域口径见 [`queue.md`](queue.md) §4.8）：
    - `refill`（本端点内嵌的那段与 `POST /queue/refill` 同 VO）现在**补到工种自动上限**
      `t_work_type.auto_allocate_limit`（未配置兜底 `max_held_batches`）；
    - `stopped_reason` 新增 `"AUTO_LIMIT_REACHED"`：工种配了自动上限且工人已达该上限
      （但**未**触顶 `max_held_batches`）⇒ 自动补料停下，**工人仍可走手动路径
      （pick-up / move）取到 max**。放回 / 送检两页的「没抢到料」提示按三值分支，
      `AUTO_LIMIT_REACHED` 应提示「自动补料已到上限，可手动领取」；
    - ⚠️ `WORKER_POOL_EMPTY` 的广播条件**不变**（仍只认 `POOL_EMPTY`，见 §4.2）；
    - pick-up 的容量守卫**不受自动上限影响**（仍只按 `max_held_batches` 判，§2.6.1）。
12. **2026-10-15 S2：扫码接管前端接入 + PENDING 领取行为变更**（§0.0 / §2.8 / §2.6.2）：
    - **新端点** `POST /api/v2/prod/scan/take-over`（Manager / ShelfAccount）：
      `locate` 给出 `TAKE_OVER` 的批次用「诊断行的 `batch_id` + `version`」直接发起；
      推导不出下一道时**必须弹手填工序**（缺 → `40001`，链外 → `20702`）。成功后可
      拿响应的 `version` 继续做 OCC 锚。错误码矩阵见 §2.8 —— 注意 `40001` → HTTP
      **422**、`20702` → HTTP **404**（本仓集中映射的结果，别按「业务错一律 400」
      处理）。
    - **新 WS 事件** `PART_TRANSFERRED`（payload 四键全字符串，§4）：消费方按新事件名
      接入刷新（dashboard / 队列看板的「工人持有」格子）；它**只有四个键**，别指望带
      全量快照。
    - **PENDING 领取行为变更**：`pick-up` 现在可能返回 `20702`（HTTP 404）/ `20104`
      （HTTP 400）（§2.6.2），且领取后的批次工序两列 = 链首（不再是 NULL）；
      `PICKED_UP` / `SPLIT` 事件的 `process_id` 同理（无链仍 `null`）。取件页的失败
      文案要给这两个码留位置。

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
了它（2026-09-30 拆 `MoveLocation` 时与下发流入参混在同一文件）。现按**消费方**
归位到 `prod::scan::dto`，queue 侧已删除。域归属按消费方判定，与「谁先写出来」无关。

---

**`list_by_work_type`（`GET /parts/by-work-type/{id}`）留在 part 域**

它是 part 域 list 族里唯一的「按工种列工人持有件」读端点，**零前端消费方**但有集成
测试覆盖其 part 侧投影，故未随报工台那两条 list 端点一起迁走。它的取行投影已从共用
struct 拆成独立的 `WorkTypeByWorkTypeRow`（只含它自己 SELECT 的 8 列），不再与报工台
的行投影共享代码。
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

---

**`PART_BATCH_SPLIT` 广播无集成断言（两条拆批写路径皆然）**

本域两个会拆批的写路径（`worker-scan` 的部分放回 / 部分送检、`pick-up` 的部分领取）
在 commit 之后都会补发 `PART_BATCH_SPLIT`（§4），但**两条都没有集成测试断言这条广播**：
`tests/production/scan_worker_scan.rs` 与 `tests/production/pickup.rs` 断言的是 HTTP
响应、库里 `t_part_batch` 的行形态与 `t_part_event` 的事件日志（`SPLIT` 的 `quantity`
与 `batch_id` 都钉了），**`ws_hub` / `WsEvent` 完全不在断言面内**。

后果：广播的**事件名或 payload 键名被改错不会让任何测试变红** —— 被测的那条
`t_part_event` 的 `SPLIT` 记录与 WS 广播是两次独立的写入，前者对不后者。事件名对
dashboard 与队列页是硬契约（改名会静默断链），而 `kind` 在后端是裸 `String`、无枚举
保护（`src/infra/ws_hub.rs` 的 `WsEvent::DashboardEvent { kind: String, … }`），
两侧都没有编译期约束兜底。

改这两个端点的广播时，除了跑一遍端到端手动联调，别依赖现有测试给你兜底。

---

**`locate` 的 `holder_display` 是三表 COALESCE 多态歧义的**第 7 处**（2026-10-15）**

`LocateBatchItem.holder_display` 沿用 `COALESCE(sh.name, w.name, oc.name)`
（`t_shelf` / `t_worker` / `t_outsource_company` 三张表按 `current_holder_id` 各
LEFT JOIN 一次），该写法**假定** holder id 在三表 PK 空间里互不重叠；一旦某 id 同时
命中其中两表，取到的是 `t_shelf.name`。逐条论证与「修时必须全部一起改、本次刻意不
修」的理由见 `prod::inspection::repo` 的已知缺陷登记（本行是它列的全仓第 7 处，就地
注释在 `listing/repo.rs::fetch_locate_batches` 的 doc）。

**症状**：同一批次在 `locate` 与别的端点可能显示不同的持有人名（取决于那个端点查不查
`t_shelf`）。**不是** `locate` 引入的回归 —— 它是该形态的既有传播面又一处。

---

**`locate` 不回退 `t_assembly`（与 `prod::inspection` 扫码树的**有意分叉**）**

`prod::inspection` 的 `GET /scan/{serial_no}` 命中口径是「先查 `t_part.serial_no`、
未命中**回退** `t_assembly.serial_no`」；`locate` **刻意不做这层回退** —— 报工台的
「找件」对象始终是**可作业的子件批次**，装配件序列号本身没有批次，回退过去只能返回
一棵没有可行动作的树。未命中文案提示「如为装配件，请扫子件序列号」。

两处口径并存、不统一：inspection 是**品检查询**（要能扫总成看整棵树），locate 是
**作业前定位**（要能立刻点按钮）。改任一处时必须先确认另一处的消费方不受影响。

回归：`tests/production/scan_locate.rs::locate_assembly_serial_not_fallback_returns_20101`
（真是造了一个带序列号的 assembly 行去证明「不是查不到、是刻意不查」）。

---

**`locate` 放行、写侧可能拒的残余失败组合（TAKE_OVER 手填流 / 外协收回，2026-10-15）**

决策表（§3.6）刻意只收「读侧能确定写侧会拒」的闸门；以下形态 `locate` **给可点
按钮**，而写端点（或现状写路径）仍可能拒绝 —— 前端必须把手填流与错误码兜底保留，
**不要**把 `locate` 的 `action` 当成功保证：

1. ~~**PENDING + 链首 ∈ 工种映射，但链内同一工序重复 / 指针与链位置漂移**~~
   **已闭合（2026-10-15 S2-B）**：PENDING 写侧改为直接取 `first_step_in_chain` 的
   链首（不再走「链内定位歧义 ⇒ 拒」的 `resolve_chain_position` 那一轮），与读侧
   闸门逐谓词对齐 ⇒ 该组合不再存在（并发窗口另计）。
   ⚠️ 残余的**reason 优先级分歧**：PENDING 同时踩「链问题 + 容量问题」时，`locate`
   先报链类 reason、写侧按既有优先级先报容量类错误（`20904` / `20204`）—— 两侧都拒，
   只是提示不同；不要试图把写侧改成「链先于容量」（那会改变既有错误码优先级）。
2. **TAKE_OVER 的「推导不出」分支**（无链 / 指针漂移 / 链尾 / 链内歧义）：读侧仍给
   `TAKE_OVER`（前端弹手填下一道），手填值经写侧 `optional_step_id` 校验 ——
   **有链但链内没有该工序 ⇒ 写侧 `20702`**，无链 ⇒ 放行（step 可为 `null`）。
   即「链尾 + 手填链内工序」是唯一「可点但可能被拒」的接管形态。
3. **外协收回无链**：读侧放行（前端收下一道），现状自动推导路径（`POST
   /outsource-queue/move` 不带 `to.next_process_id`）对无链**拒 `20706`** ——
   S3 写端点落地后按「显式 `next_process_id`」接收，届时本分支的残余失败面收窄。

`locate` 是**诊断**不是**预演**：它按当前读到的数据回答「大概率能做什么」，
最终裁决永远在写端点。上述组合若在真机上频繁出现，正确处置是**补一手写侧闸门的
读侧对位**（同时更新 §3.6 的对照表与 `tests/production/scan_locate.rs`），而不是
让前端按错误码事后补救。

---

**`PART_TRANSFERRED` 广播无集成断言（2026-10-15 S2）**

`POST /scan/take-over` 在 commit 之后无条件广播 `PART_TRANSFERRED`（§4），但
`tests/production/scan_take_over.rs` 断言的是 HTTP 响应 / 库里的 `t_part_batch` 行
形态 / `t_part_event` 的 `TRANSFERRED` 事件，**`ws_hub` / `WsEvent` 不在断言面内** ——
与本域既有的 `PART_BATCH_SPLIT`（上一条）同一处境：事件名 / payload 键名被改错不会让
任何测试变红。改这条广播时必须端到端手动联调，别依赖现有测试兜底。

---

**`locate` 的两处口径订正（2026-10-15 S2，以写侧为准）**

1. **PENDING 链首可用性**：`has_process_chain`（`HAS_PROCESS_CHAIN_EXPR` 投影，
   不带链行软删闸门）→ `has_usable_chain_head`（链行未软删 **且** 链内有活跃 step，
   与写侧 `first_step_in_chain` 逐谓词对齐）。修的是「链行已软删但 step 仍活跃」时
   读侧给 `PICK_UP`、写侧 `20702` 必拒的分叉；回归
   `tests/production/scan_locate.rs::locate_pending_with_soft_deleted_chain_returns_none_with_step_not_found`。
2. **holder 为 NULL 的 `WORKER` 行**：原先落 `TAKE_OVER`（注释写「判成可接手由写侧
   兜底」），而写侧 `take_over` 对同形态拒 `20103`（无对象可接管）⇒ 改为
   `NONE + UNKNOWN`；回归
   `tests/production/scan_locate.rs::locate_worker_location_without_holder_returns_none_unknown`。
