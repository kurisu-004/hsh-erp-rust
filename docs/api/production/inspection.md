# prod::inspection 域 API —— 扫码查询（装配件 → 子件 → 批次 三层树）（2026-10-05 新增）

> 本文件须与 `src/modules/prod/inspection/{handler.rs,service.rs,repo.rs,vo.rs,model.rs,mod.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**单只读端点**，前端扫码弹窗的数据源。part 域既有
> `GET /api/v2/parts/by-serial/{serial_no}` 与
> `GET /api/v2/parts/by-serial/{serial_no}/part-batches` **一行未改**、保留兼容。

## 目录

- [端点列表](#端点列表)
- [为什么在 prod 域另起端点](#为什么在-prod-域另起端点)
- [GET /api/v2/prod/inspection/scan/{serial_no}](#get-apiv2prodinspectionscanserial_no)
- [两条必须记住的口径](#两条必须记住的口径)
- [`is_scanned` 的由来](#is_scanned-的由来)
- [版本号分工（前端最容易踩的一处）](#版本号分工前端最容易踩的一处)
- [软删闸门](#软删闸门)
- [响应 DTO](#响应-dto)
- [响应规模（无上限、无分页）](#响应规模无上限无分页)
- [关键错误码速查](#关键错误码速查)
- [实现位置](#实现位置)
- [维护约定](#维护约定)
- [实施状态](#实施状态)

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/inspection/scan/{serial_no}` | **Manager + Inspector** | 扫码查询：返回「装配件（可空）→ 全部子件 → 全部批次」三层树 |

> 路由挂载：`prod::mod::router().nest("/inspection", inspection::router())` —— 见
> `src/modules/prod/mod.rs`。

---

## 为什么在 prod 域另起端点

1. **part 域两个既有端点都表达不了这棵树**：`GET /parts/by-serial/{serial_no}` 返回
   28 列的 `PartDetailOut`（单 part 上下文，不展开子件、不返回装配件节点）；
   `GET /parts/by-serial/{serial_no}/part-batches` 返回单 part 的窄字段 + 全部活跃
   批次，同样没有装配件节点、没有兄弟子件。
2. **谓词不同**：本端点是「先 `t_part` 命中、未命中回退 `t_assembly`」的两表回退，
   既有端点都只有单表命中；且同号多行时按
   `ORDER BY (status = 'CANCELLED') ASC, id DESC LIMIT 1` 取值。
3. **字段集与形状都不同**：本端点是 9~11 列窄投影 + 两层 `children` 树（不是平铺
   列表）；给 part 域旧端点塞 `with_children` / `expand_assembly` 之类开关会把一个
   「详情型端点」变成「模式开关型端点」，两套口径挤在同一个出参里。
4. 与 2026-10-05 的 [`prod::process_design`](./process-design.md)、
   2026-10-01 的 [`prod::programming`](./pending-programming.md) 是同一类改动：
   **page 域从 part 域读一份谓词、字段都不同的窄投影**。

---

## `GET /api/v2/prod/inspection/scan/{serial_no}`

权限：**Manager + Inspector**（service 内 `require_any_role`）

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | ✓ | 扫码得到的序列号。**精确匹配**（不做前缀 / `ILIKE` 模糊 —— `F100` 命中 `F1001` 会弹错树，比不弹更糟）；service 内 `trim()`，纯空白按未命中收口 |

> ⚠️ **前端必须对 `serial_no` 做 `encodeURIComponent`**。序列号是业务侧自由文本，
> 含 URL 保留字符时行为分两种，**都不是 20101**：
> - 含 `/` → **未**编码时 axum 路由在 `/scan/{serial_no}` 这一段就把它当路径分隔符拆开，
>   匹配不到路由 → **HTTP 404 且响应体为空**：`v2_router()` 未挂 `.fallback(...)`，
>   handler 一行都不执行，**根本没有任何信封**可解析（前端 `res.json()` 直接抛解析
>   异常，**不是**「拿到一个 code ≠ 20101 的信封」）。**正确**编码成 `%2F` 时路由按单段
>   匹配、`Path` 解码回含 `/` 的序列号，正常走命中 / 20101 收口（回归测试：场景 13
>   同时钉死「编码后 200 命中」与「未编码 404 空 body」两种形态）；
> - 含 `?` / `#` → 未编码时在客户端或代理层被当成 query / fragment 起始符，序列号被
>   **截断**，表现为「扫到了但不对的码」。
>
> 后端**不做**解码侧兜底（不剥离非法字符、不做 `%` 还原）：无法区分「用户真敲了一
> 个 `%`」与「前端忘了编码」。`%20` 这类已编码的空白会被 Path 正常解码，再由 service
> `trim()` 收口（见集成测试场景 4）。

