# shelf ↔ process 工序映射 域 API

> 本文件须与 `src/modules/prod/shelf_process/{handler.rs,dto.rs,vo.rs,service.rs,repo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：`t_shelf_process` 多对多映射的读写（2026-10-02 自 `src/modules/shelf/process_mapping/`
> 搬入 prod 域）。货架 CRUD 见 [`../shelves.md`](../shelves.md)；工序 CRUD 见 [`./processes.md`](./processes.md)。

> **导航**：[`← index`](./index.md) · [`work-types`](./work-types.md) · [`processes`](./processes.md) · [`work-type-process-mapping`](./work-type-process-mapping.md) · [`process-chain`](./process-chain.md) · [`worker-pool`](./worker-pool.md) · **shelf-process-mapping**

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/shelf-processes` | 已登录（M/C/CNC/SHELF/INSPECTOR） | 所有 active shelf 的 mapping 批量查询（防 N+1） |
| GET | `/api/v2/prod/shelf-processes/{shelf_id}` | 已登录（M/C/CNC/SHELF/INSPECTOR）+ scope 校验 | 该货架的工序映射列表（按 sort_order） |
| POST | `/api/v2/prod/shelf-processes/{shelf_id}` | MANAGER | 整组替换 mapping（先软删全部旧 → INSERT 新） |

挂载点：`/api/v2/prod/shelf-processes`（见 `src/modules/prod/mod.rs::router`）。

