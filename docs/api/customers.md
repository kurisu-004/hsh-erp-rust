# com::customer 域 API（客户树）

> 域：`com::customer`（源码 `src/modules/com/customer/`）。本文件是该域的**唯一**契约来源，任何字段 / 端点变更必须同步本文件。
> 路由前缀 `/api/v2/com/customers`（`handler::router()`，注册链：`modules/mod.rs` 的 `/api/v2` nest → `modules/com/mod.rs` 的 `.nest("/customers", customer::router())`）。
> ⚠️ 本文件**不依赖任何行号**（别的域的注释曾引用本文的行号，随改动全部失效）；引用本文时只引小节号。

## 0. 一句话

客户是一棵**任意深度**的树（2026-10-17 由固定两层放开，对齐部门架构：集团 / 分厂 / 车间 / 班组…）；**业务单据只能挂叶子**，各域出参上的 `customer_path` 是「第 2 层到叶子」用 `" / "` 拼接的路径（**不含**根）。

## 1. 端点表

| # | 方法 | 路径 | 权限（service 层守卫） | 入参 | 响应 |
|---|---|---|---|---|---|
| 1 | GET | `/api/v2/com/customers` | `require_any_role` = Manager / Clerk / CncProgrammer / Inspector | `CustomerListQuery`（见 §3.1） | `CustomerListOut` |
| 2 | GET | `/api/v2/com/customers/{id}` | 同端点 1 | 路径参数 `id: i64` | `CustomerOut` |
| 3 | POST | `/api/v2/com/customers` | `require_any_role` = Manager / Clerk | `CustomerCreateRequest`（201） | `CustomerOut` |
| 4 | POST | `/api/v2/com/customers/{id}/update` | 同端点 3 | 路径参数 + `CustomerUpdateRequest` | `CustomerOut` |
| 5 | POST | `/api/v2/com/customers/{id}/soft-delete` | 同端点 3 | 路径参数 `id`（OCC 锚取库内 `version`，**请求体不带 version**） | 空（`R<()>`） |

- 全部走统一信封 `R { code, message, data }`。
- 写端点（3/4/5）的事务边界在 handler：`pool.begin()` → service → `tx.commit()`；读端点（1/2）`pool.acquire()` 不开事务。
- i64 一律序列化成 **JSON 字符串**（雪花 ID > 2^53，`JS Number` 会丢精度），入参也只收字符串。

## 2. 层级语义（客户树任意深度）

| 概念 | 判据 | 说明 |
|---|---|---|
| **根**（L1 集团） | `parent_id IS NULL` | 根**必须**带 `serial_prefix`（单个 ASCII 大写字母，DB CHECK + service 双校验，库里全局唯一）；根**可以**有任意多个子节点 |
| **非根**（分厂 / 车间 / 班组…） | `parent_id` 非 NULL | `serial_prefix` 必须为 `NULL`（传了就拒 20104）；`parent_id` 指向**任意层级**的父节点，但该父节点必须**存在且未软删**，否则建单时报 20102（DB 侧对 `parent_id` 无自 FK，只能服务端守） |
| **叶子** | 没有任何未软删的直接子节点 | **业务单据只能挂叶子**：`POST /parts`、`POST /parts/batch`、`POST /assemblies`（含 `/update` 改客户端点）、`POST /outsource-quotes`、送货入单（`POST /com/delivery/note/scan`）都在建单/入单时查 `has_children == false`，有子节点即报 **20104**（消息带客户路径与下级节点数）。⚠️ **已知代价**见 §2.4 |
| **树遍历设施** | `src/shared/customer.rs` | `subtree_ids`（向下纯子树）/ `root_id_of`（向上走到根）/ `has_children` / `paths_of`（批量产出 `root_name` / `own_name` / `customer_path`）。**全仓只此一份**，任何域都不得自己写「上跳一层取父名」的副本 |

### 2.1 「选中某节点」= 纯子树（不是「同根的兄弟」）

列表筛选器（零件 / 装配件 / 外协报价 / 待品检 / 待编程 / 送货单列表）拿到单个 `customer_id` 时，一律展开成**该节点 + 其任意深度的全部后代**（`shared::customer::subtree_ids`），**不含兄弟节点**。

⚠️ 旧的两层实现对 L2 是「自身 + **同 L1 下所有兄弟 L2**」，那条历史包袱已作废（它会把别人的兄弟节点混进筛选结果）。

### 2.2 `customer_path` 的口径（全仓统一）

