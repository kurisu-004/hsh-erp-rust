# shelf 域 API（货架 CRUD + 负载与自动选架）

> 本文件是 `shelf` 域的**唯一**契约来源。任何字段 / 端点变更必须同步本文件。
> 与本轮同批改动的域契约见 [`batch.md`](batch.md)（worker-scan 链尾自动送检）、
> [`queue.md`](queue.md)（refill 去架锚 + 取件优先级统一）、
> [`outsource.md`](outsource.md)（移动端点方向 C 删除）。

## 0. 2026-10-10 变更摘要

本轮把「所有落货架的写操作都由人挑一个货架」改成「**服务端按负载自动挑**」。四件事：

1. **新增存储列** `t_shelf.capacity`（整数，件数上限）。`NULL` 或 `<= 0` = **不限**，
   选架排序时恒排最后。migration **不做 backfill** —— 存量货架容量未知，留 `NULL`
   走退化路径（见 §4.2）。
   ⚠️ 加列 migration 的**版本号必须与已有 migration 不撞**（本仓格式是
   `<14 位时间戳>_<顺序>_<描述>.sql`，取当日时间戳后要看一眼同目录有没有同前缀的
   序号）。撞了不会在编译期或启动时立刻报，而是 `sqlx` 在 apply 那一刻按唯一版本号
   插 `*_sqlx_migrations` 时撞主键、返 `23505`。另外**不要在分支内给已提交的
   migration 改名** —— 改名的版本号对任何已 apply 过它的库都是「缺了一条记录」，
   启动直接 `VersionMismatch` panic；只有在那个版本号从未被任何环境 apply 过的
   分支上改名才是安全的。
2. **`ShelfOut` 出参加两列**：`capacity`（裸 `number | null`）与 `current_load`
   （裸 `number`，件数）。`load_ratio` **刻意不进 wire**（理由见 §2）。
3. **两条 picker 端点下线**：`GET /for-return` / `GET /for-inspection`（见 §5）。
4. **选架算法收进跨域设施层** `crate::shared::shelf`（零域依赖），8 条写路径改调它；
   选架的失败语义由**调用方**决定错误码（`20508` / `40301`），本域不参与。

## 1. 端点表

| # | 方法 | 路径 | 权限 | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/shelves` | Manager + Clerk + CncProgrammer + ShelfAccount + Inspector | `code_like?`、`zone?`、`is_active?`、`limit?`（缺省 50，clamp 1..500）、`offset?` | `ShelfListOut` |
| 2 | GET | `/api/v2/shelves/{id}` | 同上 | path `id`（雪花 ID 字符串） | `ShelfOut` |
| 3 | POST | `/api/v2/shelves` | **Manager 独占** | `{ code, name, zone, location?, capacity?, display_order? }` → 201 | `ShelfOut` |
| 4 | POST | `/api/v2/shelves/{id}/update` | **Manager 独占** | `{ name?, location?, capacity?, display_order?, version }` | `ShelfOut` |
| 5 | POST | `/api/v2/shelves/{id}/deactivate` | **Manager 独占** | 无 body | `R<()>` |

- 全部返回统一信封 `R { code, message, data }`。
- 端点 1 **不接受**别的 query 参数（传了被忽略）。
- 端点 4 的 `version` 是 OCC 锚，**必填**（无 `#[serde(default)]`）：缺字段走 axum 的
  `JsonRejection` → **HTTP 422 纯文本、不进 `R<T>` 信封**；与库中现值不符 → `40901`。
- 端点 2 的 `{id}` 抽不出数字 → axum `PathRejection` → **HTTP 400 纯文本**。
- i64 雪花主键（`ShelfOut.id`）序列化为 JSON **string**；`capacity` / `display_order`
  是 i32 计数，序列化为裸 JSON **number**。

### 1.1 路由注册顺序

`/{id}` 是 catch-all。现有 5 条路由里 `/` 是 1 段静态、`/{id}` 与 `/{id}/update` /
`/{id}/deactivate` 中后两条是**两段**路径（与 `/{id}` 不同形）⇒ 当前无同段位争用。
**若将来再加单段静态路径（如 `/search`），必须插在 `/{id}` 之前**（axum matchit 0.8
按注册顺序消歧）。

## 2. 逐字段

### 2.1 `ShelfOut`

