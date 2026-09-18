# upload_session 域 API

> 本文件须与 `src/modules/upload_session/{handler.rs,dto.rs,service/,repo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：**共享 STS 凭证的 Redis 会话机制**——前端直传 COS 凭证由本域统一管理，
> 调用方通过 7 个端点完成"创建会话 / 分配 tmp_key / head 校验完成 / 移除条目 /
> 重签 / 消费 / 销毁"。
>
> 2026-09-18 新增。**替代原 `POST /api/v2/part-files/upload-intents`** —— 上传意图
> 机制迁移：原一次性签 STS + 无状态 RPC → 现服务端维护 redis 会话，TTL 24h 滑动；
> 凭证 < 10min 自动 renew。

---

## 消费方说明

> 2026-09-18 review #4 修复：本模块的**主要消费方是前端 composable**（`useUploadSession`），
> 不是后端 part batch_create 流程。具体路径：
>
> - 前端 `usePartBatchPdf` / `usePartBatchManual` 等业务 composable 走 `session.allocate`
>   （分配 tmp_key）→ 前端用 STS 直传 COS tmp → `session.markComplete`（head 校验）→
>   `session.consume`（业务消费）；session 全部 7 个端点都通过 `useUploadSession`
>   composable 封装。
> - 后端 `part batch_create` 流程**只接受前端传来的 `tmp_key`**，做服务端
>   `cos.copy_object(tmp_key → CAS 永久 key)`；该流程与 session **完全解耦**——
>   业务接入会在独立的 PR 通过 `useUploadSession` 接入，前端契约已稳定后由
>   前端 team 联调。
>
> 也就是说：本模块本身是**前端直传 COS 的服务端脚手架**，上线即被 `useUploadSession`
> 使用；后端 part_batch 流程的 session 接入在独立 PR，不属于本模块范围。

---

## 设计要点

### Redis key 与 TTL

- **键**：`upload_session:{user_id}:{scope}`（string，存 JSON `UploadSession`）
- **TTL**：24h 滑动（每次写都 `SET EX 86400` 或 `EXPIRE` 续期）
- 环境变量：`UPLOAD_SESSION_TTL_SECONDS`（缺省 `86400`）

### STS 凭证签发：转发 python 后端

- rust 后端不再直连腾讯云 `sts.tencentcloudapi.com`
- 改为转发到 python 后端 `POST {PYTHON_BACKEND_BASE_URL}/api/v1/files/sts-prefix-credentials`
- python 端负责凭据管理 / 审计 / 限流
- 超时 10s；HTTP 4xx/5xx 映射到 `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED` (21608)
- **python 后端统一信封**（`UnifiedResponseMiddleware` 包装）：所有响应均为
  `{code, message, data}` 三段式；rust 端先解信封再取 `data`。
  - 成功：`code == 0` 且 `data` = STS 凭证对象
  - 业务异常：`code != 0`（如 21503 权限不足）；rust 错误消息透传 `[<python_code>] <原 message>`
- 实现见 `src/infra/python_sts.rs::parse_python_response`（纯函数，单测全覆盖）

### 自动 renew 触发条件

- `get_or_create` hit 路径：若 `expired_time - now < 600s` → 自动 renew（10min 阈值）
- 环境变量：`UPLOAD_SESSION_RENEW_THRESHOLD_SECONDS`（缺省 `600`）

### scope 白名单

- 首期仅 `"parts_new"`
- 其它 scope 直接 422 `BIZ_UPLOAD_SESSION_SCOPE_INVALID`
- 后续扩容时改 `src/modules/upload_session/dto.rs::is_valid_scope` + 本文档同步

### tmp_key 派生规则

- 模板：`{tmp_prefix}{sha16}_{safe_filename}`
- `tmp_prefix` 由 service 派生：`tmp/sess/<session_uuid>/`（含尾斜杠）
- `sha16` = `content_sha256` 前 16 hex chars
- `safe_filename` = ASCII 字母数字 / `.` / `-` / `_` 保留，其它替换 `_`，长度 ≤ 80

### session_id 防混淆

- path 上的 `session_id` 必须等于 Redis 中 `UploadSession.session_id`
- 否则 → 409 `BIZ_UPLOAD_SESSION_MISMATCH`（防 cross-user / cross-scope 误用）

### 权限

- 7 个端点统一：`Manager / Clerk`（与原 part_file upload-intents 一致）
- `CurrentUser::require_any_role([Role::Manager, Role::Clerk])`

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/upload-sessions/get-or-create` | Manager / Clerk | 拿 / 创建上传会话（miss 签发；hit 自动 renew） |
| POST | `/api/v2/upload-sessions/{session_id}/files:allocate` | Manager / Clerk | 批量分配 tmp_key（client_ref 幂等） |
| POST | `/api/v2/upload-sessions/{session_id}/files/{client_ref}/complete` | Manager / Clerk | head 校验 tmp → 标 done/error |
| POST | `/api/v2/upload-sessions/{session_id}/files:remove` | Manager / Clerk | 移除条目 + 异步删 tmp |
| POST | `/api/v2/upload-sessions/{session_id}/renew` | Manager / Clerk | 显式重签 STS |
| POST | `/api/v2/upload-sessions/{session_id}/consume` | Manager / Clerk | 仅移除条目（tmp 删由 confirm / batch 端点负责） |
| POST | `/api/v2/upload-sessions/{session_id}/discard` | Manager / Clerk | 整条删除 Redis key |

