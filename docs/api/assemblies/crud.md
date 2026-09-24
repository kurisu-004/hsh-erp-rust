# assembly 域 — CRUD

> 本文件须与 `src/modules/assembly/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（AssemblyOut / AssemblyListItem / AssemblyListOut / AssemblyChildOut / AssemblyFileRef / AssemblyDetail / AssemblyCreateResult）见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 7 个 CRUD 端点（list / create / get / update / soft-delete / **files-list（D-09）** / **children（D-07）**）+ start / files。cancel 见 [`./cancel.md`](./cancel.md)。
>
> 注：跨域端点 `GET /api/v2/parts/{part_id}/assembly`（D-08）虽挂在 parts 路由下但属于本域契约，文档收录在本文件末尾（仅文档归口，不改 frontend 路径）。

## 本文件目录


- [GET /api/v2/assemblies](#get-apiv2assemblies)
- [POST /api/v2/assemblies](#post-apiv2assemblies)
- [GET /api/v2/assemblies/{assembly_id}](#get-apiv2assembliesassembly_id)
- [POST /api/v2/assemblies/{assembly_id}/update](#post-apiv2assembliesassembly_idupdate)
- [POST /api/v2/assemblies/{assembly_id}/soft-delete](#post-apiv2assembliesassembly_idsoft-delete)
- [GET /api/v2/assemblies/{assembly_id}/files](#get-apiv2assembliesassembly_idfiles) （2026-09-25 D-09）
- [POST /api/v2/assemblies/{assembly_id}/children](#post-apiv2assembliesassembly_idchildren) （2026-09-25 D-07）
- [GET /api/v2/parts/{part_id}/assembly](#get-apiv2partspart_idassembly) （2026-09-25 D-08）

---

### `GET /api/v2/assemblies`

权限: **Manager / Clerk / Inspector / CncProgrammer**（service 内 `require_any_role`）

Query：

| 字段 | 类型 | 说明 |
|---|---|---|
| `customer_id` | string (i64)? | L1 → 自身 + 全部 L2 子节点；L2 → 仅自身；缺省不过滤 |
| `status` | string? | 单状态过滤（PENDING / IN_PROCESS / COMPLETED / CANCELLED） |
| `statuses` | string? | 多状态过滤，逗号分隔；与 `status` 同时传以 `statuses` 为准 |
| `is_urgent` | bool? | 紧急标记过滤 |
| `keyword` | string? | 模糊匹配 `name` / `drawing_no` / `serial_no`（ILIKE %kw%） |
| `sort_by` | string? | 白名单 `CREATED_AT` / `UPDATED_AT` / `DRAWING_NO` / `NAME`；其它退化为 `id` |
| `sort_dir` | string? | `ASC` / `DESC`（缺省 `DESC`） |
| `limit` | i64? | 1..=500（缺省 50） |
| `offset` | i64? | ≥ 0（缺省 0） |

Response 200 `data`：[`AssemblyListOut](./index.md#assemblylistout-字段)

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [AssemblyListItem](./index.md#assemblylistitem-字段)[] | 含 TAssembly 完整列 + `customer_name` / `parent_customer_name` 冗余 |
| `total` | i64 | 满足过滤的总数（与 `items` 解耦） |
| `limit` | i64 | 实际生效的 limit |
| `offset` | i64 | 实际生效的 offset |

错误码：40001（limit/offset 越界）、40300（角色不符）、50001（DB）。

### `POST /api/v2/assemblies`

权限: **Manager / Clerk**

Multipart body：

| 字段 | content-type | 必填 | 说明 |
|---|---|---|---|
| `data` | text/plain | ✓ | 文本字段，序列化的 `AssemblyCreateRequest` JSON |
| `files` | application/pdf | — | 可多个 PDF 二进制；**当前只处理首份**（与分支一致）做页数校验 |

**Multipart 严格校验**：

- 必须恰好含一个 `data` 字段（缺 / 多 / 其它字段名一律 40001）
- `data` 字段必须是合法 UTF-8 文本（无法解析为 JSON → 20104 INVALID_VALUE）

**`data` JSON 字段表**：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `drawing_no` | string | ✓ | 图号 |
| `name` | string | ✓ | 装配体名 |
| `applicant_name` | string? | — | 申请人 |
| `customer_id` | string (i64) | ✓ | **必须是 L2 叶子**（`parent_id NOT NULL`）；L1 / 不存在 → 20302 / 20102 |
| `request_date` | date? | — | 客户请求日 |
| `planned_delivery_date` | date? | — | 计划交付日 |
| `is_urgent` | bool? | — | 缺省 `false` |
| `quantity` | i32? | — | 缺省 `1` |
| `unit_price` | decimal? | — | 单价 |
| `total_price` | decimal? | — | 总价 |
| `order_no` | string? | — | 订单号 |
| `system_delivery_date` | date? | — | 系统派工日 |
| `note` | string? | — | 备注 |
| `children` | `AssemblyChildRequest`[] | — | 子件；≤ 99 个（超出 → 20303） |

**业务流转**：

1. 校验 `customer_id` 存在且为 L2 叶子（→ 20102 / 20302）
2. 子件数量 ≤ 99（→ 20303）
3. **若提供 PDF**：用 `lopdf::Document::load_mem` 解析首份；`page_count` 必须 == `children.len() + 1`（首页 + 每子件 1 页；不匹配 / 解析失败 → 20305）
4. **若提供 PDF**：从 L1 客户的 `serial_prefix` 派发序列号（无 prefix → 20308；序列号池耗尽 → 20105；prefix 未注册 → 20108）
5. INSERT `t_assembly`（`status='PENDING'`，`version=0`）
6. **若提供 PDF 且 serial 已派发**：为每个 child 按 `{asm_serial}-{i:02d}` 派生 `serial_no`，INSERT `t_part`（同事务）

**§3.1（2026-09-11）子件字段继承**：第 6 步 INSERT 子件时，子件从父件 `t_assembly` 继承以下字段（不在入参里也能正确建档）：

| 子件列 | 来源 |
|---|---|
| `applicant_name` | 父件 `applicant_name`（父空 → 子空串兜底） |
| `request_date` | 父件 `request_date` |
| `order_no` | 父件 `order_no` |
| `system_delivery_date` | 父件 `system_delivery_date` |
| `is_urgent` | 父件 `is_urgent` |
| `note` | 父件 `note` |
| `planned_delivery_date` | 子件入参优先；缺省继承父件 |
| `customer_id` / `quantity` / `serial_no` | 现状不变（customer 继承父件；quantity 为实际加工数；serial `{asm_serial}-{i:02d}`） |
| `unit_price` / `total_price` | 保持 0（本期不动价格语义） |

> 保留「有 PDF 才派 serial、才建子件」的门槛；不在本期放开。

WS 广播（commit 后下发）：

- `ASSEMBLY_CREATED` —— payload `{ assembly_id }`

Response 201 `data`：[`AssemblyCreateResult](./index.md#assemblycreateresult-字段) — 含刚 INSERT 的 assembly 行 + 创建的子件列表（无 PDF 时 `created_children` 为空数组）。

错误码：

- 20102 — `customer_id` 不存在（HTTP 404）
- 20104 — `data` JSON 解析失败 / `serial_prefix` 为空（HTTP 400）
- 20105 — 序列号池耗尽（HTTP 400）
- 20108 — `t_serial_counter` 找不到对应 prefix（HTTP 404）
- 20302 — `customer_id` 是 L1（集团节点，不允许作为装配体客户）（HTTP 400）
- 20303 — `children` 数量 > 99（HTTP 400）
- 20305 — PDF 页数与 `children.len()+1` 不匹配 / `lopdf` 解析失败（HTTP 400）
- 20308 — L1 客户的 `serial_prefix` 为空（HTTP 400）
- 40001 — multipart 字段错 / `data` 字段缺失（HTTP 422）
- 40300 — 角色不符（HTTP 403）

### `GET /api/v2/assemblies/{assembly_id}`

权限: **Manager / Clerk / Inspector / CncProgrammer**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Response 200 `data`：[`AssemblyDetail](./index.md#assemblydetail-字段)

| 字段 | 类型 | 说明 |
|---|---|---|
| `assembly` | [AssemblyOut](./index.md#assemblyout-字段) | TAssembly 完整 22 列 |
| `children` | [AssemblyChildOut](./index.md#assemblychildout-字段)[] | 该 assembly 下的 part 子件（`PartRepo::list_by_assembly_id`） |
| `files` | [AssemblyFileRef](./index.md#assemblyfileref-字段)[] | PDF 文件引用；**本 pass 始终为空数组**（与分支一致） |

错误码：

- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 40300 — 角色不符（HTTP 403）

### `POST /api/v2/assemblies/{assembly_id}/update`

权限: **Manager / Clerk**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Request：`AssemblyUpdateRequest` — 字段全部可选（缺省 = DB 不动）；`version` 必填（OCC）。

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁；与 DB 不匹配 → 40901 |
| `name` | string? | — | |
| `drawing_no` | string? | — | |
| `applicant_name` | string? | — | `None` = 不动；`Some("...")` = 覆盖（不支持三态 NULL clear） |
| `customer_id` | string (i64)? (三态) | — | `None` = 不动；`Some("...")` = 覆盖 + L2 校验（→ 20302 / 20102） |
| `request_date` | date? (三态) | — | `None` = 不动；`Some(null)` = 置 NULL；`Some("2026-08-27")` = 覆盖 |
| `planned_delivery_date` | date? (三态) | — | 同上 |
| `is_urgent` | bool? | — | |

> 2026-09-16 PR-2（migration 027）：`AssemblyUpdateRequest` 删 `actual_delivery_date`
> 入参 —— `t_assembly.actual_delivery_date` 列已删；实际交付日期由
> `t_part_event.event_type='DELIVERED'` 事件派生（子件批次交付事件体现）。
>
> 2026-09-17 PR-4：`request_date` / `planned_delivery_date` 在 DDL 是 NOT NULL
> （migration 005:20-21），DTO 沿用三态语义（`Option<Option<NaiveDate>>`）保留
> 兼容；service 层遇 `Some(None)` 改判 `20104 BIZ_INVALID_VALUE`（`t_assembly`
> 模型与 DDL 对齐后两字段已非 `Option<>`）。
| `quantity` | i32? | — | |
| `unit_price` | decimal? (三态) | — | `None` / `Some(null)` / `Some(0.5)` |
| `total_price` | decimal? (三态) | — | 同上 |
| `order_no` | string? | — | |
| `system_delivery_date` | date? (三态) | — | |
| `note` | string? | — | |

> **三态 nullable 字段语义**：`Option<Option<T>>`，`None` = 不更新，`Some(None)` = 置 NULL，`Some(Some(v))` = 覆盖。普通可空字段（`applicant_name` / `order_no` / `note`）保持 `Option<T>`，不支持三态 NULL clear（与 Python `applicant_name` 语义对齐）。

Response 200 `data`：[`AssemblyOut](./index.md#assemblyout-字段)

WS 广播（commit 后下发）：

- `ASSEMBLY_UPDATED` —— payload `{ assembly_id }`

**§3.2（2026-09-11）update 级联**：本端点成功后**同事务**内级联 `UPDATE t_part SET ... WHERE assembly_id=$aid AND deleted_at IS NULL`，把以下 8 个共享信息字段无条件覆盖为父件"更新后的当前行值"（`version += 1`，`updated_by = current.id`）：

| 子件列 | 来源 |
|---|---|
| `request_date` / `applicant_name` / `order_no` / `system_delivery_date` / `planned_delivery_date` / `is_urgent` / `note` / `customer_id` | 父件"更新后的当前行值" |

> - 2026-09-16 PR-2（migration 027）：`actual_delivery_date` 列已从 `t_assembly`
>   删除（与 `t_part` 同处理），级联子件集合保持 8 字段不变（PR-2 §
>   `assembly/service.rs:522` §3.2 注释）。
> - 排除 `quantity`：单独走 §3.3 缩放（见下）。
> - 实现上按"父件更新后的当前行值"覆盖，避免三态解析歧义；未变更字段被覆写为原值（语义无差）。
> - `customer_id` 变更时同样级联。

**§3.3（2026-09-11）套数缩放**：本端点入参 `quantity` 有值且 ≠ 父件现值时，触发缩放：

```
new_child_qty = max(1, round(child_qty * new_qty / old_qty))
```

> - `old_qty <= 0` 视为无缩放（防御，避免除零 / 反向缩放）。
> - 同事务 UPDATE 每个子件 `quantity`（`version += 1`）。
> - **不**追溯调整 `t_part_batch.quantity`（已拆分流转中的批次保持原量）。

错误码：

- 20102 — `customer_id` 不存在（HTTP 404）
- 20104 — `customer_id` 解析失败（HTTP 400）
- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 20302 — `customer_id` 不是 L2 叶子（HTTP 400）
- 40901 — version 不匹配 / 已软删（HTTP 409）
- 40001 — 字段 shape 错（HTTP 422）
- 40300 — 角色不符（HTTP 403）

### `POST /api/v2/assemblies/{assembly_id}/soft-delete`

权限: **Manager**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Request：`{ "version": i32 }`（OCC 必填）

Response 200 `data: null`（软删成功；commit 后 WS 广播 `ASSEMBLY_DELETED`）。

WS 广播（commit 后下发）：

- `ASSEMBLY_DELETED` —— payload `{ assembly_id }`

错误码：

- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 40901 — version 不匹配（HTTP 409）
- 40300 — 非 Manager（HTTP 403）

> 注：本 pass 的 `soft_delete` 仅校验 `version` + `deleted_at IS NULL`，未对终态（COMPLETED / CANCELLED）做禁删守卫（与分支一致）。后续 PR 可加 `20307 BIZ_ASSEMBLY_HAS_SHIPMENT` 校验。

---

### `GET /api/v2/assemblies/{assembly_id}/files`

权限: **Manager / Clerk / Inspector / CncProgrammer**

2026-09-25 新增（D-09 api-drift-fix）：列出装配体已上传的 PDF 文件（kind=ASSEMBLY_MASTER），与现有 `POST /{id}/files` 配套。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Response 200 `data`：[`PartFileListOut`](../files.md#partfilelistout-字段)

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | [PartFileOut](../files.md#partfileout-字段)[] | `owner_kind='ASSEMBLY'` + `kind='ASSEMBLY_MASTER'` 的文件 |
| `total` | i64 | 文件总数 |

> 不分页——单 owner 视图，按 `t_part_file.created_at DESC` 排序。

业务流转：service 层走 `AssemblyRepoTrait::list_part_files_by_owner('ASSEMBLY', asm.id)`，再在内存过滤 `kind='ASSEMBLY_MASTER'`（CAS 历史可能有其他 kind 但装配体视图只显示 ASSEMBLY_MASTER）。

错误码：

- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 40300 — 角色不符（HTTP 403）

---

### `POST /api/v2/assemblies/{assembly_id}/children`

权限: **Manager / Clerk**

2026-09-25 新增（D-07 api-drift-fix）：在已存在的装配体下追加单个 part 子件。子件继承父件 7 个共享信息字段（applicant_name / request_date / order_no / system_delivery_date / is_urgent / note / customer_id）；planned_delivery_date 子件入参优先，缺省继承父件。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Request：`AssemblyChildAddRequest` JSON

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `drawing_no` | string | ✓ | 子件图号（trim 后非空；空 → 40001） |
| `name` | string | ✓ | 子件名（trim 后非空；空 → 40001） |
| `planned_delivery_date` | date? | — | 缺省 → 继承父件 |
| `quantity` | i32 | ✓ | > 0（≤ 0 → 40001） |

业务流转（service 层）：

1. 校验 `drawing_no` / `name` 非空（→ 40001），`quantity > 0`（→ 40001）
2. 校验 assembly 存在（→ 20301）
3. 子件字段继承父件（7 个共享字段）；`customer_id` 强制为父件 customer（即便父件是 L2 也透传）
4. **不**派生 serial_no —— 已存在装配体追加子件属于「补件」语义，不打开序列号派发通道；新子件 `serial_no = NULL`（`uk_t_part_serial_no` 唯一索引允许多 NULL）
5. 同事务 INSERT `t_part` + INSERT 初始 `t_part_batch`（batch_no=1 / status='PENDING' / location=NULL），与 `PartService::create_part` 对齐
6. 读回 Part 行 → 渲染 `PartListItem`（含 `customer_name` / `l1_customer_name` 冗余）

WS 广播（commit 后下发）：

- `ASSEMBLY_UPDATED` —— payload `{ assembly_id }`

Response 200 `data`：[`PartListItem`](../parts/index.md#partlistitem-字段)

| 字段 | 类型 | 说明 |
|---|---|---|
| `part` | TPart 完整列 | 含 `serial_no=null` / `assembly_id=父件 id` |
| `customer_name` | string? | L2 客户名 |
| `l1_customer_name` | string? | L1（集团）客户名 |
| `location` / `holder_name` | null | 本端点不派生（避免引入批次 query） |

错误码：

- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 40001 — 字段 shape 错 / drawing_no 空 / name 空 / quantity ≤ 0（HTTP 422）
- 40300 — 角色不符（HTTP 403）

---

### `GET /api/v2/parts/{part_id}/assembly`

> 路由注册在 `part::router()`（`/api/v2/parts/{part_id}/assembly`），但属于 assembly 域契约。frontend API 客户端（`src/api/assembly.ts:30`）走此路径。

权限: **Manager / Clerk / Inspector / CncProgrammer**

2026-09-25 新增（D-08 api-drift-fix）：按 part 反查其所属装配体。响应 `R<Option<AssemblyDetail>>`——`null` 表示 part 不属于任何装配体（独立工单 / 单件 part）。

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 零件雪花 ID |

Response 200 `data`：`Option<AssemblyDetail>`

| 字段 | 类型 | 说明 |
|---|---|---|
| `Some(detail)` | [AssemblyDetail](#assemblydetail-字段) | 含 assembly 行 + children parts + files，与 `GET /assemblies/{id}` 同形 |
| `None` | null | part 存在但无父装配体（assembly_id IS NULL） |

业务流转（service 层走 `PartService::get_assembly_by_part`）：

1. `SELECT assembly_id FROM t_part WHERE id = $1 AND deleted_at IS NULL`
   - 行不存在 → 40401 PART_NOT_FOUND
2. `assembly_id IS NULL` → 返回 `Ok(None)`（独立 part）
3. 否则委托 `AssemblyService::get_assembly(asm_id)` 拿 AssemblyDetail（含 children + files）
   - 该 asm 已软删 → 20301（AssemblyService::get_assembly 内部校验）

错误码：

- 20101 — part 不存在 / 已软删（HTTP 404）
- 20301 — part 存在但其父装配体已软删（HTTP 404，传递自 AssemblyService::get_assembly）
- 40300 — 角色不符（HTTP 403）

> part 存在但 `assembly_id IS NULL` → 返回 `200 { data: null }`（不是 404）。

---

## CRUD 专属 DTO

### AssemblyCreateRequest 字段

见上文 [`POST /assemblies`](#post-apiv2assemblies) 字段表。

### AssemblyChildRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✓ | 子件名 |
| `drawing_no` | string? | — | 子件图号 |
| `planned_delivery_date` | date? | — | 子件计划交付日 |
| `quantity` | i32? | — | 缺省 `1` |

### AssemblyUpdateRequest 字段

见上文 [`POST /assemblies/{id}/update`](#post-apiv2assembliesassembly_idupdate) 字段表。注意 `customer_id` / `request_date` / `planned_delivery_date` / `unit_price` / `total_price` / `system_delivery_date` 是三态 `Option<Option<T>>`。
> 2026-09-16 PR-2：`actual_delivery_date` 已从 `AssemblyUpdateRequest` 删除（t_assembly 列已删）。

## Rust DTO 定义

```rust
// ---- 出参 ----

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub applicant_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    // 2026-09-17 PR-4：request_date / planned_delivery_date 与 DDL NOT NULL 对齐去 Option
    pub request_date: NaiveDate,
    pub planned_delivery_date: NaiveDate,
    // 2026-09-16 PR-2（migration 027）：删 `actual_delivery_date` —— 由
    // t_part_event.event_type='DELIVERED' 派生（PR-2 § assembly/dto.rs:141）。
    pub is_urgent: bool,
    pub status: String,                 // PENDING / IN_PROCESS / COMPLETED / CANCELLED
    pub version: i32,
    pub serial_no: Option<String>,      // 主装配体序列号
    pub quantity: i32,
    pub unit_price: Option<Decimal>,
    pub total_price: Option<Decimal>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListItem {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub customer_name: Option<String>,         // L2 名称
    pub parent_customer_name: Option<String>,  // L1（集团）名称
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListOut {
    pub items: Vec<AssemblyListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyChildOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,      // {asm_serial}-{i:02d}
    pub name: String,
    pub drawing_no: Option<String>,
    pub status: String,
    pub version: i32,
    pub quantity: i32,                  // 实际加工数；2026-09-11 起按父件套数等比缩放
    pub planned_delivery_date: Option<NaiveDate>,
    // §3.4 — 子件继承 / 级联字段（创建时 §3.1 继承父件；update 时 §3.2 级联）
    pub applicant_name: String,
    pub request_date: NaiveDate,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyFileRef {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub original_filename: String,
    pub page_count: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyDetail {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub children: Vec<AssemblyChildOut>,
    pub files: Vec<AssemblyFileRef>,    // 本 pass 始终空
}

#[derive(Debug, Clone, Serialize)]
pub struct AssemblyCreateResult {
    pub assembly: AssemblyOut,
    pub created_children: Vec<AssemblyChildOut>,
}

// ---- 入参 ----

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssemblyListQuery {
    #[serde(default)]
    pub customer_id: Option<String>,          // 雪花字符串
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyChildRequest {
    pub name: String,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    #[serde(default = "default_child_qty")]
    pub quantity: Option<i32>,                // 缺省 1
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssemblyCreateRequest {
    pub drawing_no: String,
    pub name: String,
    #[serde(default)]
    pub applicant_name: Option<String>,
    pub customer_id: String,                  // L2 叶子雪花字符串
    #[serde(default)]
    pub request_date: Option<NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<NaiveDate>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default = "default_qty")]
    pub quantity: Option<i32>,                // 缺省 1
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    #[serde(default)]
    pub total_price: Option<Decimal>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub children: Vec<AssemblyChildRequest>,  // ≤ 99
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssemblyUpdateRequest {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_str")]
    pub customer_id: Option<Option<String>>,         // 三态
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub request_date: Option<Option<NaiveDate>>,     // 三态
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub planned_delivery_date: Option<Option<NaiveDate>>,
    // 2026-09-16 PR-2（migration 027）：删 `actual_delivery_date` —— 由
    // t_part_event.event_type='DELIVERED' 派生（PR-2 § assembly/dto.rs:174）。
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub unit_price: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_decimal")]
    pub total_price: Option<Option<Decimal>>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_date")]
    pub system_delivery_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub note: Option<String>,
    pub version: i32,                                   // OCC 必填
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssemblyStatus {
    PENDING,
    IN_PROCESS,
    COMPLETED,
    CANCELLED,
}

impl AssemblyStatus {
    pub fn from_str(s: &str) -> Option<Self> { /* ... */ }
    pub fn as_str(&self) -> &'static str { /* ... */ }
    pub fn can_transition_to(self, to: Self) -> bool {
        use AssemblyStatus::*;
        matches!(
            (self, to),
            (PENDING, IN_PROCESS) | (PENDING, CANCELLED)
                | (IN_PROCESS, COMPLETED) | (IN_PROCESS, CANCELLED)
        )
    }
}
```
