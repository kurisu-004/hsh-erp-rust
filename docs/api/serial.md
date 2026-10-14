# serial 域 API（一键释放终态工单占用的序列号）

> 本文件是 `serial` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 代码位置：`src/modules/serial/`（`mod.rs` · `handler.rs` · `service.rs` · `repo.rs` · `dto.rs`）。
> 权威路由清单：`handler::router()`（1 条）。
> 集成测试：`tests/serial/`（binary `serial`，5 条用例）。

## 0. 需求与范围

**用户原话**：「当前还有残留已完成的的装配件但占用序列号的情况，创建一个 api 用于一键释放已完成工单的序列号」。

经与用户确认的范围：

- **两张表都清** —— 装配件（`t_assembly.serial_no`）+ 零件 / 子件（`t_part.serial_no`）。
- **返回计数 + 明细** —— 交互形态是「`dry_run` 预览 → 二次确认 → 实跑」。

### 0.1 本域**做**什么（1 条端点，全在 §1）

把**全部**「状态已是终态、但 `serial_no` 仍占着」的工单一次清空，并如实报出
「清了多少行」+「清的是哪些行」。

### 0.2 本域**不做**什么（明确划界，防后人误接）

| 不做 | 理由 |
|---|---|
| **不建序列号池 / 状态机** | 全仓**没有** `t_serial_no` 表，也没有 `FREE` / `BOUND` / `USED` 枚举。序列号就是两张既有表上的两列，唯一性靠 partial unique index（§8.2）。本域**只做减法**（置 `NULL`），不重新设计序列号的生命周期 |
| **不做「按条件选一部分」释放** | 候选判据写死为「终态 + 持号」，不可由客户端传。传筛选条件就变成「批量改数据」的通用口子，而这个端点的破坏性已足够（§8.3 第 1 条） |
| **不做分页** | 一次全表扫、明细封顶 200 行。分页对「一次性维护动作」没有意义 |
| **不提供单条释放端点** | 单条释放已由既有路径覆盖：工单正常流转到终态时，状态机派生链会自动清号（§4.2）。本域只补**从未走过那条链**的历史欠账 |
| **不写归档事件到装配件侧** | `t_assembly` 没有事件表，而它的 `note` 列是**用户可编辑的业务备注**（§4.2） |

## 1. 端点表（**1 个**）

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | POST | `/api/v2/serials/release-terminal` | `require_role(Manager)` | JSON `{"dry_run": bool}`；`dry_run` 缺省 `false` | 200 + `ReleaseTerminalOut` |

**无硬切记录**（本域 2026-10-19 新立，无任何旧路径、无 alias）。

### 1.1 错误码

| 码 | HTTP | 条件 |
|---|---|---|
| `40300` `FORBIDDEN` | 403 | 角色不是 MANAGER（ADMIN 元角色由 `require_role` 内部短路放行）。守卫**先于**任何全表扫 |
| `40001` `VALIDATION_ERROR` | 422 | body 不是合法 JSON / `dry_run` 不是布尔 |
| `50001` | 500 | DB 错误（唯一可能的实际形态：`SELECT … FOR UPDATE` 与并发 DDL 冲突） |

本端点**不产生任何业务码**（`20xxx` / `21xxx`）：它没有「找不到」形态（不存在即 0 行，
幂等返回 200），也没有「校验失败」形态（候选判据写死在服务端）。

### 1.2 守卫顺序（逐条 early-fail，顺序固定）

1. `current.require_role(Role::Manager)?` —— **必须早于候选 SELECT**。本端点会全表扫
   `t_assembly` + `t_part` 两张表，不该让无权账号拿它做信息探测（有多少行泄漏着序列号）。
   判在 `service::release_terminal` 第一行（不是 handler 第一行）：service 是唯一能保证
   「候选 SELECT 之前」的位置。

## 2. 出参 `ReleaseTerminalOut` 逐字段