---

## 1) `POST /api/v2/upload-sessions/get-or-create`

**拿 / 创建上传会话。**

权限：**Manager / Clerk**

入参（JSON）：`GetOrCreateIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域；首期仅 `"parts_new"`（其它 → 422） |

响应 200 `GetOrCreateOut`：

```jsonc
{
  "session_id": "uuid",                     // 服务端生成（v4）
  "scope": "parts_new",
  "tmp_prefix": "tmp/sess/<uuid>/",         // 写入 COS tmp 区的前缀（前端按此拼 key）
  "bucket": "...",                           // 从 python 端 sts 响应取
  "region": "...",
  "credentials": {
    "tmp_secret_id": "...",
    "tmp_secret_key": "...",
    "session_token": "...",
    "start_time": 1234567890,                // unix 秒（i64 number）
    "expired_time": 1234571490               // unix 秒；前端在此前 5min 触发 renew
  },
  "expires_in": 3600,                        // = expired_time - start_time
  "files": []                                // 当前 session 已登记的文件列表（创建时为空）
}
```

业务语义：

1. scope 白名单校验（首期仅 `parts_new`）
2. miss（Redis 无 key）→ 调 python 签发（`POST /api/v1/files/sts-prefix-credentials`，body `{prefix, expire_seconds}`）→ 生成 uuid → 写 Redis
3. hit 且凭证剩余有效期 < `renew_threshold`（默认 600s / 10min）→ 自动 renew + 更新 Redis
4. hit 且剩余足够 → 直接返回（EXPIRE 续期 TTL）
5. **返回时永远保证 credentials 可用**（剩余 ≥ renew_threshold）

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | （仅变更类端点）Redis key 不存在 |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单（首期仅 `parts_new`） |
| 21608 | BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED | 503 | 转发 python STS 失败（HTTP 4xx/5xx/超时） |
| 40300 | FORBIDDEN | 403 | 角色不足（非 Manager/Clerk） |

---

## 2) `POST /api/v2/upload-sessions/{session_id}/files:allocate`

**批量登记文件并派生 tmp_key。**

权限：**Manager / Clerk**

入参（JSON）：`AllocateFilesIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域；首期仅 `"parts_new"` |
| `files` | `AllocateFileItemIn[]` | ✓ | 1..=200，每 item 一份上传意图 |

`AllocateFileItemIn`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `client_ref` | string | ✓ | 客户端 reference（UUID 或任意字符串）；用于跨端点幂等 |
| `kind` | string | ✓ | `"drawing"` / `"3d_model"`（白名单由 `is_valid_kind` 校验） |
| `original_filename` | string | ✓ | 含扩展名（≤ 255 字符） |
| `file_size` | number (i64) | ✓ | 客户端声明字节数；> 0 |
| `content_type` | string | ✓ | MIME（建议与扩展名匹配） |
| `content_sha256` | string | ✓ | 64 hex chars |

