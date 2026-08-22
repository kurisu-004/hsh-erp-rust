# 送货单模块新版设计（扫码建单 + 规则分类 + 勾选打印标签）

> 对应 Python 实现：`myERP/api/v1/delivery_note.py`、`service/delivery_note.py`、
> `service/delivery_note_print.py`、`repository/delivery_note.py`、
> `statemachines/delivery_note.py`、`schema/delivery_note.py`。
> 本文档是 `src/modules/delivery_note/` 的实施蓝本，已定稿的决策见 §2。

## 1. 背景与目标

现状（Python v1）：品检员建送货单时要在候选零件列表（PartPickerDialog）里逐条
勾选；法拉电子（L1）旗下二厂/五厂/六厂要合印在一张送货单上，其余分厂各自单印，
目前靠人工挑件分单；装配件（同一 `t_assembly` 下的子件）打印时合并为一行
「1 套」；标签打印已支持勾选子集（`line_item_ids`，2026-08-07）。

新版目标（Rust v2 接管）：

1. **扫码建单**：品检员用扫码枪扫零件条码（图背 Code128，载荷 = `serial_no`）
   即可入单，无需查列表。
2. **装配件一扫入单**：扫总装图条码（`t_assembly.serial_no`）或任一子件条码
   （`{serial}-NN` 派生码），整套入单；**任一子件不可入单则整套拒绝**。
3. **规则分类**：按可配置的分组规则自动把零件分到不同送货单（二五六厂一张、
   其他分厂各自一张）。
4. **勾选打印标签**：`print-labels` 支持 `line_item_ids` 子集，不再全量打印
   （移植 Python 既有语义）。

非目标：司机领取流程（pickup-scan / pickup）保持不变，仅移植；图纸 PDF 打印
（`service/printing.py`）属于 part_file 域，不在本文范围。

## 2. 已定稿决策（2026-08-21 与用户确认）

| # | 决策点 | 结论 |
|---|---|---|
| D1 | 分类承载模型 | **送货单绑定分组**：`t_delivery_note` 增加「范围」属性（分组 或 单个二级客户），一单一范围 = 一份打印文件 = 一次司机领取。打印/领取逻辑零改动。 |
| D2 | 装配件部分子件不可入单 | **整套拒绝**，报错逐条列出不可入单的子件及原因；事务整体回滚。 |
| D3 | 扫码会话是否先选客户 | **免选客户自动路由**：扫码即解析所属 L1 → 分类 → find-or-create 草稿；响应携带落单信息供前端展示。 |

补充默认规则（设计自定，低风险）：

- D4：L1 未配置任何分组时，扫码落入**遗留 L1 全域草稿**（与现状一致，
  例如路达混合分厂印在同一张单上）。一旦配置了 ≥1 个分组即启用严格分类：
  组内 L2 → 分组单；组外 L2 → 各自的单厂单。
- D5：同范围的 DRAFT 全司共享一单（不是每人一单），代表「下一张发往该范围
  的送货单」；由 DB 部分唯一索引保证并发安全。
- D6：手工建单（非扫码）保留，可显式指定范围；范围缺失 = 遗留 L1 全域单
  （逃生舱，不强制分类）。

## 3. 领域模型

### 3.1 新实体：送货分组（DeliveryGroup）

按 L1 客户配置的具名分组，成员为该 L1 的直接子客户（L2）：

```
t_delivery_group         — 分组头：id, customer_id(L1), name, 审计/软删
t_delivery_group_member  — 成员：id, group_id, customer_id(L2), 软删
```

约束：

- `(customer_id, name)` 活跃内唯一（同 L1 下分组不重名）；
- `member.customer_id` 全局活跃唯一 —— 一个 L2 最多属于一个活跃分组；
- 成员必须是该 L1 的**直接子节点**（`parent_id = l1_id`，service 校验；
  与 Python 送货单的一级父链假设一致）。

初始数据（法拉电子）：分组「二五六厂」= {二厂， 五厂， 六厂}；一厂/三厂/母排厂等
不入组 → 各自单印。数据迁移不入迁移文件，上线后由文员在界面上配置。

### 3.2 送货单范围（NoteScope）

`t_delivery_note` 新增两列（均逻辑外键、可空）：

| 列 | 含义 |
|---|---|
| `delivery_group_id` | 非空 → 分组单；只收该组成员 L2 的件 |
| `leaf_customer_id` | 非空 → 单厂单；只收该 L2 的件 |

