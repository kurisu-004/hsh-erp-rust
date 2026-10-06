# inspection 域 API（待品检队列 + 扫码查询）

> 域：`prod::inspection`（嵌套域，源码在 `src/modules/prod/inspection/`）。本文件是该域的**唯一**契约来源，任何字段 / 端点变更必须同步本文件。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/prod/inspection/queue` | Manager + Inspector | `InspectionQueueQuery`（见 §3.1） | `InspectionQueueListOut` |
| 2 | GET | `/api/v2/prod/inspection/scan/{serial_no}` | Manager + Inspector | 路径参数 `serial_no`（`Path<String>`） | `ScanTreeOut` |

路由注册链：`src/modules/mod.rs` 的 `/api/v2` nest → `src/modules/prod/mod.rs` 的 `.nest("/inspection", inspection::router())` → 本域 `mod.rs` 的 `/queue` + `/scan/{serial_no}`。

- 两个端点返回统一信封 `R { code, message, data }`。
- 两个端点都**纯读**：`pool.acquire()` 不开事务，**不发** WS 广播。
- ⚠️ 角色守卫**下沉在 service 第一行**（`require_any_role(READ_ROLES)`），handler 不重复校验。`READ_ROLES` 是 `service.rs` 里的 `[Role::Manager, Role::Inspector]`，**两个 service 共用同一常量**，改白名单只改这一处。放行 `Clerk` / `CncProgrammer` / `ShelfAccount` 会让它们看到本该看不到的批次明细。
- ⚠️ 两条路由段数不同（`/queue` 1 段静态、`/scan/{serial_no}` 2 段），matchit 无同段位争用，注册顺序无关。
- ⚠️ 端点 2 的 `serial_no` 是 `Path<String>` 而非数值提取器：序列号是字符串且可能含 `-`（子件 `{asm}-{i:02d}`）。**前端必须 `encodeURIComponent`** —— 含 `/` 未编码时 axum 路由在该段就把它当路径分隔符拆开 ⇒ 匹配不到路由 ⇒ **HTTP 404 且响应体为空**（本仓 `v2_router()` 未挂 `.fallback(...)`，拿不到任何信封，前端 `res.json()` 直接抛解析异常）。含 `?` / `#` 未编码则在客户端或代理层被截断。后端**不做**解码侧兜底（无法区分「用户真敲了一个 `%`」与「前端忘了编码」）。

## 2. 扫码树 `ScanTreeOut`（端点 2）

### 2.1 三层结构

```text
ScanTreeOut
├─ hit_kind / scanned_serial_no      命中来源与原始扫码串
├─ assembly: ScanAssemblyOut | null  装配件节点（**没有批次**）
└─ children: ScanPartOut[]           顶层零件节点
   └─ children: ScanBatchOut[]         该零件的全部批次
```

- `children` 恒为**数组**（不返 `null`）：装配件无活跃子件时是空数组，前端直接渲染「该装配件没有子件」。
- ⚠️ 装配件节点**没有批次**：`t_assembly` 在 `t_part_batch` 里没有行，批次只挂在 `ScanPartOut::children` 上。前端不要在装配件层找「送检」动作的锚。
- ⚠️ `assembly` 非空 **≠** `hit_kind == "ASSEMBLY"`：扫到**子件**时 `assembly` 同样有值。前端分形态时两个信息都要看。

### 2.2 `ScanTreeOut` 逐字段

| 字段 | 类型 | 来源 / 口径 |
|---|---|---|
| `hit_kind` | string | `service.rs` 私有 `HitKind` 枚举收敛成两个字面量：`"ASSEMBLY"`（扫到装配件条码）/ `"PART"`（扫到独立件或装配件子件条码）。出参是 `String` 不是枚举（契约如此），前端按 `z.enum(['ASSEMBLY','PART'])` 校验 |
| `scanned_serial_no` | string | 回显**trim 后**的原始扫码串，便于前端把扫码结果与历史记录对齐 |
| `assembly` | object \| null | 仅 `hit_kind == "ASSEMBLY"` 或扫中的零件是某个装配件的子件时有值；独立件树为 `null` |
| `children` | array | 装配件树 = 该装配件的**全部**子件（不止被扫中的那个）；独立件树 = `[被扫中的那个 part]` |

### 2.3 `ScanAssemblyOut` 逐字段（9）

| 字段 | 类型 | SQL 来源 |
|---|---|---|
| `id` | string | `a.id`（雪花 ID 字符串化） |
| `serial_no` | string \| null | `a.serial_no` |
| `name` | string | `a.name` |
| `drawing_no` | string | `a.drawing_no` |
| `status` | string | `a.status` 原文（`t_assembly` 7 态） |
| `quantity` | number | `a.quantity` |
| `is_urgent` | boolean | `a.is_urgent` |
| `system_delivery_date` | string \| null | `a.system_delivery_date` |
| `customer_name` | string \| null | `LEFT JOIN t_customer c ON c.id = a.customer_id AND c.deleted_at IS NULL`（**带**软删闸门，客户被软删时退化为 `null`，不影响节点返回） |

### 2.4 `ScanPartOut` 逐字段（11）

| 字段 | 类型 | SQL 来源 |
|---|---|---|
| `id` | string | `p.id` |
| `serial_no` | string \| null | `p.serial_no` |
| `name` | string | `p.name` |
| `drawing_no` | string | `p.drawing_no` |
| `status` | string | `p.status` 原文（`t_part` 8 态） |
| `quantity` | number | `p.quantity` |
| `is_urgent` | boolean | `p.is_urgent` |
| `system_delivery_date` | string \| null | `p.system_delivery_date` |
| `customer_name` | string \| null | 同上（带 `c.deleted_at IS NULL`） |
| `version` | number | `p.version` ⚠️ **仅展示**，任何批次写动作的 OCC 锚是 `ScanBatchOut::version` |
| `children[]` | array | `ScanBatchOut` |

