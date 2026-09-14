# outsource-shipments 域 API

> 本文件须与 `src/modules/outsource/{handler.rs,dto.rs,service.rs,model.rs,repo.rs}` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`../index.md`](../index.md)
>
> 域覆盖：外协发货对账页更新（shipment 1 端点）。2026-09-13 Phase 2 落地。
> 关联域：[`./outsource-companies.md`](./outsource-companies.md) / [`./outsource-quotes.md`](./outsource-quotes.md)。

---

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| POST | `/api/v2/outsource-shipments/{id}/reconcile-update` | Manager / Clerk | 对账页更新 shipment 行（OCC + 状态机守卫） |

---

## 业务模型

- **发货表** `t_outsource_shipment`：与 `t_outsource_quote` 1:N（一份报价可多次发货）；`status` ∈ {`OPEN` / `IN_TRANSIT` / `DELIVERED` / `RECONCILED` / `CANCELLED`}。
- 本端点主要给对账 UI 使用：根据实际收到的 part 数量 / 状态对账行 UPDATE。

---

## 共享错误码（215xx）

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 21501 | BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND | 404 | shipment 不存在 / 已软删 |
| 21502 | BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION | 400 | shipment.status 不允许本次操作（状态机白名单拒绝） |
| 21503 | BIZ_OUTSOURCE_SHIPMENT_NO_OPEN | 404 | 找不到开口（`status='OPEN'`）的 shipment |
| 21504 | BIZ_OUTSOURCE_SHIPMENT_QUANTITY_EXCEEDS | 400 | 本次接收数量超过开口 shipment.quantity |

---

## 共享 DTO

### OutsourceShipmentOut 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string (i64) | 雪花 ID |
| `quote_id` | string (i64) | 关联报价 |
| `part_id` | string (i64) | 关联 part |
| `quantity` | i32 | 本次发货数量 |
| `status` | string | "OPEN" / "IN_TRANSIT" / "DELIVERED" / "RECONCILED" / "CANCELLED" |
| `shipped_at` | naive datetime? | 发货时间 |
| `received_at` | naive datetime? | 接收时间 |
| `note` | string? | |
| `version` | i32 | 乐观锁 |
| `created_at` | naive datetime | |
| `updated_at` | naive datetime | |

### OutsourceShipmentReconcileUpdateRequest 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `version` | i32 | ✓ | OCC（shipment 行 version） |
| `quantity` | i32? | — | 实际接收数量（> 0；超过开口 → 21504） |
| `status` | string? | — | 目标状态（OPEN / IN_TRANSIT / DELIVERED / RECONCILED / CANCELLED） |
| `received_at` | naive datetime? | — | 实际接收时间 |
| `note` | string? | — | 对账备注 |

---

## 端点契约要点

### `POST /api/v2/outsource-shipments/{id}/reconcile-update`

权限：**Manager / Clerk**（service 层 `require_any_role`）

业务流转：

1. 解析 `id` 雪花 ID + `version` OCC 锚点
2. 校验 shipment 存在 → 21501
3. 状态机守卫：当前状态 → 目标状态必须 `can_transition_to` 放行；不通过 → 21502
4. 若 `quantity` 减少至超过开口（OPEN shipment.quantity - 已 DELIVERED 累计）→ 21504
5. OCC UPDATE（带 version + deleted_at IS NULL）；命中 0 行 → 40901
6. 返回最新 `OutsourceShipmentOut`

WS 广播：本域**无**独立事件（对账是后台核对动作，不驱动前端实时视图）。

### 乐观锁（OCC）

- 表行 `version` 列；UPDATE 带 `WHERE id=$1 AND version=$2 AND deleted_at IS NULL`，命中 0 行 → 40901。

### 事务边界

- handler 层开 tx → 传 `&mut tx` 给 service → 显式 `tx.commit()`；失败时 `Transaction::drop` 自动回滚。

### 防 N+1

- 单点更新，无 list 端点。

---

## 实现位置

- handler：`src/modules/outsource/handler.rs::reconcile_update_shipment` + `shipment_router()`
- service：`src/modules/outsource/service.rs::OutsourceService::reconcile_update_shipment`
- repo：`src/modules/outsource/repo.rs`
- dto：`src/modules/outsource/dto.rs`
- model：`src/modules/outsource/model.rs::TOutsourceShipment + OutsourceShipmentStatus`
- 路由挂载：`/outsource-shipments`（见 `src/modules/mod.rs::v2_router`）

---

## 集成测试

`tests/outsource_send_receive_api.rs`（4+ 用例：reconcile OPEN → DELIVERED happy / OCC 40901 / quantity 超开口 21504 / 状态机拒绝 21502）