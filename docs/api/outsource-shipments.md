# outsource-shipments 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,model.rs,repo/,service/}` 及
> `src/modules/outsource/vo/shipment.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：外协发货对账页（shipment 2 端点，2026-10-03 由 1 端点增为 2）。
> 2026-09-13 Phase 2 落地 reconcile-update；2026-10-03 补 in-flight 在途一览。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) /
> [`./outsource-quotes.md`](./outsource-quotes.md) / `outsource-sendable.md`
> （可发外协一览，独立顶层前缀 `GET /api/v2/outsource-sendable`；该文件与本域的
> `GET /in-flight` 由读侧分支在**同一次合并**中落地，单分支快照下不存在）。
> **shipment 的写入方不是本域**：`send-to-outsource` / `receive-from-outsource` 在
> prod 域批次侧（见 [`./production/batches.md#外协流转send--receive`](./production/batches.md#外协流转send--receive)），
> 本域只读 + 对账改。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-shipments/in-flight` | Manager / Clerk | 在途批次一览（`status='OUTSOURCING'` 的 shipment ⋈ 批次 ⋈ 工单） |
| POST | `/api/v2/outsource-shipments/{id}/reconcile-update` | Manager / Clerk | 对账页更新 shipment 行（OCC + 状态机守卫） |

> 路由注册：`in-flight` 是 1 段静态路径，与 2 段的 `/{id}/reconcile-update` 无
> matchit 冲突（见 `src/modules/outsource/handler.rs::shipment_router`）。

> ⚠️ **合并时点（2026-10-03 登记）**：本文件描述的 `GET /in-flight` 与写端点
> `POST /{id}/reconcile-update` **不在同一条开发分支上**，但在**同一次编排中先后合入
> master**，合并后本文件整体自洽。若只看其中任一分支的单分支快照，`in-flight` 都
> **不在册**（本仓 `shipment_router()` 只注册 `/{id}/reconcile-update`），此时该路径
> 404 —— 这是分支切分期的正常状态，不是契约缺失。**别据此撤掉本节**。
>
> 旧路径 `/api/v2/parts/outsource-in-flight` 已下线：不带 alias，且**返回 400 而非
> 404** —— part 域还有 `GET /parts/{part_id}` catch-all，matchit 静态段优先、参数段
> 兜底，于是 `outsource-in-flight` 被当作 `part_id` 交给雪花 ID 反序列化
> （`Path<i64>` 的 `ErrorKind::ParseError`）→ 400。400 比 404 更安全：404 无法区分
> 「端点被删」与「端点从未存在」，400 至少能指出「这里现在要一个雪花 ID」。

---

## 业务模型

- **发货表** `t_outsource_shipment`：与 `t_outsource_quote` 1:N（一份报价可多次发货）。
- `status` 取值只有两个（DB check `ck_t_outsource_shipment_status`）：
  `OUTSOURCING`（已发出、开口）/ `RECEIVED`（已整批收回）。第三值 `CANCELLED` 被
  check 允许但**当前无任何代码写入**。
- 时间列是 `sent_at`（发出，必填）/ `received_at`（**整批**收回时才写）。
- 唯一索引 `uq_t_outsource_shipment_open_batch (batch_id) WHERE deleted_at IS NULL
  AND status = 'OUTSOURCING'` —— **一个批次同时最多一张开口 shipment**。

### 部分接收的记账口径（2026-10-03 明确，有意为之）

`receive-from-outsource` 支持部分接收（`quantity`），口径是**只拆批次、不动
shipment**：

- shipment 行记的是**发出时**的全量（`send-to-outsource` 落 `quantity` = 本次发送量）。
- 部分回收 6 件（共发出 10 件）时：源批次 `quantity -= 6` 保留余量 4 件、**状态仍
  `OUTSOURCE`**（货还在外协厂），那张 shipment **仍 `OUTSOURCING`**、`received_at`
  仍 NULL；`received_at` / `status='RECEIVED'` 只在**整批**回收时才落。
- 因此对账列表里 `shipment.quantity` 与批次当前余量**可能不相等**。对账要回答的
  是「发出去多少、单价多少」，不是「现在库里还剩多少」；实际收回量以
  `t_part_event`（`RECEIVED_FROM_OUTSOURCE` 的 `quantity`）为准。
- 对应的 in-flight 列表里 `quantity` 取的是 **`t_part_batch.quantity`（当前余量）**，
  前端拿它做「部分接收」输入框的 max 值 —— 两者口径不同是刻意的。

---

## 共享错误码（215xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21501 | BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND | 404 | shipment 不存在 / 已软删 |
| 21502 | BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION | 400 | 同一批次已有开口 shipment（重复发送），或对账状态不在 `OUTSOURCING` / `RECEIVED` |
| 21503 | BIZ_OUTSOURCE_SHIPMENT_NO_OPEN | 404 | 找不到开口（`status='OUTSOURCING'`）的 shipment |
| 21504 | BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS | 400 | 本次接收数量超过开口 shipment.quantity |

> 21503 / 21504 目前**无代码路径触发**（口径改为部分接收不关 shipment 后，
> 「开口余量不足」不再可能发生）。保留登记以免后续实现重新用到时与本表冲突。

---

## 共享 DTO

### OutsourceShipmentOut 字段（`reconcile-update` 出参）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | shipment 雪花 ID |
| `version` | i32 | 乐观锁（对账行 OCC 锚） |
| `quote_id` | string (i64) | 关联报价（**NOT NULL**；DIRECT 占位报价也在这里） |
| `part_id` | string (i64) | 关联工单 |
| `batch_id` | string (i64)? | 关联批次；历史行可能为 null |
| `batch_no` | i32? | 批次号（`batch_id` 为 null 时为 null） |
| `outsource_company_id` | string (i64) | 外协公司 |
| `process_id` | string (i64) | 外协加工的工序 |
| `quantity` | i32 | **发出量**（见上方记账口径） |
| `unit_price` | string | Decimal 字符串，单件单价 |
| `status` | string | `"OUTSOURCING"` / `"RECEIVED"`（`"CANCELLED"` 允许但无写入方） |
| `sent_at` | naive datetime | 发出时间 |
| `received_at` | naive datetime? | 整批收回时间；部分回收时为 null |
| `is_billed` | bool | 是否已开票（对账页勾选） |
| `created_at` / `updated_at` | naive datetime | |
| `part_drawing_no` / `part_name` | string? | 工单显示字段 |
| `outsource_company_name` / `process_name` | string? | 公司 / 工序名 |
| `customer_path` | string? | 客户路径。2026-10-03 起与读侧同一次合并落地：`shipment_out` 改为真算（`part_customer_names` + `join_customer_path`，有 L1 拼 `L1 / L2`、否则仅 L2 名、缺客户为 `null`），不再硬编码 `None` |

### OutsourceInFlightItem 字段（`GET /in-flight` 单行）

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单 |
| `batch_id` | string (i64) | `t_part_batch.id` —— **必填**（驱动 SQL 是 INNER JOIN 主导，无批次的行不入结果），**部分接收端点的路径锚点** |
| `batch_no` | i32 | 批次号（必填，同上） |
| `quantity` | i32 | **`t_part_batch.quantity`（当前余量）**，不是 `shipment.quantity`；前端拿它做部分接收的 max |
| `serial_no` | string? | 工单序列号 |
| `drawing_no` / `name` | string? | 工单图号 / 名称 |
| `is_urgent` | bool | 加急标记 |
| `customer_path` | string? | 客户路径（有 L1 拼 `L1 / L2`，否则仅 L2 名，缺客户为 `null`） |
| `next_process_id` | string (i64)? | 外协加工的工序（= `shipment.process_id`） |
| `next_process_name` | string? | 工序名 |
| `outsource_company_id` | string (i64) | 外协公司（必填） |
| `outsource_company_name` | string? | 公司名 |
| `sent_at` | naive datetime | 发出时间（必填） |
| `version` | i32 | **`t_part_batch.version`（不是 shipment.version）** —— `receive-from-outsource` 的 OCC 锚 |


### OutsourceInFlightListOut 字段

`items: OutsourceInFlightItem[]` + `total: i64` + `limit: i64` + `offset: i64`。

### OutsourceInFlightListQuery（GET query）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keyword` | string? | — | part 的 `drawing_no` / `name` ILIKE 模糊匹配 |
| `limit` | i64? | — | 分页 |
| `offset` | i64? | — | 分页 |

### OutsourceShipmentReconcileUpdateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC（shipment 行 version）；不匹配 → 40901 |
| `unit_price` | string? | — | Decimal 字符串，覆盖单件单价 |
| `quantity` | i32? | — | 覆盖发出量；`<= 0` → 400 `20104 BIZ_INVALID_VALUE` |
| `is_billed` | bool? | — | 是否已开票 |

> 本 DTO **只有**上面 4 个字段。尤其**不含** `status` / `received_at` / `note`：
> shipment 的状态与收货时间只能由外协回收侧（`receive-from-outsource` /
> `receive-from-outsource-to-inspection`）在**整批**回收时改，对账端点不参与状态机，
> 备注也没有落库列可写。

---

## 端点契约要点

### `GET /api/v2/outsource-shipments/in-flight`

权限：**Manager / Clerk**（service 层 `require_any_role`，与被它取代的 part 域旧
`/parts/outsource-in-flight` 一致）

1. 只读端点：`pool.acquire()` 不开事务，无 WS 广播。
2. 驱动 SQL 是 **INNER JOIN 主导**：`t_outsource_shipment` ⋈ `t_part_batch` ⋈
   `t_part`，判据 `s.deleted_at IS NULL AND s.status = 'OUTSOURCING'`。批次 / 工单
   被软删的行不出现在结果里（`version` / `quantity` 必须取自批次行，缺批次时该语义
   无从谈起）。
