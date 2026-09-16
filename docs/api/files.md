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
| POST | `/api/v2/part-files/upload-intents` | Manager / Clerk | 直传 COS 链路预签：批量预生成 `batch_uuid` + 每 item STS 凭证（场景 A 批量预生成 / 场景 B 详情页补传 + dedup_hit） |
| POST | `/api/v2/parts/{part_id}/files/confirm` | Manager / Clerk | 直传 COS 链路绑定：客户端 PUT 到 tmp 区成功后，把 tmp 对象 copy 到 CAS key + INSERT `t_part_file` + 异步清理 tmp |

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

### `POST /api/v2/part-files/upload-intents`

直传 COS 链路预签：批量预生成 `batch_uuid`（场景 A，每 item 一份 STS 凭证）+ 详情页补传（场景 B，按 `kind` 派生凭证；已存在同 `owner_id + kind + sha256` 时返回 `dedup_hit=true` 让前端跳过本次上传）。

权限：**Manager / Clerk**

入参（JSON）：`UploadIntentsIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `batch_uuid` | string? | — | 场景 A：批量预生成时客户端传 UUID（同一批 item 共用），后端按 `batch_uuid` + `kind` 派生 STS 凭证路径前缀 |
| `owner_kind` | string | ✓ | `"PART"` / `"ASSEMBL"``Y` |
| `owner_id` | string (i64) | ✓ | 雪花 id 字符串 |
| `items` | `UploadIntentItemIn[]` | ✓ | 1..=200，每 item 一份上传意图 |

`UploadIntentItemIn`：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | string | ✓ | `"DRAWING"` / `"3D_MODEL"` / `"G_CODE"` / `"SETUP_SHEET"` / `"ASSEMBLY_MASTER"` / `"CAD_2D"` |
| `content_sha256` | string | ✓ | 客户端预计算的 64-hex SHA-256 |
| `original_filename` | string | ✓ | 含扩展名 |
| `file_size` | i64 | ✓ | 字节数；超过 `COS_MAX_FILE_SIZE` → 21103 |
| `content_type` | string | ✓ | MIME；必须与扩展名白名单匹配 → 21102 |

响应 200 `UploadIntentsOut { items: UploadIntentItemOut[] }`，每项：

| 字段 | 类型 | 说明 |
|---|---|---|
| `kind` | string | 同入参 |
| `dedup_hit` | bool | true = CAS 命中（已存在同 owner + kind + sha），前端应跳过本次 PUT；false = 需新上传 |
| `tmp_key` | string? | dedup_hit=false 时为 COS tmp 对象 key（前端 PUT 目标）；dedup_hit=true 时为 null |
| `credentials` | `CosCredentialsOut`? | dedup_hit=false 时下发 STS 临时凭证（access_key / secret_key / session_token / expired_at / region / bucket / endpoint / scheme）；dedup_hit=true 时为 null |

`CosCredentialsOut`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `access_key` | string | STS 临时 AccessKey |
| `secret_key` | string | STS 临时 SecretKey |
| `session_token` | string | STS SessionToken |
| `expired_at` | i64 | epoch 秒；服务端默认下发 `COS_STS_DURATION_SECONDS`（900s） |
| `region` | string | COS bucket 所在 region |
| `bucket` | string | COS bucket 名 |
| `endpoint` | string | COS endpoint（带 scheme） |
| `scheme` | string | `https` / `http` |

业务语义：

- **场景 A（批量预生成）**：`POST /api/v2/parts/batch` 入参 items 里有 drawing_file / model3d_file binding，handler 在调 service 前先调本端点取 `tmp_key` + STS 凭证，然后由前端并发 PUT 上传；service 内部 `prepare_binding_head_copy` 读 head 校验 size + sha，校验通过后 copy 到 CAS key。**整个批量预生成共享同一 `batch_uuid`**（UUIDv4），STS 凭证路径前缀包含 `batch_uuid` + `kind`，便于后续批量管理（重启 / 取消 / 关联）。
- **场景 B（详情页补传）**：`batch_uuid` 字段省略或为 null；按 `kind` 单 item 派生 tmp_key + STS 凭证；若 owner + kind + sha256 已存在 `t_part_file` 行（content_sha256 命中唯一索引），返回 `dedup_hit=true`，前端拿到响应后跳过 PUT 直接复用既有 part_file 行。

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21102 | BIZ_PART_FILE_BAD_TYPE | 400 | kind 不在白名单 / content_type 与扩展名不一致 |
| 21103 | BIZ_PART_FILE_TOO_LARGE | 400 | file_size 超过 `COS_MAX_FILE_SIZE`（默认 300 MB） |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | owner_kind / owner_id 在 DB 不存在 |
| 21116 | STS_ISSUE_FAILED | 500 | STS 凭证下发失败（GetFederationToken 抛错） |
| 40001 | VALIDATION_ERROR | 422 | sha 非 64 hex / filename 含非法字符 / kind 空 / size ≤ 0 |

### `POST /api/v2/parts/{part_id}/files/confirm`

直传 COS 链路的"提交绑定"端点：客户端 PUT 到 tmp 区成功后，调用本端点把 tmp 对象 copy 到 CAS key + INSERT `t_part_file` + 异步清理 tmp 对象。

权限：**Manager / Clerk**（与 `upload-intents` 一致）

入参（JSON）：`ConfirmFileIn`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | string | ✓ | DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D |
| `tmp_key` | string | ✓ | COS tmp 对象 key（前端 PUT 后得到）；必须以 `COS_TMP_PREFIX`（默认 `tmp/`）开头 |
| `content_sha256` | string | ✓ | 64 hex |
| `original_filename` | string | ✓ | 含扩展名 |
| `file_size` | i64 | ✓ | 字节数 |
| `content_type` | string | ✓ | MIME |

响应 200 `PartFileOut`（同 multipart 上传路径；详见下方）。

业务语义：

1. `head_object(tmp_key)` — 校验 tmp 对象存在 + size 一致；不存在 → 21114，size 不一致 → 21115
2. 同事务 soft_delete 旧 part_file（同 owner + kind） + INSERT 新 part_file（unique 索引 `(owner_id, kind, content_sha256)` 并发兜底 → 21108）
3. copy_object(tmp_key, cas_key) — CAS 落到正式 key（与 multipart 上传路径模板一致：`{prefix}part/{part_id}/{KIND}/{sha16}_{safe_name}`）
4. tx.commit() 成功后 spawn `cos.delete_object(tmp_key)` 兜底清理（commit 失败 / IO 失败仅 warn，不阻断 API 返回）

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21102 | BIZ_PART_FILE_BAD_TYPE | 400 | 扩展名不在 kind 白名单 / content_type 与扩展名不一致 |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | part_id 不存在 |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 同 owner + kind + sha256 已存在（unique 索引并发兜底） |
| 21114 | BIZ_PART_FILE_TMP_OBJECT_MISSING | 404 | tmp 对象 head_object 失败（404 NoSuchKey） |
| 21115 | BIZ_PART_FILE_SIZE_MISMATCH | 400 | tmp 对象 head size 与声明 size 不一致 |
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | copy_object 失败（CAS 写入异常） |
| 40000 | BIZ_INVALID_VALUE | 400 | tmp_key 不在 `COS_TMP_PREFIX` 范围内 |
| 40001 | VALIDATION_ERROR | 422 | 字段校验失败（kind / sha / filename / size / content_type） |

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
| 21103 | BIZ_PART_FILE_TOO_LARGE | 400 | file_size 超过 `COS_MAX_FILE_SIZE`（upload-intents 入参校验） |
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | COS SDK 抛错（put_object / copy_object 失败） |
| 21105 | BIZ_PART_FILE_OWNER_NOT_FOUND | 404 | polymorphic owner (part / assembly) 不存在 |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 唯一索引并发兜底（同 owner + kind + sha 撞 23505） |
| 21114 | BIZ_PART_FILE_TMP_OBJECT_MISSING | 404 | 直传 COS confirm：head_object(tmp_key) 失败（404 NoSuchKey） |
| 21115 | BIZ_PART_FILE_SIZE_MISMATCH | 400 | 直传 COS confirm：head size 与声明 size 不一致 |
| 21116 | STS_ISSUE_FAILED | 500 | upload-intents：STS 凭证下发失败（GetFederationToken 抛错） |

> 21101（NOT_FOUND）：保留对齐 Python 错误码表，本文档对应端点暂不返回。

---

## 实现要点

- 事务边界：handler 开 tx → service 写 DB → commit。
- SHA-256 CAS 去重：CAS 命中跳过 COS PUT；并发兜底靠 `uk_t_part_file_part_kind_sha` 唯一索引 + 服务端捕获 23505 → 21108。
- COS object key 模板：`{owner_kind.to_lowercase()}/{owner_id}/{kind}/{sha[..16]}_{sanitized_filename}`。
- 权限：service 层 `require_any_role([Manager, Clerk, CncProgrammer])`；列表 / 详情额外允许 Inspector。
- WS 广播：本域不上报 WS 事件（part_file 是只读资产）。