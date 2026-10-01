# admin 域 API

> 本文件须与 `src/modules/admin/{mod.rs,handler.rs,service.rs,dto.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域定位：**对账 / 修数据的逃生口**。域内**不含任何新的派生算法**，只复用
> part / assembly 域既有的 rollup 函数把派生缓存重算一遍。
>
> 导航：本页即入口（admin 目前只有 1 个端点）

---

## 端点列表

| Method | Path | 权限 | 说明 | 详情 |
|---|---|---|---|---|
| POST | `/api/v2/admin/recompute-rollup` | **Manager** | 按 `t_part_batch → t_part → t_assembly` 重跑派生算法，回报「检查多少 / 变了多少 / 每条 before → after」 | [下节](#post-apiv2adminrecompute-rollup) |

---

## 三层状态与「派生缓存」

```
t_part_batch.status   ← 唯一真源（谁能写它见 part 域文档）
      │  min-progress 聚合（status_gate::rollup_part_derived）
      ▼
t_part.status / t_part.next_process_id   ← 派生缓存
      │  compute_assembly_target（AssemblyService::sync_from_part_change）
      ▼
t_assembly.status     ← 派生缓存
```

两层派生的正常触发点是「写批次状态」那一个函数
（`part::repo::status_gate::apply_batch_status_change`），它在同一个事务里做完
写 + 派生 + 级联 + 终态序列号释放。**所以新代码不该再用本端点**——本端点只解决
两类派生缓存与真源不一致的情况：

1. **历史漂移**：收口之前有 3 个写点漏调 sync，库里已经存在
   「批次全完成、part 还 IN_PROCESS」这类行，代码再正确也修不了既有数据；
2. **事后漂移**：进程在 commit 与 WS 广播之间被杀、手工 SQL 改过库等极端情况。
   正常路径下次任意 part 流转会自愈，但那个 part 若再也不动，漂移就永久留着。

> 端点**幂等**：同一范围跑第二次必然报 0 变化（这是集成测试
> `tests/part/rollup_recompute.rs` 的硬断言）。

---

## `POST /api/v2/admin/recompute-rollup`

权限：**Manager**（单角色，见下方「为什么是 Manager」）

### Request

**body 可省略**。省略 body（不带 `Content-Type` 头）或传 `{}` = **全量对账**。

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_ids` | string[]? | ❌ | 指定要重算的 `t_part.id` 列表（i64 → JSON 字符串）。**不给 = 不重算 part**（除非整体走全量简写） |
| `assembly_ids` | string[]? | ❌ | 指定要重算的 `t_assembly.id` 列表。**不给 = 不重算装配件**（同上） |
| `limit` | int? | ❌ | 「不限 id」时每个列表最多处理多少行。默认 `1000`，上限 `10000`；`≤0` 或超上限 → `20104` |

作用域（`scope`）由「给了哪些 id 列表」决定，**缺一个不等于全量**：

| 请求 | scope | 实际动作 |
|---|---|---|
| 无 body / `{}` | `ALL` | 全表 part + 全表 assembly（各受 `limit` 约束） |
| `{ "part_ids": [...] }` | `PART_IDS` | 只重算这些 part（父装配件由 status_gate 内部自动级联） |
| `{ "assembly_ids": [...] }` | `ASSEMBLY_IDS` | **只**重算这些装配件，part 段完全跳过 |
| 两个都给 | `PART_IDS+ASSEMBLY_IDS` | 先 part 后 assembly（顺序见下） |

显式 id 列表长度上限 `1000` 个（超 → `20104`）。

**顺序保证**：part 段先于 assembly 段。父装配件的聚合读的是子件**当前**状态，
反序会让本轮刚修正的 part 不被计进父件聚合（要等下一次调用才对齐，破坏
「调一次就收敛」的直觉）。

### Response 200

标准 `R<T>` 信封，`data` 形状：

| 字段 | 类型 | 说明 |
|---|---|---|
| `scope` | string | `ALL` / `PART_IDS` / `ASSEMBLY_IDS` / `PART_IDS+ASSEMBLY_IDS` |
| `parts_examined` | int | 本次实际跑过派生的 part 数 |
| `parts_changed` | int | `t_part.status` 真变了的 part 数 |
| `parts_next_process_id_fixed` | int | `t_part.next_process_id` 被修正的 part 数（status 没变、只有工序指针漂移也计入——那会让派工指到上一道工序） |
| `assemblies_examined` | int | 本次跑过聚合的装配件数 |
| `assemblies_changed` | int | `t_assembly.status` 真变了的数 |
| `truncated` | bool | 命中 `limit` 上限、**还有行没扫到** |
| `changes` | object[] | 逐条 before → after（**只含真变了的**） |

