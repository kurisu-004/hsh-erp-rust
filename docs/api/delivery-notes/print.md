# delivery-notes / 打印

> 本目录条目须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步，详见 [`index.md`](./index.md)
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> **导航**：[`index.md`](./index.md) · [`queries.md`](./queries.md) · [`drafts.md`](./drafts.md) · [`workflow.md`](./workflow.md) · **`print.md`**

## 本文件目录

1. [POST /api/v2/delivery-notes/{id}/print (P4 打印)](#post-apiv2delivery-notesidprint--p4-打印)
2. [POST /api/v2/delivery-notes/{id}/print-labels (P4 标签打印)](#post-apiv2delivery-notesidprint-labels--p4-标签打印)

---

### `POST /api/v2/delivery-notes/{id}/print`  （P4 打印）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 批次 ID 序列；与 `line_items[*].id` 一一对应 |
| `merge_assemblies` | bool? | — | true → 同装配件子件合并一行（默认 `false`） |
| `merge_quantities` | object? | — | `{ "<assembly_id>": <count> }`，按装配件 ID 覆盖合并行数量 |

Response 200：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-note.xlsx"`
- Body: xlsx 二进制

错误码：

- 21109 BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED — root prefix 未配置模板
- 21112 BIZ_DELIVERY_TEMPLATE_TOO_MANY_PARTS — 所选零件超过模板容量
- 21113 BIZ_DELIVERY_PRINT_BAD_ORDER（422） — custom_order 含非法 batch id 或漏行
- 21401 BIZ_DELIVERY_NOTE_NOT_FOUND
- 21402 BIZ_DELIVERY_NOTE_INVALID_TRANSITION — 状态不允许打印

### `POST /api/v2/delivery-notes/{id}/print-labels`  （P4 标签打印）

权限: **Manager / Clerk / Inspector**

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `custom_order` | [string (i64)]? | — | 同 `/print` |
| `merge_assemblies` | bool? | — | 标签默认 `true`（与 Python 一致） |
| `merge_quantities` | object? | — | 同 `/print` |
| `line_item_ids` | [string (i64)]? | — | None / 缺省 = 全部数据行；Some([]) → 400 |

Response 200：

- `Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `Content-Disposition: attachment; filename="F-<YYYY-MM-DD>-labels.xlsx"`
- Body: xlsx 二进制

错误码：

- 同 `/print`，外加 20104 BIZ_INVALID_VALUE（line_item_ids=[]）