| 字段 | JSON 类型 | 说明 |
|---|---|---|
| `id` | string | `t_shelf.id`（雪花，`serialize_i64`） |
| `code` | string | 业务唯一键（`uk_t_shelf_code`，活跃行唯一） |
| `name` | string | 显示名 |
| `zone` | string | `PRODUCTION` / `INSPECTION` |
| `location` | string \| null | 物理位置描述 |
| `is_active` | boolean | `deactivate` 后恒 false |
| `display_order` | number | 物理顺序（选架退化路径的排序键，见 §4.2） |
| `capacity` | number \| null | **2026-10-10 新增**：负载上限（件数）。`null` 或 `<= 0` = 不限 |
| `current_load` | number | **2026-10-10 新增**：在架**件数**（`SUM(t_part_batch.quantity)`），**不是**批次数 |
| `version` | number | OCC 锚 |
| `created_at` / `updated_at` | string | ISO 时间 |

⚠️ **`capacity` 不套 `serialize_i64`**：那个 helper 把 id 序列化成字符串是为了防 JS
`Number.MAX_SAFE_INTEGER` 精度截断（19 位雪花 ID），而 `capacity` 是 i32 量级的计数，
发字符串会让前端表单控件拿到 `"200"` 而不是 `200`。

⚠️ **`load_ratio` 刻意不进本 VO**（前端自己用 `current_load / capacity` 算）：

1. 浮点进 JSON 对前端不友好（前端还要自己定精度与四舍五入位数）；
2. `null` 的语义在 wire 上冗余 —— `capacity === null || capacity <= 0` 已完整表达
   「不限」，多一列只会让后端前后端口径漂移时出现「`capacity=0` 但 `load_ratio=null`」
   这种自相矛盾的响应；
3. 写死的比例在 `current_load` 之后立刻过时 —— 让前端按当次的两个整数现算，永远自洽。

### 2.2 `ShelfCreateRequest`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | 是 | 空 / 全空白 → `20104` |
| `name` | string | 是 | 空 / 全空白 → `20104` |
| `zone` | string | 是 | 非 `{PRODUCTION, INSPECTION}`（大小写不敏感，会 trim + 转大写）→ `20104` |
| `location` | string | 否 | trim 后全空白视为无值 |
| `display_order` | number | 否 | 缺省 `0` |
| `capacity` | number \| null | 否 | **2026-10-10 新增**。`<= 0` **被接受**（按「不限」处理），不报 `20104` |

⚠️ **`capacity <= 0` 不报错是刻意的**：「`NULL` 或 `<= 0` = 不限」是选架排序的口径，
加 CHECK 约束或 service 层 `> 0` 校验都会把这个语义钉死成非法值。零 / 负值与 `NULL`
在选架侧行为完全相同，没有理由区别对待。

### 2.3 `ShelfUpdateRequest`

| 字段 | 类型 | 三态语义 |
|---|---|---|
| `name` | string | 缺省不改；`Some("")`（空串 / 全空白）→ `20104` |
| `location` | string \| null | 三态 |
| `capacity` | number \| null | **三态且真的三态**（2026-10-10 新增） |
| `display_order` | number | 缺省不改 |
| `version` | number | **必填**（OCC 锚） |

`capacity` 的三态：字段**缺省** ⇒ 不改；`null` ⇒ 清空成「不限」；给值 ⇒ 改上限。
这是靠 `#[serde(deserialize_with = "deserialize_some")]` 做到的 —— `Option<Option<i32>>`
用裸 derive 时 JSON `null` 与「字段缺省」**都**反序列化成外层 `None`，`Some(None)`
分支不可达（已实测确认）。

⚠️ **同文件的 `location` 声明了三态但当前不可达**：`Option<Option<String>>` 缺了那个
`deserialize_with`，所以 `service/crud.rs` 里 `Some(None) ⇒ 清空` 那个分支走不到 ——
今天发 `{"location": null}` 的实际效果是**不改**。本轮**不改** —— 加上
`deserialize_some` 会让「清空」这个动作第一次开始生效，属于行为变更，要先与前端确认
有没有调用方在依赖当前的 no-op。

## 3. 口径表

