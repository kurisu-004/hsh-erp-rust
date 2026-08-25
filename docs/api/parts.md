# part 域 API

> 本文件须与 `src/modules/part/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> 当前 PR 只上线 pass_inspection 两条端点（单件 + 批量）。part 域其余 CRUD（创建工单 / 修改工单 / 列表查询等）尚未实施，会得到 `404 Not Found`。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/parts/batch-pass-inspection` | **Manager** / **Inspector** | 批量通过品检（INSPECTION → READY_TO_SHIP），per-item 独立处理 |
| POST | `/api/v2/parts/{part_id}/pass-inspection` | **Manager** / **Inspector** | 单件通过品检（INSPECTION → READY_TO_SHIP），payload 可空 |
| POST | `/api/v2/parts/batch-scan-inspect` | **Manager** / **Inspector** | 批量一键送检（共享品检架 + per-item decision，N≤200） |
| POST | `/api/v2/parts/{part_id}/scan-inspect` | **Manager** / **Inspector** | 单件一键送检（PENDING/PROGRAMMING/IN_PROCESS → INSPECTION → PASS/FAIL） |
| POST | `/api/v2/parts/{part_id}/fail-inspection` | **Manager** / **Inspector** | 单件品检打回（INSPECTION → IN_PROCESS，依赖 shelf+next_process） |
| POST | `/api/v2/parts/worker-scan` | **Manager** / **ShelfAccount** | 工人扫码归还 / 送检（SHELF_ACCOUNT @ 该 shelf）；成功后同事务触发 worker-pool refill |

> 路由顺序：`/batch-pass-inspection` 必须在 `/{part_id}/pass-inspection` **之前**注册（axum 防止把 `batch-pass-inspection` 当作 `part_id` 解析）。`/worker-scan` 同理为静态段，须在 `/{part_id}/...` 之前注册。

---

### `POST /api/v2/parts/batch-pass-inspection`