出参 `customer_path`（必填 `String`）= **第 2 层到叶子**的名字用 `" / "`（前后各一个空格）拼接，**不含**根：

```text
华强(L1, serial_prefix=H)
└─ 终端BG(L2)
   ├─ 手机部(L3)      ← 有子节点 ⇒ 非叶子，单据挂它会被 20104 拒
   │  └─ 影像组(L4)   ← 叶子
   └─ 显示BG(L3)      ← 手机部的兄弟；叶子
```

| 节点 | `customer_name` | `parent_customer_name` / `l1_customer_name` | `customer_path` |
|---|---|---|---|
| 华强 | `华强` | `华强` | `华强` |
| 终端BG | `终端BG` | `华强` | `终端BG` |
| 手机部 | `手机部` | `华强` | `终端BG / 手机部` |
| 影像组 | `影像组` | `华强` | `终端BG / 手机部 / 影像组` |

- ⚠️ `parent_customer_name` / `l1_customer_name` 是**根**名，**不是**「直接父」名。旧形（SQL 自连 `t_customer pc ON pc.id = c.parent_id` 取一层父名）在三层以上的树上会退化成中间层名。
- ⚠️ 本域自己的 `parent_name`（§4）是**另一回事**：它是 `CustomerOut` 上「本节点的直接父名」，只用于前端画树的一层缩进，**不要**拿它当集团名。
- ⚠️ **「取不到」的确切口径**（全仓逐字同款，见 `src/shared/customer.rs` 的 `paths_of` 文档）：map 里没有该 id **只意味着「终点行不存在 / 已软删」** —— 锚点带 `deleted_at IS NULL`，而 `GROUP BY target_id` 对每个未软删终点行**恒产出一组**。**成环 / 超过 `MAX_CUSTOMER_DEPTH` 时 map 里仍有该项**，此时 `path` 是**截断路径**、`root_name` 退化成断链处那个祖先（不是根）；这两种形态只出现在脏数据里（`create` 要求父行存在且未软删、`update` 拒改 `parent_id`）。
- ⇒ 消费侧落**空串**（不是 `null`）的情形**只有**「终点行已软删 / 不存在」这一种，即 `paths.get(&id)` 落空后 `unwrap_or_default()` 的兜底。

### 2.3 各域对客户树的特殊约定

| 域 | 约定 | 是否随层级加深而改变 |
|---|---|---|
| `com::applicant` | 申请人 `customer_id` **必须指向根**（申请人属于集团不属于分厂） | **不变**（判据只看本节点自己是不是根，不看它有没有子节点） |
| `com::delivery_note` | 送货单 `customer_id` **恒为 L1 根**（判定键 / 编号前缀键 / 部分唯一索引 `uk_t_delivery_note_l1_open_draft` 都挂在根上）；送货分组的分组键也是根，但**成员可以是根的任意后代** | 分组键不变；成员由「根的直接子」放宽为「任意后代」 |
| `com::union_list` / `prod::*` / `assembly` / `outsource` | 挂叶子、取根名 + `customer_path` | 是（见 §2.1 / §2.2） |

### 2.4 叶子闸门的**已知代价**：散件一旦挂在「后来被加深」的客户上就改不动

2026-10-17。产品决定是「单据只挂叶子」，但它有一个不对称的后果，现场必须知道：

- **触发形态**：零件建单时客户 C 是叶子；之后有人在 C 下面**又建了一个子客户**，C 成了中间层。此时该零件的每一次「再挂单」（再次 `POST /parts/batch`、被装配件建单引用、送货入单）都会被 20104 拦下。
- **散件无自助修复路径**：`PartUpdateRequest`（`POST /parts/{id}/update`）**没有 `customer_id` 字段**，零件一旦建出来就换不了客户。要改只能走「新建零件」或后台改库。
- **装配件子件尚可连带改挂**：`POST /assemblies/{id}/update` 的 `cascade_sync_from_assembly` 会把父件上的 `customer_id` 一起同步给全部未软删子件（`PartRepo::cascade_sync_from_assembly` 的同步字段含 `customer_id`），而这条路径**带同一条叶子闸门**（`AssemblyRepoTrait::require_leaf_customer`）⇒ 改挂到叶子客户即可连带把子件一起挪走。
- ⇒ 两类零件的修复成本不同（散件 = 只能重建，装配件子件 = 改父件即可）。这是**已知缺口**，不是 bug；要给散件补改挂入口需要先给 `PartUpdateRequest` 加 `customer_id` 并补叶子闸门，属独立立项。

