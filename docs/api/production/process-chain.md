# process_chain 域 API

> 本文件须与 `src/modules/prod/process_chain/{handler.rs,dto.rs,service.rs,model.rs,repo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：每个 part 绑定一份多步工艺链（part 1:1 chain；chain 1:N steps）。
>
> **2026-09-16 FK 翻转（migration 026）**：1:1 归属关系改由 `t_part.process_chain_id`
> 承载（原 `t_part_process_chain.part_id` 列已删除）。前端「工序制定」页批量查询
> part 列表，按 `process_chain_id` 是否为 `null` 区分「已制定 / 未制定工序」；
> 点击零件后按 `process_chain_id` 调 `GET /{chain_id}` 加载工序信息。
>
> 当前暴露 3 个端点：
> - `GET /api/v2/prod/process-chains/by-part/{part_id}` —— 读 part 绑定的工艺链（header + steps）
> - `PUT /api/v2/prod/process-chains/by-part/{part_id}` —— 整组 upsert（替换语义：保留 header id，version++，软删旧 steps，INSERT 新 steps）
> - `GET /api/v2/prod/process-chains/{chain_id}` —— 按链 id 读工艺链（FK 翻转新增）
>
> 实施阶段：part-worker-pool-federated-rocket（2026-09-11）；FK 翻转 PR-1（2026-09-16）

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/prod/process-chains/by-part/{part_id}` | **Manager+Clerk+Inspector+CncProgrammer**（任意已登录） | 读 part 绑定的工艺链（header + steps）。无链 → 20701 + 404 |
| PUT | `/api/v2/prod/process-chains/by-part/{part_id}` | **Manager** | 整组 upsert：1:1 binding、OCC、软删旧 steps、INSERT 新 steps、单事务；**PENDING 守卫（20705）** |
| GET | `/api/v2/prod/process-chains/{chain_id}` | **Manager+Clerk+Inspector+CncProgrammer**（任意已登录） | 按链 id 读工艺链。无链 / 已软删 → 20701 + 404 |

> 路由挂载：`/process-chains` 走 `/api/v2`（见 `src/modules/mod.rs`）。
> axum 静态段 `by-part` 优先于参数段 `{chain_id}`，`/by-part/123` 不会被解析成 chain_id。

---

### `GET /api/v2/prod/process-chains/by-part/{part_id}`

权限：**Manager+Clerk+Inspector+CncProgrammer**（service 内 `require_any_role`；无 `ShelfAccount`）。

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | i64 (snowflake) | ✓ | 工单雪花 ID |

业务流转（service `get_by_part`）：

1. 经 `t_part` JOIN 查链：`t_part_process_chain c JOIN t_part p ON p.process_chain_id = c.id`，
   要求 `p.id = part_id` 且双方未软删 → 无 → `BIZ_PROCESS_CHAIN_NOT_FOUND` (20701, HTTP 404)
2. 查 `t_process_chain_step` 按 `chain_id` 排序（sort_order ASC, id ASC）取全部未软删 steps
3. 返回 `ProcessChainOut`

