# prod::programming 域 API —— 待编程一览（2026-10-01 新增）

> 本文件须与 `src/modules/prod/programming/{handler.rs,dto.rs,service.rs,repo.rs,vo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：**CNC 编程员待编程工作台**的单一只读筛选端点。前端「待编程一览」页从 part 域
> `GET /api/v2/parts/pending-programming` 切到本端点（2026-10-01）；part 域旧端点
> **保留兼容、一行未改**，仅在 [`../parts/lifecycle.md`](../parts/lifecycle.md) 追加弃用说明。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/programming/pending` | **Manager+Clerk+Inspector+CNC_PROGRAMMER** | 待编程工单列表（三规则并集，part 级去重） |

> 路由挂载：`prod::mod::router().nest("/programming", programming::router())` —— 见 `src/modules/prod/mod.rs`。

---

## 为什么不在 part 域改

1. **part 域旧端点在开发库恒返空**：其规则 B 依赖「批次所在货架 → 工序」链路
   （`t_part_batch.current_holder_id → t_shelf_process → t_process.is_cnc`），而开发库
   `t_process.is_cnc` 全 false、`t_process_chain_step` 0 行 → 恒 0 命中。
2. **旧端点读的是间接链路，不是权威列**：migration 004
   （`20260930000000_004_add_batch_current_process_id.sql`）起，
   `t_part_batch.current_process_id` 才是批次工序归属的**唯一权威依据**（下发时由
   `BatchRepo::update_batch_dispatched` 写入，worker_pool 候选池 3 条 SQL 也按它普通过滤）。
   本端点规则 3 直接读该列。
3. part 是跨域枢纽，CLAUDE.md 要求其只承载「工单本体生命周期」；待编程一览是**生产
   调度视角**（按 CNC 工序归属筛），归 prod 域更合适。

---

### `GET /api/v2/prod/programming/pending`

权限：**Manager + Clerk + Inspector + CNC_PROGRAMMER**（service 内守卫）

> ⚠️ `CNC_PROGRAMMER` **必须**在内：前端「待编程一览」页由 CNC 编程员账号进入，
> 漏掉该角色会直接 40300。

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `has_cnc_program` | bool? | ✗ | Tab 切换三态：`true` 仅已上传 G_CODE；`false` 仅未上传；缺省（`null` 或空串）全部 |
| `keyword` | string? | ✗ | 模糊匹配 `name` / `drawing_no` / `serial_no`（`ILIKE '%kw%' ESCAPE '\'`）；trim 后为空按缺省处理。**`%` / `_` / `\` 按字面量转义**（`keyword=50%` 只命中字面量含 `50%` 的行，不做通配全扫） |
| `serial_no` | string? | ✗ | 工单序列号**精确**匹配（`p.serial_no = $n`）；trim 后为空按缺省处理 |
| `sort_by` | string? | ✗ | 白名单 `CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` / `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它值**退化**为 `PLANNED_DELIVERY_DATE`（不报错，大小写敏感） |
| `sort_dir` | string? | ✗ | `ASC` / `DESC`（缺省 `ASC`；非 `DESC` 一律按 `ASC` 处理，大小写不敏感） |
| `limit` | int? | ✗ | 缺省 50；service 层 `clamp(1, 500)`。值以 URL query 形态到达（`?limit=50`），**带引号的 `"50"` 不接受**（400）；**空串 / 全空白按缺省处理** |
| `offset` | int? | ✗ | 缺省 0；service 层 `max(0)`。值以 URL query 形态到达（`?offset=0`），带引号的 `"0"` 不接受（400）；**空串 / 全空白按缺省处理** |

> **`limit` / `offset` 的取值容错**（2026-10-01）：① query string 无类型之分，
> 数字一律以字符串到达，统一 `parse` 成 `i64` —— 带引号的 `"50"` 属非法字面量 → 400；
> ② **数字两侧的空白会被 trim**（`?limit=%2012%20` → `12`）；
> ③ 空串 / 全空白按**缺省**处理（`?limit=&offset=` → `50` / `0`），与
> `?has_cnc_program=` 的宽容度一致；④ 非数字（`abc`）/ 小数（`50.5`）/ 溢出仍 → 400。