响应 200 `AllocateFilesOut`：

```jsonc
{
  "items": [
    {
      "client_ref": "uuid",
      "tmp_key": "tmp/sess/<uuid>/<sha16>_<safe>.ext"
    }
  ]
}
```

业务语义：

1. scope 白名单 + session_id 防混淆
2. **幂等**：`client_ref` 已存在 → 返回旧条目，不重新分配 tmp_key
3. 同 sha 不同 client_ref 仍分配新 tmp_key（防止误用别人 client_ref 锁死自己的 key 分配）
4. 派生 tmp_key：`{tmp_prefix}{sha16}_{safe_filename}`
5. 原子覆盖写 Redis（GET → modify → SET EX）

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在（discarded / TTL 过期） |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |
| 21605 | BIZ_UPLOAD_SESSION_BAD_TYPE | 422 | kind 不在 `drawing` / `3d_model` 白名单 |

---

## 3) `POST /api/v2/upload-sessions/{session_id}/files/{client_ref}/complete`

**客户端 PUT 到 tmp 区成功后调用本端点做 head 校验。**

权限：**Manager / Clerk**

入参（JSON）：`CompleteFileIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域 |
| `etag` | string? | — | 客户端声明的 etag（仅 audit 参考；服务端以 COS head 返回为准） |
| `file_size` | number (i64)? | — | 客户端声明字节数；若有则与 head size 交叉校验 |

响应 200 `CompleteFileOut`（即更新后的 `SessionFile`）：

```jsonc
{
  "client_ref": "uuid",
  "kind": "drawing",
  "original_filename": "drawing.pdf",
  "file_size": 1024,                          // 同步为 head 实测值
  "content_type": "application/pdf",
  "content_sha256": "a3f9...",
  "tmp_key": "tmp/sess/<uuid>/<sha16>_<safe>.pdf",
  "status": "done",                            // "pending" → "done" 或 "error"
  "etag": "\"d41d8cd98f00b204e9800998ecf8427e\"",  // COS 返回（带引号）
  "uploaded_at": "2026-09-18T10:23:45.123+00:00"  // ISO 8601 UTC
}
```

业务语义：

1. scope 白名单 + session_id 防混淆 + client_ref 校验
2. `cos.head_object(tmp_key)` 校验 tmp 对象：
   - 失败（NoSuchKey / 不可达）→ status=`error` 写回 + 抛 `BIZ_UPLOAD_SESSION_HEAD_FAILED` (21606)
   - size 不一致（若 req.file_size 声明） → status=`error` 写回 + 抛 `BIZ_UPLOAD_SESSION_SIZE_MISMATCH` (21607)
3. 成功 → status=`done` + etag（head 实测）+ uploaded_at（UTC now）+ 同步 file_size 为 head 实测值

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在 |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |
| 21604 | BIZ_UPLOAD_SESSION_FILE_NOT_FOUND | 404 | client_ref 不在 session.files |
| 21606 | BIZ_UPLOAD_SESSION_HEAD_FAILED | 400 | COS head_object 失败（NoSuchKey / 不可达） |
| 21607 | BIZ_UPLOAD_SESSION_SIZE_MISMATCH | 400 | head size 与声明 size 不一致 |

---

## 4) `POST /api/v2/upload-sessions/{session_id}/files:remove`

**从 session 移除条目并异步删 tmp 对象。**

权限：**Manager / Clerk**

入参（JSON）：`RemoveFilesIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域 |
| `client_refs` | string[] | ✓ | 待移除的 client_ref 列表 |

响应 200 `RemoveFilesOut`：

```jsonc
{
  "removed": ["uuid1", "uuid2"]   // 仅真实移除的（顺序按入参）
}
```

业务语义：

1. scope 白名单 + session_id 防混淆
2. 逐项从 `session.files` 移除（不存在的 client_ref 静默跳过，不计入 `removed`）
3. `tokio::spawn` 异步 `cos.delete_object(tmp_key)`（best-effort，失败仅 warn）
4. 原子覆盖写 Redis

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在 |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |

---

## 5) `POST /api/v2/upload-sessions/{session_id}/renew`