两列均 NULL = 遗留 L1 全域单（现状行为）。CHECK 约束两列不同时非空。

分类函数（纯函数，service 内 `#[cfg(test)]` 单测覆盖）：

```rust
pub enum NoteScope { L1Wide, Group(i64), Leaf(i64) }

fn classify(leaf_customer_id: i64, groups: &[GroupWithMembers]) -> NoteScope {
    if groups.is_empty() { return NoteScope::L1Wide; }              // D4
    match groups.iter().find(|g| g.member_ids.contains(&leaf_customer_id)) {
        Some(g) => NoteScope::Group(g.id),
        None    => NoteScope::Leaf(leaf_customer_id),               // 组外 L2 各自单印
    }
}
```

范围锚点：散件取 `part.customer_id`；装配件取 **`assembly.customer_id`**
（整套一个范围；子件即使客户不同也随装配体走 —— 前提是建装配件时子件与
装配体同客户，此为既有数据约定，本设计不另加校验）。

### 3.3 草稿唯一性与并发

- 分组单：`uq (customer_id, delivery_group_id) WHERE status='DRAFT'
  AND deleted_at IS NULL AND delivery_group_id IS NOT NULL`
- 单厂单：`uq (customer_id, leaf_customer_id) WHERE ... AND leaf_customer_id IS NOT NULL`
- L1 全域草稿**不加**唯一索引（避免限制手工建多单的现状）；扫码 find-or-create
  取「最早的活跃 L1 全域 DRAFT」（`ORDER BY id ASC LIMIT 1`），并发撞出重复
  草稿时后续扫码自然收敛到最早一单，多余空草稿人工软删即可。
- find-or-create 撞唯一索引（23505）→ 重新 SELECT 已存在的草稿继续，
  标准 upsert 模式。
- 边界：`recall`（SUBMITTED→DRAFT）时若同范围已存在另一张 DRAFT →
  409 `21419 BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT`，提示先处理现有草稿。

### 3.4 范围对入单的校验

`add_parts`（手工/扫码共用校验）在 Python 既有校验（批次状态 ∈
{INSPECTION, READY_TO_SHIP}、同 L1、不在其他 active 单）之上加范围校验：

- 分组单：`part.customer_id ∈ group.member_ids`（组被软删 → 21413，禁止再入单，
  已入单件可移除，打印/提交不受影响）；
- 单厂单：`part.customer_id == leaf_customer_id`；
- 全域单：沿用 Python 同 L1 校验，不加范围限制。

装配件扫码入单时子件**不逐件做范围校验**（随装配体锚点，见 §3.2）；
手工 add_parts 仍逐件校验（与 Python 行为对齐）。

## 4. 迁移 012（`20260821000001_012_create_delivery_group_note_scope.sql`）