Response 200 `data`：[`ProgrammingListOut`](#programminglistout-字段)

业务流转：

1. 角色守卫：Manager + Clerk + Inspector + CNC_PROGRAMMER
2. limit / offset 边界 clamp（`limit=0 → 1`，`offset=-1 → 0`）
3. 两条 SQL（`ProgrammingRepo::list` / `ProgrammingRepo::count`）共用同一个私有
   `push_where` + `FROM_SQL` 常量 —— list / count 的谓词**只此一份**，杜绝
   「改 list 漏 count」导致 `total` 与 `items` 对不上

---

## 过滤谓词（part 状态闸门 + 三规则并集，part 级去重）

```sql
SELECT p.id, p.version, p.serial_no, p.name, p.drawing_no, p.quantity,
       p.status, p.is_urgent, p.planned_delivery_date, p.system_delivery_date,
       c.name AS customer_name, pc.name AS parent_customer_name,
       EXISTS (SELECT 1 FROM t_part_file pf
                WHERE pf.part_id = p.id AND pf.kind = 'G_CODE'
                  AND pf.deleted_at IS NULL) AS has_cnc_program
FROM t_part p
LEFT JOIN t_customer c  ON c.id = p.customer_id          -- L2 叶子客户
LEFT JOIN t_customer pc ON pc.id = c.parent_id           -- L1 一级集团
WHERE p.deleted_at IS NULL
  AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')   -- ⚠️ 状态闸门，约束全部三规则
  AND (
       -- 规则1：兼容旧筛选
       p.status = 'PROGRAMMING'
    OR -- 规则2：工单工艺链上含 CNC 工序
       EXISTS (SELECT 1 FROM t_process_chain_step s
                JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL
               WHERE s.chain_id = p.process_chain_id
                 AND s.deleted_at IS NULL
                 AND pr.is_cnc = TRUE)
    OR -- 规则3：工单批次当前挂在 CNC 工序
       EXISTS (SELECT 1 FROM t_part_batch pb
                JOIN t_process pr ON pr.id = pb.current_process_id AND pr.deleted_at IS NULL
               WHERE pb.part_id = p.id
                 AND pb.deleted_at IS NULL
                 AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING')
                 AND pr.is_cnc = TRUE)
  )
  AND ( <$has_cnc IS NULL> OR EXISTS (…kind='G_CODE'…) = <$has_cnc> )   -- Tab 切换三态
  [AND (p.name ILIKE $kw ESCAPE '\' OR p.drawing_no ILIKE $kw ESCAPE '\' OR p.serial_no ILIKE $kw ESCAPE '\')]
  [AND p.serial_no = $serial_no]
ORDER BY <白名单列> <ASC|DESC> NULLS LAST, p.id DESC
LIMIT $limit OFFSET $offset
```

> **客户侧故意不过滤 `deleted_at`**：`c` / `pc` 两个 `LEFT JOIN` 刻意不加软删条件
> —— 历史工单需要显示其原客户名（客户软删后名字不能变空）。这与其余 5 处严格软删
> 过滤（`p` / `pb` / `pr` / `s` / `t_part_file`）的差异是**有意的**。

每条规则一句话业务解释：

- **规则1（`p.status = 'PROGRAMMING'`）**：工单状态仍停在 PROGRAMMING 的历史数据仍允许消化，避免旧在制品在新口径下消失。
- **规则2（链含 CNC 工序）**：工单绑定的工艺链上有任一 `is_cnc = TRUE` 工序 step —— 编程员按工艺链判断该编哪道程序。
- **规则3（批次在 CNC 工序）**：工单至少有一个在制批次（`PENDING` / `IN_PROCESS` / `PROGRAMMING`），其 `current_process_id` 指向 `is_cnc = TRUE` 工序 —— 链还没建时，批次本身就是工序归属依据。

> **⚠️ 三条规则全部受 part 状态白名单约束**（2026-10-01 加固）：状态闸门
> `p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` 写在 WHERE 最外层
> （与 `p.deleted_at IS NULL` 同级、在三规则括号**之外**），因此**规则2 / 规则3 同样
> 受其约束**。已交付（`DELIVERED`）/ 已完成（`COMPLETED`）/ 已取消（`CANCELLED`）
> 的工单**即使挂过 CNC 工序也不出现**。
>
> 加固原因：`t_part.process_chain_id` **从不清空**，而规则2 本身不看 part 状态
> → 历史上挂过 CNC 链的已交付/已完成工单会**永久**命中本页。
> [`../parts/lifecycle.md`](../parts/lifecycle.md) 描述的 part 域旧端点本来就有这条
> 闸门（回归测试 `tests/part/lifecycle.rs::list_pending_programming_excludes_completed_or_cancelled`
> 锁住），**新端点不允许比旧端点更宽**。本端点无 `status` query 参数，前端无法在
> 客户端二次过滤。
> 回归测试：`tests/production/pending_programming.rs::part_status_gate_excludes_completed_and_delivered`。

> **三规则是并集，且按 part 去重**：同一工单同时命中多条规则时**只出现一行**
> （谓词写在 `WHERE` 里而非 JOIN 里，天然不产生行放大），`total` 同样按 part 计数。

> **⚠️ 规则3 必须用 `t_part_batch.current_process_id`**：严禁改引
> `t_part_batch.next_process_id` —— 该列已被 archive/028 DROP，canonical baseline
> （`migrations/20260925000000_001_baseline.sql`）里**没有**这列（只有开发库因
> `pg_restore` 旧备份残留才看得到），在干净库上引用会直接 500。

### `has_cnc_program` 真相源

`EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id AND kind = 'G_CODE' AND deleted_at IS NULL)`
—— 与 part 域旧端点、`prod::worker_pool` 候选池判定「已编程」同源，软删文件不算已上传。

该表达式在 `repo.rs` 里是**唯一常量 `G_CODE_EXISTS`**，同时供 list 的 SELECT 列表
（返回给前端的 `has_cnc_program` 值）与 WHERE 的三态过滤复用 —— 改 `kind` / 软删
条件时只需改一处，不存在「返回值与过滤口径漂移」的可能。

---

## 业务场景

- **Tab = 待编程**（`has_cnc_program=false`）：命中三规则但**未**上传 G_CODE 的工单。编程员在列表内点「上传 G_CODE」→ `POST /api/v2/part-files/upload-intents` + 直传 COS + `confirm`。
- **Tab = 已编程**（`has_cnc_program=true`）：命中三规则且**已**上传 G_CODE 的工单。编程员确认无误后通知车间放行（走 `POST /api/v2/prod/batches/{batch_id}/release-from-programming`）。
- **Tab = 全部**（`has_cnc_program` 缺省）：所有命中三规则的工单。

---

## 字段定义

### `ProgrammingItemOut` 字段（13 个）

```jsonc
{
  "id": "1001",                     // string(i64) 雪花
  "version": 0,                     // i32，乐观锁
  "serial_no": "B01",               // Option<String>，手工工单可空
  "name": "fala-A",                 // String
  "drawing_no": "DWG-001",          // String
  "quantity": 5,                    // i32
  "status": "PROGRAMMING",          // String，t_part.status
  "is_urgent": false,               // bool
  "planned_delivery_date": "2026-09-30", // String（"YYYY-MM-DD"）；DB NOT NULL
  "system_delivery_date": null,     // Option<NaiveDate>
  "customer_name": "ACME L2",       // Option<String>，L2 叶子客户
  "parent_customer_name": "ACME Group", // Option<String>，L1 一级集团
  "has_cnc_program": false          // bool，是否已上传 G_CODE
}
```

> 字段集**刻意收窄**：不加 `match_reason` 之类诊断字段 —— 命中原因由规则语义表达，
> 前端不需要逐行归因。

### `ProgrammingListOut` 字段

```jsonc
{
  "items": [ProgrammingItemOut, ...],
  "total": 42,        // i64，配套 COUNT（不受 limit/offset 限制）
  "limit": 50,        // i64，caller 传入（service 层 clamp(1,500)）
  "offset": 0         // i64
}
```

> `total` / `limit` / `offset` 是**分页计数类 i64，序列化为 JSON number**（非 string）——
> 它们远小于 `2^53` 无 JS 精度风险，形态与 part 域 `PartListOut`（本端点所替换的
> `GET /parts/pending-programming` 的出参，定义见
> [`../parts/lifecycle.md`](../parts/lifecycle.md#get-apiv2partspending-programming)）
> **逐字一致**，前端从旧端点切到本端点时该层零改动。只有雪花 ID
> （`ProgrammingItemOut.id`）序列化为 string。

---

## 关键错误码速查

| Code | Name | HTTP | 触发场景 |
|---|---|---|---|
| 40300 | FORBIDDEN | 403 | 角色守卫失败（非 Manager/Clerk/Inspector/CNC_PROGRAMMER） |
| 50001 | DB_ERROR | 500 | DB 查询失败 |

**关于 40001**：本端点**不会**用 40001 报 `limit` / `offset` 越界 —— 越界一律
**静默 clamp**（`limit=0 → 1`、`limit=9999 → 500`、`offset=-1 → 0`，见 service 层），
调用方永远拿到 200。`40001 VALIDATION_ERROR` 在本端点当前**无触发路径**（保留表格行仅
为对齐 part 域旧端点的同名表述）。

**关于 query 解析失败**：`limit=abc` 这类**无法反序列化为 i64** 的请求由 axum
`Query` extractor 直接拒绝 → HTTP 400 + 纯文本 body（**不走 R 包络**，全仓无自定义
rejection handler）。前端只需按 HTTP 400 兜底展示。
**空串不算失败**：`?limit=&offset=`（以及全空白 `?limit=%20%20`）与
`?has_cnc_program=` 一样按**缺省**处理（`limit=50` / `offset=0`）并返回 200。

> 完整错误码见 [`../index.md`](../index.md#跨域错误码速查) 与 `src/shared/error.rs::code`。

---

## 实施状态

- ✅ **`prod::programming`**（2026-10-01 新增）：1 只读端点
  - 5 文件子模块（`mod/dto/vo/repo/service/handler`），零 schema 变更
  - repo 2 静态方法（`list` / `count`）+ 私有 `push_where`（list/count 共用谓词）
  - 单一常量 `G_CODE_EXISTS` 供 SELECT 列表与 WHERE 过滤复用
  - `keyword` 的 `%` / `_` / `\` 走 `escape_like` 转义 + `ESCAPE '\'`
  - part 状态闸门 `status IN (PENDING, IN_PROCESS, PROGRAMMING)` 约束全部三规则
  - 角色守卫含 `CNC_PROGRAMMER`
- ✅ 集成测试：`tests/production/pending_programming.rs` —— **14 场景**
  （1-10 三规则各自单独命中 / 并集去重 / 反例 / `has_cnc_program` 三态 /
  keyword+serial_no / 排序默认+DESC+非法退化 / 分页边界 / 角色守卫含 CNC_PROGRAMMER
  与 SHELF_ACCOUNT 403；11 part 状态闸门排除 COMPLETED+DELIVERED；
  12 五处软删过滤（含 G_CODE 软删后 `has_cnc_program` 翻回 false）；
  13 `?limit=&offset=` 空串走缺省；14 keyword 通配符按字面量匹配）

## 参考

- 模块 README：见 `src/modules/prod/programming/{mod,handler,service,repo,vo,dto}.rs`
- 旧端点（保留兼容）：[`../parts/lifecycle.md`](../parts/lifecycle.md#get-apiv2partspending-programming)
- 候选池同源判定：`docs/api/production/worker-pool.md`