| 口径 | 定义 | 备注 |
|---|---|---|
| `current_load` | `SUM(t_part_batch.quantity)`，状态集 `IN ('PENDING','IN_PROCESS','INSPECTION','OUTSOURCE')` 且 `deleted_at IS NULL`，按 `current_holder_id` 分组 | **件数**，不是批次数。唯一的 SQL 真源是 `shared::shelf::load::LOAD_AGGREGATE_SQL` |
| `current_load` 为什么不是存储列 | 它是 `t_part_batch` 的聚合，存一份就要在每次批次流转时维护并保证一致 | 只读时聚合，几十个货架的代价可忽略 |
| 软删闸门 | 聚合子查询**自己**带 `deleted_at IS NULL` | 外层 `t_shelf` 带了不算数；不过滤则软删批次的 quantity 被永久计入 |
| `capacity` 的「不限」 | `NULL` 或 `<= 0` | 排序里恒排最后 |

## 4. 选架算法（`shared::shelf::select::pick_least_loaded`）

本域**不**实现选架；它是 `crate::shared::shelf::select` 的算法，本域只提供「被选」的
货架数据。这里记口径，供前端与运维理解「为什么落到了那个架」。

### 4.1 候选集与排序

候选：`zone = 目标区` ∧ `is_active` ∧ `deleted_at IS NULL` ∧（给了 `process_id` 时）
存在 `t_shelf_process` 中该工序的未软删映射 ∧（`shelf_scope_for(&current)` 给了 scope
时）`id = ANY(scope)`。

排序（4 段，缺一不可）：

1. `capacity IS NULL OR <= 0` 的架排最后（显式一段，不依赖 PG 的 `NULLS LAST` 默认值）；
2. `current_load / capacity` 升序（`COALESCE(load.cnt, 0)` 不可省 —— 空架的
   `LEFT JOIN` 未命中会排到有货的架之后，与「空架最该被选中」正好相反）；
3. `display_order ASC`；
4. `id ASC`（稳定兜底：同样的输入恒选到同一个架）。

**超载不拒**：全部候选都 ≥ 100% 时仍取比例最低的那个。拒收会让一批货既不能上架也不能
送检，只能靠人工找一个已满的架手动放行 —— 与自动选架的目标相反。

### 4.2 退化路径

候选集里**全部**架都没配 `capacity` 时，第 1 段全部为 1、第 2 段全部为 0，排序自然
退化到 `display_order ASC, id ASC`（=「按人工排的物理顺序取第一个可用架」）。

⚠️ 这与 2026-10-10 之前 dispatch 用的 `t_shelf_process.sort_order ASC, id ASC`
**不是同一套排序**（映射 `sort_order` vs 货架 `display_order` 是两套独立的人工排序），
同一道工序在两种口径下可能落到不同的架。`ShelfProcessRepo::find_first_shelf_for_process`
（那套旧口径）因此**保留但已无调用方**，下一轮决定删还是复用为显式退化路径 ——
登记见 [`queue.md`](queue.md) §8.4。

**存量货架的 `capacity` 全为 NULL**（migration 不 backfill），故生产库现状下选架恒走
这条退化路径。要让负载均衡真正生效，需要在货架管理页（端点 4）给货架配容量。

### 4.3 `shelf_scope_for`

| 账号形态 | scope |
|---|---|
| `shelf_wildcard = true` 或有 `Role::Manager` | `None`（不限） |
| 有 `Role::ShelfAccount`（货架一体机 / 绑定账号） | `Some(shelf_ids)`，**空数组原样保留** |
| 其余（`Clerk` / `Inspector` / `CncProgrammer` …） | `None`（不限） |

⚠️ 第三条**刻意**与读侧 `part::service::phase1::work_type::pickable_shelf_scope`
（只有两条分支）分歧：`shelf_ids` 只对 `ShelfAccount` 角色填（见
`iam::service::session::resolve_roles_and_scope`），所以一个从未被授予货架范围的
`Inspector` 的 `shelf_ids` 恒为 `[]`。若照读侧那样收窄，它的 scope 就是 `Some([])` ⇒
选架候选恒空 ⇒ `to-inspection` / `dispatch` / `place-on-shelf` 一律 `40301` ——
而这些端点的角色白名单里 `Inspector` / `Clerk` 恰恰是**主要使用者**（送检就是品检员
做的事）。读侧不受影响（那里把无货架范围的 Clerk 收窄成空列表无害）。

空数组**必须**原样返回 `Some([])`：`ANY('{}')` 对任何货架都为假 ⇒ 候选为空 ⇒
「未绑架的 SHELF_ACCOUNT 看不见任何架」。误判成 `None` 会让它拿到全厂货架。

### 4.4 跨域依赖登记

