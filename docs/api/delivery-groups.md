# delivery-groups 域 API

> 本文件须与 `src/modules/delivery_note/{handler.rs,dto.rs,service.rs}` 保持同步（`delivery-groups` 与 `delivery-notes` 共享同一 `delivery_note` 模块）
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> `delivery-notes` 域见 [`./delivery-notes/index.md`](./delivery-notes/index.md)
>
> **前置依赖**：L1 客户与 L2 客户必须先建好。分组仅在 L1 客户下创建，成员为该 L1 下的 L2 客户。

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/delivery-groups` | 已登录 | 列出指定 L1 客户下的所有分组 + 未入组 L2 |
| POST | `/api/v2/delivery-groups` | 已登录（service 层 enforce 角色） | 创建分组 |
| POST | `/api/v2/delivery-groups/{id}/update` | 已登录 | 修改分组（OCC，全量替换成员） |
| POST | `/api/v2/delivery-groups/{id}/soft-delete` | 已登录 | 软删分组（OCC） |

---

### `GET /api/v2/delivery-groups`

权限: 已登录

Query：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | L1 客户 ID |

Response 200 `data`：`DeliveryGroupListOut`

| 字段 | 类型 | 说明 |
|---|---|---|
| `groups` | [DeliveryGroup] | 已建立的分组 |
| `ungrouped_customers` | [UngroupedCustomer] | 未入组的 L2 客户 |

`groups[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | |
| `customer_id` | string (i64) | L1 客户 ID |
| `name` | string | |
| `members` | [Member] | 见下 |
| `version` | i32 | |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

`groups[].members[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `customer_id` | string (i64) | L2 客户 ID |
| `customer_name` | string | |

`ungrouped_customers[]`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | L2 客户 ID |
| `name` | string | |

### `POST /api/v2/delivery-groups`

权限: 已登录（service 层 enforce 角色）

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `customer_id` | string (i64) | ✓ | L1 客户 ID |
| `name` | string | ✓ | trim 后 1..=100 字符 |
| `member_customer_ids` | [string (i64)] | ✓ | 成员 L2 客户 ID 列表（空数组 = 创建时无成员） |

Response 200 `data`：`DeliveryGroup`

错误码：

- 20102 BIZ_CUSTOMER_NOT_FOUND
- 21414 BIZ_DELIVERY_GROUP_DUPLICATE_NAME — 同 L1 下分组重名
- 21415 BIZ_DELIVERY_GROUP_MEMBER_CONFLICT — L2 已属于其他活跃分组

### `POST /api/v2/delivery-groups/{id}/update`

权限: 已登录

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |
| `name` | string? | — | None = 不改；Some(空) = 400；Some(>100) = 400 |
| `member_customer_ids` | [string (i64)]? | — | None = 不改；Some(vec) = **全量替换**（缺失软删、新增插入） |

Response 200 `data`：`DeliveryGroup`

错误码：

- 21413 BIZ_DELIVERY_GROUP_NOT_FOUND
- 21414 BIZ_DELIVERY_GROUP_DUPLICATE_NAME
- 21415 BIZ_DELIVERY_GROUP_MEMBER_CONFLICT
- 40901 VERSION_CONFLICT

### `POST /api/v2/delivery-groups/{id}/soft-delete`

权限: 已登录

Path：

| 参数 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 分组 ID |

Request：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | 乐观锁 |

Response 200 `data`: `null`

错误码：

- 21413 BIZ_DELIVERY_GROUP_NOT_FOUND
- 40901 VERSION_CONFLICT
