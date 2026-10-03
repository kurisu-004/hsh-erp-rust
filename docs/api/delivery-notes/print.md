# delivery-notes / 打印

> 本目录条目须与 `src/modules/delivery_note/handler/print.rs` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 范围：本文件覆盖 2 个端点（`POST /api/v2/delivery-notes/{id}/print` /
> `POST /api/v2/delivery-notes/{id}/print-labels`）。两者都是**纯转发**：Rust 侧强制
> JWT + RBAC 之后把请求原样送到 Python 后端，xlsx 的生成由 Python 端执行。Rust 端
> 不读 DB、不开事务、不解析请求字段、不二次包装响应。
> part 域的 2 个 PDF 打印端点（同样是转发形态）见 [`../parts/print.md`](../parts/print.md)。
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · [`workflow.md`](./workflow.md) · **`print.md`**

## 本文件目录

1. [鉴权](#鉴权)
2. [行为（BFF 转发）](#行为bff-转发)
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
- Python 端这 2 个端点**本身无鉴权**，安全性来自部署层：nginx 不把 `/api/v1` 转发到
  公网，Python 只在 compose 内网经 Rust 触达。⚠️ 该设计对部署配置有依赖——恢复
  `/api/v1` 的公网转发块会让这 2 个端点等于裸开。

## 行为（BFF 转发）

### v2 → v1 映射

| v2（Rust 对外） | v1（Python 现状） | Method |
|---|---|---|
| `POST /api/v2/delivery-notes/{id}/print` | `{PYTHON_BACKEND_BASE_URL}/api/v1/delivery-notes/{id}/print` | POST |
| `POST /api/v2/delivery-notes/{id}/print-labels` | `{PYTHON_BACKEND_BASE_URL}/api/v1/delivery-notes/{id}/print-labels` | POST |

这 2 条与 Python 端**同名同路径段**（part 域的 2 条打印端点不同名，见
[`../parts/print.md`](../parts/print.md)）。

### `POST /api/v2/delivery-notes/{id}/print`  （P4 打印）

Request（body 是 `Json<Value>`，**原样透传**，Rust 侧不解析字段、不做 schema 校验）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 批次 id 序列；缺省走 Python 端的默认顺序 |
| `merge_assemblies` | bool? | — | true → 同装配件子件合并一行（缺省 `false`） |
| `merge_quantities` | object? | — | `{ "<assembly_id>": <count> }`，按装配件 id 覆盖合并行数量 |

⚠️ 雪花 id 一律是 JSON **string**（> 2^53，JSON number 会丢精度）：Rust 侧若解析成
number 就是一次有损转换，故全链路按 string 透传，由 Python 端解析。

Response：Python 的响应原样透传（status + body + 经清洗的 headers）。成功形态：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-note.xlsx"`
- Body: xlsx 二进制

### `POST /api/v2/delivery-notes/{id}/print-labels`  （P4 标签打印）

Request：同 `/print` 的 3 个字段，另加：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `line_item_ids` | [string (i64)]? | — | 只打这些批次行（`line_items[].id`）；缺省 = 全部数据行，传空数组 → Python 端 400 |

Response：Python 的响应原样透传。成功形态：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-labels.xlsx"`
- Body: xlsx 二进制

> 两个端点的文件名由 Python 端按送货单日期生成，前端 `parseFilename` 读
> `content-disposition` 取名，取不到时回落到 `note-{id}.xlsx` / `label-{id}.xlsx`。

## Python 错误码透传

下列错误码在本链路由**Python 端产出、原样透传**，Rust 端不产出、不改写、不二次包装：

| code | 名称 | 语义 |
|---|---|---|
| 21109 | BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED | 未按客户 prefix 配出 xlsx 模板 |
| 21111 | BIZ_DELIVERY_PART_STATUS_INVALID | 所选零件状态不允许打印 |
| 21112 | BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS | 所选零件超过模板容量 |
| 21113 | BIZ_DELIVERY_PRINT_BAD_ORDER（HTTP 422） | `custom_order` 含非法批次 id 或漏行 |
| 21401 | BIZ_DELIVERY_NOTE_NOT_FOUND | 送货单不存在 |
| 21402 | BIZ_DELIVERY_NOTE_INVALID_TRANSITION | 送货单状态不允许打印 |

`/print-labels` 另可能返回 `20104 BIZ_INVALID_VALUE`（`line_item_ids=[]`）。

这 4 个 211xx 模板码与 214xx 送货单码在 `src/shared/error.rs::code` 里**仍然注册**
（保留与 Python 错误码表对齐），但 Rust 端已无生产点：链路上的它们全部来自 Python。
Python 侧还会返回哪些码以 Python 端为准，Rust 侧不预判、不枚举；前端按
`{code, message, data}` 解封即可。

## rust 端错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20407 | BIZ_PRINT_FORWARD_FAILED | 502 | rust → python 网络层失败：连接拒 / 超时 / 读 body 失败 / `PYTHON_BACKEND_BASE_URL` 未配置（`NoopPyBackend` 占位） |
| 40800 | REQUEST_TIMEOUT | 408 | 打印路径超过 `PRINT_REQUEST_TIMEOUT_SECONDS`（缺省 660s）—— 由 `middleware::timeout` 返回，带标准信封 |

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
  `Host` / `Content-Length`（reqwest 自管）/ hop-by-hop 全套
  （`Connection` / `Keep-Alive` / `Transfer-Encoding` / `Upgrade` / `Te` / `Trailer`）。

**响应侧**

- 保留：`content-type`（前端靠它区分 xlsx / PDF）/ `content-disposition`
  （前端 `parseFilename` 靠它取下载文件名）/ `cache-control`（Python 端给的语义照搬）/
  其余自定义头。
- 剥离：hop-by-hop 全套 + `content-encoding`（body 已被 reqwest 在解码层消费，
  再带着这头出去会让前端 blob 拿到「声明为 gzip 实为明文」的数据）+ `date` /
  `server`（上游 server 的自我标识，Rust 自己会写）。
- **重算**：`content-length` 一律按实际 body 长度。打印响应可能是数 MB 的 xlsx，
  长度与实际不符时前端 `responseType: 'blob'` 的下载会被截断，表现为「下到一个坏
  文件」且无报错，极难排查。

## 实现要点

- **无 DB 读、无事务**：handler 内不出现 `state.pool`，也不开 tx。
- **body 原样透传**：`Json<Value>` 透传，不定义强类型 DTO、不解析字段
  （雪花 id 是 string，解析只是引入一层无收益的转换；字段语义由 Python 端 schema
  负责，与 STS 转发同构）。
- **身份单头传递**：handler clone 一份 `HeaderMap` 再注入 `X-Forwarded-User-Id`
  （不能直接 mutate extractor 给的那份，会污染共用同一 `HeaderMap` 的其它
  extractor / middleware）。
- **响应不 gzip**：`/api/v2` 的 `CompressionLayer` 谓词排除 xlsx 的 content-type
  （已压缩格式再 gzip 只是白烧 CPU），PDF 同样排除；其余 content-type 的行为与
  默认谓词一致。
- **幂等跳过**：打印路径被 `middleware::idempotency` 跳过（前端打印请求不带
  `Idempotency-Key`；大体积 xlsx 响应也不该进 Redis 缓存）。
- **无 WS 事件**：打印端点不广播 WS 事件。
