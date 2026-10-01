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
> 删除的 `ShelfOut.account_count` 会打爆前端 Zod 必填字段，完整清单见下方
> [前端配套改动清单](#前端配套改动清单)，**合入本后端 commit 前必须先落前端 PR**。
> ⚠️ 例外：`GET /shelves/processes` 现在落到 shelf 域的 `/{id}` 路由上，因 `processes`
> 非 i64 会被 axum 拒为 **400 纯文本**（非 `R` 信封），而非 404。

---

## 业务模型

`t_shelf_process` 记录「某货架可执行哪些工序」及其显示顺序，是 worker-scan 返库、
prod 批次下发解析货架、worker-pool 移动校验三处的共同依据。

- **逻辑引用**：`shelf_id` / `process_id` 为 bigint 逻辑引用，DB 层无 FK 约束
- **软删**：mapping 行有 `deleted_at`；整组替换时先 `UPDATE … SET deleted_at = now()`，
  再 `bulk_insert` 新行（历史行保留，便于追溯）
- **读守卫**：所有读查询一律带 `deleted_at IS NULL`；全集查询额外要求 `s.is_active = true`
- **依赖方向（2026-10-02 翻转）**：本域只**读** `shelf::repo::ShelfRepo::get_by_id`
  校验货架存在 / scope（prod → shelf）；反向的 shelf → prod 依赖已随端点搬移清零

业务约束（service 层 enforce）：

| 操作 | 约束 |
|---|---|
| `GET /` | 任意已登录；SHELF_ACCOUNT 按 `user.shelf_ids` 收窄；按 `shelf_id ASC, sort_order ASC` |
| `GET /{shelf_id}` | 货架不存在 → 20501；SHELF_ACCOUNT 越界 → 40301；按 `sort_order ASC, id ASC` |
| `POST /{shelf_id}` | 货架不存在 → 20501；items 里有 process_id 不存在或已软删 → 20505；整组替换语义，`items: []` = 清空 |

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
2. 校验 items 里所有 process_id 存在且未软删（20505）
3. 软删该 shelf 的全部 active mapping（清 `deleted_at`）
4. `bulk_insert` 新 mapping（带 sort_order）

`items` 可为 `[]`（清空映射）。

Response 200 `data`：`null`

错误码：

- 20501 `BIZ_SHELF_NOT_FOUND` —— shelf 不存在 / 已软删
- 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
- 20104 `BIZ_INVALID_VALUE` —— process_id 非整数

---

## 前端配套改动清单

> ⚠️ 2026-10-02 新增小节。本节纠正此前文档里「前端只改 URL」的说法：**该说法只对
> 本文档 3 个 mapping 端点成立，对同 commit 删除的 `ShelfOut.account_count`
> 不成立** —— 后端删字段后前端若不同步，**每次货架列表 / 详情响应都会 Zod 硬失败**
> （`shelfSchema.account_count` 是必填字段，`useProductionShelvesQuery` 对每个
> 响应做 `shelfListResultSchema.parse(...)`），不是降级展示而是直接报错。

### A. 3 个 URL 硬切（写路径现在 404）

`src/api/shelves.ts`：

| 前端函数 | 旧 URL | 新 URL | 备注 |
|---|---|---|---|
| `getShelfProcesses(id)` | `GET /shelves/{id}/processes` | `GET /prod/shelf-processes/{shelf_id}` | 响应 `ShelfWithProcesses` 逐字不变 |
| `setShelfProcesses(id, payload)` | `POST /shelves/{id}/processes` | `POST /prod/shelf-processes/{shelf_id}` | **写路径，旧 URL 现在 404**；入参 `items[].process_id` / `items[].sort_order` 逐字不变 |
| `getAllShelfProcessMappings()` | `GET /shelves/processes` | `GET /prod/shelf-processes` | ⚠️ 旧 URL **不是 404 而是 400 纯文本**（落进 shelf 域 `/{id}` 路由，`processes` 非 i64 被 axum Path 解析拒），前端 axios 侧会拿到非 `R` 信封的裸错误 |

**已知消费者（7 处，需一并核对）**：`src/views/shelves/ShelfList.vue`（`getShelfProcesses`
/ `setShelfProcesses` 两处）、`src/composables/useShelfProcessFilter.ts`（`getAllShelfProcessMappings`）、
`src/views/cnc/composables/usePendingProgrammingStore.ts`、
`src/views/inspection/InspectionPending.vue`、
`src/views/outsource/composables/useOutsourceReceivingList.ts`、
`src/views/parts/detail/PartDetail.vue`、`src/views/parts/detail/components/PartCncCard.vue`
（后 4 处经 `useShelfProcessFilter` / `ShelfWithProcesses` 类型间接消费），
另有 2 个测试 mock 点（`src/views/cnc/composables/__tests__/usePendingProgrammingStore.spec.ts`）。

### B. `account_count` 出参删除（4 处前端落点 + 1 条回归用例）

| # | 文件 | 现状 | 必改原因 |
|---|---|---|---|
| 1 | `src/composables/queries/schemas.ts` | `shelfSchema.account_count: z.number()`（**必填**） | 后端不再下发该字段 → `shelfListResultSchema.parse(...)` 每次抛 `ZodError`，**所有走共享 query 的货架列表/详情页面硬失败**（`ShelfList.vue`、待编程 store、inspection picker 等） |
| 2 | `src/types/shelf.ts` | `Shelf.account_count: number`（必填字段） | 类型层与后端契约脱节，须删字段并同步 11 字段注释 |
| 3 | `src/views/shelves/ShelfList.vue` | 表格列 `{ key: 'account_count', label: '账号数' }` | 列渲染恒 `undefined`，须删列 |
| 4 | `src/composables/queries/schemas.ts` 头部注释 | 列举 `Shelf` 11 字段含 `account_count` | 注释失真，须同步 |

**回归用例**：`src/composables/queries/__tests__/schemas.spec.ts` 的 **S30** 用例断言
「缺 `account_count` 必抛 ZodError」—— 这条原本把**即将废除的契约**钉成了回归基线。
前端 PR 必须改写 S30（改为断言「缺 `account_count` **不**抛错」或直接改为针对其它
必填字段的 strip 陷阱 guard），否则改完 `schemas.ts` 测试必红。

> 后端侧说明：`account_count` 的真源是 iam 域 `t_user_role`（`scope_type='shelf'`），
> 前端若仍需展示「账号数」，应另走 iam 域用户列表按 `scope_type='shelf'` 聚合，
> **不要**指望货架域继续下发该字段。

---

## DTO 字段参考

`SetShelfProcessesRequest`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [SetShelfProcessesItem](#setshelfprocessesitem-字段) | — | 空数组 = 清空全部 mapping |

`SetShelfProcessesItem`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `process_id` | string (i64) | ✓ | 雪花 ID 字符串；不存在 → 20505 |
| `sort_order` | i32 | — | 显示顺序；缺省 0 |

`ShelfProcessMappingOut` / `AllShelfProcessMappingOut`：见上各端点「Response」表。

---

## 错误码归属（2026-10-02 调整说明，数字不动）

`20504` ~ `20508` 仍是**已发布契约**，本次只改归属说明，**数字一律不动**
（20507 被 `part/worker_scan` + `prod/worker_pool` + `delivery print` 三处判定，
20508 被 `prod::batch` dispatch 判定）。

| 码 | 名称 | 归属 | 触发场景 |
|---|---|---|---|
| 20501 | `BIZ_SHELF_NOT_FOUND` | 货架域（数字留 205xx 段） | shelf 不存在 / 已软删 |
| 20502 | `BIZ_SHELF_DUPLICATE_CODE` | 货架域 | `uk_t_shelf_process` 撞（理论不应发生，service 已去重） |
| 20504 | `BIZ_SHELF_PROCESS_SHELF_NOT_FOUND` | **prod::shelf_process** | 货架不存在 |
| 20505 | `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` | **prod::shelf_process** | items 里有 process_id 不存在 |
| 20506 | `BIZ_SHELF_NO_MATCH_FOR_PROCESS` | prod 域 | 没有 active 货架映射指定 process |
| 20507 | `BIZ_SHELF_PROCESS_NOT_MAPPED` | prod::worker_pool | 货架未映射该工序（move / worker-scan 复用） |
| 20508 | `BIZ_SHELF_PROCESS_NOT_FOUND` | prod::batch | 按 `target_process_id` 在 `t_shelf_process` 0 结果 |

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

- 集成测试：`tests/production/shelf_process.rs`（4 场景：整组替换 / 全集查询 /
  20505 拒未知工序 / 旧路径 4xx）
- 错误码：`src/shared/error.rs::code`
- 货架 CRUD：[`../shelves.md`](../shelves.md)
