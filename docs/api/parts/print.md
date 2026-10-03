# 图纸打印 —— part 的 2 个 PDF 打印端点

> 本文件须与 `src/modules/part/handler/print.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（PartOut / 端点约束）见 [`./index.md`](./index.md)
>
> 范围：本文件覆盖 2 个端点（`GET /api/v2/parts/{part_id}/print-drawing` /
> `POST /api/v2/parts/print-drawing-batch`）。两者都是**纯转发**：Rust 侧强制
> JWT + RBAC 之后把请求原样送到 Python 后端，PDF 的生成由 Python 端执行。Rust 端
> 不读 DB、不开事务、不解析请求字段、不二次包装响应。
> 送货单的 2 个打印端点（同样是转发形态）见 [`../delivery-notes/print.md`](../delivery-notes/print.md)。
>
> 导航：[**`index.md`**](./index.md) · [`crud.md`](./crud.md) · [`lifecycle.md`](./lifecycle.md) · [`inspection.md`](./inspection.md) · [`batch.md`](./batch.md) · **`print.md`**

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/parts/{part_id}/print-drawing` | Manager / Clerk / Inspector / CncProgrammer | 单件图纸 PDF → 转发 python `GET /api/v1/parts/{part_id}/print` |
| POST | `/api/v2/parts/print-drawing-batch` | Manager / Clerk / Inspector / CncProgrammer | 多件合并 PDF（可追加总装图页）→ 转发 python `POST /api/v1/parts/print-batch` |

## 鉴权

- 强制 JWT 鉴权（Bearer token），由 `v2_router` 全局 `authenticate_middleware` 处理；
  缺 / 坏 / 过期 token → 40100 / 40102 / 40105。
- RBAC：`require_any_role([MANAGER, CLERK, INSPECTOR, CNC_PROGRAMMER])`，
  不通过 → 40300 FORBIDDEN。`SHELF_ACCOUNT`（货架终端）不放行——它只该扫码。
- 图纸打印比送货单打印多放行一个 `CNC_PROGRAMMER`：CNC 编程岗要看图纸才能编程序，
  送货单是单据打印、编程岗用不上。
- Python 端这 2 个 `/api/v1` 端点**自身无应用层鉴权**，谁能把请求送到它们就等于拿到
  打印能力。因此本仓提供的**唯一受支持入口**是本文件覆盖的 2 条 `/api/v2` 路径
  （`print-drawing` / `print-drawing-batch`）：强制 JWT + RBAC 在 Rust 侧判定通过，
  Rust 才会把请求转给 Python。
- 鉴权凭据**不**透传给 Python：`Authorization` / `Cookie` 被 `filter_request_headers`
  剥离，Python 侧只能读到 Rust 注入的 `X-Forwarded-User-Id`。Python 端因此不需要、
  也不应该自己解析 JWT。
- `/api/v1` **不是**受支持的对外入口。Python 后端只应在内网经 `PYTHON_BACKEND_BASE_URL`
  被触达（compose 服务名 / 集群内网），对外暴露面以 `/api/v2` 这道闸门收口。
- 反向代理模板（nginx 配置）归 `frontend/` 子模块所有，不在本仓描述其内容与状态；
  本仓可核实的契约边界就是上面这几条。

## 行为（BFF 转发）

### v2 → v1 映射

| v2（Rust 对外） | v1（Python 现状） | Method |
|---|---|---|
| `GET /api/v2/parts/{part_id}/print-drawing` | `{PYTHON_BACKEND_BASE_URL}/api/v1/parts/{part_id}/print` | GET |
| `POST /api/v2/parts/print-drawing-batch` | `{PYTHON_BACKEND_BASE_URL}/api/v1/parts/print-batch` | POST |

⚠️ 这 2 条的路径段与 Python 端**不同名**（`print-drawing` → `print`、
`print-drawing-batch` → `print-batch`），映射只写在 `infra::py_backend` 的 impl 里
（每方法 1 行 `format!`），Python 端改名时只动那两行。

### `GET /api/v2/parts/{part_id}/print-drawing`

无请求体。Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `vector` | bool? | — | 原始 query 串**原样**转交 Python（`RawQuery`），Rust 侧不解析成 bool：默认值 / 兼容性语义由 Python 端负责，Rust 解析等于把同一份规则复制到两处 |

Response：Python 的响应原样透传（status + body + 经清洗的 headers）。成功形态：

- `Content-Type: application/pdf`
- `Content-Disposition: inline; filename="part-{part_id}.pdf"`
- `Cache-Control: private, max-age=600`
- Body: PDF 二进制

前端单件打印走 iframe 直接吃浏览器内联渲染，不消费这个文件名。

### `POST /api/v2/parts/print-drawing-batch`

