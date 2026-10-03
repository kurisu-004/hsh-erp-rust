# Dashboard API

> 本文件须与 `src/modules/dashboard/{handler,service,dto,vo}.rs` 保持同步
>
> 通用约定（响应信封 / 认证 / 角色 / 主键 / 错误码）见 [`./index.md`](./index.md)
>
> WebSocket 端点（`GET /ws/dashboard`）见 [`./websocket.md`](./websocket.md)。

## 端点列表

| Method | Path | 权限 | 状态 |
|---|---|---|---|
| GET | `/api/v2/dashboard/snapshot` | 任意已登录（*） | ✅ 2026-09-28 新增：HTTP 全量首取大屏快照 |

> 路径：挂在 `modules::v2_router` 的 `/dashboard` nest 下，鉴权走
> `v2_router` 末尾的 `authenticate_middleware`（Bearer JWT + Redis session）。

---

### `GET /api/v2/dashboard/snapshot`

权限：任意已登录用户（MANAGER / SHELF_ACCOUNT / CLERK / INSPECTOR / CNC_PROGRAMMER），
与 [`GET /ws/dashboard`](./websocket.md#get-wsdashboard--websocket-升级) 对齐。

Request：

- Header：`Authorization: Bearer <access_token>`（必填，走 `v2_router` 中间件）
- Query（可选，2026-09-30 新增）：
  - `upcoming_days` — 未来 N 天交付分桶的天数，i64 字符串形式（沿仓内 part 域 DTO
    `deserialize_i64_opt` 解析规则）；缺省 / 非法 → service 层兜底为 14；
    取值范围 `1..=60`（service 层 `clamp` 防御恶意大数 / 拼写错把日期塞成 10000）。
- 无 body

调用链：

```
HTTP request
  → v2_router authenticate_middleware (Bearer JWT + Redis session)
  → CurrentUser extractor
  → handler::get_snapshot
    → Query<SnapshotQuery> 解析 upcoming_days（缺省 None）
    → state.pool.begin()
    → state.dashboard_service.build_snapshot_with_workers(&mut *tx, None, q.upcoming_days)
       // service 层 days.unwrap_or(14).clamp(1, 60) 兜底
    → tx.commit()
  → Json(R<DashboardSnapshot>)
```

> 与 WS 端点共用 `DashboardService::build_snapshot_with_workers`：service
> 调用代码块逐字相同（handler 三形态 ①：snapshot 单次只读聚合，开 tx 仅作
> 边界，commit 即结束；不引入新 repo / 新 SQL）。WS 端点的握手 snapshot 推送
> 保留不变（向后兼容），前端新 dashboard 视图可只走 HTTP 首取 + WS 事件
> invalidate 模式。

响应 200（HTTP JSON 信封）：

```json
{
  "code": 0,
  "message": "ok",
  "data": {
    "on_production_shelves": [
      {
        "shelf_id": "<stringified i64>",
        "shelf_code": "S-001",
        "shelf_name": "一号架",
        "total_count": 1,
        "items": [
          {
            "id": "<stringified i64>",
            "batch_id": "<stringified i64 | null>",
            "batch_no": 1,
            "serial_no": null,
            "name": "p-dash",
            "drawing_no": "DWG-D",
            "quantity": 5,
            "is_urgent": false,
            "planned_delivery_date": "2026-09-15",
            "picked_up_at": "2026-09-15T10:00:00",
            "current_holder_id": "<stringified i64 | null>",
            "current_holder_kind": "SHELF",
            "shelf_code": "S-001",
            "customer_id": "<stringified i64 | null>",
            "customer_name": "l2",
            "customer_path": "l1 / l2",
            "next_process_id": "<stringified i64 | null>",
            "next_process_name": "下道工序",
            "worker_name": "张三"
          }
        ]
      }
    ],
    "on_inspection_shelves": [...],
    "in_process": [...],
    "upcoming_delivery": [
      {
        "date": "2026-09-15",
        "count": 6,
        "by_status": { "DELIVERED": 1, "INSPECTION": 2, "PENDING": 3 }
      }
      // 固定 N 条（N = ?upcoming_days；缺省 14，service 层 clamp(1, 60)）；每条都含 by_status（必填，空对象 = 当日 0 件）
    ],
    "ts": "2026-09-28T15:00:00+08:00"
  }
}
```

响应字段（对齐 `src/modules/dashboard/vo/snapshot.rs::DashboardSnapshot`）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data.on_production_shelves` | array | 是 | 生产区货架分组（每架 `OnProductionShelfGroup`） |
| `data.on_inspection_shelves` | array | 是 | 待品检货架上的 part 项（`DashboardItem`） |
| `data.in_process` | array | 是 | 加工中的 part（`DashboardItem`，holder 类别为 `WORKER` / `WORKER_POOL`） |
| `data.upcoming_delivery` | array | 是 | 未来 N 天交付分桶（`UpcomingDeliveryBucket`），固定 N 条；N 来自 `?upcoming_days=`，缺省 14，service 层 `clamp(1, 60)` 兜底（2026-09-30 新增） |
| `data.ts` | string | 是 | 快照构建本地时间戳（`YYYY-MM-DDTHH:MM:SS.fff+08:00`） |
| `data.on_production_shelves[].shelf_id` | string | 是 | 货架 snowflake id（**i64 → 字符串**） |
| `data.on_production_shelves[].shelf_code` | string | 是 | 货架代号 |
| `data.on_production_shelves[].shelf_name` | string | 是 | 货架名 |
| `data.on_production_shelves[].total_count` | integer | 是 | 该架 item 数（top-10 截流后，可能小于 SQL 实际数） |
| `data.on_production_shelves[].items` | array | 是 | `DashboardItem` 列表 |
| `data.on_production_shelves[].items[].id` | string | 是 | part snowflake id |
| `data.on_production_shelves[].items[].batch_id` | string \| null | 否 | 批次 snowflake id |
| `data.on_production_shelves[].items[].batch_no` | integer \| null | 否 | 批内序号 |
| `data.on_production_shelves[].items[].serial_no` | string \| null | 否 | 序列号 |
| `data.on_production_shelves[].items[].name` | string | 是 | part 名 |
| `data.on_production_shelves[].items[].drawing_no` | string | 是 | 图号 |
| `data.on_production_shelves[].items[].quantity` | integer | 是 | 数量 |
| `data.on_production_shelves[].items[].is_urgent` | boolean | 是 | 是否加急 |
| `data.on_production_shelves[].items[].planned_delivery_date` | string \| null | 否 | 计划交付日期 `YYYY-MM-DD` |
| `data.on_production_shelves[].items[].picked_up_at` | string \| null | 否 | 工人领取时间（`NaiveDateTime`，无时区） |
| `data.on_production_shelves[].items[].current_holder_id` | string \| null | 否 | 当前持有者 id（i64 → 字符串） |
| `data.on_production_shelves[].items[].current_holder_kind` | string \| null | 否 | 持有者类别（`SHELF` / `WORKER` / `WORKER_POOL` 等） |
| `data.on_production_shelves[].items[].shelf_code` | string \| null | 否 | 货架代号（重复字段，便于前端按 shelf_code 索引） |
| `data.on_production_shelves[].items[].customer_id` | string \| null | 否 | 客户 id（i64 → 字符串） |
| `data.on_production_shelves[].items[].customer_name` | string \| null | 否 | L2 客户名 |
| `data.on_production_shelves[].items[].customer_path` | string \| null | 否 | L1 / L2 客户路径 |
| `data.on_production_shelves[].items[].next_process_id` | string \| null | 否 | 下一道工序 id（deprecated 标记保留，2026-09-27 part 域字段对齐影响）。**2026-09-30 改直读 `t_part_batch.current_process_id`**（migration 004）——原先经 `LEFT JOIN t_process_chain_step` 取 `s.process_id`，新下发批次（step 为 NULL）会显示 `null` 工序；**字段名不变** |
| `data.on_production_shelves[].items[].next_process_name` | string \| null | 否 | 下一道工序名 |
| `data.on_production_shelves[].items[].worker_name` | string \| null | 否 | 当前持有工人姓名 |
| `data.upcoming_delivery[].date` | string | 是 | 日期 `YYYY-MM-DD` |
| `data.upcoming_delivery[].count` | integer | 是 | 当天预计交付 part 数（i64，JSON wire 保留 number；非 snowflake ID 故不走字符串化） |
| `data.upcoming_delivery[].by_status` | object<string, integer> | 是 | 当天按 `OrderStatus` 细分的件数（2026-09-30 新增；供 dashboard 分层堆叠柱状图用）。**COMPLETED / CANCELLED 已 WHERE 排除，by_status 不会含这两个 key**；空对象 `{}` 表示当日 0 件。key 字母序排列（BTreeMap 序列化保证），但前端按 key 直接查，不依赖顺序。 |

> `data.upcoming_delivery[].by_status` 与 `data.upcoming_delivery[].count` 的关系：
> `count = by_status 所有 value 之和`（service 端求和，VO 与 SQL 二次一致性由 SQL 单次聚合保证）。

错误码：

| code | 名称 | HTTP | 触发场景 |
|---|---|---|---|
| 40100 | UNAUTHORIZED | 401 | 缺失 `Authorization` Header / Bearer JWT 验签失败 |
| 40105 | SESSION_REVOKED | 401 | Redis session 不存在（登出 / 改密 / 管理员停用） |
| 50000 | INTERNAL | 500 | DB / SQL 透传错误（tx begin / commit / SQL 执行失败） |

> 错误码段说明：本端点不引入新业务错误码；DB / SQL 失败走 `AppError::from(sqlx::Error)`
> 透传（50000 / 50001 段），与现有 HTTP handler 风格一致。

与 `/ws/dashboard` 的关系：

| 维度 | `GET /api/v2/dashboard/snapshot`（本端点） | `GET /ws/dashboard` |
|---|---|---|
| 协议 | HTTP（`R<T>` 信封） | WebSocket（`WsSnapshotMsg` envelope） |
| 路径前缀 | `/api/v2/dashboard` | `/ws/dashboard`（不带 `/api/v2`） |
| 鉴权方式 | `authenticate_middleware` | `verify_session_token`（WS upgrade 不能被 HTTP middleware 拦截） |
| 鉴权凭证 | `Authorization: Bearer <jwt>` | `?token=<jwt>` query 参数 |
| 用途 | 视图首屏加载（HTTP 全量首取） | 实时增量（snapshot envelope + 业务事件 + 心跳） |
| 共用 service | `DashboardService::build_snapshot_with_workers` | 同 |
| 调用方前端 | dashboard 视图组件 mount 时 1 次 | dashboard 视图组件 mount 后维持连接 |

实现要点：

- snapshot 构造：`DashboardService::build_snapshot_with_workers`（service 内部无新
  SQL，与 WS 端点的 `build_snapshot_msg` 共用同一 service 方法）。
- 鉴权镜像 WS 端点的 `verify_session_token` 语义：JWT 验签（RS256 + kid）+ iss 校验
  + Redis session 校验 + 滑动 TTL；HTTP path 走 `authenticate_middleware`、
  WS path 走 `verify_session_token`，两者同源。
- i64 字段：
  - **snowflake ID 类**（`shelf_id` / `id` / `batch_id` / `customer_id` /
    `current_holder_id` / `next_process_id`）在 JSON 中序列化为**字符串**，
    与现有 HTTP `R<T>` 风格一致（避免 JS `Number.MAX_SAFE_INTEGER` 精度截断）。
    其中 `next_process_id` 的**取值来源**为 `t_part_batch.current_process_id`
    （2026-09-30 起直读，migration 004），字段名与序列化形态均不变。
  - **数值类**（`data.upcoming_delivery[].count`）保留 JSON integer（小整数，
    不会触发精度问题）。
- WS handshake 仍推一次 `WsSnapshotMsg`（保留向后兼容），不动 `ws_dashboard`
  handler 的 snapshot 推送逻辑；前端可走「HTTP 首取 + WS 事件 invalidate」
  模式忽略 WS 首帧 snapshot，也可保留原行为。