## 3. 入参逐字段

### 3.1 `CustomerListQuery`（GET 列表）

| 字段 | 类型 | 语义 |
|---|---|---|
| `name_like` | string? | 名称模糊匹配；trim 后空串 ≡ 不筛选 |
| `parent_id` | string? | 按直接父过滤。**与 `is_root` 互斥**：`parent_id` 有值时 `is_root` 被忽略 |
| `is_root` | bool? | `true` = 只看根（`parent_id IS NULL`）、`false` = 只看非根；两者皆无 = 不按层级过滤 |
| `limit` | number? | **不传 = 不分页、返回全量**（见 §5）；传了才分页并夹到 `[1, 500]` |
| `offset` | number? | 只在传了 `limit` 时生效，负数取 0 |

⚠️ `parent_id` / `limit` 传非整数串 → 20104（`parent_id 非整数` / 分页参数 clamp 失败）。

### 3.2 `CustomerCreateRequest`

| 字段 | 类型 | 语义 |
|---|---|---|
| `name` | string | 必填，trim 后非空 |
| `parent_id` | string? | 缺省 / 空串 = 建根；给了必须指向存在且未软删的客户 |
| `serial_prefix` | string? | 根必填（非空、单个大写字母、库内唯一）；非根**必须不给**（给了 20104） |

### 3.3 `CustomerUpdateRequest`（全部三态）

| 字段 | 语义 |
|---|---|
| `version` | 必填 OCC 锚，与库内当前 version 不一致 → 40901 |
| `name` | `None` 不改；`Some("")` 拒（20104）；`Some(v)` 改 |
| `serial_prefix` | `None` 不改；`Some(None)` 显式清空（**仅根允许**）；`Some(Some(v))` 改（根必须仍是单个大写字母且不与他人重复） |
| `parent_id` | ⚠️ **不支持改挂**：`Some(..)` 无论内层是什么都被拒（20104「不支持修改 parent_id（客户层级不可移动）」）。改挂是**产品决定**（会连带改序列号派发口径与既有单据的客户归属），不是未实现 |

## 4. 出参逐字段

### 4.1 `CustomerOut`

| 字段 | 类型 | 口径 |
|---|---|---|
| `id` | string | 雪花 ID |
| `name` | string | 本节点单段名 |
| `parent_id` | string \| null | **只上跳一层**（直接父）；根为 `null`。⚠️ 想拿根 id 走 `parent_id` 链自己上溯，别假定一次就够 |
| `parent_name` | string \| null | **直接父**的名字（根为 `null`）。⚠️ 与其它域的 `parent_customer_name`（= 根名）**语义不同**，两者不要互相 cast |
| `customer_path` | string | **第 2 层到叶子**的 `" / "` 拼接（**不含**根），本节点即根时 = 根自己的名字（口径与全仓其它出参逐字同款，见 §2.2）。⚠️ **必填非空键** —— 前端 `customerSchema` 声明成 `z.string()`；取不到时退化为**本节点单段名**（不是空串） |
| `serial_prefix` | string \| null | **只在根上有**（单大写字母）；非根恒 `null` |
| `version` | number | OCC 锚 |
| `created_at` / `updated_at` | string | `NaiveDateTime` 原生序列化 |

### 4.2 `CustomerListOut`

`items: CustomerOut[]` + `total`（**过滤后**的行数，与分页无关）+ `limit` + `offset`。全量模式下 `limit` 回填**实际行数**、`offset` 恒 `0`。

## 5. 分页口径：不传 `limit` = 全量

2026-10-17 起列表**默认不分页**（旧的 `DEFAULT_LIMIT = 50` 已删除）。

- 客户列表的语义是「**整棵树**」而不是「一页行」：前端 `listCustomers()` 不传 `limit`，把整份列表当全集用（拼树 / 反查祖先 / 生成 label）。旧口径下节点一多就**静默截断** —— 筛选器里凭空少一批客户且**零报错**，比报错更难查。
- 显式传 `limit` 才分页，并 `clamp(1, 500)`；`offset` 负数取 0。
- 排序恒为 `id ASC`（旧的 `ORDER BY parent_id NULLS FIRST, id` 只在两层树上恰好让兄弟连续，深度放开后不再成立）。

## 6. 错误码