`changes[]` 元素：

| 字段 | 类型 | 说明 |
|---|---|---|
| `level` | string | `PART` / `ASSEMBLY` |
| `id` | string (i64) | 雪花 ID（序列化为字符串） |
| `from` | string | 改前状态 |
| `to` | string | 改后状态 |

示例（part 漂移修正 + 装配件级联）：

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "scope": "PART_IDS",
    "parts_examined": 1,
    "parts_changed": 1,
    "parts_next_process_id_fixed": 0,
    "assemblies_examined": 0,
    "assemblies_changed": 0,
    "truncated": false,
    "changes": [
      { "level": "PART", "id": "9000000000000101", "from": "PENDING", "to": "COMPLETED" }
    ]
  }
}
```

**什么都不需要改时仍返回 200**（不是错误）：`parts_changed = 0`、
`changes = []`。幂等是**成功**语义，前端据此把按钮置灰 / 展示「数据已一致」。

### 附带效果

- part 被推进到终态（`COMPLETED` / `CANCELLED`）时，`rollup_part_derived` 的
  step 4 会一并执行：先归档一条 `t_part_event`（`event_type='SERIAL_RELEASED'`，
  note 记原序列号）再清 `t_part.serial_no`。即对账也补做序列号释放。
  与业务流完全同一段代码，故不会重复释放（每个 part 至多 1 条归档事件）。
- 父装配件进终态时清 `t_assembly.serial_no`（不归档，`t_assembly` 无事件表）。

### 事务 / 并发

- 分块处理：**每 200 行一个事务**（`handler.rs::CHUNK_SIZE`），块间 commit。
  不做成一个大事务的原因：派生写**不走 OCC**（靠行锁串行化），单个覆盖全表的长
  事务会在 commit 前一直占着全部 `t_part` / `t_assembly` 行锁；且某块撞脏数据
  时，前面几块已提交，重试只需重跑失败块。
- 派生层的 OCC 冲突**一律降级为「跳过 + `tracing::warn!`**，不会让本端点失败
  （见 [`../assemblies/index.md`](../assemblies/index.md#子件状态聚合auto-rollup)）。

### WS 广播

**仅在真有变化时**，全部块 commit 之后广播一次：

| kind | payload |
|---|---|
| `ROLLUP_RECOMPUTED` | `{ scope, parts_changed, assemblies_changed, operator_id }` |

幂等空跑**不发**广播（避免大屏无意义刷新）。

### 错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 0 | SUCCESS | 200 | 成功（含「0 变化」的空跑） |
| 40300 | FORBIDDEN | 403 | 非 Manager |
| 20104 | BIZ_INVALID_VALUE | 400 | `limit ≤ 0` / `limit > 10000`；`part_ids` / `assembly_ids` 超过 1000 个 |
| 40100 | UNAUTHORIZED | 401 | 未登录 / token 失效（中间件） |

> 本域**未新增**错误码。参数越界复用 part 域段的 `20104`（不新增槽位，避免
> 跨域错误码表膨胀）。

### 为什么是 Manager 单角色

它是对**全量数据**动手的修数端点：一次误用就能把成千上万行的派生状态改掉。
虽然端点幂等（再跑一次就追平），但**错误权限造成的误用本身无法回滚**。仓库里
另一个「全表 / 跨域影响」的端点（`POST /api/v2/prod/pool/auto-allocate`）也是
Manager 单角色，口径一致。

---

## 参考

- 集成测试：`tests/part/rollup_recompute.rs`（5 用例：part 定点修正 + 幂等 +
  序列号释放归档 / assembly 反向漂移修正 + 幂等 / RBAC 403 且不改数据 /
  无 body = 全量 + `limit` 超上限 400）
- 复用的既有派生实现（**一行算法都没重写**）：
  - part：`src/modules/part/repo/status_gate.rs::rollup_part_derived`
  - assembly：`src/modules/assembly/service/sync_from_part.rs::sync_assembly_status`
    （经 `recompute_assembly_status_by_id` 暴露「从 assembly_id 出发」的入口）
- 守门单测：`src/modules/part/repo/status_gate.rs::write_guard_tests`
  （`cargo test --lib` 跑；扫全 `src/**/*.rs`，除 `status_gate.rs` 外任何人
  写 `t_part_batch.status` 即 CI 失败）