| 字段 | 类型 | 语义 |
|---|---|---|
| `dry_run` | bool | 与入参 `dry_run` **原样回显**。前端据此确认「这次到底是预览还是实跑」 |
| `assembly_count` | i64 | 本次清空的**装配件**行数（`t_assembly`） |
| `part_count` | i64 | 本次清空的**零件 / 子件**行数（`t_part`） |
| `released_total` | i64 | `assembly_count + part_count`。**真实总数，不受明细上限截断** |
| `truncated` | bool | `released_total > 200` ⇒ `affected` 是不完整列表 |
| `affected` | `ReleasedSerialItem[]` | 明细，**最多 200 行** |

### 2.1 `ReleasedSerialItem` 逐字段

| 字段 | 类型 | 语义 |
|---|---|---|
| `kind` | `"assembly"` \| `"part"` | 行所属表 |
| `id` | **string** | 雪花 id。⚠️ **必须序列化成字符串** —— 超出 JS `Number.MAX_SAFE_INTEGER`，前端一律按字符串处理（同 `assembly` 域 `force_complete` 响应的 `assembly_id`） |
| `serial_no` | string | 被释放掉的原序列号。恒非空（候选判据带 `serial_no IS NOT NULL`） |
| `drawing_no` | string | 图纸号。⚠️ **恒非空**：`t_assembly.drawing_no` 与 `t_part.drawing_no` 在 baseline 里都是 `NOT NULL`（两表皆然，无后续 migration 放开该约束）⇒ 出参**不是** `Option` |
| `status` | string | 释放时该行的状态，必为 `COMPLETED` 或 `CANCELLED`（判据写死） |

### 2.2 明细排序与封顶

- **装配件在前、子件在后**，各自按 `id` 升序（`ORDER BY id`）。
- **总封顶 200 行**（`dto::RELEASE_DETAIL_LIMIT`）—— 装配件段 + 子件段拼起来再 `take(200)`。
- ⚠️ **装配件优先是刻意的**：子件序列号由父装配件派生（`{serial_no}-{i:02d}`），
  释放**父件**序列号的影响面更大（连带一批子件的号段一起让出来）⇒ 明细与用户心智都
  应先看装配件。代价：极端形态（装配件残留极多、子件很少）下子件会被挤出上限 ——
  `part_count` 仍如实报出子件总数（§4.1）。

## 3. 入参 `ReleaseTerminalRequest` 逐字段

| 字段 | 类型 | 缺省 | 语义 |
|---|---|---|---|
| `dry_run` | bool | `false` | `true` = 只读预览（**零写入**、不开事务、不广播 WS）；`false` = 真跑 |

⚠️ **`dry_run` 缺省为 `false`（真跑）**：`{"dry_run": false}` 与**整个 body 传空对象
`{}`** 都等价于「真跑全量清理」。这是破坏性操作最危险的默认值，登记在 §8.3 第 1 条
（前端必须先预览再确认）。

## 4. 口径表

### 4.1 ★ 「计数」与「明细」是**两套解耦**的口径（本端点最核心的对外契约）

| 口径 | 来源 | 受 200 上限影响？ |
|---|---|---|
| `assembly_count` / `part_count` / `released_total` | 真实候选集合（`dry_run` 走 `COUNT(*)`，apply 走锁内候选集 `.len()` / 原语 `rows_affected`） | **否** |
| `affected` | 同一候选集合 `take(200)` | **是**（并置 `truncated = true`） |
| **实际写入行数** | 同计数口径，**与 `affected` 长度无关** | **否** |

⚠️ **写入永远不受明细上限约束**：apply 路径的候选 SELECT **不加 `LIMIT`**
（`repo::list_*` 的 `limit` 形参传 `None`）。若把上限顺手套到写入上，就会出现
「5000 行残留只清得掉 200 行，而 `released_total` 报 5000」——
**报出去的数和实际发生的事对不上**，比不限流严重得多。
集成测试 `truncated_flag_and_limit` 专门钉这条（种 201 行、断言实跑后**残留持号行 = 0**）。

