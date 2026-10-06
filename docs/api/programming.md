# programming 域 API（待编程一览）

> 域：`prod::programming`（嵌套域，源码在 `src/modules/prod/programming/`）。本文件是该域的**唯一**契约来源，任何字段 / 端点变更必须同步本文件。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/prod/programming/pending` | Manager + Clerk + Inspector + CncProgrammer | `ProgrammingListQuery`（见下表） | `ProgrammingListOut` |

路由注册链：`src/modules/mod.rs` 的 `/api/v2` nest → `src/modules/prod/mod.rs` 的 `.nest("/programming", programming::router())` → 本域 `mod.rs` 的 `.route("/pending", get(handler::list_pending))`。

- 统一信封 `R { code, message, data }`。
- ⚠️ 角色守卫**下沉在 service 第一行**（`service.rs::ProgrammingService::list_pending` 的 `require_any_role`），handler 只做参数提取 + `pool.acquire()` + `R::ok`，不重复校验。
- ⚠️ `Role::CncProgrammer` **必须**留在白名单里：前端「待编程一览」页由 CNC 编程员账号进入，漏掉该角色直接 403。

### 1.1 `ProgrammingListQuery` 逐参数（Query string）

| 参数 | 类型 | 语义 | 缺省 | 非法值处理 |
|---|---|---|---|---|
| `has_cnc_program` | bool（三态） | Tab 切换：`true` 仅已上传 G_CODE、`false` 仅未上传、`None` 全部 | 不过滤 | 非 `true` / `false` 字面量 → axum `Query` 提取器 **HTTP 400 纯文本**（不走 `R` 信封）；**空串按缺省**（不过滤），不是 422。⚠️ **大小写不敏感；首尾空白会被 trim**（详见下方） |
| `keyword` | string | `name` / `drawing_no` / `serial_no` 三列任一 `ILIKE '%kw%'` | 不过滤 | 任意字符串都接受；trim 后空串 → 不过滤；`%` / `_` / `\` 被**转义**成字面量（见 §3.4） |
| `serial_no` | string | 工单序列号**精确**匹配（`p.serial_no = $n`） | 不过滤 | trim 后空串 → 不过滤 |
| `sort_by` | string | 排序列白名单，7 键（见 §4.3） | 计划交期 | **不报错**，一律静默退化到 `p.planned_delivery_date`（含注入串、小写写法） |
| `sort_dir` | string | `ASC` / `DESC` | `ASC` | **不报错**，非 `DESC`（忽略大小写）一律按 `ASC` |
| `limit` | i64 | 每页行数 | **50** | `0` 及负数 clamp 到 **1**，上界 clamp 到 **500**；空串 / 全空白按缺省；非数字字面量 → HTTP 400 纯文本 |
| `offset` | i64 | 偏移 | `0` | 负数 `max(0)`；空串 / 全空白按缺省；非数字字面量 → HTTP 400 纯文本 |

⚠️ **宽容度是对齐过的，不是巧合**：`limit` / `offset` 走 `dto.rs` 私有 `deserialize_i64_opt_lenient`、`has_cnc_program` 走私有 `deserialize_bool_opt`，两者都显式兜住 serde_urlencoded 对空串的 `visit_some`（`?limit=` 会得到 `Some("")` 而非 `None`）。这保证「筛选框清空态」在前端三种参数上表现一致（按缺省处理），不会有一半走 400 一半走缺省。

⚠️ 带引号的字面量（`?limit="50"`）是**非法**值 → 400；后端不剥引号。

⚠️ **`has_cnc_program` 的宽容度比「非 `true` / `false` 就 400」更宽**（2026-10-07 review 第 1 轮订正）。`dto.rs` 私有 `deserialize_bool_opt` 的顺序是**先 `str::trim`、再 `eq_ignore_ascii_case`**：

- 大小写不敏感：`true` / `TRUE` / `True` / `tRuE` 全部接受；`false` 同理。
- 首尾空白被 trim：`?has_cnc_program=%20false%20` → `Some(false)`。
- ⚠️ **纯空白按缺省而非 400**：`?has_cnc_program=%20` trim 后是空串 ⇒ 落进 `None | Some("")` 那一支 ⇒ `Ok(None)`（不过滤）。即「空串按缺省」的口径要**包含全空白**，不只字面空串。
- trim 之后仍非 `true` / `false`（如 `abc` / `1` / `yes`）才是 400 纯文本。

⚠️ 与 `limit` / `offset` 的宽容度口径一致（三者都先 trim 再判），但**实现来源不同**：本域三个都走 `programming/dto.rs` 的私有 helper。⚠️ `prod::inspection` 队列的同位置参数（`customer_id` / `limit` / `offset`）走 `shared::types::deserialize_i64_opt`，**不 trim、不放行空串** ⇒ 那边发空串是 400 而不是缺省。详见 `docs/api/inspection.md` §3.1。

## 2. `ProgrammingListOut` 逐字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[]` | array | `ProgrammingItemOut`，最多 `limit` 行（§2.1） |
| `total` | number | 过滤后总行数，**不受 `items` 截断影响** |
| `limit` | number | 生效的 limit（clamp 后回显） |
| `offset` | number | 生效的 offset |