**显式重签 STS（前置端点已自动 renew 时通常不需要调用）。**

权限：**Manager / Clerk**

入参（JSON）：`RenewIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域 |

响应 200 `RenewOut`：

```jsonc
{
  "credentials": {
    "tmp_secret_id": "...",
    "tmp_secret_key": "...",
    "session_token": "...",
    "start_time": 1234567890,
    "expired_time": 1234571490
  },
  "expires_in": 3600
}
```

业务语义：

1. scope 白名单 + session_id 防混淆
2. 调 `python_sts.issue(tmp_prefix, expire_seconds)` 重签同 prefix
3. 更新 Redis 中 `credentials` 字段（保留原 `session_id` / `tmp_prefix` / `files`）
4. 同时 `EXPIRE` 续期 TTL

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在 |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |
| 21608 | BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED | 503 | 转发 python STS 失败 |

---

## 6) `POST /api/v2/upload-sessions/{session_id}/consume`

**业务消费：仅从 session 移除条目（不触发 tmp 删）。**

权限：**Manager / Clerk**

入参（JSON）：`ConsumeFilesIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域 |
| `client_refs` | string[] | ✓ | 待消费的 client_ref 列表 |

响应 200 `ConsumeFilesOut`：

```jsonc
{
  "consumed": ["uuid1", "uuid2"]   // 仅真实移除的（顺序按入参）
}
```

业务语义：

1. scope 白名单 + session_id 防混淆
2. 逐项从 `session.files` 移除（不存在的 client_ref 静默跳过，不计入 `consumed`）
3. **不**触发 `cos.delete_object(tmp_key)`——tmp 删理由 `batch.rs::batch_create_parts` /
   `part_file::bind_uploaded_file` 的现有 spawn delete 兜底统一处理（避免双层删除竞态）
4. 原子覆盖写 Redis

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在 |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |

---

## 7) `POST /api/v2/upload-sessions/{session_id}/discard`

**整条删除 Redis key。**

权限：**Manager / Clerk**

入参（JSON）：`DiscardIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scope` | string | ✓ | 业务作用域 |

响应 200 `DiscardOut`：

```jsonc
{
  "session_id": "uuid"
}
```

业务语义：

1. scope 白名单校验
2. 取 Redis 中 session：
   - 不存在（已 discard / TTL 过期）→ 幂等返回 `session_id`（不报错）
   - 存在但 path session_id != session.session_id → 409 `BIZ_UPLOAD_SESSION_MISMATCH`
   - 存在且匹配 → DEL Redis key
3. **不**清理 tmp 对象（业务上调用方已通过 complete/confirm 链路把 tmp → CAS key）

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单 |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id（首次 discard 不报错；重复 discard 时若 session 已被别人重写才会触发） |

---

## 共享数据结构

### UploadSession（Redis 内部 JSON）

```rust
pub struct UploadSession {
    pub session_id: String,    // uuid v4
    pub user_id: i64,
    pub scope: String,         // 首期仅 "parts_new"
    pub tmp_prefix: String,    // "tmp/sess/<uuid>/"
    pub bucket: String,
    pub region: String,
    pub credentials: SessionCredentials,
    pub expires_in: i64,
    pub files: Vec<SessionFile>,
    pub created_at: i64,       // unix 秒
    pub updated_at: i64,       // unix 秒
}
```

### SessionFile（Redis 内部 JSON + 部分端点出参）

```rust
pub struct SessionFile {
    pub client_ref: String,
    pub kind: String,                  // "drawing" / "3d_model"
    pub original_filename: String,
    pub file_size: i64,                // JSON number
    pub content_type: String,
    pub content_sha256: String,        // 64 hex
    pub tmp_key: String,
    pub status: String,                // "pending" / "done" / "error"
    pub etag: Option<String>,          // COS head 返回（带引号）
    pub uploaded_at: Option<String>,   // ISO 8601 UTC
}
```

### SessionCredentials（Redis 内部 JSON + 出参 SessionCredentialsOut）

```rust
pub struct SessionCredentials {
    pub tmp_secret_id: String,
    pub tmp_secret_key: String,
    pub session_token: String,
    pub start_time: i64,      // unix 秒（JSON number）
    pub expired_time: i64,
}
```