### 2.5 `ScanBatchOut` 逐字段（10）—— 本端点唯一的写操作锚

| 字段 | 类型 | SQL 来源 / 口径 |
|---|---|---|
| `id` | string | `b.id` —— 前端拿它当 `POST /api/v2/prod/batches/{batch_id}/to-ship\|to-process\|to-inspection` 的路径参数 |
| `batch_no` | number | `b.batch_no` |
| `quantity` | number | `b.quantity`（**批次量**） |
| `status` | string | `b.status` 原文（8 态）。⚠️ **状态闸门由前端按本字段决定**：后端不过滤、不改写 |
| `version` | number | `b.version` —— 前端作 OCC 锚回传。⚠️ 必须是**批次**版本，不是 `t_part.version` |
| `is_repairing` | boolean | `b.is_repairing`；前端据此禁用「指定工序」（后端对返修中批次返 **20118**） |
| `location` | string \| null | `b.location`（`PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OFFICE` …） |
| `current_holder_display` | string \| null | `COALESCE(s.name, w.name, oc.name)`（`t_shelf` / `t_worker` / `t_outsource_company` 三表各 LEFT JOIN 一次） |
| `process_name` | string \| null | `LEFT JOIN t_process pr ON pr.id = b.current_process_id`。⚠️ 对 `INSPECTION` / `DELIVERED` 批次**恒为 `null`**（见 §2.7） |
| `is_scanned` | boolean | 本端点**唯一**内存派生字段（见 §2.6） |

⚠️ **`current_holder_display` 的多态歧义**：`COALESCE(s.name, w.name, oc.name)` 假定 holder id 在三表 PK 空间里互不重叠；一旦某 id 同时命中其中两表，取到的是 `t_shelf.name`。完整说明（全仓 6 处清单 + 正确解法 `CASE location …`）见 `src/modules/prod/batch/repo/mod.rs` 模块 doc（⚠️ `prod::batch::repo` 是**目录**，模块 doc 在 `mod.rs` 而非 `repo.rs`）；**本文件是该形态的全仓第 6 处**。刻意不修：修一处会让 6 条 SQL 对部分历史脏数据的行为发生变化，且「先清脏数据还是先改判别式」需产品侧确认；**修时必须 6 处一起改**。

### 2.6 命中口径与 `is_scanned`

序列号在**两张表都有值域**（子件 `{asm}-{i:02d}` 与父件 `{prefix}{4 位}`），故固定「先 part 后 assembly」：

1. `t_part.serial_no` 命中 → `hit_kind = "PART"`
2. 未命中再查 `t_assembly.serial_no` → 命中 → `hit_kind = "ASSEMBLY"`
3. 都未命中 → **20101**（HTTP 404）；**trim 后为空串**同样按未命中收口

扫到**子件**时：父装配件活跃 → 返回整棵装配件树；父装配件已软删（取不到）→ 退化成独立件树（`assembly = null` + `children = [被扫中的那个]`），不返回「有子件但没有装配件节点」的孤儿树。

扫到**装配件**时返回的是**同一棵树**（`hit_kind` 不同，`children` 完全一致）。

`is_scanned` 的由来：`t_part_batch` **没有序列号列**，批次与扫码串之间没有可 join 的关系，命中关系只能由 service 在内存里比对 `batch.part_id == 命中 part.id` 得出。结果：装配件树里**只有**被扫中那个子件的批次为 `true`；扫装配件条码时无命中零件，故全部批次 `is_scanned = false`。

### 2.7 ⚠️ 两条必须记住的口径

**1. `process_name` 对 `INSPECTION` / `DELIVERED` 批次恒为 `null` —— 这不是 bug。** 它是「出池必须把 `current_process_id` 置 NULL」这条不变式的**正确**结果：`DELIVERED` 更进一步 —— 进 `READY_TO_SHIP` 的边只有 `INSPECTION → READY_TO_SHIP`，故也必经 `INSPECTION`、同样恒 NULL。前端在这两个状态下**不要**渲染工序标签。

取值列是 `current_process_id`（migration 004 确立的工序归属权威列），**不是** `current_process_step_id`：后者只在首次定位工序时写、之后永不推进，多工序链工单上会停在第一步。其余展示类列表仍走 step 派生 —— ⚠️ 本端点是那条分工的**唯一有意例外**，登记在 `src/modules/prod/batch/model.rs` 模块 doc 的读取方分工清单里。

**2. 本端点读全部批次，不按状态过滤。** 含 `COMPLETED` / `CANCELLED` 等终态，与 `GET /api/v2/parts/{id}/batches` 同口径。理由：扫码弹窗要回答「这批货总共分了几批、每批现在什么状态」，砍掉终态就答不了；**状态闸门在前端**。

### 2.8 单次请求的 SQL 条数（无 N+1）

| 扫到的东西 | 依次执行的 SQL | 条数 |
|---|---|---:|
| 独立件（`assembly_id IS NULL`） | `find_part_by_serial` → `list_batches_by_part_ids` | 2 |
| 装配件子件（父装配件活跃） | `find_part_by_serial` → `find_assembly_by_id` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 4 |
| 装配件子件（父装配件已软删） | `find_part_by_serial` → `find_assembly_by_id` → `list_batches_by_part_ids` | 3 |
| 装配件条码 | `find_assembly_by_serial` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 3 |
| 两表皆未命中 | `find_part_by_serial` → `find_assembly_by_serial` | 2 |

