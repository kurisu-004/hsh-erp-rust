# com::delivery_note 域 API（送货单 + 送货分组 + 司机候选）

> 本文件是 `com::delivery_note` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码位置：`src/modules/com/delivery_note/`（`handler/` · `service/` · `repo/` · `vo/` · `dto.rs`）。
> 权威路由清单：`handler::ROUTES`（`/note` 段）、`handler::GROUP_ROUTES`（`/group` 段）、
> `handler::DRIVERS_ROUTES`（`/drivers` 段），单测
> `handler::tests::routes_declared_in_router` 逐条比对三者与各自 `xxx_router()` 源码。

## 0. 变更摘要

1. **域平移** `src/modules/delivery_note/` → `src/modules/com/delivery_note/`；URL **硬切无
   alias**：旧 `/api/v2/delivery-notes/*` 与 `/api/v2/delivery-groups/*` 一律 404。多个前缀收敛成
   一个 nest `/api/v2/com/delivery/{note,group,drivers}`。
2. **事件子系统下线**（`DROP TABLE t_delivery_note_event`）+ **`NoteScope` 范围判定逻辑删除**
   + 建单判定键从 `(customer_id, scope)` 三态收敛为 `(customer_id, status='DRAFT')` **单键**。
3. **入单收敛为扫码单一入口**：`POST /scan` 重写（客户端显式提交 `entries[]`，服务端在同一事务内
   find-or-create + DP 分配 + 拆批 + 挂单），删 7 条端点；新增只读扫码三层树
   `GET /scan/{serial_no}`；新增 DP 批次分配算法。
4. **打印链路全删**（前端改 hucre 本地生成 xlsx）+ 新增 `POST /{id}/driver` 与 `GET /drivers`
   + `/pickup` 入参瘦身并重跑司机校验。
5. **VO 字段裁剪 29 个** + `RemoveParts` → `RemoveBatches` 改名 + `SubmitDeliveryOut` 塌缩为
   `R<String>`。
6. **菜单 `delivery_dispatch` 下线**（`seeds/menu.sql`，非 migration —— seed 每次启动重放）：
   §0 复活清单 / §2 INSERT / §4.1 + §4.3 白名单全部移除，§3.5 显式软删既有行，§4.6 回收
   `t_role_menu`。该菜单的目标页依赖的恰好是本轮删掉的 `GET /pickup-pending` /
   `POST /{id}/pickup-scan` ⇒ 前端必须同步删页（§8.3 第 16 行）。
7. **`DeliveryNoteLineItem` 新增 `customer_id`**（L2 叶子 id，必填非空）：打印分组键由
   `customer_name` 切到 id —— `t_customer.name` 非唯一，同名 L2 会被并进同一张 sheet。

### 0.1 变更记录

- **2026-10-10 `applicant_name` 加装配件回落（§2.3.2）**：行项「申请人」改为
  **子件为空时回落所属装配件的 `t_assembly.applicant_name`**。存量数据里装配件向下
  继承申请人这条设计**大量未落地**（开发库实测 490 条子件中 360 条 `t_part.applicant_name`
  为空串，而 90 个 `t_assembly.applicant_name` 无一为空），直接读子件列会让这些行渲染成
  「—」。**只做读侧回落、不回填存量**（理由见 §2.3.2）。字段本身恒定存在、前端三处渲染点
  已在读 ⇒ **前端零改动**（§8.3 前端配套表第 19 条）。同时把单张详情与批量详情两处
  逐字重复的行项装配收进 `service/line_item.rs::build_line_item` 一个实现，口径只维护一处。
- **2026-10-10 行项默认序改为「加入本单的次序」（§2.3.1）**：单据详情与批量详情的
  `line_items[]` 排序键从 `t_part_batch.id` 切成 `delivery_seq ASC NULLS LAST, id ASC`
  —— 批次 id 是**建批顺序**，整批直接挂单的批次用的是建批时的 id，「先扫 A 后扫 B、
  但 A 建得更早」会把 A 排到前面。为此新增 DB 列 `t_part_batch.delivery_seq`
  （migration `20261010000000_001_add_batch_delivery_seq`，按 `pb.id` 序回填历史行、
  部署无视觉跳变）与行项字段 `DeliveryNoteLineItem.delivery_seq`（`number | null`，
  走普通 serde）。挂单写点赋 `MAX+1`、摘单 / 单据软删写点置 NULL。
  ⚠️ 与本仓既有的 `sort_order`（配置显示序）语义不同，不要混用。
- **2026-10-09 `POST /scan` 占用闸门的作用域收窄（§4.1）**：21406（批次被其它送货单占用）
  **不再顺带拒绝整个请求**，与同日的 21405 状态闸门收敛成同口径。判定权同样交给 DP ——
  该零件的可入单量够本次要的量就正常入单，被占用的批次只是不参与分配；只有凑不出时才报
  21406，并在 message 里附被占用批次的单号与单据状态。**21406 的业务含义没有削弱**：占用
  批次依然不可入单（可入单集合的 SQL 已结构性排除它们 ⇒ 不存在重复挂单），依然被点名。
  错误码在汇总处二选一：**失败 part 里只要有一条占用明细就是 21406**，否则 21405（混合场景
  取「冲突」这一侧，message 里两类明细都列全）。
- **2026-10-09 `POST /scan` 状态闸门的作用域收窄（§4.1）**：批次状态 ≠ `READY_TO_SHIP`
  **不再顺带拒绝整个请求**。判定权交给 DP —— 该零件的可入单量够本次要的量就正常入单，
  非 READY 批次只是不参与分配；只有凑不出时才报 21405，并在 message 里附被拦下的批次明细。
  DP 失败改为**收集**而非遇错即停 ⇒ 一次请求里所有凑不出的 part 汇总进同一条 21405（按
  `part_id` 升序分段）；请求含 ≥2 个 part 时每个失败段必带 `part {id}（需 N 件）：` 前缀。

## 1. 端点表（**17 个**）

### 1.1 `/api/v2/com/delivery/note`（12 个）

| # | 方法 | 路径 | 权限 | 入参 | 响应 `data` |
|---|---|---|---|---|---|
| 1 | GET | `/` | Mgr+Clerk+Insp+CncProg | `DeliveryNoteListQuery` | `DeliveryNoteListOut` |
| 2 | GET | `/batch-detail?ids=` | Mgr+Clerk+Insp | `ids` 逗号串（1..=200） | `BatchDeliveryDetailData` |
| 3 | GET | `/scan/{serial_no}` ★新增 | Mgr+Clerk+Insp | 路径参数（**原样，序列号可含 `-`**） | `DeliveryScanTreeOut` |
| 4 | POST | `/scan` ★重写 | Mgr+Clerk+Insp | `ScanEntryRequest` | `DeliveryNoteDetailOut` |
| 5 | GET | `/{id}` | Mgr+Clerk+Insp | — | `DeliveryNoteDetailOut` |
| 6 | POST | `/{id}/update` | Mgr+Clerk+Insp | `DeliveryNoteUpdateRequest` | `DeliveryNoteOut` |
| 7 | POST | `/{id}/remove-batches` ★改名 | Mgr+Clerk+Insp | `DeliveryNoteRemoveBatchesRequest` | `DeliveryNoteDetailOut` |
| 8 | POST | `/{id}/driver` ★新增 | Mgr+Clerk+Insp | `DeliveryNoteDriverRequest` | `DeliveryNoteOut` |
| 9 | POST | `/{id}/submit` | Mgr+Clerk+Insp | `DeliveryNoteVersionedRequest` | `String`（单据 id）★改 |
| 10 | POST | `/{id}/recall` | Mgr+Clerk+Insp | `DeliveryNoteVersionedRequest` | `DeliveryNoteOut` |
| 11 | POST | `/{id}/pickup` ★入参瘦身 | 任意已登录账号 | `DeliveryNotePickupRequest` | `DeliveryNoteOut` |
| 12 | POST | `/{id}/soft-delete` | Mgr+Clerk+Insp | `DeliveryNoteVersionedRequest` | `null` |

`GET /` 的权限含 `CncProgrammer`（编程岗要看单据列表）；其余 11 条是 Mgr+Clerk+Insp。

### 1.2 `/api/v2/com/delivery/group`（4 个）

| # | 方法 | 路径 | 权限 | 入参 | 响应 `data` |
|---|---|---|---|---|---|
| 13 | GET | `/` | Mgr+Clerk+Insp+CncProg | `customer_id`（L1） | `DeliveryGroupListOut` |
| 14 | POST | `/` | Mgr+Clerk+**Insp** ★ | `CreateDeliveryGroupRequest` | `DeliveryGroupOut` |
| 15 | POST | `/{id}/update` | Mgr+Clerk+**Insp** ★ | `UpdateDeliveryGroupRequest` | `DeliveryGroupOut` |
| 16 | POST | `/{id}/soft-delete` | Mgr+Clerk+**Insp** ★ | `DeliveryGroupIdRequest` | `null` |

★ 2026-10-08 加 `Inspector`：品检员在扫码入单页要能按 L2 归属分单，此前被卡在这一步。