> **时间字段序列化策略**：本契约**不**沿用雪花 id 的"string 防 JS 精度截断"策略——
> unix 秒远小于 2^53（精度截断阈值），JSON number 安全。前端拿到 number 后自行
> `new Date(expired_time * 1000)` 算倒计时。

---

## 错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21601 | BIZ_UPLOAD_SESSION_NOT_FOUND | 404 | Redis key 不存在（discarded / TTL 过期） |
| 21602 | BIZ_UPLOAD_SESSION_SCOPE_INVALID | 422 | scope 不在白名单（首期仅 `parts_new`） |
| 21603 | BIZ_UPLOAD_SESSION_MISMATCH | 409 | path session_id != Redis session.session_id |
| 21604 | BIZ_UPLOAD_SESSION_FILE_NOT_FOUND | 404 | client_ref 不在 session.files |
| 21605 | BIZ_UPLOAD_SESSION_BAD_TYPE | 422 | kind 不在 `drawing` / `3d_model` 白名单 |
| 21606 | BIZ_UPLOAD_SESSION_HEAD_FAILED | 400 | complete 时 COS head_object 失败（NoSuchKey / 不可达） |
| 21607 | BIZ_UPLOAD_SESSION_SIZE_MISMATCH | 400 | complete 时 head size 与声明 size 不一致 |
| 21608 | BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED | 503 | python_sts 转发 python 签发失败（HTTP 4xx/5xx/超时） |
| 40300 | FORBIDDEN | 403 | 角色不足（非 Manager/Clerk） |
| 40100 | UNAUTHORIZED | 401 | JWT 缺失 / 无效 / 服务端 session 吊销 |

> 21608 显式 HTTP 503（SERVICE_UNAVAILABLE）— 业务可重试；与 5xxxx 系统错语义区分。

---

## 实现要点

- **事务边界**：本域不写 DB，全部走 Redis（无 tx）；但仍按 `handler → service → repo` 三层调用结构，便于未来扩展 DB 元数据
- **session_id 防混淆**：path 上的 `session_id` 必须等于 Redis 中的 `session.session_id`（防 cross-user 误用别人的 session）
- **竞态保证**：单 key 写者即单 user（`upload_session:{user_id}:{scope}` 隔离），用最简的 GET → modify → SET EX 流程；如需更强保证可后续切 WATCH/MULTI/EXEC
- **凭证自动续期**：`get_or_create` / `renew` 路径在剩余有效期 < `renew_threshold`（默认 600s）时自动 renew + EXPIRE 续期 TTL
- **tmp_key 隔离**：每个 session 有独立 tmp prefix（`tmp/sess/<uuid>/`），跨 session 互不干扰
- **WS 广播**：本域不上报 WS 事件（上传会话是临时态，无业务状态需广播）
- **scope 白名单硬编码**：`is_valid_scope` 仅放行 `"parts_new"`；后续扩容需改 `dto.rs` + 同步本文件
- **kind 白名单**：`is_valid_kind` 复用 `part_file::policy::allowed_exts`（与 multipart 上传 kind 一致）
- **STS 转发**：`HttpPythonSts` 调 `POST {PYTHON_BACKEND_BASE_URL}/api/v1/files/sts-prefix-credentials`，
  body `{prefix, expire_seconds}`，超时 10s；HTTP 4xx/5xx → `BIZ_UPLOAD_SESSION_STS_FORWARD_FAILED` (21608)
- **python 信封解包**：python 后端所有响应经 `UnifiedResponseMiddleware` 包装为
  `{code, message, data}`；rust 端 `parse_python_response` 先反序列化为 `PythonEnvelope<T>`
  再判 `code` / 取 `data`。成功路径返回 `data`，业务异常路径把 python `code` + `message`
  透传到 rust 错误消息（前端能看到原始错码）。解析失败（malformed JSON / data 缺字段）
  → 21608 + body 前 200 字节预览。2026-09-18 修复 21608 历史 bug 时引入。
- **trait 注入模式**：与 `CosClient` / `StsCredentialIssuer` / `SessionStore` 同形
  （trait + `Arc<dyn>` + Noop 占位）