`shared::shelf` 是本仓**零域依赖**的 shared 模块：只 import
`crate::auth` / `crate::infra` / `crate::shared` / `crate::state`，表数据一律自己写
SQL 在本层聚合。理由与对照（`shared::batch` 为什么反而依赖 4 个域）见
`src/shared/shelf/mod.rs` 顶部注释。

代价是同一张 `t_shelf` 在本域 `repo/sql.rs` 里另有一份查询 —— 但**负载聚合只有一份**
（`LOAD_AGGREGATE_SQL`），本域的列表 / 详情走 `ShelfRepoTrait::load_by_ids` 委托它。

## 5. 移除记录（2026-10-10）

### 5.1 端点

| 被移除项 | 原因 | 替代者 |
|---|---|---|
| `GET /api/v2/shelves/for-return` | picker 的存在意义是「让人挑一个最空的架」；自动选架上线后该动作消失。保留它等于保留自动选架的旁路（调用方可以指定任意架，从而绕过选架的 scope 与映射守卫） | 服务端 `pick_least_loaded`；调用方不再需要选架 |
| `GET /api/v2/shelves/for-inspection` | 同上 | 同上 |

⚠️ **「下线」的响应形态是 400 而不是 404**：本域还挂着 `/{id}`（`Path<i64>`），所以
`/for-return` 现在落进那个 catch-all 并在 Path 提取器阶段被拒 ⇒ **400 + 纯文本**
`Invalid URL: Cannot parse \`for-return\` to a \`i64\``，**不进 `R<T>` 信封**。无论哪种
都不是 200、都不会返回货架数据。回归见
`tests/shelf/api.rs::picker_endpoints_are_gone`。

连带删除：`service/picker.rs` 整个文件、`dto.rs::ShelfForReturnQuery`、4 个 picker VO
（`ShelfForReturnItem/Out`、`ShelfInspectionItem/Out`）、`repo/sql.rs` 的
`list_active_production_ordered` / `list_active_inspection_with_load` 与行结构
`TShelfWithLoad`、`ShelfRepoTrait` 的同名两方法（trait 11 → 9 方法）。

### 5.2 字段

本域**没有**字段被移除。`capacity` 是新增。

## 6. 与 WS 的关系

本域**不发任何 WS 事件**（纯 CRUD）。货架相关的 WS 广播由写路径的调用方负责：

- `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`（`prod::batch::handler::transition`
  的 worker-scan handler，按 `scan_out.event_type` 广播 —— 链尾自动送检会让「请求
  `RETURNED` / 广播 `WORKER_SCAN_INSPECTED`」成立，链路因此自动正确）；
- `OUTSOURCE_MOVE_DONE`（外协移动端点）；
- `WORKER_POOL_REFILL_DONE` / `WORKER_POOL_EMPTY`（refill；`WORKER_POOL_EMPTY` 的
  payload 里 `shelf_id` 自 2026-10-10 起恒为 `null`）。

dashboard 侧对以上事件名的监听与处理见 [`dashboard.md`](dashboard.md)。

## 7. 表依赖与前端配套

### 7.1 读的表

`t_shelf`（本域）、`t_part_batch`（负载聚合）、`t_shelf_process`（选架的工序过滤）、
`t_user_role`（读侧 scope，不在本域）。

### 7.2 前端配套改动清单（2026-10-10）

| 改动 | 说明 |
|---|---|
| 删掉 picker 调用点 | `src/api/shelves.ts` 里 `for-return` / `for-inspection` 两个请求函数 |
| 删掉 `shelfId` 表单项 | 所有「落货架」的表单不再让用户选架 |
| 货架管理页加 `capacity` 输入 | 端点 3 / 4 传 `capacity`；端点 2 的回显可展示 |
| 货架列表展示 `current_load` / `capacity` | 百分比由前端现算：`capacity && capacity > 0 ? Math.round(current_load / capacity * 100) : null` |
| `GET /shelves` 的 TS 类型加两字段 | `capacity: number \| null`、`current_load: number` |

**与后端是人工同步关系，无编译期保障**。

### 7.3 已知偏差登记

- **`location` 的三态声明未实现**（§2.3）—— 已知，本轮刻意不改。
- **`ShelfOut` 无 `load_ratio`**（§2.1）—— 有意，不是遗漏。
- **选架退化路径与旧 `sort_order` 口径不等价**（§4.2）—— 已知，待下一轮决策。
- **`POST /shelves/{id}/update` 的 `version` 缺字段返回 422 纯文本**（不在 `R<T>` 信封
  内）—— axum `JsonRejection` 的全仓统一行为，非本端点特例。