### 1.3 `/api/v2/com/delivery/drivers`（1 个）

| # | 方法 | 路径 | 权限 | 响应 `data` |
|---|---|---|---|---|
| 17 | GET | `/` | Mgr+Clerk+Insp | `DeliveryDriverListOut` |

比 `GET /api/v2/prod/workers`（MANAGER-only）宽，但**只返「工种 = 送货司机」这一人群**，
每项 3 字段（`id` / `name` / `badge_code`），不泄露整张工人表（`prod::worker::WorkerOut` 带
`id_card_no` / `phone` 等 11 字段，本端点刻意不复用）。

### 1.4 路由注册顺序（**硬约束**，已升为 `ROUTES` 断言）

`matchit` 要求静态分支先于参数分支。`note_router()` 里 **3 段静态路径**
（`/batch-detail`、`/scan`、`/`）与 **1 段静态前缀**（`/scan/{serial_no}`）**必须先于
`/{id}` 注册**，否则 axum 在 nest 构建期直接 panic（不是运行期 404）。往任一
`xxx_router()` 加一条 `.route(...)` 而忘了登记对应的 `ROUTES` / `GROUP_ROUTES` /
`DRIVERS_ROUTES`，`routes_declared_in_router` 立刻红。

### 1.5 响应信封与 i64 约定

- 统一信封 `R<T> = { code: 0, message: "ok", data: T }`；出错时 `data: null`。
- **一切雪花 i64 一律 JSON `string`**（`shared::types::serialize_i64` / `serialize_i64_opt`）——
  超过 2^53，JSON number 在 JS 侧会丢精度。**计数类**（`part_count` / `version` /
  `quantity` / `entry_max_*` / `total` / `limit` / `offset`）是 JSON number。
- **入参 i64 走 `shared::types::deserialize_i64`**（只接受 JSON 字符串；发 number 会被 axum
  `JsonRejection` 拒成 HTTP 422 纯文本，不进 `R<T>` 信封）。

### 1.6 错误码（`214xx` 段全列 + 跨域共用码）

| 码 | 常量 | 本域触发点 | HTTP |
|---|---|---|---|
| 20101 | `BIZ_PART_NOT_FOUND` | `GET /scan/{serial_no}` 两表皆未命中 / trim 后为空 | 404 |
| 20102 | `BIZ_CUSTOMER_NOT_FOUND` | 扫码树 / 入单时锚点客户查不到 | 404 |
| 20104 | `BIZ_INVALID_VALUE` | 入参校验（`AppError::validation`） | 422 |
| 21401 | `BIZ_DELIVERY_NOTE_NOT_FOUND` | 任一 `/{id}` 端点单据不存在 / 已软删 | 404 |
| 21402 | `BIZ_DELIVERY_NOTE_INVALID_TRANSITION` | `update` 单据非 DRAFT/SUBMITTED；`/{id}/driver` 作用于已领取/已归档单 | 400 |
| 21403 | `BIZ_DELIVERY_NOTE_NOT_DRAFT` | `soft-delete` 单据非 DRAFT | 400 |
| 21404 | `BIZ_DELIVERY_NOTE_NOT_SUBMITTED` | `recall` / `pickup` 单据非 SUBMITTED | 400 |
| **21405** | `BIZ_DELIVERY_NOTE_PART_NOT_READY` | ★ 入单唯一允许的批次状态是 `READY_TO_SHIP`：批次状态不符**且**该零件可入单量凑不出本次要的量（作用域见 §4.1）/ DP 不可行 / `sets > entry_max_sets`；`submit` 单上有非 `READY_TO_SHIP` 批次；`pickup` 单上有非 `READY_TO_SHIP` 批次 | 400 |
| 21406 | `BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED` | 批次已被**其它**送货单占着（`reason` 两种字面量：「活跃单，货还在这张单上」/「已领取/已归档单，货已随该单送出，不可再次入单」）**且该零件可入单量凑不出本次要的量**（作用域与 `reason` 映射见 §4.1）；汇总处与 21405 二选一：失败 part 里有占用明细 ⇒ 21406，否则 21405 | 409 |
| **21407** | `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` | 零件的 L1 客户 ≠ 单据 L1 客户（同一请求混入别的 L1 的零件） | 400 |
| 21409 | `BIZ_DELIVERY_NOTE_DRIVER_INVALID` | `validate_driver` 5 条任一不过；`pickup` 单据未指定司机 | 400 |
| 21411 | `BIZ_DELIVERY_NOTE_INVALID_VALUE` | 空单提交 / 空单领取 | 400 |
| 21413 | `BIZ_DELIVERY_GROUP_NOT_FOUND` | `/group` 端点分组不存在 / 已软删 | 404 |
| 21414 | `BIZ_DELIVERY_GROUP_DUPLICATE_NAME` | 同 L1 下分组重名 | 409 |
| 21415 | `BIZ_DELIVERY_GROUP_MEMBER_CONFLICT` | L2 已属于其它活跃分组 | 409 |
| 21417 | `BIZ_DELIVERY_SCAN_UNKNOWN_CODE` | `POST /scan` 的 `serial_no` 两表皆未命中 | 404 |
| 21419 | `BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT` | `recall` 时该 L1 已有另一张 DRAFT | 409 |
| 21421 | `BIZ_DELIVERY_BATCH_STATE_INVALID` | `submit` 时单上批次被旁路改成 `READY_TO_SHIP` 之外的状态 | 400 |
| 40100 | `AUTH_*` | 未带 token / token 失效（中间件层） | 401 |
| 40300 | `FORBIDDEN` | 角色不足（含 `ShelfAccount` 访问本域任意端点） | 403 |
| 40901 | `VERSION_CONFLICT` | 任一写端点 OCC 不匹配 | 409 |

**已不使用的码**（随端点下线留空，勿复用）：`21408 BIZ_DELIVERY_NOTE_SCAN_MISMATCH`
（送货台扫码核销）、`21410 BIZ_DELIVERY_NOTE_SCAN_INCOMPLETE`、`21412
BIZ_DELIVERY_NOTE_PARTS_LOCKED`（`add-parts` 端点）、`21416
BIZ_DELIVERY_NOTE_SCOPE_MISMATCH`（范围判定）、`21418
BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY`、`21420 BIZ_DELIVERY_NOTE_LOCKED_PART`
（由 part / assembly 域守卫使用）。

> ⚠️ **规格偏差登记**：任务书 §4.2 的闸门表把「零件的 L1 客户 ≠ 单据 L1 客户」写成
> `21416 BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS`，但仓内 `21416` 是
> `BIZ_DELIVERY_NOTE_SCOPE_MISMATCH`（随范围判定一并下线），而
> `BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS` 的值是 **21407**。实现按**语义名**走
> **21407**，与删除前的 `add_parts_inner` 完全一致 ⇒ 前端的错误处理代码不用改。

## 2. 逐字段

### 2.1 `DeliveryNoteOut`（15 字段，list / detail head / 多数写端点的公共响应）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` | string | `t_delivery_note.id` |
| `version` | number | `t_delivery_note.version`（**OCC 锚**，前端回传） |
| `delivery_note_no` | string | `t_delivery_note.delivery_note_no`（`DN-YYYYMMDD-NNNN`） |
| `customer_id` | string | `t_delivery_note.customer_id`（**L1** id） |
| `customer_name` | string \| null | `CustomerRepo::list_by_ids` → `t_customer.name`（取不到 null） |
| `customer_path` | string \| null | `build_note_outs` 内存派生：`"{L1} / {L2}"`，L1 自指时只给 L1 名 |
| `status` | string | `t_delivery_note.status`（4 态，见 §5） |
| `submitted_at` | string \| null | `t_delivery_note.submitted_at` |
| `picked_up_at` | string \| null | `t_delivery_note.picked_up_at` |
| `driver_worker_name` | string \| null | `SELECT name FROM t_worker WHERE id = driver_worker_id AND deleted_at IS NULL` |
| `part_count` | number | `PartBatchRepo::list_by_delivery_note` 的行数 |
| `note` | string \| null | `t_delivery_note.note` |
| `delivery_date` | string \| null | `t_delivery_note.delivery_date`（`YYYY-MM-DD`） |
| `created_at` | string | `t_delivery_note.created_at` |

### 2.2 `DeliveryNoteDetailOut`（2 字段）

`head`（`#[serde(flatten)]` 的 `DeliveryNoteOut`，wire 上与 head 字段同层）+ `line_items`。

### 2.3 `DeliveryNoteLineItem`（27 字段，**行 = 批次**，`id` = `t_part_batch.id`）