```sql
-- t_delivery_group
CREATE TABLE public.t_delivery_group (
    id          bigint NOT NULL,
    customer_id bigint NOT NULL,               -- L1 root，逻辑外键
    name        character varying(100) NOT NULL,
    version     integer DEFAULT 0 NOT NULL,
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    updated_at  timestamp without time zone DEFAULT now() NOT NULL,
    updated_by  bigint,
    deleted_at  timestamp without time zone
);
ALTER TABLE ONLY public.t_delivery_group ADD CONSTRAINT t_delivery_group_pkey PRIMARY KEY (id);
CREATE INDEX ix_t_delivery_group_customer_id ON public.t_delivery_group USING btree (customer_id);
CREATE INDEX ix_t_delivery_group_deleted_at  ON public.t_delivery_group USING btree (deleted_at);
CREATE UNIQUE INDEX uq_t_delivery_group_name_active
    ON public.t_delivery_group USING btree (customer_id, name) WHERE (deleted_at IS NULL);

-- t_delivery_group_member
CREATE TABLE public.t_delivery_group_member (
    id          bigint NOT NULL,
    group_id    bigint NOT NULL,               -- 逻辑外键 → t_delivery_group.id
    customer_id bigint NOT NULL,               -- L2 叶子，逻辑外键 → t_customer.id
    created_at  timestamp without time zone DEFAULT now() NOT NULL,
    created_by  bigint,
    deleted_at  timestamp without time zone
);
ALTER TABLE ONLY public.t_delivery_group_member ADD CONSTRAINT t_delivery_group_member_pkey PRIMARY KEY (id);
CREATE INDEX ix_t_delivery_group_member_group_id    ON public.t_delivery_group_member USING btree (group_id);
CREATE INDEX ix_t_delivery_group_member_customer_id ON public.t_delivery_group_member USING btree (customer_id);
-- 一个 L2 最多属于一个活跃分组
CREATE UNIQUE INDEX uq_t_delivery_group_member_customer_active
    ON public.t_delivery_group_member USING btree (customer_id) WHERE (deleted_at IS NULL);

-- t_delivery_note 范围列
ALTER TABLE public.t_delivery_note
    ADD COLUMN delivery_group_id bigint,
    ADD COLUMN leaf_customer_id  bigint,
    ADD CONSTRAINT ck_t_delivery_note_scope_exclusive
        CHECK (NOT (delivery_group_id IS NOT NULL AND leaf_customer_id IS NOT NULL));

CREATE INDEX ix_t_delivery_note_delivery_group_id ON public.t_delivery_note USING btree (delivery_group_id)
    WHERE (delivery_group_id IS NOT NULL);
CREATE INDEX ix_t_delivery_note_leaf_customer_id ON public.t_delivery_note USING btree (leaf_customer_id)
    WHERE (leaf_customer_id IS NOT NULL);
-- 同范围活跃 DRAFT 唯一（find-or-create 并发兜底）
CREATE UNIQUE INDEX uq_t_delivery_note_draft_group ON public.t_delivery_note
    USING btree (customer_id, delivery_group_id)
    WHERE (deleted_at IS NULL AND status = 'DRAFT' AND delivery_group_id IS NOT NULL);
CREATE UNIQUE INDEX uq_t_delivery_note_draft_leaf ON public.t_delivery_note
    USING btree (customer_id, leaf_customer_id)
    WHERE (deleted_at IS NULL AND status = 'DRAFT' AND leaf_customer_id IS NOT NULL);
```

## 5. 扫码入单流程（核心新增）

`POST /api/v2/delivery-notes/scan`，角色 INSPECTOR / CLERK / MANAGER。
事务边界在 handler（`pool.begin()` → service → `commit()`）。

入参：`{ "code": "<扫码载荷>" }`（trim 后 1..=64；空 → 400）。

service `scan_add(tx, code, user)` 步骤：

1. **解析**（exact match，`deleted_at IS NULL`）：
   - `t_part.serial_no == code` 命中：
     - `part.assembly_id` 非空 → 按**装配件**处理（加载 assembly + 全部未删子件）；
     - 否则 → 散件，目标集 = [part]。
   - 未命中 → `t_assembly.serial_no == code` 命中 → 装配件（全部未删子件）。
   - 都未命中 → 404 `21417 BIZ_DELIVERY_SCAN_UNKNOWN_CODE`。
   - （序列号由同一 prefix 计数器发放，part/assembly 不会撞码，无歧义分支。）
2. **定位客户与分类**：锚点 leaf = `assembly.customer_id` 或 `part.customer_id`；
   L1 = `leaf.parent_id ?? leaf.id`；加载该 L1 的全部分组+成员 → `classify()`。
3. **find-or-create 草稿**（范围 + L1；创建时发放 `DN-YYYYMMDD-NNNN` 单号、
   `delivery_date = today`；撞唯一索引重查一次）。
4. **逐目标件评估批次**（`t_part_batch`，未删）：
   - 可入单：`status ∈ {INSPECTION, READY_TO_SHIP}` 且未挂在其他 active
     （DRAFT/SUBMITTED）单上；
   - 幂等：已挂在**本单** → 计入 `already_present`，不算冲突。
5. **原子性判定**：
   - 装配件：每个子件须有 ≥1 个可入单批次（或已在本单）；不满足的子件逐条
     收集 `{serial_no, name, reason}`（状态不符 / 已在送货单 DN-xxx 上）→
     400 `21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY`，message 含全部失败
     子件；**整单回滚**（D2）。
   - 散件：无可入单批次且不在本单 → 挂在其他 active 单 → 409 `21406`
     （附单号）；否则 → 400 `21405`（附当前状态）。
6. **入单**：可入单批次 `delivery_note_id = note.id`（整批，扫码不做部分量
   拆分；部分量仍走手工 add-parts）。
7. **响应**：

