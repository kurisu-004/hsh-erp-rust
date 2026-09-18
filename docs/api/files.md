# part_file 域 API

> 本文件须与 `src/modules/part_file/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：零件文件 CRUD（multipart 单文件上传 + 列表 + 详情 + COS 预签下载 URL）。
> 2026-09-14 Phase 3 落地。
>
> 2026-09-18 重大变更：**删除** `POST /api/v2/part-files/upload-intents` 端点——
> 上传意图机制迁移至 [`upload_session.md`](./upload_session.md)（共享 STS 凭证 +
> Redis 会话）。本文件保留 confirm 端点与 multipart 上传端点。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/part-files` | Manager / Clerk / CncProgrammer | 单文件上传（multipart：`data` JSON + `file` 二进制） |
| GET | `/api/v2/part-files` | Manager / Clerk / Inspector / CncProgrammer | 列表 + 分页（owner_kind / owner_id / kind 过滤） |
| GET | `/api/v2/part-files/{file_id}/url` | Manager / Clerk / Inspector / CncProgrammer | 单条详情 + COS 预签下载 URL（默认 1h） |
| GET | `/api/v2/part-files/{file_id}/content` | Manager / Clerk / Inspector / CncProgrammer | 后端代理文件二进制流（透传 content_type） |
| POST | `/api/v2/part-files/{file_id}/delete` | 按 `kind` 派生（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；G_CODE → M+CNC） | 软删 + COS 异步清理 |
| POST | `/api/v2/parts/{part_id}/files/confirm` | Manager / Clerk | 直传 COS 链路绑定：客户端 PUT 到 tmp 区成功后，head 校验 size → copy 到 CAS key → INSERT READY part_file → 异步清理 tmp |

> 2026-09-18 删除：`POST /api/v2/part-files/upload-intents` 端点（迁移至 upload_session 域）。

### `POST /api/v2/part-files/{file_id}/delete`

入参（JSON）：`{ "version": i32 }`（OCC）。

权限按 kind 派生：

| kind | 允许角色 |
|---|---|
| `DRAWING` / `3D_MODEL` / `CAD_2D` / `SETUP_SHEET` | Manager / Clerk |
| `G_CODE` | Manager / CncProgrammer |

行为：乐观锁守；UPDATE `deleted_at = now()` + `version = version + 1`；handler commit 后 `tokio::spawn` 异步调 `cos.delete_object`（失败仅 warn，不阻断）。

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21101 | BIZ_PART_FILE_NOT_FOUND | 404 | file_id 不存在 |
| 21102 | BIZ_PART_FILE_BAD_TYPE | 400 | kind 不可软删 |
| 40901 | VERSION_CONFLICT | 409 | version 不匹配 |

---

### `POST /api/v2/parts/{part_id}/files/confirm`

直传 COS 链路的"提交绑定"端点：客户端 PUT 到 tmp 区成功后，调用本端点把 tmp 对象 copy 到 CAS key + INSERT `t_part_file` + 异步清理 tmp 对象。

权限：**Manager / Clerk**（与 upload_session.get_or_create 一致）