| 字段 | 类型 | 后端 SQL 来源 |
|---|---|---|
| `id` | string | `t_part_batch.id`（行身份） |
| `part_id` | string | `t_part_batch.part_id` |
| `batch_no` | number | `t_part_batch.batch_no` |
| `delivery_seq` | number \| null ★ | `t_part_batch.delivery_seq` —— **加入本单的先后次序**，本单内从 1 起递增；`null` = 未挂单。`line_items[]` 的默认序即按它升序，见 §2.3.1 |
| `batch_label` | string | 派生：`"{serial_no}B{batch_no:02}"`；无序列号时 `"批次{batch_no}"` |
| `serial_no` | string | `t_part.serial_no`（可空 → `""`） |
| `drawing_no` | string | `t_part.drawing_no` |
| `name` | string | `t_part.name` |
| `quantity` | number | `t_part_batch.quantity`（**拆批后**是新批次的量） |
| `status` | string | `t_part_batch.status` |
| `applicant_name` | string \| null ★ | `t_part.applicant_name`；**装配件子件为空时回落 `t_assembly.applicant_name`**，见 §2.3.2。空串 → null |
| `request_date` | string | `t_part.request_date` |
| `planned_delivery_date` | string | `t_part.planned_delivery_date` |
| `system_delivery_date` | string \| null | `t_part.system_delivery_date` |
| `order_no` | string \| null | `t_part.order_no` |
| `note` | string \| null | `t_part.note` |
| `customer_name` | string \| null | 零件所属 L2 客户名 |
| `parent_customer_name` | string \| null | L2 客户的父客户名（取不到回落 L2 名） |
| `customer_path` | string \| null | `"{L1} / {L2}"`，L1 自指时只给 L2 名 |
| `customer_id` | string ★ | `t_part.customer_id`（**L2 叶子 id**，必填非空）。打印分组按它查 `t_delivery_group_member` 定位分组；**不得改用 `customer_name` 匹配**（`t_customer.name` 只有非唯一 btree 索引，同名 L2 会被并进同一张 sheet） |
| `assembly_id` | string \| null | `t_part.assembly_id`（散件 null） |
| `assembly_serial_no` | string \| null | `AssemblyRepo::list_by_ids(include_deleted=false)` |
| `assembly_drawing_no` | string \| null | 同上 |
| `assembly_name` | string \| null | 同上 |
| `assembly_order_no` | string \| null | 同上 |
| `assembly_quantity` | number \| null | `t_assembly.quantity`（工单总套数） |
| `shippable_sets` | number \| null | `service::shippable_sets` 公式，见 §4.2 |

**装配件被软删 / 不存在时**：`assembly_id` 仍有值（那是 `t_part` 上的列）但
`assembly_*` 与 `shippable_sets` 全为 `null`（`AssemblyRepo::list_by_ids(include_deleted=false)`
解析不到）—— 前端据此把子件当散件行展示。

#### 2.3.1 ★ `line_items[]` 默认序 = 加入本单的次序

单据详情 `GET /{id}` 与批量详情 `GET /batch-detail` 的 `line_items[]` 排序键是
`t_part_batch.delivery_seq ASC NULLS LAST, t_part_batch.id ASC`
（`PartBatchRepo::list_with_part_by_delivery_note` /
`list_with_part_by_delivery_note_ids`；批量详情逐单分桶保持 SQL 返回序 ⇒ 桶内同序）。
**不**再按 `t_part_batch.id` 单键排序。

| 判据 | 口径 |
|---|---|
| **为什么不用批次 id** | id 是**建批顺序**：只有拆批路径产生新批次 id（反映扫码时刻），整批直接挂单的批次用的是建批时的 id ⇒「先扫 A、后扫 B，但 A 的批次建得更早」会把 A 排到前面，与用户看到的扫码次序相反 |
| **`delivery_seq` 怎么来** | 挂单写点（`PartBatchRepo::attach_to_note`，扫码 Step 7 在一个事务里循环调用）赋 `COALESCE(MAX(delivery_seq),0)+1`（限本单）；READ COMMITTED 下同事务内前几次 UPDATE 对后续 `MAX()` 可见 ⇒ **同一批挂单动作内部**产出连续的 1,2,3…，但整单生命周期内的 seq 不保证连续（见「序号空洞」） |
| **并发安全** | 同一张单的并发扫码被 `POST /scan` Step 4 的 `note_version` OCC 串行化（草稿刚建出时是 Step 8 的 `note.version++` OCC），败者整事务回滚 ⇒ 同一单上不会有两条事务同时读到同一个 `MAX` |
| **摘单 / 单据软删** | 与 `delivery_note_id = NULL` 同一条 UPDATE 置 `delivery_seq = NULL`（`remove_batches` / `soft_delete`）⇒ 不变式 `delivery_seq IS NULL ⟺ delivery_note_id IS NULL` 恒成立；重新挂单时 seq 按新单重新计 |
| **序号空洞** | 本单内 seq **不重复**、但**可能不连续**，空洞不影响相对序（排序仍正确）：① 摘单只清被摘那行、**不重排**剩余行 ⇒ 摘掉 seq=2 之后本单是 1,3,4；② 回填的 `ROW_NUMBER()` 只按 `delivery_note_id IS NOT NULL` 过滤、不看 `deleted_at`，而展示查询按 `deleted_at IS NULL` 过滤 ⇒ 表里若有软删的批次行，它占的号在结果集里看不到。前端要行号用数组下标，不要拿 `delivery_seq` 推算 |
| **历史数据** | migration `20261010000000_001_add_batch_delivery_seq` 按 `ROW_NUMBER() OVER (PARTITION BY delivery_note_id ORDER BY id)` 回填 ⇒ 与上线前的 `id ASC` **逐行一致**，部署无视觉跳变 |
| **⚠️ 从备份恢复的库** | `scripts/restore_from_backup.sh` 默认 `REBUILD_SCHEMA=1` = `DROP SCHEMA` → `sqlx migrate run` → 灌 dump：回填 UPDATE 跑在**空表**上、dump 里也没有本列 ⇒ 恢复出来的库里**已挂单批次的 `delivery_seq` 全为 NULL**，`line_items[].delivery_seq` 也是 null。此时展示整体退回 `id ASC`（= 上线前行为，**不是故障**）；但该单**此后新挂**的行 seq 从 1 重新起（`MAX` 只看得到新写的非 NULL 行），而 `NULLS LAST` 让非 NULL 排在前面 ⇒ 新扫的批次显示在恢复出来的旧行**上面**。要让该库的行项序正确，只能按各单 `pb.id` 序手工回填一次 |
| **`NULLS LAST` + `id ASC` 的作用** | `NULLS LAST` 兜「`delivery_note_id` 有值而 seq 为 NULL」的脏值（外部改坏、或上面那条从备份恢复的库）；末尾 `id ASC` 兜同 seq / 同 NULL 时结果集仍确定 |
| **⚠️ 与 `sort_order` 无关** | 本仓既有 `sort_order`（iam 菜单 / 外协公司 / 外协工序能力清单）全是**配置显示序**（人工维护的静态排列）；本列是挂单时自动递增的**业务事实**，且只在「这一张送货单」的语境内有意义（同一批次先后挂过两张单时，seq 相对各自那张单重新计） |

`line_items[].delivery_seq` 走**普通 serde**（同 `batch_no`）：它是单内计数不是雪花 id，
不需要 `serialize_i64_opt`，也不 `skip_serializing_if` —— 字段恒定存在。

#### 2.3.2 ★ `applicant_name`：子件为空时回落装配件

取值优先级（实现唯一点：`com/delivery_note/service/line_item.rs::resolve_applicant_name`）：

| 序 | 判据 | 取值 |
|---|---|---|
| 1 | `t_part.applicant_name` 非空串 | 自己的申请人（无论该行是不是装配件子件） |
| 2 | 自己是装配件子件（`t_part.assembly_id` 非空）**且** 父装配件解析得到（`include_deleted=false`） | `t_assembly.applicant_name`（再判一次空串） |
| 3 | 都不成立 | `null`（前端渲染「—」） |

**为什么需要回落**：装配件**设计上**由父件向下继承申请人（`assembly` 域建单与
update 级联两条写路径都会写子件），但存量数据里这条继承**大量未落地** —— 开发库实测
490 条装配件子件中 **360 条 `t_part.applicant_name` 是空串**，而 90 个
`t_assembly.applicant_name` **无一为空**。行项只读子件列会把这些行渲染成「—」，
送货单打印模板 / 标签 xlsx 两处都据此渲染，等于整批单据看不到申请人。

**这不是漏字段**：`applicant_name` 在两条出参路径上恒定存在，前端三处渲染点
（详情列定义、标签工作簿、打印模板）都已在读，无需加字段、无需改前端契约。

**为什么零额外 DB 往返**：装配件父行字段（`assembly_serial_no` / `assembly_drawing_no` /
`assembly_name` / `assembly_order_no` / `assembly_quantity`）本来就已把装配件批查进
`assembly_map`，`TAssembly` 自带 `applicant_name` ⇒ 回落只是一次内存查表。

**为什么散件不回落**：`assembly_map` 只装「本单出现过的装配件」⇒ 散件行的装配件恒
`None`，第 2 条分支对散件天然不成立，不需要额外判据。

**⚠️ 只做读侧回落，不回填存量**：补写那 360 行等于凭空造一条「谁在什么时候把它同步给
子件」的审计轨迹。若日后要清洗存量，应作为独立的数据迁移立项（按 `t_part.assembly_id`
从父装配件回填），本口径不必跟着改 —— 回填后走的是第 1 条分支，值相同。