子件再多也只有一条批次查询（`part_id = ANY($1)` 一次捞回整棵树的批次层）。装配件条码在「装配件无活跃子件」时是 2 条：`list_parts_by_assembly` 返回空 → `list_batches_by_part_ids` 对空切片**提前返空、不发 SQL**。

## 3. 待品检队列（端点 1）

### 3.1 `InspectionQueueQuery` 逐参数（Query string）

| 参数 | 类型 | 语义 | 缺省 | 非法值处理 |
|---|---|---|---|---|
| `drawing_no` | string | `p.drawing_no ILIKE '%…%'` | 不过滤 | 含 `%` / `_` / `\` → **40001**（HTTP 422）；空白串按不过滤 |
| `name` | string | `p.name ILIKE '%…%'` | 不过滤 | 同上 |
| `serial_no` | string | `p.serial_no ILIKE '%…%'`（⚠️ 本端点是**模糊**，与 `prod::programming` 的精确 `serial_no` 不同） | 不过滤 | 同上 |
| `customer_id` | i64 | 客户筛选，单值 | 不过滤 | 客户不存在（含软删）→ **20102**（HTTP 404）；非数字字面量 → HTTP 400 纯文本；⚠️ 空串同样 → HTTP 400 纯文本（同一机制，见 §4.3） |
| `system_delivery_date_from` | `YYYY-MM-DD` | 系统交期下界（**含**） | 不过滤 | 格式非法 → HTTP 400 纯文本（`chrono::NaiveDate` 反序列化失败） |
| `system_delivery_date_to` | `YYYY-MM-DD` | 系统交期上界（**含**） | 不过滤 | 同上 |
| `sort_by` | string | 排序列白名单，7 键（见 §5.2） | 系统交期 | **不报错**，静默退化到 `p.system_delivery_date` |
| `sort_dir` | string | `ASC` / `DESC` | `ASC` | **不报错**，非 `DESC`（忽略大小写）一律按 `ASC` |
| `limit` | i64 | 每页行数 | **200** | clamp 到 `[1, 200]`；非数字字面量 → HTTP 400 纯文本；⚠️ **空串 / 全空白 → HTTP 400 纯文本**（⚠️ 与 `prod::programming` 相反：那边走私有 `deserialize_i64_opt_lenient` 兜成缺省） |
| `offset` | i64 | 偏移 | `0` | 负数 `max(0)`；非数字字面量 → HTTP 400 纯文本；⚠️ **空串 / 全空白 → HTTP 400 纯文本**（⚠️ 与 `prod::programming` 相反：那边走私有 `deserialize_i64_opt_lenient` 兜成缺省） |

⚠️ 本端点**不接** `statuses` 参数：判据写死 `pb.status = 'INSPECTION'`。要按其它状态筛请走 `GET /api/v2/prod/batches/repair` / `/repairing`（属 `prod::batch` 域，28 字段宽 VO）。

⚠️ 日期区间筛的是**系统交期**（页面已不显示计划交期）。与 `prod::programming` 的 `sort_by` 白名单里**有** `REQUEST_DATE` / `PLANNED_DELIVERY_DATE` 不同，本域没有这两列可排。

⚠️ **三个 i64 入参的空串一律 400，不是按缺省**（`customer_id` / `limit` / `offset`，2026-10-07 review 第 1 轮订正原「空串按缺省」的错述）。链路：`dto.rs` 三个字段都标 `deserialize_with = "deserialize_i64_opt"`，指向 `shared::types::deserialize_i64_opt` —— 它对 `Some(str)` 只有一条 `str.parse::<i64>()`，**既不 trim 也不放行空串**。而 `serde_urlencoded 0.7.1` 的 `deserialize_option` 无条件 `visit_some`，`?limit=` 拿到的是 `Some("")` 而非 `None` ⇒ `"".parse::<i64>()` 失败 ⇒ axum `Query` extractor 拒绝 ⇒ **HTTP 400 纯文本（无 `R` 信封）**。连带后果：`limit=%2050`（数字两侧带空白）同样 400。

⇒ ⚠️ 同一组「筛选框清空态」在前端必须**按域分别处理**：待编程页三个参数都能发空串（本域的宽松版 helper 兜着），待品检页三个参数**发空串就 400**。前端清空筛选时应**省略该 query key**（不带 `=`），不要发空值。

### 3.2 `InspectionQueueListOut` 逐字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[]` | array | `InspectionQueueItemOut`，最多 `limit` 行（§3.3） |
| `total` | **string** | 过滤后总行数，**不受 `items` 截断影响** |
| `limit` | **string** | 生效的 limit（clamp 后回显） |
| `offset` | **string** | 生效的 offset |

⚠️⚠️ **分页三件套是 JSON string 不是 number**（走 `shared::types::serialize_i64`，雪花 ID 字符串化的连带）。这是雪花 ID 字符串化的连带后果：整个 `InspectionQueueListOut` 的 i64 字段都带 `serialize_with`。消费方必须在边界 `Number(resp.total)` 转 number 才能塞进分页组件。

⚠️ **与 `prod::programming` 的分页计数方向相反**（那边是裸 i64 ⇒ number）。两个前端 schema 千万别互相照抄：前端 `inspectionQueueListResultSchema` 声明 `z.string()`，`pendingProgrammingListResultSchema` 声明 `z.number()`。

### 3.3 `InspectionQueueItemOut` 逐字段（13 个）

