# statistics 域 API

> 本文件须与 `src/modules/statistics/{handler,dto,service,repo}.rs` 保持同步
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)

## 端点列表

| Method | Path | 权限 | 说明 |
|---|---|---|---|
| GET | `/api/v2/statistics/overview` | Manager | 生产概览（总工单 / 状态分布 / 平均流转时长） |
| GET | `/api/v2/statistics/workers` | Manager | 工人贡献度一览（按 PICKED_UP 事件统计） |
| GET | `/api/v2/statistics/workers/{worker_id}` | Manager | 工人详情（持有件 / 完成件数 / 跳序次数） |
| GET | `/api/v2/statistics/pickup-skips` | Manager | 跳序取件次数汇总（按工人） |
| GET | `/api/v2/statistics/pickup-skips/{worker_id}` | Manager | 工人跳序明细（分页） |

权限：`MANAGER-only`（与 Python router `dependencies=[Depends(require_role(UserRole.MANAGER))]` 对齐）。

> 2026-09-16 PR-2（migration 027）交付 / 逾期口径变更：
> 1. **实际交付日期** 改由 `t_part_event.event_type='DELIVERED'` 事件派生（`MAX(e.created_at)::date`），不再读 `t_part.actual_delivery_date`（已删列）。
> 2. **未交付判定** 改用 `NOT EXISTS (DELIVERED 事件)` 口径（多批次场景：任一活跃批次的 DELIVERED 事件即视为已交付）。
> 3. **orange / red 分类**（on_time / 晚于 planned / 晚于 system）沿用，不变。
> 4. 详见 `src/modules/statistics/repo.rs::delivered_stats / count_overdue_undelivered` 与 [`../../api/parts/index.md`](../../api/parts/index.md) §PartOut。

---

## `GET /api/v2/statistics/overview`

入参（query）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `date_from` | date (YYYY-MM-DD) | 是 | 开始日期（含）；半开区间左端 |
| `date_to` | date (YYYY-MM-DD) | 是 | 结束日期（含）；半开区间右端 |

业务语义：

- `created_count`：期内 `t_part.created_at ∈ [date_from, date_to+1)` 新建工单数（未软删）。
- `completed_count`：期内 `t_part_event.event_type='COMPLETED' AND batch_id IS NULL` 的 distinct part_id 数。
- `in_process_count`：期末在制（事件重构）— `date_to+1` 前已创建且**不存在** COMPLETED/CANCELLED 工单级事件的工单数。
- `delivered_count` / `delivered_value`：期内**实际交付日期 ∈ [from, to]** 交付件数与 `sum(total_price)`。**2026-09-16 PR-2（migration 027）**：实际交付日期由 `t_part_event.event_type='DELIVERED'` 派生（`MAX(e.created_at)::date WHERE b.deleted_at IS NULL`）；不再读 `t_part.actual_delivery_date`（已删列）。多批次场景下任一活跃批次的 DELIVERED 事件即视为已交付。
- `late_orange_count`：`actual > planned AND (system IS NULL OR actual ≤ system)` 的红橙档命中数（actual 同上由 DELIVERED 事件派生）。
- `late_red_count`：`system NOT NULL AND actual > system` 的严重逾期数。
- `on_time`：`max(delivered_count - orange - red, 0)`（互斥拆分）。
- `overdue_undelivered_count`：**2026-09-16 PR-2**：判定口径由「`actual IS NULL`」改为 `NOT EXISTS DELIVERED 事件`（`planned < today AND 状态非终态 AND t_part_event 无对应批次的 DELIVERED 记录`）。多批次场景下任一活跃批次有 DELIVERED 事件即视为已交付。
- `repair_part_count`：期内 REPAIR_STARTED 事件 distinct part_id 数。
- `daily_created` / `daily_completed`：零填充到 `[date_from, date_to]` 的每日计数。
- `delivery_performance`：`{ on_time, orange, red }` 拆分。
- `status_distribution`：当前各 status 工单数（含 CANCELLED）。

响应：`OverviewOut`（见 `src/modules/statistics/dto.rs`）。

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20104 | BIZ_INVALID_VALUE | 400 | `date_from > date_to` |

---

## `GET /api/v2/statistics/workers`

入参（query）：同 `overview`。

返回：`WorkerStatsListOut { items: WorkerStatsItem[] }`（一次性返回所有未软删工人；前端分页自管）。

`WorkerStatsItem`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | 雪花 ID |
| `worker_name` | string | 工人姓名 |
| `badge_code` | string | 工牌 |
| `work_type_id` | string (i64)? | 工种 ID（可空） |
| `work_type_name` | string? | 工种名 |
| `is_active` | bool | 是否启用 |
| `pickup_count` | i64 | 期内 PICKED_UP 事件数 |
| `pickup_quantity` | i64 | 期内 PICKED_UP 事件 `sum(quantity)` |
| `participated_part_count` | i64 | 期内该工人参与的 distinct 工单数 |
| `contribution_pct` | float? | 贡献度百分比（公式见 `_compute_contribution`） |

---

## `GET /api/v2/statistics/workers/{worker_id}`

入参：

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` (path) | string | 雪花 ID 字符串 |
| `date_from` / `date_to` (query) | date | 同 overview |

返回：`WorkerDetailOut { worker, pickup_count, pickup_quantity, participated_part_count, return_count, daily_pickups, parts }`

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 20104 | BIZ_INVALID_VALUE | 400 | `worker_id` 解析失败 / `date_from > date_to` |
| 20201 | BIZ_WORKER_NOT_FOUND | 404 | worker 不存在或已软删 |

---

## `GET /api/v2/statistics/pickup-skips`

无日期范围（跳序事件是 append-only 历史流）；按 `skip_count desc, last_skip_at desc` 排序。

返回：`PickupSkipSummaryOut { items: PickupSkipSummaryItem[] }`。

`PickupSkipSummaryItem`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `worker_id` | string (i64) | |
| `worker_name` | string | 软删时回退 `'(已删除)'` |
| `badge_code` | string | 软删时回退空串 |
| `work_type_name` | string? | 工种名 |
| `skip_count` | i64 | 该工人累计跳序次数 |
| `last_skip_at` | naive datetime? | 最近一次跳序时间 |

---

## `GET /api/v2/statistics/pickup-skips/{worker_id}`

入参：

| 字段 | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `worker_id` (path) | string | 是 | — | 雪花 ID 字符串 |
| `limit` (query) | i64 | 否 | 50 | 1..=200 |
| `offset` (query) | i64 | 否 | 0 | ≥0 |

返回：`PickupSkipDetailOut { items, total, limit, offset }`。

错误码：20104（worker_id 解析失败）。

---

## 实现要点

- 事务边界在 handler：`pool.begin()` → 传 `&mut tx` → 显式 `commit()`。
- 全部走运行时 `sqlx::query` / `sqlx::query_scalar`（不依赖 `.sqlx/` 离线元数据）。
- 贡献度公式隔离在 `StatisticsService::_compute_contribution`：工人领取次数 / 同工种 total × 100，保留 2 位小数；公式调整只动这一处。
- WS 广播：本域不上报 WS 事件（statistics 不应触发 dashboard 重推）。
- 错误码段：statistics 暂用通用 `BIZ_INVALID_VALUE` / `BIZ_INVALID_QUERY`（不强求单独段；`/api/v2/statistics/pickup-skips/{worker_id}` 不存在用 `BIZ_WORKER_NOT_FOUND`）。