```jsonc
{
  "outcome": "ADDED",              // ADDED | ALREADY_PRESENT
  "resolved": {                     // 识别结果
    "kind": "ASSEMBLY",            // PART | ASSEMBLY
    "assembly_id": "…", "serial_no": "L1067",
    "drawing_no": "…", "name": "…",
    "child_count": 6
  },
  "note": {                         // 落在哪张单
    "id": "…", "delivery_note_no": "DN-20260821-0003",
    "version": 4, "status": "DRAFT",
    "scope_label": "二五六厂",       // 分组名 / L2 名 / L1 名
    "customer_path": "法拉电子 / 二厂",
    "line_count": 13
  },
  "added_batches":    [{ "batch_id": "…", "part_id": "…", "serial_no": "…", "quantity": 2 }],
  "already_present":  [{ "batch_id": "…", "serial_no": "…" }]
}
```

8. commit 后 `ws_hub.broadcast` 触发看板刷新（对齐 Python 延迟广播）。

并发与幂等设计要点：

- 扫码枪连扫同一件 → 第二次命中 `already_present`，返回 `ALREADY_PRESENT`
  （200，不报错），前端只提示不告警；
- 同一范围并发首扫 → 唯一索引 + 重查收敛（§3.3）；
- 扫码不自动推进批次状态（INSPECTION 件入单后仍由品检流程转
  READY_TO_SHIP；submit 时按 Python 语义统一校验 READY_TO_SHIP）。

## 6. API 一览（全部挂 `/api/v2`）

### 6.1 送货分组（新）

| 路由 | 角色 | 说明 |
|---|---|---|
| `GET /delivery-groups?customer_id=<L1>` | M/C/I | 分组列表（含成员 L2 id+name）+ 组外 L2 列表（`ungrouped_customers`） |
| `POST /delivery-groups` | M/C | 创建 `{customer_id, name, member_customer_ids[]}`（同 tx 写成员；L2 校验、重名 409 `21414`、成员已被占用 409 `21415`） |
| `POST /delivery-groups/{id}/update` | M/C | `{version, name?, member_customer_ids?}`；成员为**全量替换**语义（软删缺失 + 补插新增，同 tx） |
| `POST /delivery-groups/{id}/soft-delete` | M/C | `{version}`；连带软删成员。有 DRAFT 单引用时不拦（入单校验在 add 时报 21413） |

### 6.2 送货单（移植 + 扩展）

| 路由 | 角色 | 与 Python 的差异 |
|---|---|---|
| `GET /delivery-notes/candidate-parts` | M/C/I | 无（手工兜底保留） |
| `GET /delivery-notes` | M/C/I | 出参增 `delivery_group_id/name`、`leaf_customer_id/name`、`scope_label` |
| `GET /delivery-notes/pickup-pending` | 已登录 | 无 |
| `POST /delivery-notes` | M/C/I | 入参增可选 `delivery_group_id` / `leaf_customer_id`（校验归属；同时给 → 400） |
| `POST /delivery-notes/scan` | M/C/I | **新增**，见 §5 |
| `GET /delivery-notes/{id}` | 已登录 | 出参同列表增范围字段 |
| `GET /delivery-notes/{id}/events` | 已登录 | 无 |
| `POST /delivery-notes/{id}/update` | M/C/I | 无 |
| `POST /delivery-notes/{id}/add-parts` | M/C/I | 增范围校验（§3.4），违例 400 `21416` |
| `POST /delivery-notes/{id}/remove-parts` | M/C/I | 无 |
| `POST /delivery-notes/{id}/submit` | M/C/I | 无 |
| `POST /delivery-notes/{id}/recall` | M/C/I | 增 DRAFT 范围冲突检查（409 `21419`） |
| `POST /delivery-notes/{id}/pickup-scan` | 已登录 | 无 |
| `POST /delivery-notes/{id}/pickup` | 已登录 | 无 |
| `POST /delivery-notes/{id}/soft-delete` | M/C/I | 无 |
| `POST /delivery-notes/{id}/print` | M/C/I | Rust 重实现，语义不变（§8） |
| `POST /delivery-notes/{id}/print-labels` | M/C/I | 含 `line_item_ids` 勾选子集（§8） |

axum 静态段优先于参数段，`/scan`、`/candidate-parts`、`/pickup-pending`
无需像 FastAPI 那样靠注册顺序防 catch-all。

## 7. 状态机 / 事件 / 错误码

`statemachine.rs`：

```rust
pub enum DeliveryNoteStatus { Draft, Submitted, PickedUp, Archived }
// can_transition_to: Draft→Submitted, Submitted→Draft,
//                    Submitted→PickedUp, PickedUp→Archived
```