**两条路径同源**：单张详情 `GET /{id}` 与批量详情 `GET /batch-detail` 逐字共用
`line_item::build_line_item`，不存在「同一张单两条路径给不同申请人」的可能。

### 2.4 `DeliveryNoteListOut` / `BatchDeliveryDetailData`

前者 `{ items: DeliveryNoteOut[], total, limit, offset }`；后者 `{ items:
DeliveryNoteDetailOut[] }`（按入参 `ids` 顺序，缺失 id 静默跳过）。

### 2.5 `DeliveryGroupOut`（4 字段）/ `DeliveryGroupListOut`

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_delivery_group.id` |
| `name` | string | `t_delivery_group.name`（trim 后 1..=100） |
| `members` | array | `DeliveryGroupMemberOut[]` = `{ customer_id: string, customer_name: string }` |
| `version` | number | `t_delivery_group.version`（OCC 锚） |

列表出参 `{ groups: DeliveryGroupOut[], ungrouped_customers: { id, name }[] }`
（`ungrouped_customers` = 该 L1 名下**未入任何活跃分组**的 L2 客户）。

### 2.6 `DeliveryDriverOption`（3 字段）/ `DeliveryDriverListOut`

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_worker.id` —— 回传给 `POST /{id}/driver` 的 `driver_worker_id` |
| `name` | string | `t_worker.name` |
| `badge_code` | string | `t_worker.badge_code`（司机核销时手输的那串） |

出参 `{ items: DeliveryDriverOption[] }`，空时 `items: []`（**不是** `null`）。

### 2.7 ★ `DeliveryScanTreeOut` 三层树（端点 3）

```text
DeliveryScanTreeOut
├─ hit_kind: string                       "ASSEMBLY" | "PART"
├─ scanned_serial_no: string              trim 后的回显
├─ draft: DeliveryScanDraftOut | null     ★ 该 L1 现有的 DRAFT（无则 null，本端点绝不建单）
├─ assembly: DeliveryScanAssemblyOut | null
└─ children: Vec<DeliveryScanPartOut>     装配件树 = 全部子件；独立件树 = [该件]
   └─ children: Vec<DeliveryScanBatchOut>
```

#### `DeliveryScanDraftOut`（4 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `note_id` | string | `t_delivery_note.id` |
| `note_no` | string | `t_delivery_note.delivery_note_no` |
| `version` | number | `t_delivery_note.version` —— **OCC 锚**，前端原样回传给 `POST /scan` 的 `note_version` |
| `status` | string | `t_delivery_note.status` |

#### `DeliveryScanAssemblyOut`（13 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_assembly.id` |
| `serial_no` | string \| null | `t_assembly.serial_no` |
| `name` | string | `t_assembly.name` |
| `drawing_no` | string | `t_assembly.drawing_no` |
| `status` | string | `t_assembly.status`（7 态原文） |
| `quantity` | number | `t_assembly.quantity` = 工单总套数 |
| `is_urgent` | bool | `t_assembly.is_urgent` |
| `system_delivery_date` | string \| null | `t_assembly.system_delivery_date` |
| `customer_name` | string \| null | `LEFT JOIN t_customer`（软删客户 → null） |
| `customer_id` | string ★ | `t_assembly.customer_id`（L2） |
| `entry_max_sets` | number ★ | §4.2 公式，分子 = 子件的**可入单**批次 |
| `per_set_parts` | array ★ | `[{ part_id: string, per_set_quantity: number }]`，装配序沿用 `list_parts_by_assembly` 的源序 `serial_no ASC NULLS LAST, id ASC` |

⚠️ 装配件节点**没有批次**：`t_assembly` 在 `t_part_batch` 里没有行，批次只挂在
`DeliveryScanPartOut::children` 上。

#### `DeliveryScanPartOut`（13 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_part.id` |
| `serial_no` | string \| null | `t_part.serial_no` |
| `name` | string | `t_part.name` |
| `drawing_no` | string | `t_part.drawing_no` |
| `status` | string | `t_part.status`（8 态原文） |
| `quantity` | number | `t_part.quantity` = 工单总件数 |
| `is_urgent` | bool | `t_part.is_urgent` |
| `system_delivery_date` | string \| null | `t_part.system_delivery_date` |
| `customer_name` | string \| null | `LEFT JOIN t_customer` |
| `version` | number | `t_part.version` —— **仅展示**（写动作的 OCC 锚是批次 version） |
| `customer_id` | string ★ | `t_part.customer_id`（L2） |
| `entry_max_quantity` | number ★ | §4.1「可入单件数」 |
| `children` | array | `DeliveryScanBatchOut[]`，`batch_no ASC, id ASC` |

#### `DeliveryScanBatchOut`（11 字段）

| 字段 | 类型 | 来源 |
|---|---|---|
| `id` | string | `t_part_batch.id` |
| `batch_no` | number | `t_part_batch.batch_no` |
| `quantity` | number | `t_part_batch.quantity` |
| `status` | string | `t_part_batch.status` —— **后端不过滤不改写，状态闸门在前端** |
| `version` | number | `t_part_batch.version` —— **批次** OCC 锚 |
| `is_repairing` | bool | `t_part_batch.is_repairing` |
| `location` | string \| null | `t_part_batch.location` |
| `current_holder_display` | string \| null | `COALESCE(t_shelf.name, t_worker.name, t_outsource_company.name)` |
| `process_name` | string \| null | `LEFT JOIN t_process ON t_process.id = current_process_id` |
| `is_scanned` | bool | 内存派生：该批次所属零件 == 被扫中的那个零件（扫装配件码时全 false） |
| `occupied_by_note_no` | string \| null ★ | `LEFT JOIN t_delivery_note ON dn.id = b.delivery_note_id AND dn.deleted_at IS NULL` → `dn.delivery_note_no` |

## 3. 扫码树口径表

| 判据 | 口径 | 理由 |
|---|---|---|
| **命中顺序** | 先查 `t_part.serial_no`，未命中再查 `t_assembly.serial_no` | 序列号在两表都有值域（子件 `{asm}-{i:02d}` 与父件 `{prefix}{4 位}`），固定「先 part 后 assembly」与全仓扫码入口取数方向一致，少一个要解释的特例 |
| **未命中** | `20101 BIZ_PART_NOT_FOUND`（HTTP 404），trim 后为空同样按未命中 | 复用 part 域的码而不是另开：语义相同（扫到的东西不存在），前端按同一个码弹「未找到」 |
| **匹配方式** | `serial_no = $1` **精确匹配**，不做前缀 / `ILIKE` | 扫码枪给的是完整序列号，`F100` 命中 `F1001` 比不命中更糟 |
| **同序列号多行** | `ORDER BY (p.status = 'CANCELLED') ASC, p.id DESC LIMIT 1` | `uk_t_part_serial_no` 是**部分**唯一索引（`WHERE serial_no IS NOT NULL AND deleted_at IS NULL AND status <> 'CANCELLED'`），软删行与 `CANCELLED` 行可同号共存。`CANCELLED` 排最后（扫到历史废弃工单毫无意义，工单 cancel 后同号重建是常规操作），`id DESC` 兜底取新 |
| **只有 `CANCELLED` 行时** | 照样返回正常树，`part.status` 原文透出 `CANCELLED` | 排序键只保证「活跃行优先」，不保证「必有活跃行」。前端据此禁用写操作。刻意**不加** `AND status <> 'CANCELLED'` —— 那会让已取消工单的货再也扫不到 |
| **软删闸门** | `t_part` / `t_assembly` / `t_part_batch` 三表逐条 SQL 写死 `deleted_at IS NULL`；`LEFT JOIN t_customer` 也带 | 扫到软删行等于扫到业务上已不存在的码，前端据此弹「未找到」比弹一棵含已删数据的树更安全 |
| **父装配件软删** | 退化成独立件树（`assembly = null` + `children = [被扫中的那个]`） | 不返回「有子件但没有装配件节点」的孤儿树 |
| **子件列表** | `include_deleted=false`，排序 `serial_no ASC NULLS LAST, id ASC` | 子件序列号由父件序列号派生，该序即装配序；`NULLS LAST` 让没序列号的手工子件排末尾 |
| **批次层状态** | ★**不过滤**（含 `COMPLETED` / `CANCELLED` / `DELIVERED` 等终态） | 扫码弹窗要回答「这批货总共分了���批、每批现在什么状态」，砍掉终态就答不了；状态闸门在前端 |
| **`occupied_by_note_no`** | `null` = 未占用；非空 = 单号。JOIN 带 `dn.deleted_at IS NULL` | 被**软删**的单占用的批次视为未占用（可重新入单），与 `POST /scan` 的 `list_entryable_batches_by_part_ids` 同口径 |
| **`draft` 判定** | `l1_id = customer.parent_id.unwrap_or(customer.id)`，取 `find_open_draft_by_l1(l1_id)`；无则 `null` | **本端点纯读，绝不建单**。`null` 的含义是「这次扫码会新建一张草稿」，不是「扫码失败」 |
| **`children` 恒为数组** | 装配件无活跃子件时是 `[]` 不是 `null` | 少一层前端 `?? []` 判空 |
| **`per_set_quantity`** | `part.quantity / assembly.quantity`，**整数除法向零截断** | 与 `POST /scan` 的服务端重算同公式；前端不传 per_set 值，只用它展示 |
| **`assembly.quantity == 0`** | `per_set_parts` 返回 `[]`（不参与除法） | 避免除零；此时 `entry_max_sets` 恒 0 |
| **SQL 条数** | 独立件 5 条 / 装配件子件（父活跃）7 条 / 装配件子件（父软删）6 条 / 装配件条码 7 条 / 未命中 2 条 | 批次层**一条** `part_id = ANY($1)` 取回整棵树，零 N+1；`entry_max_*` 是**另一条** `list_entryable_batches_by_part_ids`。另有恒 2 条（`l1_of` → `CustomerRepo::get_by_id` + `note_find_open_draft_by_l1`）。⚠️ 这 5 个数字由单测 `com::delivery_note::sql_count_guard_tests` 钉死（禁循环内 SQL + 钉调用点重数 + 与 `repo/scan_tree.rs` 条数表对账），改任何一处必须三处同改 |

