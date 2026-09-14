# part_file 域 API

> 本文件须与 `src/modules/part_file/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：零件文件 CRUD（multipart 单文件上传 + 列表 + 详情 + COS 预签下载 URL）。
> 2026-09-14 Phase 3 落地。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/part-files` | Manager / Clerk / CncProgrammer | 单文件上传（multipart：`data` JSON + `file` 二进制） |
| GET | `/api/v2/part-files` | Manager / Clerk / Inspector / CncProgrammer | 列表 + 分页（owner_kind / owner_id / kind 过滤） |
| GET | `/api/v2/part-files/{file_id}/url` | Manager / Clerk / Inspector / CncProgrammer | 单条详情 + COS 预签下载 URL（默认 1h） |
| GET | `/api/v2/part-files/{file_id}/content` | Manager / Clerk / Inspector / CncProgrammer | 后端代理文件二进制流（透传 content_type） |
| POST | `/api/v2/part-files/{file_id}/delete` | 按 `kind` 派生（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；G_CODE → M+CNC） | 软删 + COS 异步清理 |

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
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | COS SDK 抛错（put_object 失败） |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | polymorphic owner (part / assembly) 不存在 |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 唯一索引并发兜底（同 owner + kind + sha 撞 23505） |

> 21101（NOT_FOUND）、21103（TOO_LARGE）本 pass 未触发（预留码）；保留对齐 Python 错误码表。

---

## 实现要点

- 事务边界：handler 开 tx → service 写 DB → commit。
- SHA-256 CAS 去重：CAS 命中跳过 COS PUT；并发兜底靠 `uk_t_part_file_part_kind_sha` 唯一索引 + 服务端捕获 23505 → 21108。
- COS object key 模板：`{owner_kind.to_lowercase()}/{owner_id}/{kind}/{sha[..16]}_{sanitized_filename}`。
- 权限：service 层 `require_any_role([Manager, Clerk, CncProgrammer])`；列表 / 详情额外允许 Inspector。
- WS 广播：本域不上报 WS 事件（part_file 是只读资产）。