| # | 字段 | 类型 | SQL 来源 | 口径 |
|---|---|---|---|---|
| 1 | `batch_id` | string | `pb.id` | 雪花 ID 字符串化；三个写端点的路径参数 + 扫码选行标识 |
| 2 | `batch_no` | number | `pb.batch_no` | 批次列 |
| 3 | `quantity` | number | `pb.quantity` | 数量列 + 部分通过弹窗上限（`POST /prod/batches/{batch_id}/to-ship` 的 `quantity` 不得超过本值） |
| 4 | `version` | number | `pb.version` | OCC 锚 `t_part_batch.version`（**不是** `t_part.version`） |
| 5 | `part_id` | string | `p.id` | 雪花 ID 字符串化；详情页 `/parts/{part_id}` |
| 6 | `serial_no` | string \| null | `p.serial_no` | 序列号列（手工工单可空） |
| 7 | `drawing_no` | string | `p.drawing_no` | 图号列 |
| 8 | `name` | string | `p.name` | 名称列 |
| 9 | `system_delivery_date` | string \| null | `p.system_delivery_date` | 系统交期列，可空 → JSON `null` |
| 10 | `is_urgent` | boolean | `p.is_urgent` | 加急红底 |
| 11 | `customer_id` | string | `p.customer_id` | 雪花 ID 字符串化；客户表头筛选的入参回显（caller 选中 L1 / L2 都用它） |
| 12 | `customer_name` | string \| null | `c.name` | **L2** 叶子客户名 |
| 13 | `l1_customer_name` | string \| null | 派生（见 §3.5） | **L1** 一级集团名 |

字段集严格对齐前端 7 个数据列（序列号 / 图号 / 名称 / 批次 / 数量 / 系统交期 / 客户）+ 操作列所需的锚点（`batch_id` / `version` / `part_id` / `is_urgent` / `customer_id`）。

⚠️ **本 VO 与返修 VO 不共用**：`GET /api/v2/prod/batches/repair` / `/repairing` 继续用 `prod::batch::vo::InspectionBatchListItemOut`（28 字段）。共用会让那 15 个字段在待品检页成为无用负载。两端点的行对象**不可互相 cast**。

### 3.4 队列口径：WHERE 五段拼装

`repo.rs` 私有 `push_inspection_queue_where` —— list 与 count **共用**，天然杜绝「count 与 items 各说各话」的分页 bug。逐段：

1. **状态 + 双软删闸门**：`pb.status = 'INSPECTION' AND pb.deleted_at IS NULL AND p.deleted_at IS NULL`（固定，不接受入参）
2. **客户**：`cardinality($ids::bigint[]) = 0 OR p.customer_id = ANY($ids)` —— 空数组命中全部；非空时限定到展开后的 L1+L2 ids（同一数组绑两次）
3. **表头 3 个文本列各一个独立 ILIKE**：`$n::text IS NULL OR <col> ILIKE $n`（短路 ⇒ 缺省不过滤）
4. **系统交期区间**：两个可空边界各自 `$n::date IS NULL OR …`，缺界不参与过滤
5. （仅 list）`ORDER BY {order_col} {order_dir} NULLS LAST, pb.id ASC LIMIT / OFFSET`

JOIN 只有 3 张：`t_part_batch` JOIN `t_part` JOIN `t_customer`（L2）+ `t_customer` 自连（L1）。⚠️ **不** JOIN `t_shelf` / `t_worker` / `t_outsource_company` / `t_process` / `t_process_chain_step` / `t_delivery_note` —— 待品检页不渲染那些列。

⚠️ 排序**必须**带 `NULLS LAST`：`p.system_delivery_date` 可空，而 PG 默认 ASC → `NULLS LAST` / DESC → `NULLS FIRST`，不显式指定时按交期倒序会把未填交期的行顶到最前。`pb.id ASC` 是兜底键（排序列可重复，无兜底键时翻页会漏行 / 重复行），覆盖用例 `tests/part/inspection_batches.rs::inspection_batches_pagination_tiebreak_by_batch_id_is_stable`。

⚠️ `customer_id` 的 L1/L2 展开走 `crate::shared::customer::expand_customer_id`：L1 客户 → 自身 + 全部 L2 子节点 ids；L2 客户 → 自身 + 同 L1 下所有兄弟 L2 ids。该函数在 `shared`（公共设施，**不是**域），故本域引用它**不**违反零跨域依赖。

### 3.5 ⚠️ `l1_customer_name` 的派生口径（两处并存，勿统一）

本端点（`repo.rs`）的口径：

- `c.parent_id IS NOT NULL` → `pc.name.or(c.name)`：pc 的 LEFT JOIN **不带** `deleted_at` 过滤，父行在即取父名，父行悬空才回落 `c.name`
- 否则（自身即 L1）→ `c.name`

⚠️ **与 `prod::batch::service::repair::list_batches_matching` 的同名派生口径不同**：返修侧那条 SQL 里 `l1_customer_name` 就是 `c_l1.name` 裸值 —— **不回落** `c.name`（客户自身即 L1、或父客户被软删时为 `null`），且它的 JOIN **带** `c_l1.deleted_at IS NULL`。两条 SQL 的分叉是**有意的**（队列侧要「L1 名恒尽量非空」的展示体验，返修侧要「客户被软删就不该假装它还有父集团」的严格性），但 ⚠️ **目前只有本域侧写了登记**（`repo.rs` 模块 doc），返修侧**没有对应注释**。

⇒ 这条不对称本身是已登记的债：改本域这一处前先去看 `src/modules/prod/batch/service/repair.rs` 的 SQL 是否仍是分叉；要统一就得两侧一起改，并给返修侧补上登记。

## 4. 口径表

### 4.1 行单位差异（跨端对数前必读）