状态机不写 DB；`submit/recall/pickup` 的事件（SUBMITTED / WITHDRAWN /
PICKED_UP / ARCHIVED + CREATED）由 service 在事务内 insert，与 Python 对齐。
扫码入单**不写**噪音事件（沿用 Python 2026-07-23「ITEM_ADDED 不记录」决策）。

新错误码（214xx 段内顺延，`shared/error.rs` 常量 + `status_from_code` 表 +
两处表驱动单测同步补）：

| 码 | 常量 | HTTP | 含义 |
|---|---|---|---|
| 21413 | `BIZ_DELIVERY_GROUP_NOT_FOUND` | 404 | 分组不存在/已删 |
| 21414 | `BIZ_DELIVERY_GROUP_DUPLICATE_NAME` | 409 | 同 L1 下分组重名 |
| 21415 | `BIZ_DELIVERY_GROUP_MEMBER_CONFLICT` | 409 | L2 已属于其他分组 |
| 21416 | `BIZ_DELIVERY_NOTE_SCOPE_MISMATCH` | 400 | 零件分类与单范围不符 |
| 21417 | `BIZ_DELIVERY_SCAN_UNKNOWN_CODE` | 404 | 条码无法识别 |
| 21418 | `BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY` | 400 | 装配件整套拒绝（附失败子件清单） |
| 21419 | `BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT` | 409 | recall 时同范围已存在 DRAFT |

## 8. 打印移植（umya-spreadsheet）

移植 `service/delivery_note_print.py` 到 `src/modules/delivery_note/print.rs`，
行口径/校验/错误码全部对齐 Python：

- `PrintRow` / `CellBinding` / `PageSetupSpec` / `TemplateConfig` 结构照搬；
  `TEMPLATE_CONFIGS` 按 prefix `F`（法拉 A5 横 10 行/页）/ `L`（路达 A4 横
  25 行/页）静态表（`LazyLock<HashMap<char, TemplateConfig>>`）。
- 行构建：同 part 多批次折叠求和 → `custom_order` 投影（代表批次 id 校验，
  非法/漏行 422 `21113`）→ `line_item_ids` 子集（仅 labels；空数组 400、
  未知 id 422）→ 缺 serial/drawing 跳过（全缺 400）→ 装配件合并
  （`merge_assemblies` 默认 true，`merge_quantities` 按 assembly 覆盖套数，
  合并行单位「套」、散件「件」，组位置 = 组内最早批次位次）。
- 渲染：模板加载（`template/delivery_note_{fala,luda}.xlsx`）→ 基线列宽
  快照 → 页面设置（纸张/方向/fitToPage/边距/print_area）→ 超行数克隆
  sheet 分页 → `_fill_row` → footer 日期（法拉 A17 文案「送货日期：Y年M月D日」、
  路达 I2 日期对象）→ 行高/数据对齐/预算列宽/shrink_to_fit。
- 模板路径配置：`AppConfig.delivery_note_template_dir`（默认 `./template`），
  文件名约定 `delivery_note_{lower(prefix)}.xlsx`；prefix 未配置 →
  `21109 BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED`。
- CPU 密集渲染包 `tokio::task::spawn_blocking`；响应按 64KB chunk 流式
  （`Body::from_stream`），保留前端 `onDownloadProgress` 体验；文件名
  `{prefix}-YYYY-MM-DD-(note|label).xlsx`。

**风险与 spike（Phase 0 必做）**：umya-spreadsheet 对「克隆 Worksheet、合并
单元格写入、pageSetup/fitToPage、print_area、列宽/行高/对齐/shrinkToFit」
的保真度未验证。spike = 最小 bin 加载法拉模板 → 克隆 sheet 填 10 行 → 写出
后与 Python 产物在 Excel 中逐格比对（重点：打印区域、缩放、页脚合并区）。
若关键项失真 → 打印端点暂留 Python v1（v2 先行上线其余功能），或改
rust_xlsxwriter 从零复刻版式（成本高，最后手段）。

## 9. 模块落点（`src/modules/delivery_note/`）