### 4.2 候选判据（两张表**逐字相同**）

```sql
deleted_at IS NULL
AND serial_no IS NOT NULL
AND status IN ('COMPLETED', 'CANCELLED')
```

- 终态 = `COMPLETED | CANCELLED`（与两域状态机一致：`shared::batch::status::is_terminal`）。
- 该谓词在 `repo::TERMINAL_PREDICATE` **单点定义**，count / 两条 list SELECT 共用同一常量。
- ⚠️ **必须与两个既有释放原语内联的谓词同源**，否则会出现「明细列了一行、实跑没清它」：

  | 侧 | 原语 | 附加语义 |
  |---|---|---|
  | 装配件 | `AssemblyRepo::clear_serial_no_if_terminal` | `version + 1`；**不写归档事件**（`t_assembly` 无事件表，`note` 是用户可编辑业务字段） |
  | 零件 / 子件 | `shared::batch::status::release_part_serial_no` | **先读原值 → 写 `t_part_event`（`event_type = 'SERIAL_RELEASED'`）→ 再清列** + `version + 1` |

  ⇒ **本域一行写 SQL 都不写**，清列全部委托给这两个原语。这与 `docs/api/batch.md`
  里「写 `t_part_batch.status` 只能走 `apply_batch_status_change`」是同一条纪律：
  自写 UPDATE 会立刻分叉出三处（归档事件丢失 / `version` 不递增 / 守卫口径漂移）。
  **实测验证**：把 `repo::TERMINAL_PREDICATE` 的 status 条件去掉后，
  `apply_releases_both_tables_and_archives_parts` 仍绿 —— 因为复用原语**自身**的
  守卫把 IN_PROCESS 行挡住了。谓词自身由 `dry_run_returns_counts_without_writing`
  的计数断言独立钉住。

### 4.3 并发口径（apply 路径 `SELECT … FOR UPDATE`）

候选 SELECT 带 `FOR UPDATE`，把行锁**锁在事务内**，再逐行清列。理由：否则「选出候选」
与「清列」之间，并发进来的工单流转可能把 `status` 改掉，导致计数与实动行数对不上。

⚠️ **不用「先 `UPDATE … RETURNING` 再判」**：零件侧必须**先归档后清**
（`release_part_serial_no` 内部要先读出 `serial_no` 才能写进归档 `note`）；
一旦先 UPDATE 把列清了，那个原语内部那次读拿到 `NULL` ⇒ 走「已清空 → 无需归档」
早退分支，**归档事件永久丢失**。

**代价**（登记在 §8.3 第 2 条）：候选极多时长时间持锁。这是维护型端点
（MANAGER 单角色、人工触发），可接受。

### 4.4 雪花 `event_id`：每个 part 各拿一个**独立** id

`t_part_event.id` 是 pkey。`release_part_serial_no` 的 `event_id` 形参**必须**传
`Some(真实雪花 id)`：

- 传 `None` 走「只清不归档 + `tracing::error!`」降级分支 —— 那条是为「caller 拿不到
  生成器」准备的兜底，本域明明有 `state.snowflake` 却降级是白白丢审计行。
- ⚠️ **不能复用同一个 id**：一次会清几百上千个 part，复用必然撞 pkey（23505）
  ⇒ **整个事务回滚，一行都清不掉**。

`to_status` 取自锁内 SELECT 的 `status`，即该行**释放时刻**的真实状态。

### 4.5 事务与广播

| 分支 | 事务 | WS 广播 |
|---|---|---|
| `dry_run = true` | **不开**，`pool.acquire()` | **不广播**（零写入） |
| `dry_run = false` | `pool.begin()` → service → `tx.commit()` | commit **之后** 一条汇总事件 |

开事务的理由：整批候选的清列 + 子件归档事件必须**全成功或全回滚**。半途失败留下
「一部分清了、一部分没清」，比一行没清更难排查。