## 4. 入单与分配口径表

### 4.1 唯一入单入口与 `READY_TO_SHIP` 闸门

`POST /api/v2/com/delivery/note/scan` 是**唯一**入单入口。**入单只允许
`status = 'READY_TO_SHIP'` 且未占用的活跃批次**（`b.deleted_at IS NULL` ∧
`p.deleted_at IS NULL` ∧ `(b.delivery_note_id IS NULL OR dn.id IS NULL)`，其中 `dn` 的
JOIN 带 `deleted_at IS NULL`）—— 这条 SQL 是「可入单」的**唯一**定义
（`repo/scan_tree.rs::list_entryable_batches_by_part_ids`）。

| 状态 | 可入单？ | 报什么码 |
|---|---|---|
| `READY_TO_SHIP` + 未占用 | ✅ | — |
| `INSPECTION` | ❌ | **21405**（该零件可入单量不够时），message 带 `part / serial_no / batch_no / status` 明细 |
| `PENDING` / `PROGRAMMING` / `IN_PROCESS` / `DELIVERED` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` | ❌ | 21405（同上） |
| `READY_TO_SHIP` + 挂在**活跃单**（`DRAFT` / `SUBMITTED`）上 | ❌ | **21406**（该零件可入单量不够时），message 标「活跃单，货还在这张单上」 |
| `READY_TO_SHIP` + 挂在**已领取/已归档单**上 | ❌ | **21406**（该零件可入单量不够时），message 标「已领取/已归档单，货已随该单送出，不可再次入单」 |
| 任意状态 + 挂在**已软删**的单上 | 按状态判（软删单的占用视同未占用） | 状态不符且不够量 → 21405 |
| 任意状态 + 挂在**本单**上 | 幂等跳过 | — |

**状态闸门的判定作用域 = 「本次分配实际需要的量」**。上表左列判的是「这个批次本身能不能
被选中」，**不构成对整个请求的拒绝**：同一个零件只要 `READY_TO_SHIP` 的可入单量够本次要的量，
就正常入单，非 `READY_TO_SHIP` 的批次只是**不参与分配**。只有当该零件的 DP **凑不出本次
target** 时才报 21405，并在 message 里附上被拦下的批次明细。

**占用闸门的判定作用域 = 「本次分配实际需要的量」**。同理，被别的送货单占着的批次**只是不
参与分配**：同零件另有足量空闲 `READY_TO_SHIP` 批次时正常入单（用例
`occupied_batch_on_active_note_does_not_block_free_batch`、
`mixed_occupancy_part_enters_from_free_batch`）。只有 DP 凑不出本次 target 时才报 21406，并
附上被占用批次的单号与单据状态。

这条收敛**没有削弱 21406 的业务含义**，也不引入重复挂单风险：可入单集合由
`list_entryable_batches_by_part_ids` 的 SQL 定义（写死 `b.status = 'READY_TO_SHIP'` ∧
`(b.delivery_note_id IS NULL OR dn.id IS NULL)`），DP 的候选只来自它 ⇒ **DP 不可能选中占用
批次**。占用判定不承担数据一致性职责，它值得一句提醒，但作用域只能是「本次分配实际需要的量」。

**错误码二选一**：失败 part 里**只要有一条占用明细就是 21406**（409，`冲突`：货在别的单上，
要去那张单处理），否则 21405（400，`校验不过`）。判据的输入是**失败 part 集合**而非整张占用
明细表 —— 分配成功的 part 上有占用明细**不**参与选码，用例
`occupied_detail_on_successful_part_stays_21405` 钉死这一半。混合场景（有 part 是占用原因、
有 part 只是状态原因）取 21406 —— 那是更可执行的一侧，且 message 里两类明细都会列全。

**归因取舍**：占用批次**只**归占用桶，**不**按 status 二次归因。一个 `status = COMPLETED` 且
被 `PICKED_UP` 单占着的批次，报给用户时只说「已在送货单 X（已随该单送出）」，不附带
`status = COMPLETED` —— 「去那张单看」比「它完工了」更有行动指向，且库里绝大多数占用批次
确实是 `COMPLETED` / `DELIVERED`，若按状态归因 21406 几乎永远选不到。

| 失败形态 | message |
|---|---|
| **请求只含 1 个 part**，且它无状态明细（货就是不够） | 只回 DP 原文：「可入单件数不足：需要 N 件，候选批次合计 M 件」。**不**加「READY_TO_SHIP」字样 —— 那种场景说成状态问题是说错话 |
| **请求只含 1 个 part**，且它有状态明细 | DP 原文 + `；入单只允许 READY_TO_SHIP，以下批次不可用：part {id}（{serial_no}）批次 {no} status={status}；…` |
| **请求只含 1 个 part**，且它有占用明细 | DP 原文 + `；该零件另有批次已在其它送货单上、不可入单：part {id} 批次 {no} 已在送货单 {note_id}（{reason}）；…` |
| **请求只含 1 个 part**，占用明细与状态明细并存 | DP 原文 + 占用段 + 状态段（占用段在前） |
| **请求含 ≥ 2 个 part**（不论几个失败） | 每个失败 part 一段，按 `part_id` 升序：`part {id}（需 N 件）：{DP 文案}`，有占用明细 / 状态明细则依次追加对应明细段；同一段内的多条明细用 `；` 分隔 |

> 上表的**前缀维度**与**明细维度**是正交的两件事：`part_id` 前缀只取决于「本请求是否含多个
> part」，占用明细段与状态明细段只取决于「该失败 part 在 `occupied_by_part` /
> `not_ready_by_part` 里有没有条目」。所以「两个 part、只有 B 凑不出」得到的是**一段**带
> `part B（需 8 件）：…` 的文案，不是两句，也不是无主语的裸 DP 文案；两个 part 都凑不出则是
> **两段**、按 `part_id` 升序（用例
> `tests/com/delivery_note/entry_gate.rs::multi_part_failures_are_listed_in_part_id_order`
> 钉死）。段间分隔符统一是 `；`。两个明细段都**没有顶层前缀**、各带自己的引导语 ——
> 一个响应里两种失败原因可能并存，顶层前缀无法同时描述。
>
> 错误码与明细段的一致性由 `mixed_failure_reasons_pick_21406_and_list_both_details` 钉死：
> 一个请求里 A 全被 `PICKED_UP` 单占用、B 是状态原因 ⇒ 报 21406，message 同时列出 A 的占用
> 明细与 B 的 `status=IN_PROCESS`。

> ⚠️ **三桶必须穷尽**。批次挂在 `PICKED_UP` 单上时，它既不在「可入单」集合里（repo 按状态 +
> 占用筛）、也不算「活跃占用」、状态又是 `READY_TO_SHIP`（不进 `not_ready_by_part`）⇒ 若分类
> 循环漏了这一桶，会掉进 DP 的「凑不出」返回 **21405「可入单件数不足」**，语义完全错（用户
> 看到的是「货不够」，实际是「货已经送走了」）。

> ⚠️ **`INSPECTION` 必须被显式收进状态明细**。它不在可入单集合里，分类循环若不显式收集，
> 整条链路上没有任何一处会提到这些批次：DP 只能报「可入单件数不足」，用户看到的是「货不够」，
> 真实原因却是「还没品检完」⇒ 状态信息丢失，message 里也没有任何线索指向该去做品检。集成测试
> `tests/com/delivery_note/entry_gate.rs::inspection_batch_is_not_silently_reported_as_already_present`
> 钉死这条（断言 message 必须点名该批次）。

> ⚠️ **跨 part 原子性**：DP 循环收集完全部失败 part 才决定拒绝，拒绝点落在第一个拆批 / 挂单
> 写之前 ⇒ 「A part 分配成功 + B part 凑不出」时 A 的批次也**不挂单**（草稿行的 find-or-create
> 与它们同处 handler 开的那条事务，handler 只在 `Ok` 时 commit ⇒ 整体回滚）。用例
> `tests/com/delivery_note/entry_gate.rs::multi_part_shortage_writes_nothing_across_parts`
> 钉死的是**可观测**的那一半（零写入）；「拒绝点在第一个写之前」这条结构属性在 DB 上不可观测
> —— 把写挪进 DP 循环的写法与现写法在集成测试里同解。

### 4.2 套装数公式

```text
entryable_qty(c)  = Σ 该子件 c 的「可入单」批次 quantity      （Σ 为 0 时该子件贡献 0）
per_set(c)        = entryable_qty(c) * asm.quantity / c.quantity     （整数除法向零截断）
entry_max_sets(a) = LEAST( MIN( per_set(c) for c ∈ a 的**全部**未软删子件 ), a.quantity )
```

两个消费方**共用同一份实现**（`service/shippable_sets::shippable_sets`，宽类型适配壳
`note_shippable_sets` 供详情 VO 用）：

| 消费方 | 字段 | 分子 | 量的是 |
|---|---|---|---|
| 扫码三层树 `GET /scan/{serial_no}` | `assembly.entry_max_sets` | 「**可入单**」批次 | **还能再入多少套** |
| 送货单详情 `GET /{id}` | `line_items[].shippable_sets` | 「**本单上**」的 `READY_TO_SHIP` 批次 | **这一单能出多少套** |

⚠️ 二者**只在批次未占用时相等**；批次一旦被某张单挂上，树报 0（已无可入单的货）而该单详情
仍报它能出的套数。**这不是不一致，是两个不同的问题**（用例
`entry_max_sets_is_entryable_based_not_note_based` 钉住两个场景）。

`min` 的定义域是「该装配件的**全部**子件」，本单没交批次的子件按 0 参与 ⇒ 凑不齐整套就是 0 套
（业务原文：「剩余的部分不能单独发货，需要等待其他子零件收集齐组装为装配件出货」）。

**回归锁（用户给的例子）**：cap=3（F1001 送 3 套）；F1001-01 整单 9 件（批次 5 件
`READY_TO_SHIP` + 4 件 `IN_PROCESS`）、F1001-02 整单 6 件（4 件 `READY_TO_SHIP` + 2 件
`INSPECTION`）⇒ `per_set` = `5×3/9 = 1`（截断）与 `4×3/6 = 2` ⇒ **1 套**。分子不过滤状态时
会算出 **3 套**（错的）。单测：`only_ready_to_ship_batches_count_toward_sets`。

边界：`c.quantity == 0` 的子件跳过（不整除零、不拖累 min）；无子件参与 ⇒ 0 套；
`LEAST(..., asm.quantity)` 顺带收口子件超交。

### 4.3 DP 批次分配

两条业务原则：① **如无必要不拆批**（优先零拆批方案）；② **不与 ① 冲突时优先入单小批**
（零拆批方案有多个时取「升序**数量**序列字典序最小」的）。

| 例 | target | 候选数量 | 期望分配 |
|---|---:|---|---|
| 1 | 10 | `{2, 3, 9, 10}` | `[(10, 10)]` —— 零拆批解唯一 |
| 2 | 10 | `{2, 3, 5, 10}` | `[(2,2), (3,3), (5,5)]` —— 零拆批解有 `{10}` 与 `{2,3,5}`，取后者 |

算法：① 全体子集和 DP（后向可达表）；② 可达 ⇒ 回溯取字典序最小解（拆批数 0）；③ 不可达 ⇒
排除 quantity 最大的那个批次再跑一次 DP 得 `max_reachable`，差额从被排除的最大批次拆出
（**拆批数恒为 1**）；④ 差额不可行（target > Σ 全部数量）⇒ `21405`（该 part 另有占用明细时
为 21406，见 §4.1）。

**「最大」的定序**：`quantity DESC, batch_no ASC` —— 数量相同时取 batch_no 小的，保证同一输入
的分配结果稳定可复现。

**拆批语义**（照 `PartBatchRepo::split_batch`）：新批次 `quantity = qty`、**不继承**
`delivery_note_id`（随后由入单路径挂上）；源批次 `quantity -= qty`、**保持未挂单**。
装配件下**每个子件独立跑一次**（套数已由 `entry_max_sets` 的 min 定死，子件之间不耦合）⇒
无跨子件组合爆炸。

分配表的顺序：DP 路径（正常输入）按 `batch_id` 升序返回；**降级贪心 G1 路径按入参序**
（`quantity ASC, batch_no ASC`，见 §8.4 第 1 条）⇒ 前端**不要依赖返回顺序**，
按批次逐条处理即可。

### 4.4 与全局 `fetch_delivered_sets` 的有意分叉

`part::service::list_enrichment::fetch_delivered_sets`（**全局已送套数**）的分子带
`b.status IN ('DELIVERED','COMPLETED')` 过滤；本域两套口径都**只计 `READY_TO_SHIP`**：

| | `fetch_delivered_sets` | 本域 `shippable_sets` |
|---|---|---|
| 分子范围 | **全局**（该装配件历史上出过的所有批次） | 扫码树 = 全局可入单批次；详情 = 本单批次 |
| 分子状态过滤 | `DELIVERED` / `COMPLETED` | `READY_TO_SHIP` |
| 驱动表 | `t_part`（子件） | 同样以子件为定义域，但分子来自「可入单」或「本单」 |

三者是**两两不同**的口径，不是同一口径的两种实现。前端不要拿任何一个去对另一个。

### 4.5 建单判定键

`(customer_id, status='DRAFT', deleted_at IS NULL)` **单键**，由数据库部分唯一索引
`uk_t_delivery_note_l1_open_draft` 兜底。

- 「同一天」只是描述**默认行为**（新建时 `delivery_date` 默认 `today.date()`），**不是筛选
  条件** —— 日期是可编辑字段，用户改到明天后继续扫码应该加到同一张单。
- 一个 L1 同时只允许一张 DRAFT ⇒ `recall` 时若已有另一张 DRAFT，返回 `21419`（`recall` 的
  `UPDATE` 会撞唯一索引，故前置用业务码拒，给出可读原因）。
- `recall` 的冲突判据不需要「排除自己」：被 recall 的单当前是 `SUBMITTED`，不可能被这条查询
  命中。

## 5. 状态域约定（**无编译期保障**）

`DeliveryNoteStatus` 是 `varchar(16)` 的**字符串字面量**（DB 层无 CHECK 之外的类型保障），
迁移表在 `model.rs::DeliveryNoteStatus::can_transition_to`：

```text
DRAFT ──submit──▶ SUBMITTED ──pickup──▶ PICKED_UP ──▶ ARCHIVED
  ▲                   │
  └──────recall───────┘