Request（body 是 `Json<Value>`，**原样透传**，Rust 侧不解析字段、不做 schema 校验）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_ids` | [string (i64)] | ✓ | 待合并的工单 id 列表；Python 端按此顺序合并 |
| `assembly_ids` | [string (i64)]? | — | 追加总装图页 |
| `vector` | bool? | — | 光栅旁路，与单件端点的 `?vector=` 同一语义（Python 端 `PrintBatchRequest` 缺省 `false`） |

⚠️ 雪花 id 一律是 JSON **string**（> 2^53，JSON number 会丢精度）：Rust 侧若解析成
number 就是一次有损转换，故全链路按 string 透传，由 Python 端解析。

Response：`application/pdf` + `Content-Disposition: inline; filename="parts-batch.pdf"` +
`Cache-Control: private, max-age=600`，Body 是合并后的单个 PDF（文件名**不含** part id，
故前端拿到 header 也无法从文件名反推是哪一批）。批量打印 20 件/批，合法耗时数分钟 ——
这 2 条路径因此走长档请求超时（见「env 配置」）。

## Python 错误码透传

Python 端产出的业务错误码与错误信封（`{code, message, data}`）**原样透传**，Rust 端
不二次包装、不改写 code。前端 `envelopeResponseInterceptor` 按 rust 信封形态解封。
Python 打印端点会返回哪些码由 Python 端负责，Rust 侧不预判、不枚举。

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
| `PYTHON_PRINT_TIMEOUT_MS` | 打印转发的单次请求超时（毫秒）。渲染在 Python 端执行、耗时可达数分钟，不能与 STS 的 10s 通道同档 | `600_000` |
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

- 保留：`content-type`（前端靠它区分 PDF）/ `content-disposition`
  （前端 `parseFilename` 靠它取下载文件名）/ `cache-control`（Python 端对 PDF 给的
  `private, max-age=600`，语义照搬）/ 其余自定义头。
- 剥离：hop-by-hop 全套 + `content-encoding` + `date` / `server`（上游 server 的
  自我标识，Rust 自己会写）。
- `content-encoding` 之所以能安全剥：reqwest 开了 `gzip` feature，发请求时带
  `accept-encoding: gzip`，收到 gzip 响应时在**解码层**把 body 还原成明文（并顺手
  摘掉 `content-encoding` / `content-length`）。**解码与剥头必须成对**——只剥不解会
  让前端拿到「声明 PDF 实为 gzip 流」的坏文件且无报错。
- **重算**：`content-length` 一律按实际 body 长度。打印响应是数 MB 的 PDF，长度与
  实际不符时前端 `responseType: 'blob'` 的下载会被截断，表现为「下到一个坏文件」
  且无报错，极难排查。

## 实现要点

- **无 DB 读、无事务**：handler 内不出现 `state.pool`，也不开 tx。
- **body / query 原样透传**：批量 body 走 `Json<Value>`（雪花 id 是 string，解析只是
  一层无收益的转换）；`?vector=` 走 `RawQuery` 取原始 query 串直接拼到 URL
  （不走 `RequestBuilder::query()`——那会把 `&` / `=` 当作待转义的值再编码一次，
  Python 收到的是字面量而非参数）。
- **身份单头传递**：handler clone 一份 `HeaderMap` 再注入 `X-Forwarded-User-Id`
  （不能直接 mutate extractor 给的那份，会污染共用同一 `HeaderMap` 的其它
  extractor / middleware）。
- **响应不 gzip**：`/api/v2` 的 `CompressionLayer` 谓词 = tower-http `DefaultPredicate`
  （< 32 字节 / `image/*` / gRPC / SSE 不压缩）**且**排除 `application/pdf` 与 xlsx 的
  content-type（已压缩格式再 gzip 只是白烧 CPU）。用 `.and()` 组合而非替换，因为
  `compress_when` 是替换语义，只写排除项会把默认谓词的 4 条保护一起丢掉。
- **幂等跳过**：打印路径被 `middleware::idempotency` 跳过（前端打印请求不带
  `Idempotency-Key`；数 MB 的 PDF 响应也不该进 Redis 缓存）。
- **无 WS 事件**：打印端点不广播 WS 事件。

## 参考

- handler：`src/modules/part/handler/print.rs`；路由注册 `src/modules/part/mod.rs`（静态段
  `/print-drawing-batch` 必须在 `/{part_id}/...` catch-all 之前注册）。
- 转发客户端：`src/infra/py_backend.rs`（`PyBackendClient` / `HttpPyBackend` /
  `NoopPyBackend` / 私有 `send()` / `filter_request_headers` / `filter_response_headers`）。
- 超时分档：`src/middleware/timeout.rs`。
- 错误码：`src/shared/error.rs::code`（20407 / 40800 + HTTP 状态推导）。
- 集成测试：`tests/print_forward.rs`（4 条打印路由的 URL 拼装 / 角色闸门 / 头清洗 /
  `is_print_path` 与实际注册路由逐字一致）。
- 同机制的另一个端点：`POST /api/v2/files/sts-tmp-keys`，见 [`../files.md`](../files.md)。