⚠️ **分页三件套是裸 JSON number，不是 string** —— `total` / `limit` / `offset` 是行数 / 偏移量（远小于 2^53，无 JS 精度截断风险），刻意不走 `shared::types::serialize_i64`。这与 `prod::inspection` 队列端点的分页计数**方向相反**（那边走 `serialize_i64` → string），两个前端 schema 别互相照抄。

### 2.1 `ProgrammingItemOut` 逐字段（15 个）

| 字段 | 类型 | SQL 来源 | 口径 |
|---|---|---|---|
| `id` | string | `p.id` | 雪花 ID 字符串化（`serialize_i64`） |
| `version` | number | `p.version` | ⚠️ **part 级**乐观锁（`t_part.version`），与批次 OCC 无关 |
| `serial_no` | string \| null | `p.serial_no` | 手工工单可空 |
| `name` | string | `p.name` | |
| `drawing_no` | string | `p.drawing_no` | |
| `quantity` | number | `p.quantity` | ⚠️ **工单量**（`t_part`），不是批次量；本端点没有批次量字段 |
| `status` | string | `p.status` | 原文透出，不做枚举收窄 |
| `is_urgent` | boolean | `p.is_urgent` | |
| `planned_delivery_date` | string | `p.planned_delivery_date` | `String` 而非 `NaiveDate`；DB `NOT NULL` 已保证非空，NULL 走 `"1970-01-01"` 防御性兜底 |
| `system_delivery_date` | string \| null | `p.system_delivery_date` | `NaiveDate` 原生序列化；该列可空 → JSON `null` |
| `customer_name` | string \| null | `c.name`（`LEFT JOIN t_customer c ON c.id = p.customer_id`） | **L2 叶子客户名** |
| `parent_customer_name` | string \| null | `pc.name`（`LEFT JOIN t_customer pc ON pc.id = c.parent_id`） | **L1 一级集团名**；L2 未挂 parent 时为 `null` |
| `has_cnc_program` | boolean | `EXISTS (SELECT 1 FROM t_part_file pf WHERE pf.part_id = p.id AND pf.kind = 'G_CODE' AND pf.deleted_at IS NULL)` | `repo.rs::G_CODE_EXISTS` 单一常量，同时供 SELECT 投影与 WHERE 三态过滤复用 |
| `batch_id` | string \| null | `LEFT JOIN LATERAL` 取 `pb.id`（`status='PROGRAMMING' AND deleted_at IS NULL`，`ORDER BY pb.id DESC LIMIT 1`） | 批次锚点，见 §2.2；雪花 ID 字符串化（`serialize_i64_opt`） |
| `batch_version` | number \| null | 同上 LATERAL 的 `pb.version` | 与 `batch_id` **同生共死**；`t_part_batch.version`（批次 OCC），与上面的 part 级 `version` 严格区分 |

### 2.2 ⚠️ `batch_id` / `batch_version` 这对「批次锚点」

本端点**唯一写出口**是 `POST /api/v2/prod/batches/{batch_id}/release-from-programming`（属 `prod::batch` 域）。该端点以批次为锚：`batch_id` 走 URL path，入参 `PlaceOnShelfRequest.version` 又对 `t_part_batch.version` 做 OCC 校验（`validate_batch_version`）。所以列表行**必须**给得出「PROGRAMMING 活跃批次 id + 该批次版本」，否则前端拼不出这个请求。

