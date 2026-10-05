# assembly 域 API

> 本文件须与 `src/modules/assembly/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：装配体 CRUD（list / get / create-multipart / update / soft-delete）+ cancel。已拆为子目录：
>
> 导航：[**`index.md`**](./index.md) · [`crud.md`](./crud.md) · [`cancel.md`](./cancel.md)

---

## 端点列表

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| GET | `/api/v2/assemblies` | Manager / Clerk / Inspector / CncProgrammer | 列表查询 + 分页 + L1 客户展开 + 多字段过滤 | [`crud.md`](./crud.md#get-apiv2assemblies) |
| POST | `/api/v2/assemblies` | Manager / Clerk | 创建装配体（multipart：`data` JSON + 可选 `files` PDF）+ 自动派生子件 | [`crud.md`](./crud.md#post-apiv2assemblies) |
| GET | `/api/v2/assemblies/{assembly_id}` | Manager / Clerk / Inspector / CncProgrammer | 详情（assembly + children parts + files + child current_batch_id） | [`crud.md`](./crud.md#get-apiv2assembliesassembly_id) |
| POST | `/api/v2/assemblies/{assembly_id}/update` | Manager / Clerk | 字段可选 UPDATE（含 `customer_id` 三态校验 + L2 校验，OCC；Phase 3：`applicant_name`/`order_no`/`note` 三态 NULL-clear） | [`crud.md`](./crud.md#post-apiv2assembliesassembly_idupdate) |
| POST | `/api/v2/assemblies/{assembly_id}/soft-delete` | **Manager** | 软删（OCC；Phase 3：HAS_SHIPMENT 预检 → 20307） | [`crud.md`](./crud.md#post-apiv2assembliesassembly_idsoft-delete) |
| POST | `/api/v2/assemblies/{assembly_id}/cancel` | Manager / Clerk | 取消（终态 COMPLETED/CANCELLED 禁 cancel；非终态一律可 cancel） | [`cancel.md`](./cancel.md#post-apiv2assembliesassembly_idcancel) |
| POST | `/api/v2/assemblies/{assembly_id}/start` | Manager / Clerk | **Phase 3（deferred #4）**：PENDING → IN_PROCESS 状态机守卫 | [`crud.md`](./crud.md) |
| POST | `/api/v2/assemblies/{assembly_id}/files` | Manager / Clerk | **Phase 3（deferred #1）**：multipart PDF 上传到 COS（kind=ASSEMBLY_MASTER） | [`crud.md`](./crud.md#get-apiv2assembliesassembly_idfiles) |
| GET | `/api/v2/assemblies/{assembly_id}/files` | Manager / Clerk / Inspector / CncProgrammer | **2026-09-25 新增**：列出装配体已上传 PDF（kind=ASSEMBLY_MASTER） | [`crud.md`](./crud.md#get-apiv2assembliesassembly_idfiles) |
| POST | `/api/v2/assemblies/{assembly_id}/children` | Manager / Clerk | **2026-09-25 新增**：在已存在装配体下追加单个 part 子件（事务：INSERT t_part + INSERT 初始 t_part_batch） | [`crud.md`](./crud.md#post-apiv2assembliesassembly_idchildren) |
| GET | `/api/v2/parts/{part_id}/assembly` | Manager / Clerk / Inspector / CncProgrammer | **2026-09-25 新增**：按 part 反查其所属装配体（None 表示无父装配体） | [`crud.md`](./crud.md#get-apiv2partspart_idassembly) |

> 路由顺序：`/{assembly_id}` 必须在 `/{assembly_id}/{action}` 之前注册；当前 `/{assembly_id}` 仅 `GET`，无静态冲突。

---

## 共享 DTO

### AssemblyOut 字段

`TAssembly` 完整 22 列；i64 字段（`id` / `customer_id`）用 `serialize_i64` 序列化为 JSON string。

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `drawing_no` | string | 图号 |
| `name` | string | 装配体名 |
| `applicant_name` | string? | 申请人 |
| `customer_id` | string (i64) | 二级客户 id |
| `request_date` | date | 客户请求日 |
| `planned_delivery_date` | date | 计划交付日 |
| `is_urgent` | bool | 紧急标记 |

> 2026-09-16（migration 027）：`AssemblyOut` 删 `actual_delivery_date` 字段
> —— `t_assembly.actual_delivery_date` 列已删；装配体实际交付由子件批次交付事件体现。
>
> 2026-09-17 与 DDL 对齐：`request_date` / `planned_delivery_date` 在 DDL（migration 005:20-21）
> 是 NOT NULL，DTO 去 `Option<>` 包裹；`TAssembly` model 同步去 `Option<NaiveDate>`。
| `status` | string | 状态枚举字符串（PENDING / IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED / COMPLETED / CANCELLED，2026-09 扩 7 态对齐 Python） |
| `version` | i32 | 乐观锁 |
| `serial_no` | string? | 主装配体序列号（建单即派发，格式 `{prefix}{4 位数字}`，如 `F1000`） |
| `quantity` | i32 | 数量 |
| `unit_price` | decimal? | 单价 |
| `total_price` | decimal? | 总价 |
| `order_no` | string? | 订单号 |
| `system_delivery_date` | date? | 系统派工日 |
| `note` | string? | 备注 |
| `created_at` | naive datetime | 创建时间 |
| `updated_at` | naive datetime | 更新时间 |

### AssemblyListItem 字段

`TAssembly` 完整 22 列 + `customer_name` / `parent_customer_name` 冗余字段（两次 join：自身 L2 → name；其 L1 父 → name）。

### AssemblyListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [AssemblyListItem](#assemblylistitem-字段)[] | |
| `total` | i64 | 满足过滤的总数 |
| `limit` | i64 | 实际生效 |
| `offset` | i64 | 实际生效 |

### AssemblyChildOut 字段

`TPart` 子件投影（来自 `PartRepo::list_by_assembly_id`）+ **Phase 3（deferred #7）`current_batch_id`**：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | part 雪花 ID |
| `serial_no` | string? | 子件序列号（`{asm_serial}-{i:02d}`） |
| `name` | string | 子件名 |
| `drawing_no` | string? | 子件图号 |
| `status` | string | part 状态 |
| `version` | i32 | 乐观锁 |
| `quantity` | i32 | 子件数量（实际加工数；2026-09-11 起按父件套数等比缩放，见 §方向 A） |
| `planned_delivery_date` | date? | 计划交付日 |
| `applicant_name` | string | 申请人（§3.1 创建时继承父件，§3.2 update 级联） |
| `request_date` | date | 客户请求日（同上） |
| `order_no` | string? | 订单号（同上） |
| `system_delivery_date` | date? | 系统派工日（同上） |
| `is_urgent` | bool | 紧急标记（同上） |
| `note` | string? | 备注（同上） |
| `unit_price` | decimal? | 单价（JSON 字符串；建单缺省落 0，**不会是 NULL**） |
| `total_price` | decimal? | 总价（同上） |
| `current_batch_id` | string (i64)? | **Phase 3（deferred #7）**：子件当前激活批次 id；`None` 表示无活跃批次 |

> §3.4（2026-09-11）：后 6 字段为"共享信息字段"，创建时从父件 `t_assembly` 继承（§3.1），update 时按父件"更新后的当前行值"覆盖（§3.2）。`AssemblyService::get_assembly` 从 `t_part` 行直接透传。`AssemblyChildOut` 用于 `GET /assemblies/{id}`（children 数组）和 `POST /assemblies` 响应（created_children 数组）。

### AssemblyFileRef 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | part_file 雪花 ID（`t_part_file.part_id` = assembly id，该列兼作 polymorphic owner 列；归属类型靠 `kind='ASSEMBLY_MASTER'` 判别，不单独存盘） |
| `original_filename` | string | 原始文件名 |
| `page_count` | i32? | PDF 页数（**Phase 3**：当前 pass 总是 `None`，懒加载） |

### AssemblyDetail 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `assembly` | [AssemblyOut](#assemblyout-字段) | |
| `children` | [AssemblyChildOut](#assemblychildout-字段)[] | 该 assembly 下的 part 子件 |
| `files` | [AssemblyFileRef](#assemblyfileref-字段)[] | **Phase 3（deferred #1 落地）**：ASSEMBLY_MASTER kind 的 PDF 文件引用 |

### AssemblyCreateResult 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `assembly` | [AssemblyOut](#assemblyout-字段) | 刚 INSERT 的 assembly 行（含 `serial_no`） |
| `created_children` | [AssemblyChildOut](#assemblychildout-字段)[] | 创建的子件（`children` 缺省 / 空数组时为空数组） |


---

## 端点约束（与 Python myERP 对齐）

### Multipart 字段语义

- **必填 `data` 文本字段**：序列化的 [`AssemblyCreateRequest`](./crud.md#assemblycreaterequest-字段) JSON；解析失败 → 20104 INVALID_VALUE。
- **可选 `files` / `file` PDF 字段**：两个名字**等价**（都收，可混用）；可多个二进制；当前实现**只处理首份**做页数校验，其余累计忽略。PDF **不入库**（文件走 `POST /{id}/files` 单独上传）。
- **未识别字段**：一律丢弃（与 Python `python-multipart` 行为对齐），不报 40001。
- 整体走 axum `Multipart` extractor；body 大小上限由 `AppConfig.max_request_body_size` 控制（默认 300 MiB）。

### 序列号（serial_no）模式

- **装配体 `serial_no`**：从 `t_serial_counter` 派发，格式 `{prefix}{4 位数字}`（如 `F1000`），
  取自 4 位号池 `[1000, 9999]`（`1000 + counter % 9000`，用尽后回绕）。
  - `prefix` 是 L1 客户的 `serial_prefix` 首字母（DB CHECK：单大写字母 `A-Z`）。
  - 整个号池被该 prefix 下的单据占满（绕一圈仍无空号）→ 20105 PART_SERIAL_EXHAUSTED。
  - `t_serial_counter` 中无对应 `prefix` 行 → 20108 SERIAL_PREFIX_UNKNOWN。
  - **counter 语义 = 下一个要发的池内下标（先用后递增）**：counter=0 发出 `{prefix}1000`
    并把 counter 写成 1。counter **不得手工重置**（回绕本身就会自动回到 1000）。
  - 装配件与 part **共用同一 counter 行与同一号池**（`t_part.serial_no` 与
    `t_assembly.serial_no` 从同一行取号，互相插号），碰撞检查也必须覆盖两张表：
    装配件按 `uk_t_assembly_serial_no`（`deleted_at IS NULL`，**无** status 谓词 ⇒
    `COMPLETED` 的装配件仍占号）、part 按 `uk_t_part_serial_no`
    （`deleted_at IS NULL AND status <> 'CANCELLED'`）。详见
    [`../parts/index.md`](../parts/index.md#序列号serial_no生命周期)。
- **子件 `serial_no`**：派生 `{asm_serial}-{i:02d}`（如 `F1000-01` / `F1000-02`），
  不占号池（1 次建单只消耗 1 个号）。
  - 子件 ≤ 99（i:02d 范围 1..=99），超出 → 20303 TOO_MANY_CHILDREN。
- **派发时机**：`POST /assemblies` **无条件**派发（与是否上传 PDF 无关），并在同一事务内建全部子件。`POST /assemblies/{id}/children`（详情页补件）**不**派发，新子件 `serial_no = NULL`。

### L1 → L2 客户展开（list 与 create 共享）

- **list**（`GET /assemblies`）：`customer_id` 是 L1 → 递归取所有 L2 子节点 + 自身；是 L2 → 仅 `[customer_id]`；缺省 → 不过滤。
  - 实现：recursive CTE（`WITH RECURSIVE subtree AS (...)`）一次拿齐。
- **create**（`POST /assemblies`）：`customer_id` **必须**是 L2 叶子（`parent_id NOT NULL`）；L1（集团节点）或不存在 → 20302 BAD_CUSTOMER / 20102 CUSTOMER_NOT_FOUND。
  - 实现：`SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL`，校验 `parent_id.is_some()`。

### 防 N+1

- `list_assemblies` 一次性 `SELECT id, name, parent_id FROM t_customer WHERE deleted_at IS NULL AND id IN (...)` 批量拉齐所有 customer 名称（O(1) 查询）。
- `fetch_customer_names` 在 `ids` 为空时直接返回空 HashMap，避免构造 `IN ()` 空 SQL。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901 VERSION_CONFLICT。
- `update` / `soft_delete` 强制 OCC；`cancel` **无 OCC**（cancel 是单向状态翻转，重复 cancel 走 0 行 → 20103 INVALID_TRANSITION）。

### 软删除

- `deleted_at IS NULL`；已软删件视为不存在 → 20301 NOT_FOUND。
- 软删仅由 Manager 触发（service 内 `require_role(Role::Manager)` 守卫）。
- 本 pass 不检查"装配体已挂送货单"（20307 HAS_SHIPMENT 为预留码，后续 PR 启用）。

### 事务边界

- 事务在 **handler** 层开：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式 `tx.commit()`；提前 return 时 `Transaction::drop` 自动回滚。
- service 签名统一 `&mut PgConnection`（兼容 pool / tx）。
- WS 广播在 `tx.commit().await?` **之后**（对齐 Python 延迟广播模式）。
---

## 状态词汇（7 态，唯一权威）

`PENDING` / `IN_PROCESS` / `INSPECTION` / `READY_TO_SHIP` / `DELIVERED` /
`COMPLETED` / `CANCELLED`（2026-09 由 4 态扩展为 7 态，对齐 Python
`AssemblyStatus`；2026-09-02 订正）。

> ⚠️ **DB 注释是历史快照，不是权威**：`migrations/20260925000000_001_baseline.sql`
> 里 `t_assembly.status` 的 `COMMENT` 仍写「PENDING（默认）/ IN_PROCESS /
> COMPLETED / CANCELLED」（4 态）。该文件是**冻结的 append-only 基线，不得修改**
> （改它会让已 apply 过的环境出现校验和不一致）。**本页 + `src/modules/assembly/statemachine.rs`
> 的 7 态才是权威**；如需让 DB 侧注释同步，只能**追加一个新 migration** 的
> `COMMENT ON COLUMN`，本轮（2026-10-01）刻意**未**加——纯注释变更对应用无行为
> 影响，而每加一个 migration 就多一份跨环境 apply 成本与漂移面，收益不抵。

**assembly 域刻意没有 `OUTSOURCE` 状态**：外发的是**子件**，装配件本体不外发。
所以子件 `OUTSOURCE`（progress 3）映射为父件 `IN_PROCESS`——这是**显式契约**，
由单测 `min_progress_three_maps_to_in_process` 锁死。

---

## 子件状态聚合（auto-rollup）

父装配件 `t_assembly.status` 是**派生缓存**（真源是 `t_part_batch.status`），
由 part 侧的 status_gate 在同一事务内自动同步，**前端无需主动调用**。

**触发点**：

- Phase 0（仅 inspection 流，**2026-10-02 起归 prod 域**）：`POST /prod/batches/{batch_id}/{to-inspection,to-ship,to-process}` + `POST /prod/batches/to-inspection` + `POST /prod/batches/to-ship` + `POST /prod/batches/worker-scan`（仅 `INSPECTED` 分支）
- **Phase 1（2026-09-11 起）**：batch lifecycle 端点（`POST /prod/batches/{batch_id}/{deliver, complete, start-repair}` + 仍留 part 域的 `POST /parts/{id}/cancel`）也通过同一个 `status_gate` 派生触发同一 rollup——同一 part 的批次状态变化都会级联到父装配件

算法与 Python `service/_assembly_rollup.py::recompute_assembly_status` 对齐，按以下顺序：

1. 父已是 `COMPLETED` 或 `CANCELLED` → **短路**：不更新。
2. 该 assembly 下所有未软删子件 `status` 集合：
  - 空集 → noop
  - 全部 `CANCELLED` → 父 → `CANCELLED`
  - 全部非 `CANCELLED` 子件都是 `COMPLETED` → 父 → `COMPLETED`
  - 其它 → 取非终态、非 `CANCELLED` 子件的最小 `progress`，按 0..6 逐档映射父件（2026-09 7 态，对齐 Python）：

  | part status | progress | 父件状态 |
  |---|---|---|
  | `PENDING` | 0 | `PENDING` |
  | `PROGRAMMING` | 1 | `IN_PROCESS` |
  | `IN_PROCESS`（含返修中，**2026-10-01** REPAIRING 降级为 `is_repairing` 标记列）| 2 | `IN_PROCESS` |
  | `OUTSOURCE` | 3 | `IN_PROCESS` |
  | `INSPECTION` | 4 | `INSPECTION` |
  | `READY_TO_SHIP` | 5 | `READY_TO_SHIP` |
  | `DELIVERED` | 6 | `DELIVERED` |

3. 目标 == 当前 → noop；否则 `UPDATE t_assembly SET status = $target, version = version + 1`，带 OCC + 终态守卫。

   > **2026-10-01 行为订正**：0 行（version 已被并发改动 / 已进终态）**不再**抛
   > `40901 VERSION_CONFLICT` 回滚整个事务，而是**降级为「跳过本次派生 +
   > `tracing::warn!` 留痕」**。理由：派生层不允许否决主操作 —— 用户点了「送检 /
   > 完成」却被一个**派生缓存**的并发写否决，主操作什么都没发生，还收到一个指向
   > 完全无关实体的 409。缓存写失败最多短暂偏旧（下次任意 part 流转自动追平），
   > 代价远小于主操作被回滚。`t_part.status` 的派生写本来就**不走 OCC**（靠行锁
   > 串行化），两层策略现在一致。响应里的 `synced_assembly_id` 在这种降级下为
   > `null`（未翻转），但**用户的主操作成功**。

4. **派生写可以跨越状态机边**（重要语义，勿当 bug）：上表的状态机（见下节）管的是
   **操作员主动发起的单步流转**（`cancel` / `start` 端点），而 rollup 是**派生写**，
   走 `AssemblyRepo::update_status_if_not_terminal`（只带 OCC + 终态守卫，不查状态机）。
   因此「所有子件一夜之间全送检」会把父件从 `PENDING` **直接推到 `INSPECTION`**
   （`PENDING → INSPECTION` 不在 `can_transition_to` 白名单里）——这在业务上等价于
   「每个子件都真实流转过」，是对子件进度的如实映射。

涉及的 inspection 端点（**2026-10-02 起归 prod 域**）：`POST /prod/batches/{batch_id}/{to-inspection,to-ship,to-process}`、`POST /prod/batches/{to-inspection,to-ship}`、`POST /prod/batches/worker-scan`（仅 `INSPECTED` 分支；`RETURNED` 不动 part.status）。

WS 广播：每次实际翻状态 → commit 后下发 `ASSEMBLY_UPDATED`（payload `{ assembly_id }`），与 assembly update endpoint 复用同一 kind。

**序列号**：父装配件进入终态时由 rollup 顺带**直接清** `t_assembly.serial_no`
（不归档 —— `t_assembly` 没有事件表，其 `note` 是用户可编辑的业务备注）。
子件侧的「先归档到 `t_part_event` 再清」见 [`../parts/index.md#序列号serial_no生命周期`](../parts/index.md#序列号serial_no生命周期)。