> **2026-10-02 硬切（无 alias）**：旧路径 `GET /api/v2/shelves/processes`、
> `GET|POST /api/v2/shelves/{id}/processes` 已从 `src/modules/shelf/handler.rs` 彻底删除
> 并 404。**这 3 个端点本身**请求 / 响应契约逐字不变，前端只需改 URL（沿 2026-09-19
> prod 聚合先例）。⚠️ 但**整个后端 commit 的前端配套改动不止改 URL** —— 同 commit
> 删除的 `ShelfOut.account_count` 会打爆前端 Zod 必填字段。
>
> **状态（2026-10-02 已回填）**：后端改动已随合并
> 合入 `master`；前端配套已全部落在 `feat/shelf-domain-split`（3 commit，见下方
> [前端配套改动清单](#前端配套改动清单)的状态声明）。
>
> ⚠️ **留作一般性警示（对未来任何一次「后端加 / 删 `ShelfOut` 字段」都成立）**：
> `ShelfOut` 是**前端 Zod 守门的对象**（`shelfSchema`，`useProductionShelvesQuery` 对
> 每个响应做 `shelfListResultSchema.parse(...)`）。Zod 默认 strip 模式下**声明为必填
> 而后端不返**的字段会让 parse 每次抛 `ZodError` —— 不是降级展示、不是 `undefined`，
> 是整个货架列表 / 详情页面**硬失败**。所以后端改 `ShelfOut` 字段时，前端
> `shelfSchema`（`src/composables/queries/schemas.ts`）、`@/types/shelf::Shelf`、
> 展示列三处必须同一次改动里一起动。字段级同步清单见下方 B 节。
>
> ⚠️ 例外：`GET /shelves/processes` 现在落到 shelf 域的 `/{id}` 路由上，因 `processes`
> 非 i64 会被 axum 拒为 **400 纯文本**（非 `R` 信封），而非 404。
> 旧路径状态码由集成测试 `tests/production/shelf_process.rs::old_shelf_process_paths_are_gone`
> 按 URI 逐个钉死。

---

## 业务模型

`t_shelf_process` 记录「某货架可执行哪些工序」及其显示顺序，是 worker-scan 返库、
prod 批次下发解析货架、worker-pool 移动校验三处的共同依据。

- **逻辑引用**：`shelf_id` / `process_id` 为 bigint 逻辑引用，DB 层无 FK 约束
- **软删**：mapping 行有 `deleted_at`；整组替换时先 `UPDATE … SET deleted_at = now()`，
  再 `bulk_insert` 新行（历史行保留，便于追溯）
- **读守卫**：所有读查询一律带 `deleted_at IS NULL`；全集查询额外要求 `s.is_active = true`；
  **`find_first_shelf_for_process`（2026-10-04）额外要求 `s.is_active = true` 且
  `s.zone = 'PRODUCTION'`** —— 它是三条读侧里**唯一从映射里选出**货架的（另两条
  `worker_scan` RETURNED 与 `/prod/pool/move` WORKER→POOL 的 `shelf_id` 来自请求，各有
  独立守卫），谓词不全即静默漏件（见下方「只读诊断 SQL」）
- **写守卫**：`POST` 侧自 2026-10-04 起要求目标货架 `zone='PRODUCTION'`（`items: []`
  清空路径豁免，见下方「zone 守卫」）
- **依赖方向（2026-10-02 翻转）**：本域只**读** `shelf::repo::ShelfRepo::get_by_id`
  校验货架存在 / scope（prod → shelf）；反向的 shelf → prod 依赖已随端点搬移清零。
  2026-10-04 起额外调用**同域** `prod::batch::service::guard::validate_shelf_zone`
  （判序与错误码的唯一真源）

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| `GET /` | 任意已登录；SHELF_ACCOUNT 按 `user.shelf_ids` 收窄；按 `shelf_id ASC, sort_order ASC` |
| `GET /{shelf_id}` | 货架不存在 → 20501；SHELF_ACCOUNT 越界 → 40301；按 `sort_order ASC, id ASC` |
| `POST /{shelf_id}` | 货架不存在 → 20501（**任何** items 都守）；**`items` 非空时**货架 `is_active=false` → 20512、货架 `zone≠'PRODUCTION'` → 20104（2026-10-04 新增；`items: []` 清空路径豁免这两条）；items 里有 process_id 不存在或已软删 → 20505；整组替换语义，`items: []` = 清空 |

---

### `GET /api/v2/prod/shelf-processes`

权限：已登录（M/C/CNC/SHELF/INSPECTOR；service 层 `require_any_role`）

Response 200 `data`：`AllShelfProcessMappingOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].shelf_id` | string (i64) | |
| `items[].shelf_code` | string | |
| `items[].process_id` | string (i64) | |
| `items[].process_code` | string | |

业务规则：

- 单条 JOIN 返回所有 `is_active=true` 且未软删的 shelf ↔ process 行（防 N+1）
- SHELF_ACCOUNT 用户仅看到 `user.shelf_ids` 命中的映射
- 用途：part_batch / worker_pool 创建批次/工人时一次性拿全货架工序映射
- 本端点**不含** `sort_order` 字段（与 per-shelf 端点不同）

### `GET /api/v2/prod/shelf-processes/{shelf_id}`

权限：已登录（M/C/CNC/SHELF/INSPECTOR）+ SHELF_ACCOUNT scope 校验

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `shelf_id` | string (i64) | 货架 ID |

Response 200 `data`：`ShelfProcessMappingOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].shelf_id` | string (i64) | |
| `items[].shelf_code` | string | |
| `items[].process_id` | string (i64) | |
| `items[].process_code` | string | |
| `items[].sort_order` | i32 | |

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND`
- 40301 `SHELF_MISMATCH` — SHELF_ACCOUNT 越界

### `POST /api/v2/prod/shelf-processes/{shelf_id}`

权限：MANAGER

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `shelf_id` | string (i64) | 货架 ID |

Request：`SetShelfProcessesRequest`

```jsonc
{
  "items": [
    { "process_id": "1001", "sort_order": 0 },
    { "process_id": "1002", "sort_order": 1 }
  ]
}
```

语义：整组替换 —— 事务内：

1. 校验 shelf 存在（20501）
2. **`items` 非空时**额外校验 `is_active=true`（20512）、`zone='PRODUCTION'`（20104）
3. 校验 items 里所有 process_id 存在且未软删（20505）
4. 软删该 shelf 的全部 active mapping（清 `deleted_at`）
5. `bulk_insert` 新 mapping（带 sort_order）

`items` 可为 `[]`（清空映射）。⚠️ **2026-10-04 review 第 1 轮 B1：`items: []` 豁免第 2 步
的 20512 / 20104**（只守存在性 20501）—— 见下方「zone 守卫」的「清空路径豁免」。

Response 200 `data`：`null`

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND` —— shelf 不存在 / 已软删（**任何** `items` 都守）
- 20512 `BIZ_SHELF_INACTIVE` —— shelf `is_active=false`（2026-10-04 新增；`items: []` 不触发）
- 20104 `BIZ_INVALID_VALUE` —— shelf `zone≠'PRODUCTION'`（2026-10-04 新增；`items: []` 不触发）/ process_id 非整数
- 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在

### zone 守卫（2026-10-04 新增，写侧收紧）

`POST` 建 / 改映射时只接受 **PRODUCTION 区**货架。判序由
`prod::batch::service::guard::validate_shelf_zone` 统一承担（`ShelfRepo::get_by_id` 带
`deleted_at IS NULL`），与 `place_on_shelf` / `pickup` / `release_from_programming` /
外协收发等生产流端点**同源同码**（20501 → 20512 → 20104），不另造判定。

**为什么要收紧**：`t_shelf_process` 的三条读侧全是生产流（下发解析货架 / worker 归还 /
候选池放回）。其中只有下发（`dispatch_single`）是**从映射里选出**货架，故它的谓词必须
下沉到 `find_first_shelf_for_process` 的 SQL；另两条的 `shelf_id` 来自请求、写进
`t_part_batch.current_holder_id` 之前各有独立守卫（`worker_scan` 用
`ShelfRepo::get_by_id_zone(.., "PRODUCTION")`、`/prod/pool/move` 用 `validate_shelf_zone`）
—— 全仓 3 个 `current_holder_id` 写点已全覆盖，清单见
`src/modules/prod/batch/service/guard.rs` 模块 doc。品检流走的是显式
`target_inspection_shelf_id` + `validate_shelf_zone(.., "INSPECTION")`，**不读
`t_shelf_process`**；前端 10 处 `useShelfProcessFilter` 的货架候选源也一律是
`zone='PRODUCTION'` 过滤后的列表。品检架上的映射行因此是**无读侧消费**的纯负债。

**清空路径豁免（2026-10-04 review 第 1 轮 B1）**：`items: []` 只 `soft_delete_all_for_shelf`、
**不新增任何非法映射**，故跳过 20512 / 20104（存在性 20501 仍无条件守 —— 对已软删货架的
空操作不该静默「成功」）。不豁免的话，**存量非法映射将失去唯一的 API 清理路径**：
整组替换语义下改不了其中一条，而 `PUT /shelves/{id}` 的 `ShelfUpdateRequest` 只有 `name` /
`location`（`zone` 不可经 API 改），也没有「先换成生产架再清映射」这条绕路 —— 于是
「只读诊断 SQL ② 列出的行在本仓清不掉」，诊断与处置自相矛盾。
回归：`tests/production/shelf_process.rs::set_shelf_processes_allows_clearing_inspection_zone_mappings`。

**副作用（已知且接受）**：对品检架（或任何非 PRODUCTION 区货架）调本端点且 `items`
**非空**时返 `20104`。这是刻意的 fail-fast：让配置错误在写侧暴露，而不是继续静默产出
漏件批次。存量非法行的排查 SQL（只读）见下方「只读诊断 SQL」，**本仓不自动修数据**，
修复走独立的数据修复单 + 上面的清空路径。

**⚠️ 前端配套未完成（2026-10-04 review 第 1 轮 B1，待前端仓处理）**
`ShelfList.vue::saveShelf` 在**编辑态无条件**调 `setShelfProcesses`，且 `updateShelf`
先于它执行 ⇒ 改品检架的名称 / 物理顺序时，基本字段已落库、映射这一步返 `20104`、弹窗不关、
toast 是后端原文 `shelf {code} (id={id}) zone=INSPECTION 不等于 PRODUCTION` —— **半截保存**。
前端需两处配套：① `shelfForm.zone !== 'PRODUCTION'` 时隐藏工序多选（读侧已是这个口径）；
② 编辑态对非 PRODUCTION 区货架跳过 `setShelfProcesses` 调用。
在该配套落地前，**后端先上、库存管理页对品检架的编辑会失败**（清空路径已豁免，故清空动作
本身仍可用）。

---

## 只读诊断 SQL（2026-10-04 新增）

> ⚠️ **只读诊断，非迁移**。本节两条 SQL **不含任何 `UPDATE` / `DELETE` / `INSERT`**，
> 只作排查用。仓库既有的可执行 SQL 惯例是 `migrations/`（schema）与 `seeds/`（配置
> 数据）两个目录 —— 二者都是**会被应用**的变更脚本，把只读排查语句塞进去会与之混淆，
> 故本节落在文档里。需要执行时直接贴进 `psql` / 客户端跑即可。
>
> **用途**：`current_holder_id` 写脏的**后果是静默漏件而不是报错**。报工台取件页的
> 数据源（`GET /api/v2/parts/pickable-by-work-type/{work_type_id}`）取行 SQL 硬限定
> ```sql
> JOIN t_shelf sh ON sh.id = b.current_holder_id
> ... AND sh.is_active = true AND sh.zone = 'PRODUCTION'
> ```
> 所以一个 `location='PRODUCTION_SHELF'` 但 `current_holder_id` 指向品检架 / 停用架 /
> 已软删架的批次，**永远不会出现在工人的可领列表里，也不报任何错**。下面第 1 条把这类
> 批次全部列出来，第 2 条把「会让脏货架落进 holder」的非法映射源头列出来。

### ① `current_holder_id` 指向不可用货架的在池批次

```sql
SELECT b.id, b.part_id, b.current_holder_id, sh.code, sh.zone, sh.is_active, sh.deleted_at
FROM t_part_batch b LEFT JOIN t_shelf sh ON sh.id = b.current_holder_id
WHERE b.location = 'PRODUCTION_SHELF' AND b.status = 'IN_PROCESS' AND b.deleted_at IS NULL
  AND (sh.id IS NULL OR sh.zone <> 'PRODUCTION' OR NOT sh.is_active OR sh.deleted_at IS NOT NULL);
```

谓词逐条对应 `validate_shelf_zone` 的三个判据（`sh.id IS NULL` 覆盖悬空 holder；
`sh.zone <> 'PRODUCTION'` / `NOT sh.is_active` / `sh.deleted_at IS NOT NULL`），
`LEFT JOIN` 保证 holder 指向已消失的货架时也能命中。

### ② 非法 shelf ↔ process 映射（脏 holder 的源头）

```sql
SELECT sp.id AS mapping_id, sp.shelf_id, sh.code AS shelf_code, sh.zone, sh.is_active,
       sh.deleted_at AS shelf_deleted_at, sp.process_id, p.code AS process_code,
       sp.deleted_at AS mapping_deleted_at
FROM t_shelf_process sp
JOIN t_shelf sh ON sh.id = sp.shelf_id
JOIN t_process p ON p.id = sp.process_id
WHERE sp.deleted_at IS NULL
  AND (sh.deleted_at IS NOT NULL OR NOT sh.is_active OR sh.zone <> 'PRODUCTION');
```

即「映射行本身还 active，但货架已软删 / 已停用 / 不是生产架」。这三条正是
`ShelfProcessRepo::find_first_shelf_for_process` 自 2026-10-04 起在 SQL 里挡掉的谓词，
也是 `POST /{shelf_id}` 现在拒收的形态。**修完守卫后本查询仍应作为存量巡检手段定期跑**
（新守卫只挡新写入，挡不住历史行）。

### 本仓验证记录

2026-10-04 在本 worktree 的开发库（`DATABASE_URL=postgres://hsh:6065161@localhost:5450/hsh`，
schema 由 `sqlx migrate run` 建到 HEAD）实跑：两条均返回 **0 行**。
⚠️ 该库是**空库**（`t_shelf` / `t_shelf_process` / `t_part_batch` / `t_part` 均为 0 行，已逐表
`count(*)` 核实），故这是**空真值**、**不构成「线上无脏数据」的证据** —— 线上库需另行执行。

为确认 SQL 本身能命中（而非因写错谓词恒返空），另在同库一个 `BEGIN … ROLLBACK` 事务内
合成「1 品检架 + 1 停用架 + 1 软删架 + 1 正常架 / 各 1 条映射 / 5 条在池批次（其中 1 条
holder 指向不存在的货架）」后逐分支核对：

- **SQL ① 命中 4 行**，四条各打中一个互不相同的分支 —— `zone<>PRODUCTION` /
  `NOT is_active` / `deleted_at IS NOT NULL` / **`sh.id IS NULL`（悬空 holder）**；
  正常架那条批次正确地未被命中。
- **SQL ② 命中 3 行** —— `zone<>PRODUCTION` / `NOT is_active` / `deleted_at IS NOT NULL`。

`ROLLBACK` 后复查 `t_shelf` / `t_shelf_process` / `t_part_batch` / `t_part` 四表均回到 0 行。
（2026-10-04 review 第 1 轮订正：首版验证记录只覆盖了 ① 的前三个分支，漏了正文声称覆盖的
悬空 holder 分支，且把 ① 的行数一并记成 3；现已补测并按上列数字订正。）

---

## 前端配套改动清单

> ⚠️ 2026-10-02 新增小节（2026-10-02 已回填落地状态）。本节纠正此前文档里「前端只改 URL」
> 的说法：**该说法只对本文档 3 个 mapping 端点成立，对同 commit 删除的
> `ShelfOut.account_count` 不成立** —— 后端删字段而前端不同步，会让
> `useProductionShelvesQuery` 的 `shelfListResultSchema.parse(...)` 每次抛
> `ZodError`，货架列表 / 详情页面**硬失败**（不是降级展示）。机制说明见顶部警示块，
> 逐条落点见下方 A、B 两节。

### 落地状态（2026-10-02 回填）

**A、B 两节全部条目已落地，本节从「待办清单」转为「已完成的改动记录」**。
保留本节而非删除，是因为它同时承担两个仍有效的职责：① 记录硬切前后的 URL 对照，
方便日后排查旧路径残留；② 记录 `ShelfOut` 字段变更的前端同步面（见顶部警示块）。

| 归属 | 内容 |
|---|---|
| 后端 | 工序映射搬进 prod 域 + 账号部分消除（原始拆分） |
| 后端 | 端点数订正 + 本清单 + 测试加固 |
| 后端合并 | 货架域拆分（工序映射搬进 prod/shelf_process + 账号部分消除）已入 `master` |
| 前端 | 端点契约对齐 v2 后端（422 根因 + 3 处静默数据损坏） |
| 前端 | 保存闸门堵死静默清空 + 5 项准确性订正 |
| 前端 | 3 端点 URL 硬切到 prod 域 |
| **前端（⏳ 待办，2026-10-04 新增）** | **C. `ShelfList.vue` 按 zone 收敛映射编辑区（配合 2026-10-04 zone 守卫）** |

- **后端**：已在 `master`。
- **前端**：在 `feat/shelf-domain-split` 分支，**截至 2026-10-02 尚未合入前端 `main`**
  （`main..feat/shelf-domain-split` = 3 commit，反向为空）。部署 / 联调以外部分支
  `main` 为准的环境仍会打旧路径。
- **联调方式**：后端在本仓 worktree 起 app（`cargo run`），前端 `npm run dev`，
  用浏览器直接打 3 个端点核对：
  - `GET /api/v2/prod/shelf-processes`
  - `GET /api/v2/prod/shelf-processes/{shelf_id}`
  - `POST /api/v2/prod/shelf-processes/{shelf_id}`（body `{"items":[{"process_id":"…","sort_order":0}]}`）

  前端侧对应走 `src/api/shelves.ts` 的 `getAllShelfProcessMappings` /
  `getShelfProcesses` / `setShelfProcesses` 三个函数，无第二个 URL 拼装点（见 A 节核实结论）。

### A. 3 个 URL 硬切（写路径现在 404）—— ✅ 已落地

`src/api/shelves.ts`：

| 前端函数 | 旧 URL | 新 URL | 备注 |
|---|---|---|---|
| `getShelfProcesses(id)` | `GET /shelves/{id}/processes` | `GET /prod/shelf-processes/{shelf_id}` | 返回类型由 v1 影子类型 `ShelfWithProcesses` 改为 `ShelfProcessesResult`（`{items:[…]}`） |
| `setShelfProcesses(id, payload)` | `POST /shelves/{id}/processes` | `POST /prod/shelf-processes/{shelf_id}` | **写路径，旧 URL 现在 404**；返回类型由 `ShelfWithProcesses` 改为 `Promise<void>`（后端 `data` 为 `null`） |
| `getAllShelfProcessMappings()` | `GET /shelves/processes` | `GET /prod/shelf-processes` | ⚠️ 旧 URL **不是 404 而是 400 纯文本**（落进 shelf 域 `/{id}` 路由，`processes` 非 i64 被 axum Path 解析拒），前端 axios 侧会拿到非 `R` 信封的裸错误 |

**URL 硬切本身是本次最小的一层**，但同一次前端改动顺带修了 3 个**独立的**、与域拆分
无关的 v1(Python)→v2(Rust) 迁移遗留契约 bug（都不属于后端本次改动，故从未进过本清单）：

| 症状 | 根因 | 前端落点 |
|---|---|---|
| 保存工序映射报 **HTTP 422** | `setShelfProcesses` 发 `{process_ids: string[]}`，后端 `SetShelfProcessesRequest.items` 必填且无 `#[serde(default)]` → 40001。**该功能自 v1 迁 v2 起从未成功过一次** | `toShelfProcessesPayload()` 收口 payload 形态（`sort_order` 取数组下标 + 去重防撞 partial unique index） |
| 打开编辑弹窗已选工序被清空，点保存即**静默清空整组映射** | `getShelfProcesses` 读 `sp.processes`，后端实际返 `{items:[…]}` → `undefined.map` 抛 TypeError → 被裸 `catch` 吞掉 | `toShelfProcessIds()` 收口读形态；`catch` 不再清空而是置 `processLoadFailed` 硬拦整个保存动作（`ShelfList.vue`） |
| 多个页面的货架 / 工序下拉被**静默清空** | `getAllShelfProcessMappings` 按 v1 的「一架子集一行」读 `item.process_ids`；v2 返**扁平行**（一行一个 (货架, 工序) 对）→ `new Set(undefined)` = 空集 | `useShelfProcessFilter` 改按 `shelf_id` regroup 扁平行 |

> 顺带修正的**纯前端类型谎言**（后端从未返过这些字段，非本次后端改动引起）：
> `@/types/shelf::ShelfForReturn` 摘除 `display_order` / `mapped_process_codes`、
> `ShelfForReturnResult` 摘除 `recommended_shelf_id`，并补上后端 VO 里本就有的 `zone`。
> 连带落点 `src/views/scan/components/ShelfPickerDialog.vue`（去掉 `:mapped-process-codes`）
> 与 `src/views/scan/components/HmiPickerCard.vue`（标注该分支当前不可达、保留待后端补字段复活）。

**消费者核实（2026-10-02 逐个 `git grep` 核对）**：

| 消费方 | 调用点 | 是否需改 | 实际状态 |
|---|---|---|---|
| `src/views/shelves/ShelfList.vue` | 直调 `getShelfProcesses` / `setShelfProcesses` 各 1 | 需改 | ✅ 已改 |
| `src/composables/useShelfProcessFilter.ts` | 直调 `getAllShelfProcessMappings` | 需改 | ✅ 已改（按 `shelf_id` regroup 扁平行） |
| `src/views/cnc/composables/usePendingProgrammingStore.ts` | `useShelfProcessFilter` ×1 | **不需改** | ✅ 未改（对外 API 零变更，见下） |
| `src/views/inspection/InspectionPending.vue` | `useShelfProcessFilter` ×2 | **不需改** | ✅ 未改（同上） |
| `src/views/outsource/composables/useOutsourceReceivingList.ts` | `useShelfProcessFilter` ×1 | **不需改** | ✅ 未改（同上） |
| `src/views/parts/detail/PartDetail.vue` | `useShelfProcessFilter` ×2 | **不需改** | ✅ 未改（同上） |
| `src/views/parts/detail/components/PartCncCard.vue` | `useShelfProcessFilter` ×1 | **不需改** | ✅ 未改（同上） |
| `src/views/parts/list/composables/usePartDispatch.ts` | `useShelfProcessFilter` ×2 | **不需改** | ✅ 未改（同上）**← 本节此前漏列** |
| `src/views/repair/RepairStartDialog.vue` | `useShelfProcessFilter` ×1 | **不需改** | ✅ 未改（同上）**← 本节此前漏列** |

**已订正的三处计数 / 表述错误**：

1. **原写「已知消费者（7 处，需一并核对）」→ 实际是 8 个文件 / 12 个调用点**
   （7 个文件经 `useShelfProcessFilter`，共 10 个调用点；`ShelfList.vue` 直调 2 处）。
   原清单只列了 5 个经 composable 的文件，**漏了 `usePartDispatch.ts` 和
   `RepairStartDialog.vue`** —— 这两处是「下拉被静默清空」的实际受害面。
2. **原写「需一并核对」措辞误导**：`useShelfProcessFilter` 的对外 API 本次
   **零变更**（经它的 7 个文件 / 10 个调用点一个都不用改），需要改的只有
   `ShelfList.vue` 那 2 处直调。把两者混列成「7 处需一并核对」会让后来者
   以为要逐个去改 7 个文件。
3. **原写「另有 2 个测试 mock 点（`usePendingProgrammingStore.spec.ts`）」→ 只有 1 个文件**。

**新增测试文件（原清单未提）**：`src/api/shelfProcesses.spec.ts`（3 函数 URL +
payload / 响应形态逐字钉死 + 旧路径不出现）、`src/composables/__tests__/useShelfProcessFilter.spec.ts`
（扁平行 regroup + 失败态）、`src/views/shelves/__tests__/ShelfList.processMapping.spec.ts`
（保存闸门 P1~P4）。**漏网核查结论：全仓 `src/**` 已无任何拼装旧路径字符串的地方**
（`git grep '/shelves/processes' feat/shelf-domain-split` 的命中全在注释里，
且都是解释「旧路径已废」用的）；3 个 URL 只在 `src/api/shelves.ts` 一处拼装，
 无页面绕过 api 层自拼路径。

### B. `account_count` 出参删除 —— ✅ 已落地

后端侧已删：`master` 的 `src/modules/shelf/vo/shelf.rs::ShelfOut` 现为 **10 字段**，
无 `account_count`（连同 `ShelfRepo::count_accounts_by_shelf` 一并移除）。

| # | 文件 | 原状态 | 已落地动作 |
|---|---|---|---|
| 1 | `src/composables/queries/schemas.ts` | `shelfSchema.account_count: z.number()`（**必填**） | ✅ 删除该行 |
| 2 | `src/types/shelf.ts` | `Shelf.account_count: number`（必填字段） | ✅ 删除字段 + 就地留决策说明注释 |
| 3 | `src/views/shelves/ShelfList.vue` | 表格列 `{ key: 'account_count', label: '账号数' }` | ✅ 删除该列（`useColumnVisibility` 的 lenient 恢复会忽略 localStorage 里的残留项，**无需清浏览器缓存**） |
| 4 | `src/composables/queries/schemas.ts` 头部注释 | 列举 `Shelf` 11 字段含 `account_count` | ✅ 改为 10 字段 |

**原清单漏列、但确实改了的落点**：

- `src/composables/queries/useProductionShelvesQuery.ts` —— 头部注释 11 → 10 字段（守门点说明）
- `src/composables/queries/__tests__/schemas.spec.ts` —— 除 S30 外，**S29 的 fixture 与
  用例名也一起从 11 字段改到 10 字段**（断言 `parsed.account_count` 换成 `parsed.display_order`）
- `src/views/cnc/composables/usePendingProgrammingStore.ts` —— 注释记「两侧同步摘除」
- `src/api/shelves.ts` 文件头注释 —— 记「`account_count` 摘除已在前一个 commit 落地」

**回归用例 S30 的处置**：guard 字段从
`account_count` 换成同为必填的 `zone`**，用例改名「S30：shelfSchema 缺 zone → 抛
ZodError；shelfListResultSchema 缺 items → 抛 ZodError」。

**为什么「换字段」优于「改成不抛错」**：S30 这条用例的**设计意图不是守
`account_count` 这个具体字段，而是守「必填字段缺失必须抛 `ZodError`」** —— 它是
CLAUDE.md 架构条目 §4「Zod 默认 strip 模式会让缺字段静默丢弃，必填字段必须显式声明」
的 regression guard。若按原清单的「不抛错」处方改，这条 guard 就被**反向**了：
它会从「缺字段必须报错」变成「缺字段不报错」正是允许的，与被守护的架构条目背道而驰，
等于用删需求的方式让测试变绿。换 `zone` 则保留了原意图（`zone` 是 `ShelfOut` 至今
仍返的必填字段，且类型上不适合可选），并顺带证明 guard 机制本身与具体字段解耦。

> 因果方向备注（前端已在 `src/types/shelf.ts` / `schemas.ts` 就地注释）：前端摘该字段的
> 起因是**用户决定货架列表页不再展示账号数**，后端同 PR 删该字段是**另一次独立决策**，
> 不是「后端删了前端才跟删」。

> 后端侧说明：`account_count` 的真源是 iam 域 `t_user_role`（`scope_type='shelf'`），
> 前端若仍需展示「账号数」，应另走 iam 域用户列表按 `scope_type='shelf'` 聚合，
> **不要**指望货架域继续下发该字段。

### C. `ShelfList.vue` 按 zone 收敛映射编辑区 —— ⏳ 待办（2026-10-04 新增）

配合 [`POST` 的 zone 守卫](#zone-守卫2026-10-04-新增写侧收紧)（`items` 非空时要求
`zone='PRODUCTION'`）。**后端已上，前端未配套 ⇒ 库存管理页对品检架的编辑会失败。**

问题（`frontend/src/views/shelves/ShelfList.vue`）：

| 落点 | 现状 | 后果 |
|---|---|---|
| `saveShelf` 无条件调 `setShelfProcesses(shelfId, …)` | 不看 `shelfForm.zone` | 品检架的编辑 / 保存必收 20104 |
| 工序多选（弹窗内）对所有 zone 显示 | 读侧 10 处 `useShelfProcessFilter` 的候选源早已是 `zone='PRODUCTION'` 口径 | 与后端新守卫口径不一致 |
| `updateShelf` **先于** `setShelfProcesses` 执行 | 编辑态两个写点顺序固定 | **半截保存**：名称 / 物理顺序已落库，映射这一步报错，弹窗不关，toast 是后端原文 |

配套清单（2 处）：

1. `shelfForm.zone !== 'PRODUCTION'` 时**隐藏**工序多选（`el-form-item label="工序"`）；
2. `saveShelf` 在编辑态且 `shelfForm.zone !== 'PRODUCTION'` 时**跳过**
   `setShelfProcesses` 调用（此时报 20104 只会造成半截保存）。

配套落地前可用的绕过：品检架的映射用 `items: []` 清空（清空路径已豁免 zone 守卫，见上），
其余字段的编辑需前端修好后才不会失败。

---

## DTO 字段参考

`SetShelfProcessesRequest`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [SetShelfProcessesItem](#dto-字段参考) | — | 空数组 = 清空全部 mapping。**空数组时豁免 20512 / 20104 的 zone / is_active 守卫**（只守 20501），见 [zone 守卫](#zone-守卫2026-10-04-新增写侧收紧) |

`SetShelfProcessesItem`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | string (i64) | ✓ | 雪花 ID 字符串；不存在 → 20505 |
| `sort_order` | i32 | — | 显示顺序；缺省 0 |

`ShelfProcessMappingOut` / `AllShelfProcessMappingOut`：见上各端点「Response」表。

---

## 错误码归属（2026-10-02 调整说明，数字不动）

`20504` ~ `20508` 仍是**已发布契约**，本次只改归属说明，**数字一律不动**
（20507 被 `part/worker_scan` + `prod/worker_pool` 两处判定，
20508 被 `prod::batch` dispatch 判定）。

| 码 | 名称 | 归属 | 触发场景 |
|---|---|---|---|
| 20501 | `BIZ_SHELF_NOT_FOUND` | 货架域（数字留 205xx 段） | shelf 不存在 / 已软删 |
| 20502 | `BIZ_SHELF_DUPLICATE_CODE` | 货架域 | `uk_t_shelf_process` 撞（理论不应发生，service 已去重） |
| 20504 | `BIZ_SHELF_PROCESS_SHELF_NOT_FOUND` | **prod::shelf_process** | 货架不存在 |
| 20505 | `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` | **prod::shelf_process** | items 里有 process_id 不存在 |
| 20506 | `BIZ_SHELF_NO_MATCH_FOR_PROCESS` | prod 域 | 没有 active 货架映射指定 process |
| 20507 | `BIZ_SHELF_PROCESS_NOT_MAPPED` | prod::worker_pool | 货架未映射该工序（move / worker-scan 复用） |
| 20508 | `BIZ_SHELF_PROCESS_NOT_FOUND` | prod::batch | 按 `target_process_id` 查不到**可用**货架（2026-10-04 起含「有映射但货架已软删 / 已停用 / 非生产区」） |
| 20512 | `BIZ_SHELF_INACTIVE` | 货架域 | shelf `is_active=false`（2026-10-04 起 `POST /prod/shelf-processes/{shelf_id}` 经 `validate_shelf_zone` 也会命中） |

20104 `BIZ_INVALID_VALUE` 另见 [`../index.md`](./index.md) 通用错误码表 —— 2026-10-04
起它多了一个触发场景：本域 `POST` 收到非 PRODUCTION 区货架时。

---

## 维护约定

1. `set_shelf_processes` 走「整组替换」语义：先 `soft_delete_all_for_shelf` →
   `bulk_insert`。空 `items` = 清空全部 mapping（仍走事务）。
2. **`t_shelf_process` 的 SQL 真源只在 `src/modules/prod/shelf_process/repo.rs`**
   （`ShelfProcessRepo` 6 个静态方法）。以下位置**故意保留 inline**，不要硬抽：
   - `prod::batch::repo::preview_auto_dispatch` —— `LEFT JOIN LATERAL` 大复合查询，
     拆出来是性能回退
   - `prod::process::repo::count_process_references` —— 5 张表 sub-select 求和，
     拆出来多 5 次往返
   - part 域 3 处（`worker_scan.rs` / `phase1/mod.rs` /
     `pending_programming_sql.rs`）—— 属 part 域，不在本模块范围
3. `ShelfOut.account_count`（货架↔账号绑定数）已于 2026-10-02 删除，账号绑定真源在
   iam 域 `t_user_role`；本模块不涉及账号数据。

## 参考

- 集成测试：`tests/production/shelf_process.rs`（6 场景：整组替换 / 全集查询 /
  20505 拒未知工序 / 旧路径 4xx / 2026-10-04 品检架 20104 / 2026-10-04 停用架 20512）
- 错误码：`src/shared/error.rs::code`
- 货架 CRUD：[`../shelves.md`](../shelves.md)
