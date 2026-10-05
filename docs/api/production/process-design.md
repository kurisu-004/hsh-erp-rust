# prod::process_design 域 API —— 制定工序页零件列表（2026-10-05 新增）

> 本文件须与 `src/modules/prod/process_design/{handler.rs,dto.rs,service.rs,repo.rs,vo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**「制定工序」页**的单一只读列表端点。前端该页从 part 域
> `GET /api/v2/parts?status=PENDING&limit=200` 切到本端点（2026-10-05）；part 域旧端点
> **保留兼容、一行未改**。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/process-design/parts` | **Manager+Clerk+Inspector+CNC_PROGRAMMER** | 待制定工序的零件列表（**含装配件子件**） |

> 路由挂载：`prod::mod::router().nest("/process-design", process_design::router())` —— 见 `src/modules/prod/mod.rs`。

---

## 为什么不在 part 域改

1. **part 域旧端点的守卫把子零件排除了**：`GET /api/v2/parts` 在 service 层硬置
   `part_only: true`（`part/repo/sql/part_sql.rs::list_with_filters` 的固定
   `part_only` 传参），repo 据此在 SQL 里追加 `AND assembly_id IS NULL`，把**装配件的
   子零件全部排除**在结果集外（`t_part.assembly_id` 是子件指向父装配件的逻辑 FK）。
   该守卫在 part 域是对的 —— 那个页面的 ALL / PART 两种模式里，装配件子件由 service 层
   内存合并、不该独立成行 —— 但本页需要「所有还没定工序的零件」，子件当然也在内。
2. **谓词不同**：本页的闸门是 `deleted_at IS NULL AND status = 'PENDING'`，与
   `GET /parts` 的几十个可选筛选（`status` / `keyword` / `customer_ids` / 日期区间 /
   `order_no` …）没有交集。往旧端点塞 `row_type` / `include_assemblies` 之类开关，会把
   一个「筛选型列表」变成「模式开关型列表」，两套口径挤在同一个出参里。
3. **字段集不同**：本页只需 7 个字段的最小集，而 `PartOut` 有 20 余个（客户 / 交期 /
   数量 / 价格 / 状态 / `is_urgent` …）。
4. 与 2026-10-01 的 [`prod::programming`](./pending-programming.md) 是同一类改动：
   **page 域从 part 域 `t_part` 读一份谓词、字段都不同的窄列表**。

---

### `GET /api/v2/prod/process-design/parts`

权限：**Manager + Clerk + Inspector + CNC_PROGRAMMER**（service 内守卫）

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sort_dir` | string? | ✗ | `ASC` / `DESC`（缺省 `ASC`；非 `DESC` 一律按 `ASC` 处理，大小写不敏感）。排序键固定 `serial_no`，**无 `sort_by`** |
| `limit` | int? | ✗ | 缺省 200；service 层 `clamp(1, 500)`。值以 URL query 形态到达（`?limit=200`），**带引号的 `"200"` 不接受**（400）；**空串 / 全空白按缺省处理** |
| `offset` | int? | ✗ | 缺省 0；service 层 `max(0)`。取值容错同 `limit` |

> **刻意不提供的参数**（与 [`pending-programming.md`](./pending-programming.md) 的
> 筛选型端点不同，本端点入参极窄）：
> - `status` —— `PENDING` 是本页的**业务闸门**（只有未开工的零件才需要制定工序），不是筛选
>   旋钮，故写死在 SQL 常量里；开放成参数会让前端拼出「待制定工序页 + 查已完成零件」这种
>   自相矛盾的请求，且前端无法在客户端二次过滤。
> - `keyword` —— 前端本地过滤，后端不接收。
> - `sort_by` —— 排序键固定 `serial_no`，没有第二个可选列。
> - `row_type` / `include_assemblies` —— **本端点存在的意义就是没有那道
>   `AND assembly_id IS NULL` 守卫**，把「要不要子件」做成开关等于把守卫换个地方藏。

> **`limit` / `offset` 的取值容错**：query string 无类型之分，数字一律以字符串到达，
> 统一 `parse` 成 `i64`；**数字两侧的空白会被 trim**（`?limit=%20200%20` → `200`）；
> **空串 / 全空白按缺省处理**（`?limit=&offset=` → `200` / `0`，返回 200）；非数字
> （`abc`）/ 小数（`50.5`）/ 溢出 / 带引号的字面量仍 → 400。

Response 200 `data`：[`ProcessDesignPartListOut`](#processdesignpartlistout-字段)

业务流转：

1. 角色守卫：Manager + Clerk + Inspector + CNC_PROGRAMMER
2. limit / offset 边界 clamp（`limit=0 → 1`，`limit=99999 → 500`，`offset=-5 → 0`）
3. 两条 SQL（`ProcessDesignRepo::list` / `ProcessDesignRepo::count`）共用 `FROM_SQL` 与
   `WHERE_SKELETON` 两个常量 —— list / count 的谓词**只此一份**，杜绝「改 list 漏
   count」导致 `total` 与 `items` 对不上

---

## ⚠️ 本端点刻意**不加** `AND assembly_id IS NULL` 守卫

```sql
SELECT p.id, p.version, p.serial_no, p.name, p.drawing_no,
       p.process_chain_id, p.assembly_id