Response 200 `data`：[`ProcessChainOut`](#processchainout-字段)

错误码：

- 20701 BIZ_PROCESS_CHAIN_NOT_FOUND — part 尚未绑定工艺链 / 已软删
- 40300 FORBIDDEN — 角色不在白名单

### `GET /api/v2/prod/process-chains/{chain_id}`

**2026-09-16 FK 翻转新增。** 前端在「工序制定」页点击零件后，按 part 的
`process_chain_id` 调本端点加载工序信息。

权限：**Manager+Clerk+Inspector+CncProgrammer**（与 by-part GET 相同）。

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `chain_id` | i64 (snowflake) | ✓ | 工艺链雪花 ID |

业务流转（service `get_chain_by_id`）：

1. 按主键查 `t_part_process_chain`（`deleted_at IS NULL`）→ 无 → `BIZ_PROCESS_CHAIN_NOT_FOUND` (20701, HTTP 404)
2. 查 steps（同 by-part）
3. 返回 `ProcessChainOut`

Response 200 `data`：[`ProcessChainOut`](#processchainout-字段)

错误码：

- 20701 BIZ_PROCESS_CHAIN_NOT_FOUND — 链不存在 / 已软删
- 40300 FORBIDDEN — 角色不在白名单

### `PUT /api/v2/prod/process-chains/by-part/{part_id}`

权限：**Manager**（service 内 `require_role`）。

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | i64 (snowflake) | ✓ | 工单雪花 ID |

Request：`UpsertChainRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✗ | 链名；空串视为「不修改」；新建时默认 `'默认工艺'` |
| `note` | string? | ✗ | 备注；空串视为「显式清空」 |
| `steps` | [UpsertChainStep](#upsertchainstep-字段) | ✓ | 步骤列表（可空数组：保留 header 但清空所有步骤） |

业务流转（service `upsert_chain`）：

1. 解析 + 校验：每步 `process_id` 必须为 i64 字符串、`estimated_minutes ≥ 0`
2. sort_order 重复检查（DB 部分唯一索引兜底；前端先校验更友好）
3. 加载 part → 不存在 / 已软删 → 20101 BIZ_PART_NOT_FOUND
4. **PENDING 守卫（2026-09-16 新增）**：`part.status != 'PENDING'` →
   20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING（HTTP 409，"零件已下发，禁止制定/修改工艺链"）
5. 查现有链（经 part JOIN）
6. 整组事务：
   - **有链** → OCC `bump_chain_version`（同时按 COALESCE 模式更新 name / note，version 自增）
   - **无链** → INSERT 新 header（`name` 默认 `'默认工艺'`）+ `link_chain_to_part`
     （`t_part.process_chain_id = chain_id`，带 `process_chain_id IS NULL` 并发守卫；
     撞 `uq_t_part_process_chain` 23505 或 0 行 → 20104 并发冲突）
7. `soft_delete_all_steps_for_chain` —— 清空旧 steps（保留审计）
8. `bulk_insert_steps` —— 单条 `INSERT ... VALUES (...), (...)` 写新 steps
9. 回读 header + steps，返回 `ProcessChainOut`

错误码：

- 20101 BIZ_PART_NOT_FOUND — part 不存在 / 已软删
- 20104 BIZ_INVALID_VALUE — `process_id` 非 i64 字符串 / `estimated_minutes < 0` / sort_order 重复 / 并发建链冲突（23505 或 link 0 行）
- 20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING — part 已下发（非 PENDING），禁止制定/修改工艺链（HTTP 409）
- 40901 VERSION_CONFLICT — OCC 自增失败
- 40300 FORBIDDEN — 非 Manager

> 注：业务字段校验抛 `20104 BIZ_INVALID_VALUE`（与既有 2xxxx 业务错默认 400 对齐），不抛 `40001 VALIDATION_ERROR`（422）。

---

## 共享 DTO

### ProcessChainStepOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 步骤雪花 ID |
| `sort_order` | i32 | 稀疏 sort（10/20/30，UI 中间插入时取 `(prev+next)/2`） |
| `process_id` | string (i64) | 工序雪花 ID（逻辑指向 `t_process.id`，无 FK） |
| `estimated_minutes` | i32 | 预估耗时（≥0；CHECK 约束） |
| `note` | string? | 单步备注（车间操作员参考）；无备注时字段缺省 |
| `version` | i32 | 乐观锁（暂无并发写场景，预留） |

### ProcessChainOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工艺链雪花 ID |
| `name` | string | 链名 |
| `note` | string? | 备注 |
| `version` | i32 | 乐观锁；upsert 整组替换 +1 |
| `created_at` | datetime | DB 默认 `now()` |
| `updated_at` | datetime | DB 默认 `now()`；每次 OCC 自增时刷新 |
| `steps` | [ProcessChainStepOut](#processchainstepout-字段) | 步骤列表（按 sort_order ASC, id ASC） |

> **2026-09-16 BREAKING**：`part_id` 字段已移除（FK 翻转）。归属关系从 part 侧读：
> `GET /api/v2/parts` / `GET /api/v2/parts/{id}` 响应含 `process_chain_id`（string i64?）。

### UpsertChainStep 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sort_order` | i32 | ✓ | 步骤顺序；同 chain 内未软删步骤不可重复 |
| `process_id` | string (i64 字符串) | ✓ | 工序 ID；service 层 `parse::<i64>()` |
| `estimated_minutes` | i32 | ✓ | 预估耗时；必须 ≥ 0 |
| `note` | string? | ✗ | 单步备注；空串视作 None |

### UpsertChainRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✗ | 链名；空串 → 不修改（已存在链）/ 默认 `'默认工艺'`（新建） |
| `note` | string? | ✗ | 备注；空串 → 显式清空（DB SET NULL） |
| `steps` | [UpsertChainStep](#upsertchainstep-字段) | ✓ | 步骤列表；空数组 = 保留 header 但清空 steps |

---

## 端点约束

- **1:1 binding（2026-09-16 翻转）**：`t_part.process_chain_id` 上的部分唯一索引
  `uq_t_part_process_chain`（`WHERE process_chain_id IS NOT NULL AND deleted_at IS NULL`）
  强约束「活跃 part ↔ 链」1:1；并发绑同 part → link 0 行 / 撞索引 23505 → 20104
- **PENDING 守卫（2026-09-16 新增）**：仅 `part.status = 'PENDING'` 允许 upsert；
  零件下发后工艺链冻结（20705，HTTP 409）
- **part 软删级联（2026-09-16 新增）**：`soft_delete_part` 同事务内级联
  软删全部 steps → 软删链 → `t_part.process_chain_id` 置 NULL（让出 uq 槽位）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → 40901
- **软删除**：`deleted_at IS NULL`；步骤软删后不占 sort_order 槽位（部分唯一索引 `(chain_id, sort_order) WHERE deleted_at IS NULL`）
- **稀疏 sort**：sort_order 默认 10/20/30；中间插入取 `(prev+next)/2`；精度耗尽（差值=1）触发 service `reorder_with_step_size` 批量重排（statemachine.rs 实现，本次未实装触发路径）
- **事务边界在 handler**：`state.pool.begin()` → `&mut tx` 给 service → 显式 `tx.commit()`
- **WS 广播**：本域暂无 WS（仅 upsert 改静态数据，前端按需轮询）

### 20706 BIZ_PROCESS_CHAIN_REQUIRED（2026-09-16 PR-3 新增）

「part 进入生产流前必须已绑定工艺链」守卫（service `require_process_chain`
helper，`src/modules/part/service/phase1.rs:342`）。错误码 HTTP 409，业务含义：
「请先制定工序链（part 未绑定 process_chain）」。触发条件：

- `t_part.process_chain_id IS NULL`（part 还没制定工艺链）

受影响的 7 个端点（part 域，含 6 个 Phase 1 + 1 个 inspection 流）：
- `POST /api/v2/parts/{part_id}/place-on-shelf` —— 上架
- `POST /api/v2/parts/{part_id}/release-from-programming` —— 编程完成释放
- `POST /api/v2/parts/{part_id}/send-to-outsource` —— 派发外协
- `POST /api/v2/parts/{part_id}/receive-from-outsource` —— 外协回收入库
- `POST /api/v2/parts/{part_id}/complete-repair` —— 完成维修
- `POST /api/v2/parts/{part_id}/repair-dispatch` —— 派发维修
- `POST /api/v2/parts/{part_id}/to-process` —— 品检打回（`inspection_core.rs:262`）

> 注：与 `to-process` 共用 service 的两个批量聚合端点（`batch-to-process`
> 之类）一并继承此守卫。具体每个端点的「错误码」段列在
> [`docs/api/parts/lifecycle.md`](../../api/parts/lifecycle.md) 与
> [`docs/api/parts/inspection.md`](../../api/parts/inspection.md) 中。

## 实施状态

- ✅ Migration 016：`t_work_type.max_held_minutes` 列（TIME 模式阈值依据）
- ✅ Migration 017：`t_part_process_chain` + `t_process_chain_step` 两表 + 部分唯一索引
- ✅ Migration 018：生产管理菜单（production_group 一级 + part_process_chain 二级 + worker_queue 迁移）
- ✅ Migration 026（2026-09-16）：FK 方向翻转 —— `t_part.process_chain_id` + 回填 +
  `ix_t_part_process_chain_id` + `uq_t_part_process_chain`；`t_part_process_chain` 删 `part_id` 列
- ✅ 错误码 20701 / 20702 / 20703 / 20704 / 20705（PENDING 守卫）/ **20706（2026-09-16 PR-3 新增，PROCESS_CHAIN_REQUIRED，作用于 7 个生产流端点）**
- ✅ 模块 6 文件 + repo / service 子模块拆分
- ✅ 端点：`GET / PUT /by-part/{part_id}` + **`GET /{chain_id}`（2026-09-16 新增）**
- ✅ 集成测试 10 场景：happy（含 part 指针回写断言）/ 404 / 替换 steps / negative minutes /
  重复 sort / 非 Manager 403 / step.note 往返 / **非 PENDING 20705** / **by-id 命中+未命中** /
  **part 软删级联**

## 参考

- 集成测试：`tests/process_chain_api.rs`
- 仓库分层：`src/modules/prod/process_chain/handler.rs` (axum) → `service/crud.rs` (业务) → `repo/query.rs` + `repo/mutate.rs` (SQL)
- 错误码：`src/shared/error.rs::code`（20101 / 20104 / 20701 / 20702 / 20703 / 20704 / 20705 / **20706 PROCESS_CHAIN_REQUIRED（PR-3 新增）** / 40001 / 40300 / 40901）

---

> **2026-09-23 PR12 同步说明**：仓库内部重构（PR6）将 `prod/process_chain/statemachine.rs`
> 改名 `helpers.rs`（文件名误导 —— 该文件内容非状态机，仅是 sort_order 间隙检测
> 等 helper 集合；改名后 git mv 保留历史）。**对外 API 与 DTO 无任何变化**，
> docs/api/production/process-chain.md 现有 4 个章节（端点列表 / 共享 DTO / 端点约束
> / 错误码）保持不变。