3. 排序 `ORDER BY s.sent_at DESC, pb.id DESC`；一次 `list` + 一次 `count`（同 WHERE），
   无 N+1（公司名 / 工序名 / 客户路径在 list SQL 内 `LEFT JOIN` 解析）。
4. `limit` 缺省取域默认（缺省页长），并 `clamp(1, 上限)`；`offset` 缺省 0 且
   `max(0)`。

### `POST /api/v2/outsource-shipments/{id}/reconcile-update`

权限：**Manager / Clerk**（service 层 `require_any_role`）

1. 解析 `id` 雪花 ID + `version` OCC 锚点
2. 校验 shipment 存在（未软删）→ 21501
3. `version` 不符 → 40901
4. 状态机守卫：仅 `OUTSOURCING` / `RECEIVED` 可编辑；否则 400
   `21302 BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION`
5. `quantity <= 0` → 400 `20104 BIZ_INVALID_VALUE`
6. OCC UPDATE（带 version + deleted_at IS NULL）；命中 0 行 → 40901
7. 返回最新 `OutsourceShipmentOut`

WS 广播：本域**无**独立事件（对账是后台核对动作，不驱动前端实时视图）。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901。

### 事务边界

- reconcile-update：handler 层开 tx → 传 `&mut tx` 给 service → 显式 `tx.commit()`；失败时 `Transaction::drop` 自动回滚。
- in-flight：只读，不开事务。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_in_flight` / `reconcile_update_shipment` + `shipment_router()`（`list_in_flight` 与读侧同一次合并落地）
- service：`src/modules/outsource/service/shipment.rs::OutsourceService::{list_in_flight, reconcile_update_shipment}`（+ 文件内私有 `shipment_out` 拼装）
- repo：`src/modules/outsource/repo/sql.rs::OutsourceShipmentRepo`（trait 声明在 `repo/mod.rs::OutsourceRepoTrait`）
- dto：`src/modules/outsource/dto.rs::{OutsourceShipmentReconcileUpdateRequest, OutsourceInFlightListQuery}`
- model：`src/modules/outsource/model.rs::TOutsourceShipment`
- vo：`src/modules/outsource/vo/shipment.rs::{OutsourceShipmentOut, OutsourceInFlightItem, OutsourceInFlightListOut}`
- 写入侧（send / receive）：`src/modules/prod/batch/service/outsource.rs`
- 路由挂载：`/outsource-shipments`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

- `tests/outsource/send_receive.rs`（写入侧 + 对账：整批 / 部分收发、shipment
  `OUTSOURCING → RECEIVED` 记账、部分接收不动 shipment、部分接收 → 整批回收余量
  才关 shipment、reconcile OCC 40901）
- `tests/outsource/shipment.rs`（in-flight / sent-parts 读侧；**读侧分支文件**，与
  `GET /in-flight` 同一次合并落地，单分支快照下不存在）
