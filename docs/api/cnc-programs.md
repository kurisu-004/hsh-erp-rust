# cnc_program 域 API

> 本文件须与 `src/modules/cnc_program/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：CNC 程序配对上传 + 列表。2026-09-14 Phase 3 落地。
> 存储复用 part_file（kind='G_CODE' + kind='SETUP_SHEET'，`paired_file_id` 互指）。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/cnc-programs/pairs` | Manager / CncProgrammer | 配对上传（multipart：`data` JSON + `g_code` 二进制 + `setup_sheet` 二进制） |
| GET | `/api/v2/cnc-programs/parts/{part_id}` | Manager / Clerk / CncProgrammer / Inspector | 列出 part 全部 CNC 配对 |

---

## 配对上传 multipart 契约

请求 body（`multipart/form-data`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `data` | 文本字段 | `PairUploadData` JSON |
| `g_code` | 二进制字段 | G_CODE 程序文件（扩展名 tap/nc/gcode/mpf/cnc） |
| `setup_sheet` | 二进制字段 | 工艺单 PDF |

`PairUploadData`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string | 雪花 id（part.id） |
| `note` | string? | 备注 |

服务端校验：

1. part 存在性 → `20101 BIZ_PART_NOT_FOUND`
2. 扩展名校验：G_CODE 必须是 tap/nc/gcode/mpf/cnc；SETUP_SHEET 必须是 pdf → `21102 BIZ_PART_FILE_BAD_TYPE`
3. SHA-256 CAS 去重（按 kind 独立计算）
4. COS 上传（CAS 命中跳过） + INSERT `t_part_file` 两条，`paired_file_id` 互指
5. 返回 `CncPairOut { g_code, setup_sheet }`，含下载 URL

## 共享 DTO

### CncPairOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `g_code` | CncFileRef | |
| `setup_sheet` | CncFileRef | |

### CncFileRef 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `kind` | string | "G_CODE" / "SETUP_SHEET" |
| `file_type` | string | |
| `original_filename` | string | |
| `file_size` | string (i64) | |
| `content_type` | string | |
| `content_sha256` | string? | |
| `download_url` | string | COS 预签 URL（1h） |
| `paired_file_id` | string (i64)? | 配对文件 id |

### CncPairListItem 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `g_code_id` | string (i64) | |
| `setup_sheet_id` | string (i64) | |
| `g_code_filename` | string | |
| `setup_sheet_filename` | string | |
| `created_at` | naive datetime | |

### CncPairListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | CncPairListItem[] | |
| `total` | i64 | 配对总数 |

---

## 错误码

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20101 | BIZ_PART_NOT_FOUND | 404 | part 不存在 |
| 21102 | BIZ_PART_FILE_BAD_TYPE | 400 | G_CODE / SETUP_SHEET 扩展名 / content_type 不匹配 |
| 21104 | BIZ_PART_FILE_UPLOAD_FAILED | 500 | COS SDK 抛错 |
| 21108 | BIZ_PART_FILE_DUPLICATE | 409 | 唯一索引并发兜底 |

---

## 实现要点

- 配对语义：G_CODE 与 SETUP_SHEET 是「一对」，`paired_file_id` 互相指向；查询时按 G_CODE created_at DESC 排，反查 SETUP_SHEET。
- SHA-256 CAS 去重：CAS 命中跳过 COS PUT；并发兜底靠唯一索引。
- COS object key 模板：`part/{part_id}/G_CODE/{sha[..16]}_{filename}` / `part/{part_id}/SETUP_SHEET/{sha[..16]}_{filename}`。
- 权限：service 层 `require_any_role([Manager, CncProgrammer])` 上传；列表额外允许 Clerk + Inspector。
- WS 广播：本域不上报 WS 事件。