取值口径（`repo.rs::PROGRAMMING_BATCH_JOIN`）：

- 只认 `status = 'PROGRAMMING'`。`release_from_programming` 硬要求源状态是 PROGRAMMING（`prod/batch/service/programming.rs` 里 `from != PartStatus::PROGRAMMING` 直接 **20103**），给 PENDING / IN_PROCESS 批次的 id 等于给前端一个**必然失败**的锚点。
- 同一 part 有多个 PROGRAMMING 批次时取 `id` 最大者（雪花 ID 随时间单调递增，即最新那个）。
- **无 PROGRAMMING 批次 → 两列都是 `null`**（`LEFT JOIN LATERAL` 语义）。前端据此禁用「下发」按钮 —— 语义正确，不是数据缺失。
- ⚠️ 与 §2.1 的 part 级 `version` **不要混用**：批次 OCC 只认 `batch_version`。拿 `version` 顶替会打成 `40901 VERSION_CONFLICT`。
- 刻意**不**把这段 LATERAL 放进 `count` 的 FROM：`count` 只要行数，塞进去会给每个待计数的 part 白跑一次相关子查询。

## 3. 过滤谓词（part 状态闸门 + 三规则并集，part 级去重）

### 3.1 骨架

```sql
FROM t_part p
LEFT JOIN t_customer c  ON c.id = p.customer_id          -- L2 叶子客户
LEFT JOIN t_customer pc ON pc.id = c.parent_id           -- L1 一级集团
LEFT JOIN LATERAL ( ... ) pb_prog ON true                -- 批次锚点（§2.2，list 专用）
WHERE p.deleted_at IS NULL
  AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')  -- ⓿ 状态闸门，在三规则括号之外
  AND (
       -- 规则1
       p.status = 'PROGRAMMING'
    OR -- 规则2
       EXISTS (SELECT 1 FROM t_process_chain_step s
               JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL
              WHERE s.chain_id = p.process_chain_id
                AND s.deleted_at IS NULL
                AND pr.is_cnc = TRUE)
    OR -- 规则3
       EXISTS (SELECT 1 FROM t_part_batch pb
               JOIN t_process pr ON pr.id = pb.current_process_id AND pr.deleted_at IS NULL
              WHERE pb.part_id = p.id
                AND pb.deleted_at IS NULL
                AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING')
                AND pr.is_cnc = TRUE)
  )
  AND ( <$has_cnc IS NULL> OR EXISTS (t_part_file kind='G_CODE') = <$has_cnc> )
  [AND (p.name ILIKE $kw ESCAPE '\' OR p.drawing_no ILIKE $kw ESCAPE '\' OR p.serial_no ILIKE $kw ESCAPE '\')]
  [AND p.serial_no = $serial_no]
ORDER BY <白名单列> <ASC|DESC> NULLS LAST, p.id DESC LIMIT $limit OFFSET $offset
```

`list` 与 `count` **共用**私有 `push_where` 与常量 `FROM_SQL`：谓词只写一份，天然杜绝「`total` 与 `items` 对不上」的分页 bug。

### 3.2 三条命中规则

| 规则 | 判据 | 为什么要它 |
|---|---|---|
| 1 | `p.status = 'PROGRAMMING'` | 兼容旧筛选：PROGRAMMING 状态的**唯一**出口就是 `release-from-programming`，历史 PROGRAMMING 工单必须能被列出、才能被消化掉 |
| 2 | 工单工艺链上存在 `is_cnc = TRUE` 的工序 step | 编程员据此进生产流（`t_process.is_cnc`，migration `20260929100000_002_add_is_cnc_to_process.sql` 引入） |
| 3 | 工单存在批次，其 `current_process_id` 指向 `is_cnc = TRUE` 的工序，且批次状态 ∈ `PENDING/IN_PROCESS/PROGRAMMING` | 链可能还没建，先由批次定位 |

⚠️ **规则 3 必须用 `t_part_batch.current_process_id`，严禁改引 `next_process_id`**：后者已被 archive/028 DROP，canonical baseline（`migrations/20260925000000_001_baseline.sql`）里**没有**这一列 —— 只有开发库因 `pg_restore` 旧备份残留才看得到。在干净库上引用会直接 500。`current_process_id` 是 migration `20260930000000_004_add_batch_current_process_id.sql` 引入的**批次工序归属唯一权威列**，下发时由 `BatchRepo::update_batch_dispatched` 写入，worker_pool 候选池 3 条 SQL 也按它普通过滤。