| 用途 | 行单位 | 说明 |
|---|---|---|
| **端点 1 队列** | **批次级** | 一个工单拆了 N 个 INSPECTION 批次 ⇒ N 行 |
| **端点 2 扫码树** | **零件级 + 批次级两层** | `children` 是零件节点，每个零件的 `children` 才是批次；装配件节点**无批次** |
| `prod::programming` | **工单级** | 一个 part 恒 1 行 |
| `dashboard::in_inspection_count` | **批次级**（`COUNT(*)`） | 行单位与端点 1 同，但 ⚠️ 判据**多一条** holder 闸门，见下 |

⚠️ 端点 1 与端点 2 虽读同一批数据（`status='INSPECTION'` 的活跃批次），但**行数不可直接比**：端点 2 还会带出非 INSPECTION 状态的批次（§2.7 第 2 条：不按状态过滤），且以零件为中间层折叠。

⚠️ 端点 1 与 `dashboard::in_inspection_count` 的**判据也不完全相同**：后者（`dashboard` 域 `repo/sql.rs::count_inspection_batches`）除 `status='INSPECTION'` + 双软删闸门外还带 `current_holder_id IN (品检区 active 货架)`，端点 1 **没有**这条。于是「holder 已出池但状态还停在 INSPECTION」的批次两边口径会分叉（dashboard 侧已剔除、本端点侧仍列出）。这是两边各自服务目的不同导致的**有意差异**，不是缺陷 —— 详见 `docs/api/dashboard.md` §2。

### 4.2 ⚠️ 排序白名单：与 `part_sql.rs` 的 `id DESC` 不同（有意分叉）

`service.rs` 的 `resolve_order_col`（映射表**放 service 层**，repo 收不到任何外部输入）：

| `sort_by` | ORDER BY 列 |
|---|---|
| `SERIAL_NO` | `p.serial_no` |
| `DRAWING_NO` | `p.drawing_no` |
| `NAME` | `p.name` |
| `BATCH_NO` | `pb.batch_no` |
| `QUANTITY` | `pb.quantity` |
| `CUSTOMER_NAME` | `c.name` |
| `SYSTEM_DELIVERY_DATE` | `p.system_delivery_date` |
| **缺省 / 非法值** | `p.system_delivery_date` |

与前端表头 7 列一一对应。

⚠️ **与 `part::repo::sql::part_sql::list_with_filters` 的分叉**：那边缺省是 `id` + `DESC`（最近建的排前），方向规则还**相反**（只认 `ASC`，其余 → `DESC`），白名单放 **repo** 内、8 键（含 `SYSTEM_DELIVERY_DATE`）。**不要**把两者「统一」：零件一览语义是「最新在前」，待品检页语义是「最急交期在前」，缺省本就不同。

大小写不对称与 `prod::programming` 相同：`resolve_order_col` **只认全大写 token**（小写静默退化到缺省列、不报错），`resolve_order_dir` 用 `eq_ignore_ascii_case`。**前端传参一律用全大写枚举。**

排序白名单与 ILIKE 拒绝规则锁在 `src/modules/prod/inspection/service.rs` 的 `order_col_whitelist_maps_and_degrades` / `order_dir_only_accepts_desc` / `ilike_pat_rejects_wildcards_and_blanks_out_empty` 三个单测里。

### 4.3 ⚠️ `to_ilike_pat` 对 `%` / `_` / `\` 的**拒绝**（语义约束，防全表扫描）

3 个表头筛选值经 `service.rs::to_ilike_pat`：trim → 空串按不过滤 → **含 `%` / `_` / `\` 直接返 40001** → 否则拼 `%…%`。

- ⚠️ 拒绝是**语义**约束，不是注入防护：注入面由 repo 侧 `push_bind` 参数化保证。拒它的理由是 `%…%` 会被 PG 当通配符放大 —— 表头筛选框只输一个 `%` 就能把整张表捞出来，1 次请求退化成全表 ILIKE 扫描。
- ⚠️ **与 `prod::programming` 的做法相反**：那边是**转义**通配符（`escape_like` + `ESCAPE '\'`，`keyword=50%` 命中字面量含 `50%` 的行），这边是**拒绝**（40001）。两边都有各自的理由，**不要互相统一**。
- ⚠️ **另一条与 `prod::programming` 相反的分叉：分页 i64 入参的空串容错**（2026-10-07 review 第 1 轮登记，详见 §3.1）。⚠️ 本条的 3 个 ILIKE 文本筛选**有** trim（`to_ilike_pat` 第一步就 trim、空串按不过滤），但 `customer_id` / `limit` / `offset` 走的是 `shared::types::deserialize_i64_opt`，**没有** trim 也没有空串兜底 ⇒ 空串 400。`prod::programming` 三个对应参数（`limit` / `offset` / `has_cnc_program`）全走它自己那个私有宽松版 helper（`deserialize_i64_opt_lenient` / `deserialize_bool_opt`，两者都先 trim 再兜空串）⇒ 那边空串是**缺省**。⇒ **不要**因为「本域 3 个文本筛选对空串宽容」就推断分页参数也宽容，两组参数的宽容度在同域内都不一致。

## 5. 错误码表

