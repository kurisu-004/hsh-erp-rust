# assembly 域 — Cancel

> 本文件须与 `src/modules/assembly/{handler.rs,dto.rs,service.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
> 共享 DTO（AssemblyOut / 端点约束）见 [`./index.md`](./index.md)
> 状态机 / 错误码见 [`./index.md`](./index.md#状态机)
>
> 范围：本文件覆盖 1 个 cancel 端点。CRUD 见 [`./crud.md`](./crud.md)。

## 本文件目录


- [POST /api/v2/assemblies/{assembly_id}/cancel](#post-apiv2assembliesassembly_idcancel)

---

### `POST /api/v2/assemblies/{assembly_id}/cancel`

权限: **Manager / Clerk**

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `assembly_id` | string (i64) | 装配体雪花 ID |

Request：无 body（cancel 是单向状态翻转，无 OCC）。

Response 200 `data`：[`AssemblyOut`](./index.md#assemblyout-字段)

> 业务流转：repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫；命中 0 行 → `BIZ_INVALID_TRANSITION`（终态禁 cancel 或已删除）。
>
> **2026-10-01 review 第 1 轮 M3**：cancel 成功后**同事务**补一次
> `clear_serial_no_if_terminal` 清空 `t_assembly.serial_no` ——
> `uk_t_assembly_serial_no` 的谓词是 `deleted_at IS NULL AND serial_no IS NOT NULL`，
> **不含 status 条件**，只翻状态的话一个作废的装配件会**永久占着**它的序列号，
> 同序列号再也无法被新工单复用。migration 007 只清存量行，新增量原本会继续漏。
> 父装配件由子件派生进终态时（rollup step 5）也走同一条清理，二者幂等。

WS 广播（commit 后下发）：

- `ASSEMBLY_CANCELLED` —— payload `{ assembly_id }`

错误码：

- 20301 — assembly 不存在 / 已软删（HTTP 404）
- 20103 — 当前状态为 COMPLETED / CANCELLED（终态禁 cancel）或已删除（HTTP 400）
- 40300 — 角色不符（HTTP 403）