**对账兜底**：派生缓存仍可能与真源不一致（历史漏调 sync / 手工改库等）。修复入口是
`POST /api/v2/admin/recompute-rollup`（Manager，可传 `assembly_ids` 定点重算）——
**复用**本节这段聚合实现，不会与业务流算出两个结果。见 [`../admin.md`](../admin.md)。

---

## 状态机

| from | to | 触发场景 |
|---|---|---|
| PENDING | IN_PROCESS | 任一 inspection 流子件翻非 `PENDING`（auto-rollup） |
| IN_PROCESS | INSPECTION | 任一子件进入 INSPECTION（auto-rollup） |
| INSPECTION | READY_TO_SHIP | 任一子件进入 READY_TO_SHIP（auto-rollup） |
| READY_TO_SHIP | DELIVERED | 任一子件进入 DELIVERED（auto-rollup） |
| DELIVERED | COMPLETED | 所有非 CANCELLED 子件翻 `COMPLETED`（auto-rollup） |
| IN_PROCESS | COMPLETED | 兼容：所有非 PENDING 子件全 COMPLETED |
| PENDING | CANCELLED | `cancel`（service 内 `repo::cancel` 守卫） |
| IN_PROCESS | CANCELLED | `cancel` |
| INSPECTION | CANCELLED | `cancel` |
| READY_TO_SHIP | CANCELLED | `cancel` |
| DELIVERED | CANCELLED | `cancel` |
| COMPLETED | 终态 | self-loop / 反向 / 跨度过渡均拒绝 |
| CANCELLED | 终态 | self-loop / 反向 / 跨度过渡均拒绝 |

