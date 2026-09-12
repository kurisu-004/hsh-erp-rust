# process_chain 域 API

> 本文件须与 `src/modules/process_chain/{handler.rs,dto.rs,service.rs,model.rs,repo/}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 范围：每个 part 绑定一份多步工艺链（part 1:1 chain；chain 1:N steps）。
> 当前 MVP 暴露 2 个端点：
> - `GET /api/v2/process-chains/by-part/{part_id}` —— 读 part 绑定的工艺链（header + steps）
> - `PUT /api/v2/process-chains/by-part/{part_id}` —— 整组 upsert（替换语义：保留 header id，version++，软删旧 steps，INSERT 新 steps）
>
> 实施阶段：part-worker-pool-federated-rocket（2026-09-11）

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/process-chains/by-part/{part_id}` | **Manager+Clerk+Inspector+CncProgrammer**（任意已登录） | 读 part 绑定的工艺链（header + steps）。无链 → 20701 + 404 |
| PUT | `/api/v2/process-chains/by-part/{part_id}` | **Manager** | 整组 upsert：1:1 binding、OCC、软删旧 steps、INSERT 新 steps、单事务 |

> 路由挂载：`/process-chains` 走 `/api/v2`（见 `src/modules/mod.rs`）。

---

### `GET /api/v2/process-chains/by-part/{part_id}`

权限：**Manager+Clerk+Inspector+CncProgrammer**（service 内 `require_any_role`；无 `ShelfAccount`）。

Path：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | i64 (snowflake) | ✓ | 工单雪花 ID |

业务流转（service `get_by_part`）：

1. 查 `t_part_process_chain` 按 `part_id` 取未软删 header → 无 → `BIZ_PROCESS_CHAIN_NOT_FOUND` (20701, HTTP 404)
2. 查 `t_process_chain_step` 按 `chain_id` 排序（sort_order ASC, id ASC）取全部未软删 steps
3. 返回 `ProcessChainOut`

Response 200 `data`：[`ProcessChainOut`](#processchainout-字段)

错误码：

- 20701 BIZ_PROCESS_CHAIN_NOT_FOUND — part 尚未绑定工艺链 / 已软删
- 40300 FORBIDDEN — 角色不在白名单

### `PUT /api/v2/process-chains/by-part/{part_id}`

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
3. 查现有链（按 `part_id`）
4. 整组事务：
   - **有链** → OCC `bump_chain_version`（同时按 COALESCE 模式更新 name / note，version 自增）
   - **无链** → INSERT 新 header（`name` 默认 `'默认工艺'`；uk_t_part_process_chain_part_id 23505 → 400 INVALID）
5. `soft_delete_all_steps_for_chain` —— 清空旧 steps（保留审计）
6. `bulk_insert_steps` —— 单条 `INSERT ... VALUES (...), (...)` 写新 steps
7. 回读 header + steps，返回 `ProcessChainOut`

错误码：

- 20104 BIZ_INVALID_VALUE — `process_id` 非 i64 字符串 / `estimated_minutes < 0` / sort_order 重复 / 23505（并发建链）
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
| `version` | i32 | 乐观锁（暂无并发写场景，预留） |

### ProcessChainOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 工艺链雪花 ID |
| `part_id` | string (i64) | 绑定的工单 ID（1:1 强约束 `uk_t_part_process_chain_part_id`） |
| `name` | string | 链名 |
| `note` | string? | 备注 |
| `version` | i32 | 乐观锁；upsert 整组替换 +1 |
| `created_at` | datetime | DB 默认 `now()` |
| `updated_at` | datetime | DB 默认 `now()`；每次 OCC 自增时刷新 |
| `steps` | [ProcessChainStepOut](#processchainstepout-字段) | 步骤列表（按 sort_order ASC, id ASC） |

### UpsertChainStep 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sort_order` | i32 | ✓ | 步骤顺序；同 chain 内未软删步骤不可重复 |
| `process_id` | string (i64 字符串) | ✓ | 工序 ID；service 层 `parse::<i64>()` |
| `estimated_minutes` | i32 | ✓ | 预估耗时；必须 ≥ 0 |

### UpsertChainRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✗ | 链名；空串 → 不修改（已存在链）/）默认 `'默认工艺'`（新建） |
| `note` | string? | ✗ | 备注；空串 → 显式清空（DB SET NULL） |
| `steps` | [UpsertChainStep](#upsertchainstep-字段) | ✓ | 步骤列表；空数组 = 保留 header 但清空 steps |

---

## 端点约束

- **1:1 binding**：`t_part_process_chain.part_id UNIQUE` 索引强约束；并发建同 part 链 → 23505
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → 40901
- **软删除**：`deleted_at IS NULL`；步骤软删后不占 sort_order 槽位（部分唯一索引 `(chain_id, sort_order) WHERE deleted_at IS NULL`）
- **稀疏 sort**：sort_order 默认 10/20/30；中间插入取 `(prev+next)/2`；精度耗尽（差值=1）触发 service `reorder_with_step_size` 批量重排（statemachine.rs 实现，本次未实装触发路径）
- **事务边界在 handler**：`state.pool.begin()` → `&mut tx` 给 service → 显式 `tx.commit()`
- **WS 广播**：本域暂无 WS（仅 upsert 改静态数据，前端按需轮询 `GET by-part`）

## 实施状态（2026-09-11 part-worker-pool-federated-rocket）

- ✅ Migration 016：`t_work_type.max_held_minutes` 列（TIME 模式阈值依据）
- ✅ Migration 017：`t_part_process_chain` + `t_process_chain_step` 两表 + 部分唯一索引
- ✅ Migration 018：生产管理菜单（production_group 一级 + part_process_chain 二级 + worker_queue 迁移）
- ✅ 错误码 20701 / 20702 / 20703 / 20704
- ✅ 模块 6 文件 + repo / service 子模块拆分
- ✅ 端点：`GET / PUT /by-part/{part_id}`
- ✅ 集成测试 6 场景：happy / 404 / 替换 steps / negative minutes / 重复 sort / 非 Manager 403

## 参考

- 集成测试：`tests/process_chain_api.rs`
- 仓库分层：`src/modules/process_chain/handler.rs` (axum) → `service/crud.rs` (业务) → `repo/query.rs` + `repo/mutate.rs` (SQL)
- 错误码：`src/shared/error.rs::code`（20104 / 20701 / 20702 / 20703 / 20704 / 40001 / 40300 / 40901）