Query：**无**（本端点不接受任何 query 参数；分页 / 筛选一概没有 —— 体积口径见
[响应规模](#响应规模无上限无分页)）

Request body：**无**

Response 200 `data`：[`ScanTreeOut`](#scantreeout-字段)

业务流转：纯读端点 —— handler `pool.acquire()` **不开事务**、**不发** WS 广播（无业务
流转），一次请求 2~4 条 SQL，**无 N+1**。

### 命中口径（先 `t_part` 再 `t_assembly`）

1. `t_part.serial_no` 命中 → `hit_kind = "PART"`
2. 未命中再查 `t_assembly.serial_no` → 命中 → `hit_kind = "ASSEMBLY"`
3. 都未命中（含 `serial_no` trim 后为空）→ `20101 BIZ_PART_NOT_FOUND`（HTTP 404）

扫到**子件**时：`assembly` 有值、`children` 是该装配件的**全部**活跃子件；
扫到**装配件**时返回的是**同一棵树**（`children` 完全一致，仅 `hit_kind` 与
`is_scanned` 不同）。扫到**独立件**时：`assembly = null`、`children = [被扫中的那个]`。

### ⚠️ 同一 `serial_no` 可能并存多行：取哪一行是写死的口径

`t_part.uk_t_part_serial_no` 是**部分**唯一索引（谓词
`serial_no IS NOT NULL AND deleted_at IS NULL AND status <> 'CANCELLED'`），所以
「软删行 + `CANCELLED` 行」与活跃行可以同号共存。命中查询按
`ORDER BY (p.status = 'CANCELLED') ASC, p.id DESC LIMIT 1` 取值：
`CANCELLED` 行**排到最后**（扫到已废弃工单毫无意义，而工单被 `cancel` 后同号重建是
常规操作），剩余行由部分唯一索引保证至多一条。

`t_assembly.uk_t_assembly_serial_no` 是**全量**唯一索引（谓词
`deleted_at IS NULL AND serial_no IS NOT NULL`），活跃行必然唯一，故
`find_assembly_by_serial` 只需 `LIMIT 1`。

⚠️ **同号仅存 `CANCELLED` 行时，按正常命中返回一棵树**：`sort` 键只保证「活跃行
优先」，不保证「必有活跃行」。`POST /api/v2/parts/{id}/cancel` 把 part 打成
`CANCELLED` 后，rollup 的终态守卫会拦下 `release_part_serial_no`，序列号**保留**在
库中，同号重建前一直可扫。此时 `children[].status == "CANCELLED"` 原文透出，
`hit_kind` 仍是 `"PART"`、响应仍是 200 —— **前端必须自行禁用该节点上的写操作按钮**。
后端刻意**不加** `AND p.status <> 'CANCELLED'`：那会让已取消工单的货再也扫不到，
属产品决策（须用户拍板），不是实现细节。

### 父装配件已软删 → 退化成独立件树

`find_assembly_by_id` 返回 `None` **只可能是父装配件已软删**。此时响应退化成
`assembly = null` + `children = [被扫中的那个子件]`，而**不是**返回一棵「有子件但
没有装配件节点」的孤儿树。

> ⚠️ **前端无法从 payload 区分这一形态与「真独立件」**：`hit_kind = "PART"` +
> `assembly = null` + `children.len() == 1` 三者同时成立时，父装配件被软删的子件与
> 真正的独立件**逐字段同形**（后端不返回任何「父件已删」标记）。因此前端**不要**把
> `assembly = null` 直接渲染成「这是一个独立零件」——它也可能是「所属装配件已被
> 删除的子件」。若业务上必须提示用户，需另开端点或在 `t_part` 留「父件已删」标记列，
> 属独立需求。

### 空子件装配件

装配件无活跃子件时 `children` 是**空数组**（不是 `null`）—— 前端直接渲染「该装配件
没有子件」，省一层 `?? []` 判空。

---

## 两条必须记住的口径

### 1. `process_name` 对 `INSPECTION` / `DELIVERED` 批次恒为 `null`

这**不是 bug**，是「出池必须把 `current_process_id` 置 NULL」这条不变式的**正确**
结果：所有进 `INSPECTION` 的写点（`BatchService::scan_inspect` /
`BatchService::receive_from_outsource_to_inspection` /
`BatchService::complete_repair` / `mark_batch_inspected`）都按出池清该列；
`DELIVERED` 更进一步 —— 进 `READY_TO_SHIP` 的边只有 `INSPECTION → READY_TO_SHIP`，
故也必经 `INSPECTION`、同样恒 NULL。

**前端在这两个状态下不要渲染工序标签。**

取值列是 `current_process_id`（migration 004 确立的工序归属权威列），**不是**
`current_process_step_id`。取舍：后者只在首次定位工序时写、之后永不推进，多工序链工单
上会停在第一步，用它渲染「当前工序」会显示过时信息。其余展示类列表
（`GET /parts/{id}/batches`、待品检队列、返修列表）仍走 step 派生，**本端点是唯一
的有意例外**，已登记在 `src/modules/prod/batch/model.rs` 模块 doc 的「读取方分工」
清单第 4 条 —— 后端侧的后续改动请先对照那份清单，不要凭端点名想当然把它当缺陷「修」
回 step 派生。

> 回归测试：`tests/production/inspection.rs::process_name_is_null_for_out_of_pool_states`
> —— 同一条用例里既断言 `INSPECTION` / `READY_TO_SHIP` / `PENDING` 批次
> `process_name` 为 `null`，又断言 `IN_PROCESS` 批次取到真工序名，防止该组断言因
> 「恒为 null」而空过。

### 2. 本端点**读全部批次，不按状态过滤**

含 `COMPLETED` / `CANCELLED` 等终态批次，与 `GET /api/v2/parts/{id}/batches` 同口径
（该端点同样是「无 status 过滤」的展示类列表）。理由：扫码弹窗要回答「这批货总共分了
几批、每批现在什么状态」，砍掉终态就答不了；而**状态闸门在前端** —— 按
`ScanBatchOut.status` 决定「送检 / 指定工序」等按钮的显隐，后端不去重、不改写、不代做
决策。

> 回归测试：`tests/production/inspection.rs::all_batch_statuses_are_returned_including_terminal_ones`

---

## `is_scanned` 的由来

本端点**唯一**一处内存派生字段。`t_part_batch` **没有序列号列**，批次与扫码串之间没有
可 join 的关系，命中关系只能由 service 在内存里比对
`batch.part_id == 命中 part.id` 得出。

| 扫到的东西 | `is_scanned` |
|---|---|
| 独立件 | 该件的全部批次 `true`（树里也只有它） |
| 装配件子件 | **只有**被扫中那个子件的批次 `true`，兄弟子件全 `false` |
| 装配件条码 | `hit_kind = "ASSEMBLY"` 且无命中零件 → **全部批次 `false`**（前端此时可把整树当「已定位到装配件」整体高亮） |

---

## 版本号分工（前端最容易踩的一处）

| 字段 | 来源列 | 用途 |
|---|---|---|
| `ScanPartOut.version` | `t_part.version` | **仅展示**（工单级聚合投影，不参与任何写操作） |
| `ScanBatchOut.version` | `t_part_batch.version` | `POST /prod/batches/{batch_id}/to-ship` / `to-process` / `to-inspection` 的 **OCC 锚** |

批次 id + 批次 version 是一对，回传时**不许**拿零件 version 顶替。

> 回归测试：`tests/production/inspection.rs::batch_version_comes_from_part_batch_not_part`
> —— fixture 刻意让独立件 `t_part.version = 1` 而它的 INSPECTION 批次
> `t_part_batch.version = 3`，两处（独立件树 + 装配件树）各钉一遍。

---

## 软删闸门

part / assembly / batch 三处的软删行一律不返回（逐条 SQL 写死）：

| 表 | 闸门 |
|---|---|
| `t_part` | `p.deleted_at IS NULL`（命中查询 + 子件列表） |
| `t_assembly` | `a.deleted_at IS NULL`（按序列号 + 按 id 两条） |
| `t_part_batch` | `b.deleted_at IS NULL` |
| `t_customer`（附带） | `LEFT JOIN ... AND c.deleted_at IS NULL` —— 客户软删时客户名退化为 `null`，**不影响**该零件/批次返回 |

而 `LEFT JOIN` 进来的 `t_process` / `t_shelf` / `t_worker` / `t_outsource_company`
四张表**不加**软删闸门 —— 与 `prod::batch::repo` 的 `list_active_by_part_id_with_holder`
既有写法一致（工序名 / holder 名都是展示用附加信息，被软删也照常显示最后的样子）。
故上表不是「本端点读过的全部表」的清单。

⚠️ 软删闸门**不是**「保守过滤」而是本端点的语义闸门：扫到软删行等于扫到一个业务上
已不存在的码，前端据此弹「未找到」（404 + 20101）比弹一棵含已删数据的树更安全。

> 回归测试：`tests/production/inspection.rs::soft_deleted_child_and_batch_excluded`

---

## 响应 DTO

### `ScanTreeOut` 字段

```jsonc
{
  "hit_kind": "PART",          // "PART" | "ASSEMBLY"
  "scanned_serial_no": "SI-ASM-01", // 回显 trim 后的原始扫码串
  "assembly": { /* ScanAssemblyOut 或 null */ },
  "children": [ /* ScanPartOut[]，装配件树 = 全部子件；独立件树 = [被扫中的那个] */ ]
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `hit_kind` | string | 命中来源，**只有两个字面量**：`"PART"` = 扫到独立件或装配件子件；`"ASSEMBLY"` = 扫到装配件条码。前端 Zod 按 `z.enum(['ASSEMBLY','PART'])` 校验 |
| `scanned_serial_no` | string | 回显原始扫码串（**trim 后**的值），便于前端把扫码结果与历史记录对齐 |
| `assembly` | [`ScanAssemblyOut`](#scanassemblyout-字段)? | 装配件节点。仅 `hit_kind == "ASSEMBLY"` 或扫中的零件是某个装配件的子件时有值；独立件树 / 父装配件已软删时为 `null`。⚠️ **装配件节点没有批次**（`t_assembly` 在 `t_part_batch` 里没有行），前端不要在装配件层找「送检」动作的锚 |
| `children` | [`ScanPartOut`](#scanpartout-字段)[] | 顶层零件节点。**恒为数组**（空数组而非 `null`），按 `serial_no ASC NULLS LAST, id ASC` 排序（子件序列号由父件序列号派生 `{asm}-{i:02d}`，该序即业务装配序） |

### `ScanAssemblyOut` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | `serialize_i64` → JSON string。`t_assembly.id` |
| `serial_no` | string? | `varchar(15)` 可空（老数据可能没派发序列号）→ JSON `null` |
| `name` | string | `t_assembly.name` |
| `drawing_no` | string | `t_assembly.drawing_no` |
| `status` | string | `t_assembly.status` 原文（7 态，无 `OUTSOURCE`） |
| `quantity` | i32 | `t_assembly.quantity` |
| `is_urgent` | bool | 是否加急 |
| `system_delivery_date` | date? | 系统交期；可空 → JSON `null` |
| `customer_name` | string? | L2 叶子客户名（`LEFT JOIN t_customer`，客户软删 / 悬空 id → `null`） |

### `ScanPartOut` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | `serialize_i64` → JSON string。`t_part.id` |
| `serial_no` | string? | `varchar(15)` 可空（手工工单没序列号）→ JSON `null` |
| `name` | string | `t_part.name` |
| `drawing_no` | string | `t_part.drawing_no` |
| `status` | string | `t_part.status` 原文（8 态） |
| `quantity` | i32 | `t_part.quantity` |
| `is_urgent` | bool | 是否加急 |
| `system_delivery_date` | date? | 系统交期；可空 → JSON `null` |
| `customer_name` | string? | L2 叶子客户名 |
| `version` | i32 | `t_part.version` —— ⚠️ **仅展示**，不参与任何批次写操作的 OCC（见[版本号分工](#版本号分工前端最容易踩的一处)） |
| `children` | [`ScanBatchOut`](#scanbatchout-字段)[] | 该零件的**全部**批次（不按状态过滤），按 `batch_no ASC, id ASC` 排序 |

### `ScanBatchOut` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | `serialize_i64` → JSON string。`t_part_batch.id` —— 前端拿它当 `POST /api/v2/prod/batches/{batch_id}/to-ship\|to-process\|to-inspection` 的路径参数 |
| `batch_no` | i32 | 工单内批次序号（1 起） |
| `quantity` | i32 | 本批次数量 |
| `status` | string | `t_part_batch.status` 原文（8 态）。⚠️ **状态闸门由前端按本字段决定**：后端不过滤、不改写 |
| `version` | i32 | `t_part_batch.version` —— 前端作 OCC 锚回传。⚠️ **必须是批次版本**，不是 `t_part.version` |
| `is_repairing` | bool | 返修中标记；前端据此禁用「指定工序」（后端对返修中批次返 `20118`） |
| `location` | string? | 位置（`PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OFFICE` / `OUTSOURCE_COMPANY` …）；可空 → JSON `null` |
| `current_holder_display` | string? | 当前位置持有者名（`t_shelf` / `t_worker` / `t_outsource_company` 三表 `COALESCE`） |
| `process_name` | string? | 批次当前工序名（`current_process_id` → `t_process.name`）。⚠️ `INSPECTION` / `DELIVERED` 批次**恒为 `null`**（[见口径 1](#1-process_name-对-inspection--delivered-批次恒为-null)） |
| `is_scanned` | bool | 该批次所属零件**就是被扫中的那个** → 前端高亮（[由来见下](#is_scanned-的由来)） |

> **i64 → string 契约**：只有雪花 ID 字段（`assembly.id` / `children[].id` /
> `children[].children[].id`）序列化为 JSON **string**（`serialize_i64`）。`version` /
> `batch_no` / `quantity` 是版本号与计数，序列化为 JSON **number**，形态与
> `prod::programming` / part 域 `PartOut` 逐字一致。

### 响应示例

```json
{ "code": 0, "message": "ok", "data": {
  "hit_kind": "PART",
  "scanned_serial_no": "SI-ASM-01",
  "assembly": {
    "id": "9000000000000000278", "serial_no": "SI-ASM", "name": "SI assembly",
    "drawing_no": "D-SI-ASM", "status": "IN_PROCESS", "quantity": 1,
    "is_urgent": false, "system_delivery_date": null,
    "customer_name": "SI L1 customer"
  },
  "children": [
    { "id": "9000000000000000280", "serial_no": "SI-ASM-01", "name": "SI child 1",
      "drawing_no": "D-SI-02", "status": "INSPECTION", "quantity": 5,
      "is_urgent": false, "system_delivery_date": null,
      "customer_name": "SI L2 customer", "version": 4,
      "children": [
        { "id": "9000000000000000300", "batch_no": 1, "quantity": 5,
          "status": "INSPECTION", "version": 7, "is_repairing": false,
          "location": "INSPECTION_SHELF",
          "current_holder_display": "SI inspection shelf",
          "process_name": null, "is_scanned": true }
      ] },
    { "id": "9000000000000000281", "serial_no": "SI-ASM-02", "name": "SI child 2",
      "drawing_no": "D-SI-03", "status": "PENDING", "quantity": 5,
      "is_urgent": false, "system_delivery_date": null,
      "customer_name": "SI L2 customer", "version": 0, "children": [] },
    { "id": "9000000000000000282", "serial_no": "SI-ASM-03", "name": "SI child 3",
      "drawing_no": "D-SI-04", "status": "INSPECTION", "quantity": 5,
      "is_urgent": false, "system_delivery_date": null,
      "customer_name": "SI L2 customer", "version": 0, "children": [] }
  ] } }
```

> ⚠️ 示例里的 `version` 是刻意错开的两对：零件 `version: 4` vs 批次 `version: 7`。
> 前端调 `to-ship` / `to-process` / `to-inspection` 时回传的是**批次**的 7。
>
> ⚠️ 示例里 `assembly.system_delivery_date` 是 `null`：该列（`t_assembly` 的
> `system_delivery_date date`，可空无默认）在本文配套的 fixture 里**没写**，
> 故实际响应就是 `null` —— 与同示例里 3 个子件的 `system_delivery_date` 形态一致，
> 不是漏抄。

---

## 响应规模（无上限、无分页）

**本端点不接受任何 query 参数**（见端点下的「Query」行），`children` 与每个
`children[].children` 都是**无界数组**，后端不做任何条数截断：

| 数组 | 规模由什么决定 |
|---|---|
| `children` | 该装配件的**活跃子件数**（独立件树恒为 1） |
| `children[].children` | 该子件的**活跃批次数**（不按 status 过滤，含终态） |

⇒ 响应体规模 ≈ **子件数 × 每件批次数**，**无上限、无分页**。大装配件 + 多子件 + 多批次
时体积不受控。对比 `GET /api/v2/prod/batches/inspection` 那个队列端点带
`limit`（clamp 到 1~200），本端点**刻意没有** —— 扫码弹窗要一次答全「这批货总共分了
几批、哪些压在品检架上、每批能点什么动作」，截断答不全，分页则会让动作链多一次往返。

⚠️ **超大装配件的正解在前端**：本端点将来也**不会**变成分页端点（加 `limit` 属契约
变更，须前后端同一次改动）。届时应由前端对树做**虚拟表格 / 按需展开**（默认只渲染展开
路径上的节点），而不是等后端加参数。

---

## 关键错误码速查

| Code | Name | HTTP | 触发场景 |
|---|---|---|---|
| 20101 | BIZ_PART_NOT_FOUND | 404 | `t_part.serial_no` 与 `t_assembly.serial_no` **两表皆未命中**（含 `serial_no` trim 后为空、命中行已软删） |
| 40300 | FORBIDDEN | 403 | 角色守卫失败（非 Manager / Inspector） |
| 50001 | DATABASE | 500 | DB 查询失败（`code::DATABASE` / `AppError::Database`） |

**关于 20101**：本端点**复用** part 域的 `BIZ_PART_NOT_FOUND` 而不是另开错误码 ——
语义完全相同（扫到的东西不存在），前端按同一个 code 弹「未找到」即可。`message` 携带
原始（trim 后的）扫码串，模板是 `序列号 {serial_no} 未找到对应零件或装配件`。

> ⚠️ **纯空白串那条路径的 `{serial_no}` 是占位符 `(空)`**，不是空串 —— 直接插值会渲染
> 成「序列号␣␣未找到…」（双空格），故 service 传 `"(空)"`，message 原文是
> `序列号 (空) 未找到对应零件或装配件`。前端按 code 弹窗、别去解析 message。

**关于 40100**：未登录 / token 过期由中间件返回，见
[`../index.md`](../index.md#跨域错误码速查)。

**本端点不会产生 40001**：`serial_no` 是 `Path<String>`，非数字串不是错误（序列号本来
就是字符串），空串也走 20101 而非 400。

> 完整错误码见 [`../index.md`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实现位置

| 层 | 位置 |
|---|---|
| handler | `src/modules/prod/inspection/handler.rs::scan`（只做 `Path` 提取 + `pool.acquire()` + `R::ok`；角色守卫**不在**这里） |
| service | `src/modules/prod/inspection/service.rs::InspectionScanService::scan`（角色守卫 `SCAN_ROLES` + trim + 两表回退命中 + 内存分组挂树）；同文件的 `not_found` / `assembly_to_out` / `part_to_out` / `batch_to_out` 与私有 `HitKind` 枚举 |
| vo | `src/modules/prod/inspection/vo.rs`（`ScanTreeOut` / `ScanAssemblyOut` / `ScanPartOut` / `ScanBatchOut`） |
| repo | `src/modules/prod/inspection/repo.rs::InspectionScanRepo`（ZST + 5 个静态方法：`find_part_by_serial` / `find_assembly_by_serial` / `find_assembly_by_id` / `list_parts_by_assembly` / `list_batches_by_part_ids`） |
| model | `src/modules/prod/inspection/model.rs`（`ScanPartRow` / `ScanAssemblyRow` / `ScanBatchRow`，`query_as!` 宏的编译期校验对象） |
| 路由 | `src/modules/prod/mod.rs::router()` → `.nest("/inspection", inspection::router())`；子模块 router 在 `src/modules/prod/inspection/mod.rs::router()` |

### SQL 条数（单次请求，无 N+1）

| 扫到的东西 | 依次执行的 SQL | 条数 |
|---|---|---:|
| 独立件（`assembly_id IS NULL`） | `find_part_by_serial` → `list_batches_by_part_ids` | 2 |
| 装配件子件（父装配件活跃） | `find_part_by_serial` → `find_assembly_by_id` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 4 |
| 装配件子件（父装配件已软删） | `find_part_by_serial` → `find_assembly_by_id` → `list_batches_by_part_ids` | 3 |
| 装配件条码 | `find_assembly_by_serial` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 3 |
| 两表皆未命中 | `find_part_by_serial` → `find_assembly_by_serial` | 2 |

> ⚠️ 「装配件条码」那行在**装配件无活跃子件**（没有子件，或子件全被软删）时是
> **2 条**：`list_parts_by_assembly` 返回空 → `list_batches_by_part_ids` 对空切片
> 直接返空、不发 SQL。

子件再多也只有一条批次查询（`b.part_id = ANY($1)` 一次捞回整棵树的批次）。零件列表
为空时 `list_batches_by_part_ids` **直接返空 Vec 而不发 SQL**（`= ANY('{}')` 在 PG 里恒为
false、结果本就为空）。

### 事务 / WS / schema

- 纯读端点：handler `pool.acquire()` **不开事务**，**不发** WS 广播（无业务流转）。
- 零 schema 变更（无新 migration）。
- 本子模块**刻意没有 `dto.rs`**：无 query / body 入参（`serial_no` 走 path），没有可反
  序列化的入参结构，造一个空 DTO 模块只是噪音。
- ⚠️ **已知风险登记（不修）**：读端点不开事务意味着 2~4 条语句在 READ COMMITTED 下
  各看一个快照，理论上可撕裂（4 条之间被扫中的子件被软删 → 树里没有刚扫的码、全树
  `is_scanned = false`、**前端无任何错误提示**）。概率极低，且全仓读端点都是这个形态
  （读端点不开事务是 CLAUDE.md 的约定），消除它需给读端点开 REPEATABLE READ 快照事务，
  属跨域惯例改动。

---

## 维护约定

1. **`process_name` 的取值列不要改成 `current_process_step_id`**。step 指针只在首次
   定位工序时写、之后永不推进，多工序链工单上会停在第一步。代价（`INSPECTION` /
   `DELIVERED` 恒 `null`）是**接受**的，见[口径 1](#1-process_name-对-inspection--delivered-批次恒为-null)。
   本端点是「展示类列表一律走 step 派生」这条读取方分工的**唯一有意例外**，已登记在
   `src/modules/prod/batch/model.rs` 的读取方分工清单第 4 条 —— 不要凭那份清单把它
   当成越界顺手改回去。
2. **不要给批次层加 `status` 过滤**。状态闸门在前端，加了过滤后前端就答不了
   「这批货总共分了几批」，见[口径 2](#2-本端点读全部批次不按状态过滤)。
3. **`t_part` 的 11 列投影写了两份字面量**（`find_part_by_serial` 与
   `list_parts_by_assembly`）：`query_as!` 宏要求 SQL 是字面量（不能插 `const` / 拼
   `format!`），故只能各写一份。**两份的 `SELECT` 列表必须同步改** —— 字段集由
   `ScanPartRow` 唯一决定，宏编译期会校验两份都与该结构匹配。
4. **已知缺陷：holder 三表 `COALESCE` 的多态歧义**。`current_holder_display` 沿用
   `COALESCE(s.name, w.name, oc.name)`，该写法**假定** holder id 在
   `t_shelf` / `t_worker` / `t_outsource_company` 三表 PK 空间里互不重叠；一旦某 id
   同时命中其中两表，取到的是 `t_shelf.name`。本文件是该形态的全仓**第 6 处**（4 处
   `t_shelf.name` 形态含本文件 + 2 处 `t_shelf.code` 变体；6 处清单见
   `prod::batch::repo` 模块 doc 的同名小节）。本次**刻意不修**（会让 6 条 SQL 对部分
   历史脏数据的行为发生变化，且「先清洗还是先改判别式」需产品侧确认）；**将来要修必须
   6 处一起改**，逐处改会造成同一 holder 在不同端点显示不同名字。正确解法是
   `CASE location …`。
5. **角色白名单只有 2 个角色**（Manager / Inspector），与
   `GET /api/v2/prod/batches/inspection` 及 `to-ship` / `to-inspection` / `to-process`
   三个写端点同一组 —— 扫码树里的批次就是那三个写端点的操作对象，能看就必须能操作。
   加角色前先确认它同时能操作那些批次。
6. **`hit_kind` 的两个字面量只在 service 内的私有 `HitKind` 枚举构造**，出参仍是
   `String`（契约逐字要求，前端 Zod 用 `z.enum` 校验）。拼错字面量必须成为编译错误。
7. **不要给命中查询加 `AND p.status <> 'CANCELLED'`**。已取消工单的序列号按终态
   守卫会留在库里，加守卫等于让这批货再也扫不到，属**产品决策**（见端点下的「⚠️
   同一 `serial_no` 可能并存多行」节）；本端点按「同号仅存 `CANCELLED` 行也返回正常
   树、`status` 原文透出」处理，写操作门禁在前端。
8. **不要给本端点加 `limit` / 任何 query 参数**。响应规模 = 子件数 × 每件批次数，
   **无上限、无分页**是**刻意**的（见[响应规模](#响应规模无上限无分页)）：扫码弹窗要
   一次取全，截断答不全、分页多一次往返。规模过大时的正解是前端虚拟表格 / 按需展开，
   不是后端加参数 —— 真要加就是契约变更，须前后端同一次改动。

---

## 实施状态

- ✅ **`prod::inspection`**（2026-10-05 新增）：1 只读端点
  - 5 文件子模块（`mod/handler/service/repo/vo/model`），零 schema 变更
  - repo ZST + 5 个静态方法，全走 `sqlx::query_as!` 宏（编译期连库校验列名 / 列类型）
  - 角色守卫 Manager + Inspector（service 内 `require_any_role`）
  - **part 域 `GET /parts/by-serial/{serial_no}` 与 `/part-batches` 一行未改**（保留兼容）
- ✅ 集成测试：`tests/production/inspection.rs` —— **13 场景**
  （1 独立件树 + trim / 2 ★扫子件返回整棵装配件树 / 3 扫装配件条码 = 同一棵树 +
  `is_scanned` 全 false / 4 未命中与纯空白串 404 + 20101 / 5 软删子件与软删批次闸门 /
  6 `is_scanned` 只标被扫中那个 part / 7 八种批次状态全在（含终态）/ 8 批次 version
  来自 `t_part_batch` 而非 `t_part` / 9 i64 → JSON string / 10 角色守卫两放行三拒绝 /
  11 `process_name` 出池态恒 null + 生产中取真值 / 12 `is_repairing` 标记透传 /
  13 序列号含 `/`：`%2F` 编码后单段匹配 + 解码命中，未编码则 404 **空 body**）
- ✅ fixture：`test-support/fixtures/inspection.sql` + `test-support/src/fixture/inspection.rs`
  （ID 走 9_000_000_000_000_000_261+ 区段，与 `test-support/fixtures/` 下全部
  fixture 声明的 ID 段物理不相交；新增 fixture 前请核对该目录全部文件的头注释）

## 参考

- 模块 README：见 `src/modules/prod/inspection/{mod,handler,service,repo,vo,model}.rs`
- 保留兼容的旧端点（扫码上下文单 part 版）：[`../parts/inspection.md`](../parts/inspection.md#get-apiv2partsby-serialserial_nopart-batches)
- 待品检队列（本端点返回的批次通常从那里来）：[`../parts/inspection.md`](../parts/inspection.md#get-apiv2prodbatchesinspection)
- 同类先例：[`./process-design.md`](./process-design.md)（2026-10-05）、[`./pending-programming.md`](./pending-programming.md)（2026-10-01）