## 5. 状态域约定（无编译期保障）

`status` 是 `varchar(20)`，**无 DB ENUM**。本端点依赖的两个终态字面量
（`COMPLETED` / `CANCELLED`）在下列位置**各有一份**，改任何一处都要同步：

| 位置 | 用途 |
|---|---|
| `serial::repo::TERMINAL_PREDICATE` | 本域候选判据 |
| `shared::batch::status::is_terminal` | part 侧终态判据（私有） |
| `modules::assembly::repo::sql::clear_serial_no_if_terminal` 内联 SQL | 装配件侧清列守卫 |

⚠️ 「某个工单是否该被本端点清」**完全由这三处字面量一致**保证，没有编译期检查。

## 6. 移除记录

| 旧物 | 处置 | 说明 |
|---|---|---|
| （无） | — | 本域 2026-10-19 新立，**从未有过旧路径、无 alias、无端点下线** |

**但下列「曾经存在过的相邻物」本仓同样没有，别去重建**：

| 物 | 为什么它不存在 |
|---|---|
| `POST /api/v2/admin/recompute-rollup` 等 `admin` 域批量端点 | 2026-10-13 随整个 `admin` 域删除（`CLAUDE.md` 路由声明规约第 9 条有订正记录）。本域**不是**它的复活 |
| migration `20261001000200_007_serial_release.sql` 里那段手写批量清号 SQL | 那是**一次性数据修复**，不是运行时能力。本端点把它自动化（同样处理两张表），但**不删**那段 migration —— 它已执行过，是历史账 |
| 任何「序列号池」端点（分配 / 回收 / 查占用） | 全仓**没有** `t_serial_no` 表，序列号只有两张表上的两列（§0.2） |

## 7. 与 WS 的关系

实跑成功后广播**一条** `TERMINAL_SERIALS_RELEASED`：

```json
{ "kind": "TERMINAL_SERIALS_RELEASED", "payload": { "assembly_count": 1, "part_count": 2 } }
```

- **一条汇总事件，不是逐行 N 条** —— 整表扫下逐行就是几千条噪音帧。
- **不进 `AFFECTS_DASHBOARD` 白名单**（`views/dashboard/composables/useDashboardInvalidation.ts`
  的前端白名单）：序列号列**不进** dashboard 三张报表（`snapshot` / `upcoming` /
  `delivery_orders`）的任何一个切片 ⇒ 这条事件对大屏**零驱动作用**。
  ⚠️ 前端若将来要消费它，需要显式加白名单；**别默认「WS 发了前端就会刷新」**。
- 广播在 **commit 之后**（commit 失败却已广播 = 大屏看到一次从未发生的清理）。
- `dry_run` **不广播**。

## 8. 表依赖与前端配套

### 8.1 读的表（2 张，都只读 `serial_no` / `status` / `drawing_no` / `id`）

| 表 | 角色 | 备注 |
|---|---|---|
| `t_assembly` | 候选 + 清列（经既有原语） | 装配件 |
| `t_part` | 候选 + 归档 + 清列（经既有原语） | 零件 / 子件 |

附带写 `t_part_event` 一行（经既有原语）。**本域不新增任何表 / 列 / 索引 / migration。**

### 8.2 唯一性来源（本域存在的理由）与**两表谓词不对称**

| 索引 | 谓词（**现行**，非 baseline） |
|---|---|
| `uk_t_assembly_serial_no` | `serial_no IS NOT NULL AND deleted_at IS NULL` —— **无 status 谓词** |
| `uk_t_part_serial_no` | `serial_no IS NOT NULL AND deleted_at IS NULL AND status <> 'CANCELLED'` |

⚠️ **两张表的谓词不一样，且 part 侧是迁移 `20261001000200_007_serial_release.sql`
改过的**（baseline 时它只有 `serial_no IS NOT NULL`，缺 `deleted_at`；该迁移同时补了
`deleted_at` 并加了 `status <> 'CANCELLED'`）。别照 baseline 推断现行谓词。

