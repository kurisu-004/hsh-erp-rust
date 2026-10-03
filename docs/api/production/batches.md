# prod::batch 域 API —— 车间下发 + `t_part_batch` 生产流转

> 本文件须与 `src/modules/prod/batch/{handler/,service/,repo/,dto.rs,vo.rs,mod.rs,model.rs,status_gate.rs}` 保持同步（2026-10-03 订正：`handler` / `service` / `repo` 为目录制，与 [`../parts/inspection.md`](../parts/inspection.md) 实现位置段同一口径。）
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **范围一 —— 车间下发**（PENDING 批次专用域）：UI「待下发队列」展示 + 一键 / 批量 / 自动预览 3 路径。
> 2026-09-29 新增 + 2026-09-30 重构：
> - dispatch 统一 bulk-only（单条下发即 `targets.length == 1`）
> - auto-dispatch 改为只读 preview（不再真下发，返回首道工序 + 首货架 + skip_reason）
> - bulk-dispatch 端点删除（路由层不再挂载）
>
> **范围二 —— `t_part_batch` 生产流转**（2026-10-02 自 part 域迁入的 25 条以单个批次为
> 操作对象的路由：19 条子资源 + 3 条静态批量 / 事件 + 3 条集合读），逐条清单见
> [下方「t_part_batch 子资源迁入」节](#2026-10-02-t_part_batch-子资源迁入)。本域端点总数 3 → 28。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/batches/pending` | **Manager+Clerk+Inspector** | 车间 PENDING 批次列表（JOIN 工单 + 客户 L1+L2 + 申请人） |
| POST | `/api/v2/prod/batches/dispatch` | **Manager+Clerk** | bulk-only 下发：`targets` 数组顺序执行，单批即 `targets.length==1`；任一失败 → 全回滚 |
| POST | `/api/v2/prod/batches/auto-dispatch` | **Manager+Clerk** | **只读预览**：返回每个 batch 的首道工序 + 首货架 + `skip_reason`；前端据此构造 dispatch 请求 |

> 路由挂载：`prod::mod::router().nest("/batches", batch::router())` —— 见 `src/modules/prod/mod.rs`。
> 旧 `/batches/bulk-dispatch` 端点 404（router 层不再挂载）。

---

## 2026-10-02 t_part_batch 子资源迁入

`t_part_batch` 是生产执行单元，其 OCC / `status_gate` rollup / 状态机本体
（`PartBatchRepo` + `status_gate` + 批次 SQL）整体归 prod 域，25 条「以单个批次为
操作对象」的路由随之从 `/api/v2/parts/*` 迁到 `/api/v2/prod/batches/*`。
**URL 硬切换，无 alias**；旧路径 404。

路径锚点由 `part_id` 改为 `batch_id`（`t_part_batch.id` 全局唯一即锚点），
子资源端点的 `batch_id` **同时从请求体删除**。

**留在 part 域的判据**：操作对象是**多个批次**或**根本不是批次**——
`POST /parts/{part_id}/cancel`（翻转该 part 全部活跃批次）、
`POST /parts/{part_id}/force-complete`（全部非 CANCELLED 批次）、
`POST /parts/{part_id}/soft-delete`、`GET /parts/{part_id}/batches`，
以及全部 CRUD / 文件 / 各类 list 端点。

### 单批流转子资源（19 条，锚点 = `batch_id`）

| Method | Path | 权限 | 说明 | 文档章节 |
|---|---|---|---|---|
| POST | `/api/v2/prod/batches/{batch_id}/to-inspection` | Manager / Inspector | 单件送检 | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesbatch_idto-inspection) |
| POST | `/api/v2/prod/batches/{batch_id}/to-ship` | Manager / Inspector | 单件通过品检 | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesbatch_idto-ship) |
| POST | `/api/v2/prod/batches/{batch_id}/to-process` | Manager / Inspector | 单件指定下一工序 | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesbatch_idto-process) |
| POST | `/api/v2/prod/batches/{batch_id}/deliver` | Manager / Clerk | READY_TO_SHIP → DELIVERED | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_iddeliver) |
| POST | `/api/v2/prod/batches/{batch_id}/complete` | Manager / Clerk | DELIVERED → COMPLETED | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idcomplete) |
| POST | `/api/v2/prod/batches/{batch_id}/start-repair` | Manager / Clerk / Inspector | 置 `is_repairing=true` | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idstart-repair) |
| POST | `/api/v2/prod/batches/{batch_id}/place-on-shelf` | Manager / Clerk | 上架 | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idplace-on-shelf) |
| POST | `/api/v2/prod/batches/{batch_id}/recall-to-pending` | Manager / Clerk | 召回至 PENDING | lifecycle.md 尚无独立章节（见 [`../parts/index.md`](../parts/index.md) 端点表） |
| POST | `/api/v2/prod/batches/{batch_id}/release-from-programming` | Manager / Clerk | 编程完成释放 | lifecycle.md 尚无独立章节；20706 守卫见 [`./process-chain.md`](./process-chain.md#20706-biz_process_chain_required) |
| POST | `/api/v2/prod/batches/{batch_id}/send-to-outsource` | Manager / Clerk / Inspector | 派发外协（APPROVAL / DIRECT 双模式 + 部分发送） | [外协流转](#外协流转send--receive)（本节） |
| POST | `/api/v2/prod/batches/{batch_id}/receive-from-outsource` | Manager / Clerk / Inspector | 外协回收入库（支持部分接收） | [外协流转](#外协流转send--receive)（本节） |
| POST | `/api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection` | Manager / Clerk / Inspector | 外协回收 → 品检（整批） | [外协流转](#外协流转send--receive)（本节） |
| POST | `/api/v2/prod/batches/{batch_id}/complete-repair` | Manager / Clerk / Inspector | 完成维修 | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idcomplete-repair) |
| POST | `/api/v2/prod/batches/{batch_id}/repair-dispatch` | Manager / Clerk / Inspector | 派发维修 | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idrepair-dispatch) |
| POST | `/api/v2/prod/batches/{batch_id}/scan-inspect` | Manager / Inspector | 扫码品检 | [`../parts/inspection.md` 状态机表](../parts/inspection.md#状态机can_transition_to-白名单)（尚无独立章节） |
| POST | `/api/v2/prod/batches/{batch_id}/split` | Manager / Clerk | 拆分批次 | [`../parts/batch.md`](../parts/batch.md#post-apiv2prodbatchesbatch_idsplit) |
| POST | `/api/v2/prod/batches/{batch_id}/cancel` | Manager / Clerk | 取消**单个**批次 | [`../parts/index.md`](../parts/index.md)（尚无独立章节） |
| POST | `/api/v2/prod/batches/{batch_id}/pick-up` | Manager / Clerk / ShelfAccount | 手动 pick-up 兜底（**支持部分领取**：传 `quantity` 小于批量时自动拆批） | [`../parts/lifecycle.md`](../parts/lifecycle.md#post-apiv2prodbatchesbatch_idpick-up) |
| POST | `/api/v2/prod/batches/scan/deliver` | Manager / ShelfAccount | 扫码发货（**无 Path**，`ScanDeliverPartRequest` body 不变） | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesscandeliver) |

### 静态批量 / 事件（3 条，无 Path，请求体逐字不变）

| Method | Path | 权限 | 说明 | 文档章节 |
|---|---|---|---|---|
| POST | `/api/v2/prod/batches/to-ship` | Manager / Inspector | 静态批量通过品检（`items[].batch_id` 仍在 body） | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesto-ship) |
| POST | `/api/v2/prod/batches/to-inspection` | Manager / Inspector | 静态批量送检（`items[].batch_id` 仍在 body） | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesto-inspection) |
| POST | `/api/v2/prod/batches/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检；成功后同事务触发 worker-pool refill | [`../parts/inspection.md`](../parts/inspection.md#post-apiv2prodbatchesworker-scan) |

`worker-scan` 保持**无 Path extractor** + `serial_no` 主键 + `batch_id` 可选消歧。它一笔
事务改 2 个批次（扫的那个 + 同事务从工人池补的），是全仓唯一的跨 part 批次写点，但动作
语义仍是「以批次为对象的工人报工」，故归 prod。

### 集合读（3 条，从 parts 迁入，与已有 `/prod/batches/pending` 并列）

| Method | Path | 权限 | 说明 | 文档章节 |
|---|---|---|---|---|
| GET | `/api/v2/prod/batches/inspection` | Manager / Inspector | 待品检批次列表（判据 `status='INSPECTION'`）；**2026-10-03 VO 收口为 `InspectionQueueItemOut`（13 字段），查询参数加表头筛选 + 服务端排序，与 repair / repairing 不再共用** | [`../parts/inspection.md`](../parts/inspection.md#get-apiv2prodbatchesinspection) |
| GET | `/api/v2/prod/batches/repair` | Manager / Inspector | 维修批次列表（判据 `status='DELIVERED'`） | [`../parts/lifecycle.md`](../parts/lifecycle.md#get-apiv2prodbatchesrepair) |
| GET | `/api/v2/prod/batches/repairing` | Manager / Inspector | 维修中批次列表（判据 `is_repairing = true`） | [`../parts/lifecycle.md`](../parts/lifecycle.md#get-apiv2prodbatchesrepairing) |

### 注册顺序（axum / matchit）

- 静态段必须先于 `/{batch_id}` 注册。静态批量 3 条与集合读 3 条是 1 段、子资源是 2 段，
  **段数不同，无冲突**。
- 但 `POST /prod/batches/scan/deliver`（2 段，首段静态 `scan`）与
  `POST /prod/batches/{batch_id}/*`（2 段，首段动态）**同段数**，靠 matchit 的静态优先
  规则消解 —— 必须实测本路径未被 `/{batch_id}` 吞掉。
- **不要**新增 `GET /prod/batches/{batch_id}`：它与上面 3 条静态集合读同形状，
  会引入歧义（当前设计上单批详情走 part 域 `GET /parts/{part_id}/batches`）。

### 错误码语义变更（20109 / 20101）

| code | 变更前 | 变更后 |
|---|---|---|
| 20109 `BIZ_PART_BATCH_NOT_FOUND` | 传一个「不属于该 part 的 `batch_id`」（靠 SQL 的 `AND part_id = $2` 判定） | **退化为**「批次不存在 / 已软删 / 状态不是流转起点」—— `batch_id` 全局唯一即锚点，「跨 part 批次」不再是可表达的场景。留在 part 域的 `cancel` / `force-complete` 操作对象是该 part 的多个批次、不接受 `batch_id` 入参，故不返回 20109 |
| 20101 `BIZ_PART_NOT_FOUND` | 传了不存在的 `part_id` | 仍可达，语义不变：只能经由「批次的 part 已软删」触发 |

登记处见 [`../inconsistencies.md`](../inconsistencies.md)。

> **待办（2026-10-02）**：批次子资源迁出后，`parts/{index,inspection}.md` 中若干
> `src/modules/part/**` 实现位置引用待回填（handler 目录 / 文件切分以代码为准）——
> 包括 `inspection.md` 的 `by-serial/…/part-batches` 端点实现位置、`to-process` 返修
> 守卫位置、`index.md` 的仓库分层图与文件头同步声明、`statemachine.rs` 与
> `status_gate` 写入口 CI 测试的 Rust 模块路径。

---

## 外协流转（send / receive）

3 条端点，写侧实现集中在 `src/modules/prod/batch/service/outsource.rs`
（`BatchService::send_to_outsource` / `receive_from_outsource` /
`receive_from_outsource_to_inspection`），handler 在
`src/modules/prod/batch/handler/lifecycle.rs`（事务边界在 handler）。

| 端点 | 迁移 | shipment 记账 |
|---|---|---|
| `POST /{batch_id}/send-to-outsource` | `PENDING → OUTSOURCE`（`location='OUTSOURCE_COMPANY'`） | 同事务 INSERT `t_outsource_shipment`（`OUTSOURCING`） |
| `POST /{batch_id}/receive-from-outsource` | `OUTSOURCE → IN_PROCESS`（`location='PRODUCTION_SHELF'`） | 整批回收才把开口 shipment 标 `RECEIVED` |
| `POST /{batch_id}/receive-from-outsource-to-inspection` | `OUTSOURCE → INSPECTION`（`location='INSPECTION_SHELF'`） | 整批回收，口 shipment 标 `RECEIVED` |

三者的角色守卫都是 **Manager + Clerk + Inspector**。

### `POST /{batch_id}/send-to-outsource`（`SendToOutsourceRequest`）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC 锚 `t_part_batch.version`（批次级） |
| `outsource_company_id` | string(i64) | ✓ | 必须在册 + `is_active` |
| `process_id` | string(i64) | ✓ | 外协工序；`t_process.category` 必须 `'OUTSOURCE'`，且该公司必须映射它 |
| `quote_id` | string(i64)? | — | **APPROVAL 模式**：APPROVED 报价；`unit_price = quote.price` |
| `direct` | bool? | — | **DIRECT 模式**（免审批直发），与 `quote_id` **互斥** |
| `quantity` | i32? | — | 部分发送数量；缺省或 `== 批次量` = 整批 |
| `note` | string? | — | 落到 `t_part_event.note` 与 quote event `SENT` 的 note |

> ⚠️ **工序字段名 = `process_id`，不是 `next_process_id`**（2026-10-03 登记）。本字段
> **无 `#[serde(default)]`**，是必填：`SendToOutsourceRequest` 也**没有**
> `deny_unknown_fields`，所以发 `next_process_id` 会被 serde **静默丢弃**，紧接着因
> 必填字段缺失而失败。失败形态是 **`422` + 纯文本**（axum `Json` 提取器的
> `MissingField`，`tests/iam/wx_bind.rs` 有同形态先例），响应体形如
> `Failed to deserialize the JSON body into the target type: missing field \`process_id\``，
> **不是**业务信封、也不是 `BIZ_PROCESS_NOT_FOUND` —— 排查未升级的历史客户端时按这个
> 特征认。
>
> 取舍：DTO **不加** `deny_unknown_fields`。加了以后任何多余字段都直接 422，迁移面
> 远大于收益（会连带打到 Python v1 客户端等已上线的调用方）；字段名保持 `process_id`
> 是因为它与已上线的 Python v1 客户端绑定，改名会破坏它。**前端已适配**
> （`SendToOutsourcePayload` 的键为 `process_id`，并有契约用例逐字钉死 + 反断言禁止
> `next_process_id` 出现在 body 里），两个仓同一次编排合入。

**价来源二选一**：`direct` 与 `quote_id` 必须恰给一个，否则 `400 20104 BIZ_INVALID_VALUE`。

- **APPROVAL**：`quote_id` 必须是 `APPROVED`，且 `part_id` / `outsource_company_id` /
  `process_id` 三者与本次请求一致（否则 `400 21302`）。
- **DIRECT**（2026-10-03 由 501 stub 落地）：
  1. 按 `(part_id, outsource_company_id, process_id)` 找活跃（`SUBMITTED` /
     `APPROVED`）报价里的 **APPROVED** 条目 → 命中则**复用**它（`unit_price` = 该报价单价）；
  2. 未命中 → 自动 INSERT 一条 `price = 0` / `status='APPROVED'` /
     `is_direct = true` 的占位报价，`note` 固定写
     `DIRECT 直发自动创建（免审批，单价待对账补录）`，再用它的 id 走同一条
     APPROVAL 校验 + 写 `SENT` 事件路径。**对账页单价为 0 的行据此识别**。
  3. **幂等靠两条互补的 partial 唯一索引**（谓词互斥，缺一不可）：

     | 索引 | 键 | 谓词 | 拦的是谁 |
     |---|---|---|---|
     | `uq_t_outsource_quote_approved_part_process`（baseline） | `(part_id, process_id)` | `deleted_at IS NULL AND status='APPROVED' AND is_direct = false` | 审批报价：每 (零件, 工序) 最多一条 |
     | `uq_t_outsource_quote_direct_part_company_process`（**migration 008，2026-10-03 新增**） | `(part_id, outsource_company_id, process_id)` | `deleted_at IS NULL AND status='APPROVED' AND is_direct = true` | DIRECT 占位报价：每 (零件, 公司, 工序) 最多一条 |

     第一条**故意**排除 `is_direct = true`（免审批直发不该占用审批报价的唯一键 —— 这
     正是 `is_direct` 列存在的意义），代价是单靠它兜不住 DIRECT 行；migration 008
     补上后半条后，下面的 `ON CONFLICT DO NOTHING` + 回查才**真正成立**：并发下第二
     个 INSERT 命中该索引 → 0 行 → 回查取第一条的 id 当 `quote_id`，同一 tuple 恒定
     只留一条 0 元占位报价（回归用例
     `send_to_outsource_direct_same_tuple_keeps_single_placeholder_quote`）。

两条路径都会写 `t_outsource_quote_event` `SENT`（`from_status='APPROVED'` →
`to_status='APPROVED'`，不改 quote 状态）。

#### 部分发送（`quantity`）

| `quantity` | 行为 |
|---|---|
| 缺省 / `== 批次量` | 整批发送，**不拆批** |
| `0 < q < 批次量` | 拆批发送：源批次 `quantity -= q` 且**状态 / location / holder 全不变**，新子批次走完整的发外协流程；shipment 挂在**子批次**上、`quantity = q` |
| `q <= 0` 或 `q > 批次量` | `400 20104 BIZ_INVALID_VALUE`（批次未被改动） |

拆批统一走 `PartBatchRepo::_split_batch_inner`（同一事务内 max(batch_no)+1 / INSERT /
UPDATE 三条 SQL，源批次 UPDATE 带 `version` OCC + `quantity > q` 数量守卫，命中 0 行 →
`409 40901`）。OCC 锚分两段：**源批次**用请求里的 `version`，**子批次**用读回行的
`version`（`_split_batch_inner` 把新批次 `version` 写死 0，拿请求的 `version` 去撞子
批次会恒 409）。

> ⚠️ **拆批成功后源批次的 `version` 已经 +1**（`_split_batch_inner` 的源批次 UPDATE 带
> `version = version + 1`）。所以**同一个源批次的第二次部分发送必须先刷新列表**拿新
> `version`，否则恒 409 —— 这是设计意图（OCC 挡住「基于旧读数继续拆」），不是缺陷。
> 典型序列：发 5 件（`quantity=2`，源批次 v0 → v1，余量 3）→ 再发 2 件必须带
> `version=1`；第二次会再拆一个新子批次并把源批次顶到 v2。
>
> `t_part` 派生状态：min-progress 下源批次 `PENDING`(rank 0) 慢于子批次
> `OUTSOURCE`(rank 3)，故**部分发送后 `t_part.status` 仍为 `PENDING`**（回归用例
> `send_to_outsource_partial_quantity_splits_batch` 断言该值）。

`t_part_event`（`SENT_TO_OUTSOURCE`）的 `batch_id` 指向真正发出去的那个批次、
`quantity` 记本次发送量。

### `POST /{batch_id}/receive-from-outsource`（`ReceiveFromOutsourceRequest`）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC 锚（**源批次**的 `t_part_batch.version`） |
| `shelf_id` | string(i64) | ✓ | 目标货架，`zone` 必须 `PRODUCTION` |
| `next_process_id` | string(i64) | ✓ | 收回后重新入池的工序；`t_shelf_process` 必须映射 |
| `quantity` | i32? | — | 部分接收数量；缺省或 `== 批次量` = 整批 |
| `note` | string? | — | 落到 `t_part_event.note` 与 quote event `RECEIVED` 的 note |

> ⚠️ **本端点与 send 端的工序字段名不同，勿互相套用**：`send-to-outsource` 用
> `process_id`（外协**这道**工序），`receive-from-outsource` 用 `next_process_id`
> （收回后**下一道**工序，用于重新入池）。两者都是必填、都没有 `#[serde(default)]`。

2026-10-03 起入参**不再复用** `PlaceOnShelfRequest`（后者仍被 `place-on-shelf` /
`release-from-programming` 共用，加 `quantity` 会污染它们的契约）。

#### 部分接收（`quantity`）与 shipment 记账口径

| `quantity` | 批次 | shipment | quote event |
|---|---|---|---|
| 缺省 / `== 批次量` | 整批回生产架 | 开口 shipment 标 `RECEIVED` + 写 `received_at` | 写 `RECEIVED` |
| `0 < q < 批次量` | 拆批：新子批次回生产架（`IN_PROCESS` + `PRODUCTION_SHELF`），源批次**留在外协厂**（`OUTSOURCE` + 余量 `-= q`） | **不动** —— 仍 `OUTSOURCING`、`received_at` 仍 NULL | **不写** |
| `q <= 0` 或 `q > 批次量` | `400 20104`（批次未被改动） | 不动 | 不写 |

**记账口径（有意为之，勿"顺手修"）**：shipment 记的是**发出时**的全量。例：发出 10 件
@ 单价 P，部分回收 6 件时只拆批次，源批次保留余量 4 件且继续挂着那张
`OUTSOURCING` shipment；`received_at` / `status='RECEIVED'` 只在**整批**回收时才落。
所以对账列表里 `shipment.quantity` 与批次当前余量**可能不相等** —— 对账要回答的是
「发出去多少、单价多少」。`t_part_event`（`RECEIVED_FROM_OUTSOURCE`）仍记本次回收量
与子批次 id。

**二次回收必须先刷新列表**（与部分发送同款）：第一次部分回收把源批次 `version` 顶到
+1，第二次请求带旧 `version` 恒 409。链式示例（发出 5 件、源批次初始 v0）：

| 步骤 | 请求 | 结果 | 源批次 version |
|---|---|---|---|
| 1 | `quantity=2` | 拆批：子批次 2 件回生产架，源批次余量 3 件仍 `OUTSOURCE`，shipment 仍开口 | 0 → 1 |
| 2 | `version=1`，不带 `quantity`（或 `quantity=3`） | 整批回收余量：源批次 `IN_PROCESS` + `PRODUCTION_SHELF`，**开口 shipment 此刻才关**（`RECEIVED` + 写 `received_at` + 写 quote event `RECEIVED`） | 1 → 2 |

全程只会有 1 张 shipment（`uq_t_outsource_shipment_open_batch`），RECEIVED 事件只写
1 条。回归用例：`receive_from_outsource_partial_then_whole_closes_shipment`（正向）
+ `receive_from_outsource_partial_then_stale_version_conflicts`（复用旧 version → 409）。

> **`t_part` 派生状态**：部分接收后源批次 `OUTSOURCE`(rank 3) 与子批次
> `IN_PROCESS`(rank 2) 并存，min-progress 取 2 → **`t_part.status` 变 `IN_PROCESS`**，
> **尽管源批次还在外协厂**。这是三层派生契约（batch → part → assembly）的固有性质、
> 不是新 bug：前端工单列表会看到「在产」，而外协在途页仍有在途批次，两边都正确。
> 回归用例 `receive_from_outsource_partial_quantity_keeps_shipment_open` 断言该值。

### `POST /{batch_id}/receive-from-outsource-to-inspection`（`ReceiveFromOutsourceToInspectionRequest`）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC 锚 |
| `shelf_id` | string(i64) | ✓ | 目标货架，`zone` 必须 `INSPECTION` |
| `auto_pass_inspection` | bool? | — | 保留字段，当前 service 未消费 |
| `note` | string? | — | 备注 |

**整批**端点（无 `quantity`）。落 `INSPECTION` + `INSPECTION_SHELF`，同时清
`current_process_id` / `current_process_step_id`（出池），并把开口 shipment 标
`RECEIVED`。

`t_part_event.event_type` = `RECEIVED_TO_INSPECTION`（外协收回 → 直接进品检）。
**列宽是硬约束**：该列是 `varchar(30)`，字面量超 30 字符 → PG `22001` 使**整个
事务**回滚；本字面量 22 字符在限内。WS 事件名 `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED`
不在 payload 内、不受该列宽约束，逐字不变。

### 外协收发守卫一览

| 守卫 | 错误码 | 命中场景 |
|---|---|---|
| 角色 | 40300 | 非 Manager / Clerk / Inspector |
| OCC | 40901 | `version` 与批次行不符（含部分收发时拆批 OCC 失败） |
| 状态机 | 20103 | 源状态不在白名单（如已 `OUTSOURCE` 再 send） |
| 价来源 | 20104 | `direct` 与 `quote_id` 都给或都不给 |
| 数量 | 20104 | `quantity <= 0` / `> 批次量` |
| 工序类别 | 20104 | `process.category != 'OUTSOURCE'` |
| 公司↔工序映射 | 20104 | `t_outsource_company_process` 无该 (company, process) 未删行 |
| 公司在册 / 启用 | 21201 / 21205 | 公司不存在 / 已停用 |
| 工艺链 | 20706 | part 未绑定 `process_chain_id` |
| 重复开口 shipment | 21502 | 同一批次已有 `OUTSOURCING` shipment |
| 入参字段名 | 422（axum `Json` 提取器，**非业务信封**） | body 缺 `process_id`（send）/ `next_process_id`（receive），或字段名拼错被静默丢弃 |

### 已知不一致（2026-10-03 登记，未修）

1. **状态机缺 `IN_PROCESS → OUTSOURCE` 边**：`PartStatus::can_transition_to` 只放行
   `PENDING → OUTSOURCE`。故 `send_to_outsource` 实际只能从 `PENDING` 发起；service
   里「`IN_PROCESS` 必须在 `PRODUCTION_SHELF`」那段守恒在当前代码里不可达，端点注释
   与早期文档写的「`PENDING` 或 `IN_PROCESS+PRODUCTION_SHELF`」与实现不符。修法是给
   状态机补这条边（`src/modules/part/statemachine.rs`），属 part 域改动，不在本域范围。
2. **`t_part_event.event_type` 与 `backend-python` 词汇分叉**（2026-10-03 登记）：本仓
   直送品检事件用 `RECEIVED_TO_INSPECTION`（见上节），而
   `backend-python/model/enums.py` 仍定义 `RECEIVED_FROM_OUTSOURCE_INSPECTED`。两个
   后端共库，同一业务动作会按「谁服务的」产出两种 `event_type` 值。分叉保留（仓内零
   消费方、无历史行需要迁移）。
   **彻底解决需追加一条 append-only migration**：
   `ALTER TABLE t_part_event ALTER COLUMN event_type TYPE varchar(40);` —— 本轮不做
   （列宽是既有的全表约束，改它影响所有域的历史行与 Python 端写入路径）。
3. **`uq_t_part_batch_part_no` 的 `MAX(batch_no)+1` 竞态**（2026-10-03 登记）：`_split_batch_inner`
   先 `SELECT COALESCE(MAX(batch_no),0)+1` 再 INSERT，两条语句之间无锁。并发拆批
   （例如两个批次同时对外协做部分发送）会算出同一个 `batch_no` → 撞唯一约束 → 整事务
   **500** 而非 409。**只登记不修**：修法要么给拆批加 part 级 advisory lock、要么把
   `batch_no` 改成可重试分配，两条都会动到所有拆批调用方（`split_batch` /
   `split_batch_for_partial_pass` / pickup 路径），超出本域范围。

---

## 共同设计要点

### 货架解析（零 schema 变更）
`target_process_id` → service 查 `t_shelf_process WHERE process_id = $1 AND deleted_at IS NULL ORDER BY sort_order ASC, id ASC LIMIT 1` 解析货架。多结果取 `sort_order` 最小者；0 结果 → `20508 BIZ_SHELF_PROCESS_NOT_FOUND`。

### 状态机与事件
dispatch 路径：`PENDING → IN_PROCESS`，`location='PRODUCTION_SHELF'`，`current_holder_id=shelf_id`，**`current_process_id=target_process_id`**（2026-09-30 新增；工序候选池归属的权威依据），`current_process_step_id=NULL`（**有意的**：dispatch 路径不解析 step —— 无工序链时本就解析不出；该列已降级为**可选的显示用定位信息**，NULL 不影响入池）。同事务写 `t_part_event.kind='PLACED_ON_SHELF'`（from='PENDING', to='IN_PROCESS'）。

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

Response 200 `data`：[`DispatchResult`](#dispatchresult-字段2026-09-30-重构bulk-only-形态)

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

Response 200 `data`：[`AutoDispatchResult`](#autodispatchresult-字段2026-09-30-重构只读-preview)

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
  "current_process_step_id": null,    // Option<i64>，dispatch 路径不解析 step → null（有意；可选的显示用定位信息）
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
  - in-source 单测：`src/modules/prod/batch/service/dispatch.rs::tests` —— list_pending /
    dispatch_batch 成功路径 + 二次 dispatch 40903 / 不存在 batch_id 40404 /
    并发冲突 40901 / t_shelf_process 多结果取 LIMIT 1 / Inspector 角色 40300 /
    bulk_dispatch 全回滚 + 空 targets 422 / auto_dispatch 无 chain / 无 step /
    全部无 chain / 有 chain 成功首道 step.id

## 参考

- 模块 README：见 `src/modules/prod/batch/{mod.rs,model.rs,status_gate.rs,dto.rs,vo.rs,handler/,service/,repo/}`
- 错误码：`src/shared/error.rs::code`
- 前端模块文档：`frontend/docs/03-modules/production/README.md`
- 前端视图目录：`frontend/src/views/production/`
- 报工入口（part 域）：`docs/api/parts/`