FROM t_part p
WHERE p.deleted_at IS NULL
  AND p.status = 'PENDING'
ORDER BY p.serial_no {ASC|DESC} NULLS LAST, p.id DESC
LIMIT $limit OFFSET $offset
```

**这是本端点存在的全部理由。** 加回那道守卫，装配件的子零件就会重新从「制定工序」页
消失 —— 而子件同样需要定工序（页内选零件 → 建工艺链 → 下发）。

**后人不要"好心"把它加回去。** 若将来发现结果集混进了不该出现的行，请先确认那是「装配件
主表行」还是「子件行」：**子件行 `assembly_id` 有值，是本页的正常成员**。

> 回归测试：`tests/production/process_design.rs::assembly_child_parts_are_visible`
> —— 该用例除了断言「子件可见」，还**反向断言 part 域 `GET /parts` 确实看不到子件**，
> 把两者的口径差异钉成事实，避免后人误以为两者等价。

`count` 是同谓词的 `SELECT COUNT(*)::bigint`（见下）。

---

## 过滤谓词（软删闸门 + PENDING 状态闸门）

两段，全静态，无动态段：

| 谓词 | 说明 |
|---|---|
| `p.deleted_at IS NULL` | 软删闸门（全仓硬要求） |
| `p.status = 'PENDING'` | 业务闸门，写死在 SQL 常量里，**不暴露成 query 参数** |

`IN_PROCESS` / `PROGRAMMING` / `COMPLETED` / `CANCELLED` 的零件**一律不出现** —— 它们要么
已经开工（工序在流转中），要么已经终结（无需再定工序）。

### ⚠️ 排序是**字典序**不是数值序（**不是缺陷**）

`serial_no` 是 `varchar(15)`，排序按字符比较：`F1001-10` 排在 `F1001-2` **前面**（第 7 位
上 `'1' < '2'`），尽管数值上 10 > 2。这与 part 域旧端点传 `sort_by=SERIAL_NO` 时的行为
**同构**（同一列、同一 collation），前端切端点后排序观感不变，故**按现状保留**。

若将来要改数值序，只能改列（如加数值列）或改前端本地排序，**不要**在本 SQL 里写
`NULLIF(serial_no,'')::int` 这类表达式 —— 序列号里混着非纯数字前缀（`F1001-01` 等），
转换会直接报错。

### `NULLS LAST` 是显式写死的（两个方向都适用）

`serial_no` 可空（手工工单没序列号）。PG 的 `ASC` 默认是 `NULLS LAST`、但 `DESC` 默认是
`NULLS FIRST` —— 升序不加显式子句与降序的观感会不一致。故**两种方向都显式带
`NULLS LAST`**，让「没序列号的件」在两种方向下都排在末尾，不会被升到最前面抢眼。

### list / count 共用谓词（从结构上杜绝漂移）

`list` 与 `count` 共用 `repo.rs` 里的 `FROM_SQL` 与 `WHERE_SKELETON` 两个常量。part 域旧
端点把同一段谓词手抄两遍，改一处漏一处就会让 `total` 与 `items` 对不上；本模块从结构上
杜绝这种漂移。

`SELECT_COLS`（7 列）**刻意只被 `list` 引用** —— `count` 只需行数，把投影塞进去会给每个
待计数的 part 白跑一次无用列读取。

---

## 业务场景

「制定工序」页：列出所有 `PENDING` 零件（含装配件子件），供生产人员逐件（或批量）指定 /
调整工艺链。

- `process_chain_id` 为 `null` → 该零件**尚未制定工序**（本页的主闸门标记，前端据此显示
  「待制定」并提供「新建工艺链」入口）。
- `process_chain_id` 非 `null` → 已有工艺链，前端据此显示「查看 / 调整」。
- `assembly_id` 非 `null` → 该零件是**装配件的子件**（前端可据此标注归属），但**照样要在
  本页出现**（它同样需要工序）。

---

## 字段定义

### `ProcessDesignPartItemOut` 字段（7 个）

```jsonc
{
  "id": "5000000000001",   // string(i64) 雪花（t_part.id）
  "version": 0,            // i32，t_part.version 乐观锁；当前无消费方，预留
  "serial_no": "F1001-01", // Option<String>，varchar(15)，手工工单可空
  "name": "支架",           // String
  "drawing_no": "DWG-001", // String
  "process_chain_id": null,     // string(i64)?，null = 未制定工序
  "assembly_id": null           // string(i64)?，null = 独立零件；非 null = 装配件子件
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number` 会丢精度）。`t_part.id` |
| `version` | i32 | `t_part.version` 乐观锁版本号。⚠️ **当前无消费方**（本页暂不在本列表上改 part），预留给未来「在本页改零件」的 OCC 回传。与 part 域 `PartOut.version` 是同一列、同一语义 |
| `serial_no` | string? | `varchar(15)` 可空。手工工单没序列号 → JSON `null`（**不是空串**）。排序键，`NULLS LAST` |
| `name` | string | `t_part.name` |
| `drawing_no` | string | `t_part.drawing_no` |
| `process_chain_id` | string (i64)? | `serialize_i64_opt` → JSON string / null。`t_part.process_chain_id`。**`null` = 尚未制定工序**，本页的主闸门标记 |
| `assembly_id` | string (i64)? | `serialize_i64_opt` → JSON string / null。`t_part.assembly_id`。**`null` = 独立零件；非 `null` = 装配件的子零件**。⚠️ 该字段**不**被用作过滤条件（见上文警示段），只供前端标注归属 |

> 字段集**刻意收窄到 7 个**：不加客户 / 交期 / 数量 / `status` 等字段 —— 本页只做
> 「选零件 → 定工序」，展示信息够用即可；且与 part 域 `PartOut`（20 余字段）逐字不同，
> 前端要多一层类型适配，收窄反而更省事。

### `ProcessDesignPartListOut` 字段

```jsonc
{
  "items": [ProcessDesignPartItemOut, ...],
  "total": 2,         // i64，配套 COUNT（不受 limit/offset 限制）
  "limit": 200,       // i64，caller 传入（service 层 clamp(1,500)）
  "offset": 0         // i64
}
```

> `total` / `limit` / `offset` 是**分页计数类 i64，序列化为 JSON number**（非 string）——
> 它们是行数 / 偏移量，远小于 `2^53`，不存在 JS 精度截断风险；形态与 `prod::programming`
> 的 `ProgrammingListOut`（以及 part 域 `PartListOut`）**逐字一致**，前端从
> `GET /parts` 切到本端点时该层无需改动。只有雪花 ID 字段（`id` / `process_chain_id` /
> `assembly_id`）序列化为 string。

### 响应示例

```json
{ "code": 0, "message": "ok", "data": {
  "items": [
    { "id": "5000000000001", "version": 0, "serial_no": "F1001-01", "name": "支架",
      "drawing_no": "DWG-001", "process_chain_id": null, "assembly_id": null },
    { "id": "5000000000002", "version": 0, "serial_no": "F1001-02", "name": "齿轮",
      "drawing_no": "DWG-002", "process_chain_id": "7000000000001", "assembly_id": "8000000000001" }
  ],
  "total": 2, "limit": 200, "offset": 0 } }
```

> 第 2 行是**装配件子件**（`assembly_id` 有值）且**已有工艺链**（`process_chain_id` 有值）——
> 它正常出现在列表里，这正是本端点与 part 域 `GET /parts` 的核心差异。

---

## 关键错误码速查

| Code | Name | HTTP | 触发场景 |
|---|---|---|---|
| 40300 | FORBIDDEN | 403 | 角色守卫失败（非 Manager/Clerk/Inspector/CNC_PROGRAMMER） |
| 50001 | DB_ERROR | 500 | DB 查询失败 |

**关于 40001**：本端点**不会**用 40001 报 `limit` / `offset` 越界 —— 越界一律
**静默 clamp**（`limit=0 → 1`、`limit=99999 → 500`、`offset=-5 → 0`，见 service 层），
调用方永远拿到 200。`40001 VALIDATION_ERROR` 在本端点当前**无触发路径**。

**关于 query 解析失败**：`limit=abc` 这类**无法反序列化为 i64** 的请求由 axum `Query`
extractor 直接拒绝 → HTTP 400 + 纯文本 body（**不走 R 包络**，全仓无自定义 rejection
handler）。前端只需按 HTTP 400 兜底展示。
**空串不算失败**：`?limit=&offset=`（以及全空白 `?limit=%20%20`）按**缺省**处理
（`limit=200` / `offset=0`）并返回 200。

> 完整错误码见 [`../index.md`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实施状态

- ✅ **`prod::process_design`**（2026-10-05 新增）：1 只读端点
  - 6 文件子模块（`mod/dto/vo/repo/service/handler`），零 schema 变更
  - repo 2 静态方法（`list` / `count`）+ 常量 `SELECT_COLS` / `FROM_SQL` /
    `WHERE_SKELETON`（后两者 list / count 共用，从结构上杜绝谓词漂移）
  - 走 `sqlx::QueryBuilder` + 手写 `FromRow`，**未新增 `query!` 宏**、`.sqlx/` 离线元数据零变更
  - `sort_dir` 白名单收敛成 `ASC` / `DESC` 两个字面量（Rust 侧 `match` 兜底，零注入面）
  - 角色守卫含 `CNC_PROGRAMMER`
  - **part 域 `GET /parts` 一行未改**（旧端点保留兼容）
- ✅ 集成测试：`tests/production/process_design.rs` —— **8 场景**
  （1 ★装配件子件可见，含「part 域旧端点确实看不到」的反向断言 / 2 软删闸门 /
  3 四种非 PENDING 状态闸门 / 4 `total` 全量口径 + 翻页 / 5 clamp 边界 + 空串兜底 /
  6 `DESC` 倒序 + 非法值退化 + 字典序口径 / 7 `NULLS LAST` 两方向各验一次 /
  8 角色守卫四角色放行 + SHELF_ACCOUNT 40300）

## 参考

- 模块 README：见 `src/modules/prod/process_design/{mod,handler,service,repo,vo,dto}.rs`
- 被替换的旧端点（保留兼容）：[`../parts/`](../parts/index.md)
- 同类先例：[`./pending-programming.md`](./pending-programming.md)（2026-10-01 `prod::programming`）