迁移表见 `src/modules/assembly/statemachine.rs::can_transition_to`。不在白名单的 source / target 组合返回 20103 BIZ_INVALID_TRANSITION。

> 当前实现的 `cancel` 走 `AssemblyRepo::cancel`（`status NOT IN ('COMPLETED','CANCELLED')` 直接 SQL 守卫），未调用 `statemachine.rs::can_transition_to`。状态机 enum 仅做静态校验与单元测试。

---

## 错误码参考

assembly 域（203xx）见下方表格；共享错误码（40001 / 40300 / 40901 / 20102 / 20103 / 20104 / 20105 / 20108）详见 [`../index.md`](../index.md) 跨域错误码速查。

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20301 | BIZ_ASSEMBLY_NOT_FOUND | 404 | assembly 不存在 / 已软删 |
| 20302 | BIZ_ASSEMBLY_BAD_CUSTOMER | 400 | `customer_id` 是 L1（集团节点，不允许作为装配体客户） |
| 20303 | BIZ_ASSEMBLY_TOO_MANY_CHILDREN | 400 | `children` 数量 > 99（序列号派生 i:02d 范围 1..=99） |
| 20305 | BIZ_ASSEMBLY_PDF_INVALID | 400 | PDF 页数与 `children.len()+1` 不匹配 / `lopdf` 解析失败 |
| 20306 | BIZ_ASSEMBLY_CHILD_PRICE_LOCKED | 400 | 父装配体已设总价时禁止子件改价（预留码，当前未启用） |
| 20307 | BIZ_ASSEMBLY_HAS_SHIPMENT | 409 | 装配体已挂送货单，禁止 soft_delete（预留码） |
| 20308 | BIZ_CUSTOMER_NO_SERIAL_PREFIX | 400 | L1 客户的 `serial_prefix` 为空（无法派发序列号） |

