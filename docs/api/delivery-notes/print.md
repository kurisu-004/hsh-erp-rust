# delivery-notes / 打印

> 本目录条目须与 `src/modules/delivery_note/handler/print.rs` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：本文件覆盖 2 个端点（`POST /api/v2/delivery-notes/{id}/print` /
> `POST /api/v2/delivery-notes/{id}/print-labels`）。Rust 侧强制 JWT + RBAC 之后，
> **读本单批次算装配件可出货套数**并把结果注入转发 body，再把请求送到 Python 后端；
> xlsx 的生成仍由 Python 端执行。Rust 端不开事务、不改 DB、不解析前端字段、
> 不二次包装响应。
> part 域的 2 个 PDF 打印端点（**纯**转发形态，无 DB 读）见
> [`../parts/print.md`](../parts/print.md)。
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · [`workflow.md`](./workflow.md) · **`print.md`**

## 本文件目录

1. [鉴权](#鉴权)
2. [行为（BFF 转发 + 套数注入）](#行为bff-转发--套数注入)
3. [POST /api/v2/delivery-notes/{id}/print  （P4 打印）](#post-apiv2delivery-notesidprint--p4-打印)
4. [POST /api/v2/delivery-notes/{id}/print-labels  （P4 标签打印）](#post-apiv2delivery-notesidprint-labels--p4-标签打印)
5. [Python 错误码透传](#python-错误码透传)
6. [rust 端错误码](#rust-端错误码)
7. [env 配置](#env-配置)
8. [Header 透传策略](#header-透传策略)
9. [实现要点](#实现要点)

---

## 鉴权

- 强制 JWT 鉴权（Bearer token），由 `v2_router` 全局 `authenticate_middleware` 处理；
  缺 / 坏 / 过期 token → 40100 / 40102 / 40105。
- RBAC：`require_any_role([MANAGER, CLERK, INSPECTOR])`，不通过 → 40300 FORBIDDEN。
  `SHELF_ACCOUNT`（货架终端）不放行——它只该扫码，不该开单打印。
- Python 端这 2 个 `/api/v1` 端点**自身无应用层鉴权**，谁能把请求送到它们就等于拿到
  打印能力。因此本仓提供的**唯一受支持入口**是本文件覆盖的 2 条 `/api/v2` 路径
  （`/print` 与 `/print-labels`）：强制 JWT + RBAC 在 Rust 侧判定通过，Rust 才会把
  请求转给 Python。
- 鉴权凭据**不**透传给 Python：`Authorization` / `Cookie` 被 `filter_request_headers`
  剥离，Python 侧只能读到 Rust 注入的 `X-Forwarded-User-Id`。Python 端因此不需要、
  也不应该自己解析 JWT。
- `/api/v1` **不是**受支持的对外入口。Python 后端只应在内网经 `PYTHON_BACKEND_BASE_URL`
  被触达（compose 服务名 / 集群内网），对外暴露面以 `/api/v2` 这道闸门收口。
- 反向代理模板（nginx 配置）归 `frontend/` 子模块所有，不在本仓描述其内容与状态；
  本仓可核实的契约边界就是上面这几条。

## 行为（BFF 转发 + 套数注入）

### v2 → v1 映射

| v2（Rust 对外） | v1（Python 现状） | Method |
|---|---|---|
| `POST /api/v2/delivery-notes/{id}/print` | `{PYTHON_BACKEND_BASE_URL}/api/v1/delivery-notes/{id}/print` | POST |
| `POST /api/v2/delivery-notes/{id}/print-labels` | `{PYTHON_BACKEND_BASE_URL}/api/v1/delivery-notes/{id}/print-labels` | POST |

这 2 条与 Python 端**同名同路径段**（part 域的 2 条打印端点不同名，见
[`../parts/print.md`](../parts/print.md)）。

### Rust 侧注入的两个键

打印端点**不再是纯转发**：转发前 Rust 读本单批次，算出每个装配件的
**可出货套数**，覆盖写入 body 的两个键。

**覆盖契约（定死，无歧义）**：`merge_quantities` 一旦被写入，就是**整体替换**
该键（`assembly_ids` 同理），不是逐键 merge。前端发来的同名字段**全部作废**，
Python 端不会看到任何前端值（前端已不再发这两个键，见「跨仓生效前提」第 3 条）。
唯一的例外是「本单没有可解析装配件」时两个键都不写、body 原样转发 —— 但那时
`assembly_map` 为空，Python 端也用不到它们。

| 键 | 类型 | 谁写 | 说明 |
|---|---|---|---|
| `assembly_ids` | [string (i64)] | **Rust 注入** | 本单批次所属、且能解析到的装配件 id（未软删）。Python 端用它组装 `assembly_map`；`None` / 空 → 不做装配件合并 |
| `merge_quantities` | {string (i64): i32} | **Rust 注入** | `{ "<assembly_id>": <可出货套数> }`。**值为 0 表示该装配件凑不齐整套，其子件不进 xlsx** |

计算口径（`src/modules/delivery_note/service/shippable_sets.rs`，
**只统计本单**批次）：

```text
child_note_qty(c) = Σ 本单上子件 c 的 b.quantity          (i64，本单无批次 = 0)
per_set(c)        = child_note_qty(c) * asm.quantity / c.quantity
sets(asm)         = LEAST(COALESCE(MIN(per_set(c) for c ∈ asm 的**全部**子件), 0),
                           asm.quantity)                → i32
```

⚠️ **`min` 的定义域是「该装配件的全部子件」**，不是「本单出现过的子件」。
本单完全没交批次的子件以 `child_note_qty = 0` **参与** `min` ⇒ 必然把该装配件
压到 0 套。业务规则原文「剩余的部分不能单独发货，需要等待其他子零件收集齐组装为
装配件出货」：凑不齐整套就不能发。此时 Python 端拿到 0 会丢掉该装配件的全部子件行，
不会印出物理上不存在的整套。与 part 列表「已送套数」同源（`COALESCE(SUM, 0)`），
差别只在分子是**本单**而非全局已送。

边界：

- `part.quantity == 0` 的子件**不参与** `min`（对应 SQL 的 `NULLIF`）；
- 装配件**无参与子件**（没有子件 / 全部子件 `quantity = 0`）→ `0` 套
  （`COALESCE` 在 `LEAST` 里面，写反会在「子件总量全为 0」时返回
  `asm.quantity`，与「全零 → 0 套」正好相反）；
- 软删子件不参与（取子件时统一 `include_deleted = false`）；
- **`不按批次状态过滤`（与全局已送口径有意不同）**：本单口径的分子只要求批次挂在
  本单（`PartBatchRepo::list_with_part_by_delivery_note` 只过滤 `deleted_at`）；
  part 列表的 `shippable_sets`（`fetch_delivered_sets`）额外要求
  `b.status IN ('DELIVERED','COMPLETED')`。⇒ DRAFT 单上若挂着 `INSPECTION` 状态
  的批次（入单校验允许 `INSPECTION` / `READY_TO_SHIP` 进单，见
  `service/inner.rs`），本单口径会把它算进去、全局口径不会。这是**多算**方向
  （缺件仍压到 0，不会凭空多出整套），且与打印语义自洽。**不要**给本单口径补状态
  过滤：那会让 DRAFT 单在 `INSPECTION` 阶段就打印出 0 套，与详情 VO 同源同值的
  约束冲突。理由详见 `src/modules/delivery_note/service/shippable_sets.rs` 模块文档；
- `LEAST(..., asm.quantity)` 顺带收口子件超交（不会出现「100 / 10 套」），
  并消除 int8→int4 溢出（中间量用 i64，收口后钳到 i32）；
- PG 整数除法向零截断。

**与详情只读字段同源同值**：`GET /api/v2/delivery-notes/{id}` 的
`line_items[].shippable_sets`（前端预览显示的套数）与本处注入的
`merge_quantities`（实际导出 xlsx 的套数）由**同一个纯函数**算出，且三条调用链
（打印 handler / 详情 / 批量详情）传入的子件集合同口径（都取「该装配件的全部
未软删子件」）。契约测试：`tests/print_forward.rs`
`detail_shippable_sets_match_injected_merge_quantities`。

**不注入、原样转发的 4 种情形**（BFF 层不新造失败路径）：本单无批次 / 本单无
装配件 / 本单引用的装配件全部软删或不存在 / `body` 不是 JSON object。
「送货单不存在」的 404 仍由 Python 侧 `BIZ_DELIVERY_NOTE_NOT_FOUND` 兜。

⚠️ 雪花 id 一律是 JSON **string**（> 2^53，JSON number 会丢精度）：`assembly_ids`
的元素与 `merge_quantities` 的 key 都是 string；`merge_quantities` 的 value 是普通
JSON number（套数是计数，不是 id）。

### 跨仓生效前提（缺一即静默空操作）

注入的 `merge_quantities` 要真正影响 xlsx，**下列 3 件事必须同时成立**，任一不成立
都不会报错、只是套数注入退化成空操作（xlsx 回落成「每套装配件 1 套」）。2026-10-04
三仓齐发后三条均已满足；仍逐条列出，是为了让后来人改动其中任一环时能立刻意识到
会再次静默失效：

1. **前端必须发 `merge_assemblies = true`**。Python 端 `_build_print_rows` 在
   `merge_assemblies` 为假时**直接早退逐行输出**，`merge_quantities` 根本不被读取。
   `PrintPreviewDialog.vue` 的 `mergeMode` 初值是 `'merge'`
   （`ref<'separate' | 'merge'>('merge')`，`onConfirm` 里 `mergeFlag = true`），即
   **默认就是合并模式、默认发 `merge_assemblies = true`**，本条前提当前**已满足**；
   只有操作员主动切到「分开打印」时才是 `false`，**那个分支下注入是空操作**。
2. **Python 端 `PrintDeliveryNoteRequest` 必须有 `assembly_ids` 字段**。
   2026-10-04 已满足：Python 端补上了
   `assembly_ids: list[str] | None = Field(default=None, max_length=500)`
   （`api/v1/delivery_note_print.py`），新增 `_parse_assembly_ids` 把 str 列表解析成
   service 层的 `list[int]`（非法 / 空 id → 400，不静默丢弃），`/print` 与
   `/print-labels` 两个 handler 都改为取 body 值透传给 `service.print*`（原先硬传
   `assembly_ids=None`）⇒ `assembly_map` 不再恒空，装配件合并真正生效。
   ⚠️ **三仓版本必须齐发**：`/api/v1/*` 无公网入口、pydantic 对未知字段默认静默
   丢弃，所以 Python 端一旦回退该字段或漏发本次改动，Rust 侧**不会报错**、注入照样
   成功，只是 `assembly_map` 仍为空，结果与本次修复前完全一样。
3. **前端不得再引入手工 override**。2026-10-04 已满足：前端的
   `PrintPreviewDialog.vue` 装配件行 `el-input-number` 与
   `merge_quantities[asm] = r.quantity` 发送均已删除，`PrintNotePayload` 不再带
   `merge_quantities`；父行数量改为只读展示后端算出的 `shippable_sets`（该值缺失渲染
   「—」、真 0 渲染「0 套」）。⚠️ 若前端重新加回「手填装配件套数」的输入并随请求
   发送，它会被 Rust 整体覆盖，操作员却以为自己填的值生效。

### `POST /api/v2/delivery-notes/{id}/print`  （P4 打印）

Request（body 是 `Json<Value>`：前端字段原样透传，Rust 侧不解析、不做 schema 校验；
唯二被 Rust 改写的是上面两个注入键）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 行顺序（批次 id 序列）。**每个 part 只承认其「代表批次 id」**（该 part 在本单最小的 `b.id`），且必须**覆盖本单全部 part** —— 漏行 / 非代表 id / 不属于本单都 → Python 侧 422 `BIZ_DELIVERY_PRINT_BAD_ORDER`。缺省走 Python 端默认顺序（`b.id ASC`） |
| `merge_assemblies` | bool? | — | true → 同装配件子件合并一行（缺省 `false`）。⚠️ 无 `assembly_ids` 时合并不生效（`assembly_map` 为空）；**且为 `false` 时 Python 端早退逐行、`merge_quantities` 完全不被消费 ⇒ 注入是空操作**（见「跨仓生效前提」） |
| `assembly_ids` | [string (i64)]? | — | **Rust 注入**（见上） |
| `merge_quantities` | object? | — | **Rust 注入**（见上）：`{ "<assembly_id>": <可出货套数> }`，值为 0 = 该装配件子件不进 xlsx。**总是整体覆盖**前端同名字段 |
| `line_item_ids` | [string (i64)]? | — | 标签端点专用：只打这些批次行（`line_items[].id`）；见下一节 |

Response：Python 的响应原样透传（status + body + 经清洗的 headers）。成功形态：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: inline; filename="delivery-note-{prefix}{delivery_note_no}.xlsx"`
- Body: xlsx 二进制

### `POST /api/v2/delivery-notes/{id}/print-labels`  （P4 标签打印）

Request：同 `/print` 的字段（含同一套注入键 —— 两个端点在 Python 端共用同一份行
构建逻辑，套数口径必须一致），另加：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `line_item_ids` | [string (i64)]? | — | 只打这些批次行（`line_items[].id`）；缺省 = 全部数据行，传空数组 → Python 端 400 |

Response：Python 的响应原样透传。成功形态：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: inline; filename="labels-{prefix}{delivery_note_no}.xlsx"`
- Body: xlsx 二进制

> 文件名由 Python 端按「客户 `serial_prefix` + 送货单 `delivery_note_no`」生成
> （`api/v1/delivery_note_print.py`），disposition 是 `inline`（浏览器内联渲染而非强制
> 下载）。前端 `parseFilename`（`src/api/deliveryNote.ts`）直接读这个 header，取不到时
> 回落到 `note-{id}.xlsx` / `label-{id}.xlsx`。

## Python 错误码透传

下列错误码在本链路由**Python 端产出、原样透传**，Rust 端不产出、不改写、不二次包装。
清单以 Python 仓 `core/error_code.py` 的 `ErrCode` 为准，Python 端新增码时以那儿的
定义为准：

| code | 名称 | 语义 |
|---|---|---|
| 20104 | BIZ_INVALID_VALUE | `/print-labels` 传了 `line_item_ids=[]`（空数组） |
| 21109 | BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED | 未按客户 prefix 配出 xlsx 模板 |
| 21113 | BIZ_DELIVERY_PRINT_BAD_ORDER（HTTP 422） | `custom_order` 不满足 reps 口径：**漏行**（未覆盖本单全部 part）/ **非代表 id**（该 part 只承认本单最小的 `b.id`）/ **不属于本单**的批次 id —— 逐条对应上表 `custom_order` 行 |
| 21401 | BIZ_DELIVERY_NOTE_NOT_FOUND（HTTP 404） | 送货单不存在 |

表中的 2 个 211xx 模板码与 21401 在 `src/shared/error.rs::code` 里**仍然注册**
（保留与 Python 错误码表对齐），但 Rust 端已无生产点：链路上的它们全部来自 Python。
两处计数的口径不同，别混：本表只列**这 2 个打印端点**当前由 Python 产出的码；
211xx 段内「已无 rust 生产点」的**全段**清单是 4 个码（21109 / 21111 / 21112 / 21113），
见 [`../index.md`](../index.md)。Python 侧还会返回哪些码以 Python 端为准，
Rust 侧不预判、不枚举；前端按 `{code, message, data}` 解封即可。

## rust 端错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20407 | BIZ_PRINT_FORWARD_FAILED | 502 | rust → python 网络层失败：连接拒 / 超时 / 读 body 失败 / `PYTHON_BACKEND_BASE_URL` 未配置（`NoopPyBackend` 占位） |
| 40800 | REQUEST_TIMEOUT | 408 | 打印路径超过 `PRINT_REQUEST_TIMEOUT_SECONDS`（缺省 660s）—— 由 `middleware::timeout` 返回，带标准信封 |
| 50001 | DATABASE | 500 | 转发前读本单批次 / 装配件 / 子件失败（`state.pool.acquire()` 与 3 条 repo 查询的 `?`）。DB 抖动/连接池耗尽时出现，**可重试** |

> 50001 是 2026-10-04 随「转发前读 DB 算套数」新增的失败路径（此前这 2 个端点
> 只读取数之外什么都不做，故表里只有 20407 / 40800）。注意它**不是** 20407：
> 20407 = rust 转发不到 Python；50001 = rust 根本没能读到本单数据、请求没发出去。
> 该路径**刻意不做降级**（不降级成「不注入任何键」），理由见下方实现要点。
>
> 20407 与 STS 转发的 20406 `BIZ_STS_FORWARD_FAILED` 是两条链路的独立命名：前端与
> 日志能直接看出挂的是 STS 还是打印。
>
> 40800 刻意留 60s 边际：打印执行方是 Python（600s 超时），Rust 自己的 HTTP 超时若与
> 它同时到点，先被杀的是 Rust，Python 侧真实的 502 就再也浮不上来，用户只看到一句
> 无信息量的「请求超时」。

## env 配置

| env | 说明 | 缺省 |
|---|---|---|
| `PYTHON_BACKEND_BASE_URL` | Python 后端 base URL（如 `http://backend:8000`）；设置即 `enabled=true`，未设 / 留空 → `NoopPyBackend`。⚠️ 降级**不**拒启，调用时才以 20407 暴露 | 空 |
| `PYTHON_PRINT_TIMEOUT_MS` | 打印转发的单次请求超时（毫秒）。不能与 STS 的 10s 通道同档 | `600_000` |
| `PRINT_REQUEST_TIMEOUT_SECONDS` | 打印路径的 HTTP 请求级超时（秒）。**须大于** `PYTHON_PRINT_TIMEOUT_MS` 换算的秒数 | `660` |

超时只作用于这 2 条打印路径：`middleware::timeout::is_print_path` 精确命中
（首段 + 段数 + 尾段字面量的组合匹配，**不用裸后缀匹配**——否则将来别的域加个
`/print` 短端点就会静默继承长超时）。其余 `/api/v2/*` 仍走
`REQUEST_TIMEOUT_SECONDS`（缺省 30s）。

## Header 透传策略

**请求侧**

- 注入：`X-Forwarded-User-Id: <CurrentUser.id>`（Python 端 STS 已信任该头；打印端点目前不读）。
- 保留：`X-Request-Id` + 其它自定义业务头（trace 在 前端 nginx → rust → python 一致）。
- 剥离：`Authorization` / `Cookie`（不让 Python 端反向依赖 Rust 的 JWT）/
  `Host` / `Content-Length` / `Content-Type`（reqwest 自管：`.headers()` 是**追加**语义，
  放行前端那份会与 `.json()` 自己写的那条并存）/ hop-by-hop 全套
  （`Connection` / `Keep-Alive` / `Transfer-Encoding` / `Upgrade` / `Te` / `Trailer`）。

**响应侧**

- 保留：`content-type`（前端靠它区分 xlsx / PDF）/ `content-disposition`
  （前端 `parseFilename` 靠它取下载文件名）/ `cache-control`（Python 端给的语义照搬）/
  其余自定义头。
- 剥离：hop-by-hop 全套 + `content-encoding` + `date` / `server`（上游 server 的
  自我标识，Rust 自己会写）。
- `content-encoding` 之所以能安全剥：reqwest 开了 `gzip` feature，发请求时带
  `accept-encoding: gzip`，收到 gzip 响应时在**解码层**把 body 还原成明文（并顺手
  摘掉 `content-encoding` / `content-length`）。**解码与剥头必须成对**——只剥不解会
  让前端拿到「声明 xlsx 实为 gzip 流」的坏文件且无报错。
- **重算**：`content-length` 一律按实际 body 长度。打印响应可能是数 MB 的 xlsx，
  长度与实际不符时前端 `responseType: 'blob'` 的下载会被截断，表现为「下到一个坏
  文件」且无报错，极难排查。

## 实现要点

- **只读取数、不开 tx、不改 DB**：handler 内 `state.pool.acquire()` 拿连接（读端点
  范式，同 `handler/crud.rs`），跑只读查询即 drop。读数**仅**用于注入转发
  body，不参与任何业务写入。
- **读失败 fail-loud，绝不降级成「不注入」**：DB 读失败一律 `AppError::Database`
  （50001 / HTTP 500），不 catch 后原样转发。理由：降级会让 Python 端回落到
  「每套装配件默认 1 套」（`_build_print_rows` 里 `(merge_quantities or {}).get(asm_id, 1)`；
  该回落**只在 Rust 未注入该键时**发生，正常链路上该键恒由 Rust 写入）
  ⇒ 静默打出**错标签**，用户拿到一份看起来正常但套数全错的 xlsx。打印错标签比
  打印失败难查得多。
- **取数 3 步，不新增 `query!` 宏**：① `PartBatchRepo::list_with_part_by_delivery_note`
  （本单未删批次 × 工单）② `AssemblyRepo::list_by_ids(..., include_deleted=false)`
  （装配件）③ 子件 —— 打印 / 单单详情按装配件逐个 `PartRepo::list_children`（单单
  装配件通常 1~3 个，N 次小查询可接受，先例 `service/scan/mod.rs`）；批量详情
  （N 单 × M 装配件）用 1 条 SQL 的 `PartRepo::list_children_by_assemblies`（非宏
  `sqlx::query_as`，不进 `.sqlx/` 离线缓存）。两条取子件路径同 `include_deleted=false`
  口径。
- **套数只有一个纯函数**：`service::shippable_sets::note_shippable_sets`，三处调用
  （打印注入 / `line_items[].shippable_sets` / 批量详情）共用。它的入参只含
  `装配件 id → quantity` 与 `装配件 id → 全部子件`，不含 `TAssembly` 整行
  （算套数不需要装配件的其它字段，调用方就不必 clone 整行）。
- **前端字段原样透传**：`Json<Value>` 透传，不定义强类型 DTO、不解析前端字段
  （雪花 id 是 string，解析只是引入一层无收益的转换）。唯二被改写的是
  `assembly_ids` / `merge_quantities` 两个注入键（整体覆盖）。
- **不新增 404**：本单查不到数据时原样转发，让 Python 侧产出既有错误码
  （`BIZ_DELIVERY_NOTE_NOT_FOUND` 等），避免 BFF 层多出一条与上游不一致的失败路径。
  ⚠️ 这条只针对「查得到但查不出东西」（无批次 / 无装配件 / 全软删）；**连接失败与
  查询报错不在此列**，那些是 50001。
- **注入的 DB 读不做用户货架 scope 过滤**：delivery_note 域本就无 user-scope 校验，
  打印端点连单是否存在都不查（404 交给 Python）。当前**不泄漏** —— 响应体来自
  Python 端自己的单据查询，Rust 算出的套数只写进转发 body。将来若任何响应回显套数，
  必须先补 scope 过滤。
- **身份单头传递**：handler clone 一份 `HeaderMap` 再注入 `X-Forwarded-User-Id`
  （不能直接 mutate extractor 给的那份，会污染共用同一 `HeaderMap` 的其它
  extractor / middleware）。
- **响应不 gzip**：`/api/v2` 的 `CompressionLayer` 谓词 = tower-http `DefaultPredicate`
  （< 32 字节 / `image/*` / gRPC / SSE 不压缩）**且**排除 xlsx 的 content-type
  （已压缩格式再 gzip 只是白烧 CPU，PDF 同样排除）。用 `.and()` 组合而非替换，因为
  `compress_when` 是替换语义，只写排除项会把默认谓词的 4 条保护一起丢掉。
- **幂等跳过**：打印路径被 `middleware::idempotency` 跳过（前端打印请求不带
  `Idempotency-Key`；大体积 xlsx 响应也不该进 Redis 缓存）。
- **无 WS 事件**：打印端点不广播 WS 事件。