```

| 当前状态 | 允许的写动作 |
|---|---|
| `DRAFT` | `update` / `remove-batches` / `driver` / `submit` / `soft-delete` / `scan`（入单） |
| `SUBMITTED` | `update` / `driver` / `recall` / `pickup`（**不**能再入单 / 移除批次 / 软删） |
| `PICKED_UP` | 只读；`driver` 被拒（`21402`） |
| `ARCHIVED` | 只读 |

**批次状态闸门**（各动作独立判定，不共用一处）：

| 动作 | 批次要求 |
|---|---|
| 入单（`POST /scan`） | 候选必须是 `READY_TO_SHIP` + 未占用 |
| 提交（`POST /{id}/submit`） | 单上**全部**批次必须 `READY_TO_SHIP`，否则 `21421` |
| 领取（`POST /{id}/pickup`） | 单上**全部**批次必须 `READY_TO_SHIP`，否则 `21405`；领取后全部翻 `DELIVERED` |
| 移除批次（`POST /{id}/remove-batches`） | 只清 `delivery_note_id`，不动 `status` / `quantity` |

## 6. 移除记录（2026-10-08）

### 6.1 端点（7 条删除 + 1 条改名 + 1 条路径硬切）

| 端点 | 原因 |
|---|---|
| `POST /api/v2/delivery-notes`（手动建单） | 入单收敛为扫码单一入口；建单只可能由 `scan_find_or_create_draft` 发生 |
| `POST /{id}/add-parts` | 同上（入单必须走 DP 分配，客户端不能直接指定批次） |
| `POST /{id}/attach-batches` | 同上（`POST /scan` 在同一事务内完成分配 + 挂单） |
| `GET /candidate-parts` | 候选取批列表与扫码树的信息重叠（树里已有 `entry_max_*` + 批次清单）；前端改为扫码 |
| `GET /pickup-pending` | 待司机领取一览与 `GET /` + `?statuses=SUBMITTED` 等价 |
| `GET /{id}/events` | 事件子系统下线（表已 DROP） |
| `POST /{id}/pickup-scan` | 送货台逐件扫码核销：后端不维护扫码状态（出参恒 `scanned_count=0 / ready=false`） |
| `POST /{id}/print` · `POST /{id}/print-labels` | 打印链路下线，xlsx 改由前端 **hucre 本地生成** |
| `POST /{id}/remove-parts` → **`remove-batches`** | 改名：行 = 批次，不是零件（原名会让人以为按 part_id 移除） |
| `GET /api/v2/delivery-notes/*` → `/api/v2/com/delivery/note/*` | 域平移，**硬切无 alias** |
| `GET /api/v2/delivery-groups/*` → `/api/v2/com/delivery/group/*` | 同上 |

### 6.2 字段（29 条删除）

**恒定值（4）**：`DeliveryNoteLineItem.is_urgent`（两处装配都硬编码 `false`）、
`.is_scanned`、`.scanned`（成对的「兼容字段」，两处都硬编码 `false`，前端两个都不读）、
`DeliveryNoteDetailOut.scanned_serials`（恒 `vec![]`）。

**前端零读（11）**：`DeliveryNoteOut.parent_customer_name`（前端统一读 `customer_path`）/
`.submitted_by` / `.picked_up_by`（只有 id 没有姓名，页面无从展示）/ `.driver_worker_id`
（用 `driver_worker_name` 判空即可）/ `.updated_at`；`DeliveryGroupOut.customer_id` /
`.created_at` / `.updated_at`；`vo/scan.rs` 的 `ScanDeliveryOut` 等 11 个类型（端点下线）。

**范围（5）**：`DeliveryNoteOut.delivery_group_id` / `.delivery_group_name` /
`.leaf_customer_id` / `.leaf_customer_name` / `.scope_label`。

**扫码入参**：`DeliveryNotePickupRequest.driver_worker_id`（改从单据读）。

裁剪判据两条：**前端仓零读** + **没有姓名佐证的裸 id 无法展示**。保留字段的共同点是「前端 Zod
schema 与页面都在用」。

### 6.3 类型 / 文件 / 表

| 删 | 说明 |
|---|---|
| `DeliveryNoteEvent` · `DeliveryNoteEventType` · `DeliveryNoteEventRepo` | 事件子系统下线。`Recalled` / `Archived` 两个枚举变体**从未被构造**（`recall()` 实际写的是 `WITHDRAWN`） |
| `DeliveryNoteOut` · `DeliveryNoteEventOut` | 事件读端点下线 |
| `SubmitDeliveryOut` · `SubmitOutcomeDto` · `vo/submit.rs` | 候选分流依赖「单上挂 `INSPECTION` 批次」，而入单只允许 `READY_TO_SHIP` ⇒ 该分支恒不可达。`POST /{id}/submit` 改为返回 `R<String>`（单据 id） |
| `UnresolvedTargetDto` · `AvailableBatchDto` · `AttachableBatchDto` · `AttachBatchesOut` · `AttachBatchConflict` | `attach-batches` 端点下线 |
| `DeliveryNoteCandidatePart` · `DeliveryNoteCandidatePartsOut` | `candidate-parts` 端点下线 |
| `DeliveryNotePickupScan{Request,Out}` · `DeliveryNotePickupListOut` | 送货台端点下线 |
| `DeliveryNoteCreateRequest` · `DeliveryNoteAddItem` · `DeliveryNoteAddPartsRequest` · `AttachBatch*` · `DeliveryNotePickupScanRequest` · `DeliveryNoteCandidatePartsQuery` · `DeliveryNotePickupPendingQuery` | 对应端点下线 |
| `NoteScope`（三态枚举 + `classify()` + `display_label()`） | 建单判定键改单键 |
| `DeliveryNoteRepoTrait::{event_add, event_list_by_note, note_find_open_draft_by_scope, group_list_active_groups_with_members_for_l1, note_list_for_pickup}` | 对应功能下线（trait 21 → 19 方法） |
| `repo/query.rs` · `repo/mutate.rs` | 重导出壳（2026-09-22 D-5 拆分期垫片），全仓零 caller，且一个别名指向已删的 `DeliveryNoteEventRepo` |
| `handler/print.rs`（227 行） | 打印链路下线 |
| `service/scan/{mod,classify,helpers,resolve_scan_kind,tests}.rs`（约 1100 行） | 旧 5 组状态分类（B/C 组概念）随入单重写整体下线 |
| `PyBackendClient::forward_delivery_note_print` / `forward_delivery_note_labels` | 同上 |
| **`t_delivery_note_event` 表** | 零外键 / 零其它读端点 / 零跨域引用 / 无软删列 / 无保留策略 ⇒ `DROP TABLE` |

### 6.4 `shippable_sets` 分子收窄（推翻 2026-10-04 review 第 3 轮 MINOR-4）

原模块 doc 明文禁止加状态过滤，理由是「那会让 DRAFT 单在 `INSPECTION` 阶段就打印出 0 套」。
**入单只允许 `READY_TO_SHIP` 让该理由失效** —— DRAFT 单上只可能有 `READY_TO_SHIP` 批次
（提交后翻 `DELIVERED`）⇒ 加过滤与不过滤在「本单批次集合」上结果相同。分子收窄为
`status == 'READY_TO_SHIP'`，与「可入单」定义同源，并对「挂单后被旁路改状态」的脏数据不再虚高
套数。

## 7. 与 WS 的关系

8 个 `DELIVERY_NOTE_*` kind 里，**本轮只有 4 个有写端点**（其余 4 个随端点下线失效，
`docs/api/dashboard.md` 的 kind 列表里仍是历史全集）。

| kind | 写端点 | payload |
|---|---|---|
| `DELIVERY_NOTE_SCAN_ADD` ★ | `POST /com/delivery/note/scan` | `{ delivery_note_id, delivery_note_no, line_count, version }` |
| `DELIVERY_NOTE_SUBMITTED` | `POST /com/delivery/note/{id}/submit` | `{ delivery_note_id, delivery_note_no }`（后者按 path id 的字符串形式给出，⚠️ 见 §8.4） |
| `DELIVERY_NOTE_DRIVER_SET` ★新增 | `POST /com/delivery/note/{id}/driver` | `{ delivery_note_id, delivery_note_no, driver_worker_id, driver_worker_name }` |
| `DELIVERY_NOTE_PICKED_UP` | `POST /com/delivery/note/{id}/pickup` | `{ delivery_note_id, delivery_note_no, part_count, driver_worker_id }` |
| ~~`DELIVERY_NOTE_CREATED`~~ | ~~`POST /`~~ | 端点下线 |
| ~~`DELIVERY_NOTE_PARTS_ADDED`~~ | ~~`POST /{id}/add-parts`~~ | 端点下线 |
| ~~`DELIVERY_NOTE_BATCHES_ATTACHED`~~ | ~~`POST /{id}/attach-batches`~~ | 端点下线 |

- 广播一律在 `tx.commit()` **之后**（handler 层）。
- 大屏侧的 `DELIVERY_STATUSES` 分桶与本域无关（读 `t_part_batch` 的 `DELIVERED` 批次数），
  见 `docs/api/dashboard.md`。

## 8. 表依赖与前端配套

### 8.1 读的表（本域 + 跨域只读）

| 表 | 用途 |
|---|---|
| `t_delivery_note` | 送货单本体（**唯一**本域业务表） |
| `t_delivery_group` / `t_delivery_group_member` | 送货分组（`/group` 4 端点） |
| `t_delivery_note_counter` | 单号日计数器（`next_delivery_note_no`） |
| `t_part` | 零件 / 装配件子件（扫码树、入单、详情行项） |
| `t_assembly` | 装配件（扫码树节点、套装数分子） |
| `t_part_batch` | 批次（入单分配、拆批、挂单、扫码树批次层）；`delivery_seq` 列承载行项默认序（§2.3.1），本域是它的**全部写点**（挂单赋值 / 摘单与单据软删置 NULL） |
| `t_customer` | L1 / L2 客户（`customer_path`、`L1_id` 推导、成员名） |
| `t_worker` | 指定司机（`driver_worker_name`、`GET /drivers`） |
| `t_work_type` | 工种（`送货司机` 判据） |
| `t_shelf` · `t_process` · `t_outsource_company` | 扫码树批次层的 **3 张 `LEFT JOIN` 展示用附表**（holder 名 / 工序名）；**刻意不加软删闸门**，与 `prod::batch::repo` 既有写法一致 |

`t_delivery_note.delivery_group_id` / `leaf_customer_id` 两列**逻辑废弃**（范围判定下线，
新单一律写 NULL）。保留列本身不动，避免 DDL 抖动与既有 baseline 约束
`ck_t_delivery_note_scope_exclusive` 的耦合。

### 8.2 跨域依赖登记

本域**整体不适用** `shared::domain_guard::assert_no_foreign_domain`：它合法依赖
`com::customer` / `part` / `assembly` / `prod::batch` / `prod::worker` /
`prod::work_type` 六个域（读 `t_customer` / `t_part` / `t_assembly` / `t_part_batch` /
`t_worker` / `t_work_type` / `t_shelf`），这些是送货单的**固有依赖**，不是「跨域偷跑」。

另外该探测器的触发点是根段字面量 `crate::modules::`（`shared/domain_guard.rs` 的
`ROOT_SEG` 拼接）：域平移后若写成 `crate::modules::com::delivery_note::…` 会被判为本域；
但同容器的 `crate::modules::com::customer::…` 会被 `first_divergence` 在第 2 段判为外域 ⇒
装在本域上只会产生恒失败的假闸门。故改用本节文档登记。

### 8.3 前端配套改动清单

| # | 改什么 | 落到哪 |
|---|---|---|
| 1 | 全部送货单 URL 硬切 | `src/api/` 的送货单 API 层一处 URL 常量：`/api/v2/delivery-notes/*` → `/api/v2/com/delivery/note/*`、`/api/v2/delivery-groups/*` → `/api/v2/com/delivery/group/*`（**无 alias，旧路径 404**） |
| 2 | 删「手动建单」表单 | `views/delivery/` 建单入口（`POST /` 已删） |
| 3 | 删「候选批次」弹窗 | `GET /candidate-parts` + `POST /{id}/add-parts` + `POST /{id}/attach-batches` 三条链路；改为扫码 → 扫码树弹窗 → 一次 `POST /scan` |
| 4 | 扫码入单改请求体 | `POST /scan` 的 body 从 `{ code }` 改成 `{ serial_no, note_version?, entries: [{ node_kind, node_id, sets?/quantity? }] }`；`node_id` 是 JSON **string** |
| 5 | 扫码入单改响应 | `ScanDeliveryOut` → `DeliveryNoteDetailOut`（含拆批后的 `line_items`，可**就地替换草稿卡**不用重新扫） |
| 6 | 新增扫码树 Zod schema | `GET /scan/{serial_no}`：5 层结构 + `draft` / `entry_max_sets` / `entry_max_quantity` / `occupied_by_note_no` 四个新增字段；`serial_no` 必须 `encodeURIComponent` |
| 7 | `INSPECTION` 批次不可入单 | 品检页「入单」按钮对非 `READY_TO_SHIP` 批次禁用；收到 21405 时 toast 走 message（带明细） |
| 8 | 「是否已指定司机」改读姓名 | `DeliveryNoteOut.driver_worker_id` 已删；用 `driver_worker_name` 判空（原打印对话框的「导出」按钮 disabled 逻辑） |
| 9 | 新增司机下拉 | `GET /api/v2/com/delivery/drivers` → `DeliveryDriverListOut`（3 字段 / 项）。建议独立 queryKey（改名单不失效单据缓存） |
| 10 | 领取请求瘦身 | `POST /{id}/pickup` body 只发 `{ version }`；先调 `POST /{id}/driver` 指定司机 |
| 11 | 提交返回类型 | `POST /{id}/submit` 的 `data` 从 `SubmitDeliveryOut` 改成 **string**（单据 id） |
| 12 | 移除批次路径 | `POST /{id}/remove-parts` → `POST /{id}/remove-batches` |
| 13 | 打印改本地生成 | 删 `POST /{id}/print` / `print-labels` 两个请求，xlsx 由前端 **hucre** 本地生成 |
| 14 | 分组权限 | `/group` 的 3 个写端点现在也接受 Inspector 角色（前端 `canEditGroup` 的角色白名单要加 Inspector） |
| 15 | VO 字段裁剪 | 从 Zod schema 里删 §6.2 的 29 个字段（多余字段若非 `.strict()` 会静默通过，删 schema 才是真删） |
| 16 | **删 `delivery_dispatch` 菜单页** | ⛔ 后端已下线该菜单（`seeds/menu.sql` §3.5 软删 + §4.1 / §4.3 白名单移除 + §4.6 回收 `role_menu`），而前端 `src/views/delivery-dispatch/DispatchNoteList.vue` 仍在。**该页依赖的恰好是本轮删掉的两条端点**（`GET /pickup-pending` / `POST /{id}/pickup-scan`）⇒ 不删就是「菜单能点、页面能开、每个请求都 404」的活条目。后端无 alias，这是前端必须同步删的一页 |
| 17 | `line_items[].customer_id` | ★ `DeliveryNoteLineItem` **新增** `customer_id: string`（L2 叶子 id，必填非空）。打印分组键从 `customer_name` 切成 `customer_id`：`t_customer.name` 只有**非唯一** btree 索引，同名 L2 会被并进同一张 sheet，而打印产物是客户签字的收货凭证。Zod schema 里加必填 `customer_id: z.string()` |
| 18 | `line_items[].delivery_seq` | ★ `DeliveryNoteLineItem` **新增** `delivery_seq: number \| null`（**加入本单的次序**，口径见 §2.3.1）。数组顺序已由后端按它排好，前端**零改动即得「按扫码先后」的展示**；Zod schema 里加 `delivery_seq: z.number().nullable()`，供页面按需展示序号列 |
| 19 | `applicant_name` 回落 | **零改动**（2026-10-10，§2.3.2）。字段名 / 类型 / 可空性都没动，只是「装配件子件自身为空时改用父装配件的值」。前端三处渲染点（详情列表列定义、标签 xlsx 工作簿、送货单打印模板）本就在读它 ⇒ 部署后立即见效，**不需要改 schema、不需要改列定义、不需要改模板** |

### 8.4 已知偏差登记

1. **DP 降级阈值**（`service/batch_allocation.rs`）：候选批次 `n > 100` 或 `target > 100_000`
   时降级为贪心 G1（升序逐批取满即停，末批取剩余量）。`O(n × target)` 的 DP 表在这两个阈值
   之上会吃掉几百 MB 内存。G1 **可能产生拆批**（牺牲原则 1），但不会算不出结果。正常业务
   （单零件十几批、单批几十件）远低于阈值。
2. **`prod::batch` 的 `"DRIVER"` 冲突**（**本轮不修**）：`prod::batch::service::scan` 用
   `work_type.code != Some("DRIVER")` 判司机，而本域用 `!= "送货司机"`。`POST /prod/batches/
   scan/deliver` 对真实司机**永远返 21409**。该「扫码发货」功能已计划移除、另案处理，改动它会
   牵动 batch 域的另 4 个端点与 `_e2e` 种子。
3. **`t_delivery_note.delivery_group_id` / `leaf_customer_id` 逻辑废弃**：列还在、约束还在
   （`ck_t_delivery_note_scope_exclusive`），但新单一律写 NULL、无任何查询使用。后续确认无
   历史查询依赖后可单独一条 migration 删列。
4. **端点总数是 17 而非任务书写的 18**：任务书 §7 的表里 `/note` 列了 14 行（含 2 条待删的
   打印端点），`/group` 4 行、`/drivers` 1 行，合计 19；删掉 2 条打印端点后 `/note` 是 12 行 ⇒
   12 + 4 + 1 = **17**。按「表里实际列出的路由」逐条实现。
5. **`DeliveryNoteLineItem.version` 未补（裁决 A）**：行项上**没有** `t_part_batch.version`
   字段，**刻意不补**。原消费者 `frontend/src/views/delivery/components/
   BatchInspectionConfirmDialog.vue`（读 `li.version`）随「过检路径」整体下线；新入单
   入口 `POST /scan` 的批次 OCC **由服务端在事务内读取当前 version 完成**
   （`split_batch` + 挂单都在同一事务里，客户端不需要回传）。⇒ **前端配套**：Zod
   schema 里如仍声明 `line_items[].version` 为必填，删掉；`removeBatches` 发的是
   `{ batch_ids, version }`，那里的 `version` 是**单据**版本（`DeliveryNoteOut.version`），
   不是批次版本，两者不要混。
6. **`DeliveryNoteLineItem.customer_id` 是新增必填字段**（2026-10-08）：后端两处装配
   （`inner.rs::get_with_parts` / `crud.rs::get_many_with_parts`）都已填。前端 schema
   同步加必填字段，见 §8.3 第 17 行。**不加 `skip_serializing_if`** —— 它是必填非空
   `i64`，恒发。
7. **`DELIVERY_NOTE_SUBMITTED` 的 `delivery_note_no` payload 是单据 id 的字符串**：写端点的
   出参已从 `SubmitDeliveryOut` 塌缩为 `String`（只有 id），handler 拿不到 `note_no` ⇒ 该
   payload 字段填的是 `path.id.to_string()`。前端只用 `delivery_note_id` 做 invalidate，无影响；
   若将来要真 no，需在 `submit` 里额外回读 `delivery_note_no`。
8. **装配件向下继承申请人：存量未落地，读侧已回落、数据未清洗**（2026-10-10，§2.3.2）。
   开发库实测 490 条装配件子件中 **360 条 `t_part.applicant_name` 是空串**、90 个
   `t_assembly.applicant_name` **无一为空** ⇒ 行项已能显示装配件的申请人（读侧回落），
   但 `t_part` 里那 360 行**仍是空串**，任何直接读该列的地方（零件一览 / 零件详情 /
   统计报表）仍会看到空。**本轮刻意不做数据迁移**：补写等于凭空造一条「谁在什么时候
   把它同步给子件」的审计轨迹，且回填范围与「当时父件申请人是什么」无从考证。若产品要
   清洗，应作为独立立项（按 `t_part.assembly_id` 从父装配件回填），届时行项口径不必改 ——
   回填后走的是「子件自身有值」那条分支，值与回落结果相同。