权限: **Manager / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [BatchPassItem](#batchpassitem-字段) | ✓ | 1..=200 个；空数组 / 超出上限 → `40001` |
| `items[].part_id` | string (i64) | ✓ | 工单雪花 ID（字符串避免 JS 精度截断） |
| `items[].batch_id` | string (i64)? | — | 指定批次；当 part 下存在多个 INSPECTION 批次时用于消歧，缺省按 part_id 唯一匹配 |
| `items[].quantity` | i32? | — | 本次送检数量；当前仅支持整批送检，`quantity ≤ 0` 或 `quantity > 批次剩余量` → `20111` |

Response 200 `data`：`BatchPassInspectionOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `passed` | [PartOut](#partout-字段) | 成功送检的件；与 `items` 顺序一一对应（`passed[i]` 对应 `items[i]`） |
| `failed` | [BatchPassFailure](#batchpassfailure-字段) | 失败的 item；单 item 不会同时出现在 `passed` 与 `failed` |

> 整体响应**始终为 200**。item 级别的失败通过 `data.failed[]` 体现（每个 item 含 `code` + `message`，调用方可按 `code` 分支处理）。
> `20111` 仅在 item-level 报出，不影响整批响应状态。

错误码：

- 40001 VALIDATION_ERROR — `items` 缺失 / 空数组 / 超过 200
- 40300 FORBIDDEN — 非 Manager / 非 Inspector
- item-level（出现在 `failed[].code`）：20101 / 20103 / 20104 / 20109 / 20111 / 40901

### `POST /api/v2/parts/{part_id}/pass-inspection`

权限: **Manager / Inspector**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 工单雪花 ID |

Request：可选 body `PassInspectionRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 缺省按 part_id 唯一匹配；多 INSPECTION 批次时必填 |
| `quantity` | i32? | — | 整批送检；`quantity ≤ 0` 或 `quantity > 批次剩余量` → `20111` |

> 当 body 完全省略（`Content-Length: 0`）时，按空对象处理。

Response 200 `data`：[`PartOut`](#partout-字段) — 流转后的工单最新投影

错误码：

- 20101 BIZ_PART_NOT_FOUND — 工单不存在 / 已软删
- 20103 BIZ_INVALID_TRANSITION — part 当前 status 不是 `INSPECTION`（状态机迁移失败）
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
- 20111 BIZ_PART_BATCH_INVALID_QUANTITY — `quantity ≤ 0` 或超过批次剩余量
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败
- 40001 VALIDATION_ERROR — payload shape 错误
- 40300 FORBIDDEN — 非 Manager / 非 Inspector

### `POST /api/v2/parts/worker-scan`

权限: **Manager** / **ShelfAccount**（**scope 校验**：`shelf_id` 与 `target_inspection_shelf_id` 必须在 `current.shelf_ids` 内或 `current.shelf_wildcard=true`；否则 `40301 SHELF_MISMATCH`）

Request：`WorkerScanRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `serial_no` | string | ✓ | 扫码得到的序列号（service 反查 part） |
| `badge_code` | string | ✓ | 工人 badge_code（service 反查 worker） |
| `event_type` | string | ✓ | `"RETURNED"` / `"INSPECTED"`（`WorkerScanEvent` 枚举） |
| `shelf_id` | string (i64) | ✓ | RETURNED 时是 worker-scan 货架（PRODUCTION 区）；INSPECTED 时是工人触发扫码的货架（可为任意 INSPECTION 区货架） |
| `next_process_id` | string (i64)? | — | **仅 RETURNED 必填**；缺 / 非法 → `40001` |
| `target_inspection_shelf_id` | string (i64)? | — | **仅 INSPECTED 必填**；缺 / 非法 → `40001`；service 校验 `zone='INSPECTION'` 且 `is_active=true` |
| `batch_id` | string (i64)? | — | 多批次歧义时 caller 显式指定以消除歧义 |

业务流转：

- **RETURNED**：worker 把 IN_PROCESS+WORKER 批次放回生产架
  - `shelf_id` 必须映射 `next_process_id`（service 校验）→ 不匹配 `20507 BIZ_SHELF_PROCESS_NOT_MAPPED`
  - `part_batch` 与 `part` 状态切回 IN_PROCESS+PRODUCTION_SHELF+holder=shelf（OCC）
  - 写 `RETURNED_BY_WORKER` 事件日志
- **INSPECTED**：worker 把持有件直接送检
  - `target_inspection_shelf_id` 必须属于 INSPECTION 区且 active
  - 不符合 → `20511 BIZ_SHELF_NOT_INSPECTION_ZONE` / `20512 BIZ_SHELF_INACTIVE`
  - part 流转到 INSPECTION 状态
- **任一成功后**同事务调用 `WorkerPoolService::refill_for_worker`：
  - 工人当前工种可加工工序池有候选 → 自动抢满 `work_type.max_held_batches`（或池空为止）
  - 池空 → 业务侧返回 `data.refill.pool_empty=true`，不报错
  - refill 失败（如工种未映射工序）→ 业务错（如 `20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING`），事务回滚 scan 写入

> **OM-6 决议**：scan 与 refill 必须**同事务**——若 scan 成功后再调 refill，期间并发 worker 可能把同一批抢走，破坏「放回 → 抢下一批」原子语义。当前实现已合并到单事务。

Response 200 `data`：`WorkerScanOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `scan` | [`WorkerScanCoreOut`](#workerscancoreout-字段) | 扫码事件最小投影 |
| `refill` | [`RefillResult`](./worker-pool.md#refillresult-字段) | 同事务 refill 结果 |

错误码：

- 20101 BIZ_PART_NOT_FOUND — `serial_no` 无法解析为 part
- 20109 BIZ_PART_BATCH_NOT_FOUND — `batch_id` 不属于该工单 / 已划掉
- 20114 BIZ_PART_BATCH_NOT_HELD_BY_WORKER — `(worker, batch)` 不在 IN_PROCESS+WORKER 持有中（worker 不是该批次当前持有人）
- 20201 BIZ_WORKER_NOT_FOUND — `badge_code` 无法解析为 worker
- 20202 BIZ_WORKER_INACTIVE — worker 已停用
- 20206 BIZ_WORKER_NO_WORK_TYPE — worker.work_type_id IS NULL
- 20901 BIZ_WORK_TYPE_NOT_FOUND — worker.work_type_id 指向不存在的工种（防御性）
- 20904 BIZ_WORK_TYPE_MAX_HELD_NOT_SET — work_type.max_held_batches IS NULL
- 20905 BIZ_WORK_TYPE_NO_PROCESS_MAPPING — work_type 未映射工序（refill 触发）
- 20507 BIZ_SHELF_PROCESS_NOT_MAPPED — `shelf_id` 未映射 `next_process_id`（RETURNED 路径）
- 20511 BIZ_SHELF_NOT_INSPECTION_ZONE — `target_inspection_shelf.zone ≠ 'INSPECTION'`（INSPECTED 路径）
- 20512 BIZ_SHELF_INACTIVE — `target_inspection_shelf.is_active = false`（INSPECTED 路径）
- 40301 SHELF_MISMATCH — `shelf_id` / `target_inspection_shelf_id` 不在 `current.shelf_ids` 内且非 wildcard
- 40300 FORBIDDEN — 非 Manager / 非 ShelfAccount
- 40001 VALIDATION_ERROR — payload shape / 必填字段缺失
- 40901 VERSION_CONFLICT — 并发写，乐观锁失败

WS 广播（commit 后下发）：

- `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（依 `event_type`）—— payload = `WorkerScanCoreOut`
- 若 `refill.taken.len() > 0` → `WORKER_POOL_REFILL_DONE` —— payload = `RefillResult`
- 若 `refill.pool_empty=true` 且 `taken` 为空 → `WORKER_POOL_EMPTY` —— payload `{ worker_id, shelf_id }`

详见 [`./websocket.md`](./websocket.md) 与 [`./worker-pool.md`](./worker-pool.md)。

---

## 共享 DTO

### PartOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID（`serialize_i64`） |
| `serial_no` | string? | 序列号 |
| `name` | string | |
| `drawing_no` | string | 图号 |
| `status` | string | part 状态枚举字符串（`INSPECTION` / `READY_TO_SHIP` 等） |
| `version` | i32 | 乐观锁 |
| `quantity` | i32 | |
| `order_no` | string? | |
| `actual_delivery_date` | date? | 实际交付日 |
| `updated_at` | naive datetime | |
| `updated_by` | string (i64)? | |

### PassInspectionRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_id` | string (i64)? | — | 见端点小节 |
| `quantity` | i32? | — | 见端点小节 |

> 整个 body 可省略，等价于全部字段 `None`。

### BatchPassItem 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | `deserialize_i64` 反序列化 |
| `batch_id` | string (i64)? | — | 同 `PassInspectionRequest.batch_id` |
| `quantity` | i32? | — | 同 `PassInspectionRequest.quantity` |

### BatchPassInspectionRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | [BatchPassItem](#batchpassitem-字段) | ✓ | 1..=`BATCH_PASS_INSPECTION_MAX_ITEMS`（200） |

### BatchPassFailure 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64) | 失败的工单 ID |
| `code` | i32 | item-level 错误码（参见 endpoint `错误码` 节） |
| `message` | string | 失败原因（中文） |

### BatchPassInspectionOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `passed` | [PartOut](#partout-字段) | 成功送检的件 |
| `failed` | [BatchPassFailure](#batchpassfailure-字段) | 失败的件（`passed` ∩ `failed` = ∅） |

### Phase F2（scan-inspect）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScanDecision { PASS, FAIL }

pub struct ScanInspectRequest {
    pub target_inspection_shelf_id: String,  // 雪花 ID（i64 字符串）
    pub decision: ScanDecision,
    pub shelf_id: Option<String>,            // FAIL 必填
    pub next_process_id: Option<String>,     // FAIL 必填
    pub note: Option<String>,                // ≤ 500 字符
    pub batch_id: Option<String>,            // 多批次歧义时必填
    pub quantity: Option<i32>,               // 缺省=整批
}

pub struct BatchScanInspectItem {
    pub part_id: i64,                        // 雪花 ID
    pub decision: Option<ScanDecision>,      // 缺省=PASS
    pub shelf_id: Option<String>,
    pub next_process_id: Option<String>,
    pub note: Option<String>,
    pub batch_id: Option<String>,
    pub quantity: Option<i32>,
}

pub struct BatchScanInspectRequest {
    pub target_inspection_shelf_id: String,
    pub items: Vec<BatchScanInspectItem>,    // 1..=200
}

pub struct BatchScanInspectFailure {
    pub part_id: i64,
    pub code: i32,                           // 业务错误码
    pub message: String,                     // 错误 message
}

pub struct BatchScanInspectOut {
    pub submitted: Vec<PartOut>,
    pub failed: Vec<BatchScanInspectFailure>,
}

pub struct FailInspectionRequest {
    pub shelf_id: String,                    // 必填（PRODUCTION 区 active）
    pub next_process_id: String,             // 必填
    pub note: Option<String>,
    pub batch_id: Option<String>,
    pub quantity: Option<i32>,
}
```

### Phase W（worker-scan + worker-pool 联动）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkerScanEvent { RETURNED, INSPECTED }

pub struct WorkerScanRequest {
    pub serial_no: String,                   // 扫码：序列号
    pub badge_code: String,                  // 扫码：工人 badge_code
    pub event_type: WorkerScanEvent,
    #[serde(deserialize_with = "deserialize_i64")]
    pub shelf_id: i64,                       // worker-scan 货架
    #[serde(default)]
    pub next_process_id: Option<String>,     // 仅 RETURNED 必填
    #[serde(default)]
    pub target_inspection_shelf_id: Option<String>,  // 仅 INSPECTED 必填
    #[serde(default)]
    pub batch_id: Option<String>,
}

pub struct WorkerScanCoreOut {
    pub worker_id: i64,
    pub part_id: i64,
    pub batch_id: i64,
    pub event_type: String,                  // "RETURNED" / "INSPECTED"
}

// WorkerScanOut = { scan: WorkerScanCoreOut, refill: RefillResult }
// （RefillResult 定义在 worker_pool/model.rs，详见 ./worker-pool.md）
```

---

## 端点约束（与 Python 一致）

- **i64 雪花 ID**：JSON 序列化为 `string`，避免 JS `Number.MAX_SAFE_INTEGER` 精度截断（详见 `shared::types`）
- **乐观锁（OCC）**：表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2`，命中 0 行 → `40901 VERSION_CONFLICT`
- **软删除**：`deleted_at IS NULL`；已软删件视为不存在 → `20101`
- **状态机**：详见 [状态机（can_transition_to 白名单）](#状态机can_transition_to-白名单)；不在白名单内的 source / target 组合返回 `20103 BIZ_INVALID_TRANSITION`（迁移表见 `src/modules/part/statemachine.rs`）
- **事件日志**：状态迁移在 service 内事务内统一插入对应事件，service 提交后由 WS 中枢广播

## 状态机（can_transition_to 白名单）

| from | to | 触发场景 |
|---|---|---|
| INSPECTION | READY_TO_SHIP | `pass_inspection` 单/批；`scan-inspect` PASS 分支 |
| PROGRAMMING | INSPECTION | `scan-inspect`（PROGRAMMING 工件） |
| PENDING | INSPECTION | `scan-inspect`（待下发工单） |
| IN_PROCESS | INSPECTION | `scan-inspect`（生产架工件，**必须 IN_PROCESS+PRODUCTION_SHELF**；service 层组合校验） |

INSPECTION → IN_PROCESS 由 `fail_inspection`（推荐需求 3）走 service 流程：
- INSPECTION 状态 + `location='PRODUCTION_SHELF'` + `current_holder_id=shelf.id` + `next_process_id=...`
- 事件日志：`event_type='INSPECTION_FAILED'`

### 货架错误码（20511 / 20512 — scan-inspect / fail-inspection 专用）

| code | 名称 | 触发场景 |
|---|---|---|
| 20511 | BIZ_SHELF_NOT_INSPECTION_ZONE | `target_inspection_shelf.zone ≠ 'INSPECTION'` |
| 20512 | BIZ_SHELF_INACTIVE | `target_inspection_shelf.is_active = false` |

## 未上线端点（前端勿调用）

| Method | Path | 说明 |
|---|---|---|
| POST | `/api/v2/parts` | 创建工单 — 尚未实施 |
| GET | `/api/v2/parts` | 工单列表 — 尚未实施 |
| GET | `/api/v2/parts/{id}` | 工单详情 — 尚未实施 |
| POST | `/api/v2/parts/{id}/update` | 工单修改（OCC） — 尚未实施 |
| POST | `/api/v2/parts/{id}/soft-delete` | 软删 — 尚未实施 |

> 实施后在本节删除并迁移到上文 `端点列表`。

## 参考

- 集成测试：`tests/part_api.rs`（创建→通过品检→展示 READY_TO_SHIP 全链路）
- 仓库分层：`src/modules/part/handler.rs` (axum) → `service.rs` (业务) → `repo.rs` (SQL)
- 状态机：`src/modules/part/statemachine.rs`
- 错误码：`src/shared/error.rs::code`（20101 / 20103 / 20104 / 20109 / 20111 / 20114 / 20201 / 20202 / 20206 / 20507 / 20511 / 20512 / 20901 / 20904 / 20905 / 40001 / 40300 / 40301 / 40901）
- worker-scan 联动：详见 [`./worker-pool.md`](./worker-pool.md)
