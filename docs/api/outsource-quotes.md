# outsource-quotes 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service.rs,model.rs,repo.rs,statemachine.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：外协报价 lifecycle（quote 8 端点）。2026-09-13 Phase 2 落地。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-shipments.md`](./outsource-shipments.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/outsource-quotes` | Manager / Clerk / Inspector / CncProgrammer | 列表（part / company / status 过滤 + 分页） |
| POST | `/api/v2/outsource-quotes` | Manager / Clerk | 新建 DRAFT 报价 |
| GET | `/api/v2/outsource-quotes/{id}` | Manager / Clerk / Inspector / CncProgrammer | 报价详情 |
| POST | `/api/v2/outsource-quotes/{id}/update` | Manager / Clerk | DRAFT 部分更新（OCC） |
| POST | `/api/v2/outsource-quotes/{id}/submit` | Manager / Clerk | DRAFT → SUBMITTED |
| POST | `/api/v2/outsource-quotes/{id}/approve` | **Manager** | SUBMITTED → APPROVED（需 `version` + 可选 `review_note`） |
| POST | `/api/v2/outsource-quotes/{id}/reject` | **Manager** | SUBMITTED → REJECTED（需 `version` + `review_note`） |
| POST | `/api/v2/outsource-quotes/{id}/soft-delete` | Manager | DRAFT/REJECTED 软删（OCC） |

---

## 业务模型

- **报价表** `t_outsource_quote`：`id` / `part_id` / `outsource_company_id` / `process_id` / `price` (numeric) / `status` / `quantity`? / `is_direct` / `is_billed` / `version` / 审计字段 + 软删。
- **状态机**：`DRAFT → SUBMITTED → APPROVED | REJECTED`（单向前进，不可回退；详见 `src/modules/outsource/statemachine.rs::can_transition_to`）。
- **唯一约束**：同 `(part_id, outsource_company_id, process_id)` 仅一个活跃报价（DB partial unique 兜底 → 21303 DUPLICATE）。

---

## 共享错误码（213xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21301 | BIZ_OUTSOURCE_QUOTE_NOT_FOUND | 404 | 报价不存在 / 已软删 |
| 21302 | BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION | 400 | 当前状态不允许此操作（如 SUBMITTED 调 update） |
| 21303 | BIZ_OUTSOURCE_QUOTE_DUPLICATE | 409 | 同 `(part, company, process)` 已存在活跃报价 |
| 21307 | BIZ_OUTSOURCE_QUOTE_NOT_APPROVED | 404 | 找不到 `(part, company, process)` 的 APPROVED 报价 |

> 21304–21306 预留（业务未触发的中间状态码）。

---

## 共享 DTO

### OutsourceQuoteOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `part_id` | string (i64) | 关联 part |
| `outsource_company_id` | string (i64) | 关联外协公司 |
| `process_id` | string (i64) | 关联工序 |
| `price` | decimal | 报价单价 |
| `quantity` | i32? | 可选（部分模式下不指定数量） |
| `status` | string | "DRAFT" / "SUBMITTED" / "APPROVED" / "REJECTED" |
| `is_direct` | bool | 是否直接派外协（不经仓库） |
| `is_billed` | bool | 是否已开票 |
| `review_note` | string? | Manager 审批 / 拒绝备注 |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

### OutsourceQuoteListOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | `OutsourceQuoteOut[]` | |
| `total` | i64 | 全量命中行数 |
| `limit` | i64 | 回显 |
| `offset` | i64 | 回显 |

### OutsourceQuoteCreateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `part_id` | string (i64) | ✓ | 关联 part |
| `outsource_company_id` | string (i64) | ✓ | 关联外协公司 |
| `process_id` | string (i64) | ✓ | 关联工序（必须是公司已映射的 OUTSOURCE 工序） |
| `price` | decimal | ✓ | 报价单价（> 0） |
| `quantity` | i32? | — | 可选 |
| `is_direct` | bool? | — | 缺省 false |

### OutsourceQuoteUpdateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `price` | decimal? | — | None = 不改 |
| `quantity` | i32? | — | None = 不改；Some(null) 清空 |
| `is_direct` | bool? | — | None = 不改 |

> 仅 DRAFT 可 update；SUBMITTED 及以后返回 21302。

### OutsourceQuoteApproveRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `review_note` | string? | — | 审批备注 |

### OutsourceQuoteRejectRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC |
| `review_note` | string | ✓ | 拒绝理由（> 0 字符） |

### OutsourceQuoteListQuery 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `part_id` | string (i64)? | 按 part 过滤 |
| `outsource_company_id` | string (i64)? | 按 company 过滤 |
| `status` | string? | 单状态过滤 |
| `statuses` | string? | 多状态过滤（逗号分隔） |
| `limit` | i64? | 默认 50，clamp(1, 500) |
| `offset` | i64? | 默认 0，max(0, …) |

---

## 端点契约要点

### Lifecycle 守卫

- **create**：唯一性校验 → INSERT DRAFT。
- **update**：仅 DRAFT 可改价 / 数量；其它状态 → 21302。
- **submit**：DRAFT → SUBMITTED；状态机迁移由 `statemachine.rs` 守卫。
- **approve**：SUBMITTED → APPROVED；Manager-only（service 层 `require_role`）。
- **reject**：SUBMITTED → REJECTED；Manager-only；`review_note` 必填。
- **soft-delete**：DRAFT / REJECTED 可软删；SUBMITTED / APPROVED 拒绝（防审计丢失）。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901。
- `update` / `submit` / `approve` / `reject` / `soft-delete` 强制 OCC；`create` / `list` / `get` 不要求。

### 事务边界

- handler 层开 tx → 传 `&mut tx` 给 service → 显式 `tx.commit()`；失败时 `Transaction::drop` 自动回滚。
- WS 广播在 `tx.commit().await?` **之后**；本域**无** WS 事件（同步经由 part 域 `send-to-outsource` 等端点）。

### 防 N+1

- `list_quotes` 单 SQL JOIN `t_outsource_company` + `t_process` 一次拿齐 company_name / process_name。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::list_quotes / create_quote / get_quote / update_quote / submit_quote / approve_quote / reject_quote / soft_delete_quote` + `quote_router()`
- service：`src/modules/outsource/service.rs::OutsourceService::list_quotes / create_quote / get_quote / update_quote / submit_quote / approve_quote / reject_quote / soft_delete_quote`
- repo：`src/modules/outsource/repo.rs`
- dto：`src/modules/outsource/dto.rs`
- model：`src/modules/outsource/model.rs::TOutsourceQuote + OutsourceQuoteStatus`
- 状态机：`src/modules/outsource/statemachine.rs`
- 路由挂载：`/outsource-quotes`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

`tests/outsource_quote_api.rs`（8+ 用例：create DRAFT / 唯一性 21303 / update DRAFT happy / update SUBMITTED 21302 / submit / approve MANAGER-only / reject review_note 必填 / soft-delete 仅 DRAFT/REJECTED）