### 3.3 ⚠️ 状态闸门为什么要约束**全部三条**规则

`p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` 写在 WHERE 骨架**最外层**（与 `p.deleted_at IS NULL` 同级、在三规则括号**之外**），因此规则 2 / 3 同样受它约束。

起因：`t_part.process_chain_id` **从不清空**。规则 2（链含 CNC 工序）本身不看 part 状态 —— 若不加状态闸门，历史上挂过 CNC 链的 `COMPLETED` / `CANCELLED` / `DELIVERED` / `READY_TO_SHIP` 工单会**永久**命中「待编程一览」，越积越多且永远消不掉。

回归测试：`tests/production/pending_programming.rs::part_status_gate_excludes_completed_and_delivered`。

### 3.4 `keyword` 的通配符转义

service 规范化后由 `repo.rs::escape_like` 把用户 keyword 里的 `\ / % / _` 三个 LIKE 元字符各前置一个 `\`，配合 SQL 侧 `ESCAPE '\'`：`keyword=50%` 只命中**字面量**含 `50%` 的行，`keyword=a_b` 不会把 `a` + 任意字符 + `b` 全扫进来。

转义顺序固定「先补 `\`、再 push 原字符」，保证 `\` 自身也被正确转义（先转义 `\` 才不会出现 `\%` 被二次解读）。

注入面本就为 0（走 `push_bind` 参数化），这一层是**语义**约束而非安全防护 —— 与 `prod::inspection` 的做法相反：那边**拒绝**通配符（40001），本域是**转义**通配符。两边都别互相「统一」。

## 4. 口径表

### 4.1 行单位差异（跨端对数前必读）

| 用途 | 行单位 | 说明 |
|---|---|---|
| **本端点** | **工单级**（part 级去重） | 三规则并集在 `t_part` 上判定，一个 part 无论命中几条规则、无论有几个 PROGRAMMING 批次，都只出 1 行 |
| `dashboard::in_process` | **批次级** | 一个工单多个 IN_PROCESS 批次 ⇒ 多行（前端因此必须用 `batch_id` 做 `:key`） |
| `prod::batch` 系列批次列表 | **批次级** | |
| `prod::inspection::queue` | **批次级** | |

⚠️ 跨端对数前必读：本端点的**行数**与「待品检队列」「大屏在制」的**行数**不可直接比。本端点 1 个 part 只出 1 行，但 `batch_id` / `batch_version` 只指向其中**最新**那个 PROGRAMMING 批次（§2.2）—— 另外几个 PROGRAMMING 批次在本端点被折叠掉了，在批次级的端点上仍各占一行。

### 4.2 part 状态域与 `DELIVERY_STATUSES` 的分叉

| 集合 | 取值 | 用途 |
|---|---|---|
| 本域状态闸门 | `PENDING` / `IN_PROCESS` / `PROGRAMMING`（3 态） | 只圈「还没离开编程前流程」的工单 |
| `dashboard` 的 `DELIVERY_STATUSES` | `PENDING` / `PROGRAMMING` / `IN_PROCESS` / `OUTSOURCE` / `INSPECTION` / `READY_TO_SHIP`（6 态） | 判「未交付」 |

两者是**独立的两份白名单，不共用常量**：本域的 3 态是「编程员还能动它吗」的判据，`DELIVERY_STATUSES` 是「货交了没」的判据。⚠️ 不要为了「统一状态域」把本域闸门放宽到 `DELIVERY_STATUSES` —— 那样 §3.3 的终态泄漏会立刻回来。

### 4.3 ⚠️ 排序白名单的大小写不对称（已登记口径）

`service.rs` 两个映射函数的大小写敏感度**故意不同**：

| 函数 | 匹配方式 | 小写 `created_at` 的结果 |
|---|---|---|
| `resolve_order_col` | **只认全大写 token**（`Some("CREATED_AT") => …`） | ⚠️ 静默退化到缺省列 `p.planned_delivery_date`，**不报错** |
| `resolve_order_dir` | `eq_ignore_ascii_case("DESC")` | 正确识别为 `DESC` |

后果：前端若传小写 `sort_by=created_at`，页面会按**计划交期**排，且**没有任何报错**。**前端传参一律用全大写枚举。**

排序列白名单（7 键，缺省与非法值一律退化到 `p.planned_delivery_date`）：

| `sort_by` | ORDER BY 列 |
|---|---|
| `CREATED_AT` | `p.created_at` |
| `UPDATED_AT` | `p.updated_at` |
| `PLANNED_DELIVERY_DATE` | `p.planned_delivery_date` |
| `REQUEST_DATE` | `p.request_date` |
| `SERIAL_NO` | `p.serial_no` |
| `DRAWING_NO` | `p.drawing_no` |
| `NAME` | `p.name` |

映射放在 service 层而不是 repo：`order_col` 会被拼进 SQL 文本，只有经过这张映射表的 `sort_by` 才能到达 repo —— 外部输入不可能直接成为 SQL 片段。注入串（如 `p.serial_no; DROP TABLE t_part`）同样只是退化到缺省列。映射表本身锁在 `src/modules/prod/programming/service.rs` 的 `order_col_whitelist_maps_and_degrades` / `order_dir_only_accepts_desc` 两个单测里（含小写退化与注入串退化两条断言），端到端行为由 `tests/production/pending_programming.rs::sorting_default_desc_and_invalid_sort_by` 钉住。

排序缺省值是 `p.planned_delivery_date ASC`（早交期在前），兜底键 `p.id DESC` 保证翻页稳定。

### 4.4 ⚠️ 与其它域的**有意分叉**（不要动）

| 对照域 | 分叉点 | 为什么分叉 |
|---|---|---|
| `part::repo::sql::part_sql` | 缺省排序 `id DESC`（最近建的排前）；方向规则**相反**（只认 `ASC`，其余 → `DESC`）；白名单放 **repo** 内、8 键（含 `SYSTEM_DELIVERY_DATE`） | 零件一览语义是「最新在前」，本域语义是「最急交期在前」。两套并存是有意的，**不要**统一 |
| `prod::inspection::service` | 缺省排序 `p.system_delivery_date ASC`；白名单 7 键（`SERIAL_NO` / `DRAWING_NO` / `NAME` / `BATCH_NO` / `QUANTITY` / `CUSTOMER_NAME` / `SYSTEM_DELIVERY_DATE`）；通配符**拒绝**而非转义 | 待品检页按系统交期近优先排；两个端点的表头列集合本来就不同 |
| `prod::worker_pool` | `has_cnc_program` 的 EXISTS **谓词逐字相同**（`t_part_file.kind='G_CODE' AND deleted_at IS NULL`），但 ① 行单位不同（part 级 vs **batch** 级 `pb.part_id`）；② worker_pool 把它当 `ORDER BY` 第 1 键（已编程 batch 优先 take），本域只作 Tab 筛选与展示 | 同一真相源、不同消费方式。谓词要改必须两侧同批改 |
| `statistics::repo::sql::count_overdue_undelivered` | 走 `planned_delivery_date` 口径且带 `NOT EXISTS (… DELIVERED 事件)` 兜底 | 服务生产统计页，本域只排序不计数。详见 `docs/api/dashboard.md` §4.3 对同一条分叉的登记 |
| `dashboard` | `DELIVERY_STATUSES` 6 态 vs 本域 3 态闸门 | 见 §4.2 |

## 5. 错误码表

| 码 | HTTP | 触发条件 | 出处 |
|---|---|---|---|
| `40300` `FORBIDDEN` | 403 | 角色不在 `{Manager, Clerk, Inspector, CncProgrammer}` 内 | `CurrentUser::require_any_role` |
| `40100` `UNAUTHORIZED` | 401 | 缺 / 坏 Bearer token、签名失败、claims 不合规 | `auth::middleware` + `auth::extractor` |
| `40102` `TOKEN_EXPIRED` | 401 | JWT `ExpiredSignature` | `auth::middleware` |
| `40105` `SESSION_REVOKED` | 401 | Redis session 查不到 / jti 命中吊销黑名单 | `auth::middleware` |
| **HTTP 400 纯文本**（无 `R` 信封） | 400 | `limit` / `offset` 非数字字面量；`has_cnc_program` 非 `true`/`false` | axum `Query` 提取器反序列化失败，**不经 `AppError`** |
| `40800` `REQUEST_TIMEOUT` | 408 | 请求级超时中间件到点（全域 middleware，非本域特有） | `src/middleware/timeout.rs` |
| `500` | 500 | SQL 失败（`sqlx::Error` → `AppError` 映射）；含「引用了 baseline 里不存在的列」这类改错列名导致的运行期错误 | `ProgrammingRepo` → `AppError` |

⚠️ **本域不产生任何业务码（2xxxx）**：排序非法值、`keyword` 通配符、`limit` 越界全部静默降级，不返 201xx。本域唯一「本可以返业务码却返 400 纯文本」的情形是 `Query` 提取器层失败 —— 前端拿不到 `code` 字段，只能按 HTTP 状态分流。

## 6. 移除记录（2026-10-07）

### 6.1 part 域旧端点 `GET /parts/pending-programming` 已删除

2026-10-07 随 part 域旧端点清理一并删除（提交 `refactor(part): 删除旧端点 GET /parts/pending-programming`）。前端已于 2026-10-01 把「待编程一览」的数据源迁到本域端点，旧端点零调用方，**无 alias**（旧路径 404）。

删除范围：SQL 文件 `pending_programming_sql.rs` 整文件、`PartRepoTrait` 上对应的 2 个方法声明 + 2 个 impl + 2 个 re-export、handler `list_pending_programming` + 路由注册 + re-export、DTO `PendingProgrammingQuery`、service `PartService::list_pending_programming`、VO `PendingProgrammingOut`，以及 `tests/part/lifecycle.rs` 里 4 个筛选口径用例 + 其 4 个专用 helper。

### 6.2 ⚠️ 与本端点的语义漂移（旧端点有的 / 本端点没有的）

留着旧端点会诱导误用 —— 两边返回的行集合**不一样**：

| 旧端点的口径（有偏差） | 本域的口径 |
|---|---|
| **缺** `p.status = 'PROGRAMMING'` 这一条规则 | 规则1 就是它（§3.2）；该状态有唯一出口 `release-from-programming`，漏掉就消化不掉历史存量 |
| CNC 判定走 `current_holder_id → t_shelf_process` **间接链路** | 规则3 走 `t_part_batch.current_process_id` **权威列**（§3.2）。间接链路在 `t_process.is_cnc` 未正确维护 / 批次未上架时取不到工序 |
| 无 `serial_no` **精确**筛选（只能靠 keyword 模糊命中） | `serial_no` 是独立参数，`p.serial_no = $n` 精确匹配（§1.1） |
| 无 `escape_like`（`%` / `_` 被当通配符放大） | 转义成字面量（§3.4） |
| 无 L1 / L2 客户名 | `customer_name`（L2）+ `parent_customer_name`（L1）（§2.1） |
| list 与 count **两份手抄 SQL**（其中 `has_cnc_program` 的 EXISTS 子查询重复手抄 3 处）→ `total` 与 `items` 可能对不上 | 共用 `push_where` + `FROM_SQL`，判据只写一份（§3.1） |
| 无 part 状态闸门 | 状态闸门在最外层约束全部三规则（§3.3） |

### 6.3 其它已下线项（登记以防重建）

- `send-to-programming` / `recall-to-programming`（进入 PROGRAMMING 状态的路径）：已下线。编程员通过工艺链 + CNC step（`t_process.is_cnc`）直接进生产流，`PROGRAMMING` 状态只留出口供历史数据消化。⇒ 本域的 `PROGRAMMING` 存量是**只出不进**的，列表会单调变短。
- `t_part_batch.next_process_id` 列：已被 archive/028 DROP。§3.2 已说明为何禁引用。

## 7. 与 WS 的关系

- **本端点不发 WS 广播**：纯筛选列表，无业务流转。handler 只 `pool.acquire()`，不开事务。
- 本端点的**数据新鲜度**靠两件事：① 页面自己写成功后显式 `invalidateQueries({ queryKey: qk.programmingPrefix })`；② 可选 300s `refetchInterval`（页面「自动刷新」开关，走 `computed(() => autoRefresh ? 300_000 : false)` + `refetchIntervalInBackground: true`）。该 query **不设** `staleTime` / `gcTime`，走 `main.ts` 的全局默认（TanStack Query 库默认：`staleTime: 0` / `gcTime: 5min`；`main.ts` 只覆盖 `retry: 0` + `refetchOnWindowFocus: false`）。跨页面 / 他人写的操作**不做精确失效**（全仓既定策略）。

### 7.1 会改变本域返回行的 WS 事件

后端 `WsEvent` 现在只有 `DashboardEvent { kind, payload }` 一个变体，且 ⚠️ **`kind` 是裸 `String`、无枚举保护**（`src/infra/ws_hub.rs`）。下面是后端 `ws_broadcast` 生产方里会改变本域结果的 `kind`：

| `kind` | 对本域的影响 |
|---|---|
| `PART_RELEASED_FROM_PROGRAMMING` | **本域唯一写出口**发的。批次 PROGRAMMING → IN_PROCESS ⇒ 该行通常不再命中（§3.3 状态闸门） |
| `PART_SOFT_DELETED` | `p.deleted_at` 置位 ⇒ 整行消失 |
| `PART_CANCELLED` / `PART_COMPLETED` / `PART_DELIVERED` / `PART_FORCE_COMPLETED` | part 出 §4.2 的 3 态闸门 ⇒ 整行消失 |
| `PART_BATCH_CANCELLED` | 批次软删 ⇒ 可能让规则 3 不再命中（若该 part 仅靠规则3 命中） |
| `PART_BATCH_SPLIT` / `BATCH_PLACED_ON_SHELF` | 改批次集合 ⇒ 影响规则 3 与 §2.2 的批次锚点 |
| `PART_SENT_TO_OUTSOURCE` / `PART_RECEIVED_FROM_OUTSOURCE` | 改批次状态 ⇒ 影响规则 3 的状态白名单命中 |
| `PART_BATCH_WITH_PDFS_CREATED` | 批量建批次 ⇒ 新 part / 新批次进入候选 |

### 7.2 ⚠️ 与前端白名单是**人工同步**关系（无编译期保障）

前端 `useDashboardInvalidation.ts` 的 `AFFECTS_DASHBOARD` 集合是 dashboard 域的失效白名单，与本域**没有**订阅关系 —— 本域页面不消费 WS 事件。⚠️ 而 §7.1 表里的 `kind` **有一部分连 `frontend/src/types/dashboard.ts` 的 `DashboardEventType` 联合类型都没进**，遑论进白名单。

⚠️ 下表是 §7.1 全部 12 个 `kind` 与该联合的**逐项差集**（2026-10-07 review 第 1 轮补全 —— 原版只列了 6 个，漏了 `PART_COMPLETED`）。⚠️ 后端 `kind` 是裸 `String`、**无枚举保护**，所以这份差集不会在任一侧编译失败，只会在「列表不动」这类症状里显形；⚠️ §7.1 每新增一个 `kind` 都要回来重算本表。

| §7.1 `kind` | 在 `DashboardEventType` 联合里？ |
|---|---|
| `PART_RELEASED_FROM_PROGRAMMING` | ❌ 不在（**本域唯一写出口**，见 §8.3 第 1 条） |
| `PART_CANCELLED` | ❌ 不在 |
| `PART_COMPLETED` | ❌ 不在 |
| `PART_FORCE_COMPLETED` | ❌ 不在 |
| `BATCH_PLACED_ON_SHELF` | ❌ 不在（⚠️ 易误判：联合里那个是 v1 旧事件集里的 `PLACED_ON_SHELF`，**与本 kind 不是同一个字符串**） |
| `PART_SENT_TO_OUTSOURCE` | ❌ 不在 |
| `PART_RECEIVED_FROM_OUTSOURCE` | ❌ 不在 |
| `PART_SOFT_DELETED` | ✅ 在 |
| `PART_DELIVERED` | ✅ 在 |
| `PART_BATCH_CANCELLED` | ✅ 在 |
| `PART_BATCH_SPLIT` | ✅ 在 |
| `PART_BATCH_WITH_PDFS_CREATED` | ✅ 在 |

⇒ 差集共 **7 个**（前 7 行），交集 5 个。「在联合里」只保证 TS 侧能把它当 `DashboardEventType` 用，**不代表进了 `AFFECTS_DASHBOARD` 白名单** —— 后者是 `useDashboardInvalidation.ts` 里的独立集合，本域页面两条路都不走。

**无编译期约束的后果**：任一侧新增 `kind` 不会让另一侧编译失败，只会让「看板不动 / 列表不动」这类症状极难定位。§8.3 登记了本域的具体已知偏差。

## 8. 表依赖与前端配套

### 8.1 读的 6 张表

`t_part` / `t_part_batch` / `t_customer`（L2 + 自连 L1，同一张表 JOIN 两次）/ `t_process` / `t_process_chain_step` / `t_part_file`。

软删闸门（5 处，`repo.rs` 内逐条写死）：`p` / `pb` / `pr` / `s` / `t_part_file pf`。

⚠️ **客户侧故意不过滤 `deleted_at`**（`FROM_SQL` 的两条 `LEFT JOIN t_customer` 都没有软删条件），这是**有意的**分叉：历史工单需要显示其原客户名 —— 软删客户后工单仍在册，名字不能变空。若将来要改这一口径，须同步评估历史列表页的展示回归。

本域**零跨域依赖**：`shared::domain_guard` 的单测 `src/modules/prod/programming/mod.rs` 里的 `programming_domain_depends_on_no_other_domain` 把这条边界变成 CI 强制（扫 `src/modules/prod/programming/**/*.rs`，代码区里出现任何 `crate::modules::<他域>` 路径即 panic；`prod::batch` 这类**兄弟域**同样算跨域）。

### 8.2 前端配套改动清单

| 落点（`frontend` 仓） | 路径 | 备注 |
|---|---|---|
| queryKey 工厂 | `src/composables/queries/keys.ts` 的 `qk.programmingList(params)` / `qk.programmingPrefix` | 全仓唯一 queryKey 来源，禁止在调用点拼字面量数组 |
| Zod 守门 schema | `src/views/cnc/composables/pendingProgrammingSchema.ts` | `pendingProgrammingItemSchema`（15 字段逐个显式声明）+ `pendingProgrammingListResultSchema` |
| query hook | `src/views/cnc/composables/usePendingProgrammingQuery.ts` | hook 无自有状态，入参 `MaybeRefOrGetter`（params + `enabled` + `autoRefresh`） |
| 页面级 store | `src/views/cnc/composables/usePendingProgrammingStore.ts` | 分页 / 筛选 / 对话框态 + mutation + `invalidateProgrammingQuery(qc)` |
| 列定义 | `src/views/cnc/pendingProgrammingColumnDefs.ts` | 批次锚点改名义务在 `RELEASE_*` 常量注释上双向登记 |
| api 层 | `src/api/programming.ts` | URL `/prod/programming/pending` |

⚠️ **客户字段名与 part 域不同名**：本域是 `parent_customer_name`(L1) / `customer_name`(L2)，part 域 `PartListItem` 是 `l1_customer_name` / `customer_name`。两边的行对象**不能互相 cast**，否则列渲染读不到键、静默显示「—」。

⚠️ **Zod schema 是 strip 模式的 `z.object`（非 `.strict()`）**：后端改字段名不会让 Zod 报错，只会被静默丢弃。所以批次锚点两个字段的改名是**双向登记**义务，不是单侧约定。

### 8.3 已知偏差登记

1. **`PART_RELEASED_FROM_PROGRAMMING` 不在任何前端 WS 白名单里**（§7.2）。后果：编程员 A 在页面 X 点「下发」后，页面 Y 开着待编程一览不会自动刷新该行；要等下一次挂载 / 手动刷新 / 勾上「自动刷新」后的 300s 轮询。产品决议（2026-10-07）：**不处理**。本域页面靠页面自身 mutation 的显式失效 + 可选轮询兜新鲜度，与全仓其它域的策略一致（跨页面写操作不做穷举失效）。
2. **`batch_id` / `batch_version` 是「最新 PROGRAMMING 批次」这一个锚点**，同 part 若存在多个 PROGRAMMING 批次，前端下发操作只会作用到最新的那个；另外几个批次在本端点**不可见、不可操作**。产品决议（2026-10-07）：**不处理** —— `send-to-programming` 已下线，多 PROGRAMMING 批次是存量数据，新数据不会再产生这种形态。
3. **`quantity` 是工单量不是批次量**：待编程页展示的「数量」列与下发时真正流动的数量（批次量）**可能不同**。这是行单位差异的必然结果（§4.1），不是缺陷。产品决议（2026-10-07）：**不处理**，前端下发走批次 id，批次量以写端点为准。