> 预留码（20306 / 20307）：常量已注册，但当前 service 未触发，留待后续 PR 启用。

---

## 参考

- 集成测试：`tests/assembly/`（`api.rs` 13 例 / `create_serial_price.rs` 8 例 / `files.rs` 6 例 / `status_sync.rs` 5 例 / `children.rs` 4 例 / `files_list.rs` 4 例 / `by_part.rs` 3 例）
- 仓库分层：`src/modules/assembly/handler.rs` (axum) → `service.rs` (业务) → `repo.rs` (SQL) → `dto.rs` / `model.rs` / `statemachine.rs`
- 状态机：`src/modules/assembly/statemachine.rs`
- 错误码：`src/shared/error.rs::code`
- WS 事件：`ASSEMBLY_CREATED` / `ASSEMBLY_UPDATED` / `ASSEMBLY_DELETED` / `ASSEMBLY_CANCELLED`，详见 [`../websocket.md`](../websocket.md)
- L1 / L2 客户约束：`src/modules/com/customer/`，详见 [`../customers.md`](../customers.md)
- 序列号派发：`t_serial_counter`（part 域共用），详见 [`../parts/index.md`](../parts/index.md)
- Python myERP 参考：`/Users/ren/Code/myERP/api/v1/assembly.py`（9 个端点；本目录 6 个端点之外的 4 个 Python 独有端点见 [`../inconsistencies.md`](../inconsistencies.md)）