入参（JSON）：`ConfirmFileIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | string | ✓ | `DRAWING` / `3D_MODEL`（白名单由 `policy::allowed_exts` 校验，422 → 40001） |
| `tmp_key` | string | ✓ | COS tmp 对象 key（前端 PUT 后得到）；必须以 `COS_TMP_PREFIX`（默认 `tmp/`）开头，否则 → 40000 |
| `content_sha256` | string | ✓ | 64 hex chars |
| `original_filename` | string | ✓ | 含扩展名（≤255 字符） |
| `file_size` | string (i64) | ✓ | 客户端声明字节数；> 0 且 ≤ `COS_MAX_FILE_SIZE` |
| `content_type` | string | ✓ | MIME；必须与 `original_filename` 扩展名白名单匹配 |

响应 200 `PartFileOut`（同 multipart 上传路径；详见下方）。

业务语义：

1. **字段校验** —— `validate::check_confirm_file_in`（kind / sha / filename / size / content_type）；任一不合法 → 40001 VALIDATION_ERROR（HTTP 422）。
2. **head_object(tmp_key)** — 校验 tmp 对象存在 + size 与声明一致：
   - 不存在（NoSuchKey） → `BIZ_PART_FILE_TMP_OBJECT_MISSING` 21114（HTTP 400，2xxxx 兜底 → BAD_REQUEST；语义上更像 404，需在 `status_from_code` 显式登记方可对齐）
   - size 不一致 → `BIZ_PART_FILE_SIZE_MISMATCH` 21115（HTTP 400）
3. **tmp_key 前缀防呆** — 必须以 `cfg_tmp_prefix`（默认 `tmp/`）开头；否则 → `BIZ_INVALID_VALUE` 40000（HTTP 400，防客户端乱传 key 读到别人文件）。
4. **copy_object(tmp_key, cas_key)** — 派生 CAS key（模板 `{upload_prefix}part/{part_id}/{KIND}/{sha16}_{safe_filename}`，与 multipart 上传路径模板一致）；copy 失败 → `BIZ_PART_FILE_UPLOAD_FAILED` 21104（HTTP 500）。copy 成功后**立刻** spawn `cos.delete_object(tmp_key)` 兜底清理（commit 失败也走 delete 兜底，避免 tmp 孤儿）。
5. **单事务 INSERT** — soft_delete 同 owner + kind 旧活跃 part_file 行（保留 owner+kind+deleted_at IS NULL 的唯一约束）→ INSERT 新 part_file（`upload_status="READY"`）。`uk_t_part_file_owner_kind_sha` 唯一索引并发兜底：撞 23505 → `BIZ_PART_FILE_DUPLICATE` 21108（HTTP 409）。
6. **commit 后** handler 二次 spawn `cos.delete_object(tmp_key)` 兜底清理（与 service 内早期 spawn 双层防护，delete_object 幂等）。

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | copy_object 失败（CAS 写入异常） |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | `part_id` 在 `t_part` 不存在（service 校验 owner 阶段） |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 同 owner + kind + sha256 已存在（唯一索引并发兜底） |
| 21114 | BIZ_PART_FILE_TMP_OBJECT_MISSING | 400 | tmp 对象 head_object 失败（404 NoSuchKey）；HTTP 走 2xxxx 兜底 → BAD_REQUEST |
| 21115 | BIZ_PART_FILE_SIZE_MISMATCH | 400 | tmp 对象 head size 与声明 size 不一致 |
| 40000 | BIZ_INVALID_VALUE | 400 | tmp_key 不在 `COS_TMP_PREFIX` 范围内 |
| 40001 | VALIDATION_ERROR | 422 | 字段校验失败（kind / sha / filename / size / content_type） |

> **HTTP 状态码推导**：`BIZ_PART_FILE_TMP_OBJECT_MISSING` (21114) 走 `AppError::biz` → `status_from_code(21114)` → 未在显式 404 列表 → 落入 `(20000..30000)` 兜底 → `BAD_REQUEST`（400）。语义上 tmp 对象缺失更像 404，但当前错误码表未将其列入显式映射，因此 HTTP=400；后续若需改为 404，可将 21114 加进 `status_from_code` 404 段。
>
> 注：本端点**不**返回 21102（kind / content_type 校验走 40001 VALIDATION_ERROR，避开与 multipart 端点 21102 在「客户端没按规范填字段」vs「kind 不匹配」语义重叠）；confirm handler 直接复用 `validate::check_confirm_file_in` 与 upload_session.get_or_create 入口保持一致。

---

## multipart 上传契约

请求 body（`multipart/form-data`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `data` | 文本字段 | 序列化后的 `UploadFileData` JSON |
| `file` | 二进制字段 | PDF / STEP / STL 等 |

`UploadFileData`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `owner_kind` | string | `"PART"` / `"ASSEMBLY"` |
| `owner_id` | string | 雪花 id（字符串形式） |
| `kind` | string | `"DRAWING"` / `"3D_MODEL"` / `"G_CODE"` / `"SETUP_SHEET"` / `"ASSEMBLY_MASTER"` / `"CAD_2D"` |

服务端校验：

1. owner 存在性 → `21105 BIZ_PART_FILE_OWNER_NOT_FOUND`
2. 扩展名（从 filename 解析）必须在 `kind` 白名单内 → `21102 BIZ_PART_FILE_BAD_TYPE`
3. content_type 必须在扩展名白名单内（含 `application/octet-stream` 兜底） → `21102`
4. SHA-256 → 同 owner + kind + sha 撞唯一索引 → `21108 BIZ_PART_FILE_DUPLICATE`（并发兜底）

CAS 命中（已上传过相同内容）→ 跳过 COS PUT，直接复用已有 object_key。

## 共享 DTO

### PartFileOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `owner_id` | string (i64) | polymorphic owner id |
| `owner_kind` | string | "PART" / "ASSEMBLY" |
| `kind` | string | |
| `file_type` | string | 大写扩展名（PDF / STEP / STL 等） |
| `object_key` | string | COS object key |
| `original_filename` | string | 客户端上传时文件名 |
| `file_size` | string (i64) | 文件字节数 |
| `content_type` | string | MIME |
| `upload_status` | string | READY / PENDING / FAILED |
| `content_sha256` | string? | SHA-256 hex |
| `paired_file_id` | string (i64)? | CNC 配对文件 id（G_CODE <-> SETUP_SHEET 互指），未配对为 null；2026-09-16 补投影（前端零件详情 CNC 配对分组依赖） |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `created_by` | string (i64)? | |

### PartFileWithUrlOut 字段

`PartFileOut` 子集 + `download_url` / `url_expires_in_seconds`。

### PartFileListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | PartFileOut[] | |
| `total` | i64 | 满足过滤的总数 |
| `limit` | i64 | 实际生效 |
| `offset` | i64 | 实际生效 |

### PartFileListQuery 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `owner_kind` | string? | PART / ASSEMBLY |
| `owner_id` | string? | 雪花 id 字符串 |
| `kind` | string? | |
| `limit` | i64? | 1..=500 |
| `offset` | i64? | ≥ 0 |

---

## 错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21102 | BIZ_PART_FILE_BAD_TYPE | 400 | 扩展名与 kind 不匹配 / content_type 不匹配 |
| 21103 | BIZ_PART_FILE_TOO_LARGE | 400 | file_size 超过 `COS_MAX_FILE_SIZE`（upload_session.allocate 入参校验） |
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | COS SDK 抛错（put_object / copy_object 失败） |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | polymorphic owner (part / assembly) 不存在 |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 唯一索引并发兜底（同 owner + kind + sha 撞 23505） |
| 21114 | BIZ_PART_FILE_TMP_OBJECT_MISSING | 400 | 直传 COS confirm：head_object(tmp_key) 失败（NoSuchKey）；HTTP 走 2xxxx 兜底 → BAD_REQUEST |
| 21115 | BIZ_PART_FILE_SIZE_MISMATCH | 400 | 直传 COS confirm：head size 与声明 size 不一致 |

> 21101（NOT_FOUND）：保留对齐 Python 错误码表，本文档对应端点暂不返回。
> 21114 当前未列入 `status_from_code` 显式映射段，HTTP 走 `(20000..30000)` 兜底 → BAD_REQUEST；语义上更像 404，若需对齐 HTTP 语义，需在 `src/shared/error.rs::status_from_code` 显式登记。
>
> 2026-09-18 注：原 `POST /upload-intents` 文档段（含 21103 / 21116 错误码）已删除——上传意图机制迁移至 upload_session 域，相关错误码（216xx 系列）见 [`upload_session.md`](./upload_session.md#错误码)。

---

## 实现要点

- 事务边界：handler 开 tx → service 写 DB → commit。
- SHA-256 CAS 去重：CAS 命中跳过 COS PUT；并发兜底靠 `uk_t_part_file_part_kind_sha` 唯一索引 + 服务端捕获 23505 → 21108。
- COS object key 模板：`{owner_kind.to_lowercase()}/{owner_id}/{kind}/{sha[..16]}_{sanitized_filename}`。
- 权限：service 层 `require_any_role([Manager, Clerk, CncProgrammer])`；列表 / 详情额外允许 Inspector。
- WS 广播：本域不上报 WS 事件（part_file 是只读资产）。
- 上传意图机制：2026-09-18 整体迁移至 upload_session 域（共享 STS 凭证 + Redis 会话 + 转发 python 签发）。