| 码 | HTTP | 端点 | 触发条件 | 出处 |
|---|---|---|---|---|
| `40300` `FORBIDDEN` | 403 | 两者 | 角色不在 `{Manager, Inspector}` 内 | `CurrentUser::require_any_role` |
| `40100` `UNAUTHORIZED` | 401 | 两者 | 缺 / 坏 Bearer token、签名失败、claims 不合规 | `src/auth/middleware.rs` + `src/auth/extractor.rs` |
| `40102` `TOKEN_EXPIRED` | 401 | 两者 | JWT `ExpiredSignature` | `src/auth/middleware.rs` |
| `40105` `SESSION_REVOKED` | 401 | 两者 | Redis session 查不到 / jti 命中吊销黑名单 | `src/auth/middleware.rs` |
| `20101` `BIZ_PART_NOT_FOUND` | 404 | 扫码 | 序列号在 `t_part` 与 `t_assembly` 皆未命中；trim 后为空串同样按未命中 | `service.rs::not_found`（复用 part 域同码，不另开槽位） |
| `20102` `BIZ_CUSTOMER_NOT_FOUND` | 404 | 队列 | `customer_id` 指向不存在或已软删的客户 | `shared::customer::expand_customer_id` |
| `40001` `VALIDATION_ERROR` | 422 | 队列 | 3 个表头筛选值含 `%` / `_` / `\` | `service.rs::to_ilike_pat` → `AppError::validation` |
| `20118` `BIZ_PART_REPAIR_NOT_TRIGGERED` | 400 | **本域不产生**，登记备查：`ScanBatchOut::is_repairing` 为 `true` 的批次若被调 `POST /prod/batches/{batch_id}/to-process` 会撞这个码（语义是「返修流转前置条件不满足」，文案提示改用 `complete-repair`）。前端据此用 `is_repairing` 提前禁用「指定工序」按钮 | `prod::batch` 的 `transition_core` |
| **HTTP 400 纯文本**（无 `R` 信封） | 400 | 队列 | `customer_id` / `limit` / `offset` 非数字字面量；`system_delivery_date_from` / `_to` 格式非法 | axum `Query` 提取器反序列化失败，**不经 `AppError`** |
| **HTTP 404 空响应体**（无 `R` 信封） | 404 | 扫码 | `serial_no` 含 `/` 且前端未 `encodeURIComponent` ⇒ 路由不匹配，`v2_router()` 无 `.fallback` | axum 路由层（§1） |
| `40800` `REQUEST_TIMEOUT` | 408 | 两者 | 请求级超时中间件到点（全域，非本域特有） | `src/middleware/timeout.rs` |
| `500` | 500 | 两者 | SQL 失败（`sqlx::Error` → `AppError` 映射） | `InspectionScanRepo` / `InspectionQueueRepo` |

⚠️ 扫码端点的 `serial_no` 参数**不做解码侧兜底**（不剥离非法字符、不做 `%` 转义还原），因为无法区分「用户真敲了一个 `%`」与「前端忘了编码」。

## 6. 移除记录 / 路由变更记录（2026-10-07）

### 6.1 ⚠️ 破坏性路由变更：队列读从 `prod::batch` 迁入本域

| | 旧 | 新 |
|---|---|---|
| 路径 | `GET /api/v2/prod/batches/inspection` | `GET /api/v2/prod/inspection/queue` |
| 状态 | **已下线，无 alias（旧路径 404）** | 现行 |
| 出参 | `InspectionQueueListOut` | `InspectionQueueListOut` **逐字未变**（字段名、`items[*]` 恰好 13 个 key、计数为 JSON **string** 而非 number） |
| 入参 | `InspectionQueueQuery` | `InspectionQueueQuery` **逐字未变**（9 个 query 参数） |

因为出参逐字未变，**前端 Zod schema 无需改动**。迁入范围：SQL 落到 `repo.rs` 的 `InspectionQueueRepo`（与 `InspectionScanRepo` 并列的第二个 ZST）+ 私有 `push_inspection_queue_where` + 常量 `INSPECTION_QUEUE_SELECT` / `INSPECTION_QUEUE_COUNT_FROM`；行结构 `InspectionQueueRow` 落到 `model.rs`；VO 落到 `vo.rs`；DTO `InspectionQueueQuery` 落到 `dto.rs`；service `InspectionQueueService` + 共用的 `READ_ROLES` 落到 `service.rs`；handler `queue` 落到 `handler.rs`。

⇒ 迁后本域**零跨域依赖**，这是本次迁移的主要收益。

⚠️ 前端配套改动在**前端仓**（`~/Code/hsh-erp/frontend`）单独提交 —— 后端那次提交**不含**任何前端文件。落点共 3 类：

1. **1 处 URL 字面量**：队列读的 api 封装 `listInspectionBatches` 与其行 / 入参类型并入既有的 `src/api/inspection.ts`（该模块此前已承载扫码端点），URL 字面量 `/prod/batches/inspection` → `/prod/inspection/queue`
2. **1 处单测路径断言**：`src/api/parts/__tests__/routes.spec.ts` 里那条 `expect(...).toBe('/prod/batches/inspection')` 迁到 `src/api/__tests__/inspection.contract.spec.ts`（该 spec 里扫码端点 URL 是独立的 Q4 断言）
3. **6 个文件的注释引用旧路径**：`src/composables/queries/{keys,schemas}.ts` / `src/types/inspection.ts` / `src/views/inspection/inspectionColumnDefs.ts` / `src/views/inspection/composables/{inspectionSchema,useInspectionQueueQuery}.ts` —— 纯文案、不影响行为

后端侧的集成测试文件仍在 `tests/part/inspection_batches.rs`（文件位置未迁，断言逐字未改，只把请求路径换成新路径）。

### 6.2 其它已下线项（登记以防重建）

- `part` 域的 `GET /api/v2/parts/by-serial/{serial_no}` 与 `GET /api/v2/parts/by-serial/{serial_no}/part-batches`：**part 域一行未改**，旧端点保留兼容。两者都无法表达「装配件 + 全部子件 + 全部批次」这棵树（前者不展开子件、后者不返回装配件节点），故另起本域端点而非给 part 域塞开关。前端待品检页扫码路径**建议**切到本域端点（尚未在前端仓合入）。

## 7. 与 WS 的关系

- **两个端点都不发 WS 广播**：纯查询，无业务流转。handler 只 `pool.acquire()`，不开事务。
- 队列读的数据新鲜度靠两件事：① 页面自己写成功后显式 `invalidateQueries({ queryKey: qk.inspectionPrefix })`；② 可选 300s `refetchInterval`（页面「自动刷新」开关）。该 query **不设** `staleTime` / `gcTime`，走 `frontend/src/main.ts` 的全局默认（TanStack Query 库默认 `staleTime: 0` / `gcTime: 5min`；`main.ts` 只覆盖 `retry: 0` + `refetchOnWindowFocus: false`）。跨页面 / 他人写的操作**不做精确失效**。
- ⚠️ 后端 `WsEvent` 现在只有 `DashboardEvent { kind, payload }` 一个变体，且 **`kind` 是裸 `String`、无枚举保护**（`src/infra/ws_hub.rs`）。

### 7.1 会改变本域返回的 WS `kind`

| `kind` | 对队列读的影响 |
|---|---|
| `BATCH_TO_INSPECTION` / `PART_TO_INSPECTION` | 批次进 `INSPECTION` ⇒ **新行出现**（端点 1 唯一的「增长」来源之一） |
| `PART_SCAN_INSPECT_PASSED` / `PART_SCAN_INSPECT_FAILED` | 品检流转 ⇒ 批次离开 `INSPECTION`，行消失 |
| `PART_TO_SHIP` / `BATCH_TO_SHIP` | 交付流转 ⇒ 可能离开 `INSPECTION` |
| `PART_BATCH_SPLIT` / `PART_BATCH_CANCELLED` | 拆批 / 批次软删 ⇒ 行集合变化 |
| `PART_SOFT_DELETED` / `PART_CANCELLED` | `p.deleted_at` 或状态出闸门 ⇒ 行消失 |
| `PART_REPAIR_STARTED` / `PART_REPAIR_DISPATCHED` / `PART_REPAIR_COMPLETED` | 返修流转 ⇒ 批次状态变化 |

对扫码树（端点 2）的影响面更大：它**读全部批次不按状态过滤**（§2.7 第 2 条），所以任何改批次状态 / 拆批 / holder / 工序的事件都可能改变树内容。

⚠️ **与前端白名单是人工同步关系（无编译期保障）**：前端 `useDashboardInvalidation.ts` 的 `AFFECTS_DASHBOARD` 是 **dashboard 域**的失效白名单，本域页面**不订阅** WS 事件。⚠️ 而上表里的 `PART_SCAN_INSPECT_PASSED` / `PART_SCAN_INSPECT_FAILED` / `PART_REPAIR_*` / `PART_CANCELLED` **并不全在** `frontend/src/types/dashboard.ts` 的 `DashboardEventType` 联合类型里（`PART_SCAN_INSPECT_*` 在，`PART_REPAIR_*` 与 `PART_CANCELLED` 不在），遑论进白名单。任一侧新增 `kind` 不会让另一侧编译失败，只会让「列表不动」这类症状极难定位。

## 8. 表依赖与前端配套

### 8.1 读的表

端点 1（队列读）：`t_part_batch` / `t_part` / `t_customer`（L2 + 自连 L1）。

端点 2（扫码读）：`t_part` / `t_assembly` / `t_part_batch` / `t_customer`，另 `LEFT JOIN` `t_process` / `t_shelf` / `t_worker` / `t_outsource_company` 五表。

⚠️ **两张表的软删口径不一样，不要混**：

| 端点 | 软删闸门 | 说明 |
|---|---|---|
| 端点 2 | `t_part` / `t_assembly` / `t_part_batch` **三处必过滤**；`t_customer` 带闸门（客户名退化为 `null`，不影响节点返回）；`t_process` / `t_shelf` / `t_worker` / `t_outsource_company` **刻意不加** | 软删闸门在这里是**语义**闸门不是保守过滤：扫到软删行等于扫到一个业务上已不存在的码，前端弹「未找到」比弹一棵含已删数据的树更安全 |
| 端点 1 | `pb.deleted_at IS NULL` + `p.deleted_at IS NULL`；**两条 `t_customer` JOIN 都不过滤 `deleted_at`** | 与端点 2 的 `t_customer` 闸门**方向相反**，同域内两个端点口径不同，改任一处的客户 JOIN 前先确认不会破坏另一处 |

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 路径 | 备注 |
|---|---|---|
| queryKey 工厂 | `src/composables/queries/keys.ts` 的 `qk.inspectionQueueList(params)` / `qk.inspectionPrefix` | 前缀失效用 `inspectionPrefix` |
| Zod 守门 schema | `src/views/inspection/composables/inspectionSchema.ts` | `inspectionQueueListItemSchema`（**13 个 key + `.strict()`**，多一个键即抛 `unrecognized_keys`）+ `inspectionQueueListResultSchema`（计数 `z.string()`）+ 扫码树 schema |
| query hook | `src/views/inspection/composables/useInspectionQueueQuery.ts` | hook 无自有状态，入参 `MaybeRefOrGetter`（params + `enabled` + `autoRefresh`） |
| 页面级 store | `src/views/inspection/composables/useInspectionListStore.ts` | 分页 / 筛选 / 弹窗态 + 写 mutation + `invalidateInspectionQuery()` |
| 列定义 | `src/views/inspection/inspectionColumnDefs.ts` | 表头 7 列 |
| api 层 | `src/api/inspection.ts` | 队列读 `listInspectionBatches` 与扫码端点**同模块** |

⚠️ 队列读行 schema 用 `.strict()`（13 键锁死），扫码树 schema 用普通 strip 模式 `z.object` —— 两侧宽容度不同，加字段时注意各自的行为差异。

### 8.3 写端点（**全部在 `prod::batch` 域**，不在本域文档里）

| 用途 | 端点 |
|---|---|
| 送检 | `POST /api/v2/prod/batches/{batch_id}/to-inspection` |
| 品检通过（部分通过） | `POST /api/v2/prod/batches/{batch_id}/to-ship` |
| 指定工序 | `POST /api/v2/prod/batches/{batch_id}/to-process` |
| 扫码流转 | `POST /api/v2/prod/batches/{batch_id}/scan-inspect` |

本域只提供**读**上下文；所有写动作由 `prod::batch` 的 handler / service 执行并开事务、发 WS 广播。

### 8.4 已知偏差登记

1. **`l1_customer_name` 与返修侧口径分叉，且只有本侧有登记**（§3.5）。产品决议（2026-10-07）：**不处理**（两条分叉各自的服务目的不同），但已把「返修侧缺登记」登记为债 —— 改任一侧前必须核对另一侧。
2. **同域内 `t_customer` 软删口径不一致**（端点 2 带闸门、端点 1 不过滤，见 §8.1）。这是迁域时「SQL 与派生口径逐字未改」的必然结果：队列读的 JOIN 是从 `prod::batch` 原样搬过来的。产品决议（2026-10-07）：**不处理**，端点 1 的「不过滤」与 `prod::programming` 一致（历史工单要显示原客户名）。
3. **`current_holder_display` 的 holder 三表 COALESCE 多态歧义**（§2.5，本形态全仓第 6 处）。产品决议（2026-10-07）：**不处理**（需先确认「清脏数据」还是「改判别式」，且修必须 6 处同批）。
4. **读端点不开事务 ⇒ 装配件分支的 2~4 条语句不保证同一快照**（READ COMMITTED 下每条语句各看一个快照）。理论上的可撕裂场景：4 条语句之间被扫中的子件被软删 ⇒ `children` 里没有自己刚扫的码、全树 `is_scanned = false`，**无任何错误提示**。发生概率极低，且全仓读端点都是这个形态（读端点不开事务是本仓约定），故不改；真要消除只能给读端点开 REPEATABLE READ 快照事务，属跨域惯例改动。
5. **⚠️ `customer_id` / `limit` / `offset` 的空串容错与 `prod::programming` 相反，且无测试钉住**（§3.1 / §4.3）。本域三个字段复用 `shared::types::deserialize_i64_opt`（不 trim、不放行空串），`prod::programming` 三个字段走**该域**私有的宽松版 helper（trim + 空串按缺省）—— 后者正是当初被 `?limit=` 的 400 逼出来的（见 `src/modules/prod/programming/dto.rs` 模块 doc）。⚠️ `tests/part/inspection_batches.rs` **无空串用例** ⇒ 这条宽容度分叉在 CI 上没有任何保护，后人「顺手对齐」或「顺手收紧」都不会被测试拦住。产品决议（2026-10-07）：**不处理**（本域前端目前不发空串）；⚠️ 改任一侧的 i64 反序列化前必须先看另一侧，并补一条空串用例。

## 9. 域隔离

本域**零跨域依赖**：`src/shared/domain_guard` 把「本域不 import 其它域的 service / repo」从口头约定变成 CI 强制。单测 `src/modules/prod/inspection/mod.rs` 的 `inspection_domain_depends_on_no_other_domain` 传本域路径 `prod::inspection` + 源码目录，扫全部 `.rs`，代码区里出现任何他域路径即 panic。⚠️ 嵌套域走**前缀匹配**而非「首段相同即本域」—— 否则同父兄弟域 `prod::batch` 会被一起放行，护栏等于没装。

本域对 `crate::shared::customer::expand_customer_id` 的引用**不**违规：`shared` 是公共设施不是域。

### 9.1 ⚠️ 已知漏报盲区（诚实登记，来源：`src/shared/domain_guard` 的「已知漏报盲区」节）

探测器的触发点是**根段字面量本身**（`crate::modules::`），下列形态读不出根段或读不出连续标识符段，故扫不到：

1. **段与段之间插空白**：`crate::<域根> :: part::…`（语法上完全合法）漏过。
2. **不带根段字面量的裸路径引用**：`<域根>::<兄弟域>::service::…` / `as` 改名后的别名 / 域容器别名 / `macro_rules!` 展开目标。
   - ⚠️ **「相对 `super::` 链」是对本域风险最高的一条**：嵌套域的兄弟域就是 `../batch`，于是 `use super::super::batch::service::BatchService;`（写在域内子模块文件里）/ `use super::batch::…`（写在域 `mod.rs` 里）/ `use self::super::…` 三种相对写法都是合法的跨域引用，却一个根段字面量都没有 ⇒ **全部不命中**（实测对本域扫这三个字符串，违规列表均为空）。内联的 `super::batch::service::X` 表达式同理。
   - **只登记不修**：域间引用在本仓一律走 `crate::<域根>::` 全限定路径，现无此写法；要覆盖它得让探测器解析模块树、把 `super::` 链按目录层级折叠回绝对路径，成本远超它在本仓的暴露面。
3. **`use crate::<域根>;` 本身**（含 `as mods;`）：末尾没有 `::`，读不到段就当没看见。（同一容器写成 glob 反而会被抓到。）
4. **未闭合块注释**会让文件剩余全部内容被当注释跳过，其后的真实跨域 import 漏过。

⚠️ 另有两条**只会误报、不会漏报**（方向上安全）的精度边界：跨行 raw string / 普通字符串字面量正文里的外来域路径会被当代码报出来；字符字面量里的 `"`（`let c = '"';`）会让同行 `//` 之后的内容被误判为非注释。

⇒ **要人工复核的收敛面**：本域源码里任何 `super::` 形式的跨模块引用（含 `use` 与内联表达式）都不是护栏能覆盖的。改本域时对 `super::` 保持人工敏感。