| 文件 | 内容 |
|---|---|
| `model.rs` | `DeliveryNote` / `DeliveryNoteEvent` / `DeliveryGroup` / `DeliveryGroupMember`（`FromRow`）；枚举 `DeliveryNoteStatus` / `DeliveryNoteEventType` / `DeliveryNoteSortKey` / `NoteScope` / `ScanOutcome` |
| `dto.rs` | 移植 `schema/delivery_note.py` 全套 + `DeliveryGroupOut/ListOut/CreateReq/UpdateReq` + `ScanDeliveryReq/ScanDeliveryOut`；i64 → JSON string 用 `shared::types` serde helper |
| `repo.rs` | 本域表查询（note CRUD/list/pickup 列表、counter `INSERT … ON CONFLICT … RETURNING`、event append/list、group+member CRUD、find_draft_by_scope）；跨域读（part/batch/assembly/customer/worker/work_type）调各域 `repo` 公共函数 |
| `service.rs` | 生命周期（移植）+ `classify`（纯函数）+ `scan_add`（§5）+ 范围校验 |
| `print.rs` | §8 打印（新文件，对齐 Python 同名 service 拆分） |
| `statemachine.rs` | §7 状态机 |
| `handler.rs` / `mod.rs` | 路由（§6）；事务在 handler；commit 后 WS 广播 |

错误码改动落在 `src/shared/error.rs`（§7 表 + `status_from_code` + 两个
表驱动测试数组同步追加）。

**前置依赖**：part / assembly / customer / worker 域的 `model.rs` + `repo.rs`
需先就绪（架构路线图第 3→4 步）。本域需要的跨域只读点：
`part.get_by_serial / list_children(assembly_id) / list_by_ids`、
`part_batch.list_by_part_ids / list_by_delivery_note / get_by_id / update`、
`part_batch.split_batch`（手工 add-parts 部分量，移植 `_batch_ops`）、
`assembly.get_by_serial / get_by_id`、`customer.get_by_id / list_by_ids /
list_children`、`worker.get_by_id`、`work_type.get_by_id`。
若各域并行开发，先在这些域落最小只读 repo。

`Cargo.toml` 增补：`umya-spreadsheet`（打印）、`rust_decimal`（金额字段）、
`tokio-stream`（分块流式响应）；`calamine` 仅 dev-dependencies（测试回读 xlsx
校验单元格）。

## 10. 测试计划

单测（随源码 `#[cfg(test)]`）：

- `classify`：无分组 → L1Wide；组内 → Group；组外 → Leaf；L1 自身 → Leaf(L1)
- 打印纯函数：`_estimate_cell_width`（中文宽 2）、预算列宽分配（Σ ≤ 预算恒成立）、
  `M月D日` / 法拉 footer 文案、同 part 折叠求和、装配件合并行（位置/套数覆盖/
  散件保留）、`custom_order`/`line_item_ids` 校验分支

集成测试（`tests/delivery_note*.rs`，对齐 Python `tests/test_delivery_note*.py`
并新增扫码场景）：

- 分组 CRUD：建/改/删、重名 409、成员冲突 409、全量替换成员
- 扫码：散件入单 → 重扫幂等；未知码 404；非 READY/INSPECTION 400；挂在其他
  active 单 409（附单号）；装配件整入（含扫子件码路由）；装配件整套拒绝
  （消息含失败子件明细、DB 无残留）；组内 L2 → 分组单、组外 L2 → 单厂单、
  无分组 L1 → 全域草稿；同范围草稿复用（不重复建单）
- 生命周期移植：create/add/remove/submit/recall/pickup-scan/pickup/
  soft-delete/events，版本冲突 409、SUBMITTED 冻结 409、范围校验 21416、
  recall 撞草稿 21419
- 打印：`/print` 与 `/print-labels` 语义对齐 Python（用 calamine 回读断言
  关键单元格：合并行单位「套」、数量覆盖、勾选子集只出勾选行）

## 11. 分期实施路线

| 期 | 内容 | 出口标准 |
|---|---|---|
| P0 | umya-spreadsheet spike（§8） | 法拉/路达模板产物与 Python 逐格一致；失真则记录决策（打印暂留 v1） |
| P1 | 迁移 012 + model/枚举 + 错误码 + 分组 CRUD + classify | 分组 API 集成测试过；`cargo clippy` 净 |
| P2 | 送货单生命周期移植（含范围校验，不含打印/扫码） | Python `test_delivery_note.py` 对应场景全绿 |
| P3 | 扫码端点（自动路由 + 整套拒绝 + 幂等） | §10 扫码场景全绿 |
| P4 | 打印移植（note + labels + line_item_ids） | 与 Python 产物逐格比对一致；`test_delivery_note_print*` 对应场景全绿 |
| P5 | WS 广播 + 文档收尾（architecture.md 链接本文件） | 全量 `cargo test` 绿 |

P2–P4 期间 v1 打印继续可用，前端按端点灰度切换。