**这个不对称决定了「谁卡住了取号」**：

| 行形态 | 还占着唯一键吗 | 本端点会清吗 |
|---|---|---|
| 装配件 `COMPLETED` / `CANCELLED` 持号 | **会**（装配件索引无 status 谓词） | 会 |
| 零件 `COMPLETED` 持号 | **会** | 会 |
| 零件 `CANCELLED` 持号 | **不会**（`status <> 'CANCELLED'` 已把它排除） | 会（但**不解除任何占号**） |
| 任一表「已软删 + 持号」 | **不会**（两索引都带 `deleted_at IS NULL`） | 不会（候选判据同样带 `deleted_at IS NULL`） |

⇒ **本端点最紧急的价值在装配件的 `COMPLETED` 行**（唯一真正把新工单取号挡死的形态）。
零件 `CANCELLED` 行照样清，是**顺带的对齐**（用户明确要求两表都清），不是刚需；
清它只是让 `serial_no` 列不再留一个没用的值 + 顺带写归档事件。

### 8.3 已知偏差登记

1. **`dry_run` 缺省为 `false`（真跑）**：空 body `{}` 等价于全量清理。
   破坏性操作的危险默认值。前端**必须**先 `{"dry_run": true}` 预览、用户确认后再
   `{"dry_run": false}`。本域**刻意不**改成「缺省 `true`」—— 那会让「忘了传参」
   变成静默无操作，比误清理更难察觉。
2. **`FOR UPDATE` 在超大量候选下长时间持锁**（§4.3）。未做分批 / 超时保护。
   维护型端点下可接受；生产残留若已达万级需评估。
3. **软删 + 持号的行不会被清理**：候选判据带 `deleted_at IS NULL`，两个唯一索引
   也都带 `deleted_at IS NULL` ⇒ 这类行**既不占号、也不会被本端点清**。
   两者一致，**没有隐患**，但也意味着「软删行残留序列号」是本端点**够不到**的死角
   （要清它必须单开一个「含软删行」的维护变体，**本轮不做**）。
4. **零件 `CANCELLED` 行的清理不解除任何占号**（§8.2）：`uk_t_part_serial_no` 带
   `status <> 'CANCELLED'`，这类行本就不占唯一键。清它只是让列不留废值 + 写归档事件。
   本端点**照清**（用户明确要求两表都清），但别把它算进「救回来的号」里。
5. **明细封顶 200 且装配件优先**（§2.2）：装配件残留极多时子件会被挤出明细，
   需靠 `part_count` 才知道子件也清了多少。
6. **装配件侧无归档事件**：`t_assembly` 没有事件表，且 `note` 是用户可编辑业务字段
   （写系统动作会污染用户数据）⇒ 装配件的清号**没有审计行**，只有 `version` 递增。
   这是既有原语的设计沿用，本域**刻意不**为此新建事件表。

### 8.4 前端配套改动清单

**后端本轮已完成，前端尚未接入**（截至 2026-10-19）。前端需要：

1. 一个 MANAGER-only 的「释放终态序列号」入口（按钮）—— ⚠️ 后端守卫是
   `require_role(Manager)`，前端入口可见性按同口径判。
2. 确认框：**先**发 `{"dry_run": true}` 拿 `ReleaseTerminalOut`，展示
   `released_total`（总数）+ 两个分表计数；`truncated == true` 时**必须**额外提示
   「仅显示前 200 条」。
3. 用户确认后发 `{"dry_run": false}`。
4. 出参的 `id` 是**字符串**，前端不要 `Number(id)`。

### 8.5 参考

- 端点级归属裁决与 `/batches` 先例：`src/modules/serial/mod.rs` 模块 doc。
- 事务边界 / WS 时序：`src/modules/serial/handler.rs` 模块 doc。
- 复用两个原语的理由：`src/modules/serial/service.rs` 模块 doc。