| 码 | 常量 | HTTP | 触发场景 |
|---|---|---|---|
| `20102` | `BIZ_CUSTOMER_NOT_FOUND` | 404 | 查 / 改 / 软删一个不存在或已软删的客户；**建单时 `parent_id` 指向不存在或已软删的父客户**（防造断链） |
| `20113` | `BIZ_CUSTOMER_IN_USE` | 409 | 软删时 `t_part` / `t_assembly` 仍有非软删行引用它 |
| `20124` | `BIZ_CUSTOMER_HAS_CHILDREN` | 400 | 软删时有**任意深度**的未软删子节点（消息带下级节点数） |
| `20104` | `BIZ_INVALID_VALUE` | 400 | `name` 空；`serial_prefix` 非单个大写字母 / 非根却给了 / 根却没给；`serial_prefix` 重复；`parent_id` 非整数；不支持改 `parent_id`；软删根时试图清 `serial_prefix` |
| `40901` | `VERSION_CONFLICT` | 409 | 改单时请求体 `version` 与库内当前值不一致（软删端点不收 version，按库内值条件删） |

⚠️ `20124` 与 `20113` 是**两道独立闸门、顺序固定**：先子节点、再单据引用（2026-10-17 新增子节点闸门，旧实现**完全不查** `parent_id`，靠前端显式级联 —— 任意深度下那套隐式约定已经不成立，且单据挂叶子，级联会连带软删已被引用的节点）。
⚠️ `20109` 在 Python 时代被 `BIZ_PART_BATCH_NOT_FOUND` 与 `BIZ_CUSTOMER_IN_USE` 双重占用；Rust 里 `20113` 是 `BIZ_CUSTOMER_IN_USE` 的**独占**槽位。

## 7. 前端配套清单

| 前端消费方 | 读的字段 / 行为 |
|---|---|
| `composables/queries/schemas.ts::customerSchema` + `customerListResultSchema` | `CustomerOut` 全字段（`parent_name` 只用于画一层缩进；`customer_path` 画客户树的面包屑；`serial_prefix` 用于建根时的输入框显隐） |
| 客户管理页（`views/customers/`） | 全量列表 → 前端拼树；`create/update/soft-delete` 三条写端点 |
| 零件 / 装配件 / 外协报价 / 送货单 / 报工台等**筛选器** | `GET /com/customers` 拉全集，客户端按 `parent_id` 组树；把选中节点的 id 作为 `customer_id` 传给各域列表端点（后端负责展开成纯子树，见 §2.1） |
| 各域列表页的客户列 | 读各域 VO 上的 `customer_path` / `l1_customer_name`（**不是**本域的 `parent_name`），见 §2.2 |

⚠️ **部署顺序：后端必须先上**。本域**有**键级依赖了：`CustomerOut.customer_path` 是前端
`customerSchema` 的**必填**字段，旧版本后端不返这个键 ⇒ `useCustomersQuery` 的守门
`parse` 抛 ZodError ⇒ `data === undefined` ⇒ 客户一览页 + **全站所有客户筛选器**
（零件 / 装配件 / 外协 / 送货单 / 报工台）集体空白且**零红色报错**。
另：**各域 VO 新增 `customer_path` 的那些域（inspection 队列 / 扫码树、programming、
assembly 详情、`dashboard` snapshot 的交期三桶）前端是 `.strict()` / 必填键 schema
⇒ 后端先上会让对应页面在前端 schema 跟进前白屏**（见 §8）。两个方向都有依赖，
发布时前后端必须同批或按「后端先上」排。

## 8. 跨域影响清单（改本域前先看）

| 消费域 | 依赖的东西 |
|---|---|
| `part` / `assembly` / `outsource` | `subtree_ids`（筛选展开）、`root_id_of`（序列号派发取根的 `serial_prefix`）、`has_children` + `paths_of`（叶子闸门与三列客户名） |
| `prod::queue` / `prod::scan` | `subtree_ids`（报工台取件页筛选）、`paths_of`（候选卡三列） |
| `prod::inspection` / `prod::programming` | `subtree_ids`（队列 / 待编程筛选）、`paths_of`（`customer_path` 与根名） |
| `prod::batch`（返修列表） | `paths_of`（`customer_name` 承载 `customer_path`、`l1_customer_name` 承载根名） |
| `dashboard` / `com::delivery_note` | `paths_of`；送货单另有 `root_id_of`（由零件客户上溯到根确定判定键） |
| `com::applicant` | **只**判「是不是根」（`parent_id IS NULL`），不依赖遍历设施 |
